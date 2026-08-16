//! The single per-user Fut daemon and its local socket lifecycle.

mod git;
mod lease;
pub mod path;

pub mod autostart;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use regex::Regex;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Mutex, Notify, broadcast, mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::{Duration, timeout},
};
use tokio_util::codec::Framed;
use uuid::Uuid;

use crate::{
    agent_detection::{CodexStabilizer, detect_codex, is_codex_process},
    client::config as global_config,
    domain::{
        AgentActivity, AgentDetection, AgentReport, AgentState, ClientId, CopyModeAction,
        CopyModeError, DeltaRow, MAX_AGENT_PROMPT_BYTES, MAX_TERMINAL_OUTPUT_PATTERN_BYTES,
        MAX_TERMINAL_OUTPUT_ROWS, PaneId, ScreenDelta, ScreenSnapshot, SessionId, SplitId, TabId,
        TerminalId, TerminalOutput, TerminalOutputMatcher, TerminalOutputSource, TerminalSize,
        WorkspaceId,
    },
    project::{ProjectError, ProjectResolver, ResolvedLocation},
    protocol::{
        AcknowledgedCommand, ClientMessage, ClientMode, ContextScope, ContextualCommand, Envelope,
        OpenDisposition, PROTOCOL_VERSION, RenameSelector, SelectedTarget, SelectedView,
        SelectionExpectation, ServerMessage, TerminalContext, TerminalInputOperation, codec,
        decode_payload, encode_payload,
    },
    resources::{
        CheckoutDestination, InitialPath, Mutation, PresentationTokenTarget, ResourceError,
        ResourceTree, SessionSelector, TabPath, TargetSelector, TokenPublication, WorkspacePath,
    },
    splits::{SplitDirection, SplitRatio, SplitTree},
    terminal::{
        CommandError, CopyModeOutcome, MouseInputOutcome, OutputCapture, OutputCaptureError,
        SpawnSpec, TerminalEvent, TerminalHandle, TerminalLifecycle, spawn_terminal,
    },
};

use lease::AttachmentLease;
use path::prepare_runtime_dir;

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
        let mut env = std::env::vars().collect::<HashMap<_, _>>();
        env.insert("FUT_SOCKET".into(), socket_path.display().to_string());
        Self {
            socket_path,
            spawn: SpawnSpec {
                id: TerminalId::new(),
                program,
                argv: Vec::new(),
                cwd,
                env,
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            },
        }
    }
}

const CONNECTION_GRACE_PERIOD: Duration = Duration::from_secs(1);
const HOOK_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(1);

struct RuntimeEntry {
    handle: Arc<TerminalHandle>,
    lease: AttachmentLease,
}

struct SharedState {
    resources: ResourceTree,
    runtimes: HashMap<TerminalId, RuntimeEntry>,
    expected_finalizations: HashSet<TerminalId>,
    exited_terminals: VecDeque<(TerminalId, Option<i32>)>,
    resource_changes: watch::Sender<u64>,
    agent_events: broadcast::Sender<AgentLifecycleUpdate>,
    child_env: HashMap<String, String>,
    extensions: Vec<crate::extensions::Extension>,
    hook_queue: crate::extensions::HookQueue,
    accepting: bool,
}

const AGENT_EVENT_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
struct AgentLifecycleUpdate {
    terminal_id: TerminalId,
    activity: AgentActivity,
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
            ResourceError::InvalidAgentReport(_) => "invalid_agent_report",
            ResourceError::TooManyMaterializedTokens => "token_limit",
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
    fn validate_terminal_context(
        &self,
        context: TerminalContext,
    ) -> Result<crate::resources::ResolvedTerminalPath, DaemonError> {
        let path = self
            .resources
            .resolve_terminal_target(Some(TargetSelector::Terminal(context.terminal_id)))?;
        if path.session_id != context.session_id
            || path.workspace_id != context.workspace_id
            || path.tab_id != context.tab_id
            || path.pane_id != context.pane_id
        {
            return Err(DaemonError::new(
                "context_changed",
                "calling terminal ancestry changed before the operation could run",
            ));
        }
        let runtime = self.runtimes.get(&context.terminal_id).ok_or_else(|| {
            DaemonError::new("stale_context", "calling terminal runtime no longer exists")
        })?;
        if !matches!(
            *runtime.handle.subscribe_lifecycle().borrow(),
            TerminalLifecycle::Running
        ) {
            return Err(DaemonError::new(
                "terminal_exited",
                "calling terminal is no longer running",
            ));
        }
        Ok(path)
    }

    fn publish_resource_change(&self, revision: u64) {
        if revision > *self.resource_changes.borrow() {
            self.resource_changes.send_replace(revision);
        }
    }

    fn publish_mutation(&self, mutation: &Mutation) {
        self.publish_resource_change(mutation.revision);
        self.hook_queue.enqueue(mutation);
    }

    fn publish_token(
        &mut self,
        extension_id: &str,
        token_name: &str,
        value: String,
        target: PresentationTokenTarget,
    ) -> Result<TokenPublication, DaemonError> {
        if !self.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let extension = self
            .extensions
            .iter()
            .find(|extension| extension.id() == extension_id)
            .ok_or_else(|| {
                DaemonError::new(
                    "unknown_extension",
                    format!("extension {extension_id:?} is not configured"),
                )
            })?;
        let declaration = extension.presentation_token(token_name).ok_or_else(|| {
            DaemonError::new(
                "undeclared_token",
                format!(
                    "extension {extension_id:?} does not declare presentation token {token_name:?}"
                ),
            )
        })?;
        let target_scope = match target {
            PresentationTokenTarget::Session(_) => crate::extensions::PresentationScope::Session,
            PresentationTokenTarget::Workspace(_) => {
                crate::extensions::PresentationScope::Workspace
            }
            PresentationTokenTarget::Tab(_) => crate::extensions::PresentationScope::Tab,
            PresentationTokenTarget::Pane(_) => crate::extensions::PresentationScope::Pane,
        };
        if declaration.scope() != target_scope {
            return Err(DaemonError::new(
                "invalid_token_scope",
                format!(
                    "presentation token {:?} has {} scope, not {} scope",
                    declaration.qualified_name(),
                    declaration.scope().as_str(),
                    target_scope.as_str()
                ),
            ));
        }
        crate::extensions::validate_presentation_value(&value)
            .map_err(|error| DaemonError::new("invalid_token_value", error.to_string()))?;
        let publication = self.resources.publish_presentation_token(
            target,
            declaration.qualified_name().to_owned(),
            value,
        )?;
        if publication.changed {
            self.publish_resource_change(publication.revision);
        }
        Ok(publication)
    }

