//! The single per-user Fut daemon and its local socket lifecycle.

mod lease;
pub mod path;

pub mod autostart;

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::watch,
    task::JoinSet,
    time::{Duration, timeout},
};
use tokio_util::codec::Framed;

use crate::{
    domain::{ClientId, TerminalSize},
    protocol::{
        AcknowledgedCommand, ClientKind, ClientMessage, Envelope, PROTOCOL_VERSION, ServerMessage,
        codec, decode_payload, encode_payload,
    },
    terminal::{
        CommandError, SpawnSpec, TerminalEvent, TerminalHandle, TerminalLifecycle, spawn_terminal,
    },
};

use lease::AttachmentLease;
use path::{prepare_runtime_dir, runtime_dir};

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub spawn: SpawnSpec,
}

impl DaemonConfig {
    pub fn shell(socket_path: PathBuf, cwd: PathBuf) -> Self {
        let program = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        Self {
            socket_path,
            spawn: SpawnSpec {
                program,
                argv: Vec::new(),
                cwd,
                env: std::env::vars().collect::<HashMap<_, _>>(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            },
        }
    }
}

const CONNECTION_GRACE_PERIOD: Duration = Duration::from_secs(1);

/// Bind Fut's sole socket and run while its terminal is alive.
pub async fn run_daemon(config: DaemonConfig) -> Result<()> {
    let socket = bind_socket(&config.socket_path).await?;
    let terminal = Arc::new(spawn_terminal(config.spawn)?);
    let lease = AttachmentLease::default();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let mut lifecycle = terminal.subscribe_lifecycle();
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            changed = lifecycle.changed() => {
                if changed.is_err() || matches!(*lifecycle.borrow(), TerminalLifecycle::Exited { .. }) {
                    break;
                }
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = socket.listener.accept() => {
                let (stream, _) = accepted.context("accept daemon client")?;
                let terminal = Arc::clone(&terminal);
                let lease = lease.clone();
                let shutdown = shutdown_tx.clone();
                connections.spawn(async move {
                    if let Err(error) = handle_connection(stream, terminal, lease, shutdown).await {
                        tracing::debug!(%error, "client connection ended");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::debug!(%error, "client connection task failed");
                }
            }
        }
    }
    if let Err(error) = terminal.close().await
        && !matches!(error, CommandError::Stopped)
    {
        return Err(error).context("close terminal during shutdown");
    }

    // Terminal exit is already durable. Give attached and command handlers time
    // to flush TerminalExited/CommandCompleted, but never wait forever on a client.
    let _ = timeout(CONNECTION_GRACE_PERIOD, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.shutdown().await;
    Ok(())
}

async fn bind_socket(path: &Path) -> Result<OwnedSocket> {
    prepare_runtime_dir(path)?;
    let lock = acquire_socket_lock(path)?;
    match UnixListener::bind(path) {
        Ok(listener) => secure_socket(listener, path, lock),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path).await {
                Ok(_) => bail!("a Fut daemon is already running at {}", path.display()),
                Err(connect_error) if definitely_stale(&connect_error) => {
                    let metadata = fs::symlink_metadata(path)
                        .with_context(|| format!("inspect stale socket {}", path.display()))?;
                    if !metadata.file_type().is_socket() {
                        bail!("refusing to remove non-socket path {}", path.display());
                    }
                    // SAFETY: geteuid has no preconditions and cannot fail.
                    if metadata.uid() != unsafe { libc::geteuid() } {
                        bail!(
                            "refusing to remove socket not owned by current user: {}",
                            path.display()
                        );
                    }
                    fs::remove_file(path)
                        .with_context(|| format!("remove stale socket {}", path.display()))?;
                    secure_socket(UnixListener::bind(path)?, path, lock)
                }
                Err(connect_error) => Err(connect_error)
                    .with_context(|| format!("cannot prove socket is stale: {}", path.display())),
            }
        }
        Err(error) => Err(error).with_context(|| format!("bind socket {}", path.display())),
    }
}

fn acquire_socket_lock(socket: &Path) -> Result<File> {
    let path = runtime_dir(socket)?.join("fut.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("open daemon lock {}", path.display()))?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!(
            "daemon lock is not a private file owned by the current user: {}",
            path.display()
        );
    }
    // SAFETY: file is an open descriptor and flock does not retain the pointer.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            bail!("a Fut daemon already owns {}", socket.display());
        }
        return Err(error).with_context(|| format!("lock daemon socket {}", socket.display()));
    }
    Ok(file)
}

fn definitely_stale(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    )
}

