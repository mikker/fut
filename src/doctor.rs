use std::{
    env,
    ffi::CString,
    io::IsTerminal,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::Path,
    time::Duration,
};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{net::UnixStream, time};
use tokio_util::codec::Framed;
use uuid::Uuid;

use crate::{
    client::config,
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

pub async fn run(socket: &Path, config_dir: Option<&Path>) -> DoctorReport {
    let mut checks = Vec::new();
    let mut configured_icons = None;

    match config::resolve_location(config_dir) {
        Ok(location) => match config::load_location(&location) {
            Ok(loaded) => {
                configured_icons = Some((
                    loaded.ui.icon_preset_name(),
                    loaded.ui.icon_probe(),
                ));
                let present = loaded.present;
                let extension_count = loaded.extensions.len();
                let path = location.path.as_deref().map(path_text);
                checks.push(check(
                    "config",
                    CheckStatus::Ok,
                    if present {
                        format!(
                            "valid {}",
                            path.as_deref().expect("present config has a path")
                        )
                    } else {
                        "valid defaults; no configuration file".into()
                    },
                    json!({
                        "source": location.source,
                        "path": path,
                        "present": present,
                        "extensions": extension_count,
                    }),
                ));
            }
            Err(error) => checks.push(check(
                "config",
                CheckStatus::Error,
                error.to_string(),
                json!({ "source": location.source, "path": location.path.as_deref().map(path_text) }),
            )),
        },
        Err(error) => checks.push(check(
            "config",
            CheckStatus::Error,
            error.to_string(),
            Value::Null,
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
                checks.push(probe_protocol(socket).await);
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

async fn probe_protocol(socket: &Path) -> DoctorCheck {
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
        })) if version == PROTOCOL_VERSION => check(
            "protocol",
            CheckStatus::Ok,
            format!("daemon {server_version} answered compatible protocol {version}"),
            json!({ "client_protocol": PROTOCOL_VERSION, "server_protocol": version, "client_version": env!("CARGO_PKG_VERSION"), "server_version": server_version }),
        ),
        Ok(Ok(ServerMessage::IncompatibleProtocol { client, server })) => check(
            "protocol",
            CheckStatus::Error,
            format!("incompatible protocol: client {client}, server {server}"),
            json!({ "client_protocol": client, "server_protocol": server }),
        ),
        Ok(Ok(message)) => check(
            "protocol",
            CheckStatus::Error,
            format!("unexpected handshake response: {message:?}"),
            Value::Null,
        ),
        Ok(Err(error)) => check(
            "protocol",
            CheckStatus::Error,
            format!("protocol probe failed: {error}"),
            Value::Null,
        ),
        Err(_) => check(
            "protocol",
            CheckStatus::Error,
            "protocol probe timed out".into(),
            Value::Null,
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn matching_protocol_is_reported_as_compatible() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("fut.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, codec());
            let request = framed.next().await.unwrap().unwrap();
            let hello: Envelope<ClientMessage> = decode_payload(&request).unwrap();
            framed
                .send(Bytes::from(
                    encode_payload(&Envelope {
                        request_id: hello.request_id,
                        message: ServerMessage::Welcome {
                            version: PROTOCOL_VERSION,
                            server_version: "0.2.0".into(),
                            selected: None,
                        },
                    })
                    .unwrap(),
                ))
                .await
                .unwrap();
        });

        let protocol = probe_protocol(&socket).await;

        assert_eq!(protocol.status, CheckStatus::Ok);
        assert_eq!(
            protocol.summary,
            format!("daemon 0.2.0 answered compatible protocol {PROTOCOL_VERSION}")
        );
        server.await.unwrap();
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
