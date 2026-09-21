use std::{
    env,
    ffi::CString,
    io::IsTerminal,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::Path,
    process::Stdio,
    time::Duration,
};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::UnixStream,
    process::Command,
    time,
};
use tokio_util::codec::Framed;
use uuid::Uuid;

use crate::{
    client::config,
    machines::Catalog,
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, ServerMessage, codec,
        decode_payload, encode_payload,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorCheck {
    pub id: &'static str,
    pub status: CheckStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub status: CheckStatus,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn has_errors(&self) -> bool {
        self.status == CheckStatus::Error
    }

    pub fn render_human(&self) -> String {
        let mut output = String::from("Fut doctor\n\n");
        for check in &self.checks {
            let label = match check.status {
                CheckStatus::Ok => "ok",
                CheckStatus::Info => "info",
                CheckStatus::Warning => "warn",
                CheckStatus::Error => "error",
            };
            output.push_str(&format!("[{label:<5}] {}: {}\n", check.id, check.summary));
        }
        output.push_str(&format!(
            "\nResult: {}\n",
            match self.status {
                CheckStatus::Ok | CheckStatus::Info => "ok",
                CheckStatus::Warning => "warnings",
                CheckStatus::Error => "errors",
            }
        ));
        output
    }
}

pub async fn run(socket: &Path, location: &config::ConfigLocation) -> DoctorReport {
    let mut checks = Vec::new();
    let mut configured_icons = None;

    match config::load_location(location) {
        Ok(loaded) => {
            configured_icons = Some((loaded.ui.icon_preset_name(), loaded.ui.icon_probe()));
            let present = loaded.present;
            let extension_count = loaded.extensions.len();
            let extension_packages = loaded
                .extensions
                .iter()
                .map(configured_extension_details)
                .collect::<Vec<_>>();
            let path = location.path.as_deref().map(path_text);
            checks.push(check(
                "config",
                CheckStatus::Ok,
                if present {
                    format!(
                        "valid {}; {} extension candidate{}",
                        path.as_deref().expect("present config has a path"),
                        extension_count,
                        if extension_count == 1 { "" } else { "s" },
                    )
                } else {
                    "valid defaults; no configuration file".into()
                },
                json!({
                    "source": location.source,
                    "path": path,
                    "present": present,
                    "extensions": extension_count,
                    "extension_packages": extension_packages,
                }),
            ));
        }
        Err(error) => checks.push(check(
            "config",
            CheckStatus::Error,
            format!("{error:#}"),
            json!({ "source": location.source, "path": location.path.as_deref().map(path_text) }),
        )),
    }

    let term = env::var("TERM").ok();
    let terminal_status = if term.as_deref() == Some("dumb") {
        CheckStatus::Error
    } else if term.is_none() {
        CheckStatus::Warning
    } else {
        CheckStatus::Ok
    };
    checks.push(check(
        "terminal",
        terminal_status,
        match term.as_deref() {
            Some("dumb") => "TERM=dumb cannot host interactive Fut".into(),
            Some(term) => format!("TERM={term}"),
            None => "TERM is not set".into(),
        },
        json!({
            "term": term,
            "colorterm": env::var("COLORTERM").ok(),
            "term_program": env::var("TERM_PROGRAM").ok(),
            "stdin_tty": std::io::stdin().is_terminal(),
            "stdout_tty": std::io::stdout().is_terminal(),
        }),
    ));

    let locale = env::var("LC_ALL")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| env::var("LC_CTYPE").ok().filter(|value| !value.is_empty()))
        .or_else(|| env::var("LANG").ok().filter(|value| !value.is_empty()));
    let utf8 = locale.as_deref().is_some_and(|locale| {
        let normalized = locale.to_ascii_lowercase().replace('-', "");
        normalized.contains("utf8")
    });
    checks.push(check(
        "unicode",
        if utf8 {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        if utf8 {
            format!(
                "UTF-8 locale declared by {}",
                locale.as_deref().unwrap_or_default()
            )
        } else {
            "locale does not declare UTF-8; glyph support remains unknown".into()
        },
        json!({ "locale": locale, "utf8_declared": utf8 }),
    ));

    let runtime = socket.parent().filter(|path| !path.as_os_str().is_empty());
    let mut socket_safe = false;
    match runtime {
        None => checks.push(check(
            "runtime",
            CheckStatus::Error,
            "socket path must have a nonempty parent directory".into(),
            json!({ "path": path_text(socket) }),
        )),
        Some(path) => match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                // SAFETY: geteuid has no preconditions and cannot fail.
                let euid = unsafe { libc::geteuid() };
                let secure = metadata.file_type().is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == euid
                    && metadata.permissions().mode() & 0o077 == 0
                    && accessible(path, libc::W_OK | libc::X_OK);
                checks.push(check(
                    "runtime",
                    if secure { CheckStatus::Ok } else { CheckStatus::Error },
                    if secure {
                        "private writable runtime directory owned by the current user".into()
                    } else {
                        "runtime directory is not private, owned, writable, and searchable".into()
                    },
                    json!({ "path": path_text(path), "mode": metadata.permissions().mode() & 0o777, "uid": metadata.uid() }),
                ));
                socket_safe = secure;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let creatable = std::fs::metadata(parent).is_ok_and(|metadata| {
                    metadata.file_type().is_dir() && accessible(parent, libc::W_OK | libc::X_OK)
                });
                checks.push(check(
                    "runtime",
                    if creatable {
                        CheckStatus::Info
                    } else {
                        CheckStatus::Error
                    },
                    if creatable {
                        "runtime directory is absent and can be created when Fut starts".into()
                    } else {
                        "runtime directory is absent and its immediate parent is not creatable"
                            .into()
                    },
                    json!({ "path": path_text(path) }),
                ));
            }
            Err(error) => checks.push(check(
                "runtime",
                CheckStatus::Error,
                format!("cannot inspect runtime directory: {error}"),
                json!({ "path": path_text(path) }),
            )),
        },
    }

    if runtime.is_some()
        && let Ok(log) = crate::daemon::autostart::daemon_log_path(socket)
    {
        checks.push(check(
            "daemon_log",
            CheckStatus::Info,
            format!("daemon log: {}", path_text(&log)),
            json!({
                "path": path_text(&log),
                "rotated_path": path_text(&crate::daemon::autostart::rotated_daemon_log_path(&log)),
                "rotate_bytes": crate::daemon::autostart::DAEMON_LOG_ROTATE_BYTES,
            }),
        ));
    }

    match std::fs::symlink_metadata(socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => checks.push(check(
            "socket",
            CheckStatus::Info,
            "socket is absent; daemon is not running".into(),
            json!({ "path": path_text(socket) }),
        )),
        Err(error) => checks.push(check(
            "socket",
            CheckStatus::Error,
            format!("cannot inspect socket: {error}"),
            json!({ "path": path_text(socket) }),
        )),
        Ok(metadata) => {
            // SAFETY: geteuid has no preconditions and cannot fail.
            let euid = unsafe { libc::geteuid() };
            let safe = socket_safe
                && metadata.file_type().is_socket()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == euid
                && metadata.permissions().mode() & 0o077 == 0;
            checks.push(check(
                "socket",
                if safe { CheckStatus::Ok } else { CheckStatus::Error },
                if safe {
                    "private Fut socket is present".into()
                } else {
                    "socket path is not a private, owned Unix socket".into()
                },
                json!({ "path": path_text(socket), "mode": metadata.permissions().mode() & 0o777, "uid": metadata.uid() }),
            ));
            if safe {
                checks.extend(probe_daemon(socket).await);
            }
        }
    }

    if !checks.iter().any(|check| check.id == "protocol") {
        checks.push(check(
            "protocol",
            CheckStatus::Info,
            "skipped because no safe running socket is available".into(),
            json!({ "client_protocol": PROTOCOL_VERSION }),
        ));
    }
    if !checks.iter().any(|check| check.id == "extensions") {
        checks.push(check(
            "extensions",
            CheckStatus::Info,
            "active daemon catalog unavailable because no safe running socket is available".into(),
            Value::Null,
        ));
    }

    checks.push(openssh_check().await);
    checks.push(saved_machines_check());
    checks.push(remote_endpoints_check().await);

    let (preset, glyphs) = configured_icons.unwrap_or_else(|| ("unknown", Vec::new()));
    let nerd_font = preset == "nerd_font";
    checks.push(check(
        "icons",
        if nerd_font {
            CheckStatus::Warning
        } else {
            CheckStatus::Info
        },
        if nerd_font {
            format!(
                "Nerd Font preset enabled; active font cannot be detected; visually verify: {}",
                glyphs.join(" ")
            )
        } else {
            format!("{preset} preset; visually verify: {}", glyphs.join(" "))
        },
        json!({
            "preset": preset,
            "glyphs": glyphs,
            "active_font": "unknown",
            "detection": "unavailable",
            "nerd_font_required": nerd_font,
        }),
    ));

    let status = if checks
        .iter()
        .any(|check| check.status == CheckStatus::Error)
    {
        CheckStatus::Error
    } else if checks
        .iter()
        .any(|check| check.status == CheckStatus::Warning)
    {
        CheckStatus::Warning
    } else {
        CheckStatus::Ok
    };
    DoctorReport { status, checks }
}