fn secure_socket(listener: UnixListener, path: &Path, lock: File) -> Result<OwnedSocket> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure socket {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)?;
    Ok(OwnedSocket {
        listener,
        path: path.to_owned(),
        device: metadata.dev(),
        inode: metadata.ino(),
        _lock: lock,
    })
}

#[derive(Debug)]
struct OwnedSocket {
    listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
    // Declared last and therefore dropped last, after cleanup and the listener.
    _lock: File,
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    terminal: Arc<TerminalHandle>,
    lease: AttachmentLease,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let mut framed = Framed::new(stream, codec());
    let Some(frame) = timeout(Duration::from_secs(2), framed.next())
        .await
        .context("client hello timed out")?
    else {
        return Ok(());
    };
    let first: Envelope<ClientMessage> = decode_payload(&frame?)?;
    let (version, kind, size) = match first.message {
        ClientMessage::Hello {
            version,
            kind,
            size,
            ..
        } => (version, kind, size),
        _ => {
            send_error(
                &mut framed,
                first.request_id,
                "hello_required",
                "first message must be hello",
            )
            .await?;
            return Ok(());
        }
    };
    if version != PROTOCOL_VERSION {
        send(
            &mut framed,
            first.request_id,
            ServerMessage::IncompatibleProtocol {
                client: version,
                server: PROTOCOL_VERSION,
            },
        )
        .await?;
        return Ok(());
    }
    if let Err(error) = size.validate() {
        send_error(
            &mut framed,
            first.request_id,
            "invalid_size",
            &error.to_string(),
        )
        .await?;
        return Ok(());
    }

    let client = ClientId::new();
    let mut lifecycle = terminal.subscribe_lifecycle();
    let lease_guard = if kind == ClientKind::Interactive {
        match lease.acquire(client) {
            Some(guard) => Some(guard),
            None => {
                send_error(
                    &mut framed,
                    first.request_id,
                    "already_attached",
                    "another interactive client holds the attachment lease",
                )
                .await?;
                return Ok(());
            }
        }
    } else {
        None
    };
    let current_lifecycle = lifecycle.borrow().clone();
    if kind == ClientKind::Interactive
        && let TerminalLifecycle::Exited { exit_code } = current_lifecycle
    {
        send_error(
            &mut framed,
            first.request_id,
            "terminal_exited",
            &format!("terminal already exited with status {exit_code:?}"),
        )
        .await?;
        return Ok(());
    }
    send(
        &mut framed,
        first.request_id,
        ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").into(),
            terminal_id: terminal.id(),
            child_pid: terminal.child_pid(),
        },
    )
    .await?;

    if kind == ClientKind::Control {
        return control_loop(&mut framed, &terminal, shutdown).await;
    }
    if let Err(error) = terminal.resize(size).await {
        send_command_error(&mut framed, None, error).await?;
    }
    let mut snapshots = terminal.subscribe_snapshots();
    let mut events = terminal.subscribe_events();
    let initial_screen = snapshots.borrow().clone();
    send(
        &mut framed,
        None,
        ServerMessage::Snapshot {
            terminal_id: terminal.id(),
            screen: initial_screen,
        },
    )
    .await?;

    loop {
        tokio::select! {
            frame = framed.next() => {
                let Some(frame) = frame else { break };
                let envelope: Envelope<ClientMessage> = decode_payload(&frame?)?;
                match envelope.message {
                    ClientMessage::Input { bytes } => command_response(&mut framed, envelope.request_id, AcknowledgedCommand::Input, terminal.input(bytes).await).await?,
                    ClientMessage::Resize { size } => {
                        if let Err(error) = size.validate() {
                            send_error(&mut framed, envelope.request_id, "invalid_size", &error.to_string()).await?;
                        } else {
                            command_response(&mut framed, envelope.request_id, AcknowledgedCommand::Resize, terminal.resize(size).await).await?;
                        }
                    }
                    ClientMessage::Detach => {
                        send(&mut framed, envelope.request_id, ServerMessage::Detached).await?;
                        break;
                    }
                    ClientMessage::Ping => send(&mut framed, envelope.request_id, ServerMessage::Pong { daemon_pid: std::process::id() }).await?,
                    ClientMessage::CloseTerminal | ClientMessage::Shutdown => send_error(&mut framed, envelope.request_id, "control_only", "command requires a control connection").await?,
                    ClientMessage::Hello { .. } => send_error(&mut framed, envelope.request_id, "already_hello", "hello was already received").await?,
                }
            },
            changed = snapshots.changed() => {
                if changed.is_err() { break; }
                let screen = snapshots.borrow_and_update().clone();
                send(&mut framed, None, ServerMessage::Snapshot {
                    terminal_id: terminal.id(),
                    screen,
                }).await?;
            },
            event = events.recv() => match event {
                Ok(TerminalEvent::TerminalExited { .. }) => {},
                Ok(TerminalEvent::Error { message }) => send_error(&mut framed, None, "terminal", &message).await?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            changed = lifecycle.changed() => {
                if changed.is_err() { break; }
                let current_lifecycle = lifecycle.borrow().clone();
                if let TerminalLifecycle::Exited { exit_code } = current_lifecycle {
                    send(&mut framed, None, ServerMessage::TerminalExited { terminal_id: terminal.id(), exit_code }).await?;
                    break;
                }
            },
        }
    }
    drop(lease_guard);
    Ok(())
}

