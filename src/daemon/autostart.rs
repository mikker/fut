use std::{
    fs::OpenOptions,
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
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
    let log_path = runtime_dir(socket)?.join("fut-daemon.log");
    if should_start {
        prepare_runtime_dir(socket)?;
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
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
    use crate::protocol::PROTOCOL_VERSION_0_1;

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
        let incompatible_version = PROTOCOL_VERSION_0_1;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, codec());
            let request = framed.next().await.unwrap().unwrap();
            let hello: Envelope<ClientMessage> = decode_payload(&request).unwrap();
            assert!(matches!(
                hello.message,
                ClientMessage::Hello {
                    version: PROTOCOL_VERSION,
                    ..
                }
            ));
            framed
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
                .await
                .unwrap();
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
        server.await.unwrap();
    }
}