async fn probe_daemon(socket: &Path) -> Vec<DoctorCheck> {
    let request_id = Uuid::new_v4();
    let result = time::timeout(Duration::from_millis(500), async {
        let stream = UnixStream::connect(socket).await?;
        let mut framed = Framed::new(stream, codec());
        let hello = Envelope {
            request_id: Some(request_id),
            message: ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                client_version: env!("CARGO_PKG_VERSION").into(),
                mode: ClientMode::Control,
            },
        };
        framed.send(Bytes::from(encode_payload(&hello)?)).await?;
        let response = framed
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("daemon closed during handshake"))??;
        let envelope = decode_payload::<Envelope<ServerMessage>>(&response)?;
        if envelope.request_id != Some(request_id) {
            anyhow::bail!("daemon returned an uncorrelated handshake response");
        }
        Ok::<ServerMessage, anyhow::Error>(envelope.message)
    })
    .await;
    match result {
        Ok(Ok(ServerMessage::Welcome {
            version,
            server_version,
            selected: None,
            extension_catalog,
        })) if version == PROTOCOL_VERSION => vec![
            check(
                "protocol",
                CheckStatus::Ok,
                format!("daemon {server_version} answered compatible protocol {version}"),
                json!({ "client_protocol": PROTOCOL_VERSION, "server_protocol": version, "client_version": env!("CARGO_PKG_VERSION"), "server_version": server_version }),
            ),
            active_extensions_check(&extension_catalog),
        ],
        Ok(Ok(ServerMessage::IncompatibleProtocol { client, server })) => vec![
            check(
                "protocol",
                CheckStatus::Error,
                format!("incompatible protocol: client {client}, server {server}"),
                json!({ "client_protocol": client, "server_protocol": server }),
            ),
            unavailable_extensions_check(
                "active catalog unavailable through an incompatible daemon protocol",
            ),
        ],
        Ok(Ok(message)) => vec![
            check(
                "protocol",
                CheckStatus::Error,
                format!("unexpected handshake response: {message:?}"),
                Value::Null,
            ),
            unavailable_extensions_check(
                "active catalog unavailable after an unexpected handshake response",
            ),
        ],
        Ok(Err(error)) => vec![
            check(
                "protocol",
                CheckStatus::Error,
                format!("protocol probe failed: {error}"),
                Value::Null,
            ),
            unavailable_extensions_check(
                "active catalog unavailable because the protocol probe failed",
            ),
        ],
        Err(_) => vec![
            check(
                "protocol",
                CheckStatus::Error,
                "protocol probe timed out".into(),
                Value::Null,
            ),
            unavailable_extensions_check(
                "active catalog unavailable because the protocol probe timed out",
            ),
        ],
    }
}

