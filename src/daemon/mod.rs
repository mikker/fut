//! The single per-user Fut daemon and its local socket lifecycle.

mod lease;
pub mod path;

pub mod autostart;

use std::{
    collections::{HashMap, HashSet},
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
    sync::{Mutex, broadcast, mpsc, watch},
    task::JoinSet,
    time::{Duration, timeout},
};
use tokio_util::codec::Framed;

use crate::{
    domain::{ClientId, PaneId, SessionId, TabId, TerminalId, TerminalSize, WorkspaceId},
    project::{ProjectError, ProjectResolver, ResolvedLocation},
    protocol::{
        AcknowledgedCommand, ClientMessage, ClientMode, Envelope, OpenDisposition,
        PROTOCOL_VERSION, SelectedTarget, ServerMessage, codec, decode_payload, encode_payload,
    },
    resources::{
        CheckoutDestination, InitialPath, ResourceError, ResourceTree, TabPath, TargetSelector,
        WorkspacePath,
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
    expected_finalizations: HashSet<TerminalId>,
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
            ResourceError::TargetRequired | ResourceError::AmbiguousTarget => "target_required",
            ResourceError::EmptyName => "invalid_name",
            _ => "resource_error",
        };
        Self::new(code, error.to_string())
    }
}

impl From<ProjectError> for DaemonError {
    fn from(error: ProjectError) -> Self {
        let code = match error {
            ProjectError::Input { .. }
            | ProjectError::NotDirectory(_)
            | ProjectError::Canonicalize { .. }
            | ProjectError::BareRepository(_) => "invalid_cwd",
            ProjectError::GitNotFound(_) | ProjectError::GitSpawn(_) => "git_unavailable",
            _ => "project_resolution",
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
        self.expected_finalizations.remove(&terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        Ok(())
    }

    fn register_workspace(
        &mut self,
        session_id: SessionId,
        path: WorkspacePath,
        terminal: Arc<TerminalHandle>,
    ) -> Result<(), DaemonError> {
        self.resources.add_workspace(session_id, path)?;
        self.expected_finalizations.remove(&terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        Ok(())
    }

    fn register_tab(
        &mut self,
        workspace_id: WorkspaceId,
        path: TabPath,
        terminal: Arc<TerminalHandle>,
    ) -> Result<(), DaemonError> {
        self.resources.add_tab(workspace_id, path)?;
        self.expected_finalizations.remove(&terminal.id());
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
        if self.expected_finalizations.remove(&terminal_id) {
            return Ok(false);
        }
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

    fn finalize_terminal_for_replacement(
        &mut self,
        terminal_id: TerminalId,
    ) -> Result<bool, DaemonError> {
        let empty = self.finalize_terminal(terminal_id)?;
        self.expected_finalizations.insert(terminal_id);
        Ok(empty)
    }

    fn begin_target_close(
        &mut self,
        selector: TargetSelector,
    ) -> Result<(CloseScope, Vec<Arc<TerminalHandle>>), DaemonError> {
        if !self.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let scope = match selector {
            TargetSelector::Session(selector) => {
                CloseScope::Session(self.resources.resolve_session(selector)?)
            }
            TargetSelector::Workspace(id) => CloseScope::Workspace(id),
            other => CloseScope::Pane(self.resources.resolve_terminal_target(Some(other))?.pane_id),
        };
        let mut planned = self.resources.clone();
        let terminals = scope.begin(&mut planned)?.terminals_to_close;
        let handles = terminals
            .iter()
            .map(|id| {
                self.runtimes
                    .get(id)
                    .map(|entry| Arc::clone(&entry.handle))
                    .ok_or_else(|| {
                        DaemonError::new(
                            "resource_error",
                            format!("terminal runtime missing for {id}"),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        scope.begin(&mut self.resources)?;
        Ok((scope, handles))
    }

    fn begin_shutdown(&mut self) -> Vec<Arc<TerminalHandle>> {
        self.accepting = false;
        self.runtimes
            .values()
            .map(|entry| Arc::clone(&entry.handle))
            .collect()
    }
}

#[derive(Clone, Copy)]
enum CloseScope {
    Session(SessionId),
    Workspace(WorkspaceId),
    Pane(PaneId),
}

impl CloseScope {
    fn begin(self, tree: &mut ResourceTree) -> Result<crate::resources::Mutation, ResourceError> {
        match self {
            Self::Session(id) => tree.close_session(id),
            Self::Workspace(id) => tree.close_workspace(id),
            Self::Pane(id) => tree.close_pane(id),
        }
    }

    fn cancel(self, tree: &mut ResourceTree) -> Result<crate::resources::Mutation, ResourceError> {
        match self {
            Self::Session(id) => tree.cancel_close_session(id),
            Self::Workspace(id) => tree.cancel_close_workspace(id),
            Self::Pane(id) => tree.cancel_close_pane(id),
        }
    }
}

type Shared = Arc<Mutex<SharedState>>;

struct LeasedTarget {
    selected: SelectedTarget,
    terminal: Arc<TerminalHandle>,
    _lease: lease::LeaseGuard,
}

struct Attachment {
    target: LeasedTarget,
    snapshots: watch::Receiver<crate::domain::ScreenSnapshot>,
    events: broadcast::Receiver<TerminalEvent>,
    lifecycle: watch::Receiver<TerminalLifecycle>,
}

impl Attachment {
    fn new(target: LeasedTarget) -> Self {
        let snapshots = target.terminal.subscribe_snapshots();
        let events = target.terminal.subscribe_events();
        let lifecycle = target.terminal.subscribe_lifecycle();
        Self {
            target,
            snapshots,
            events,
            lifecycle,
        }
    }

    fn exit_code(&self) -> Option<Option<i32>> {
        match *self.lifecycle.borrow() {
            TerminalLifecycle::Running => None,
            TerminalLifecycle::Exited { exit_code } => Some(exit_code),
        }
    }
}

/// Bind Fut's sole socket and run while at least one session is alive.
pub async fn run_daemon(config: DaemonConfig) -> Result<()> {
    let resolved = ProjectResolver::default()
        .resolve(&config.spawn.cwd)
        .await
        .map_err(DaemonError::from)?;
    let socket = bind_socket(&config.socket_path).await?;
    let mut initial_spawn = config.spawn;
    initial_spawn.cwd = resolved.cwd.clone();
    let terminal = Arc::new(spawn_terminal(initial_spawn)?);
    let initial = initial_path(
        &resolved,
        resolved.suggested_session_name.clone(),
        terminal.id(),
    );
    let mut state = SharedState {
        resources: ResourceTree::default(),
        runtimes: HashMap::new(),
        expected_finalizations: HashSet::new(),
        accepting: true,
    };
    if let Err(error) = state.register_session(initial, Arc::clone(&terminal)) {
        let _ = terminal.close().await;
        return Err(error.into());
    }
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

fn initial_path(resolved: &ResolvedLocation, name: String, terminal_id: TerminalId) -> InitialPath {
    InitialPath {
        session_id: SessionId::new(),
        session_name: name,
        project: resolved.project.clone(),
        workspace_id: WorkspaceId::new(),
        workspace_name: resolved.suggested_workspace_name.clone(),
        root: resolved.workspace_root.clone(),
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
    let (leased, interactive_size) = match mode {
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
            let selected = match lease_target(&shared, selector, client).await {
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
    let current_lifecycle = leased
        .as_ref()
        .map(|target| target.terminal.subscribe_lifecycle().borrow().clone());
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
            selected: leased.as_ref().map(|target| target.selected.clone()),
        },
    )
    .await?;

    let Some(requested_size) = interactive_size else {
        return control_loop(&mut framed, shared, exited, shutdown).await;
    };
    let target = leased.expect("interactive connection selected a terminal");
    let mut attachment = Attachment::new(target);
    let mut size = attachment.snapshots.borrow().size;
    match attachment.target.terminal.resize(requested_size).await {
        Ok(()) => size = requested_size,
        Err(error) => send_command_error(&mut framed, None, error).await?,
    }
    let initial_screen = attachment.snapshots.borrow_and_update().clone();
    send(
        &mut framed,
        None,
        ServerMessage::Snapshot {
            terminal_id: attachment.target.selected.terminal_id,
            screen: initial_screen,
        },
    )
    .await?;

    loop {
        // A watch receiver created after the terminal exits considers its current
        // value already seen. Inspect that durable value before waiting so exit
        // delivery never depends on changed() observing a transition.
        if let Some(exit_code) = attachment.exit_code() {
            if attachment.snapshots.has_changed().unwrap_or(false) {
                let screen = attachment.snapshots.borrow_and_update().clone();
                send(
                    &mut framed,
                    None,
                    ServerMessage::Snapshot {
                        terminal_id: attachment.target.selected.terminal_id,
                        screen,
                    },
                )
                .await?;
            }
            send(
                &mut framed,
                None,
                ServerMessage::TerminalExited {
                    terminal_id: attachment.target.selected.terminal_id,
                    exit_code,
                },
            )
            .await?;
            break;
        }

        tokio::select! {
            frame = framed.next() => {
                let Some(frame) = frame else { break };
                let envelope: Envelope<ClientMessage> = decode_payload(&frame?)?;
                match envelope.message {
                    ClientMessage::Input { bytes } => command_response(&mut framed, envelope.request_id, AcknowledgedCommand::Input, attachment.target.terminal.input(bytes).await).await?,
                    ClientMessage::Resize { size: requested } => {
                        if let Err(error) = requested.validate() {
                            send_error(&mut framed, envelope.request_id, "invalid_size", &error.to_string()).await?;
                        } else {
                            let result = attachment.target.terminal.resize(requested).await;
                            if result.is_ok() { size = requested; }
                            command_response(&mut framed, envelope.request_id, AcknowledgedCommand::Resize, result).await?;
                        }
                    }
                    ClientMessage::SelectTarget { selector } => {
                        match resolves_to_current(&shared, &selector, attachment.target.selected.terminal_id).await {
                            Ok(true) => {
                                send(&mut framed, envelope.request_id, ServerMessage::TargetSelected { selected: attachment.target.selected.clone() }).await?;
                                continue;
                            }
                            Err(error) => {
                                send_error(&mut framed, envelope.request_id, error.code, &error.message).await?;
                                continue;
                            }
                            Ok(false) => {}
                        }
                        match switch_candidate(&shared, selector, client, size).await {
                            Ok(mut candidate) => {
                                let screen = candidate.snapshots.borrow_and_update().clone();
                                let terminal_id = candidate.target.selected.terminal_id;
                                let selected = candidate.target.selected.clone();
                                attachment = candidate;
                                send(&mut framed, envelope.request_id, ServerMessage::TargetSelected { selected }).await?;
                                send(&mut framed, None, ServerMessage::Snapshot { terminal_id, screen }).await?;
                            }
                            Err(error) => send_error(&mut framed, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::CreateTab { workspace_id, name, cwd, program, argv } => {
                        let request = CreateTabRequest {
                            workspace_id,
                            name,
                            cwd,
                            program,
                            argv,
                            size,
                        };
                        match create_tab(&shared, &exited, request, CreateTabMode::Attached(client)).await {
                            Ok(CreatedTab::Attached(target)) => {
                                let mut candidate = Attachment::new(target);
                                if let Some(exit_code) = candidate.exit_code() {
                                    send_error(
                                        &mut framed,
                                        envelope.request_id,
                                        "terminal_exited",
                                        &format!("terminal already exited with status {exit_code:?}"),
                                    ).await?;
                                } else {
                                    let screen = candidate.snapshots.borrow_and_update().clone();
                                    let terminal_id = candidate.target.selected.terminal_id;
                                    let selected = candidate.target.selected.clone();
                                    attachment = candidate;
                                    send(&mut framed, envelope.request_id, ServerMessage::TabCreated { selected }).await?;
                                    send(&mut framed, None, ServerMessage::Snapshot { terminal_id, screen }).await?;
                                }
                            }
                            Ok(CreatedTab::Detached(_)) => unreachable!("attached creation returns a lease"),
                            Err(error) => send_error(&mut framed, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::ListResources => {
                        let snapshot = shared.lock().await.resources.snapshot();
                        send(&mut framed, envelope.request_id, ServerMessage::Resources { snapshot }).await?;
                    }
                    ClientMessage::Detach => {
                        drop(attachment);
                        send(&mut framed, envelope.request_id, ServerMessage::Detached).await?;
                        return Ok(());
                    }
                    ClientMessage::Ping => send(&mut framed, envelope.request_id, ServerMessage::Pong { daemon_pid: std::process::id() }).await?,
                    ClientMessage::OpenLocation { .. } | ClientMessage::CloseTarget { .. } | ClientMessage::Shutdown => send_error(&mut framed, envelope.request_id, "control_only", "command requires a control connection").await?,
                    ClientMessage::Hello { .. } => send_error(&mut framed, envelope.request_id, "already_hello", "hello was already received").await?,
                }
            },
            changed = attachment.snapshots.changed() => {
                if changed.is_err() { break; }
                let screen = attachment.snapshots.borrow_and_update().clone();
                send(&mut framed, None, ServerMessage::Snapshot {
                    terminal_id: attachment.target.selected.terminal_id,
                    screen,
                }).await?;
            },
            event = attachment.events.recv() => match event {
                Ok(TerminalEvent::TerminalExited { .. }) => {},
                Ok(TerminalEvent::Error { message }) => send_error(&mut framed, None, "terminal", &message).await?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            changed = attachment.lifecycle.changed() => {
                if changed.is_err() { break; }
                // The durable state is handled at the top of the loop, after any
                // queued final snapshot has had a chance to win this select.
            },
        }
    }
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
            ClientMessage::OpenLocation {
                name,
                cwd,
                program,
                argv,
            } => match open_location(&shared, &exited, name, cwd, program, argv).await {
                Ok((selected, disposition)) => {
                    send(
                        framed,
                        envelope.request_id,
                        ServerMessage::LocationOpened {
                            selected,
                            disposition,
                        },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(framed, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::CreateTab {
                workspace_id,
                name,
                cwd,
                program,
                argv,
            } => match create_tab(
                &shared,
                &exited,
                CreateTabRequest {
                    workspace_id,
                    name,
                    cwd,
                    program,
                    argv,
                    size: TerminalSize {
                        columns: 80,
                        rows: 24,
                    },
                },
                CreateTabMode::Detached,
            )
            .await
            {
                Ok(CreatedTab::Detached(selected)) => {
                    send(
                        framed,
                        envelope.request_id,
                        ServerMessage::TabCreated { selected },
                    )
                    .await?
                }
                Ok(CreatedTab::Attached(_)) => {
                    unreachable!("detached creation does not acquire a lease")
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
            ClientMessage::CloseTarget { selector } => {
                match close_target(&shared, selector).await {
                    Ok(()) => {
                        send(
                            framed,
                            envelope.request_id,
                            ServerMessage::CommandCompleted {
                                command: AcknowledgedCommand::CloseTarget,
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
                {
                    let mut state = shared.lock().await;
                    state.accepting = false;
                }
                let response = send(
                    framed,
                    envelope.request_id,
                    ServerMessage::CommandCompleted {
                        command: AcknowledgedCommand::Shutdown,
                    },
                )
                .await;
                let _ = shutdown.send(true);
                response?;
                break;
            }
            ClientMessage::Detach => {
                send(framed, envelope.request_id, ServerMessage::Detached).await?;
                break;
            }
            ClientMessage::Input { .. }
            | ClientMessage::Resize { .. }
            | ClientMessage::SelectTarget { .. } => {
                send_error(
                    framed,
                    envelope.request_id,
                    "interactive_only",
                    "input, resize, and target selection require an interactive connection",
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

async fn lease_target(
    shared: &Shared,
    selector: Option<TargetSelector>,
    client: ClientId,
) -> Result<LeasedTarget, DaemonError> {
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
    if let TerminalLifecycle::Exited { exit_code } =
        runtime.handle.subscribe_lifecycle().borrow().clone()
    {
        return Err(DaemonError::new(
            "terminal_exited",
            format!("terminal already exited with status {exit_code:?}"),
        ));
    }
    let guard = runtime.lease.acquire(client).ok_or_else(|| {
        DaemonError::new(
            "already_attached",
            "another interactive client holds this terminal's attachment lease",
        )
    })?;
    Ok(LeasedTarget {
        selected: SelectedTarget {
            session_id: target.session_id,
            workspace_id: target.workspace_id,
            tab_id: target.tab_id,
            pane_id: target.pane_id,
            terminal_id: target.terminal_id,
            child_pid: runtime.handle.child_pid(),
        },
        terminal: Arc::clone(&runtime.handle),
        _lease: guard,
    })
}

async fn resolves_to_current(
    shared: &Shared,
    selector: &TargetSelector,
    current: TerminalId,
) -> Result<bool, DaemonError> {
    let state = shared.lock().await;
    if !state.accepting {
        return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
    }
    Ok(state
        .resources
        .resolve_terminal_target(Some(selector.clone()))?
        .terminal_id
        == current)
}

async fn switch_candidate(
    shared: &Shared,
    selector: TargetSelector,
    client: ClientId,
    size: TerminalSize,
) -> Result<Attachment, DaemonError> {
    let target = lease_target(shared, Some(selector), client).await?;
    let attachment = Attachment::new(target);
    if let TerminalLifecycle::Exited { exit_code } = attachment.lifecycle.borrow().clone() {
        return Err(DaemonError::new(
            "terminal_exited",
            format!("terminal already exited with status {exit_code:?}"),
        ));
    }
    attachment
        .target
        .terminal
        .resize(size)
        .await
        .map_err(|error| DaemonError::command("resize_failed", error))?;
    if let TerminalLifecycle::Exited { exit_code } = attachment.lifecycle.borrow().clone() {
        return Err(DaemonError::new(
            "terminal_exited",
            format!("terminal exited with status {exit_code:?}"),
        ));
    }
    Ok(attachment)
}

async fn open_location(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    name: Option<String>,
    cwd: PathBuf,
    program: Option<PathBuf>,
    argv: Vec<String>,
) -> Result<(SelectedTarget, OpenDisposition), DaemonError> {
    // Git and filesystem resolution can block, so it deliberately precedes the state lock.
    let resolved = ProjectResolver::default().resolve(&cwd).await?;
    let program = program.unwrap_or_else(|| {
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/bin/sh".into())
    });
    let (terminal, selected, disposition, insertion_error) = {
        let mut state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let mut destination = state
            .resources
            .checkout_destination(&resolved.project, &resolved.workspace_root)?;
        if let CheckoutDestination::Existing(workspace_id) = destination {
            let path = state
                .resources
                .initial_terminal_for_workspace(workspace_id)?;
            let exited_terminal = state
                .runtimes
                .get(&path.terminal_id)
                .ok_or_else(|| DaemonError::new("resource_error", "terminal runtime missing"))?
                .handle
                .subscribe_lifecycle()
                .borrow()
                .clone();
            if matches!(exited_terminal, TerminalLifecycle::Exited { .. }) {
                state.finalize_terminal_for_replacement(path.terminal_id)?;
                // This accepted open replaces the now-empty checkout rather than
                // turning a natural terminal exit into daemon shutdown.
                state.accepting = true;
                destination = state
                    .resources
                    .checkout_destination(&resolved.project, &resolved.workspace_root)?;
                if let CheckoutDestination::Existing(workspace_id) = destination {
                    let replacement = state
                        .resources
                        .initial_terminal_for_workspace(workspace_id)?;
                    let runtime =
                        state
                            .runtimes
                            .get(&replacement.terminal_id)
                            .ok_or_else(|| {
                                DaemonError::new("resource_error", "terminal runtime missing")
                            })?;
                    return Ok((
                        selected_target(replacement, &runtime.handle),
                        OpenDisposition::Existing,
                    ));
                }
            } else {
                let runtime = &state.runtimes[&path.terminal_id];
                return Ok((
                    selected_target(path, &runtime.handle),
                    OpenDisposition::Existing,
                ));
            }
        }
        let session_name = name.clone().unwrap_or_else(|| {
            state
                .resources
                .available_session_name(&resolved.suggested_session_name)
        });
        let workspace_name = match destination {
            CheckoutDestination::AddWorkspace { session_id } => name.clone().unwrap_or_else(|| {
                state
                    .resources
                    .available_workspace_name(session_id, &resolved.suggested_workspace_name)
            }),
            _ => resolved.suggested_workspace_name.clone(),
        };
        let proposed_session = initial_path(&resolved, session_name, TerminalId::new());
        let proposed_workspace = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name,
            root: resolved.workspace_root.clone(),
            tab_id: TabId::new(),
            tab_name: "shell".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let mut validated = state.resources.clone();
        match destination {
            CheckoutDestination::CreateSession => {
                validated.create_session(proposed_session.clone())?;
            }
            CheckoutDestination::AddWorkspace { session_id } => {
                validated.add_workspace(session_id, proposed_workspace.clone())?;
            }
            CheckoutDestination::Existing(_) => unreachable!(),
        }
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                program,
                argv,
                cwd: resolved.cwd.clone(),
                env: std::env::vars().collect(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .map_err(|error| DaemonError::new("spawn_failed", error.to_string()))?,
        );
        let (path, disposition, insertion) = match destination {
            CheckoutDestination::CreateSession => {
                let mut path = proposed_session;
                path.terminal_id = terminal.id();
                let selected = crate::resources::ResolvedTerminalPath {
                    session_id: path.session_id,
                    workspace_id: path.workspace_id,
                    tab_id: path.tab_id,
                    pane_id: path.pane_id,
                    terminal_id: path.terminal_id,
                };
                let insertion = state.register_session(path, Arc::clone(&terminal));
                (selected, OpenDisposition::SessionCreated, insertion)
            }
            CheckoutDestination::AddWorkspace { session_id } => {
                let mut path = proposed_workspace;
                path.terminal_id = terminal.id();
                let selected = crate::resources::ResolvedTerminalPath {
                    session_id,
                    workspace_id: path.workspace_id,
                    tab_id: path.tab_id,
                    pane_id: path.pane_id,
                    terminal_id: path.terminal_id,
                };
                let insertion = state.register_workspace(session_id, path, Arc::clone(&terminal));
                (selected, OpenDisposition::WorkspaceCreated, insertion)
            }
            CheckoutDestination::Existing(_) => unreachable!(),
        };
        let selected = selected_target(path, &terminal);
        (terminal, selected, disposition, insertion.err())
    };
    if let Some(error) = insertion_error {
        let _ = terminal.close().await;
        return Err(error);
    }
    watch_terminal(terminal, exited.clone());
    Ok((selected, disposition))
}

struct CreateTabRequest {
    workspace_id: WorkspaceId,
    name: Option<String>,
    cwd: Option<PathBuf>,
    program: Option<PathBuf>,
    argv: Vec<String>,
    size: TerminalSize,
}

enum CreateTabMode {
    Detached,
    Attached(ClientId),
}

enum CreatedTab {
    Detached(SelectedTarget),
    Attached(LeasedTarget),
}

async fn create_tab(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    request: CreateTabRequest,
    mode: CreateTabMode,
) -> Result<CreatedTab, DaemonError> {
    let CreateTabRequest {
        workspace_id,
        name,
        cwd,
        program,
        argv,
        size,
    } = request;
    let root = {
        let state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        state.resources.workspace_root(workspace_id)?.to_path_buf()
    };
    let cwd = match cwd {
        None => root,
        Some(cwd) => {
            let candidate = if cwd.is_absolute() {
                cwd
            } else {
                root.join(cwd)
            };
            let canonical = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
                DaemonError::new(
                    "invalid_cwd",
                    format!("could not resolve {}: {error}", candidate.display()),
                )
            })?;
            let metadata = tokio::fs::metadata(&canonical).await.map_err(|error| {
                DaemonError::new(
                    "invalid_cwd",
                    format!("could not inspect {}: {error}", canonical.display()),
                )
            })?;
            if !metadata.is_dir() {
                return Err(DaemonError::new(
                    "invalid_cwd",
                    format!("not a directory: {}", canonical.display()),
                ));
            }
            canonical
        }
    };
    let program = program.unwrap_or_else(|| {
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/bin/sh".into())
    });

    let (terminal, creation) = {
        let mut state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let tab_name = match name {
            Some(name) => name,
            None => state.resources.available_tab_name(workspace_id, "shell")?,
        };
        let proposed = TabPath {
            tab_id: TabId::new(),
            tab_name,
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let session_id = state.resources.session_id_for_workspace(workspace_id)?;
        let mut validated = state.resources.clone();
        validated.add_tab(workspace_id, proposed.clone())?;

        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                program,
                argv,
                cwd,
                env: std::env::vars().collect(),
                size,
            })
            .map_err(|error| DaemonError::new("spawn_failed", error.to_string()))?,
        );
        let mut path = proposed;
        path.terminal_id = terminal.id();
        let resolved = crate::resources::ResolvedTerminalPath {
            session_id,
            workspace_id,
            tab_id: path.tab_id,
            pane_id: path.pane_id,
            terminal_id: path.terminal_id,
        };
        let selected = selected_target(resolved, &terminal);
        let insertion = state.register_tab(workspace_id, path, Arc::clone(&terminal));
        let creation = match insertion {
            Ok(()) => Ok(match mode {
                CreateTabMode::Detached => CreatedTab::Detached(selected.clone()),
                CreateTabMode::Attached(client) => {
                    let runtime = state
                        .runtimes
                        .get(&terminal.id())
                        .expect("new runtime inserted");
                    let guard = runtime
                        .lease
                        .acquire(client)
                        .expect("new terminal has an independent lease");
                    let target = LeasedTarget {
                        selected: selected.clone(),
                        terminal: Arc::clone(&terminal),
                        _lease: guard,
                    };
                    CreatedTab::Attached(target)
                }
            }),
            Err(error) => Err(error),
        };
        (terminal, creation)
    };
    let created = match creation {
        Ok(created) => created,
        Err(error) => {
            let _ = terminal.close().await;
            return Err(error);
        }
    };
    watch_terminal(terminal, exited.clone());
    Ok(created)
}

fn selected_target(
    path: crate::resources::ResolvedTerminalPath,
    terminal: &TerminalHandle,
) -> SelectedTarget {
    SelectedTarget {
        session_id: path.session_id,
        workspace_id: path.workspace_id,
        tab_id: path.tab_id,
        pane_id: path.pane_id,
        terminal_id: path.terminal_id,
        child_pid: terminal.child_pid(),
    }
}

async fn close_target(shared: &Shared, selector: TargetSelector) -> Result<(), DaemonError> {
    let (scope, handles) = {
        let mut state = shared.lock().await;
        state.begin_target_close(selector)?
    };
    for handle in handles {
        if let Err(error) = handle.close().await {
            let mut state = shared.lock().await;
            let _ = scope.cancel(&mut state.resources);
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
        let resolved = ResolvedLocation {
            cwd: "/".into(),
            project: crate::resources::Project {
                identity: crate::resources::ProjectIdentity::CanonicalDirectory("/".into()),
            },
            workspace_root: "/".into(),
            suggested_session_name: "test".into(),
            suggested_workspace_name: "root".into(),
            workspace_kind: crate::project::WorkspaceKind::Directory,
        };
        let path = initial_path(&resolved, "test".into(), TerminalId::new());
        let mut resources = ResourceTree::default();
        resources.create_session(path.clone()).unwrap();
        (
            SharedState {
                resources,
                runtimes: HashMap::new(),
                expected_finalizations: HashSet::new(),
                accepting: true,
            },
            path,
        )
    }

    #[test]
    fn missing_runtime_rolls_back_session_close_marker() {
        let (mut state, path) = inconsistent_state();

        let Err(error) = state.begin_target_close(TargetSelector::Session(
            crate::resources::SessionSelector::Id(path.session_id),
        )) else {
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
    fn expected_duplicate_is_consumed_exactly_once_without_mutation() {
        let (mut state, path) = inconsistent_state();
        state.expected_finalizations.insert(path.terminal_id);
        let before = state.resources.snapshot();

        assert!(!state.finalize_terminal(path.terminal_id).unwrap());
        assert_eq!(state.resources.snapshot(), before);
        assert!(state.finalize_terminal(path.terminal_id).is_err());
        assert_eq!(state.resources.snapshot(), before);
    }

    #[test]
    fn proactive_last_terminal_replacement_can_keep_accepting() {
        let (mut state, path) = inconsistent_state();
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "exit".into()],
                cwd: "/".into(),
                env: HashMap::new(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .unwrap(),
        );
        let old_id = path.terminal_id;
        state.runtimes.insert(
            old_id,
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );

        assert!(state.finalize_terminal_for_replacement(old_id).unwrap());
        assert!(!state.accepting);
        state.accepting = true;

        assert!(state.accepting);
        assert!(!state.finalize_terminal(old_id).unwrap());
        assert!(state.accepting);
    }

    #[test]
    fn shutdown_stops_acceptance_before_runtime_collection() {
        let (mut state, _) = inconsistent_state();
        assert!(state.accepting);

        let handles = state.begin_shutdown();

        assert!(!state.accepting);
        assert!(handles.is_empty());
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
