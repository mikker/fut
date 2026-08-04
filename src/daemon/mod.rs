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
    sync::{Mutex, mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::{Duration, timeout},
};
use tokio_util::codec::Framed;

use crate::{
    domain::{ClientId, PaneId, SessionId, TabId, TerminalId, TerminalSize, WorkspaceId},
    project::{ProjectError, ProjectResolver, ResolvedLocation},
    protocol::{
        AcknowledgedCommand, ClientMessage, ClientMode, Envelope, OpenDisposition,
        PROTOCOL_VERSION, RenameSelector, SelectedTarget, SelectedView, ServerMessage, codec,
        decode_payload, encode_payload,
    },
    resources::{
        CheckoutDestination, InitialPath, ResourceError, ResourceTree, TabPath, TargetSelector,
        WorkspacePath,
    },
    splits::{SplitDirection, SplitTree},
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
    resource_changes: watch::Sender<u64>,
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
            ResourceError::DifferentWorkspace => "different_workspace",
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
    fn publish_resource_change(&self, revision: u64) {
        if revision > *self.resource_changes.borrow() {
            self.resource_changes.send_replace(revision);
        }
    }

    fn register_session(
        &mut self,
        path: InitialPath,
        terminal: Arc<TerminalHandle>,
    ) -> Result<(), DaemonError> {
        let mutation = self.resources.create_session(path)?;
        self.expected_finalizations.remove(&terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        self.publish_resource_change(mutation.revision);
        Ok(())
    }

    fn register_workspace(
        &mut self,
        session_id: SessionId,
        path: WorkspacePath,
        terminal: Arc<TerminalHandle>,
    ) -> Result<(), DaemonError> {
        let mutation = self.resources.add_workspace(session_id, path)?;
        self.expected_finalizations.remove(&terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        self.publish_resource_change(mutation.revision);
        Ok(())
    }

    fn register_tab(
        &mut self,
        workspace_id: WorkspaceId,
        path: TabPath,
        terminal: Arc<TerminalHandle>,
    ) -> Result<(), DaemonError> {
        let mutation = self.resources.add_tab(workspace_id, path)?;
        self.expected_finalizations.remove(&terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        self.publish_resource_change(mutation.revision);
        Ok(())
    }

    fn register_pane(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        terminal: Arc<TerminalHandle>,
    ) -> Result<(), DaemonError> {
        let mutation = self.resources.add_pane(tab_id, pane_id, terminal.id())?;
        self.expected_finalizations.remove(&terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        self.publish_resource_change(mutation.revision);
        Ok(())
    }

    fn register_split_pane(
        &mut self,
        anchor: PaneId,
        direction: SplitDirection,
        pane_id: PaneId,
        terminal: Arc<TerminalHandle>,
    ) -> Result<(), DaemonError> {
        let mutation = self
            .resources
            .split_pane(anchor, direction, pane_id, terminal.id())?;
        self.expected_finalizations.remove(&terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        self.publish_resource_change(mutation.revision);
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
        self.publish_resource_change(mutation.revision);
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
            TargetSelector::Tab(id) => CloseScope::Tab(id),
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
        let mutation = scope.begin(&mut self.resources)?;
        self.publish_resource_change(mutation.revision);
        Ok((scope, handles))
    }

    fn cancel_target_close(&mut self, scope: CloseScope) {
        if let Ok(mutation) = scope.cancel(&mut self.resources) {
            self.publish_resource_change(mutation.revision);
        }
    }

    fn move_pane(
        &mut self,
        pane_id: PaneId,
        destination_tab_id: TabId,
    ) -> Result<PaneMove, DaemonError> {
        if !self.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let source = self
            .resources
            .resolve_terminal_target(Some(TargetSelector::Pane(pane_id)))?;
        let runtime = self
            .runtimes
            .get(&source.terminal_id)
            .ok_or_else(|| DaemonError::new("not_found", "terminal runtime not found"))?;
        if !matches!(
            *runtime.handle.subscribe_lifecycle().borrow(),
            TerminalLifecycle::Running
        ) {
            return Err(DaemonError::new(
                "terminal_exited",
                "terminal is not running",
            ));
        }
        let source_tab_id = source.tab_id;
        let moved = source_tab_id != destination_tab_id;
        let mutation = self.resources.move_pane(pane_id, destination_tab_id)?;
        self.publish_resource_change(mutation.revision);
        let source_tab_closed = mutation.events.iter().any(|event| {
            matches!(
                event,
                crate::resources::ResourceEvent::TabClosed { tab_id }
                    if *tab_id == source_tab_id
            )
        });
        let fresh = self
            .resources
            .resolve_terminal_target(Some(TargetSelector::Pane(pane_id)))?;
        let runtime = self
            .runtimes
            .get(&fresh.terminal_id)
            .expect("moving a pane preserves its runtime");
        Ok(PaneMove {
            source_tab_id,
            moved,
            source_tab_closed,
            selected: selected_target(fresh, &runtime.handle),
        })
    }

    fn rename_target(&mut self, selector: RenameSelector, name: String) -> Result<(), DaemonError> {
        if !self.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let mutation = match selector {
            RenameSelector::Session(selector) => {
                let session_id = self.resources.resolve_session(selector)?;
                self.resources.rename_session(session_id, name)?
            }
            RenameSelector::Workspace(id) => self.resources.rename_workspace(id, name)?,
            RenameSelector::Tab(id) => self.resources.rename_tab(id, name)?,
        };
        self.publish_resource_change(mutation.revision);
        Ok(())
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
    Tab(TabId),
    Pane(PaneId),
}

impl CloseScope {
    fn begin(self, tree: &mut ResourceTree) -> Result<crate::resources::Mutation, ResourceError> {
        match self {
            Self::Session(id) => tree.close_session(id),
            Self::Workspace(id) => tree.close_workspace(id),
            Self::Tab(id) => tree.close_tab(id),
            Self::Pane(id) => tree.close_pane(id),
        }
    }

    fn cancel(self, tree: &mut ResourceTree) -> Result<crate::resources::Mutation, ResourceError> {
        match self {
            Self::Session(id) => tree.cancel_close_session(id),
            Self::Workspace(id) => tree.cancel_close_workspace(id),
            Self::Tab(id) => tree.cancel_close_tab(id),
            Self::Pane(id) => tree.cancel_close_pane(id),
        }
    }
}

type Shared = Arc<Mutex<SharedState>>;

struct PaneMove {
    source_tab_id: TabId,
    moved: bool,
    source_tab_closed: bool,
    selected: SelectedTarget,
}

struct LeasedTarget {
    selected: SelectedTarget,
    terminal: Arc<TerminalHandle>,
    _lease: lease::LeaseGuard,
}

struct ObservedTarget {
    selected: SelectedTarget,
    terminal: Arc<TerminalHandle>,
}

const ATTACHMENT_UPDATE_CAPACITY: usize = 8;

enum AttachmentUpdate {
    Snapshot {
        terminal_id: TerminalId,
        generation: u64,
        screen: crate::domain::ScreenSnapshot,
    },
    Error {
        terminal_id: TerminalId,
        generation: u64,
        message: String,
    },
    Exited {
        terminal_id: TerminalId,
        generation: u64,
        exit_code: Option<i32>,
    },
}

struct Attachment {
    panes: Vec<ObservedTarget>,
    focused: LeasedTarget,
    layout: SplitTree,
    fallback_terminal_ids: Vec<TerminalId>,
    resource_revision: u64,
    streamed_resource_revision: u64,
    resource_changes: watch::Receiver<u64>,
    updates: mpsc::Receiver<AttachmentUpdate>,
    update_sender: mpsc::Sender<AttachmentUpdate>,
    watchers: HashMap<TerminalId, (u64, JoinHandle<()>)>,
    next_watcher_generation: u64,
}

impl Attachment {
    fn new(
        panes: Vec<ObservedTarget>,
        focused: LeasedTarget,
        layout: SplitTree,
        fallback_terminal_ids: Vec<TerminalId>,
        resource_revision: u64,
        resource_changes: watch::Receiver<u64>,
    ) -> Self {
        let (update_sender, updates) = mpsc::channel(ATTACHMENT_UPDATE_CAPACITY);
        let mut attachment = Self {
            panes,
            focused,
            layout,
            fallback_terminal_ids,
            resource_revision,
            streamed_resource_revision: resource_revision,
            resource_changes,
            updates,
            update_sender,
            watchers: HashMap::new(),
            next_watcher_generation: 0,
        };
        attachment.reconcile_watchers();
        attachment
    }

    fn selected(&self) -> SelectedView {
        SelectedView {
            resource_revision: self.resource_revision,
            focused: self.focused.selected.clone(),
            panes: self
                .panes
                .iter()
                .map(|pane| pane.selected.clone())
                .collect(),
            layout: self.layout.clone(),
        }
    }

    fn focused_terminal(&self) -> &Arc<TerminalHandle> {
        &self.focused.terminal
    }

    fn reconcile(
        &mut self,
        panes: Vec<ObservedTarget>,
        focused: SelectedTarget,
        layout: SplitTree,
        fallback_terminal_ids: Vec<TerminalId>,
        resource_revision: u64,
    ) -> bool {
        let changed = self.focused.selected != focused
            || self.layout != layout
            || self.panes.len() != panes.len()
            || self
                .panes
                .iter()
                .zip(&panes)
                .any(|(current, next)| current.selected != next.selected);
        self.focused.selected = focused;
        self.layout = layout;
        self.fallback_terminal_ids = fallback_terminal_ids;
        self.replace_panes(panes, resource_revision);
        changed
    }

    fn observe_revision(&mut self, resource_revision: u64) {
        self.resource_revision = self.resource_revision.max(resource_revision);
    }

    fn observe_streamed_revision(&mut self, resource_revision: u64) {
        self.streamed_resource_revision = self.streamed_resource_revision.max(resource_revision);
    }

    fn replace_panes(&mut self, panes: Vec<ObservedTarget>, resource_revision: u64) {
        self.panes = panes;
        self.resource_revision = resource_revision;
        self.reconcile_watchers();
    }

    fn remove(&mut self, terminal_id: TerminalId) -> bool {
        let Some(index) = self
            .panes
            .iter()
            .position(|pane| pane.selected.terminal_id == terminal_id)
        else {
            return false;
        };
        let pane_id = self.panes[index].selected.pane_id;
        if let Some((_, task)) = self.watchers.remove(&terminal_id) {
            task.abort();
        }
        self.panes.remove(index);
        if let Some(layout) = self.layout.clone().without(pane_id) {
            self.layout = layout;
        }
        true
    }

    fn accepts(&self, terminal_id: TerminalId, generation: u64) -> bool {
        self.watchers
            .get(&terminal_id)
            .is_some_and(|(current, _)| *current == generation)
    }

    fn reconcile_watchers(&mut self) {
        let desired = self
            .panes
            .iter()
            .map(|pane| pane.selected.terminal_id)
            .collect::<HashSet<_>>();
        let removed = self
            .watchers
            .keys()
            .filter(|terminal_id| !desired.contains(terminal_id))
            .copied()
            .collect::<Vec<_>>();
        for terminal_id in removed {
            if let Some((_, task)) = self.watchers.remove(&terminal_id) {
                task.abort();
            }
        }
        for pane in &self.panes {
            let terminal_id = pane.selected.terminal_id;
            if self.watchers.contains_key(&terminal_id) {
                continue;
            }
            self.next_watcher_generation += 1;
            let generation = self.next_watcher_generation;
            let task = watch_attachment(
                Arc::clone(&pane.terminal),
                generation,
                self.update_sender.clone(),
            );
            self.watchers.insert(terminal_id, (generation, task));
        }
    }

    fn tab_id(&self) -> TabId {
        self.focused.selected.tab_id
    }

    fn all_running(&self) -> Result<(), DaemonError> {
        if let TerminalLifecycle::Exited { exit_code } =
            self.focused.terminal.subscribe_lifecycle().borrow().clone()
        {
            return Err(DaemonError::new(
                "terminal_exited",
                format!("terminal already exited with status {exit_code:?}"),
            ));
        }
        Ok(())
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        for (_, task) in self.watchers.values() {
            task.abort();
        }
    }
}

fn watch_attachment(
    terminal: Arc<TerminalHandle>,
    generation: u64,
    updates: mpsc::Sender<AttachmentUpdate>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let terminal_id = terminal.id();
        let mut snapshots = terminal.subscribe_snapshots();
        let mut events = terminal.subscribe_events();
        let mut lifecycle = terminal.subscribe_lifecycle();

        let screen = snapshots.borrow_and_update().clone();
        if updates
            .send(AttachmentUpdate::Snapshot {
                terminal_id,
                generation,
                screen,
            })
            .await
            .is_err()
        {
            return;
        }

        loop {
            let exit_code = match lifecycle.borrow().clone() {
                TerminalLifecycle::Running => None,
                TerminalLifecycle::Exited { exit_code } => Some(exit_code),
            };
            if let Some(exit_code) = exit_code {
                if snapshots.has_changed().unwrap_or(false) {
                    let screen = snapshots.borrow_and_update().clone();
                    if updates
                        .send(AttachmentUpdate::Snapshot {
                            terminal_id,
                            generation,
                            screen,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = updates
                    .send(AttachmentUpdate::Exited {
                        terminal_id,
                        generation,
                        exit_code,
                    })
                    .await;
                return;
            }

            tokio::select! {
                changed = snapshots.changed() => {
                    if changed.is_err() { return; }
                    let screen = snapshots.borrow_and_update().clone();
                    if updates.send(AttachmentUpdate::Snapshot { terminal_id, generation, screen }).await.is_err() {
                        return;
                    }
                }
                event = events.recv() => match event {
                    Ok(TerminalEvent::TerminalExited { .. }) => {}
                    Ok(TerminalEvent::Error { message }) => {
                        if updates
                            .send(AttachmentUpdate::Error {
                                terminal_id,
                                generation,
                                message,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                changed = lifecycle.changed() => {
                    if changed.is_err() { return; }
                    // Durable exit state is handled at the top, after any final snapshot.
                }
            }
        }
    })
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
    let (resource_changes, _) = watch::channel(0);
    let mut state = SharedState {
        resources: ResourceTree::default(),
        runtimes: HashMap::new(),
        expected_finalizations: HashSet::new(),
        resource_changes,
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
            let attachment = match lease_view(&shared, selector, client).await {
                Ok(attachment) => attachment,
                Err(error) => {
                    send_error(&mut framed, first.request_id, error.code, &error.message).await?;
                    return Ok(());
                }
            };
            if let Err(error) = attachment.all_running() {
                send_error(&mut framed, first.request_id, error.code, &error.message).await?;
                return Ok(());
            }
            (Some(attachment), Some(size))
        }
        ClientMode::Control => (None, None),
    };
    send(
        &mut framed,
        first.request_id,
        ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").into(),
            selected: leased.as_ref().map(Attachment::selected),
        },
    )
    .await?;

    let Some(spawn_size) = interactive_size else {
        return control_loop(&mut framed, shared, exited, shutdown).await;
    };
    let mut attachment = leased.expect("interactive connection selected a tab view");

    loop {
        tokio::select! {
            frame = framed.next() => {
                let Some(frame) = frame else { break };
                let envelope: Envelope<ClientMessage> = decode_payload(&frame?)?;
                match envelope.message {
                    ClientMessage::Input { bytes } => command_response(
                        &mut framed,
                        envelope.request_id,
                        AcknowledgedCommand::Input,
                        attachment.focused_terminal().input(bytes).await,
                    ).await?,
                    ClientMessage::Resize { terminal_id, size } => {
                        if let Err(error) = size.validate() {
                            send_error(&mut framed, envelope.request_id, "invalid_size", &error.to_string()).await?;
                        } else if terminal_id == attachment.focused.selected.terminal_id {
                            command_response(
                                &mut framed,
                                envelope.request_id,
                                AcknowledgedCommand::Resize,
                                attachment.focused_terminal().resize(size).await,
                            ).await?;
                        } else {
                            send_error(
                                &mut framed,
                                envelope.request_id,
                                "not_focused",
                                "only the focused terminal may be resized",
                            ).await?;
                        }
                    }
                    ClientMessage::SelectTarget { selector } => {
                        let selection = match observe_selection(
                            &shared,
                            &selector,
                            attachment.focused.selected.terminal_id,
                        ).await {
                            Ok(selection) => selection,
                            Err(error) => {
                                send_error(&mut framed, envelope.request_id, error.code, &error.message).await?;
                                continue;
                            }
                        };
                        if let TargetSelection::Focused {
                            selected,
                            panes,
                            layout,
                            fallback_terminal_ids,
                            resource_revision,
                        } = selection
                        {
                            attachment.reconcile(
                                panes,
                                selected,
                                layout,
                                fallback_terminal_ids,
                                resource_revision,
                            );
                            send(
                                &mut framed,
                                envelope.request_id,
                                ServerMessage::TargetSelected { selected: attachment.selected() },
                            ).await?;
                            continue;
                        }
                        match switch_candidate(&shared, selector, client).await {
                            Ok(candidate) => {
                                attachment = candidate;
                                send(
                                    &mut framed,
                                    envelope.request_id,
                                    ServerMessage::TargetSelected { selected: attachment.selected() },
                                ).await?;
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
                            size: spawn_size,
                        };
                        match create_tab(&shared, &exited, request, CreationMode::Attached(client)).await {
                            Ok(CreatedTerminal::Attached(target)) => {
                                let selected = target.selected.clone();
                                match focus_leased_attachment(&shared, &mut attachment, target).await {
                                    Ok(()) => {
                                        send(&mut framed, envelope.request_id, ServerMessage::TabCreated { selected }).await?;
                                        send(
                                            &mut framed,
                                            envelope.request_id,
                                            ServerMessage::TargetSelected { selected: attachment.selected() },
                                        ).await?;
                                    }
                                    Err(error) => send_error(
                                        &mut framed,
                                        envelope.request_id,
                                        error.code,
                                        &error.message,
                                    ).await?,
                                }
                            }
                            Ok(CreatedTerminal::Detached(_)) => unreachable!("attached creation returns a lease"),
                            Err(error) => send_error(&mut framed, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::CreatePane { tab_id, cwd, program, argv } => {
                        if tab_id != attachment.tab_id() {
                            send_error(
                                &mut framed,
                                envelope.request_id,
                                "different_tab",
                                "interactive pane creation requires the currently selected tab",
                            ).await?;
                            continue;
                        }
                        let request = CreatePaneRequest {
                            target: PaneCreationTarget::Append(tab_id),
                            cwd,
                            program,
                            argv,
                            size: spawn_size,
                        };
                        match create_pane(&shared, &exited, request, CreationMode::Attached(client)).await {
                            Ok(CreatedTerminal::Attached(target)) => {
                                let selected = target.selected.clone();
                                match focus_leased_attachment(&shared, &mut attachment, target).await {
                                    Ok(()) => {
                                        send(&mut framed, envelope.request_id, ServerMessage::PaneCreated { selected }).await?;
                                        send(
                                            &mut framed,
                                            envelope.request_id,
                                            ServerMessage::TargetSelected { selected: attachment.selected() },
                                        ).await?;
                                    }
                                    Err(error) => send_error(
                                        &mut framed,
                                        envelope.request_id,
                                        error.code,
                                        &error.message,
                                    ).await?,
                                }
                            }
                            Ok(CreatedTerminal::Detached(_)) => unreachable!("attached creation returns a lease"),
                            Err(error) => send_error(&mut framed, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::SplitPane { pane_id, direction, cwd, program, argv } => {
                        if pane_id != attachment.focused.selected.pane_id {
                            send_error(
                                &mut framed,
                                envelope.request_id,
                                "not_focused",
                                "interactive splitting requires the focused pane",
                            ).await?;
                            continue;
                        }
                        let request = CreatePaneRequest {
                            target: PaneCreationTarget::Split { anchor: pane_id, direction },
                            cwd,
                            program,
                            argv,
                            size: spawn_size,
                        };
                        match create_pane(&shared, &exited, request, CreationMode::Attached(client)).await {
                            Ok(CreatedTerminal::Attached(target)) => {
                                let selected = target.selected.clone();
                                match focus_leased_attachment(&shared, &mut attachment, target).await {
                                    Ok(()) => {
                                        send(&mut framed, envelope.request_id, ServerMessage::PaneCreated { selected }).await?;
                                        send(
                                            &mut framed,
                                            envelope.request_id,
                                            ServerMessage::TargetSelected { selected: attachment.selected() },
                                        ).await?;
                                    }
                                    Err(error) => send_error(&mut framed, envelope.request_id, error.code, &error.message).await?,
                                }
                            }
                            Ok(CreatedTerminal::Detached(_)) => unreachable!("attached split returns a lease"),
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
                    ClientMessage::OpenLocation { .. } | ClientMessage::MovePane { .. } | ClientMessage::CloseTarget { .. } | ClientMessage::RenameTarget { .. } | ClientMessage::Shutdown => send_error(&mut framed, envelope.request_id, "control_only", "command requires a control connection").await?,
                    ClientMessage::Hello { .. } => send_error(&mut framed, envelope.request_id, "already_hello", "hello was already received").await?,
                }
            },
            changed = attachment.resource_changes.changed() => {
                if changed.is_err() {
                    break;
                }
                if let Some(reconciled) = reconcile_attachment(&shared, &mut attachment).await? {
                    if reconciled.view_changed {
                        send(
                            &mut framed,
                            None,
                            ServerMessage::TargetSelected { selected: attachment.selected() },
                        ).await?;
                    }
                    send(
                        &mut framed,
                        None,
                        ServerMessage::ResourcesChanged {
                            snapshot: reconciled.snapshot,
                        },
                    ).await?;
                }
            }
            update = attachment.updates.recv() => match update {
                Some(AttachmentUpdate::Snapshot { terminal_id, generation, screen }) => {
                    if attachment.accepts(terminal_id, generation) {
                        send(&mut framed, None, ServerMessage::Snapshot { terminal_id, screen }).await?;
                    }
                }
                Some(AttachmentUpdate::Error { terminal_id, generation, message }) => {
                    if attachment.accepts(terminal_id, generation) {
                        send_error(&mut framed, None, "terminal", &message).await?;
                    }
                }
                Some(AttachmentUpdate::Exited { terminal_id, generation, exit_code }) => {
                    if !attachment.accepts(terminal_id, generation) {
                        continue;
                    }
                    let focused = terminal_id == attachment.focused.selected.terminal_id;
                    let replacements = if focused {
                        exit_replacement_ids(&shared, &attachment, terminal_id).await
                    } else {
                        Vec::new()
                    };
                    send(
                        &mut framed,
                        None,
                        ServerMessage::TerminalExited { terminal_id, exit_code },
                    ).await?;
                    if focused {
                        let mut replacement = None;
                        for terminal_id in replacements {
                            if let Ok(candidate) = lease_view(
                                &shared,
                                Some(TargetSelector::Terminal(terminal_id)),
                                client,
                            ).await
                                && candidate.all_running().is_ok()
                            {
                                replacement = Some(candidate);
                                break;
                            }
                        }
                        let Some(replacement) = replacement else {
                            break;
                        };
                        attachment = replacement;
                    } else {
                        attachment.remove(terminal_id);
                    }
                    send(
                        &mut framed,
                        None,
                        ServerMessage::TargetSelected { selected: attachment.selected() },
                    ).await?;
                    let snapshot = shared.lock().await.resources.snapshot();
                    send(
                        &mut framed,
                        None,
                        ServerMessage::ResourcesChanged { snapshot },
                    ).await?;
                }
                None => break,
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
                CreationMode::Detached,
            )
            .await
            {
                Ok(CreatedTerminal::Detached(selected)) => {
                    send(
                        framed,
                        envelope.request_id,
                        ServerMessage::TabCreated { selected },
                    )
                    .await?
                }
                Ok(CreatedTerminal::Attached(_)) => {
                    unreachable!("detached creation does not acquire a lease")
                }
                Err(error) => {
                    send_error(framed, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::CreatePane {
                tab_id,
                cwd,
                program,
                argv,
            } => match create_pane(
                &shared,
                &exited,
                CreatePaneRequest {
                    target: PaneCreationTarget::Append(tab_id),
                    cwd,
                    program,
                    argv,
                    size: TerminalSize {
                        columns: 80,
                        rows: 24,
                    },
                },
                CreationMode::Detached,
            )
            .await
            {
                Ok(CreatedTerminal::Detached(selected)) => {
                    send(
                        framed,
                        envelope.request_id,
                        ServerMessage::PaneCreated { selected },
                    )
                    .await?
                }
                Ok(CreatedTerminal::Attached(_)) => {
                    unreachable!("detached creation does not acquire a lease")
                }
                Err(error) => {
                    send_error(framed, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::SplitPane {
                pane_id,
                direction,
                cwd,
                program,
                argv,
            } => match create_pane(
                &shared,
                &exited,
                CreatePaneRequest {
                    target: PaneCreationTarget::Split {
                        anchor: pane_id,
                        direction,
                    },
                    cwd,
                    program,
                    argv,
                    size: TerminalSize {
                        columns: 80,
                        rows: 24,
                    },
                },
                CreationMode::Detached,
            )
            .await
            {
                Ok(created) => {
                    let selected = match created {
                        CreatedTerminal::Detached(selected) => selected,
                        CreatedTerminal::Attached(_) => unreachable!("control split is detached"),
                    };
                    send(
                        framed,
                        envelope.request_id,
                        ServerMessage::PaneCreated { selected },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(framed, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::MovePane {
                pane_id,
                destination_tab_id,
            } => {
                let result = shared.lock().await.move_pane(pane_id, destination_tab_id);
                match result {
                    Ok(moved) => {
                        send(
                            framed,
                            envelope.request_id,
                            ServerMessage::PaneMoved {
                                source_tab_id: moved.source_tab_id,
                                moved: moved.moved,
                                source_tab_closed: moved.source_tab_closed,
                                selected: moved.selected,
                            },
                        )
                        .await?
                    }
                    Err(error) => {
                        send_error(framed, envelope.request_id, error.code, &error.message).await?
                    }
                }
            }
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
            ClientMessage::RenameTarget { selector, name } => {
                let result = shared.lock().await.rename_target(selector, name);
                match result {
                    Ok(()) => {
                        send(
                            framed,
                            envelope.request_id,
                            ServerMessage::CommandCompleted {
                                command: AcknowledgedCommand::RenameTarget,
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

async fn lease_view(
    shared: &Shared,
    selector: Option<TargetSelector>,
    client: ClientId,
) -> Result<Attachment, DaemonError> {
    let state = shared.lock().await;
    if !state.accepting {
        return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
    }
    let focused = state.resources.resolve_terminal_target(selector)?;
    let paths = state
        .resources
        .open_terminal_paths_for_tab(focused.tab_id)?;
    let layout = state.resources.open_layout_for_tab(focused.tab_id)?;
    let fallback_terminal_ids = state.resources.fallback_terminal_ids(focused.terminal_id)?;
    let focused_runtime = state
        .runtimes
        .get(&focused.terminal_id)
        .ok_or_else(|| DaemonError::new("not_found", "terminal runtime not found"))?;
    if let TerminalLifecycle::Exited { exit_code } = focused_runtime
        .handle
        .subscribe_lifecycle()
        .borrow()
        .clone()
    {
        return Err(DaemonError::new(
            "terminal_exited",
            format!("terminal already exited with status {exit_code:?}"),
        ));
    }
    let guard = focused_runtime.lease.acquire(client).ok_or_else(|| {
        DaemonError::new(
            "already_attached",
            "another interactive client holds this terminal's attachment lease",
        )
    })?;
    let focused_target = LeasedTarget {
        selected: selected_target(focused, &focused_runtime.handle),
        terminal: Arc::clone(&focused_runtime.handle),
        _lease: guard,
    };
    let mut panes = Vec::with_capacity(paths.len());
    for path in paths {
        let runtime = state
            .runtimes
            .get(&path.terminal_id)
            .ok_or_else(|| DaemonError::new("not_found", "terminal runtime not found"))?;
        if path.terminal_id != focused.terminal_id
            && matches!(
                runtime.handle.subscribe_lifecycle().borrow().clone(),
                TerminalLifecycle::Exited { .. }
            )
        {
            continue;
        }
        panes.push(ObservedTarget {
            selected: selected_target(path, &runtime.handle),
            terminal: Arc::clone(&runtime.handle),
        });
    }
    let layout = retain_observed_layout(layout, &panes)?;
    Ok(Attachment::new(
        panes,
        focused_target,
        layout,
        fallback_terminal_ids,
        state.resources.revision(),
        state.resource_changes.subscribe(),
    ))
}

async fn focus_leased_attachment(
    shared: &Shared,
    attachment: &mut Attachment,
    mut focused: LeasedTarget,
) -> Result<(), DaemonError> {
    let state = shared.lock().await;
    if !state.accepting {
        return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
    }
    if let TerminalLifecycle::Exited { exit_code } =
        focused.terminal.subscribe_lifecycle().borrow().clone()
    {
        return Err(DaemonError::new(
            "terminal_exited",
            format!("terminal already exited with status {exit_code:?}"),
        ));
    }
    let path = state
        .resources
        .resolve_terminal_target(Some(TargetSelector::Terminal(focused.selected.terminal_id)))?;
    let paths = state.resources.open_terminal_paths_for_tab(path.tab_id)?;
    let layout = state.resources.open_layout_for_tab(path.tab_id)?;
    let fallback_terminal_ids = state.resources.fallback_terminal_ids(path.terminal_id)?;
    focused.selected = selected_target(path, &focused.terminal);
    let panes = observed_targets(&state, paths, focused.selected.terminal_id)?;
    let layout = retain_observed_layout(layout, &panes)?;
    let selected = focused.selected.clone();
    attachment.focused = focused;
    attachment.reconcile(
        panes,
        selected,
        layout,
        fallback_terminal_ids,
        state.resources.revision(),
    );
    Ok(())
}

enum TargetSelection {
    Focused {
        selected: SelectedTarget,
        panes: Vec<ObservedTarget>,
        layout: SplitTree,
        fallback_terminal_ids: Vec<TerminalId>,
        resource_revision: u64,
    },
    Different,
}

async fn observe_selection(
    shared: &Shared,
    selector: &TargetSelector,
    focused_terminal_id: TerminalId,
) -> Result<TargetSelection, DaemonError> {
    let state = shared.lock().await;
    if !state.accepting {
        return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
    }
    let path = state
        .resources
        .resolve_terminal_target(Some(selector.clone()))?;
    let runtime = state
        .runtimes
        .get(&path.terminal_id)
        .ok_or_else(|| DaemonError::new("not_found", "terminal runtime not found"))?;
    if let TerminalLifecycle::Exited { exit_code } =
        runtime.handle.subscribe_lifecycle().borrow().clone()
    {
        return Err(DaemonError::new(
            "terminal_exited",
            format!("terminal already exited with status {exit_code:?}"),
        ));
    }
    if path.terminal_id != focused_terminal_id {
        return Ok(TargetSelection::Different);
    }
    let paths = state.resources.open_terminal_paths_for_tab(path.tab_id)?;
    let layout = state.resources.open_layout_for_tab(path.tab_id)?;
    let fallback_terminal_ids = state.resources.fallback_terminal_ids(path.terminal_id)?;
    let panes = observed_targets(&state, paths, focused_terminal_id)?;
    let layout = retain_observed_layout(layout, &panes)?;
    Ok(TargetSelection::Focused {
        selected: selected_target(path, &runtime.handle),
        panes,
        layout,
        fallback_terminal_ids,
        resource_revision: state.resources.revision(),
    })
}

fn observed_targets(
    state: &SharedState,
    paths: Vec<crate::resources::ResolvedTerminalPath>,
    focused: TerminalId,
) -> Result<Vec<ObservedTarget>, DaemonError> {
    let mut panes = Vec::with_capacity(paths.len());
    for path in paths {
        let runtime = state
            .runtimes
            .get(&path.terminal_id)
            .ok_or_else(|| DaemonError::new("not_found", "terminal runtime not found"))?;
        if path.terminal_id != focused
            && matches!(
                runtime.handle.subscribe_lifecycle().borrow().clone(),
                TerminalLifecycle::Exited { .. }
            )
        {
            continue;
        }
        panes.push(ObservedTarget {
            selected: selected_target(path, &runtime.handle),
            terminal: Arc::clone(&runtime.handle),
        });
    }
    Ok(panes)
}

fn retain_observed_layout(
    layout: SplitTree,
    panes: &[ObservedTarget],
) -> Result<SplitTree, DaemonError> {
    let pane_ids = panes
        .iter()
        .map(|pane| pane.selected.pane_id)
        .collect::<HashSet<_>>();
    let layout = layout
        .retained(|pane_id| pane_ids.contains(&pane_id))
        .ok_or_else(|| DaemonError::new("resource_error", "selected layout has no pane"))?;
    if layout.leaf_ids()
        != panes
            .iter()
            .map(|pane| pane.selected.pane_id)
            .collect::<Vec<_>>()
    {
        return Err(DaemonError::new(
            "resource_error",
            "selected layout order does not match observed panes",
        ));
    }
    Ok(layout)
}

struct ReconciledResources {
    snapshot: crate::resources::ResourceSnapshot,
    view_changed: bool,
}

async fn reconcile_attachment(
    shared: &Shared,
    attachment: &mut Attachment,
) -> Result<Option<ReconciledResources>, DaemonError> {
    let state = shared.lock().await;
    let revision = state.resources.revision();
    if revision <= attachment.streamed_resource_revision {
        return Ok(None);
    }
    let snapshot = state.resources.snapshot();
    let focused_id = attachment.focused.selected.terminal_id;
    let focused_path = match state
        .resources
        .resolve_terminal_target(Some(TargetSelector::Terminal(focused_id)))
    {
        Ok(path) => path,
        Err(ResourceError::Closing(_) | ResourceError::NotFound(_)) => {
            attachment.observe_revision(revision);
            attachment.observe_streamed_revision(revision);
            return Ok(Some(ReconciledResources {
                snapshot,
                view_changed: false,
            }));
        }
        Err(error) => return Err(error.into()),
    };
    if !matches!(
        attachment
            .focused
            .terminal
            .subscribe_lifecycle()
            .borrow()
            .clone(),
        TerminalLifecycle::Running
    ) {
        attachment.observe_revision(revision);
        attachment.observe_streamed_revision(revision);
        return Ok(Some(ReconciledResources {
            snapshot,
            view_changed: false,
        }));
    }
    let paths = state
        .resources
        .open_terminal_paths_for_tab(focused_path.tab_id)?;
    let layout = state.resources.open_layout_for_tab(focused_path.tab_id)?;
    let fallback_terminal_ids = state.resources.fallback_terminal_ids(focused_id)?;
    let panes = observed_targets(&state, paths, focused_id)?;
    let layout = retain_observed_layout(layout, &panes)?;
    let focused = selected_target(focused_path, &attachment.focused.terminal);
    let view_changed =
        attachment.reconcile(panes, focused, layout, fallback_terminal_ids, revision);
    attachment.observe_streamed_revision(revision);
    Ok(Some(ReconciledResources {
        snapshot,
        view_changed,
    }))
}

async fn switch_candidate(
    shared: &Shared,
    selector: TargetSelector,
    client: ClientId,
) -> Result<Attachment, DaemonError> {
    let attachment = lease_view(shared, Some(selector), client).await?;
    attachment.all_running()?;
    Ok(attachment)
}

async fn exit_replacement_ids(
    shared: &Shared,
    attachment: &Attachment,
    exiting: TerminalId,
) -> Vec<TerminalId> {
    let mut replacements = attachment.fallback_terminal_ids.clone();
    let mut seen = replacements.iter().copied().collect::<HashSet<_>>();
    seen.insert(exiting);
    let state = shared.lock().await;
    if let Ok(current) = state
        .resources
        .open_terminal_ids_for_session(attachment.focused.selected.session_id)
    {
        replacements.extend(
            current
                .into_iter()
                .filter(|terminal_id| seen.insert(*terminal_id)),
        );
    }
    replacements
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

enum CreationMode {
    Detached,
    Attached(ClientId),
}

enum CreatedTerminal {
    Detached(SelectedTarget),
    Attached(LeasedTarget),
}

async fn create_tab(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    request: CreateTabRequest,
    mode: CreationMode,
) -> Result<CreatedTerminal, DaemonError> {
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
    let cwd = resolve_spawn_cwd(&root, cwd).await?;
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
                CreationMode::Detached => CreatedTerminal::Detached(selected),
                CreationMode::Attached(client) => {
                    let runtime = state
                        .runtimes
                        .get(&terminal.id())
                        .expect("new runtime inserted");
                    let guard = runtime
                        .lease
                        .acquire(client)
                        .expect("new terminal has an independent lease");
                    let target = LeasedTarget {
                        selected,
                        terminal: Arc::clone(&terminal),
                        _lease: guard,
                    };
                    CreatedTerminal::Attached(target)
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

struct CreatePaneRequest {
    target: PaneCreationTarget,
    cwd: Option<PathBuf>,
    program: Option<PathBuf>,
    argv: Vec<String>,
    size: TerminalSize,
}

#[derive(Clone, Copy)]
enum PaneCreationTarget {
    Append(TabId),
    Split {
        anchor: PaneId,
        direction: SplitDirection,
    },
}

async fn create_pane(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    request: CreatePaneRequest,
    mode: CreationMode,
) -> Result<CreatedTerminal, DaemonError> {
    let CreatePaneRequest {
        target,
        cwd,
        program,
        argv,
        size,
    } = request;
    let (workspace_id, session_id, root, inherited_cwd) = {
        let state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let tab_id = match target {
            PaneCreationTarget::Append(tab_id) => tab_id,
            PaneCreationTarget::Split { anchor, .. } => {
                state
                    .resources
                    .resolve_terminal_target(Some(TargetSelector::Pane(anchor)))?
                    .tab_id
            }
        };
        let workspace_id = state.resources.workspace_id_for_tab(tab_id)?;
        let session_id = state.resources.session_id_for_workspace(workspace_id)?;
        let root = state.resources.workspace_root(workspace_id)?.to_path_buf();
        let inherited_cwd = match target {
            PaneCreationTarget::Append(_) => None,
            PaneCreationTarget::Split { anchor, .. } => {
                let terminal_id = state
                    .resources
                    .resolve_terminal_target(Some(TargetSelector::Pane(anchor)))?
                    .terminal_id;
                let runtime = state
                    .runtimes
                    .get(&terminal_id)
                    .ok_or_else(|| DaemonError::new("not_found", "terminal runtime not found"))?;
                Some((
                    runtime.handle.child_pid(),
                    runtime.handle.spawn_cwd().to_path_buf(),
                ))
            }
        };
        (workspace_id, session_id, root, inherited_cwd)
    };
    let cwd = match (cwd, inherited_cwd) {
        (Some(cwd), _) => resolve_spawn_cwd(&root, Some(cwd)).await?,
        (None, Some((pid, fallback))) => {
            resolve_spawn_cwd(&root, process_cwd(pid).await.or(Some(fallback))).await?
        }
        (None, None) => resolve_spawn_cwd(&root, None).await?,
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
        // Re-resolve the parent and validate a clone after the filesystem await,
        // so a close or shutdown cannot race creation into a stale tab.
        let tab_id = match target {
            PaneCreationTarget::Append(tab_id) => tab_id,
            PaneCreationTarget::Split { anchor, .. } => {
                state
                    .resources
                    .resolve_terminal_target(Some(TargetSelector::Pane(anchor)))?
                    .tab_id
            }
        };
        state.resources.workspace_id_for_tab(tab_id)?;
        let pane_id = PaneId::new();
        let mut validated = state.resources.clone();
        match target {
            PaneCreationTarget::Append(_) => {
                validated.add_pane(tab_id, pane_id, TerminalId::new())?;
            }
            PaneCreationTarget::Split { anchor, direction } => {
                validated.split_pane(anchor, direction, pane_id, TerminalId::new())?;
            }
        }

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
        let resolved = crate::resources::ResolvedTerminalPath {
            session_id,
            workspace_id,
            tab_id,
            pane_id,
            terminal_id: terminal.id(),
        };
        let selected = selected_target(resolved, &terminal);
        let insertion = match target {
            PaneCreationTarget::Append(_) => {
                state.register_pane(tab_id, pane_id, Arc::clone(&terminal))
            }
            PaneCreationTarget::Split { anchor, direction } => {
                state.register_split_pane(anchor, direction, pane_id, Arc::clone(&terminal))
            }
        };
        let creation = match insertion {
            Ok(()) => Ok(match mode {
                CreationMode::Detached => CreatedTerminal::Detached(selected),
                CreationMode::Attached(client) => {
                    let runtime = state
                        .runtimes
                        .get(&terminal.id())
                        .expect("new runtime inserted");
                    let guard = runtime
                        .lease
                        .acquire(client)
                        .expect("new terminal has an independent lease");
                    CreatedTerminal::Attached(LeasedTarget {
                        selected,
                        terminal: Arc::clone(&terminal),
                        _lease: guard,
                    })
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

async fn resolve_spawn_cwd(root: &Path, cwd: Option<PathBuf>) -> Result<PathBuf, DaemonError> {
    let candidate = match cwd {
        Some(cwd) if cwd.is_absolute() => cwd,
        Some(cwd) => root.join(cwd),
        None => root.to_path_buf(),
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
    Ok(canonical)
}

async fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let output = timeout(
            Duration::from_millis(500),
            tokio::process::Command::new("/usr/sbin/lsof")
                .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout)
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix('n'))
            .map(PathBuf::from)
    }
    #[cfg(target_os = "linux")]
    {
        tokio::fs::read_link(format!("/proc/{pid}/cwd")).await.ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
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
            state.cancel_target_close(scope);
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
    if is_uncorrelated_transition_error(request_id, &result) {
        // Interactive input and resize messages are intentionally uncorrelated.
        // Their terminal can exit after the client sends them but before this
        // loop reconciles its attachment. Drop that transition input rather
        // than turning a normal focused-exit fallback into a fatal client error.
        return Ok(());
    }
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

fn is_uncorrelated_transition_error(
    request_id: Option<uuid::Uuid>,
    result: &Result<(), CommandError>,
) -> bool {
    request_id.is_none() && matches!(result, Err(CommandError::Stopped))
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
    fn stopped_transition_input_is_silent_only_when_uncorrelated() {
        assert!(is_uncorrelated_transition_error(
            None,
            &Err(CommandError::Stopped)
        ));
        assert!(!is_uncorrelated_transition_error(
            Some(uuid::Uuid::new_v4()),
            &Err(CommandError::Stopped)
        ));
        assert!(!is_uncorrelated_transition_error(
            None,
            &Err(CommandError::Busy)
        ));
    }

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
                resource_changes: watch::channel(1).0,
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
    fn committed_resource_mutations_publish_once_and_no_ops_stay_silent() {
        let (mut state, path) = inconsistent_state();
        let mut changes = state.resource_changes.subscribe();

        state
            .rename_target(
                RenameSelector::Session(crate::resources::SessionSelector::Id(path.session_id)),
                "renamed".into(),
            )
            .unwrap();
        assert!(changes.has_changed().unwrap());
        assert_eq!(*changes.borrow_and_update(), state.resources.revision());

        state
            .rename_target(
                RenameSelector::Session(crate::resources::SessionSelector::Id(path.session_id)),
                "renamed".into(),
            )
            .unwrap();
        assert!(!changes.has_changed().unwrap());
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

    #[tokio::test]
    async fn registered_pane_has_independent_runtime_and_finalization() {
        let (mut state, path) = inconsistent_state();
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "sleep 10".into()],
                cwd: "/".into(),
                env: HashMap::new(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .unwrap(),
        );
        let pane_id = PaneId::new();
        let terminal_id = terminal.id();

        state
            .register_pane(path.tab_id, pane_id, Arc::clone(&terminal))
            .unwrap();

        let added = state
            .resources
            .resolve_terminal_target(Some(TargetSelector::Pane(pane_id)))
            .unwrap();
        assert_eq!(added.terminal_id, terminal_id);
        assert!(state.runtimes.contains_key(&terminal_id));
        assert!(!state.finalize_terminal(terminal_id).unwrap());
        assert!(!state.runtimes.contains_key(&terminal_id));
        assert!(
            state
                .resources
                .resolve_terminal_target(Some(TargetSelector::Pane(pane_id)))
                .is_err()
        );

        terminal.close().await.unwrap();
    }

    #[tokio::test]
    async fn focused_selection_observes_exact_target_and_complete_tab() {
        let spawn = || {
            Arc::new(
                spawn_terminal(SpawnSpec {
                    program: "/bin/sh".into(),
                    argv: vec!["-c".into(), "sleep 10".into()],
                    cwd: "/".into(),
                    env: HashMap::new(),
                    size: TerminalSize {
                        columns: 80,
                        rows: 24,
                    },
                })
                .unwrap(),
            )
        };
        let focused = spawn();
        let sibling = spawn();
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
        let path = initial_path(&resolved, "test".into(), focused.id());
        let sibling_pane_id = PaneId::new();
        let (resource_changes, _) = watch::channel(0);
        let mut state = SharedState {
            resources: ResourceTree::default(),
            runtimes: HashMap::new(),
            expected_finalizations: HashSet::new(),
            resource_changes,
            accepting: true,
        };
        state
            .register_session(path.clone(), Arc::clone(&focused))
            .unwrap();
        let shared = Arc::new(Mutex::new(state));
        let stale_attachment = lease_view(
            &shared,
            Some(TargetSelector::Pane(path.pane_id)),
            ClientId::new(),
        )
        .await
        .unwrap();
        shared
            .lock()
            .await
            .register_pane(path.tab_id, sibling_pane_id, Arc::clone(&sibling))
            .unwrap();
        assert_eq!(
            exit_replacement_ids(&shared, &stale_attachment, focused.id()).await,
            vec![sibling.id()]
        );
        drop(stale_attachment);

        let TargetSelection::Focused {
            selected,
            panes,
            layout: _,
            fallback_terminal_ids,
            resource_revision,
        } = observe_selection(&shared, &TargetSelector::Pane(path.pane_id), focused.id())
            .await
            .unwrap()
        else {
            panic!("the focused terminal should be observed in place");
        };

        assert_eq!(selected.pane_id, path.pane_id);
        assert_eq!(selected.terminal_id, focused.id());
        assert_eq!(panes.len(), 2);
        assert_eq!(fallback_terminal_ids, vec![sibling.id()]);
        assert!(
            panes
                .iter()
                .any(|pane| pane.selected.pane_id == sibling_pane_id)
        );
        assert_eq!(resource_revision, shared.lock().await.resources.revision());

        sibling.close().await.unwrap();
        let TargetSelection::Focused { panes, layout, .. } =
            observe_selection(&shared, &TargetSelector::Pane(path.pane_id), focused.id())
                .await
                .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(panes.len(), 1);
        assert_eq!(layout, SplitTree::leaf(path.pane_id));
        focused.close().await.unwrap();
    }

    #[test]
    fn pane_move_errors_have_stable_codes() {
        assert_eq!(
            DaemonError::from(ResourceError::DifferentWorkspace).code,
            "different_workspace"
        );
        assert_eq!(
            DaemonError::from(ResourceError::NotFound("pane")).code,
            "not_found"
        );
        assert_eq!(
            DaemonError::from(ResourceError::Closing("tab")).code,
            "target_closing"
        );
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