fn configured_extension_details(extension: &crate::extensions::Extension) -> Value {
    json!({
        "id": extension.id(),
        "api_version": extension.api_version(),
        "version": extension.version().to_string(),
        "fut": extension.fut_requirement().to_string(),
        "capabilities": extension
            .capabilities()
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>(),
        "root": path_text(extension.root()),
        "manifest": path_text(&extension.root().join(crate::extensions::MANIFEST_FILE_NAME)),
        "commands": extension.commands().map(|command| command.name()).collect::<Vec<_>>(),
        "presentation_tokens": extension.presentation_tokens().len(),
        "provenance": "configured_package",
    })
}

fn active_extensions_check(catalog: &crate::protocol::ExtensionCatalog) -> DoctorCheck {
    let packages = catalog
        .extensions
        .iter()
        .map(|extension| {
            json!({
                "id": extension.id,
                "api_version": extension.api_version,
                "version": extension.version,
                "fut": extension.fut,
                "capabilities": extension.capabilities,
                "root": path_text(&extension.root),
                "manifest": path_text(&extension.root.join(crate::extensions::MANIFEST_FILE_NAME)),
                "hooks": extension.hooks.keys().collect::<Vec<_>>(),
                "commands": extension.commands.keys().collect::<Vec<_>>(),
                "presentation_tokens": extension.presentation_tokens.iter().map(|token| &token.name).collect::<Vec<_>>(),
                "has_config_defaults": catalog.config.defaults.contains_key(&extension.id),
                "provenance": "active_daemon_catalog",
            })
        })
        .collect::<Vec<_>>();
    check(
        "extensions",
        CheckStatus::Ok,
        format!(
            "active generation {} fingerprint {} with {} extension{}",
            catalog.generation,
            catalog.fingerprint,
            catalog.extensions.len(),
            if catalog.extensions.len() == 1 {
                ""
            } else {
                "s"
            },
        ),
        json!({
            "generation": catalog.generation,
            "fingerprint": catalog.fingerprint,
            "count": catalog.extensions.len(),
            "config_source": catalog.config.source.as_deref().map(path_text),
            "packages": packages,
        }),
    )
}

