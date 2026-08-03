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

use crate::{
    domain::TerminalSize,
    protocol::{
        ClientKind, ClientMessage, Envelope, PROTOCOL_VERSION, ServerMessage, codec,
        decode_payload, encode_payload,
    },
};

use super::path::{prepare_runtime_dir, runtime_dir};

const START_DEADLINE: Duration = Duration::from_secs(5);

/// Start a detached daemon if needed and wait until its real protocol responds.
pub async fn ensure_daemon(socket: &Path, cwd: &Path) -> Result<()> {
    if protocol_ready(socket).await {
        return Ok(());
    }
    prepare_runtime_dir(socket)?;
    let log_path = runtime_dir(socket)?.join("fut-daemon.log");
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let stderr = stdout.try_clone()?;
    let mut command = Command::new(std::env::current_exe().context("locate fut executable")?);
    command
        .arg("--socket")
        .arg(socket)
        .arg("daemon")
        .arg("--foreground")
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

    let deadline = time::Instant::now() + START_DEADLINE;
    while time::Instant::now() < deadline {
        if protocol_ready(socket).await {
            return Ok(());
        }
        time::sleep(Duration::from_millis(40)).await;
    }
    bail!(
        "daemon did not become ready at {} (see {})",
        socket.display(),
        log_path.display()
    )
}

pub async fn protocol_ready(socket: &Path) -> bool {
    time::timeout(Duration::from_millis(250), async {
        let stream = UnixStream::connect(socket).await?;
        let mut framed = Framed::new(stream, codec());
        framed
            .send(Bytes::from(encode_payload(&Envelope {
                request_id: None,
                message: ClientMessage::Hello {
                    version: PROTOCOL_VERSION,
                    client_version: env!("CARGO_PKG_VERSION").into(),
                    kind: ClientKind::Control,
                    size: TerminalSize {
                        columns: 80,
                        rows: 24,
                    },
                },
            })?))
            .await?;
        let frame = framed.next().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no welcome")
        })??;
        let response: Envelope<ServerMessage> = decode_payload(&frame)?;
        Ok::<bool, anyhow::Error>(matches!(
            response.message,
            ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                ..
            }
        ))
    })
    .await
    .is_ok_and(|result| result.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absent_socket_is_not_ready() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(!protocol_ready(&temporary.path().join("missing.sock")).await);
    }
}
