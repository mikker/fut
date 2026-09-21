use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::{net::UnixStream, time};
use tokio_util::codec::Framed;

use crate::protocol::{
    ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, ServerMessage, codec, decode_payload,
    encode_payload,
};

use super::path::{prepare_runtime_dir, runtime_dir};

const START_DEADLINE: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);
pub const DAEMON_LOG_ROTATE_BYTES: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct RotatingDaemonLog {
    state: Arc<RotatingDaemonLogState>,
}

struct RotatingDaemonLogState {
    path: PathBuf,
    writes: Mutex<()>,
}

impl RotatingDaemonLog {
    pub fn at(path: PathBuf) -> Self {
        Self {
            state: Arc::new(RotatingDaemonLogState {
                path,
                writes: Mutex::new(()),
            }),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingDaemonLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Write for RotatingDaemonLog {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let _guard = self
            .state
            .writes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let written = buffer.len();
        let buffer = &buffer[buffer
            .len()
            .saturating_sub(DAEMON_LOG_ROTATE_BYTES as usize)..];
        let mut file = open_daemon_log(&self.state.path, buffer.len() as u64)?;
        file.write_all(buffer)?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn daemon_log_path(socket: &Path) -> Result<PathBuf> {
    Ok(runtime_dir(socket)?.join("fut-daemon.log"))
}

pub fn rotated_daemon_log_path(log: &Path) -> PathBuf {
    log.with_file_name("fut-daemon.log.1")
}

fn open_daemon_log(path: &Path, incoming_bytes: u64) -> io::Result<File> {
    if fs::metadata(path).is_ok_and(|metadata| {
        metadata.len() >= DAEMON_LOG_ROTATE_BYTES
            || metadata.len().saturating_add(incoming_bytes) > DAEMON_LOG_ROTATE_BYTES
    }) {
        let rotated = rotated_daemon_log_path(path);
        match fs::remove_file(&rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(path, rotated)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtocolProbe {
    Ready,
    Incompatible { server: u16 },
    Occupied,
    Unavailable,
}

/// Start a detached daemon if needed and wait until its real protocol responds.
pub async fn ensure_daemon(
    socket: &Path,
    cwd: &Path,
    config_location: &crate::client::config::ConfigLocation,
) -> Result<()> {
    let should_start = match probe_protocol(socket).await {
        ProtocolProbe::Ready => return Ok(()),
        ProtocolProbe::Incompatible { server } => {
            bail!(
                "daemon at {} uses protocol {server}, but this Fut client requires protocol \
                 {PROTOCOL_VERSION}; run `fut daemon shutdown`, then retry",
                socket.display()
            )
        }
        ProtocolProbe::Occupied => false,
        ProtocolProbe::Unavailable => true,
    };
    let log_path = daemon_log_path(socket)?;
    if should_start {
        prepare_runtime_dir(socket)?;
        let stdout = open_daemon_log(&log_path, 0)
            .with_context(|| format!("open daemon log {}", log_path.display()))?;
        let stderr = stdout.try_clone()?;
        let mut command = Command::new(std::env::current_exe().context("locate fut executable")?);
        command.arg("--socket").arg(socket);
        if config_location.is_disabled() {
            command.arg("--no-config");
        } else if config_location.source == "--config-dir"
            && let Some(path) = config_location.path.as_deref().and_then(Path::parent)
        {
            command.arg("--config-dir").arg(path);
        }
        command
            .arg("daemon")
            .arg("run")
            .arg("--cwd")
            .arg(cwd)
            .arg("--log-file")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        // SAFETY: setsid is async-signal-safe and this closure only invokes it.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().context("start Fut daemon")?;
    }

    let deadline = time::Instant::now() + START_DEADLINE;
    while time::Instant::now() < deadline {
        match probe_protocol(socket).await {
            ProtocolProbe::Ready => return Ok(()),
            ProtocolProbe::Incompatible { server } => {
                bail!(
                    "daemon at {} uses protocol {server}, but this Fut client requires protocol \
                     {PROTOCOL_VERSION}; run `fut daemon shutdown`, then retry",
                    socket.display()
                )
            }
            ProtocolProbe::Occupied | ProtocolProbe::Unavailable => {}
        }
        time::sleep(Duration::from_millis(40)).await;
    }
    if should_start {
        bail!(
            "daemon did not become ready at {} (see {})",
            socket.display(),
            log_path.display()
        )
    } else {
        bail!("daemon at {} did not become ready", socket.display())
    }
}

pub async fn protocol_ready(socket: &Path) -> bool {
    probe_protocol(socket).await == ProtocolProbe::Ready
}

async fn probe_protocol(socket: &Path) -> ProtocolProbe {
    let stream = match time::timeout(PROBE_TIMEOUT, UnixStream::connect(socket)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) | Err(_) => return ProtocolProbe::Unavailable,
    };
    time::timeout(PROBE_TIMEOUT, async {
        let mut framed = Framed::new(stream, codec());
        framed
            .send(Bytes::from(encode_payload(&Envelope {
                request_id: None,
                message: ClientMessage::Hello {
                    version: PROTOCOL_VERSION,
                    client_version: env!("CARGO_PKG_VERSION").into(),
                    mode: ClientMode::Control,
                },
            })?))
            .await?;
        let frame = framed.next().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no welcome")
        })??;
        let response: Envelope<ServerMessage> = decode_payload(&frame)?;
        Ok::<ServerMessage, anyhow::Error>(response.message)
    })
    .await
    .map_or(ProtocolProbe::Occupied, |result| match result {
        Ok(ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            ..
        }) => ProtocolProbe::Ready,
        Ok(
            ServerMessage::Welcome {
                version: server, ..
            }
            | ServerMessage::IncompatibleProtocol { server, .. },
        ) => ProtocolProbe::Incompatible { server },
        Ok(_) | Err(_) => ProtocolProbe::Occupied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absent_socket_is_not_ready() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(!protocol_ready(&temporary.path().join("missing.sock")).await);
    }

    #[tokio::test]
    async fn incompatible_daemon_is_reported_without_starting_another() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("fut.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let incompatible_version = PROTOCOL_VERSION - 1;
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut framed = Framed::new(stream, codec());
                let Some(Ok(request)) = framed.next().await else {
                    continue;
                };
                let hello: Envelope<ClientMessage> = decode_payload(&request).unwrap();
                assert!(matches!(
                    hello.message,
                    ClientMessage::Hello {
                        version: PROTOCOL_VERSION,
                        ..
                    }
                ));
                let _ = framed
                    .send(Bytes::from(
                        encode_payload(&Envelope {
                            request_id: hello.request_id,
                            message: ServerMessage::IncompatibleProtocol {
                                client: PROTOCOL_VERSION,
                                server: incompatible_version,
                            },
                        })
                        .unwrap(),
                    ))
                    .await;
            }
        });

        let config_location = crate::client::config::resolve_location(None).unwrap();
        let error = ensure_daemon(&socket, temporary.path(), &config_location)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains(&format!("uses protocol {incompatible_version}"))
        );
        assert!(
            error
                .to_string()
                .contains(&format!("requires protocol {PROTOCOL_VERSION}"))
        );
        assert!(!temporary.path().join("fut-daemon.log").exists());
        server.abort();
    }

    #[test]
    fn daemon_log_keeps_one_bounded_rotated_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("fut-daemon.log");
        let mut log = RotatingDaemonLog::at(path.clone());
        let full = vec![b'x'; DAEMON_LOG_ROTATE_BYTES as usize];

        log.write_all(&full).unwrap();
        log.write_all(b"next\n").unwrap();

        assert_eq!(
            fs::metadata(rotated_daemon_log_path(&path)).unwrap().len(),
            DAEMON_LOG_ROTATE_BYTES
        );
        assert_eq!(fs::read(path).unwrap(), b"next\n");
    }
}