fn unavailable_extensions_check(summary: &str) -> DoctorCheck {
    check("extensions", CheckStatus::Info, summary.into(), Value::Null)
}

const OPENSSH_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_OPENSSH_OUTPUT_BYTES: usize = 4 * 1024;
const MAX_OPENSSH_IDENTITY_BYTES: usize = 256;
const MAX_DIAGNOSTIC_TEXT_BYTES: usize = 1024;

async fn openssh_check() -> DoctorCheck {
    probe_openssh(Path::new("ssh"), OPENSSH_PROBE_TIMEOUT).await
}

async fn probe_openssh(program: &Path, deadline: Duration) -> DoctorCheck {
    let mut command = Command::new(program);
    command
        .arg("-V")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let error = bounded_safe_text(&error.to_string(), MAX_DIAGNOSTIC_TEXT_BYTES);
            return check(
                "ssh",
                CheckStatus::Error,
                "OpenSSH executable is unavailable".into(),
                json!({ "available": false, "error": error }),
            );
        }
        Err(error) => {
            let error = bounded_safe_text(&error.to_string(), MAX_DIAGNOSTIC_TEXT_BYTES);
            return check(
                "ssh",
                CheckStatus::Error,
                format!("cannot start OpenSSH executable: {error}"),
                json!({ "available": false, "error": error }),
            );
        }
    };
    let stdout = child.stdout.take().expect("piped OpenSSH stdout");
    let stderr = child.stderr.take().expect("piped OpenSSH stderr");
    let result = time::timeout(deadline, async {
        tokio::join!(
            child.wait(),
            read_bounded(stdout, MAX_OPENSSH_OUTPUT_BYTES),
            read_bounded(stderr, MAX_OPENSSH_OUTPUT_BYTES),
        )
    })
    .await;
    let (status, stdout, stderr) = match result {
        Ok((Ok(status), Ok(stdout), Ok(stderr))) => (status, stdout, stderr),
        Ok((status, stdout, stderr)) => {
            let error = status
                .err()
                .or_else(|| stdout.err())
                .or_else(|| stderr.err())
                .expect("at least one OpenSSH probe operation failed");
            let error = bounded_safe_text(&error.to_string(), MAX_DIAGNOSTIC_TEXT_BYTES);
            return check(
                "ssh",
                CheckStatus::Error,
                format!("OpenSSH version probe failed: {error}"),
                json!({ "available": true, "error": error }),
            );
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = time::timeout(Duration::from_millis(100), child.wait()).await;
            return check(
                "ssh",
                CheckStatus::Error,
                "OpenSSH version probe timed out".into(),
                json!({ "available": true, "timed_out": true }),
            );
        }
    };

    let stdout_text = String::from_utf8_lossy(&stdout.bytes);
    let stderr_text = String::from_utf8_lossy(&stderr.bytes);
    let is_openssh = stdout_text.contains("OpenSSH") || stderr_text.contains("OpenSSH");
    let identity = stderr_text
        .lines()
        .chain(stdout_text.lines())
        .find(|line| !line.trim().is_empty())
        .map(|line| bounded_safe_text(line.trim(), MAX_OPENSSH_IDENTITY_BYTES));
    let successful = status.success() && is_openssh;
    let summary = if successful {
        match identity.as_deref() {
            Some(identity) => format!("OpenSSH available: {identity}"),
            None => "OpenSSH is available".into(),
        }
    } else if !status.success() {
        format!(
            "OpenSSH version probe exited with status {}",
            status
                .code()
                .map_or_else(|| "unknown".into(), |code| code.to_string())
        )
    } else {
        "ssh executable did not identify itself as OpenSSH".into()
    };
    check(
        "ssh",
        if successful {
            CheckStatus::Ok
        } else {
            CheckStatus::Error
        },
        summary,
        json!({
            "available": true,
            "openssh": is_openssh,
            "identity": identity,
            "exit_code": status.code(),
            "output_truncated": stdout.truncated || stderr.truncated,
        }),
    )
}

struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    maximum: usize,
) -> std::io::Result<BoundedOutput> {
    let mut bytes = Vec::with_capacity(maximum);
    let mut truncated = false;
    let mut buffer = [0_u8; 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = maximum.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    Ok(BoundedOutput { bytes, truncated })
}

fn saved_machines_check() -> DoctorCheck {
    match Catalog::resolve() {
        Ok(catalog) => machine_catalog_check(&catalog),
        Err(error) => {
            let error = bounded_safe_text(&format!("{error:#}"), MAX_DIAGNOSTIC_TEXT_BYTES);
            check(
                "machines",
                CheckStatus::Error,
                format!("cannot resolve saved machine catalog: {error}"),
                json!({ "error": error }),
            )
        }
    }
}

async fn remote_endpoints_check() -> DoctorCheck {
    let catalog = match Catalog::resolve().and_then(|catalog| catalog.list()) {
        Ok(machines) => machines,
        Err(error) => {
            return check(
                "remote_endpoints",
                CheckStatus::Info,
                "remote compatibility skipped because the saved machine catalog is invalid".into(),
                json!({ "error": bounded_safe_text(&format!("{error:#}"), MAX_DIAGNOSTIC_TEXT_BYTES) }),
            );
        }
    };
    let enabled = catalog
        .into_iter()
        .filter(|machine| machine.enabled)
        .collect::<Vec<_>>();
    if enabled.is_empty() {
        return check(
            "remote_endpoints",
            CheckStatus::Info,
            "no enabled saved machines to probe".into(),
            json!({ "contacted": 0, "concurrency": 4 }),
        );
    }
    let total = enabled.len();
    let results = futures_util::stream::iter(enabled.into_iter().map(|machine| async move {
        if let Err(error) = probe_ssh_config(&machine.target).await {
            return json!({
                "id": machine.id,
                "label": safe_text(&machine.label),
                "target": safe_text(&machine.target),
                "status": "ssh_config_error",
                "error": error,
            });
        }
        let result = time::timeout(
            Duration::from_secs(6),
            crate::client::diagnose_remote(&machine.target),
        )
        .await;
        match result {
            Ok(Ok(server_version)) => json!({
                "id": machine.id,
                "label": safe_text(&machine.label),
                "target": safe_text(&machine.target),
                "status": "compatible",
                "server_version": safe_text(&server_version),
            }),
            Ok(Err(error)) => json!({
                "id": machine.id,
                "label": safe_text(&machine.label),
                "target": safe_text(&machine.target),
                "status": "error",
                "error": bounded_safe_text(&format!("{error:#}"), MAX_DIAGNOSTIC_TEXT_BYTES),
            }),
            Err(_) => json!({
                "id": machine.id,
                "label": safe_text(&machine.label),
                "target": safe_text(&machine.target),
                "status": "timeout",
                "error": "bounded compatibility probe timed out",
            }),
        }
    }))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await;
    let compatible = results
        .iter()
        .filter(|result| result["status"] == "compatible")
        .count();
    check(
        "remote_endpoints",
        if compatible == total {
            CheckStatus::Ok
        } else {
            CheckStatus::Error
        },
        format!("{compatible} of {total} enabled saved machines are compatible"),
        json!({ "contacted": total, "compatible": compatible, "concurrency": 4, "profiles": results }),
    )
}

async fn probe_ssh_config(target: &str) -> Result<(), String> {
    let mut command = Command::new("ssh");
    command
        .args(["-G", "--", target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| bounded_safe_text(&error.to_string(), MAX_DIAGNOSTIC_TEXT_BYTES))?;
    let stderr = child.stderr.take().expect("piped SSH config stderr");
    let result = time::timeout(Duration::from_millis(500), async {
        tokio::join!(child.wait(), read_bounded(stderr, MAX_OPENSSH_OUTPUT_BYTES))
    })
    .await;
    match result {
        Ok((Ok(status), Ok(_))) if status.success() => Ok(()),
        Ok((status, stderr)) => {
            let message = match (status, stderr) {
                (Ok(status), Ok(stderr)) => format!(
                    "ssh -G exited with status {}: {}",
                    status
                        .code()
                        .map_or_else(|| "unknown".into(), |code| code.to_string()),
                    String::from_utf8_lossy(&stderr.bytes),
                ),
                (Err(error), _) | (_, Err(error)) => error.to_string(),
            };
            Err(bounded_safe_text(&message, MAX_DIAGNOSTIC_TEXT_BYTES))
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = time::timeout(Duration::from_millis(100), child.wait()).await;
            Err("ssh -G configuration probe timed out".into())
        }
    }
}

fn machine_catalog_check(catalog: &Catalog) -> DoctorCheck {
    let path = bounded_safe_text(&path_text(catalog.path()), MAX_DIAGNOSTIC_TEXT_BYTES);
    match catalog.list() {
        Ok(machines) => {
            let enabled = machines.iter().filter(|machine| machine.enabled).count();
            let disabled = machines.len() - enabled;
            let profiles = machines
                .iter()
                .map(|machine| {
                    json!({
                        "id": machine.id,
                        "label": safe_text(&machine.label),
                        "target": safe_text(&machine.target),
                        "enabled": machine.enabled,
                    })
                })
                .collect::<Vec<_>>();
            check(
                "machines",
                CheckStatus::Ok,
                format!(
                    "valid saved machine catalog; {} profile{} ({enabled} enabled, {disabled} disabled)",
                    machines.len(),
                    if machines.len() == 1 { "" } else { "s" },
                ),
                json!({
                    "path": path,
                    "count": machines.len(),
                    "enabled": enabled,
                    "disabled": disabled,
                    "profiles": profiles,
                }),
            )
        }
        Err(error) => {
            let error = bounded_safe_text(&format!("{error:#}"), MAX_DIAGNOSTIC_TEXT_BYTES);
            check(
                "machines",
                CheckStatus::Error,
                format!("saved machine catalog is invalid: {error}"),
                json!({ "path": path, "error": error }),
            )
        }
    }
}

fn check(id: &'static str, status: CheckStatus, summary: String, details: Value) -> DoctorCheck {
    DoctorCheck {
        id,
        status,
        summary: safe_text(&summary),
        details,
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn accessible(path: &Path, mode: libc::c_int) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: path is a valid NUL-terminated filesystem path and mode is an access bitmask.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

fn safe_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
            {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn bounded_safe_text(value: &str, maximum_bytes: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        let character = if character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            ) {
            '�'
        } else {
            character
        };
        if output.len() + character.len_utf8() > maximum_bytes {
            break;
        }
        output.push(character);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;

    fn empty_extension_catalog() -> crate::protocol::ExtensionCatalog {
        crate::protocol::ExtensionCatalog {
            generation: 1,
            fingerprint: "0".repeat(64),
            extensions: Vec::new(),
            config: crate::protocol::ExtensionCatalogConfig::default(),
        }
    }

    #[tokio::test]
    async fn matching_protocol_is_reported_as_compatible() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("fut.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut framed = Framed::new(stream, codec());
                let Some(Ok(request)) = framed.next().await else {
                    continue;
                };
                let hello: Envelope<ClientMessage> = decode_payload(&request).unwrap();
                let _ = framed
                    .send(Bytes::from(
                        encode_payload(&Envelope {
                            request_id: hello.request_id,
                            message: ServerMessage::Welcome {
                                version: PROTOCOL_VERSION,
                                server_version: "0.2.0".into(),
                                selected: None,
                                extension_catalog: empty_extension_catalog(),
                            },
                        })
                        .unwrap(),
                    ))
                    .await;
            }
        });

        let checks = probe_daemon(&socket).await;
        let protocol = checks.iter().find(|check| check.id == "protocol").unwrap();
        let extensions = checks
            .iter()
            .find(|check| check.id == "extensions")
            .unwrap();

        assert_eq!(protocol.status, CheckStatus::Ok);
        assert_eq!(
            protocol.summary,
            format!("daemon 0.2.0 answered compatible protocol {PROTOCOL_VERSION}")
        );
        assert_eq!(extensions.status, CheckStatus::Ok);
        assert_eq!(extensions.details["generation"], 1);
        assert_eq!(extensions.details["count"], 0);
        server.abort();
    }

    #[tokio::test]
    async fn openssh_probe_reports_missing_binary() {
        let temporary = tempfile::tempdir().unwrap();
        let check = probe_openssh(
            &temporary.path().join("missing-ssh"),
            Duration::from_millis(100),
        )
        .await;

        assert_eq!(check.status, CheckStatus::Error);
        assert_eq!(check.summary, "OpenSSH executable is unavailable");
        assert_eq!(check.details["available"], false);
    }

    #[tokio::test]
    async fn openssh_probe_is_bounded_and_sanitizes_its_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let ssh = temporary.path().join("ssh");
        fs::write(
            &ssh,
            "#!/bin/sh\nprintf 'OpenSSH_9.9\\033[31m\\n' >&2\ni=0\nwhile [ $i -lt 5000 ]; do printf x >&2; i=$((i + 1)); done\n",
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();

        let check = probe_openssh(&ssh, Duration::from_secs(1)).await;

        assert_eq!(check.status, CheckStatus::Ok);
        assert_eq!(check.details["openssh"], true);
        assert_eq!(check.details["output_truncated"], true);
        assert_eq!(check.details["identity"], "OpenSSH_9.9�[31m");
        assert!(!check.summary.contains('\u{1b}'));
        assert!(check.summary.len() <= MAX_OPENSSH_IDENTITY_BYTES + 20);
    }

    #[tokio::test]
    async fn openssh_probe_times_out_without_waiting_for_the_command() {
        let temporary = tempfile::tempdir().unwrap();
        let ssh = temporary.path().join("ssh");
        fs::write(&ssh, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();

        let check = probe_openssh(&ssh, Duration::from_millis(20)).await;

        assert_eq!(check.status, CheckStatus::Error);
        assert_eq!(check.summary, "OpenSSH version probe timed out");
        assert_eq!(check.details["available"], true);
        assert_eq!(check.details["timed_out"], true);
    }

    #[test]
    fn machine_catalog_reports_enabled_and_disabled_profiles_without_writing() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("machines.toml");
        let contents = format!(
            r#"version = 1

[[machines]]
id = "{}"
label = "work"
target = "alice@work.example"
enabled = true

[[machines]]
id = "{}"
label = "home"
target = "home.example"
enabled = false
"#,
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        fs::write(&path, &contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let check = machine_catalog_check(&Catalog::at(&path));

        assert_eq!(check.status, CheckStatus::Ok);
        assert_eq!(check.details["count"], 2);
        assert_eq!(check.details["enabled"], 1);
        assert_eq!(check.details["disabled"], 1);
        assert_eq!(check.details["profiles"][0]["label"], "work");
        assert_eq!(check.details["profiles"][1]["enabled"], false);
        assert_eq!(fs::read_to_string(path).unwrap(), contents);
    }

    #[test]
    fn machine_catalog_reports_profile_validation_errors_without_writing() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("machines.toml");
        let contents = format!(
            r#"version = 1

[[machines]]
id = "{}"
label = "work"
target = "-not-an-ssh-destination"
enabled = true
"#,
            Uuid::new_v4(),
        );
        fs::write(&path, &contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let check = machine_catalog_check(&Catalog::at(&path));

        assert_eq!(check.status, CheckStatus::Error);
        assert!(check.summary.contains("SSH destination must not start"));
        assert!(check.details["profiles"].is_null());
        assert_eq!(fs::read_to_string(path).unwrap(), contents);
    }

    #[test]
    fn human_report_is_ascii_structured_and_errors_control_exit_status() {
        let report = DoctorReport {
            status: CheckStatus::Error,
            checks: vec![check(
                "config",
                CheckStatus::Error,
                "invalid".into(),
                Value::Null,
            )],
        };
        assert!(report.has_errors());
        assert_eq!(
            report.render_human(),
            "Fut doctor\n\n[error] config: invalid\n\nResult: errors\n"
        );
        assert_eq!(safe_text("bad\n\u{1b}[31m\u{202e}"), "bad��[31m�");
    }
}