    fn report_agent(
        &mut self,
        terminal_id: TerminalId,
        report: AgentReport,
        metadata: crate::domain::AgentReportMetadata,
    ) -> Result<(), DaemonError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let revision =
            self.resources
                .report_agent_with_metadata(terminal_id, report, metadata, now_ms)?;
        let activity = self.resources.agent_activity(terminal_id)?.clone();
        self.publish_resource_change(revision);
        let _ = self.agent_events.send(AgentLifecycleUpdate {
            terminal_id,
            activity,
        });
        Ok(())
    }

    fn acknowledge_agent(
        &mut self,
        terminal_id: TerminalId,
        event_revision: u64,
    ) -> Result<(), DaemonError> {
        if let Some(revision) = self
            .resources
            .acknowledge_agent(terminal_id, event_revision)?
        {
            self.publish_resource_change(revision);
        }
        Ok(())
    }

    fn register_session(
        &mut self,
        path: InitialPath,
        terminal: Arc<TerminalHandle>,
    ) -> Result<(), DaemonError> {
        let mutation = self.resources.create_session(path)?;
        self.expected_finalizations.remove(&terminal.id());
        self.exited_terminals.retain(|(id, _)| *id != terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        self.publish_mutation(&mutation);
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
        self.exited_terminals.retain(|(id, _)| *id != terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        self.publish_mutation(&mutation);
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
        self.exited_terminals.retain(|(id, _)| *id != terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        self.publish_mutation(&mutation);
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
        self.exited_terminals.retain(|(id, _)| *id != terminal.id());
        self.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: terminal,
                lease: AttachmentLease::default(),
            },
        );
        self.publish_mutation(&mutation);
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
        self.publish_mutation(&mutation);
        Ok(())
    }

    fn resize_split(
        &mut self,
        tab_id: TabId,
        split_id: SplitId,
        ratio: SplitRatio,
    ) -> Result<(), DaemonError> {
        let mutation = self.resources.resize_split(tab_id, split_id, ratio)?;
        self.publish_mutation(&mutation);
        Ok(())
    }

    fn resize_split_for_attachment(
        &mut self,
        focused_terminal_id: TerminalId,
        tab_id: TabId,
        split_id: SplitId,
        ratio: SplitRatio,
    ) -> Result<(), DaemonError> {
        let focused = self
            .resources
            .resolve_terminal_target(Some(TargetSelector::Terminal(focused_terminal_id)))?;
        if focused.tab_id != tab_id {
            return Err(DaemonError::new(
                "context_changed",
                "focused terminal moved to another tab before split resize",
            ));
        }
        self.resize_split(tab_id, split_id, ratio)
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
        let exit_code = match self
            .runtimes
            .get(&terminal_id)
            .expect("runtime presence checked above")
            .handle
            .subscribe_lifecycle()
            .borrow()
            .clone()
        {
            TerminalLifecycle::Running => None,
            TerminalLifecycle::Exited { exit_code } => exit_code,
        };
        let mutation = self.resources.terminal_exited(terminal_id)?;
        self.runtimes.remove(&terminal_id);
        self.exited_terminals.push_back((terminal_id, exit_code));
        if self.exited_terminals.len() > 256 {
            self.exited_terminals.pop_front();
        }
        self.publish_mutation(&mutation);
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
        self.publish_mutation(&mutation);
        Ok((scope, handles))
    }

    fn cancel_target_close(&mut self, scope: CloseScope) {
        if let Ok(mutation) = scope.cancel(&mut self.resources) {
            self.publish_mutation(&mutation);
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
        self.publish_mutation(&mutation);
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

    fn rename_target(
        &mut self,
        selector: RenameSelector,
        name: String,
    ) -> Result<u64, DaemonError> {
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
        self.publish_mutation(&mutation);
        Ok(mutation.revision)
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
    lease: lease::LeaseGuard,
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
    owner: ClientId,
    size: TerminalSize,
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
    viewport_offsets: HashMap<TerminalId, usize>,
    snapshot_revisions: HashMap<TerminalId, u64>,
    copy_mode_terminal: Option<Arc<TerminalHandle>>,
    next_watcher_generation: u64,
}

#[derive(Clone, Copy)]
struct FocusedViewportState {
    terminal_id: TerminalId,
    offset: Option<usize>,
    snapshot_revision: Option<u64>,
}

impl Attachment {
    fn new(
        owner: ClientId,
        panes: Vec<ObservedTarget>,
        focused: LeasedTarget,
        layout: SplitTree,
        fallback_terminal_ids: Vec<TerminalId>,
        resource_revision: u64,
        resource_changes: watch::Receiver<u64>,
    ) -> Self {
        let (update_sender, updates) = mpsc::channel(ATTACHMENT_UPDATE_CAPACITY);
        let size = focused
            .lease
            .client_size()
            .expect("a new attachment retains its client size");
        let mut attachment = Self {
            owner,
            size,
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
            viewport_offsets: HashMap::new(),
            snapshot_revisions: HashMap::new(),
            copy_mode_terminal: None,
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

    async fn resize_focused(&mut self, size: TerminalSize) -> Result<TerminalSize, CommandError> {
        self.size = size;
        let geometry = self
            .focused
            .lease
            .resize(size)
            .ok_or(CommandError::Stopped)?;
        self.viewport_offsets
            .remove(&self.focused.selected.terminal_id);
        self.focused
            .terminal
            .resize_for_attachment(geometry)
            .await?;
        Ok(geometry.size)
    }

    fn size(&self) -> TerminalSize {
        self.size
    }

    fn shares_focused_terminal(&self) -> bool {
        self.focused.lease.has_peers()
    }

    fn focused_viewport_state(&self) -> FocusedViewportState {
        let terminal_id = self.focused.selected.terminal_id;
        FocusedViewportState {
            terminal_id,
            offset: self.viewport_offsets.get(&terminal_id).copied(),
            snapshot_revision: self.snapshot_revisions.get(&terminal_id).copied(),
        }
    }

    fn finish_focused_input(&mut self, before: FocusedViewportState, succeeded: bool) {
        if succeeded {
            self.viewport_offsets.remove(&before.terminal_id);
            return;
        }
        if let Some(offset) = before.offset {
            self.viewport_offsets.insert(before.terminal_id, offset);
        } else {
            self.viewport_offsets.remove(&before.terminal_id);
        }
        if let Some(revision) = before.snapshot_revision {
            self.snapshot_revisions.insert(before.terminal_id, revision);
        } else {
            self.snapshot_revisions.remove(&before.terminal_id);
        }
    }

    fn terminal(&self, terminal_id: TerminalId) -> Option<Arc<TerminalHandle>> {
        self.panes
            .iter()
            .find(|pane| pane.selected.terminal_id == terminal_id)
            .map(|pane| Arc::clone(&pane.terminal))
    }

    fn copy_mode_active(&self) -> bool {
        self.copy_mode_terminal.is_some()
    }

    fn owns_copy_mode(&self, terminal_id: TerminalId) -> bool {
        self.copy_mode_terminal
            .as_ref()
            .is_some_and(|terminal| terminal.id() == terminal_id)
    }

    fn record_copy_mode_begin(&mut self) -> Result<(), CommandError> {
        if self.copy_mode_active() {
            return Err(CopyModeError::AlreadyActive.into());
        }
        self.copy_mode_terminal = Some(Arc::clone(&self.focused.terminal));
        Ok(())
    }

    fn forget_copy_mode(&mut self, terminal_id: TerminalId) {
        if self.owns_copy_mode(terminal_id) {
            self.copy_mode_terminal = None;
        }
        self.viewport_offsets.remove(&terminal_id);
    }

    async fn copy_mode(&mut self, action: CopyModeAction) -> Result<CopyModeOutcome, CommandError> {
        let terminal_id = self.focused.selected.terminal_id;
        let offset = self.viewport_offsets.get(&terminal_id).copied();
        let beginning = action.begins();
        if beginning {
            self.record_copy_mode_begin()?;
        }
        let outcome = match self
            .focused
            .terminal
            .copy_mode(self.owner, action, offset)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                if beginning
                    || matches!(
                        &error,
                        CommandError::CopyMode(
                            CopyModeError::CursorLost | CopyModeError::NotActive
                        )
                    )
                {
                    self.forget_copy_mode(terminal_id);
                }
                return Err(error);
            }
        };
        match outcome {
            CopyModeOutcome::Active(viewport) => {
                self.set_viewport_offset(terminal_id, viewport.offset);
                let Some((screen, _first)) = self.accept_snapshot(terminal_id, viewport.screen)
                else {
                    self.clear_active_copy_mode().await?;
                    return Err(CommandError::Emulator(
                        "stale copy-mode snapshot revision".into(),
                    ));
                };
                Ok(CopyModeOutcome::Active(crate::terminal::ViewportSnapshot {
                    offset: self.viewport_offsets.get(&terminal_id).copied(),
                    screen,
                }))
            }
            CopyModeOutcome::Prepared { copy_id, text } => {
                Ok(CopyModeOutcome::Prepared { copy_id, text })
            }
            CopyModeOutcome::Finalized { screen } => {
                self.forget_copy_mode(terminal_id);
                self.observe_snapshot_revision(terminal_id, screen.revision);
                Ok(CopyModeOutcome::Finalized { screen })
            }
            CopyModeOutcome::Cancelled { screen } => {
                self.forget_copy_mode(terminal_id);
                self.observe_snapshot_revision(terminal_id, screen.revision);
                Ok(CopyModeOutcome::Cancelled { screen })
            }
        }
    }

    async fn mouse_input(
        &mut self,
        terminal_id: TerminalId,
        event: crate::domain::MouseEvent,
    ) -> Result<Option<crate::domain::ScreenSnapshot>, CommandError> {
        if self.copy_mode_active() {
            return Ok(None);
        }
        let Some(terminal) = self.terminal(terminal_id) else {
            return Ok(None);
        };
        let pty_input_allowed = terminal_id == self.focused.selected.terminal_id;
        if !pty_input_allowed && !matches!(event.kind, crate::domain::MouseEventKind::Wheel { .. })
        {
            return Ok(None);
        }
        let offset = self.viewport_offsets.get(&terminal_id).copied();
        match terminal
            .mouse_input(event, offset, pty_input_allowed)
            .await?
        {
            MouseInputOutcome::Handled => Ok(None),
            MouseInputOutcome::ReturnedToBottom(viewport)
            | MouseInputOutcome::Scrolled(viewport) => {
                Ok(self.accept_mouse_viewport(terminal_id, viewport))
            }
        }
    }

    async fn return_focused_to_bottom(
        &mut self,
    ) -> Result<Option<crate::domain::ScreenSnapshot>, CommandError> {
        if self.copy_mode_active() {
            return Ok(None);
        }
        let terminal_id = self.focused.selected.terminal_id;
        if !self.viewport_offsets.contains_key(&terminal_id) {
            return Ok(None);
        }
        let viewport = self.focused.terminal.viewport_snapshot(None).await?;
        debug_assert!(viewport.offset.is_none());
        self.set_viewport_offset(terminal_id, viewport.offset);
        Ok(self
            .accept_snapshot(terminal_id, viewport.screen)
            .map(|(screen, _first)| screen))
    }

    async fn snapshot_for_update(
        &mut self,
        terminal_id: TerminalId,
        generation: u64,
        canonical: crate::domain::ScreenSnapshot,
    ) -> Result<Option<(crate::domain::ScreenSnapshot, bool)>, CommandError> {
        if !self.accepts(terminal_id, generation) {
            return Ok(None);
        }
        let Some(screen) = self.resolve_screen(terminal_id, Some(canonical)).await? else {
            return Ok(None);
        };
        Ok(self.accept_snapshot(terminal_id, screen))
    }

    /// Publish the terminal's current screen unconditionally, bypassing the
    /// revision gate in [`Self::accept_snapshot`]. Used to answer
    /// [`crate::protocol::ClientMessage::RefreshTerminal`], where the client
    /// is explicitly out of sync and needs a resend even if this attachment
    /// already considers the revision seen.
    async fn refresh_snapshot(
        &mut self,
        terminal_id: TerminalId,
    ) -> Result<Option<crate::domain::ScreenSnapshot>, CommandError> {
        let Some(screen) = self.resolve_screen(terminal_id, None).await? else {
            return Ok(None);
        };
        self.observe_snapshot_revision(terminal_id, screen.revision);
        Ok(Some(screen))
    }

    /// Screen this attachment currently shows for `terminal_id`: the active
    /// copy-mode viewport, a scrolled-back viewport, the caller-supplied
    /// canonical screen if any, or otherwise the terminal's latest published
    /// snapshot.
    async fn resolve_screen(
        &mut self,
        terminal_id: TerminalId,
        canonical: Option<crate::domain::ScreenSnapshot>,
    ) -> Result<Option<crate::domain::ScreenSnapshot>, CommandError> {
        if self.owns_copy_mode(terminal_id) {
            let Some(terminal) = self.terminal(terminal_id) else {
                return Ok(None);
            };
            let offset = self.viewport_offsets.get(&terminal_id).copied();
            return match terminal.copy_mode_snapshot(self.owner, offset).await {
                Ok(viewport) => {
                    self.set_viewport_offset(terminal_id, viewport.offset);
                    Ok(Some(viewport.screen))
                }
                Err(
                    error @ CommandError::CopyMode(
                        CopyModeError::CursorLost | CopyModeError::NotActive,
                    ),
                ) => {
                    self.forget_copy_mode(terminal_id);
                    Err(error)
                }
                Err(error) => Err(error),
            };
        }
        if let Some(offset) = self.viewport_offsets.get(&terminal_id).copied() {
            let Some(terminal) = self.terminal(terminal_id) else {
                return Ok(None);
            };
            let viewport = terminal.viewport_snapshot(Some(offset)).await?;
            self.set_viewport_offset(terminal_id, viewport.offset);
            return Ok(Some(viewport.screen));
        }
        if let Some(canonical) = canonical {
            return Ok(Some(canonical));
        }
        let Some(terminal) = self.terminal(terminal_id) else {
            return Ok(None);
        };
        Ok(Some(terminal.subscribe_snapshots().borrow().clone()))
    }

    fn set_viewport_offset(&mut self, terminal_id: TerminalId, offset: Option<usize>) {
        if let Some(offset) = offset {
            self.viewport_offsets.insert(terminal_id, offset);
        } else {
            self.viewport_offsets.remove(&terminal_id);
        }
    }

    fn accept_mouse_viewport(
        &mut self,
        terminal_id: TerminalId,
        viewport: crate::terminal::ViewportSnapshot,
    ) -> Option<crate::domain::ScreenSnapshot> {
        if self
            .snapshot_revisions
            .get(&terminal_id)
            .is_some_and(|revision| viewport.screen.revision <= *revision)
        {
            return None;
        }
        self.set_viewport_offset(terminal_id, viewport.offset);
        self.snapshot_revisions
            .insert(terminal_id, viewport.screen.revision);
        Some(viewport.screen)
    }

    async fn clear_active_copy_mode(
        &mut self,
    ) -> Result<Option<crate::domain::ScreenSnapshot>, CommandError> {
        let Some(terminal) = self.copy_mode_terminal.clone() else {
            return Ok(None);
        };
        let terminal_id = terminal.id();
        let screen = terminal.clear_client(self.owner).await?;
        self.forget_copy_mode(terminal_id);
        if let Some(screen) = &screen {
            self.observe_snapshot_revision(terminal_id, screen.revision);
        }
        Ok(screen)
    }

    /// Accepts `screen` if newer than the last revision seen for
    /// `terminal_id`, returning it alongside whether this is the first
    /// snapshot accepted for that terminal since it was last watched (a new
    /// attachment or a newly selected pane, per [`Self::reconcile_watchers`]
    /// clearing `snapshot_revisions` on removal) — the daemon writer must
    /// send that one as a full snapshot rather than a delta, since it has no
    /// guarantee the client still holds a matching base grid.
    fn accept_snapshot(
        &mut self,
        terminal_id: TerminalId,
        screen: crate::domain::ScreenSnapshot,
    ) -> Option<(crate::domain::ScreenSnapshot, bool)> {
        let first = !self.snapshot_revisions.contains_key(&terminal_id);
        let revision = self.snapshot_revisions.entry(terminal_id).or_default();
        if screen.revision <= *revision {
            return None;
        }
        *revision = screen.revision;
        Some((screen, first))
    }

    fn observe_snapshot_revision(&mut self, terminal_id: TerminalId, revision: u64) {
        let current = self.snapshot_revisions.entry(terminal_id).or_default();
        *current = (*current).max(revision);
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
        self.forget_copy_mode(terminal_id);
        self.snapshot_revisions.remove(&terminal_id);
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
            self.viewport_offsets.remove(&terminal_id);
            self.snapshot_revisions.remove(&terminal_id);
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

    async fn close(&mut self) -> Result<(), CommandError> {
        self.clear_active_copy_mode().await?;
        Ok(())
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if let Some(terminal) = self.copy_mode_terminal.take() {
            terminal.clear_client_on_drop(self.owner);
        }
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
    let config_location = global_config::resolve_location()?;
    let extensions = global_config::load_extensions_location(&config_location)?;
    let fut_bin = std::env::current_exe().context("resolve current Fut executable")?;
    let (hook_queue, hook_receiver) = crate::extensions::hook_queue();
    let resolved = ProjectResolver::default()
        .resolve(&config.spawn.cwd)
        .await
        .map_err(DaemonError::from)?;
    let socket = bind_socket(&config.socket_path).await?;
    let mut initial_spawn = config.spawn;
    initial_spawn.cwd = resolved.cwd.clone();
    let initial = initial_path(
        &resolved,
        resolved.suggested_session_name.clone(),
        initial_spawn.id,
    );
    let initial_target = crate::resources::ResolvedTerminalPath {
        session_id: initial.session_id,
        workspace_id: initial.workspace_id,
        tab_id: initial.tab_id,
        pane_id: initial.pane_id,
        terminal_id: initial.terminal_id,
    };
    let child_env = initial_spawn.env.clone();
    initial_spawn.env = terminal_env(&child_env, initial_target);
    let terminal = Arc::new(spawn_terminal(initial_spawn)?);
    let (resource_changes, _) = watch::channel(0);
    let (agent_events, _) = broadcast::channel(AGENT_EVENT_CAPACITY);
    let mut state = SharedState {
        resources: ResourceTree::default(),
        runtimes: HashMap::new(),
        expected_finalizations: HashSet::new(),
        exited_terminals: VecDeque::new(),
        resource_changes,
        agent_events,
        child_env,
        extensions: extensions.clone(),
        hook_queue,
        accepting: true,
    };
    if let Err(error) = state.register_session(initial, Arc::clone(&terminal)) {
        let _ = terminal.close().await;
        return Err(error.into());
    }
    let git_resource_changes = state.resource_changes.subscribe();
    let shared = Arc::new(Mutex::new(state));
    let (hook_shutdown_tx, hook_shutdown_rx) = watch::channel(false);
    let mut hooks = tokio::spawn(crate::extensions::run_hooks(
        hook_receiver,
        extensions,
        fut_bin,
        config.socket_path.clone(),
        hook_shutdown_rx,
    ));
    let (exited_tx, mut exited_rx) = mpsc::unbounded_channel();
    watch_terminal(terminal, exited_tx.clone());
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let process_names = watch_process_names(Arc::clone(&shared), shutdown_tx.subscribe());
    let git_metadata = git::watch(
        Arc::clone(&shared),
        git_resource_changes,
        shutdown_tx.subscribe(),
    );
    let mut connections = JoinSet::new();
    let writers = WriterTasks::default();

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
                let writers = Arc::clone(&writers);
                connections.spawn(async move {
                    let connection = ClientConnection::new(stream, &writers);
                    if let Err(error) = handle_connection(connection, shared, exited, shutdown).await {
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
    shutdown_tx.send_replace(true);
    let _ = process_names.await;
    let _ = git_metadata.await;
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
    // Handlers only enqueue outbound frames; wait for the writer tasks to
    // actually deliver what remains before the process exits mid-frame.
    let mut writers = std::mem::take(&mut *writers.lock().expect("writer registry lock poisoned"));
    let _ = timeout(CONNECTION_GRACE_PERIOD, async {
        while writers.join_next().await.is_some() {}
    })
    .await;
    hook_shutdown_tx.send_replace(true);
    if timeout(HOOK_SHUTDOWN_GRACE_PERIOD, &mut hooks)
        .await
        .is_err()
    {
        hooks.abort();
    }
    Ok(())
}

fn watch_process_names(shared: Shared, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        let mut codex = HashMap::<TerminalId, CodexStabilizer>::new();
        let mut worktrees = HashMap::<PathBuf, Option<PathBuf>>::new();
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                _ = interval.tick() => refresh_process_names(&shared, &mut codex, &mut worktrees).await,
            }
        }
    })
}

struct ProcessObservation {
    terminal_id: TerminalId,
    pid: u32,
    name: String,
    command: String,
    cwd: Option<PathBuf>,
    codex_screen: Option<String>,
}

async fn refresh_process_names(
    shared: &Shared,
    codex: &mut HashMap<TerminalId, CodexStabilizer>,
    worktrees: &mut HashMap<PathBuf, Option<PathBuf>>,
) {
    let (terminals, automatic_names) = {
        let state = shared.lock().await;
        (
            state
                .runtimes
                .iter()
                .map(|(terminal_id, entry)| (*terminal_id, Arc::clone(&entry.handle)))
                .collect::<Vec<_>>(),
            state
                .resources
                .automatic_name_terminal_ids()
                .into_iter()
                .collect::<HashSet<_>>(),
        )
    };
    let observations = futures_util::stream::iter(terminals)
        .map(|(terminal_id, terminal)| async move {
            timeout(Duration::from_millis(350), async {
                let pid = terminal.foreground_process_id().await.ok()?;
                let name = process_name(pid).await?;
                let command = process_command(pid).await.unwrap_or_default();
                let cwd = process_cwd(pid).await;
                let codex_screen = if is_codex_process(&name, &command) {
                    terminal
                        .read_output(TerminalOutputSource::Visible, 1, false)
                        .await
                        .ok()
                        .map(|output| output.text)
                } else {
                    None
                };
                Some(ProcessObservation {
                    terminal_id,
                    pid,
                    name,
                    command,
                    cwd,
                    codex_screen,
                })
            })
            .await
            .ok()
            .flatten()
        })
        .buffer_unordered(8)
        .filter_map(std::future::ready)
        .collect::<Vec<_>>()
        .await;

    // Resolve each observed directory's Git work tree once and keep only the
    // directories still in use, so long-lived daemons don't accumulate entries.
    let mut live_cwds = HashSet::new();
    for observation in &observations {
        let Some(cwd) = observation.cwd.clone() else {
            continue;
        };
        if !worktrees.contains_key(&cwd) {
            let worktree = worktree_toplevel(&cwd).await;
            worktrees.insert(cwd.clone(), worktree);
        }
        live_cwds.insert(cwd);
    }
    worktrees.retain(|cwd, _| live_cwds.contains(cwd));

    let mut state = shared.lock().await;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let live = observations
        .iter()
        .map(|observation| observation.terminal_id)
        .collect::<HashSet<_>>();
    codex.retain(|terminal_id, _| live.contains(terminal_id));
    for observation in observations {
        let ProcessObservation {
            terminal_id,
            pid,
            name,
            command,
            cwd,
            codex_screen,
        } = observation;
        if let Some(cwd) = cwd {
            let worktree = worktrees.get(&cwd).cloned().flatten();
            if let Ok(revision) = state.resources.update_pane_cwd(terminal_id, cwd, worktree) {
                state.publish_resource_change(revision);
            }
        }
        if automatic_names.contains(&terminal_id)
            && let Ok(revision) = state
                .resources
                .update_process_name(terminal_id, name.clone())
        {
            state.publish_resource_change(revision);
        }
        let integrated = state
            .resources
            .agent_activity(terminal_id)
            .is_ok_and(|activity| activity.integration.is_some());
        if integrated {
            codex.remove(&terminal_id);
            continue;
        }
        let (detection, detected_state) = if let Some(screen) = codex_screen {
            let detected = detect_codex(&screen);
            if std::env::var_os("FUT_AGENT_DETECTION_LOG").is_some() {
                eprintln!(
                    "codex detection terminal={terminal_id} pid={pid} process={name:?} command={command:?} state={:?} rule={} screen={screen:?}",
                    detected.state, detected.rule
                );
            }
            let stabilizer = codex.entry(terminal_id).or_default();
            let Some(stable_state) = stabilizer.observe(detected.state) else {
                continue;
            };
            (
                Some(AgentDetection {
                    agent: "codex".into(),
                    rule: detected.rule.into(),
                }),
                stable_state,
            )
        } else {
            codex.remove(&terminal_id);
            (None, AgentState::Idle)
        };
        if let Ok(revision) =
            state
                .resources
                .update_agent_detection(terminal_id, detection, detected_state, now_ms)
        {
            state.publish_resource_change(revision);
        }
    }
}

async fn process_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut buffer = [0_u8; 1024];
        // SAFETY: proc_name writes at most buffer.len() bytes to this valid buffer.
        let length = unsafe {
            libc::proc_name(
                i32::try_from(pid).ok()?,
                buffer.as_mut_ptr().cast(),
                u32::try_from(buffer.len()).expect("process-name buffer length fits u32"),
            )
        };
        let length = usize::try_from(length).ok()?;
        (length > 0).then(|| String::from_utf8_lossy(&buffer[..length]).into_owned())
    }
    #[cfg(target_os = "linux")]
    {
        return tokio::fs::read_to_string(format!("/proc/{pid}/comm"))
            .await
            .ok()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let output = timeout(
            Duration::from_millis(250),
            tokio::process::Command::new("/bin/ps")
                .args(["-o", "comm=", "-p", &pid.to_string()])
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        if !output.status.success() {
            return None;
        }
        let command = String::from_utf8_lossy(&output.stdout);
        let command = command.trim();
        (!command.is_empty()).then(|| {
            Path::new(command)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(command)
                .to_owned()
        })
    }
}

async fn process_command(pid: u32) -> Option<String> {
    let output = timeout(
        Duration::from_millis(250),
        tokio::process::Command::new("/bin/ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!command.is_empty()).then_some(command)
}

fn initial_path(resolved: &ResolvedLocation, name: String, terminal_id: TerminalId) -> InitialPath {
    InitialPath {
        session_id: SessionId::new(),
        session_name: name,
        project: resolved.project.clone(),
        workspace_id: WorkspaceId::new(),
        workspace_name: String::new(),
        root: resolved.workspace_root.clone(),
        tab_id: TabId::new(),
        tab_name: String::new(),
        pane_id: PaneId::new(),
        terminal_id,
    }
}

fn terminal_env(
    base: &HashMap<String, String>,
    target: crate::resources::ResolvedTerminalPath,
) -> HashMap<String, String> {
    let mut env = base.clone();
    for (key, value) in [
        ("FUT_SESSION_ID", target.session_id.to_string()),
        ("FUT_WORKSPACE_ID", target.workspace_id.to_string()),
        ("FUT_TAB_ID", target.tab_id.to_string()),
        ("FUT_PANE_ID", target.pane_id.to_string()),
        ("FUT_TERMINAL_ID", target.terminal_id.to_string()),
    ] {
        env.insert(key.into(), value);
    }
    env
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
    let mut path = socket.as_os_str().to_os_string();
    path.push(".lock");
    let path = PathBuf::from(path);
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

/// One client connection: a read half plus a writer task fed by an outbound
/// queue.
///
/// Reads and writes must never share one loop: a scroll flood answers every
/// wheel event with a full snapshot, and once both socket buffers fill, a
/// loop that blocks on writing stops reading and deadlocks against a client
/// doing the same. Enqueueing never blocks, and while the writer lags,
/// pending snapshots coalesce so only the newest screen per terminal is
/// kept. All other messages are delivered verbatim in order.
struct ClientConnection {
    reader: SplitStream<Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>>,
    outbound: Arc<OutboundQueue>,
}

/// Writer tasks outlive their connection handlers so queued frames can still
/// drain after a session ends. The daemon joins this registry on shutdown to
/// deliver those frames before the process exits.
type WriterTasks = Arc<std::sync::Mutex<JoinSet<()>>>;

enum Outbound {
    Frame(Bytes),
    Snapshot {
        terminal_id: TerminalId,
        screen: ScreenSnapshot,
        /// Send as a full snapshot even if the writer could otherwise diff
        /// against the last screen it actually sent for this terminal. Set
        /// for the first snapshot of a newly watched terminal (new
        /// attachment or newly selected pane), where the client may not
        /// hold a matching base grid to splice a delta into.
        force_full: bool,
    },
}

#[derive(Default)]
struct OutboundState {
    items: VecDeque<Outbound>,
    finished: bool,
    failed: bool,
}

#[derive(Default)]
struct OutboundQueue {
    state: std::sync::Mutex<OutboundState>,
    ready: Notify,
}

impl ClientConnection {
    fn new(stream: UnixStream, writers: &WriterTasks) -> Self {
        let (sink, reader) = Framed::new(stream, codec()).split();
        let outbound = Arc::new(OutboundQueue::default());
        writers
            .lock()
            .expect("writer registry lock poisoned")
            .spawn(write_outbound(sink, Arc::clone(&outbound)));
        Self { reader, outbound }
    }

    async fn next(&mut self) -> Option<std::io::Result<bytes::BytesMut>> {
        self.reader.next().await
    }

    fn enqueue(&self, item: Outbound) -> Result<()> {
        self.outbound.push(item)
    }
}

impl Drop for ClientConnection {
    fn drop(&mut self) {
        self.outbound.finish();
    }
}

impl OutboundQueue {
    fn push(&self, item: Outbound) -> Result<()> {
        let mut state = self.state.lock().expect("outbound queue lock poisoned");
        if state.failed {
            bail!("client connection writer stopped");
        }
        match item {
            Outbound::Snapshot {
                terminal_id,
                screen,
                force_full,
            } => {
                // Replace this terminal's pending snapshot in place, but never
                // across an ordered frame: a snapshot enqueued after e.g. a
                // copy-mode exit must not be delivered before it.
                let pending = state
                    .items
                    .iter_mut()
                    .rev()
                    .take_while(|item| matches!(item, Outbound::Snapshot { .. }))
                    .find_map(|item| match item {
                        Outbound::Snapshot {
                            terminal_id: pending_id,
                            screen,
                            force_full,
                        } if *pending_id == terminal_id => Some((screen, force_full)),
                        _ => None,
                    });
                match pending {
                    // A coalesced-away update might have been the one that
                    // needed a full resend (e.g. a resize); once forced,
                    // stay forced.
                    Some((slot, pending_force_full)) => {
                        *slot = screen;
                        *pending_force_full |= force_full;
                    }
                    None => state.items.push_back(Outbound::Snapshot {
                        terminal_id,
                        screen,
                        force_full,
                    }),
                }
            }
            frame => state.items.push_back(frame),
        }
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<Outbound> {
        self.state
            .lock()
            .expect("outbound queue lock poisoned")
            .items
            .pop_front()
    }

    fn finish(&self) {
        self.state
            .lock()
            .expect("outbound queue lock poisoned")
            .finished = true;
        self.ready.notify_one();
    }

    fn is_finished(&self) -> bool {
        self.state
            .lock()
            .expect("outbound queue lock poisoned")
            .finished
    }

    fn fail(&self) {
        let mut state = self.state.lock().expect("outbound queue lock poisoned");
        state.failed = true;
        state.items.clear();
    }
}

/// Bound on flushing frames still queued after a connection's handler ends,
/// so a client that stopped reading cannot pin its writer task forever.
const OUTBOUND_FLUSH_DEADLINE: Duration = Duration::from_secs(10);

/// Rows changed exceeding this fraction of the grid fall back to a full
/// snapshot: past this point the delta's per-row framing overhead outweighs
/// what it saves over just resending everything.
const DELTA_ROW_FALLBACK_THRESHOLD: f64 = 0.6;

/// Choose between a full snapshot and a delta against `last_sent`, the
/// screen actually written to this connection last time — never against
/// whatever the attachment layer last produced, since coalescing can drop
/// frames the writer never sent. Updates `last_sent` to match what this call
/// returns.
fn snapshot_message(
    terminal_id: TerminalId,
    screen: ScreenSnapshot,
    force_full: bool,
    last_sent: &mut HashMap<TerminalId, ScreenSnapshot>,
) -> ServerMessage {
    if !force_full
        && let Some(previous) = last_sent.get(&terminal_id)
        && previous.size == screen.size
        && let Some(rows) = diff_rows(previous, &screen)
    {
        let delta = ScreenDelta {
            revision: screen.revision,
            base_revision: previous.revision,
            size: screen.size,
            rows,
            hyperlinks: screen.hyperlinks.clone(),
            cursor: screen.cursor,
            scroll: screen.scroll,
            mouse_tracking: screen.mouse_tracking,
            graphics: (previous.graphics != screen.graphics).then(|| screen.graphics.clone()),
        };
        last_sent.insert(terminal_id, screen);
        return ServerMessage::SnapshotDelta { terminal_id, delta };
    }
    last_sent.insert(terminal_id, screen.clone());
    ServerMessage::Snapshot {
        terminal_id,
        screen,
    }
}

/// Rows that differ between `previous` and `next`, or `None` if that's more
/// than [`DELTA_ROW_FALLBACK_THRESHOLD`] of all rows. Callers must already
/// know `previous.size == next.size`.
fn diff_rows(previous: &ScreenSnapshot, next: &ScreenSnapshot) -> Option<Vec<DeltaRow>> {
    let columns = usize::from(next.size.columns);
    let rows = usize::from(next.size.rows);
    let mut changed = Vec::new();
    for row in 0..rows {
        let start = row * columns;
        let end = start + columns;
        if previous.cells[start..end] != next.cells[start..end] {
            changed.push(DeltaRow {
                index: row as u16,
                cells: next.cells[start..end].to_vec(),
            });
        }
    }
    if changed.len() as f64 > rows as f64 * DELTA_ROW_FALLBACK_THRESHOLD {
        return None;
    }
    Some(changed)
}

async fn write_outbound(
    mut sink: SplitSink<Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>, Bytes>,
    queue: Arc<OutboundQueue>,
) {
    let mut last_sent: HashMap<TerminalId, ScreenSnapshot> = HashMap::new();
    loop {
        let Some(item) = queue.pop() else {
            if queue.is_finished() {
                return;
            }
            queue.ready.notified().await;
            continue;
        };
        let payload = match item {
            Outbound::Frame(bytes) => Ok(bytes),
            Outbound::Snapshot {
                terminal_id,
                screen,
                force_full,
            } => {
                let message = snapshot_message(terminal_id, screen, force_full, &mut last_sent);
                encode_payload(&Envelope {
                    request_id: None,
                    message,
                })
                .map(Bytes::from)
            }
        };
        let sent = match payload {
            Ok(bytes) if queue.is_finished() => timeout(OUTBOUND_FLUSH_DEADLINE, sink.send(bytes))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result.map_err(Into::into)),
            Ok(bytes) => sink.send(bytes).await.map_err(Into::into),
            Err(error) => Err(error.into()),
        };
        if let Err(error) = sent {
            tracing::debug!(%error, "client connection writer stopped");
            queue.fail();
            return;
        }
    }
}

async fn handle_connection(
    mut connection: ClientConnection,
    shared: Shared,
    exited: mpsc::UnboundedSender<TerminalId>,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let Some(frame) = timeout(Duration::from_secs(2), connection.next())
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
                &mut connection,
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
            &mut connection,
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
                    &mut connection,
                    first.request_id,
                    "invalid_size",
                    &error.to_string(),
                )
                .await?;
                return Ok(());
            }
            let mut attachment = match lease_view(&shared, selector, None, client, size).await {
                Ok(attachment) => attachment,
                Err(error) => {
                    send_error(
                        &mut connection,
                        first.request_id,
                        error.code,
                        &error.message,
                    )
                    .await?;
                    return Ok(());
                }
            };
            if let Err(error) = attachment.all_running() {
                send_error(
                    &mut connection,
                    first.request_id,
                    error.code,
                    &error.message,
                )
                .await?;
                return Ok(());
            }
            if attachment.shares_focused_terminal()
                && let Err(error) = attachment.resize_focused(size).await
            {
                send_command_error(&mut connection, first.request_id, error).await?;
                return Ok(());
            }
            (Some(attachment), Some(size))
        }
        ClientMode::Control => (None, None),
    };
    send(
        &mut connection,
        first.request_id,
        ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").into(),
            selected: leased.as_ref().map(Attachment::selected),
        },
    )
    .await?;

    let Some(spawn_size) = interactive_size else {
        return control_loop(&mut connection, shared, exited, shutdown).await;
    };
    let mut attachment = leased.expect("interactive connection selected a tab view");

    let connection_result: Result<()> = async {
        loop {
            tokio::select! {
            frame = connection.next() => {
                let Some(frame) = frame else { break };
                let envelope: Envelope<ClientMessage> = decode_payload(&frame?)?;
                if let Some(operation) = fire_and_forget_operation(&envelope.message)
                    && reject_fire_and_forget_request_id(
                        &mut connection,
                        envelope.request_id,
                        operation,
                    ).await?
                {
                    continue;
                }
                match envelope.message {
                    ClientMessage::Input { bytes } => {
                        if attachment.copy_mode_active() {
                            send_error(
                                &mut connection,
                                envelope.request_id,
                                "copy_mode_active",
                                "terminal input is disabled while copy mode is active",
                            ).await?;
                        } else {
                            focused_input_response(
                                &mut connection,
                                &mut attachment,
                                envelope.request_id,
                                AcknowledgedCommand::Input,
                                FocusedInput::Bytes(bytes),
                            ).await?;
                        }
                    }
                    ClientMessage::Paste { text } => {
                        if attachment.copy_mode_active() {
                            send_error(
                                &mut connection,
                                envelope.request_id,
                                "copy_mode_active",
                                "terminal paste is disabled while copy mode is active",
                            ).await?;
                        } else {
                            focused_input_response(
                                &mut connection,
                                &mut attachment,
                                envelope.request_id,
                                AcknowledgedCommand::Paste,
                                FocusedInput::Paste(text),
                            ).await?;
                        }
                    }
                    ClientMessage::CopyMode { terminal_id, action } => {
                        if envelope.request_id.is_none() {
                            send_error(
                                &mut connection,
                                None,
                                "request_id_required",
                                "copy-mode commands require a request ID",
                            ).await?;
                            continue;
                        }
                        if terminal_id != attachment.focused.selected.terminal_id {
                            send_error(
                                &mut connection,
                                envelope.request_id,
                                "not_focused",
                                "copy mode requires the focused terminal",
                            ).await?;
                            continue;
                        }
                        match attachment.copy_mode(action).await {
                            Ok(CopyModeOutcome::Active(viewport)) => {
                                send(
                                    &mut connection,
                                    envelope.request_id,
                                    ServerMessage::CopyModeSnapshot {
                                        terminal_id,
                                        screen: viewport.screen,
                                    },
                                ).await?;
                            }
                            Ok(CopyModeOutcome::Prepared { copy_id, text }) => {
                                send(
                                    &mut connection,
                                    envelope.request_id,
                                    ServerMessage::CopyModePrepared {
                                        terminal_id,
                                        copy_id,
                                        text,
                                    },
                                ).await?;
                            }
                            Ok(CopyModeOutcome::Finalized { screen }) => {
                                send(
                                    &mut connection,
                                    envelope.request_id,
                                    ServerMessage::CopyModeFinalized {
                                        terminal_id,
                                        screen,
                                    },
                                ).await?;
                            }
                            Ok(CopyModeOutcome::Cancelled { screen }) => {
                                send(
                                    &mut connection,
                                    envelope.request_id,
                                    ServerMessage::CopyModeCancelled {
                                        terminal_id,
                                        screen,
                                    },
                                ).await?;
                            }
                            Err(CommandError::CopyMode(error)) => {
                                send(
                                    &mut connection,
                                    envelope.request_id,
                                    ServerMessage::CopyModeError {
                                        terminal_id,
                                        error,
                                    },
                                ).await?;
                            }
                            Err(error) => {
                                send_command_error(&mut connection, envelope.request_id, error).await?;
                            }
                        }
                    }
                    ClientMessage::MouseInput { terminal_id, event } => {
                        if let Err(reason) = event.validate() {
                            tracing::warn!(
                                %terminal_id,
                                ?event,
                                reason,
                                "discarding invalid fire-and-forget mouse input"
                            );
                            continue;
                        }
                        if attachment.terminal(terminal_id).is_none() {
                            continue;
                        }
                        match attachment.mouse_input(terminal_id, event).await {
                            Ok(Some(screen)) => {
                                send_snapshot(&mut connection, terminal_id, screen, false).await?;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                respond_to_ui_event_error(
                                    &mut connection,
                                    envelope.request_id,
                                    "mouse input",
                                    error,
                                    UiEventPolicy::Disposable,
                                ).await?;
                            }
                        }
                    }
                    ClientMessage::ResetViewport { terminal_id } => {
                        if terminal_id != attachment.focused.selected.terminal_id {
                            continue;
                        }
                        match attachment.return_focused_to_bottom().await {
                            Ok(Some(screen)) => {
                                send_snapshot(&mut connection, terminal_id, screen, false).await?;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                respond_to_ui_event_error(
                                    &mut connection,
                                    envelope.request_id,
                                    "reset viewport",
                                    error,
                                    UiEventPolicy::Disposable,
                                ).await?;
                            }
                        }
                    }
                    ClientMessage::RefreshTerminal { terminal_id } => {
                        if attachment.terminal(terminal_id).is_none() {
                            continue;
                        }
                        match attachment.refresh_snapshot(terminal_id).await {
                            Ok(Some(screen)) => {
                                send_snapshot(&mut connection, terminal_id, screen, true).await?;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                respond_to_ui_event_error(
                                    &mut connection,
                                    envelope.request_id,
                                    "refresh terminal",
                                    error,
                                    UiEventPolicy::Disposable,
                                ).await?;
                            }
                        }
                    }
                    ClientMessage::Resize { terminal_id, size } => {
                        if let Err(error) = size.validate() {
                            send_error(&mut connection, envelope.request_id, "invalid_size", &error.to_string()).await?;
                        } else if terminal_id == attachment.focused.selected.terminal_id {
                            match attachment.resize_focused(size).await {
                                Ok(size) if envelope.request_id.is_some() => {
                                    send(
                                        &mut connection,
                                        envelope.request_id,
                                        ServerMessage::TerminalResized { terminal_id, size },
                                    ).await?;
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    respond_to_ui_event_error(
                                        &mut connection,
                                        envelope.request_id,
                                        "resize terminal",
                                        error,
                                        UiEventPolicy::Disposable,
                                    ).await?;
                                }
                            }
                        // An old focused pane can leave one resize queued while exit
                        // fallback selects its replacement. Uncorrelated resizes are
                        // fire-and-forget; correlated callers keep the semantic error.
                        } else if should_report_unfocused_resize(envelope.request_id) {
                            send_error(
                                &mut connection,
                                envelope.request_id,
                                "not_focused",
                                "only the focused terminal may be resized",
                            ).await?;
                        }
                    }
                    ClientMessage::ResizeSplit {
                        tab_id,
                        split_id,
                        ratio,
                    } => {
                        let result = shared
                            .lock()
                            .await
                            .resize_split_for_attachment(
                                attachment.focused.selected.terminal_id,
                                tab_id,
                                split_id,
                                ratio,
                            );
                        if let Err(error) = result {
                            tracing::debug!(
                                code = error.code,
                                message = error.message,
                                %tab_id,
                                %split_id,
                                "discarding stale or invalid split resize"
                            );
                        }
                    }
                    ClientMessage::SelectTarget { selector, expected } => {
                        let selection = match observe_selection(
                            &shared,
                            &selector,
                            expected.as_ref(),
                            attachment.focused.selected.terminal_id,
                        ).await {
                            Ok(selection) => selection,
                            Err(error) => {
                                send_error(&mut connection, envelope.request_id, error.code, &error.message).await?;
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
                                &mut connection,
                                envelope.request_id,
                                ServerMessage::TargetSelected { selected: attachment.selected() },
                            ).await?;
                            continue;
                        }
                        match switch_candidate(
                            &shared,
                            selector,
                            expected.as_ref(),
                            client,
                            attachment.size(),
                        )
                        .await
                        {
                            Ok(candidate) => {
                                if let Err(error) = attachment.close().await {
                                    send_command_error(&mut connection, envelope.request_id, error).await?;
                                    continue;
                                }
                                attachment = candidate;
                                send(
                                    &mut connection,
                                    envelope.request_id,
                                    ServerMessage::TargetSelected { selected: attachment.selected() },
                                ).await?;
                            }
                            Err(error) => send_error(&mut connection, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::CreateWorkspace { session_id, name, cwd, program, argv } => {
                        if session_id != attachment.focused.selected.session_id {
                            send_error(&mut connection, envelope.request_id, "outside_session", "workspace must be created in the attached session").await?;
                            continue;
                        }
                        let inherited_cwd = if cwd.is_none() {
                            let terminal = attachment.focused_terminal();
                            Some((
                                terminal.child_pid(),
                                terminal.spawn_cwd().to_path_buf(),
                            ))
                        } else {
                            None
                        };
                        let request = CreateWorkspaceRequest {
                            session_id,
                            source_workspace_id: attachment.focused.selected.workspace_id,
                            source_terminal_id: attachment.focused.selected.terminal_id,
                            name,
                            cwd,
                            inherited_cwd,
                            program,
                            argv,
                            size: spawn_size,
                        };
                        match create_workspace(&shared, &exited, request, CreationMode::Attached(client)).await {
                            Ok(CreatedTerminal::Attached(target)) => {
                                let selected = target.selected.clone();
                                match focus_leased_attachment(&shared, &mut attachment, target).await {
                                    Ok(()) => {
                                        send(&mut connection, envelope.request_id, ServerMessage::WorkspaceCreated { selected }).await?;
                                        send(
                                            &mut connection,
                                            envelope.request_id,
                                            ServerMessage::TargetSelected { selected: attachment.selected() },
                                        ).await?;
                                    }
                                    Err(error) => send_error(
                                        &mut connection,
                                        envelope.request_id,
                                        error.code,
                                        &error.message,
                                    ).await?,
                                }
                            }
                            Ok(CreatedTerminal::Detached(_)) => unreachable!("attached creation acquires a lease"),
                            Err(error) => send_error(&mut connection, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::CreateTab { workspace_id, name, cwd, program, argv } => {
                        let inherited_cwd = if cwd.is_none()
                            && workspace_id == attachment.focused.selected.workspace_id
                        {
                            let terminal = attachment.focused_terminal();
                            Some((
                                terminal.child_pid(),
                                terminal.spawn_cwd().to_path_buf(),
                            ))
                        } else {
                            None
                        };
                        let request = CreateTabRequest {
                            workspace_id,
                            context: None,
                            name,
                            cwd,
                            inherited_cwd,
                            program,
                            argv,
                            size: spawn_size,
                        };
                        match create_tab(&shared, &exited, request, CreationMode::Attached(client)).await {
                            Ok(CreatedTerminal::Attached(target)) => {
                                let selected = target.selected.clone();
                                match focus_leased_attachment(&shared, &mut attachment, target).await {
                                    Ok(()) => {
                                        send(&mut connection, envelope.request_id, ServerMessage::TabCreated { selected }).await?;
                                        send(
                                            &mut connection,
                                            envelope.request_id,
                                            ServerMessage::TargetSelected { selected: attachment.selected() },
                                        ).await?;
                                    }
                                    Err(error) => send_error(
                                        &mut connection,
                                        envelope.request_id,
                                        error.code,
                                        &error.message,
                                    ).await?,
                                }
                            }
                            Ok(CreatedTerminal::Detached(_)) => unreachable!("attached creation returns a lease"),
                            Err(error) => send_error(&mut connection, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::CreatePane { tab_id, cwd, program, argv } => {
                        if tab_id != attachment.tab_id() {
                            send_error(
                                &mut connection,
                                envelope.request_id,
                                "different_tab",
                                "interactive pane creation requires the currently selected tab",
                            ).await?;
                            continue;
                        }
                        let request = CreatePaneRequest {
                            target: PaneCreationTarget::Append(tab_id),
                            context: None,
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
                                        send(&mut connection, envelope.request_id, ServerMessage::PaneCreated { selected }).await?;
                                        send(
                                            &mut connection,
                                            envelope.request_id,
                                            ServerMessage::TargetSelected { selected: attachment.selected() },
                                        ).await?;
                                    }
                                    Err(error) => send_error(
                                        &mut connection,
                                        envelope.request_id,
                                        error.code,
                                        &error.message,
                                    ).await?,
                                }
                            }
                            Ok(CreatedTerminal::Detached(_)) => unreachable!("attached creation returns a lease"),
                            Err(error) => send_error(&mut connection, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::SplitPane { pane_id, direction, cwd, program, argv } => {
                        if pane_id != attachment.focused.selected.pane_id {
                            send_error(
                                &mut connection,
                                envelope.request_id,
                                "not_focused",
                                "interactive splitting requires the focused pane",
                            ).await?;
                            continue;
                        }
                        let request = CreatePaneRequest {
                            target: PaneCreationTarget::Split { anchor: pane_id, direction },
                            context: None,
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
                                        send(&mut connection, envelope.request_id, ServerMessage::PaneCreated { selected }).await?;
                                        send(
                                            &mut connection,
                                            envelope.request_id,
                                            ServerMessage::TargetSelected { selected: attachment.selected() },
                                        ).await?;
                                    }
                                    Err(error) => send_error(&mut connection, envelope.request_id, error.code, &error.message).await?,
                                }
                            }
                            Ok(CreatedTerminal::Detached(_)) => unreachable!("attached split returns a lease"),
                            Err(error) => send_error(&mut connection, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::ListResources => {
                        let snapshot = shared.lock().await.resources.snapshot();
                        send(&mut connection, envelope.request_id, ServerMessage::Resources { snapshot }).await?;
                    }
                    ClientMessage::AcknowledgeAgent { terminal_id, event_revision } => {
                        let result = shared
                            .lock()
                            .await
                            .acknowledge_agent(terminal_id, event_revision);
                        match result {
                            Ok(()) => send(
                                &mut connection,
                                envelope.request_id,
                                ServerMessage::CommandCompleted {
                                    command: AcknowledgedCommand::AcknowledgeAgent,
                                },
                            ).await?,
                            Err(error) => send_error(
                                &mut connection,
                                envelope.request_id,
                                error.code,
                                &error.message,
                            ).await?,
                        }
                    }
                    ClientMessage::Detach => {
                        match attachment.close().await {
                            Ok(()) => {
                                send(&mut connection, envelope.request_id, ServerMessage::Detached).await?;
                                return Ok(());
                            }
                            Err(error) => {
                                send_command_error(&mut connection, envelope.request_id, error).await?;
                            }
                        }
                    }
                    ClientMessage::Ping => send(&mut connection, envelope.request_id, ServerMessage::Pong { daemon_pid: std::process::id() }).await?,
                    ClientMessage::RenameTarget { selector, name } => {
                        let result = {
                            let mut state = shared.lock().await;
                            if interactive_rename_allowed(
                                &state.resources,
                                attachment.focused.selected.session_id,
                                attachment.focused.selected.workspace_id,
                                &selector,
                            ) {
                                state.rename_target(selector, name)
                            } else {
                                Err(DaemonError::new(
                                    "outside_scope",
                                    "interactive rename target is outside the active resource list",
                                ))
                            }
                        };
                        match result {
                            Ok(resource_revision) => send(
                                &mut connection,
                                envelope.request_id,
                                ServerMessage::TargetRenamed { resource_revision },
                            ).await?,
                            Err(error) => send_error(&mut connection, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::CloseTarget { selector } => {
                        match close_interactive_target(
                            &shared,
                            &attachment.focused.selected,
                            selector,
                        ).await {
                            Ok(()) => send(
                                &mut connection,
                                envelope.request_id,
                                ServerMessage::CommandCompleted {
                                    command: AcknowledgedCommand::CloseTarget,
                                },
                            ).await?,
                            Err(error) => send_error(
                                &mut connection,
                                envelope.request_id,
                                error.code,
                                &error.message,
                            ).await?,
                        }
                    }
                    ClientMessage::OpenLocation { .. } | ClientMessage::MovePane { .. } | ClientMessage::Contextual { .. } | ClientMessage::RetireWorkspace { .. } | ClientMessage::PublishToken { .. } | ClientMessage::ReportAgent { .. } | ClientMessage::TerminalInput { .. } | ClientMessage::ReadTerminalOutput { .. } | ClientMessage::WaitTerminalOutput { .. } | ClientMessage::PromptAgent { .. } | ClientMessage::WaitAgent { .. } | ClientMessage::WatchResources | ClientMessage::Shutdown => send_error(&mut connection, envelope.request_id, "control_only", "command requires a control connection").await?,
                    ClientMessage::Hello { .. } => send_error(&mut connection, envelope.request_id, "already_hello", "hello was already received").await?,
                }
            },
            changed = attachment.resource_changes.changed() => {
                if changed.is_err() {
                    break;
                }
                if let Some(reconciled) = reconcile_attachment(&shared, &mut attachment).await? {
                    if reconciled.view_changed {
                        send(
                            &mut connection,
                            None,
                            ServerMessage::TargetSelected { selected: attachment.selected() },
                        ).await?;
                    }
                    send(
                        &mut connection,
                        None,
                        ServerMessage::ResourcesChanged {
                            snapshot: reconciled.snapshot,
                        },
                    ).await?;
                }
            }
            update = attachment.updates.recv() => match update {
                Some(AttachmentUpdate::Snapshot { terminal_id, generation, screen }) => {
                    match attachment.snapshot_for_update(terminal_id, generation, screen).await {
                        Ok(Some((screen, first))) => {
                            send_snapshot(&mut connection, terminal_id, screen, first).await?;
                        }
                        Ok(None) => {}
                        Err(CommandError::CopyMode(error)) => {
                            // Runtime cursor invalidation also publishes its
                            // canonical snapshot. Send the semantic exit first
                            // so an interactive client cannot retain local copy
                            // mode while accepting that normal screen.
                            send(
                                &mut connection,
                                None,
                                ServerMessage::CopyModeError {
                                    terminal_id,
                                    error,
                                },
                            ).await?;
                        }
                        Err(error) => {
                            respond_to_ui_event_error(
                                &mut connection,
                                None,
                                "render historical viewport",
                                error,
                                UiEventPolicy::Disposable,
                            ).await?;
                        }
                    }
                }
                Some(AttachmentUpdate::Error { terminal_id, generation, message }) => {
                    if attachment.accepts(terminal_id, generation) {
                        send_error(&mut connection, None, "terminal", &message).await?;
                    }
                }
                Some(AttachmentUpdate::Exited { terminal_id, generation, exit_code }) => {
                    if !attachment.accepts(terminal_id, generation) {
                        continue;
                    }
                    attachment.forget_copy_mode(terminal_id);
                    let focused = terminal_id == attachment.focused.selected.terminal_id;
                    let replacements = if focused {
                        exit_replacement_ids(&shared, &attachment, terminal_id).await
                    } else {
                        Vec::new()
                    };
                    send(
                        &mut connection,
                        None,
                        ServerMessage::TerminalExited { terminal_id, exit_code },
                    ).await?;
                    if focused {
                        let mut replacement = None;
                        for terminal_id in replacements {
                            if let Ok(candidate) = lease_view(
                                &shared,
                                Some(TargetSelector::Terminal(terminal_id)),
                                None,
                                client,
                                attachment.size(),
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
                        if let Err(error) = attachment.close().await {
                            respond_to_ui_event_error(
                                &mut connection,
                                None,
                                "clean up exited attachment",
                                error,
                                UiEventPolicy::Disposable,
                            ).await?;
                            break;
                        }
                        attachment = replacement;
                    } else {
                        attachment.remove(terminal_id);
                    }
                    send(
                        &mut connection,
                        None,
                        ServerMessage::TargetSelected { selected: attachment.selected() },
                    ).await?;
                    let snapshot = shared.lock().await.resources.snapshot();
                    send(
                        &mut connection,
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
    .await;
    let cleanup_result = attachment
        .close()
        .await
        .context("clean up disconnected terminal attachment");
    match (connection_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            Err(error.context(format!("also failed attachment cleanup: {cleanup_error:#}")))
        }
    }
}

async fn control_loop(
    connection: &mut ClientConnection,
    shared: Shared,
    exited: mpsc::UnboundedSender<TerminalId>,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let mut watched_changes: Option<watch::Receiver<u64>> = None;
    let mut retirement = None;
    loop {
        let frame = tokio::select! {
            frame = connection.next() => frame,
            changed = watched_resource_change(&mut watched_changes) => {
                if changed.is_err() {
                    break;
                }
                let snapshot = shared.lock().await.resources.snapshot();
                send(connection, None, ServerMessage::ResourcesChanged { snapshot }).await?;
                continue;
            }
        };
        let Some(frame) = frame else { break };
        let envelope: Envelope<ClientMessage> = decode_payload(&frame?)?;
        if let Some(operation) = fire_and_forget_operation(&envelope.message)
            && reject_fire_and_forget_request_id(connection, envelope.request_id, operation).await?
        {
            continue;
        }
        match envelope.message {
            ClientMessage::Ping => {
                send(
                    connection,
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
                        connection,
                        envelope.request_id,
                        ServerMessage::LocationOpened {
                            selected,
                            disposition,
                        },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
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
                    context: None,
                    name,
                    cwd,
                    inherited_cwd: None,
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
                        connection,
                        envelope.request_id,
                        ServerMessage::TabCreated { selected },
                    )
                    .await?
                }
                Ok(CreatedTerminal::Attached(_)) => {
                    unreachable!("detached creation does not acquire a lease")
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
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
                    context: None,
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
                        connection,
                        envelope.request_id,
                        ServerMessage::PaneCreated { selected },
                    )
                    .await?
                }
                Ok(CreatedTerminal::Attached(_)) => {
                    unreachable!("detached creation does not acquire a lease")
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
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
                    context: None,
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
                        connection,
                        envelope.request_id,
                        ServerMessage::PaneCreated { selected },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
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
                            connection,
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
                        send_error(connection, envelope.request_id, error.code, &error.message).await?
                    }
                }
            }
            ClientMessage::Contextual { context, command } => {
                handle_contextual_command(
                    connection,
                    &shared,
                    &exited,
                    envelope.request_id,
                    context,
                    command,
                )
                .await?;
            }
            ClientMessage::PublishToken {
                extension_id,
                token,
                value,
                target,
            } => {
                let result = shared
                    .lock()
                    .await
                    .publish_token(&extension_id, &token, value, target);
                match result {
                    Ok(publication) => {
                        send(
                            connection,
                            envelope.request_id,
                            ServerMessage::TokenPublished {
                                resource_revision: publication.revision,
                                changed: publication.changed,
                            },
                        )
                        .await?
                    }
                    Err(error) => {
                        send_error(connection, envelope.request_id, error.code, &error.message)
                            .await?
                    }
                }
            }
            ClientMessage::ListResources => {
                let snapshot = shared.lock().await.resources.snapshot();
                send(
                    connection,
                    envelope.request_id,
                    ServerMessage::Resources { snapshot },
                )
                .await?;
            }
            ClientMessage::WatchResources => {
                // Subscribing and snapshotting under one lock leaves no gap in
                // which a change could go unstreamed.
                let snapshot = {
                    let state = shared.lock().await;
                    watched_changes = Some(state.resource_changes.subscribe());
                    state.resources.snapshot()
                };
                send(
                    connection,
                    envelope.request_id,
                    ServerMessage::Resources { snapshot },
                )
                .await?;
            }
            ClientMessage::CloseTarget { selector } => {
                match close_target(&shared, selector, None).await {
                    Ok(_) => {
                        send(
                            connection,
                            envelope.request_id,
                            ServerMessage::CommandCompleted {
                                command: AcknowledgedCommand::CloseTarget,
                            },
                        )
                        .await?
                    }
                    Err(error) => {
                        send_error(connection, envelope.request_id, error.code, &error.message).await?
                    }
                }
            }
            ClientMessage::RetireWorkspace {
                workspace_id,
                context,
            } => match prepare_workspace_retirement(&shared, workspace_id, context).await {
                Ok(pending) => {
                    retirement = Some(pending);
                    send(
                        connection,
                        envelope.request_id,
                        ServerMessage::CommandCompleted {
                            command: AcknowledgedCommand::RetireWorkspace,
                        },
                    )
                    .await?;
                    // The writer task can deliver the acknowledgement while
                    // this handler waits for the caller to receive it, flush
                    // any user-facing output, and disconnect. Ignore any
                    // further input: retirement is already committed.
                    while matches!(connection.next().await, Some(Ok(_))) {}
                    break;
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::RenameTarget { selector, name } => {
                let result = shared.lock().await.rename_target(selector, name);
                match result {
                    Ok(_) => {
                        send(
                            connection,
                            envelope.request_id,
                            ServerMessage::CommandCompleted {
                                command: AcknowledgedCommand::RenameTarget,
                            },
                        )
                        .await?
                    }
                    Err(error) => {
                        send_error(connection, envelope.request_id, error.code, &error.message).await?
                    }
                }
            }
            ClientMessage::ReportAgent {
                terminal_id,
                report,
                metadata,
            } => {
                let result = shared
                    .lock()
                    .await
                    .report_agent(terminal_id, report, metadata);
                match result {
                    Ok(()) => {
                        send(
                            connection,
                            envelope.request_id,
                            ServerMessage::CommandCompleted {
                                command: AcknowledgedCommand::ReportAgent,
                            },
                        )
                        .await?
                    }
                    Err(error) => {
                        send_error(connection, envelope.request_id, error.code, &error.message).await?
                    }
                }
            }
            ClientMessage::AcknowledgeAgent {
                terminal_id,
                event_revision,
            } => {
                let result = shared
                    .lock()
                    .await
                    .acknowledge_agent(terminal_id, event_revision);
                match result {
                    Ok(()) => {
                        send(
                            connection,
                            envelope.request_id,
                            ServerMessage::CommandCompleted {
                                command: AcknowledgedCommand::AcknowledgeAgent,
                            },
                        )
                        .await?
                    }
                    Err(error) => {
                        send_error(connection, envelope.request_id, error.code, &error.message)
                            .await?
                    }
                }
            }
            ClientMessage::TerminalInput {
                terminal_id,
                operation,
            } => match targeted_terminal_input(&shared, terminal_id, operation).await {
                Ok(()) => {
                    send(
                        connection,
                        envelope.request_id,
                        ServerMessage::CommandCompleted {
                            command: AcknowledgedCommand::TerminalInput,
                        },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::ReadTerminalOutput {
                terminal_id,
                source,
                rows,
                ansi,
            } => match read_terminal_output(&shared, terminal_id, source, rows, ansi).await {
                Ok(output) => {
                    send(
                        connection,
                        envelope.request_id,
                        ServerMessage::TerminalOutput { output },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::WaitTerminalOutput {
                terminal_id,
                source,
                rows,
                matcher,
                timeout_ms,
            } => match wait_terminal_output(
                &shared,
                terminal_id,
                source,
                rows,
                matcher,
                timeout_ms,
            )
            .await
            {
                Ok((output, start, end, matched)) => {
                    send(
                        connection,
                        envelope.request_id,
                        ServerMessage::TerminalOutputMatched {
                            output,
                            start,
                            end,
                            matched,
                        },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::PromptAgent {
                terminal_id,
                text,
                wait,
                timeout_ms,
            } => match prompt_agent(&shared, terminal_id, text, wait, timeout_ms).await {
                Ok(result) => {
                    let message = match result.activity {
                        Some(activity) => ServerMessage::AgentSettled {
                            terminal_id,
                            barrier_revision: result.barrier_revision,
                            working_revision: result.working_revision,
                            activity,
                        },
                        None => ServerMessage::AgentPrompted {
                            terminal_id,
                            barrier_revision: result.barrier_revision,
                        },
                    };
                    send(connection, envelope.request_id, message).await?
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::WaitAgent {
                terminal_id,
                timeout_ms,
            } => match wait_agent(&shared, terminal_id, timeout_ms).await {
                Ok(result) => {
                    send(
                        connection,
                        envelope.request_id,
                        ServerMessage::AgentSettled {
                            terminal_id,
                            barrier_revision: result.barrier_revision,
                            working_revision: result.working_revision,
                            activity: result.activity.expect("agent wait always returns activity"),
                        },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::Shutdown => {
                {
                    let mut state = shared.lock().await;
                    state.accepting = false;
                }
                let response = send(
                    connection,
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
                send(connection, envelope.request_id, ServerMessage::Detached).await?;
                break;
            }
            ClientMessage::MouseInput { .. }
            | ClientMessage::ResetViewport { .. }
            | ClientMessage::RefreshTerminal { .. }
            | ClientMessage::ResizeSplit { .. } => {}
            ClientMessage::CreateWorkspace { .. }
            | ClientMessage::Input { .. }
            | ClientMessage::Paste { .. }
            | ClientMessage::CopyMode { .. }
            | ClientMessage::Resize { .. }
            | ClientMessage::SelectTarget { .. } => {
                send_error(
                    connection,
                    envelope.request_id,
                    "interactive_only",
                    "workspace creation, input, paste, resize, and target selection require an interactive connection",
                )
                .await?
            }
            ClientMessage::Hello { .. } => {
                send_error(
                    connection,
                    envelope.request_id,
                    "already_hello",
                    "hello was already received",
                )
                .await?
            }
        }
    }
    if let Some(retirement) = retirement
        && let Err(error) = finish_target_close(&shared, retirement).await
    {
        tracing::warn!(%error, "workspace retirement failed after acknowledgement");
    }
    Ok(())
}

async fn handle_contextual_command(
    connection: &mut ClientConnection,
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    request_id: Option<Uuid>,
    context: TerminalContext,
    command: ContextualCommand,
) -> Result<()> {
    match command {
        ContextualCommand::CreateTab {
            name,
            cwd,
            program,
            argv,
        } => match create_tab(
            shared,
            exited,
            CreateTabRequest {
                workspace_id: context.workspace_id,
                context: Some(context),
                name,
                cwd,
                inherited_cwd: None,
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
                    connection,
                    request_id,
                    ServerMessage::TabCreated { selected },
                )
                .await?
            }
            Ok(CreatedTerminal::Attached(_)) => {
                unreachable!("contextual control creation is detached")
            }
            Err(error) => send_error(connection, request_id, error.code, &error.message).await?,
        },
        ContextualCommand::CreatePane { cwd, program, argv } => {
            handle_contextual_pane_creation(
                connection,
                shared,
                exited,
                request_id,
                context,
                PaneCreationTarget::Append(context.tab_id),
                cwd,
                program,
                argv,
            )
            .await?;
        }
        ContextualCommand::SplitPane {
            cwd,
            program,
            argv,
            direction,
        } => {
            handle_contextual_pane_creation(
                connection,
                shared,
                exited,
                request_id,
                context,
                PaneCreationTarget::Split {
                    anchor: context.pane_id,
                    direction,
                },
                cwd,
                program,
                argv,
            )
            .await?;
        }
        ContextualCommand::MovePane { destination_tab_id } => {
            let result = {
                let mut state = shared.lock().await;
                (|| -> Result<_, DaemonError> {
                    state.validate_terminal_context(context)?;
                    state.move_pane(context.pane_id, destination_tab_id)
                })()
            };
            match result {
                Ok(moved) => {
                    send(
                        connection,
                        request_id,
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
                    send_error(connection, request_id, error.code, &error.message).await?
                }
            }
        }
        ContextualCommand::Close { scope } => {
            let selector = context_target(context, scope);
            match close_target(shared, selector, Some(context)).await {
                Ok(()) => {
                    send(
                        connection,
                        request_id,
                        ServerMessage::CommandCompleted {
                            command: AcknowledgedCommand::CloseTarget,
                        },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(connection, request_id, error.code, &error.message).await?
                }
            }
        }
        ContextualCommand::Rename { scope, name } => {
            let result = {
                let mut state = shared.lock().await;
                (|| -> Result<_, DaemonError> {
                    state.validate_terminal_context(context)?;
                    let selector = match scope {
                        ContextScope::Session => {
                            RenameSelector::Session(SessionSelector::Id(context.session_id))
                        }
                        ContextScope::Workspace => RenameSelector::Workspace(context.workspace_id),
                        ContextScope::Tab => RenameSelector::Tab(context.tab_id),
                        ContextScope::Pane => {
                            return Err(DaemonError::new(
                                "invalid_context_command",
                                "panes cannot be renamed",
                            ));
                        }
                    };
                    state.rename_target(selector, name)
                })()
            };
            match result {
                Ok(_) => {
                    send(
                        connection,
                        request_id,
                        ServerMessage::CommandCompleted {
                            command: AcknowledgedCommand::RenameTarget,
                        },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(connection, request_id, error.code, &error.message).await?
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_contextual_pane_creation(
    connection: &mut ClientConnection,
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    request_id: Option<Uuid>,
    context: TerminalContext,
    target: PaneCreationTarget,
    cwd: Option<PathBuf>,
    program: Option<PathBuf>,
    argv: Vec<String>,
) -> Result<()> {
    match create_pane(
        shared,
        exited,
        CreatePaneRequest {
            target,
            context: Some(context),
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
                connection,
                request_id,
                ServerMessage::PaneCreated { selected },
            )
            .await?
        }
        Ok(CreatedTerminal::Attached(_)) => {
            unreachable!("contextual control creation is detached")
        }
        Err(error) => send_error(connection, request_id, error.code, &error.message).await?,
    }
    Ok(())
}

fn context_target(context: TerminalContext, scope: ContextScope) -> TargetSelector {
    match scope {
        ContextScope::Session => TargetSelector::Session(SessionSelector::Id(context.session_id)),
        ContextScope::Workspace => TargetSelector::Workspace(context.workspace_id),
        ContextScope::Tab => TargetSelector::Tab(context.tab_id),
        ContextScope::Pane => TargetSelector::Pane(context.pane_id),
    }
}

/// Pends forever until [`ClientMessage::WatchResources`] installs a receiver.
async fn watched_resource_change(
    changes: &mut Option<watch::Receiver<u64>>,
) -> Result<(), tokio::sync::watch::error::RecvError> {
    match changes {
        Some(changes) => changes.changed().await,
        None => std::future::pending().await,
    }
}

async fn lease_view(
    shared: &Shared,
    selector: Option<TargetSelector>,
    expected: Option<&SelectionExpectation>,
    client: ClientId,
    size: TerminalSize,
) -> Result<Attachment, DaemonError> {
    let mut state = shared.lock().await;
    if !state.accepting {
        return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
    }
    let focused = state.resources.resolve_terminal_target(selector)?;
    validate_selection_expectation(focused, expected)?;
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
    let guard = focused_runtime
        .lease
        .acquire(client, size, Arc::clone(&focused_runtime.handle))
        .ok_or_else(|| DaemonError::new("attachment_error", "client already attached"))?;
    let focused_target = LeasedTarget {
        selected: selected_target(focused, &focused_runtime.handle),
        terminal: Arc::clone(&focused_runtime.handle),
        lease: guard,
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
    let revision = state.resources.focus_pane(focused.pane_id)?;
    state.publish_resource_change(revision);
    Ok(Attachment::new(
        client,
        panes,
        focused_target,
        layout,
        fallback_terminal_ids,
        state.resources.revision(),
        state.resource_changes.subscribe(),
    ))
}

fn validate_selection_expectation(
    path: crate::resources::ResolvedTerminalPath,
    expected: Option<&SelectionExpectation>,
) -> Result<(), DaemonError> {
    let matches = match expected {
        None => true,
        Some(SelectionExpectation::Tab(tab_id)) => path.tab_id == *tab_id,
        Some(SelectionExpectation::Workspace(workspace_id)) => path.workspace_id == *workspace_id,
        Some(SelectionExpectation::Session(session_id)) => path.session_id == *session_id,
    };
    if matches {
        Ok(())
    } else {
        Err(DaemonError::new(
            "target_moved",
            "navigation target moved outside its expected scope",
        ))
    }
}

fn interactive_rename_allowed(
    resources: &ResourceTree,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    selector: &RenameSelector,
) -> bool {
    match selector {
        RenameSelector::Workspace(target) => resources
            .session_id_for_workspace(*target)
            .is_ok_and(|owner| owner == session_id),
        RenameSelector::Tab(target) => resources
            .workspace_id_for_tab(*target)
            .is_ok_and(|owner| owner == workspace_id),
        RenameSelector::Session(_) => false,
    }
}

fn interactive_close_allowed(
    resources: &ResourceTree,
    focused: &SelectedTarget,
    selector: &TargetSelector,
) -> bool {
    match selector {
        TargetSelector::Pane(target) => *target == focused.pane_id,
        TargetSelector::Tab(target) => resources
            .workspace_id_for_tab(*target)
            .is_ok_and(|owner| owner == focused.workspace_id),
        TargetSelector::Workspace(target) => resources
            .session_id_for_workspace(*target)
            .is_ok_and(|owner| owner == focused.session_id),
        TargetSelector::Session(_) | TargetSelector::Terminal(_) => false,
    }
}

async fn focus_leased_attachment(
    shared: &Shared,
    attachment: &mut Attachment,
    mut focused: LeasedTarget,
) -> Result<(), DaemonError> {
    attachment
        .clear_active_copy_mode()
        .await
        .map_err(|error| DaemonError::new(command_error_code(&error), error.to_string()))?;
    let mut state = shared.lock().await;
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
    let revision = state.resources.focus_pane(path.pane_id)?;
    state.publish_resource_change(revision);
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
    expected: Option<&SelectionExpectation>,
    focused_terminal_id: TerminalId,
) -> Result<TargetSelection, DaemonError> {
    let state = shared.lock().await;
    if !state.accepting {
        return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
    }
    let path = state
        .resources
        .resolve_terminal_target(Some(selector.clone()))?;
    validate_selection_expectation(path, expected)?;
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
    expected: Option<&SelectionExpectation>,
    client: ClientId,
    size: TerminalSize,
) -> Result<Attachment, DaemonError> {
    let attachment = lease_view(shared, Some(selector), expected, client, size).await?;
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
        // `name` names the workspace only when it joins an existing session;
        // on a fresh session it names the session and the workspace starts
        // unnamed, presenting as its live location.
        let workspace_name = match destination {
            CheckoutDestination::AddWorkspace { .. } => name.clone().unwrap_or_default(),
            _ => String::new(),
        };
        let proposed_session = initial_path(&resolved, session_name, TerminalId::new());
        let proposed_workspace = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name,
            root: resolved.workspace_root.clone(),
            tab_id: TabId::new(),
            tab_name: String::new(),
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
        let spawn_target = match destination {
            CheckoutDestination::CreateSession => crate::resources::ResolvedTerminalPath {
                session_id: proposed_session.session_id,
                workspace_id: proposed_session.workspace_id,
                tab_id: proposed_session.tab_id,
                pane_id: proposed_session.pane_id,
                terminal_id: proposed_session.terminal_id,
            },
            CheckoutDestination::AddWorkspace { session_id } => {
                crate::resources::ResolvedTerminalPath {
                    session_id,
                    workspace_id: proposed_workspace.workspace_id,
                    tab_id: proposed_workspace.tab_id,
                    pane_id: proposed_workspace.pane_id,
                    terminal_id: proposed_workspace.terminal_id,
                }
            }
            CheckoutDestination::Existing(_) => unreachable!(),
        };
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                id: spawn_target.terminal_id,
                program,
                argv,
                cwd: resolved.cwd.clone(),
                env: terminal_env(&state.child_env, spawn_target),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .map_err(|error| DaemonError::new("spawn_failed", error.to_string()))?,
        );
        let (path, disposition, insertion) = match destination {
            CheckoutDestination::CreateSession => {
                let path = proposed_session;
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
                let path = proposed_workspace;
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
    context: Option<TerminalContext>,
    name: Option<String>,
    cwd: Option<PathBuf>,
    inherited_cwd: Option<(u32, PathBuf)>,
    program: Option<PathBuf>,
    argv: Vec<String>,
    size: TerminalSize,
}

struct CreateWorkspaceRequest {
    session_id: SessionId,
    source_workspace_id: WorkspaceId,
    source_terminal_id: TerminalId,
    name: Option<String>,
    cwd: Option<PathBuf>,
    inherited_cwd: Option<(u32, PathBuf)>,
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

async fn create_workspace(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    request: CreateWorkspaceRequest,
    mode: CreationMode,
) -> Result<CreatedTerminal, DaemonError> {
    let CreateWorkspaceRequest {
        session_id,
        source_workspace_id,
        source_terminal_id,
        name,
        cwd,
        inherited_cwd,
        program,
        argv,
        size,
    } = request;
    let source_root = {
        let state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        if state
            .resources
            .session_id_for_workspace(source_workspace_id)?
            != session_id
        {
            return Err(DaemonError::new(
                "outside_session",
                "workspace source moved outside the target session",
            ));
        }
        let source = state
            .resources
            .resolve_terminal_target(Some(TargetSelector::Terminal(source_terminal_id)))?;
        if source.session_id != session_id || source.workspace_id != source_workspace_id {
            return Err(DaemonError::new(
                "invalid_source",
                "workspace creation source does not match the attached terminal",
            ));
        }
        let runtime = state
            .runtimes
            .get(&source_terminal_id)
            .ok_or_else(|| DaemonError::new("not_found", "terminal runtime not found"))?;
        if !matches!(
            *runtime.handle.subscribe_lifecycle().borrow(),
            TerminalLifecycle::Running
        ) {
            return Err(DaemonError::new(
                "terminal_exited",
                "workspace creation source has exited",
            ));
        }
        state
            .resources
            .workspace_root(source_workspace_id)?
            .to_path_buf()
    };
    let cwd = resolve_creation_cwd(&source_root, cwd, inherited_cwd).await?;
    let root = cwd.clone();
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
        let source = state
            .resources
            .resolve_terminal_target(Some(TargetSelector::Terminal(source_terminal_id)))?;
        if source.session_id != session_id || source.workspace_id != source_workspace_id {
            return Err(DaemonError::new(
                "source_moved",
                "workspace creation source changed during CWD lookup",
            ));
        }
        let runtime = state
            .runtimes
            .get(&source_terminal_id)
            .ok_or_else(|| DaemonError::new("not_found", "terminal runtime not found"))?;
        if !matches!(
            *runtime.handle.subscribe_lifecycle().borrow(),
            TerminalLifecycle::Running
        ) {
            return Err(DaemonError::new(
                "terminal_exited",
                "workspace creation source exited during CWD lookup",
            ));
        }
        let workspace_name = name.unwrap_or_default();
        let proposed = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name,
            root,
            tab_id: TabId::new(),
            tab_name: String::new(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let mut validated = state.resources.clone();
        validated.add_workspace(session_id, proposed.clone())?;

        let resolved = crate::resources::ResolvedTerminalPath {
            session_id,
            workspace_id: proposed.workspace_id,
            tab_id: proposed.tab_id,
            pane_id: proposed.pane_id,
            terminal_id: proposed.terminal_id,
        };

        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                id: proposed.terminal_id,
                program,
                argv,
                cwd,
                env: terminal_env(&state.child_env, resolved),
                size,
            })
            .map_err(|error| DaemonError::new("spawn_failed", error.to_string()))?,
        );
        let path = proposed;
        let selected = selected_target(resolved, &terminal);
        let insertion = state.register_workspace(session_id, path, Arc::clone(&terminal));
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
                        .acquire(client, size, Arc::clone(&terminal))
                        .expect("new terminal has an independent lease");
                    CreatedTerminal::Attached(LeasedTarget {
                        selected,
                        terminal: Arc::clone(&terminal),
                        lease: guard,
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

async fn create_tab(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    request: CreateTabRequest,
    mode: CreationMode,
) -> Result<CreatedTerminal, DaemonError> {
    let CreateTabRequest {
        workspace_id,
        context,
        name,
        cwd,
        inherited_cwd,
        program,
        argv,
        size,
    } = request;
    let root = {
        let state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        if let Some(context) = context {
            state.validate_terminal_context(context)?;
            if context.workspace_id != workspace_id {
                return Err(DaemonError::new(
                    "context_changed",
                    "calling terminal moved outside the inferred workspace",
                ));
            }
        }
        state.resources.workspace_root(workspace_id)?.to_path_buf()
    };
    let cwd = resolve_creation_cwd(&root, cwd, inherited_cwd).await?;
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
        if let Some(context) = context {
            state.validate_terminal_context(context)?;
            if context.workspace_id != workspace_id {
                return Err(DaemonError::new(
                    "context_changed",
                    "calling terminal moved outside the inferred workspace",
                ));
            }
        }
        let tab_name = name.unwrap_or_default();
        let proposed = TabPath {
            tab_id: TabId::new(),
            tab_name,
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let session_id = state.resources.session_id_for_workspace(workspace_id)?;
        let mut validated = state.resources.clone();
        validated.add_tab(workspace_id, proposed.clone())?;

        let resolved = crate::resources::ResolvedTerminalPath {
            session_id,
            workspace_id,
            tab_id: proposed.tab_id,
            pane_id: proposed.pane_id,
            terminal_id: proposed.terminal_id,
        };

        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                id: proposed.terminal_id,
                program,
                argv,
                cwd,
                env: terminal_env(&state.child_env, resolved),
                size,
            })
            .map_err(|error| DaemonError::new("spawn_failed", error.to_string()))?,
        );
        let path = proposed;
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
                        .acquire(client, size, Arc::clone(&terminal))
                        .expect("new terminal has an independent lease");
                    let target = LeasedTarget {
                        selected,
                        terminal: Arc::clone(&terminal),
                        lease: guard,
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
    context: Option<TerminalContext>,
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
        context,
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
        if let Some(context) = context {
            state.validate_terminal_context(context)?;
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
    let cwd = resolve_creation_cwd(&root, cwd, inherited_cwd).await?;
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
        if let Some(context) = context {
            state.validate_terminal_context(context)?;
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
        let terminal_id = TerminalId::new();
        let mut validated = state.resources.clone();
        match target {
            PaneCreationTarget::Append(_) => {
                validated.add_pane(tab_id, pane_id, terminal_id)?;
            }
            PaneCreationTarget::Split { anchor, direction } => {
                validated.split_pane(anchor, direction, pane_id, terminal_id)?;
            }
        }

        let resolved = crate::resources::ResolvedTerminalPath {
            session_id,
            workspace_id,
            tab_id,
            pane_id,
            terminal_id,
        };

        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                id: terminal_id,
                program,
                argv,
                cwd,
                env: terminal_env(&state.child_env, resolved),
                size,
            })
            .map_err(|error| DaemonError::new("spawn_failed", error.to_string()))?,
        );
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
                        .acquire(client, size, Arc::clone(&terminal))
                        .expect("new terminal has an independent lease");
                    CreatedTerminal::Attached(LeasedTarget {
                        selected,
                        terminal: Arc::clone(&terminal),
                        lease: guard,
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

async fn resolve_creation_cwd(
    root: &Path,
    explicit: Option<PathBuf>,
    inherited: Option<(u32, PathBuf)>,
) -> Result<PathBuf, DaemonError> {
    if explicit.is_some() {
        return resolve_spawn_cwd(root, explicit).await;
    }
    if let Some((pid, fallback)) = inherited {
        if let Some(current) = process_cwd(pid).await
            && let Ok(cwd) = resolve_spawn_cwd(root, Some(current)).await
        {
            return Ok(cwd);
        }
        if let Ok(cwd) = resolve_spawn_cwd(root, Some(fallback)).await {
            return Ok(cwd);
        }
    }
    resolve_spawn_cwd(root, None).await
}

/// Top level of the Git work tree containing `cwd`, or nothing outside one.
/// Bounded like every other observer probe so a hung git cannot stall naming.
async fn worktree_toplevel(cwd: &Path) -> Option<PathBuf> {
    let output = timeout(
        Duration::from_millis(500),
        tokio::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let toplevel = String::from_utf8(output.stdout).ok()?;
    let toplevel = toplevel.trim();
    (!toplevel.is_empty()).then(|| PathBuf::from(toplevel))
}

async fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::uninit();
        let size = i32::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>())
            .expect("proc_vnodepathinfo size fits i32");
        // SAFETY: proc_pidinfo writes at most `size` bytes into this valid buffer
        // and reports how many bytes it filled.
        let written = unsafe {
            libc::proc_pidinfo(
                i32::try_from(pid).ok()?,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written < size {
            return None;
        }
        // SAFETY: the kernel filled the whole struct, including a
        // NUL-terminated current-directory path.
        let info = unsafe { info.assume_init() };
        let path = unsafe { std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
        let path = path.to_str().ok()?;
        (!path.is_empty()).then(|| PathBuf::from(path))
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

struct PendingClose {
    scope: CloseScope,
    handles: Vec<Arc<TerminalHandle>>,
}

async fn prepare_workspace_retirement(
    shared: &Shared,
    workspace_id: WorkspaceId,
    context: Option<TerminalContext>,
) -> Result<PendingClose, DaemonError> {
    let mut state = shared.lock().await;
    if let Some(context) = context {
        state.validate_terminal_context(context)?;
        if context.workspace_id != workspace_id {
            return Err(DaemonError::new(
                "context_changed",
                "calling terminal is no longer in the workspace being retired",
            ));
        }
    }
    let (scope, handles) = state.begin_target_close(TargetSelector::Workspace(workspace_id))?;
    Ok(PendingClose { scope, handles })
}

async fn prepare_target_close(
    shared: &Shared,
    selector: TargetSelector,
    context: Option<TerminalContext>,
) -> Result<PendingClose, DaemonError> {
    let (scope, handles) = {
        let mut state = shared.lock().await;
        if let Some(context) = context {
            state.validate_terminal_context(context)?;
        }
        state.begin_target_close(selector)?
    };
    Ok(PendingClose { scope, handles })
}

async fn finish_target_close(shared: &Shared, pending: PendingClose) -> Result<(), DaemonError> {
    for handle in pending.handles {
        if let Err(error) = handle.close().await {
            let mut state = shared.lock().await;
            state.cancel_target_close(pending.scope);
            return Err(DaemonError::command("close_failed", error));
        }
    }
    Ok(())
}

async fn close_target(
    shared: &Shared,
    selector: TargetSelector,
    context: Option<TerminalContext>,
) -> Result<(), DaemonError> {
    let pending = prepare_target_close(shared, selector, context).await?;
    finish_target_close(shared, pending).await
}

async fn close_interactive_target(
    shared: &Shared,
    focused: &SelectedTarget,
    selector: TargetSelector,
) -> Result<(), DaemonError> {
    let pending = {
        let mut state = shared.lock().await;
        if !interactive_close_allowed(&state.resources, focused, &selector) {
            return Err(DaemonError::new(
                "outside_scope",
                "interactive close target is outside the active resource list",
            ));
        }
        let (scope, handles) = state.begin_target_close(selector)?;
        PendingClose { scope, handles }
    };
    finish_target_close(shared, pending).await
}

async fn command_response(
    connection: &mut ClientConnection,
    request_id: Option<uuid::Uuid>,
    command: AcknowledgedCommand,
    result: Result<(), CommandError>,
    policy: UiEventPolicy,
) -> Result<()> {
    if let Err(error) = result {
        respond_to_ui_event_error(
            connection,
            request_id,
            "terminal input or resize",
            error,
            policy,
        )
        .await?;
    } else if request_id.is_some() {
        send(
            connection,
            request_id,
            ServerMessage::CommandCompleted { command },
        )
        .await?;
    }
    Ok(())
}

enum FocusedInput {
    Bytes(Vec<u8>),
    Paste(String),
}

enum OutputMatcher {
    Literal(String),
    Regex(Regex),
}

impl OutputMatcher {
    fn find(&self, text: &str) -> Option<(usize, usize)> {
        match self {
            Self::Literal(value) => text.find(value).map(|start| (start, start + value.len())),
            Self::Regex(regex) => regex.find(text).map(|found| (found.start(), found.end())),
        }
    }
}

async fn output_terminal(
    shared: &Shared,
    terminal_id: TerminalId,
) -> Result<Arc<TerminalHandle>, DaemonError> {
    let state = shared.lock().await;
    if !state.accepting {
        return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
    }
    if let Err(error) = state
        .resources
        .resolve_terminal_target(Some(TargetSelector::Terminal(terminal_id)))
    {
        if let Some((_, exit_code)) = state
            .exited_terminals
            .iter()
            .find(|(exited, _)| *exited == terminal_id)
        {
            return Err(DaemonError::new(
                "terminal_exited",
                format!("terminal already exited with status {exit_code:?}"),
            ));
        }
        return Err(error.into());
    }
    state
        .runtimes
        .get(&terminal_id)
        .map(|runtime| Arc::clone(&runtime.handle))
        .ok_or_else(|| DaemonError::new("terminal_exited", "terminal is not running"))
}

fn output_error(error: CommandError) -> DaemonError {
    let code = match &error {
        CommandError::Output(OutputCaptureError::InvalidRows) => "invalid_output_rows",
        CommandError::Output(
            OutputCaptureError::TooManyCells { .. } | OutputCaptureError::TooManyBytes { .. },
        ) => "output_too_large",
        CommandError::Output(OutputCaptureError::AlternateScreen) => "alternate_screen",
        CommandError::Output(OutputCaptureError::Emulator(_)) | CommandError::Emulator(_) => {
            "terminal_emulator"
        }
        CommandError::Stopped => "terminal_exited",
        _ => command_error_code(&error),
    };
    DaemonError::new(code, error.to_string())
}

fn terminal_output(terminal_id: TerminalId, capture: OutputCapture) -> TerminalOutput {
    TerminalOutput {
        version: 1,
        terminal_id,
        revision: capture.revision,
        source: capture.source,
        requested_rows: capture.requested_rows,
        returned_rows: capture.returned_rows,
        truncated: capture.truncated,
        starts_mid_logical_line: capture.starts_mid_logical_line,
        ansi: capture.ansi,
        text: capture.text,
    }
}

async fn read_terminal_output(
    shared: &Shared,
    terminal_id: TerminalId,
    source: TerminalOutputSource,
    rows: usize,
    ansi: bool,
) -> Result<TerminalOutput, DaemonError> {
    let terminal = output_terminal(shared, terminal_id).await?;
    terminal
        .read_output(source, rows, ansi)
        .await
        .map(|capture| terminal_output(terminal_id, capture))
        .map_err(output_error)
}

fn compile_output_matcher(matcher: TerminalOutputMatcher) -> Result<OutputMatcher, DaemonError> {
    let value = matcher.value();
    if value.is_empty() {
        return Err(DaemonError::new(
            "invalid_pattern",
            "output match pattern must not be empty",
        ));
    }
    if value.len() > MAX_TERMINAL_OUTPUT_PATTERN_BYTES {
        return Err(DaemonError::new(
            "invalid_pattern",
            format!(
                "output match pattern is {} bytes; maximum is {MAX_TERMINAL_OUTPUT_PATTERN_BYTES}",
                value.len()
            ),
        ));
    }
    match matcher {
        TerminalOutputMatcher::Literal(value) => Ok(OutputMatcher::Literal(value)),
        TerminalOutputMatcher::Regex(value) => Regex::new(&value)
            .map(OutputMatcher::Regex)
            .map_err(|error| DaemonError::new("invalid_regex", error.to_string())),
    }
}

async fn wait_terminal_output(
    shared: &Shared,
    terminal_id: TerminalId,
    source: TerminalOutputSource,
    rows: usize,
    matcher: TerminalOutputMatcher,
    timeout_ms: u64,
) -> Result<(TerminalOutput, usize, usize, String), DaemonError> {
    if timeout_ms == 0 || timeout_ms > 3_600_000 {
        return Err(DaemonError::new(
            "invalid_timeout",
            "output wait timeout must be between 1ms and 1h",
        ));
    }
    if rows == 0 || rows > MAX_TERMINAL_OUTPUT_ROWS {
        return Err(DaemonError::new(
            "invalid_output_rows",
            format!("terminal output rows must be between 1 and {MAX_TERMINAL_OUTPUT_ROWS}"),
        ));
    }
    let matcher = compile_output_matcher(matcher)?;
    let terminal = output_terminal(shared, terminal_id).await?;
    // Subscribe before the initial read. A snapshot or exit racing that read
    // remains pending on these receivers and is checked on the next loop.
    let mut snapshots = terminal.subscribe_snapshots();
    let mut lifecycle = terminal.subscribe_lifecycle();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let output = tokio::time::timeout_at(deadline, terminal.read_output(source, rows, false))
            .await
            .map_err(|_| {
                DaemonError::new(
                    "output_timeout",
                    format!("terminal output did not match within {timeout_ms}ms"),
                )
            })?
            .map(|capture| terminal_output(terminal_id, capture))
            .map_err(output_error)?;
        if let Some((start, end)) = matcher.find(&output.text) {
            let matched = output.text[start..end].to_owned();
            return Ok((output, start, end, matched));
        }
        if let TerminalLifecycle::Exited { exit_code } = lifecycle.borrow().clone() {
            return Err(DaemonError::new(
                "terminal_exited",
                format!("terminal exited with status {exit_code:?} before output matched"),
            ));
        }

        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(DaemonError::new(
                    "output_timeout",
                    format!("terminal output did not match within {timeout_ms}ms"),
                ));
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return Err(DaemonError::new("terminal_exited", "terminal output stream closed"));
                }
            }
            changed = lifecycle.changed() => {
                if changed.is_err() {
                    return Err(DaemonError::new("terminal_exited", "terminal lifecycle stream closed"));
                }
                // Loop once more to inspect the final emulator state before
                // reporting terminal_exited.
            }
        }
    }
}

struct AgentCommandResult {
    barrier_revision: u64,
    working_revision: Option<u64>,
    activity: Option<AgentActivity>,
}

fn validate_agent_timeout(timeout_ms: u64) -> Result<(), DaemonError> {
    if timeout_ms == 0 || timeout_ms > 3_600_000 {
        return Err(DaemonError::new(
            "invalid_timeout",
            "agent wait timeout must be between 1ms and 1h",
        ));
    }
    Ok(())
}

fn agent_input_error(error: CommandError) -> DaemonError {
    let code = if matches!(error, CommandError::Stopped) {
        "terminal_exited"
    } else {
        command_error_code(&error)
    };
    DaemonError::new(code, error.to_string())
}

async fn prompt_agent(
    shared: &Shared,
    terminal_id: TerminalId,
    text: String,
    wait: bool,
    timeout_ms: Option<u64>,
) -> Result<AgentCommandResult, DaemonError> {
    if text.len() > MAX_AGENT_PROMPT_BYTES {
        return Err(DaemonError::new(
            "prompt_too_large",
            format!(
                "agent prompt is {} bytes; maximum is {MAX_AGENT_PROMPT_BYTES}",
                text.len()
            ),
        ));
    }
    if wait {
        validate_agent_timeout(
            timeout_ms
                .ok_or_else(|| DaemonError::new("invalid_timeout", "--wait requires a timeout"))?,
        )?;
    } else if timeout_ms.is_some() {
        return Err(DaemonError::new(
            "invalid_timeout",
            "a prompt timeout requires wait=true",
        ));
    }

    // Subscribe and capture the barrier under one lock. Reports racing the
    // terminal write are retained even if Working and Completed arrive before
    // the write acknowledgement.
    let (terminal, mut lifecycle, events, barrier_revision) = {
        let state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        state
            .resources
            .resolve_terminal_target(Some(TargetSelector::Terminal(terminal_id)))?;
        let activity = state.resources.agent_activity(terminal_id)?;
        if activity.integration.is_none() {
            return Err(DaemonError::new(
                "not_an_agent",
                format!("terminal {terminal_id} has no agent integration"),
            ));
        }
        if activity.state == crate::domain::AgentState::Working {
            return Err(DaemonError::new(
                "agent_busy",
                format!("agent {terminal_id} is currently working"),
            ));
        }
        let runtime = state
            .runtimes
            .get(&terminal_id)
            .ok_or_else(|| DaemonError::new("terminal_exited", "terminal is not running"))?;
        let lifecycle = runtime.handle.subscribe_lifecycle();
        if let TerminalLifecycle::Exited { exit_code } = lifecycle.borrow().clone() {
            return Err(DaemonError::new(
                "terminal_exited",
                format!("terminal already exited with status {exit_code:?}"),
            ));
        }
        (
            Arc::clone(&runtime.handle),
            lifecycle,
            state.agent_events.subscribe(),
            activity.revision,
        )
    };

    terminal
        .paste_and_input(text, vec![b'\r'])
        .await
        .map_err(agent_input_error)?;
    if !wait {
        return Ok(AgentCommandResult {
            barrier_revision,
            working_revision: None,
            activity: None,
        });
    }
    wait_for_agent_events(
        shared,
        terminal_id,
        barrier_revision,
        None,
        true,
        timeout_ms.expect("validated wait timeout"),
        events,
        &mut lifecycle,
    )
    .await
}

async fn wait_agent(
    shared: &Shared,
    terminal_id: TerminalId,
    timeout_ms: u64,
) -> Result<AgentCommandResult, DaemonError> {
    validate_agent_timeout(timeout_ms)?;
    let (activity, events, mut lifecycle) = {
        let state = shared.lock().await;
        state
            .resources
            .resolve_terminal_target(Some(TargetSelector::Terminal(terminal_id)))?;
        let activity = state.resources.agent_activity(terminal_id)?.clone();
        if activity.integration.is_none() {
            return Err(DaemonError::new(
                "not_an_agent",
                format!("terminal {terminal_id} has no agent integration"),
            ));
        }
        let runtime = state
            .runtimes
            .get(&terminal_id)
            .ok_or_else(|| DaemonError::new("terminal_exited", "terminal is not running"))?;
        (
            activity,
            state.agent_events.subscribe(),
            runtime.handle.subscribe_lifecycle(),
        )
    };
    if activity.state != crate::domain::AgentState::Working {
        return Ok(AgentCommandResult {
            barrier_revision: activity.revision,
            working_revision: None,
            activity: Some(activity),
        });
    }
    wait_for_agent_events(
        shared,
        terminal_id,
        activity.revision,
        Some(activity.revision),
        false,
        timeout_ms,
        events,
        &mut lifecycle,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_agent_events(
    shared: &Shared,
    terminal_id: TerminalId,
    barrier_revision: u64,
    mut working_revision: Option<u64>,
    require_working: bool,
    timeout_ms: u64,
    mut events: broadcast::Receiver<AgentLifecycleUpdate>,
    lifecycle: &mut watch::Receiver<TerminalLifecycle>,
) -> Result<AgentCommandResult, DaemonError> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        tokio::select! {
            biased;
            update = events.recv() => match update {
                Ok(update) if update.terminal_id != terminal_id => {}
                Ok(update) if update.activity.revision <= barrier_revision => {}
                Ok(update) => {
                    let Some(event) = update.activity.last_event.as_ref() else { continue };
                    if event.kind == AgentReport::Working {
                        working_revision = Some(event.revision);
                        continue;
                    }
                    let settled = matches!(event.kind, AgentReport::Completed | AgentReport::Blocked | AgentReport::Idle);
                    let after_working = working_revision.is_some_and(|revision| event.revision > revision);
                    if settled && (!require_working || after_working) {
                        return Ok(AgentCommandResult {
                            barrier_revision,
                            working_revision,
                            activity: Some(update.activity),
                        });
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    return Err(DaemonError::new(
                        "agent_events_lagged",
                        format!("agent lifecycle receiver lagged by {skipped} reports; retry from a fresh state"),
                    ));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(DaemonError::new("agent_events_closed", "agent lifecycle stream closed"));
                }
            },
            changed = lifecycle.changed() => {
                if changed.is_err() {
                    return Err(DaemonError::new("terminal_exited", "terminal lifecycle stream closed"));
                }
                let terminal_lifecycle = lifecycle.borrow().clone();
                if let TerminalLifecycle::Exited { exit_code } = terminal_lifecycle {
                    // A final report may have been published immediately before
                    // exit; the biased event branch above gets first refusal.
                    let current = shared.lock().await.resources.agent_activity(terminal_id).cloned();
                    if let Ok(activity) = current
                        && let Some(event) = activity.last_event.as_ref()
                    {
                        let after_working = working_revision
                            .is_some_and(|revision| event.revision > revision);
                        if event.revision > barrier_revision
                            && matches!(
                                event.kind,
                                AgentReport::Completed | AgentReport::Blocked | AgentReport::Idle
                            )
                            && (!require_working || after_working)
                        {
                            return Ok(AgentCommandResult {
                                barrier_revision,
                                working_revision,
                                activity: Some(activity),
                            });
                        }
                    }
                    return Err(DaemonError::new(
                        "terminal_exited",
                        format!("terminal exited with status {exit_code:?} before the agent settled"),
                    ));
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Err(DaemonError::new(
                    "agent_timeout",
                    format!("agent did not settle within {timeout_ms}ms"),
                ));
            }
        }
    }
}

async fn targeted_terminal_input(
    shared: &Shared,
    terminal_id: TerminalId,
    operation: TerminalInputOperation,
) -> Result<(), DaemonError> {
    let terminal = {
        let state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        if let Err(error) = state
            .resources
            .resolve_terminal_target(Some(TargetSelector::Terminal(terminal_id)))
        {
            if let Some((_, exit_code)) = state
                .exited_terminals
                .iter()
                .find(|(exited, _)| *exited == terminal_id)
            {
                return Err(DaemonError::new(
                    "terminal_exited",
                    format!("terminal already exited with status {exit_code:?}"),
                ));
            }
            return Err(error.into());
        }
        let runtime = state
            .runtimes
            .get(&terminal_id)
            .ok_or_else(|| DaemonError::new("terminal_exited", "terminal is not running"))?;
        if let TerminalLifecycle::Exited { exit_code } =
            runtime.handle.subscribe_lifecycle().borrow().clone()
        {
            return Err(DaemonError::new(
                "terminal_exited",
                format!("terminal already exited with status {exit_code:?}"),
            ));
        }
        Arc::clone(&runtime.handle)
    };

    let result = match operation {
        TerminalInputOperation::Text { text } => terminal.paste(text).await,
        TerminalInputOperation::Keys { bytes } => terminal.input(bytes).await,
        TerminalInputOperation::Run { text } => terminal.paste_and_input(text, vec![b'\r']).await,
    };
    result.map_err(|error| {
        let code = if matches!(error, CommandError::Stopped) {
            "terminal_exited"
        } else {
            command_error_code(&error)
        };
        DaemonError::new(code, error.to_string())
    })
}

async fn focused_input_response(
    connection: &mut ClientConnection,
    attachment: &mut Attachment,
    request_id: Option<uuid::Uuid>,
    command: AcknowledgedCommand,
    input: FocusedInput,
) -> Result<()> {
    let viewport_before_input = attachment.focused_viewport_state();
    let reset_result = attachment.return_focused_to_bottom().await;
    let input_result = match input {
        FocusedInput::Bytes(bytes) => attachment.focused_terminal().input(bytes).await,
        FocusedInput::Paste(text) => attachment.focused_terminal().paste(text).await,
    };
    let input_succeeded = input_result.is_ok();
    attachment.finish_focused_input(viewport_before_input, input_succeeded);

    match (input_succeeded, reset_result) {
        (true, Ok(Some(screen))) => {
            send_snapshot(
                connection,
                attachment.focused.selected.terminal_id,
                screen,
                false,
            )
            .await?;
        }
        (true, Ok(None)) | (false, Ok(_)) => {}
        (_, Err(error)) => {
            tracing::warn!(
                %error,
                operation = "reset historical viewport before terminal input",
                "viewport reset failed during terminal input"
            );
        }
    }
    command_response(
        connection,
        request_id,
        command,
        input_result,
        UiEventPolicy::Input,
    )
    .await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiEventErrorDisposition {
    DropTransient,
    Diagnose,
    Reply,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiEventPolicy {
    Input,
    Disposable,
}

fn ui_event_error_disposition(
    policy: UiEventPolicy,
    request_id: Option<uuid::Uuid>,
    error: &CommandError,
) -> UiEventErrorDisposition {
    if request_id.is_some() {
        return UiEventErrorDisposition::Reply;
    }
    match (policy, error) {
        (UiEventPolicy::Input, CommandError::Stopped)
        | (UiEventPolicy::Disposable, CommandError::Busy | CommandError::Stopped) => {
            UiEventErrorDisposition::DropTransient
        }
        (UiEventPolicy::Input, CommandError::Busy | CommandError::Emulator(_)) => {
            UiEventErrorDisposition::Reply
        }
        (_, CommandError::CopyMode(_)) => UiEventErrorDisposition::Reply,
        (_, CommandError::Output(_)) => UiEventErrorDisposition::Reply,
        (_, CommandError::CloseTimeout) => UiEventErrorDisposition::Reply,
        (UiEventPolicy::Disposable, CommandError::Emulator(_)) => UiEventErrorDisposition::Diagnose,
    }
}

fn should_report_unfocused_resize(request_id: Option<uuid::Uuid>) -> bool {
    request_id.is_some()
}

async fn respond_to_ui_event_error(
    connection: &mut ClientConnection,
    request_id: Option<uuid::Uuid>,
    operation: &'static str,
    error: CommandError,
    policy: UiEventPolicy,
) -> Result<()> {
    match ui_event_error_disposition(policy, request_id, &error) {
        UiEventErrorDisposition::DropTransient => {}
        UiEventErrorDisposition::Diagnose => {
            tracing::warn!(%error, operation, "uncorrelated UI event failed");
        }
        UiEventErrorDisposition::Reply => {
            send_command_error(connection, request_id, error).await?;
        }
    }
    Ok(())
}

async fn reject_fire_and_forget_request_id(
    connection: &mut ClientConnection,
    request_id: Option<uuid::Uuid>,
    operation: &'static str,
) -> Result<bool> {
    let Some(request_id) = request_id else {
        return Ok(false);
    };
    send_error(
        connection,
        Some(request_id),
        "request_id_not_allowed",
        &format!("{operation} is fire-and-forget and must not include a request ID"),
    )
    .await?;
    Ok(true)
}

fn fire_and_forget_operation(message: &ClientMessage) -> Option<&'static str> {
    match message {
        ClientMessage::MouseInput { .. } => Some("mouse input"),
        ClientMessage::ResetViewport { .. } => Some("reset viewport"),
        ClientMessage::RefreshTerminal { .. } => Some("refresh terminal"),
        ClientMessage::ResizeSplit { .. } => Some("split resize"),
        _ => None,
    }
}

async fn send_command_error(
    connection: &mut ClientConnection,
    request_id: Option<uuid::Uuid>,
    error: CommandError,
) -> Result<()> {
    send_error(
        connection,
        request_id,
        command_error_code(&error),
        &error.to_string(),
    )
    .await
}

fn command_error_code(error: &CommandError) -> &'static str {
    match error {
        CommandError::Busy => "busy",
        CommandError::Stopped => "terminal_stopped",
        CommandError::CloseTimeout => "close_timeout",
        CommandError::Emulator(_) => "terminal_emulator",
        CommandError::CopyMode(_) => "copy_mode",
        CommandError::Output(_) => "terminal_output",
    }
}

async fn send_error(
    connection: &mut ClientConnection,
    request_id: Option<uuid::Uuid>,
    code: &str,
    message: &str,
) -> Result<()> {
    send(
        connection,
        request_id,
        ServerMessage::Error {
            code: code.into(),
            message: message.into(),
        },
    )
    .await
}

async fn send(
    connection: &mut ClientConnection,
    request_id: Option<uuid::Uuid>,
    message: ServerMessage,
) -> Result<()> {
    connection.enqueue(Outbound::Frame(Bytes::from(encode_payload(&Envelope {
        request_id,
        message,
    })?)))
}

/// Publish a screen snapshot. Unlike [`send`], pending snapshots for the
/// same terminal are replaced rather than queued, so a burst of updates to a
/// slow client delivers only the newest screen. `force_full` requests a full
/// snapshot on the wire rather than a delta against the last one actually
/// sent; see [`Outbound::Snapshot`].
async fn send_snapshot(
    connection: &mut ClientConnection,
    terminal_id: TerminalId,
    screen: ScreenSnapshot,
    force_full: bool,
) -> Result<()> {
    connection.enqueue(Outbound::Snapshot {
        terminal_id,
        screen,
        force_full,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn process_names_are_read_without_control_characters() {
        let name = process_name(std::process::id())
            .await
            .expect("current process has a name");
        assert!(!name.is_empty());
        assert!(!name.chars().any(char::is_control));
    }

    #[test]
    fn snapshot_message_encodes_a_small_row_diff_as_a_delta() {
        use crate::domain::{Cell, Cursor, CursorShape, TerminalSize};
        let size = TerminalSize {
            columns: 10,
            rows: 5,
        };
        let cells = vec![Cell::default(); 50];
        let cursor = Cursor {
            column: 0,
            row: 0,
            visible: true,
            shape: Default::default(),
            blinking: false,
        };
        let terminal_id = TerminalId::new();
        let first = ScreenSnapshot::new(1, size, cells.clone(), cursor).unwrap();
        let mut last_sent = HashMap::new();
        let message = snapshot_message(terminal_id, first, false, &mut last_sent);
        assert!(matches!(message, ServerMessage::Snapshot { .. }));

        let mut second_cells = cells;
        second_cells[0].contents = "x".into();
        let second = ScreenSnapshot::new(2, size, second_cells.clone(), cursor).unwrap();
        let message = snapshot_message(terminal_id, second, false, &mut last_sent);
        assert!(matches!(message, ServerMessage::SnapshotDelta { .. }));

        let mut third = ScreenSnapshot::new(
            3,
            size,
            second_cells,
            Cursor {
                shape: CursorShape::Bar,
                blinking: true,
                ..cursor
            },
        )
        .unwrap();
        third.mouse_tracking = true;
        third
            .graphics
            .placements
            .push(crate::domain::KittyPlacement {
                image_id: 1,
                placement_id: 1,
                column: 0,
                row: 0,
                columns: 1,
                rows: 1,
                source_x: 0,
                source_y: 0,
                source_width: 1,
                source_height: 1,
                z: 0,
            });
        let message = snapshot_message(terminal_id, third, false, &mut last_sent);
        let ServerMessage::SnapshotDelta { delta, .. } = message else {
            panic!("cursor-only change should produce a delta")
        };
        assert!(delta.rows.is_empty());
        assert_eq!(delta.cursor.shape, CursorShape::Bar);
        assert!(delta.cursor.blinking);
        assert!(delta.mouse_tracking);
        assert_eq!(delta.graphics.unwrap().placements.len(), 1);
    }

    #[tokio::test]
    async fn inherited_cwd_falls_back_through_spawn_directory_to_workspace_root() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let spawn = root.join("spawn");
        tokio::fs::create_dir(&spawn).await.unwrap();

        assert_eq!(
            resolve_creation_cwd(root, None, Some((u32::MAX, spawn.clone())))
                .await
                .unwrap(),
            spawn.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_creation_cwd(root, None, Some((u32::MAX, root.join("missing"))))
                .await
                .unwrap(),
            root.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_creation_cwd(root, Some(PathBuf::from("missing")), None)
                .await
                .unwrap_err()
                .code,
            "invalid_cwd"
        );
    }

    #[tokio::test]
    async fn process_cwd_resolves_a_live_process_and_rejects_a_dead_pid() {
        assert_eq!(
            process_cwd(std::process::id()).await,
            Some(std::env::current_dir().unwrap())
        );
        assert_eq!(process_cwd(u32::MAX).await, None);
    }

    #[test]
    fn scoped_selection_rejects_a_pane_that_moved_before_acquisition() {
        let path = crate::resources::ResolvedTerminalPath {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        assert!(
            validate_selection_expectation(path, Some(&SelectionExpectation::Tab(path.tab_id)))
                .is_ok()
        );
        assert_eq!(
            validate_selection_expectation(path, Some(&SelectionExpectation::Tab(TabId::new())))
                .unwrap_err()
                .code,
            "target_moved"
        );
    }

    #[test]
    fn uncorrelated_input_discards_only_stopped() {
        assert_eq!(
            ui_event_error_disposition(UiEventPolicy::Input, None, &CommandError::Stopped),
            UiEventErrorDisposition::DropTransient
        );
        assert_eq!(
            ui_event_error_disposition(UiEventPolicy::Input, None, &CommandError::Busy),
            UiEventErrorDisposition::Reply
        );
        assert_eq!(
            ui_event_error_disposition(
                UiEventPolicy::Input,
                None,
                &CommandError::Emulator("broken".into()),
            ),
            UiEventErrorDisposition::Reply
        );
    }

    #[test]
    fn uncorrelated_disposable_ui_events_drop_expected_pressure_and_exit_errors() {
        assert_eq!(
            ui_event_error_disposition(UiEventPolicy::Disposable, None, &CommandError::Stopped),
            UiEventErrorDisposition::DropTransient
        );
        assert_eq!(
            ui_event_error_disposition(UiEventPolicy::Disposable, None, &CommandError::Busy),
            UiEventErrorDisposition::DropTransient
        );
        assert_eq!(
            ui_event_error_disposition(
                UiEventPolicy::Disposable,
                None,
                &CommandError::Emulator("broken".into()),
            ),
            UiEventErrorDisposition::Diagnose
        );
    }

    #[test]
    fn correlated_ui_errors_reply_for_every_command_error_variant() {
        let request_id = Some(uuid::Uuid::new_v4());
        for (error, code) in [
            (CommandError::Stopped, "terminal_stopped"),
            (CommandError::Busy, "busy"),
            (CommandError::Emulator("broken".into()), "terminal_emulator"),
        ] {
            assert_eq!(
                ui_event_error_disposition(UiEventPolicy::Disposable, request_id, &error),
                UiEventErrorDisposition::Reply
            );
            assert_eq!(command_error_code(&error), code);
        }
        assert!(!should_report_unfocused_resize(None));
        assert!(should_report_unfocused_resize(Some(uuid::Uuid::new_v4())));
    }

    #[test]
    fn raw_mouse_validation_rejects_incoherent_transition_and_drag_snapshots() {
        use crate::domain::{MouseButton, MouseButtons, MouseEvent, MouseEventKind};

        for event in [
            MouseEvent {
                kind: MouseEventKind::Press {
                    button: MouseButton::Left,
                },
                column: 0,
                row: 0,
                modifiers: Default::default(),
                buttons: MouseButtons::default(),
            },
            MouseEvent {
                kind: MouseEventKind::Release {
                    button: MouseButton::Middle,
                },
                column: 0,
                row: 0,
                modifiers: Default::default(),
                buttons: MouseButtons {
                    middle: true,
                    ..Default::default()
                },
            },
            MouseEvent {
                kind: MouseEventKind::Motion {
                    button: Some(MouseButton::Right),
                },
                column: 0,
                row: 0,
                modifiers: Default::default(),
                buttons: MouseButtons::default(),
            },
        ] {
            assert!(event.validate().is_err(), "accepted {event:?}");
        }

        let valid = MouseEvent {
            kind: MouseEventKind::Press {
                button: MouseButton::Right,
            },
            column: 0,
            row: 0,
            modifiers: Default::default(),
            buttons: MouseButtons {
                right: true,
                ..Default::default()
            },
        };
        assert!(valid.validate().is_ok());
    }

    #[tokio::test]
    async fn failed_viewport_reset_preserves_the_offset() {
        let (mut attachment, terminal) = test_attachment("sleep 60");
        let terminal_id = terminal.id();
        attachment.viewport_offsets.insert(terminal_id, 7);
        terminal.close().await.unwrap();

        assert!(matches!(
            attachment.return_focused_to_bottom().await,
            Err(CommandError::Stopped)
        ));
        assert_eq!(attachment.viewport_offsets.get(&terminal_id), Some(&7));
    }

    #[tokio::test]
    async fn successful_input_leaves_failed_viewport_reset_logically_at_bottom() {
        let (mut attachment, terminal) = test_attachment("sleep 60");
        let terminal_id = terminal.id();
        attachment.viewport_offsets.insert(terminal_id, 7);
        let before = attachment.focused_viewport_state();

        attachment.finish_focused_input(before, true);

        assert!(!attachment.viewport_offsets.contains_key(&terminal_id));
        terminal.close().await.unwrap();
    }

    #[tokio::test]
    async fn failed_input_restores_viewport_transaction_state() {
        let (mut attachment, terminal) = test_attachment("sleep 60");
        let terminal_id = terminal.id();
        attachment.viewport_offsets.insert(terminal_id, 7);
        attachment.snapshot_revisions.insert(terminal_id, 11);
        let before = attachment.focused_viewport_state();
        attachment.viewport_offsets.remove(&terminal_id);
        attachment.snapshot_revisions.insert(terminal_id, 12);

        attachment.finish_focused_input(before, false);

        assert_eq!(attachment.viewport_offsets.get(&terminal_id), Some(&7));
        assert_eq!(attachment.snapshot_revisions.get(&terminal_id), Some(&11));
        terminal.close().await.unwrap();
    }

    #[tokio::test]
    async fn mouse_viewport_commit_is_transactional() {
        let (mut attachment, terminal) = test_attachment("sleep 60");
        let terminal_id = terminal.id();
        let mut screen = terminal.subscribe_snapshots().borrow().clone();
        attachment.viewport_offsets.insert(terminal_id, 7);
        attachment
            .snapshot_revisions
            .insert(terminal_id, screen.revision);

        assert!(
            attachment
                .accept_mouse_viewport(
                    terminal_id,
                    crate::terminal::ViewportSnapshot {
                        offset: None,
                        screen: screen.clone(),
                    },
                )
                .is_none()
        );
        assert_eq!(attachment.viewport_offsets.get(&terminal_id), Some(&7));

        screen.revision += 1;
        assert_eq!(
            attachment
                .accept_mouse_viewport(
                    terminal_id,
                    crate::terminal::ViewportSnapshot {
                        offset: None,
                        screen: screen.clone(),
                    },
                )
                .unwrap(),
            screen
        );
        assert!(!attachment.viewport_offsets.contains_key(&terminal_id));
        terminal.close().await.unwrap();
    }

    #[tokio::test]
    async fn copy_mode_ignores_server_side_mouse_input_and_viewport_reset() {
        let (mut attachment, terminal) = test_attachment("printf content; sleep 60");
        let terminal_id = terminal.id();
        attachment
            .copy_mode(crate::domain::CopyModeAction::Begin)
            .await
            .unwrap();
        attachment.viewport_offsets.insert(terminal_id, 7);
        let revisions = attachment.snapshot_revisions.clone();

        assert!(
            attachment
                .mouse_input(
                    terminal_id,
                    crate::domain::MouseEvent {
                        kind: crate::domain::MouseEventKind::Wheel {
                            direction: crate::domain::MouseWheelDirection::Up,
                        },
                        column: 0,
                        row: 0,
                        modifiers: crate::domain::MouseModifiers::default(),
                        buttons: Default::default(),
                    },
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            attachment
                .return_focused_to_bottom()
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(attachment.viewport_offsets.get(&terminal_id), Some(&7));
        assert_eq!(attachment.snapshot_revisions, revisions);

        attachment.clear_active_copy_mode().await.unwrap();
        terminal.close().await.unwrap();
    }

    #[tokio::test]
    async fn removing_a_pane_clears_its_viewport_state() {
        let (mut attachment, focused) = test_attachment("sleep 60");
        let removed = Arc::new(spawn_test_terminal("sleep 60"));
        let terminal_id = removed.id();
        let pane_id = PaneId::new();
        let mut selected = attachment.focused.selected.clone();
        selected.pane_id = pane_id;
        selected.terminal_id = terminal_id;
        selected.child_pid = removed.child_pid();
        assert!(attachment.layout.split(
            attachment.focused.selected.pane_id,
            SplitDirection::Right,
            pane_id,
        ));
        attachment.panes.push(ObservedTarget {
            selected,
            terminal: Arc::clone(&removed),
        });
        attachment.reconcile_watchers();
        attachment.viewport_offsets.insert(terminal_id, 7);
        attachment.snapshot_revisions.insert(terminal_id, 11);

        assert!(attachment.remove(terminal_id));
        assert!(!attachment.viewport_offsets.contains_key(&terminal_id));
        assert!(!attachment.snapshot_revisions.contains_key(&terminal_id));

        drop(attachment);
        focused.close().await.unwrap();
        removed.close().await.unwrap();
    }

    #[tokio::test]
    async fn acknowledged_attachment_cleanup_clears_its_runtime_copy_owner() {
        let (mut attachment, terminal) = test_attachment("printf copy; sleep 60");
        let owner = attachment.owner;
        attachment
            .copy_mode(crate::domain::CopyModeAction::Begin)
            .await
            .unwrap();
        attachment.close().await.unwrap();

        assert!(matches!(
            terminal
                .copy_mode(
                    owner,
                    crate::domain::CopyModeAction::Move {
                        movement: crate::domain::CopyModeMovement::Left,
                    },
                    None,
                )
                .await,
            Err(CommandError::CopyMode(
                crate::domain::CopyModeError::NotActive
            ))
        ));
        terminal.close().await.unwrap();
    }

    #[tokio::test]
    async fn drop_cleanup_closes_a_cancelled_begin_after_runtime_acceptance() {
        let (mut attachment, terminal) = test_attachment("printf copy; sleep 60");
        let owner = attachment.owner;
        // This is the cancellation boundary inside Attachment::copy_mode:
        // ownership is recorded, the runtime accepts Begin, and the awaiting
        // attachment future is then dropped before observing the response.
        attachment.record_copy_mode_begin().unwrap();
        assert!(matches!(
            terminal
                .copy_mode(owner, crate::domain::CopyModeAction::Begin, None)
                .await
                .unwrap(),
            CopyModeOutcome::Active(_)
        ));
        assert_eq!(
            attachment
                .copy_mode_terminal
                .as_ref()
                .map(|terminal| terminal.id()),
            Some(terminal.id())
        );
        drop(attachment);

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match terminal
                    .copy_mode(owner, crate::domain::CopyModeAction::Begin, None)
                    .await
                {
                    Ok(CopyModeOutcome::Active(_)) => break,
                    Err(CommandError::CopyMode(crate::domain::CopyModeError::AlreadyActive)) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    _ => panic!("unexpected copy-mode cleanup result"),
                }
            }
        })
        .await
        .expect("drop cleanup did not release the recorded copy owner");
        terminal.clear_client(owner).await.unwrap();
        terminal.close().await.unwrap();
    }

    #[tokio::test]
    async fn begin_dispatch_failure_clears_the_write_ahead_owner() {
        let (mut attachment, terminal) = test_attachment("printf copy; sleep 60");
        terminal.close().await.unwrap();

        assert!(matches!(
            attachment
                .copy_mode(crate::domain::CopyModeAction::Begin)
                .await,
            Err(CommandError::Stopped)
        ));
        assert!(attachment.copy_mode_terminal.is_none());
    }

    fn test_attachment(script: &str) -> (Attachment, Arc<TerminalHandle>) {
        let terminal = Arc::new(spawn_test_terminal(script));
        let pane_id = PaneId::new();
        let selected = SelectedTarget {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            tab_id: TabId::new(),
            pane_id,
            terminal_id: terminal.id(),
            child_pid: terminal.child_pid(),
        };
        let lease = AttachmentLease::default();
        let owner = ClientId::new();
        let size = TerminalSize {
            columns: 80,
            rows: 24,
        };
        let guard = lease.acquire(owner, size, Arc::clone(&terminal)).unwrap();
        let (_, resource_changes) = watch::channel(0);
        let attachment = Attachment::new(
            owner,
            vec![ObservedTarget {
                selected: selected.clone(),
                terminal: Arc::clone(&terminal),
            }],
            LeasedTarget {
                selected,
                terminal: Arc::clone(&terminal),
                lease: guard,
            },
            SplitTree::leaf(pane_id),
            Vec::new(),
            0,
            resource_changes,
        );
        (attachment, terminal)
    }

    fn spawn_test_terminal(script: &str) -> TerminalHandle {
        spawn_terminal(SpawnSpec {
            id: TerminalId::new(),
            program: "/bin/sh".into(),
            argv: vec!["-c".into(), script.into()],
            cwd: "/".into(),
            env: HashMap::new(),
            size: TerminalSize {
                columns: 80,
                rows: 24,
            },
        })
        .unwrap()
    }

    #[test]
    fn interactive_rename_is_limited_to_the_visible_resource_scope() {
        let (mut state, path) = inconsistent_state();
        let peer = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name: "peer".into(),
            root: "/peer".into(),
            tab_id: TabId::new(),
            tab_name: "shell".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let peer_workspace = peer.workspace_id;
        let peer_tab = peer.tab_id;
        state
            .resources
            .add_workspace(path.session_id, peer)
            .unwrap();

        assert!(interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Workspace(peer_workspace),
        ));
        assert!(interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Tab(path.tab_id),
        ));
        assert!(!interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Tab(peer_tab),
        ));
        assert!(!interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Session(crate::resources::SessionSelector::Id(path.session_id,)),
        ));
    }

    #[test]
    fn interactive_close_is_limited_to_the_visible_resource_scope() {
        let (mut state, path) = inconsistent_state();
        let sibling_tab = TabId::new();
        state
            .resources
            .add_tab(
                path.workspace_id,
                TabPath {
                    tab_id: sibling_tab,
                    tab_name: "sibling".into(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                },
            )
            .unwrap();
        let peer = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name: "peer".into(),
            root: "/peer".into(),
            tab_id: TabId::new(),
            tab_name: "shell".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let peer_workspace = peer.workspace_id;
        let peer_tab = peer.tab_id;
        state
            .resources
            .add_workspace(path.session_id, peer)
            .unwrap();
        let focused = SelectedTarget {
            session_id: path.session_id,
            workspace_id: path.workspace_id,
            tab_id: path.tab_id,
            pane_id: path.pane_id,
            terminal_id: path.terminal_id,
            child_pid: 1,
        };

        assert!(interactive_close_allowed(
            &state.resources,
            &focused,
            &TargetSelector::Pane(path.pane_id),
        ));
        assert!(interactive_close_allowed(
            &state.resources,
            &focused,
            &TargetSelector::Tab(sibling_tab),
        ));
        assert!(interactive_close_allowed(
            &state.resources,
            &focused,
            &TargetSelector::Workspace(peer_workspace),
        ));
        assert!(!interactive_close_allowed(
            &state.resources,
            &focused,
            &TargetSelector::Tab(peer_tab),
        ));
        assert!(!interactive_close_allowed(
            &state.resources,
            &focused,
            &TargetSelector::Pane(PaneId::new()),
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
                exited_terminals: VecDeque::new(),
                resource_changes: watch::channel(1).0,
                agent_events: broadcast::channel(AGENT_EVENT_CAPACITY).0,
                child_env: HashMap::new(),
                extensions: Vec::new(),
                hook_queue: crate::extensions::hook_queue().0,
                accepting: true,
            },
            path,
        )
    }

    #[tokio::test]
    async fn contextual_guards_reject_moved_and_closing_calling_terminals() {
        let (mut state, path) = inconsistent_state();
        let terminal = Arc::new(spawn_test_terminal("while :; do sleep 1; done"));
        state.runtimes.insert(
            path.terminal_id,
            RuntimeEntry {
                handle: Arc::clone(&terminal),
                lease: AttachmentLease::default(),
            },
        );
        let context = TerminalContext {
            session_id: path.session_id,
            workspace_id: path.workspace_id,
            tab_id: path.tab_id,
            pane_id: path.pane_id,
            terminal_id: path.terminal_id,
        };
        assert!(state.validate_terminal_context(context).is_ok());

        let destination = TabId::new();
        state
            .resources
            .add_tab(
                path.workspace_id,
                TabPath {
                    tab_id: destination,
                    tab_name: "destination".into(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                },
            )
            .unwrap();
        state
            .resources
            .move_pane(path.pane_id, destination)
            .unwrap();
        let moved = state.validate_terminal_context(context).unwrap_err();
        assert_eq!(moved.code, "context_changed");

        let fresh = TerminalContext {
            tab_id: destination,
            ..context
        };
        assert!(state.validate_terminal_context(fresh).is_ok());
        state.resources.close_pane(path.pane_id).unwrap();
        let closing = state.validate_terminal_context(fresh).unwrap_err();
        assert_eq!(closing.code, "target_closing");
        terminal.close().await.unwrap();
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

        state
            .resources
            .split_pane(
                path.pane_id,
                SplitDirection::Right,
                PaneId::new(),
                TerminalId::new(),
            )
            .unwrap();
        let split_id = match state.resources.open_layout_for_tab(path.tab_id).unwrap() {
            SplitTree::Branch { split_id, .. } => split_id,
            _ => panic!("expected split branch"),
        };
        let ratio = SplitRatio::from_cells(37, 79).unwrap();
        state.resize_split(path.tab_id, split_id, ratio).unwrap();
        assert!(changes.has_changed().unwrap());
        assert_eq!(*changes.borrow_and_update(), state.resources.revision());

        state.resize_split(path.tab_id, split_id, ratio).unwrap();
        assert!(!changes.has_changed().unwrap());
    }

    #[test]
    fn stale_split_resize_after_focused_pane_move_cannot_mutate_the_old_tab() {
        let (mut state, path) = inconsistent_state();
        let pane_b = PaneId::new();
        state
            .resources
            .split_pane(
                path.pane_id,
                SplitDirection::Right,
                pane_b,
                TerminalId::new(),
            )
            .unwrap();
        state
            .resources
            .split_pane(
                pane_b,
                SplitDirection::Down,
                PaneId::new(),
                TerminalId::new(),
            )
            .unwrap();
        let retained_split = match state.resources.open_layout_for_tab(path.tab_id).unwrap() {
            SplitTree::Branch { second, .. } => match *second {
                SplitTree::Branch { split_id, .. } => split_id,
                other => panic!("expected nested retained split, got {other:?}"),
            },
            other => panic!("expected source split tree, got {other:?}"),
        };
        let destination = TabId::new();
        state
            .resources
            .add_tab(
                path.workspace_id,
                TabPath {
                    tab_id: destination,
                    tab_name: "destination".into(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                },
            )
            .unwrap();
        state
            .resources
            .move_pane(path.pane_id, destination)
            .unwrap();
        let revision = state.resources.revision();
        let old_layout = state.resources.open_layout_for_tab(path.tab_id).unwrap();

        let error = state
            .resize_split_for_attachment(
                path.terminal_id,
                path.tab_id,
                retained_split,
                SplitRatio::from_cells(1, 3).unwrap(),
            )
            .unwrap_err();

        assert_eq!(error.code, "context_changed");
        assert_eq!(state.resources.revision(), revision);
        assert_eq!(
            state.resources.open_layout_for_tab(path.tab_id).unwrap(),
            old_layout
        );
    }

    #[test]
    fn proactive_last_terminal_replacement_can_keep_accepting() {
        let (mut state, path) = inconsistent_state();
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                id: TerminalId::new(),
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
                id: TerminalId::new(),
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
                    id: TerminalId::new(),
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
            workspace_kind: crate::project::WorkspaceKind::Directory,
        };
        let path = initial_path(&resolved, "test".into(), focused.id());
        let sibling_pane_id = PaneId::new();
        let (resource_changes, _) = watch::channel(0);
        let mut state = SharedState {
            resources: ResourceTree::default(),
            runtimes: HashMap::new(),
            expected_finalizations: HashSet::new(),
            exited_terminals: VecDeque::new(),
            resource_changes,
            agent_events: broadcast::channel(AGENT_EVENT_CAPACITY).0,
            child_env: HashMap::new(),
            extensions: Vec::new(),
            hook_queue: crate::extensions::hook_queue().0,
            accepting: true,
        };
        state
            .register_session(path.clone(), Arc::clone(&focused))
            .unwrap();
        let shared = Arc::new(Mutex::new(state));
        let stale_attachment = lease_view(
            &shared,
            Some(TargetSelector::Pane(path.pane_id)),
            None,
            ClientId::new(),
            TerminalSize {
                columns: 80,
                rows: 24,
            },
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
        } = observe_selection(
            &shared,
            &TargetSelector::Pane(path.pane_id),
            None,
            focused.id(),
        )
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
        let TargetSelection::Focused { panes, layout, .. } = observe_selection(
            &shared,
            &TargetSelector::Pane(path.pane_id),
            None,
            focused.id(),
        )
        .await
        .unwrap() else {
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
    async fn distinct_sockets_in_one_runtime_directory_have_distinct_owners() {
        let temporary = tempfile::tempdir().unwrap();
        let first_path = temporary.path().join("runtime/fut-debug.sock");
        let second_path = temporary.path().join("runtime/fut-release.sock");

        let first = bind_socket(&first_path).await.unwrap();
        let second = bind_socket(&second_path).await.unwrap();

        assert!(first_path.exists());
        assert!(second_path.exists());
        drop((first, second));
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