async fn control_loop(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    terminal: &TerminalHandle,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    while let Some(frame) = framed.next().await {
        let envelope: Envelope<ClientMessage> = decode_payload(&frame?)?;
        match envelope.message {
            ClientMessage::Ping => {
                send(
                    framed,
                    envelope.request_id,
                    ServerMessage::Pong {
                        daemon_pid: std::process::id(),
                    },
                )
                .await?
            }
            ClientMessage::CloseTerminal => {
                let result = terminal.close().await;
                let terminal_stopped = matches!(result, Ok(()) | Err(CommandError::Stopped));
                command_response(
                    framed,
                    envelope.request_id,
                    AcknowledgedCommand::CloseTerminal,
                    result,
                )
                .await?;
                if terminal_stopped {
                    break;
                }
            }
            ClientMessage::Shutdown => {
                send(
                    framed,
                    envelope.request_id,
                    ServerMessage::CommandCompleted {
                        command: AcknowledgedCommand::Shutdown,
                    },
                )
                .await?;
                let _ = shutdown.send(true);
                break;
            }
            ClientMessage::Detach => {
                send(framed, envelope.request_id, ServerMessage::Detached).await?;
                break;
            }
            ClientMessage::Input { .. } | ClientMessage::Resize { .. } => {
                send_error(
                    framed,
                    envelope.request_id,
                    "interactive_only",
                    "input and resize require the attachment lease",
                )
                .await?
            }
            ClientMessage::Hello { .. } => {
                send_error(
                    framed,
                    envelope.request_id,
                    "already_hello",
                    "hello was already received",
                )
                .await?
            }
        }
    }
    Ok(())
}

async fn command_response(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    request_id: Option<uuid::Uuid>,
    command: AcknowledgedCommand,
    result: Result<(), CommandError>,
) -> Result<()> {
    if let Err(error) = result {
        send_command_error(framed, request_id, error).await?;
    } else if request_id.is_some() {
        send(
            framed,
            request_id,
            ServerMessage::CommandCompleted { command },
        )
        .await?;
    }
    Ok(())
}

async fn send_command_error(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    request_id: Option<uuid::Uuid>,
    error: CommandError,
) -> Result<()> {
    let code = match error {
        CommandError::Busy => "busy",
        CommandError::Stopped => "terminal_stopped",
    };
    send_error(framed, request_id, code, &error.to_string()).await
}

async fn send_error(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    request_id: Option<uuid::Uuid>,
    code: &str,
    message: &str,
) -> Result<()> {
    send(
        framed,
        request_id,
        ServerMessage::Error {
            code: code.into(),
            message: message.into(),
        },
    )
    .await
}

async fn send(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    request_id: Option<uuid::Uuid>,
    message: ServerMessage,
) -> Result<()> {
    framed
        .send(Bytes::from(encode_payload(&Envelope {
            request_id,
            message,
        })?))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_errors_are_conservative() {
        assert!(definitely_stale(&io::Error::from(
            io::ErrorKind::ConnectionRefused
        )));
        assert!(!definitely_stale(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(!definitely_stale(&io::Error::from(io::ErrorKind::TimedOut)));
    }

    #[tokio::test]
    async fn lock_prevents_a_second_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("runtime/fut.sock");
        let first = bind_socket(&path).await.unwrap();
        let error = bind_socket(&path).await.unwrap_err();
        assert!(error.to_string().contains("already owns"));
        drop(first);
    }

    #[tokio::test]
    async fn cleanup_does_not_unlink_a_replacement_socket() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("runtime/fut.sock");
        let owned = bind_socket(&path).await.unwrap();
        fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();

        drop(owned);

        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
        drop(replacement);
    }
}
