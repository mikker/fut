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
    sync::{Mutex, mpsc, watch},
    task::JoinSet,
    time::{Duration, timeout},
};
use tokio_util::codec::Framed;

use crate::{
    domain::{ClientId, PaneId, SessionId, TabId, TerminalId, TerminalSize, WorkspaceId},
    protocol::{
        AcknowledgedCommand, ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, SelectedTarget,
        ServerMessage, codec, decode_payload, encode_payload,
    },
    resources::{
        InitialPath, Project, ProjectIdentity, ResourceError, ResourceTree, SessionSelector,
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

struct RuntimeEntry {
    handle: Arc<TerminalHandle>,
    lease: AttachmentLease,
}

struct SharedState {
    resources: ResourceTree,
    runtimes: HashMap<TerminalId, RuntimeEntry>,
    accepting: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct DaemonError {
    code: &'static str,
    message: String,
}

impl DaemonError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn command(code: &'static str, error: CommandError) -> Self {
        Self::new(code, error.to_string())
    }
}

impl From<ResourceError> for DaemonError {
    fn from(error: ResourceError) -> Self {
        let code = match error {
            ResourceError::NotFound(_) => "not_found",
            ResourceError::Duplicate(_) => "duplicate",
            ResourceError::Closing(_) => "target_closing",
            ResourceError::TargetRequired => "target_required",
            ResourceError::EmptyName => "invalid_name",
            _ => "resource_error",
        };
        Self::new(code, error.to_string())
    }
}

impl SharedState {
    fn register_session(
        &mut self,
        path: InitialPath,
        terminal: Arc<TerminalHandle>,
    ) -> Result<(), DaemonError> {
        self.resources.create_session(path)?;
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        Ok(())
    }

    fn finalize_terminal(&mut self, terminal_id: TerminalId) -> Result<bool, DaemonError> {
        if !self.runtimes.contains_key(&terminal_id) {
            return Err(DaemonError::new(
                "resource_error",
                "terminal runtime missing during finalization",
            ));
        }
        let mutation = self.resources.terminal_exited(terminal_id)?;
        self.runtimes.remove(&terminal_id);
        if mutation.multiplexer_empty {
            self.accepting = false;
        }
        Ok(mutation.multiplexer_empty)
    }

    fn begin_session_close(
        &mut self,
        selector: SessionSelector,
    ) -> Result<(SessionId, Vec<Arc<TerminalHandle>>), DaemonError> {
        if !self.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let session_id = self.resources.resolve_session(selector)?;
        let terminals = self.resources.close_session(session_id)?.terminals_to_close;
        let handles = terminals
            .into_iter()
            .map(|id| {
                self.runtimes
                    .get(&id)
                    .map(|entry| Arc::clone(&entry.handle))
                    .ok_or_else(|| {
                        DaemonError::new(
                            "resource_error",
                            format!("terminal runtime missing for {id}"),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>();
        match handles {
            Ok(handles) => Ok((session_id, handles)),
            Err(error) => {
                let _ = self.resources.cancel_close_session(session_id);
                Err(error)
            }
        }
    }

    fn begin_shutdown(&mut self) -> Vec<Arc<TerminalHandle>> {
        self.accepting = false;
        self.runtimes
            .values()
            .map(|entry| Arc::clone(&entry.handle))
            .collect()
    }
}

type Shared = Arc<Mutex<SharedState>>;

/// Bind Fut's sole socket and run while at least one session is alive.
pub async fn run_daemon(config: DaemonConfig) -> Result<()> {
    let socket = bind_socket(&config.socket_path).await?;
    let cwd = fs::canonicalize(&config.spawn.cwd)
        .with_context(|| format!("canonicalize {}", config.spawn.cwd.display()))?;
    let mut initial_spawn = config.spawn;
    initial_spawn.cwd = cwd.clone();
    let terminal = Arc::new(spawn_terminal(initial_spawn)?);
    let initial = initial_path(default_session_name(&cwd), cwd, terminal.id());
    let mut state = SharedState {
        resources: ResourceTree::default(),
        runtimes: HashMap::new(),
        accepting: true,
    };
    state.register_session(initial, Arc::clone(&terminal))?;
    let shared = Arc::new(Mutex::new(state));
    let (exited_tx, mut exited_rx) = mpsc::unbounded_channel();
    watch_terminal(terminal, exited_tx.clone());
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            Some(terminal_id) = exited_rx.recv() => {
                if finalize_terminal(&shared, terminal_id).await {
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
                let shared = Arc::clone(&shared);
                let exited = exited_tx.clone();
                let shutdown = shutdown_tx.clone();
                connections.spawn(async move {
                    if let Err(error) = handle_connection(stream, shared, exited, shutdown).await {
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
    let handles = shared.lock().await.begin_shutdown();
    let mut first_close_error = None;
    for terminal in handles {
        if let Err(error) = terminal.close().await
            && first_close_error.is_none()
        {
            first_close_error = Some(error);
        }
    }
    if let Some(error) = first_close_error {
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

fn default_session_name(cwd: &Path) -> String {
    cwd.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
}

fn initial_path(name: String, cwd: PathBuf, terminal_id: TerminalId) -> InitialPath {
    InitialPath {
        session_id: SessionId::new(),
        session_name: name,
        project: Project {
            identity: ProjectIdentity::CanonicalDirectory(cwd.clone()),
        },
        workspace_id: WorkspaceId::new(),
        workspace_name: "main".into(),
        root: cwd,
        tab_id: TabId::new(),
        tab_name: "shell".into(),
        pane_id: PaneId::new(),
        terminal_id,
    }
}

fn watch_terminal(terminal: Arc<TerminalHandle>, exited: mpsc::UnboundedSender<TerminalId>) {
    tokio::spawn(async move {
        let mut lifecycle = terminal.subscribe_lifecycle();
        while matches!(*lifecycle.borrow(), TerminalLifecycle::Running) {
            if lifecycle.changed().await.is_err() {
                return;
            }
        }
        let _ = exited.send(terminal.id());
    });
}

async fn finalize_terminal(shared: &Shared, terminal_id: TerminalId) -> bool {
    let mut state = shared.lock().await;
    match state.finalize_terminal(terminal_id) {
        Ok(empty) => empty,
        Err(error) => {
            tracing::error!(message = %error.message, %terminal_id, "finalize terminal resource");
            false
        }
    }
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
    shared: Shared,
    exited: mpsc::UnboundedSender<TerminalId>,
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
    let (version, mode) = match first.message {
        ClientMessage::Hello { version, mode, .. } => (version, mode),
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
    let client = ClientId::new();
    let (selected, interactive_size) = match mode {
        ClientMode::Interactive { size, selector } => {
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
            let selected = match select_target(&shared, selector, client).await {
                Ok(selected) => selected,
                Err(error) => {
                    send_error(&mut framed, first.request_id, error.code, &error.message).await?;
                    return Ok(());
                }
            };
            (Some(selected), Some(size))
        }
        ClientMode::Control => (None, None),
    };
    let (target, mut lease_guard) = selected.unzip();
    let terminal = target.as_ref().map(|(_, terminal)| Arc::clone(terminal));
    let mut lifecycle = terminal
        .as_ref()
        .map(|terminal| terminal.subscribe_lifecycle());
    let current_lifecycle = lifecycle
        .as_ref()
        .map(|lifecycle| lifecycle.borrow().clone());
    if let Some(TerminalLifecycle::Exited { exit_code }) = current_lifecycle {
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
            selected: target.as_ref().map(|(selected, _)| selected.clone()),
        },
    )
    .await?;

    let Some(size) = interactive_size else {
        return control_loop(&mut framed, shared, exited, shutdown).await;
    };
    let terminal = terminal.expect("interactive connection selected a terminal");
    let mut lifecycle = lifecycle.take().expect("interactive lifecycle exists");
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
                        drop(lease_guard.take());
                        send(&mut framed, envelope.request_id, ServerMessage::Detached).await?;
                        break;
                    }
                    ClientMessage::Ping => send(&mut framed, envelope.request_id, ServerMessage::Pong { daemon_pid: std::process::id() }).await?,
                    ClientMessage::CreateSession { .. } | ClientMessage::ListResources | ClientMessage::CloseSession { .. } | ClientMessage::Shutdown => send_error(&mut framed, envelope.request_id, "control_only", "command requires a control connection").await?,
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
                    if snapshots.has_changed().unwrap_or(false) {
                        let screen = snapshots.borrow_and_update().clone();
                        send(&mut framed, None, ServerMessage::Snapshot {
                            terminal_id: terminal.id(),
                            screen,
                        }).await?;
                    }
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
    shared: Shared,
    exited: mpsc::UnboundedSender<TerminalId>,
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
            ClientMessage::CreateSession {
                name,
                cwd,
                program,
                argv,
            } => match create_session(&shared, &exited, name, cwd, program, argv).await {
                Ok(selected) => {
                    send(
                        framed,
                        envelope.request_id,
                        ServerMessage::SessionCreated { selected },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(framed, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::ListResources => {
                let snapshot = shared.lock().await.resources.snapshot();
                send(
                    framed,
                    envelope.request_id,
                    ServerMessage::Resources { snapshot },
                )
                .await?;
            }
            ClientMessage::CloseSession { selector } => {
                match close_session(&shared, selector).await {
                    Ok(()) => {
                        send(
                            framed,
                            envelope.request_id,
                            ServerMessage::CommandCompleted {
                                command: AcknowledgedCommand::CloseSession,
                            },
                        )
                        .await?
                    }
                    Err(error) => {
                        send_error(framed, envelope.request_id, error.code, &error.message).await?
                    }
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

async fn select_target(
    shared: &Shared,
    selector: Option<SessionSelector>,
    client: ClientId,
) -> Result<((SelectedTarget, Arc<TerminalHandle>), lease::LeaseGuard), DaemonError> {
    let state = shared.lock().await;
    if !state.accepting {
        return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
    }
    let target = state
        .resources
        .resolve_terminal_target(selector)
        .map_err(DaemonError::from)?;
    let runtime = state
        .runtimes
        .get(&target.terminal_id)
        .ok_or_else(|| DaemonError::new("not_found", "terminal runtime not found"))?;
    let guard = runtime.lease.acquire(client).ok_or_else(|| {
        DaemonError::new(
            "already_attached",
            "another interactive client holds this terminal's attachment lease",
        )
    })?;
    Ok((
        (
            SelectedTarget {
                session_id: target.session_id,
                workspace_id: target.workspace_id,
                tab_id: target.tab_id,
                pane_id: target.pane_id,
                terminal_id: target.terminal_id,
                child_pid: runtime.handle.child_pid(),
            },
            Arc::clone(&runtime.handle),
        ),
        guard,
    ))
}

async fn create_session(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    name: String,
    cwd: PathBuf,
    program: Option<PathBuf>,
    argv: Vec<String>,
) -> Result<SelectedTarget, DaemonError> {
    let cwd = fs::canonicalize(&cwd).map_err(|error| {
        DaemonError::new(
            "invalid_cwd",
            format!("canonicalize {}: {error}", cwd.display()),
        )
    })?;
    let program = program.unwrap_or_else(|| {
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/bin/sh".into())
    });
    let proposed = initial_path(name, cwd.clone(), TerminalId::new());
    let (terminal, selected, insertion_error) = {
        let mut state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let mut validated = state.resources.clone();
        validated
            .create_session(proposed.clone())
            .map_err(DaemonError::from)?;
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                program,
                argv,
                cwd: cwd.clone(),
                env: std::env::vars().collect(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .map_err(|error| DaemonError::new("spawn_failed", error.to_string()))?,
        );
        let mut path = proposed;
        path.terminal_id = terminal.id();
        let selected = SelectedTarget {
            session_id: path.session_id,
            workspace_id: path.workspace_id,
            tab_id: path.tab_id,
            pane_id: path.pane_id,
            terminal_id: path.terminal_id,
            child_pid: terminal.child_pid(),
        };
        let insertion = state.register_session(path, Arc::clone(&terminal));
        (terminal, selected, insertion.err())
    };
    if let Some(error) = insertion_error {
        let _ = terminal.close().await;
        return Err(error);
    }
    watch_terminal(terminal, exited.clone());
    Ok(selected)
}

async fn close_session(shared: &Shared, selector: SessionSelector) -> Result<(), DaemonError> {
    let (session_id, handles) = {
        let mut state = shared.lock().await;
        state.begin_session_close(selector)?
    };
    for handle in handles {
        if let Err(error) = handle.close().await {
            let mut state = shared.lock().await;
            let _ = state.resources.cancel_close_session(session_id);
            return Err(DaemonError::command("close_failed", error));
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

    fn inconsistent_state() -> (SharedState, InitialPath) {
        let path = initial_path("test".into(), "/".into(), TerminalId::new());
        let mut resources = ResourceTree::default();
        resources.create_session(path.clone()).unwrap();
        (
            SharedState {
                resources,
                runtimes: HashMap::new(),
                accepting: true,
            },
            path,
        )
    }

    #[test]
    fn missing_runtime_rolls_back_session_close_marker() {
        let (mut state, path) = inconsistent_state();

        let Err(error) = state.begin_session_close(SessionSelector::Id(path.session_id)) else {
            panic!("missing runtime should fail session close");
        };

        assert_eq!(error.code, "resource_error");
        assert!(!state.resources.snapshot().sessions[0].closing);
    }

    #[test]
    fn missing_runtime_does_not_mutate_tree_during_finalization() {
        let (mut state, path) = inconsistent_state();
        let before = state.resources.snapshot();

        assert!(state.finalize_terminal(path.terminal_id).is_err());

        assert_eq!(state.resources.snapshot(), before);
    }

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
