//! The single per-user Fut daemon and its local socket lifecycle.

mod command;
mod git;
mod lease;
pub mod path;
mod presence;
pub(crate) mod process;
mod recipe;

pub mod autostart;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
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
    signal::unix::{Signal, SignalKind, signal},
    sync::{Mutex, Notify, broadcast, mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::{Duration, timeout},
};
use tokio_util::codec::Framed;
use uuid::Uuid;

use crate::{
    agent_detection::{CodexProbe, CodexStabilizer, detect_codex, is_codex_process},
    alerts::AlertStore,
    client::config as global_config,
    domain::{
        BlockedKind, ClientId, CopyModeAction, CopyModeError, MAX_AGENT_PROMPT_BYTES,
        MAX_TERMINAL_OUTPUT_PATTERN_BYTES, MAX_TERMINAL_OUTPUT_ROWS, PaneId, ProgramStatus,
        ScreenDelta, ScreenSnapshot, SessionId, SplitId, StatusReport, StatusSource, StatusState,
        StatusUpdate, TabId, TerminalId, TerminalOutput, TerminalOutputMatcher,
        TerminalOutputSource, TerminalSize, WorkspaceId,
    },
    project::{ProjectError, ProjectResolver, ResolvedLocation},
    protocol::{
        AcknowledgedCommand, AgentPromptMode, ClientMessage, ClientMode, ClientPresenceSnapshot,
        ContextScope, ContextualCommand, Envelope, OpenDisposition, PROTOCOL_VERSION,
        PresentationTokenPublishAction, RenameSelector, SelectedTarget, SelectedView,
        SelectionExpectation, ServerMessage, TerminalContext, TerminalInputOperation, codec,
        decode_payload, encode_payload,
        local::{self, LocalWelcome},
        remote::{self, Capabilities, Capability, EndpointError, RemoteWelcome},
    },
    resources::{
        CheckoutDestination, InitialPath, MaterializedTokenValue, Mutation,
        PresentationTokenAction, PresentationTokenTarget, ResourceError, ResourceTree,
        SessionSelector, TabPath, TargetSelector, TokenPublication, WorkspacePath,
    },
    splits::{SplitDirection, SplitRatio, SplitTree},
    terminal::{
        CommandError, CopyModeOutcome, MouseInputOutcome, NativeStatusBatch, OutputCapture,
        OutputCaptureError, SpawnSpec, TerminalEvent, TerminalHandle, TerminalLifecycle,
        spawn_terminal,
    },
};

use lease::AttachmentLease;
use path::prepare_runtime_dir;
use presence::ClientPresence;

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub spawn: SpawnSpec,
    pub config_location: global_config::ConfigLocation,
    pub recipe_command_override: bool,
}

impl DaemonConfig {
    pub fn shell(
        socket_path: PathBuf,
        cwd: PathBuf,
        config_location: global_config::ConfigLocation,
    ) -> Self {
        let program = default_shell(None);
        let env = shell_environment(&socket_path, std::env::vars_os());
        Self {
            socket_path,
            config_location,
            recipe_command_override: false,
            spawn: SpawnSpec {
                terminal: crate::terminal::TerminalConfig::default(),
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

/// Resolve the program for new shell panes: the configured `terminal.shell`,
/// then the daemon's inherited `$SHELL`, then `/bin/sh`.
fn default_shell(configured: Option<&Path>) -> PathBuf {
    configured
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("SHELL").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn shell_environment(
    socket_path: &Path,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> HashMap<OsString, OsString> {
    let mut environment = inherited.into_iter().collect::<HashMap<_, _>>();
    environment.insert("FUT_SOCKET".into(), socket_path.as_os_str().to_owned());
    environment
}

const CONNECTION_GRACE_PERIOD: Duration = Duration::from_secs(1);
const HOOK_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(1);

struct RuntimeEntry {
    handle: Arc<TerminalHandle>,
    lease: AttachmentLease,
}

struct SharedState {
    attachment_store: Arc<crate::attachments::Store>,
    resources: ResourceTree,
    runtimes: HashMap<TerminalId, RuntimeEntry>,
    presence: ClientPresence,
    expected_finalizations: HashSet<TerminalId>,
    exited_terminals: VecDeque<(TerminalId, Option<i32>)>,
    resource_changes: watch::Sender<u64>,
    alerts: AlertStore,
    alert_changes: watch::Sender<u64>,
    agent_events: broadcast::Sender<StatusLifecycleUpdate>,
    child_env: HashMap<OsString, OsString>,
    terminal_config: crate::terminal::TerminalConfig,
    default_shell: PathBuf,
    config_location: global_config::ConfigLocation,
    extension_registry: Arc<crate::extensions::ExtensionRegistry>,
    extension_catalog: watch::Sender<crate::protocol::ExtensionCatalog>,
    hook_queue: crate::extensions::HookQueue,
    accepting: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinalizationOutcome {
    AlreadyFinalized,
    ResourcesRemain,
    MultiplexerEmpty,
}

impl FinalizationOutcome {
    fn is_empty(self) -> bool {
        self == Self::MultiplexerEmpty
    }
}

const AGENT_EVENT_CAPACITY: usize = 256;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Debug)]
struct StatusLifecycleUpdate {
    terminal_id: TerminalId,
    status: ProgramStatus,
    transitions: Vec<StatusRecordLifecycle>,
    terminal_process_exit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StatusRecordLifecycle {
    Working(String),
    Settled(String),
    Removed(String),
}

fn status_lifecycle_transitions(
    before: &ProgramStatus,
    updates: &[StatusUpdate],
    source: &StatusSource,
    after: &ProgramStatus,
) -> Vec<StatusRecordLifecycle> {
    let detection = matches!(source, StatusSource::Detection { .. });
    let mut states = before
        .records
        .iter()
        .map(|record| (record.id.clone(), record.state))
        .collect::<HashMap<_, _>>();
    let mut transitions = Vec::new();

    for update in updates {
        match update {
            StatusUpdate::Report(report) => {
                let previous = states.get(&report.id).copied();
                let next = if detection && report.state == StatusState::Idle {
                    match previous {
                        Some(StatusState::Working) => StatusState::Done,
                        Some(StatusState::Done | StatusState::Error) => continue,
                        Some(StatusState::Idle | StatusState::Blocked) | None => StatusState::Idle,
                    }
                } else {
                    report.state
                };
                if previous == Some(next) {
                    continue;
                }
                if previous == Some(StatusState::Working) {
                    transitions.push(StatusRecordLifecycle::Settled(report.id.clone()));
                }
                if next == StatusState::Working {
                    transitions.push(StatusRecordLifecycle::Working(report.id.clone()));
                }
                states.insert(report.id.clone(), next);
            }
            StatusUpdate::Clear { id } => {
                let removed = states
                    .keys()
                    .filter(|record_id| {
                        // A weak clear only retracts inferred records, so an
                        // explicit record that survived it was not removed.
                        status_clear_matches(id.as_deref(), record_id)
                            && !(detection
                                && after.records.iter().any(|record| &record.id == *record_id))
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                for record_id in removed {
                    states.remove(&record_id);
                    transitions.push(StatusRecordLifecycle::Removed(record_id));
                }
            }
            StatusUpdate::PromptStart | StatusUpdate::ProcessExit => {
                let removed = states
                    .iter()
                    .filter(|(_, state)| !matches!(state, StatusState::Done | StatusState::Error))
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                for record_id in removed {
                    states.remove(&record_id);
                    transitions.push(StatusRecordLifecycle::Removed(record_id));
                }
            }
        }
    }

    // The operation stream preserves otherwise invisible Working -> Done
    // evidence, but it cannot model private provenance or LRU eviction. Make
    // that evidence agree with the authoritative post-apply record set. This
    // also records the disappearance of a selected pre-existing record and of
    // fresh work that was created and evicted within this batch.
    let after_states = after
        .records
        .iter()
        .map(|record| (record.id.as_str(), record.state))
        .collect::<HashMap<_, _>>();
    let tracked = before
        .records
        .iter()
        .map(|record| record.id.clone())
        .chain(
            transitions
                .iter()
                .filter_map(|transition| match transition {
                    StatusRecordLifecycle::Working(id) => Some(id.clone()),
                    StatusRecordLifecycle::Settled(_) | StatusRecordLifecycle::Removed(_) => None,
                }),
        )
        .collect::<HashSet<_>>();

    for id in tracked {
        match after_states.get(id.as_str()).copied() {
            Some(actual) if states.get(&id).copied() != Some(actual) => {
                transitions.retain(|transition| match transition {
                    StatusRecordLifecycle::Working(record_id)
                    | StatusRecordLifecycle::Settled(record_id)
                    | StatusRecordLifecycle::Removed(record_id) => record_id != &id,
                });
                let previous = before
                    .records
                    .iter()
                    .find(|record| record.id == id)
                    .map(|record| record.state);
                if previous == Some(StatusState::Working) && actual != StatusState::Working {
                    transitions.push(StatusRecordLifecycle::Settled(id.clone()));
                }
                if previous != Some(StatusState::Working) && actual == StatusState::Working {
                    transitions.push(StatusRecordLifecycle::Working(id));
                }
            }
            None
                if !transitions.iter().any(|transition| {
                    matches!(transition, StatusRecordLifecycle::Removed(record_id) if record_id == &id)
                }) =>
            {
                transitions.push(StatusRecordLifecycle::Removed(id));
            }
            Some(_) | None => {}
        }
    }
    transitions
}

fn status_clear_matches(clear_id: Option<&str>, record_id: &str) -> bool {
    let Some(clear_id) = clear_id.filter(|id| !id.is_empty()) else {
        return true;
    };
    record_id == clear_id
        || record_id
            .strip_prefix(clear_id)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct DaemonError {
    code: &'static str,
    message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExtensionReload {
    catalog: crate::protocol::ExtensionCatalog,
    changed: bool,
}

#[derive(Debug)]
struct PreparedExtensionReload {
    base_generation: u64,
    candidate: crate::extensions::ExtensionRegistry,
    catalog: crate::protocol::ExtensionCatalog,
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
            ResourceError::InvalidWorkspaceParent(_) => "invalid_workspace_parent",
            ResourceError::InvalidWorkspacePosition(_) => "invalid_position",
            ResourceError::TargetRequired | ResourceError::AmbiguousTarget => "target_required",
            ResourceError::EmptyName | ResourceError::InvalidName => "invalid_name",
            ResourceError::InvalidStatusReport(_) => "invalid_agent_report",
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

    fn publish_alert_change(&self) {
        let revision = self.alerts.revision();
        if revision > *self.alert_changes.borrow() {
            self.alert_changes.send_replace(revision);
        }
    }

    fn publish_mutation(&self, mutation: &Mutation) {
        self.publish_resource_change(mutation.revision);
        self.enqueue_committed_mutation(mutation);
    }

    fn enqueue_committed_mutation(&self, mutation: &Mutation) {
        self.hook_queue
            .enqueue(mutation, Arc::clone(&self.extension_registry));
    }

    fn publish_token(
        &mut self,
        extension_id: &str,
        token_name: &str,
        value: String,
        target: PresentationTokenTarget,
        action: Option<PresentationTokenPublishAction>,
    ) -> Result<TokenPublication, DaemonError> {
        if !self.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let extension = self
            .extension_registry
            .extension(extension_id)
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
        if value.is_empty() && action.is_some() {
            return Err(DaemonError::new(
                "invalid_token_action",
                "an empty presentation token value cannot have an action",
            ));
        }
        let action = match action {
            Some(PresentationTokenPublishAction::Pane { pane_id }) => {
                if !self
                    .resources
                    .presentation_action_pane_is_live_descendant(target, pane_id)?
                {
                    return Err(DaemonError::new(
                        "invalid_token_action",
                        "presentation token action pane is not a descendant of its target",
                    ));
                }
                Some(PresentationTokenAction::Pane { pane_id })
            }
            Some(PresentationTokenPublishAction::ExtensionCommand { command }) => {
                if !extension
                    .commands()
                    .any(|launcher| launcher.name() == command)
                {
                    return Err(DaemonError::new(
                        "invalid_token_action",
                        format!("extension {extension_id:?} does not declare command {command:?}"),
                    ));
                }
                Some(PresentationTokenAction::ExtensionCommand {
                    extension_id: extension_id.to_owned(),
                    command,
                })
            }
            None => None,
        };
        let publication = self.resources.publish_presentation_token_value(
            target,
            declaration.qualified_name().to_owned(),
            MaterializedTokenValue::new(value, action),
        )?;
        if publication.changed {
            self.publish_resource_change(publication.revision);
        }
        Ok(publication)
    }

    #[cfg(test)]
    fn activate_extension_candidate(
        &mut self,
        candidate: crate::extensions::ExtensionRegistry,
    ) -> Result<ExtensionReload, DaemonError> {
        let prepared = self.prepare_extension_candidate(candidate)?;
        let base_generation = prepared.base_generation;
        self.commit_extension_candidate(prepared, base_generation)
    }

    fn prepare_extension_candidate(
        &self,
        candidate: crate::extensions::ExtensionRegistry,
    ) -> Result<PreparedExtensionReload, DaemonError> {
        let base_generation = self.extension_registry.generation();
        let generation = if candidate.fingerprint() == self.extension_registry.fingerprint() {
            base_generation
        } else {
            base_generation.checked_add(1).ok_or_else(|| {
                DaemonError::new(
                    "extension_reload_failed",
                    "extension generation is exhausted; restart the daemon before retrying",
                )
            })?
        };
        let candidate = candidate.at_generation(generation).map_err(|error| {
            DaemonError::new(
                "extension_reload_failed",
                format!("could not prepare extension generation {generation}: {error:#}"),
            )
        })?;
        let catalog = candidate.catalog().map_err(|error| {
            DaemonError::new(
                "extension_reload_failed",
                format!("could not prepare extension catalog: {error:#}"),
            )
        })?;
        Ok(PreparedExtensionReload {
            base_generation,
            candidate,
            catalog,
        })
    }

    fn commit_extension_candidate(
        &mut self,
        prepared: PreparedExtensionReload,
        base_generation: u64,
    ) -> Result<ExtensionReload, DaemonError> {
        if prepared.base_generation != base_generation {
            return Err(DaemonError::new(
                "invalid_extension_transaction",
                "extension reload commit does not match the prepared base generation",
            ));
        }
        if self.extension_registry.generation() != base_generation {
            return Err(DaemonError::new(
                "stale_extension_generation",
                format!(
                    "extension generation changed from {base_generation} to {}; reload the local UI candidate and retry",
                    self.extension_registry.generation()
                ),
            ));
        }
        let changed = prepared.candidate.fingerprint() != self.extension_registry.fingerprint();
        if changed {
            let pruned = self.resources.prune_extension_presentation_tokens(
                prepared.candidate.declared_presentation_tokens(),
            );
            self.extension_registry = Arc::new(prepared.candidate);
            self.extension_catalog
                .send_replace(prepared.catalog.clone());
            if pruned.changed {
                self.publish_resource_change(pruned.revision);
            }
        }
        Ok(ExtensionReload {
            catalog: prepared.catalog,
            changed,
        })
    }

    fn apply_status_updates(
        &mut self,
        terminal_id: TerminalId,
        updates: Vec<StatusUpdate>,
        source: StatusSource,
    ) -> Result<(), DaemonError> {
        self.apply_status_updates_internal(terminal_id, updates, source, false, None)
    }

    fn apply_terminal_status_updates(
        &mut self,
        terminal_id: TerminalId,
        batch: NativeStatusBatch,
    ) -> Result<(), DaemonError> {
        let automatic_pid = self
            .runtimes
            .get(&terminal_id)
            .and_then(|runtime| batch.owner_pid(runtime.handle.child_pid()));
        self.apply_status_updates_internal(
            terminal_id,
            batch.updates,
            batch.source.status_source(),
            true,
            automatic_pid,
        )
    }

    fn apply_status_updates_internal(
        &mut self,
        terminal_id: TerminalId,
        updates: Vec<StatusUpdate>,
        source: StatusSource,
        terminal_delivery: bool,
        automatic_pid: Option<u32>,
    ) -> Result<(), DaemonError> {
        let terminal_process_exit =
            terminal_delivery && updates.contains(&StatusUpdate::ProcessExit);
        let before_status = self.resources.program_status(terminal_id)?.clone();
        let before = before_status.revision;
        let revision = if let Some(automatic_pid) = automatic_pid {
            self.resources.apply_automatic_status_updates(
                terminal_id,
                updates.clone(),
                source.clone(),
                automatic_pid,
                now_ms(),
            )?
        } else {
            self.resources.apply_status_updates(
                terminal_id,
                updates.clone(),
                source.clone(),
                now_ms(),
            )?
        };
        let status = self.resources.program_status(terminal_id)?.clone();
        let changed = status.revision != before;
        if changed {
            let transitions =
                status_lifecycle_transitions(&before_status, &updates, &source, &status);
            self.publish_resource_change(revision);
            let _ = self.agent_events.send(StatusLifecycleUpdate {
                terminal_id,
                status,
                transitions,
                terminal_process_exit,
            });
        }
        Ok(())
    }

    fn report_automatic_agent(
        &mut self,
        terminal_id: TerminalId,
        report: StatusUpdate,
        source: StatusSource,
        agent_pid: u32,
    ) -> Result<(), DaemonError> {
        Self::validate_agent_report(&report, &source)?;
        self.apply_status_updates_internal(
            terminal_id,
            vec![report],
            source,
            false,
            Some(agent_pid),
        )
    }

    fn report_external_agent(
        &mut self,
        terminal_id: TerminalId,
        report: StatusUpdate,
        source: StatusSource,
    ) -> Result<(), DaemonError> {
        Self::validate_agent_report(&report, &source)?;
        self.apply_status_updates(terminal_id, vec![report], source)
    }

    fn validate_agent_report(
        report: &StatusUpdate,
        source: &StatusSource,
    ) -> Result<(), DaemonError> {
        if !matches!(report, StatusUpdate::Report(_) | StatusUpdate::Clear { .. }) {
            return Err(ResourceError::InvalidStatusReport(
                "lifecycle updates are reserved for the terminal runtime",
            )
            .into());
        }
        if !matches!(source, StatusSource::Report { .. }) {
            return Err(ResourceError::InvalidStatusReport(
                "external reports require report provenance",
            )
            .into());
        }
        Ok(())
    }

    fn release_automatic_agent_owner(
        &mut self,
        terminal_id: TerminalId,
        agent_pid: u32,
    ) -> Result<(), DaemonError> {
        let before_status = self.resources.program_status(terminal_id)?.clone();
        let before = before_status.revision;
        let revision = self
            .resources
            .release_automatic_status_owner(terminal_id, agent_pid)?;
        let status = self.resources.program_status(terminal_id)?.clone();
        if status.revision != before {
            let transitions =
                status_lifecycle_transitions(&before_status, &[], &StatusSource::Osc, &status);
            self.publish_resource_change(revision);
            let _ = self.agent_events.send(StatusLifecycleUpdate {
                terminal_id,
                status,
                transitions,
                terminal_process_exit: false,
            });
        }
        Ok(())
    }

    fn automatic_agent_owner(&self, terminal_id: TerminalId, peer_pid: Option<u32>) -> Option<u32> {
        self.runtimes
            .get(&terminal_id)
            .zip(peer_pid)
            .and_then(|(runtime, pid)| process::descendant_root(pid, runtime.handle.child_pid()))
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
        self.alerts.register_terminal(terminal.id());
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
        self.alerts.register_terminal(terminal.id());
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
        self.alerts.register_terminal(terminal.id());
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
        self.alerts.register_terminal(terminal.id());
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
        self.alerts.register_terminal(terminal.id());
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

    fn finalize_terminal(
        &mut self,
        terminal_id: TerminalId,
    ) -> Result<FinalizationOutcome, DaemonError> {
        if self.expected_finalizations.remove(&terminal_id) {
            return Ok(FinalizationOutcome::AlreadyFinalized);
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
        self.alerts.remove_terminal(terminal_id);
        self.publish_alert_change();
        self.exited_terminals.push_back((terminal_id, exit_code));
        if self.exited_terminals.len() > 256 {
            self.exited_terminals.pop_front();
        }
        self.publish_mutation(&mutation);
        if mutation.multiplexer_empty {
            self.accepting = false;
        }
        Ok(if mutation.multiplexer_empty {
            FinalizationOutcome::MultiplexerEmpty
        } else {
            FinalizationOutcome::ResourcesRemain
        })
    }

    fn finalize_terminal_for_replacement(
        &mut self,
        terminal_id: TerminalId,
    ) -> Result<FinalizationOutcome, DaemonError> {
        let outcome = self.finalize_terminal(terminal_id)?;
        self.expected_finalizations.insert(terminal_id);
        Ok(outcome)
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

    fn fail_target_close(
        &mut self,
        scope: CloseScope,
        closed: usize,
        total: usize,
        error: CommandError,
    ) -> DaemonError {
        if closed > 0 {
            return DaemonError::new(
                "close_partial",
                format!(
                    "closed {closed} of {total} terminals; the target remains marked closing because completed exits cannot be rolled back · {error}"
                ),
            );
        }

        if matches!(error, CommandError::CloseTimeout) {
            return DaemonError::new(
                "close_incomplete",
                format!(
                    "terminal termination began but did not finish; the target remains marked closing because termination cannot be rolled back · {error}"
                ),
            );
        }

        self.cancel_target_close(scope);
        DaemonError::command("close_failed", error)
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

    fn move_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        position: usize,
    ) -> Result<u64, DaemonError> {
        if !self.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let mutation = self.resources.move_workspace(workspace_id, position)?;
        self.publish_mutation(&mutation);
        Ok(mutation.revision)
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
            RenameSelector::Pane(id) => self.resources.rename_pane(id, name)?,
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

async fn prepare_extension_reload(shared: &Shared) -> Result<PreparedExtensionReload, DaemonError> {
    let location = shared.lock().await.config_location.clone();
    let source = location.path.as_ref().map_or_else(
        || "the resolved default configuration".to_owned(),
        |path| path.display().to_string(),
    );
    let candidate = tokio::task::spawn_blocking(move || {
        let loaded = global_config::load_extensions_location(&location)?;
        crate::extensions::ExtensionRegistry::new(1, loaded.extensions, loaded.config)
    })
    .await
    .map_err(|error| {
        DaemonError::new(
            "extension_reload_failed",
            format!(
                "extension reload worker failed while reading {source}: {error}; retry the reload; the active extension registry was left unchanged"
            ),
        )
    })?
    .map_err(|error| {
        DaemonError::new(
            "extension_reload_failed",
            format!(
                "could not reload extensions from {source}: {error:#}; fix the configuration or extension package and retry; the active extension registry was left unchanged"
            ),
        )
    })?;

    shared.lock().await.prepare_extension_candidate(candidate)
}

async fn reload_extensions(shared: &Shared) -> Result<ExtensionReload, DaemonError> {
    let prepared = prepare_extension_reload(shared).await?;
    let base_generation = prepared.base_generation;
    shared
        .lock()
        .await
        .commit_extension_candidate(prepared, base_generation)
}

async fn respond_to_project_config_reload(
    connection: &mut ClientConnection,
    shared: &Shared,
    request_id: Option<Uuid>,
    session_id: SessionId,
    synchronize_snapshot: bool,
) -> Result<()> {
    match recipe::reload_project_config(shared, session_id).await {
        Ok((resource_revision, changed)) => {
            if changed && synchronize_snapshot {
                let snapshot = shared.lock().await.resources.snapshot();
                send(
                    connection,
                    None,
                    ServerMessage::ResourcesChanged { snapshot },
                )
                .await?;
            }
            send(
                connection,
                request_id,
                ServerMessage::ProjectConfigReloaded {
                    resource_revision,
                    changed,
                },
            )
            .await?
        }
        Err(error) => send_error(connection, request_id, error.code, &error.message).await?,
    }
    Ok(())
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
    pending_configuration: Option<crate::terminal::AttachmentConfiguration>,
}

impl LeasedTarget {
    async fn apply_acquisition_configuration(&mut self) -> Result<(), DaemonError> {
        let Some(configuration) = self.pending_configuration.take() else {
            return Ok(());
        };
        self.terminal
            .configure_attachment(configuration)
            .await
            .map_err(|error| DaemonError::new(command_error_code(&error), error.to_string()))
    }
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
    panes: Vec<ObservedTarget>,
    focused: LeasedTarget,
    layout: SplitTree,
    fallback_terminal_ids: Vec<TerminalId>,
    resource_revision: u64,
    streamed_resource_revision: u64,
    resource_changes: watch::Receiver<u64>,
    presence_changes: watch::Receiver<ClientPresenceSnapshot>,
    updates: mpsc::Receiver<AttachmentUpdate>,
    update_sender: mpsc::Sender<AttachmentUpdate>,
    watchers: HashMap<TerminalId, (u64, JoinHandle<()>)>,
    viewport_offsets: HashMap<TerminalId, usize>,
    snapshot_revisions: HashMap<TerminalId, u64>,
    copy_mode_terminal: Option<Arc<TerminalHandle>>,
    next_watcher_generation: u64,
}

struct AttachmentInit {
    owner: ClientId,
    panes: Vec<ObservedTarget>,
    focused: LeasedTarget,
    layout: SplitTree,
    fallback_terminal_ids: Vec<TerminalId>,
    resource_revision: u64,
}

#[derive(Clone, Copy)]
struct FocusedViewportState {
    terminal_id: TerminalId,
    offset: Option<usize>,
    snapshot_revision: Option<u64>,
}

impl Attachment {
    fn new(
        init: AttachmentInit,
        resource_changes: watch::Receiver<u64>,
        presence_changes: watch::Receiver<ClientPresenceSnapshot>,
    ) -> Self {
        let (update_sender, updates) = mpsc::channel(ATTACHMENT_UPDATE_CAPACITY);
        let mut attachment = Self {
            owner: init.owner,
            panes: init.panes,
            focused: init.focused,
            layout: init.layout,
            fallback_terminal_ids: init.fallback_terminal_ids,
            resource_revision: init.resource_revision,
            streamed_resource_revision: init.resource_revision,
            resource_changes,
            presence_changes,
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
        let configuration = self
            .focused
            .lease
            .resize(size)
            .ok_or(CommandError::Stopped)?;
        self.viewport_offsets
            .remove(&self.focused.selected.terminal_id);
        self.focused
            .terminal
            .configure_attachment(configuration)
            .await?;
        Ok(configuration.size)
    }

    async fn set_focused_colors(
        &self,
        colors: crate::domain::TerminalColors,
    ) -> Result<(), CommandError> {
        let configuration = self
            .focused
            .lease
            .set_colors(colors)
            .ok_or(CommandError::Stopped)?;
        self.focused
            .terminal
            .configure_attachment(configuration)
            .await
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

        if let Err(error) = terminal.refresh_snapshot().await {
            let _ = updates
                .send(AttachmentUpdate::Error {
                    terminal_id,
                    generation,
                    message: error.to_string(),
                })
                .await;
            return;
        }

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
    let mut termination = DaemonTerminationSignals::subscribe()?;
    let config_location = config.config_location;
    let loaded_extensions = global_config::load_extensions_location(&config_location)?;
    let projects = global_config::load_projects_location(&config_location)?;
    let terminal_config = global_config::load_terminal_location(&config_location)?;
    let default_shell = default_shell(terminal_config.shell.as_deref());
    let extension_registry = Arc::new(crate::extensions::ExtensionRegistry::new(
        1,
        loaded_extensions.extensions,
        loaded_extensions.config,
    )?);
    let initial_extension_catalog = extension_registry.catalog()?;
    tracing::debug!(
        generation = extension_registry.generation(),
        fingerprint = extension_registry.fingerprint(),
        extensions = extension_registry.extensions().len(),
        "built daemon extension registry"
    );
    let fut_bin = std::env::current_exe().context("resolve current Fut executable")?;
    let (hook_queue, hook_receiver) = crate::extensions::hook_queue();
    let resolved = ProjectResolver::default()
        .resolve(&config.spawn.cwd)
        .await
        .map_err(DaemonError::from)?;
    let command_override = config
        .recipe_command_override
        .then(|| (config.spawn.program.clone(), config.spawn.argv.clone()));
    let prepared_recipe = recipe::prepare_initial(
        &projects,
        extension_registry.extensions(),
        &resolved,
        command_override,
        &default_shell,
    )
    .await?;
    let socket = bind_socket(&config.socket_path).await?;
    let mut initial_spawn = config.spawn;
    initial_spawn.cwd = resolved.cwd.clone();
    initial_spawn.terminal = terminal_config.clone();
    if !config.recipe_command_override {
        initial_spawn.program = default_shell.clone();
    }
    // Like tmux's default-shell, export the configured shell so programs that
    // spawn "the user's shell" (and recipe commands returning to one) agree.
    if let Some(shell) = &terminal_config.shell {
        initial_spawn
            .env
            .insert("SHELL".into(), shell.as_os_str().to_owned());
    }
    let child_env = initial_spawn.env.clone();
    let (resource_changes, _) = watch::channel(0);
    let (alert_changes, _) = watch::channel(0);
    let (extension_catalog, _) = watch::channel(initial_extension_catalog);
    let (agent_events, _) = broadcast::channel(AGENT_EVENT_CAPACITY);
    let mut state = SharedState {
        attachment_store: Default::default(),
        resources: ResourceTree::default(),
        runtimes: HashMap::new(),
        presence: ClientPresence::default(),
        expected_finalizations: HashSet::new(),
        exited_terminals: VecDeque::new(),
        resource_changes,
        alerts: AlertStore::default(),
        alert_changes,
        agent_events,
        child_env,
        terminal_config,
        default_shell,
        config_location,
        extension_registry,
        extension_catalog,
        hook_queue,
        accepting: true,
    };
    let initial_terminals = if let Some(recipe) = prepared_recipe {
        recipe::create_initial(&mut state, &resolved, &recipe).await?
    } else {
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
        initial_spawn.env = terminal_env(&state.child_env, initial_target);
        let terminal = Arc::new(spawn_terminal(initial_spawn)?);
        if let Err(error) = state.register_session(initial, Arc::clone(&terminal)) {
            let _ = terminal.close().await;
            return Err(error.into());
        }
        vec![terminal]
    };
    let git_resource_changes = state.resource_changes.subscribe();
    let shared = Arc::new(Mutex::new(state));
    let (hook_shutdown_tx, hook_shutdown_rx) = watch::channel(false);
    let mut hooks = tokio::spawn(crate::extensions::run_hooks(
        hook_receiver,
        fut_bin,
        config.socket_path.clone(),
        hook_shutdown_rx,
    ));
    let (exited_tx, mut exited_rx) = mpsc::unbounded_channel();
    for terminal in initial_terminals {
        watch_terminal(terminal, Arc::clone(&shared), exited_tx.clone());
    }
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let process_names = watch_process_names(Arc::clone(&shared), shutdown_tx.subscribe());
    let git_metadata = git::watch(
        Arc::clone(&shared),
        git_resource_changes,
        shutdown_tx.subscribe(),
    );
    let mut connections = JoinSet::new();
    let writers = WriterTasks::default();

    let shutdown_reason = loop {
        tokio::select! {
            biased;
            signal = termination.recv() => {
                break signal;
            }
            Some(terminal_id) = exited_rx.recv() => {
                if finalize_terminal(&shared, terminal_id).await {
                    break "last terminal exited";
                }
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break "shutdown requested";
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
    };
    tracing::info!(reason = shutdown_reason, "daemon shutting down");
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

struct DaemonTerminationSignals {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
    quit: Signal,
}

impl DaemonTerminationSignals {
    fn subscribe() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
            quit: signal(SignalKind::quit())?,
        })
    }

    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.hangup.recv() => "SIGHUP",
            _ = self.quit.recv() => "SIGQUIT",
        }
    }
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
                _ = interval.tick() => refresh_process_names(
                    &shared,
                    &mut codex,
                    &mut worktrees,
                ).await,
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
    codex_probe: CodexProbe,
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
                let codex_probe = if is_codex_process(&name, &command) {
                    match terminal
                        .read_output(TerminalOutputSource::Visible, 1, false)
                        .await
                    {
                        Ok(output) => CodexProbe::Screen(output.text),
                        Err(_) => CodexProbe::Unavailable,
                    }
                } else {
                    CodexProbe::NotRunning
                };
                Some(ProcessObservation {
                    terminal_id,
                    pid,
                    name,
                    command,
                    cwd,
                    codex_probe,
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
    let live = observations
        .iter()
        .map(|observation| observation.terminal_id)
        .collect::<HashSet<_>>();
    codex.retain(|terminal_id, _| live.contains(terminal_id));
    let exited_automatic_agents = state
        .resources
        .automatic_status_owners()
        .into_iter()
        .filter(|(terminal_id, owner_pid)| {
            let owner_is_live = state.runtimes.get(terminal_id).is_some_and(|runtime| {
                process::is_descendant(*owner_pid, runtime.handle.child_pid())
            });
            !owner_is_live
        })
        .collect::<Vec<_>>();
    for (terminal_id, owner_pid) in exited_automatic_agents {
        let _ = state.release_automatic_agent_owner(terminal_id, owner_pid);
    }
    for observation in observations {
        let ProcessObservation {
            terminal_id,
            pid,
            name,
            command,
            cwd,
            codex_probe,
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
        let detection = match codex_probe {
            CodexProbe::Screen(screen) => {
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
                Some((
                    StatusReport {
                        id: String::new(),
                        state: stable_state,
                        kind: (stable_state == StatusState::Blocked)
                            .then_some(BlockedKind::Question),
                        progress: None,
                        app: Some("codex".into()),
                        title: None,
                        message: None,
                    },
                    StatusSource::Detection {
                        agent: "codex".into(),
                        rule: detected.rule.into(),
                    },
                ))
            }
            CodexProbe::NotRunning => {
                codex.remove(&terminal_id);
                None
            }
            CodexProbe::Unavailable => continue,
        };
        if let Some((report, source)) = detection {
            let _ =
                state.apply_status_updates(terminal_id, vec![StatusUpdate::Report(report)], source);
        } else if state
            .resources
            .has_detection_status(terminal_id, "codex")
            .unwrap_or(false)
        {
            let _ = state.apply_status_updates(
                terminal_id,
                vec![StatusUpdate::ProcessExit],
                StatusSource::Detection {
                    agent: "codex".into(),
                    rule: "process_exit".into(),
                },
            );
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
        tokio::fs::read_to_string(format!("/proc/{pid}/comm"))
            .await
            .ok()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
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
        trusted_project_config: None,
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
    base: &HashMap<OsString, OsString>,
    target: crate::resources::ResolvedTerminalPath,
) -> HashMap<OsString, OsString> {
    let mut env = base.clone();
    for (key, value) in [
        ("FUT_SESSION_ID", target.session_id.to_string()),
        ("FUT_WORKSPACE_ID", target.workspace_id.to_string()),
        ("FUT_TAB_ID", target.tab_id.to_string()),
        ("FUT_PANE_ID", target.pane_id.to_string()),
        ("FUT_TERMINAL_ID", target.terminal_id.to_string()),
    ] {
        env.insert(key.into(), value.into());
    }
    env
}

fn watch_terminal(
    terminal: Arc<TerminalHandle>,
    shared: Shared,
    exited: mpsc::UnboundedSender<TerminalId>,
) {
    // Close may have discarded the unclaimed stream after registration but
    // before this watcher starts. Still observe the process until it exits.
    let mut statuses = terminal.take_status_updates();
    tokio::spawn(async move {
        let mut lifecycle = terminal.subscribe_lifecycle();
        let mut activity = terminal.subscribe_activity();
        let initial = *activity.borrow_and_update();
        if initial.bell_count > 0 {
            record_terminal_bells(&shared, terminal.id(), initial.bell_count).await;
        }
        loop {
            let process_exited = matches!(
                *lifecycle.borrow_and_update(),
                TerminalLifecycle::Exited { .. }
            );
            if process_exited {
                if let Some(statuses) = statuses.as_mut() {
                    while let Ok(batch) = statuses.try_recv() {
                        record_terminal_status_updates(&shared, terminal.id(), batch).await;
                    }
                }
                if activity.has_changed().unwrap_or(false) {
                    let update = *activity.borrow_and_update();
                    record_terminal_bells(&shared, terminal.id(), update.bell_count).await;
                }
                break;
            }
            tokio::select! {
                biased;
                batch = async {
                    match statuses.as_mut() {
                        Some(statuses) => statuses.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(batch) = batch else {
                        break;
                    };
                    record_terminal_status_updates(&shared, terminal.id(), batch).await;
                }
                changed = activity.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let update = *activity.borrow_and_update();
                    record_terminal_bells(&shared, terminal.id(), update.bell_count).await;
                }
                changed = lifecycle.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = exited.send(terminal.id());
    });
}

async fn record_terminal_status_updates(
    shared: &Shared,
    terminal_id: TerminalId,
    batch: NativeStatusBatch,
) {
    // The runtime sampled the foreground PID before publishing this batch.
    // Never query the terminal here: its bounded status send may be blocking
    // the same runtime from servicing control RPCs until this receiver drains.
    let mut state = shared.lock().await;
    if let Err(error) = state.apply_terminal_status_updates(terminal_id, batch) {
        tracing::warn!(message = %error.message, %terminal_id, "apply terminal status updates");
    }
}

async fn record_terminal_bells(shared: &Shared, terminal_id: TerminalId, bell_count: u64) {
    let mut state = shared.lock().await;
    if state.alerts.record_bells(terminal_id, bell_count, now_ms()) {
        state.publish_alert_change();
    }
}

async fn finalize_terminal(shared: &Shared, terminal_id: TerminalId) -> bool {
    let mut state = shared.lock().await;
    match state.finalize_terminal(terminal_id) {
        Ok(outcome) => outcome.is_empty(),
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
    peer_pid: Option<u32>,
    remote: Option<Capabilities>,
    local: Option<local::Capabilities>,
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
    /// An ordered screen or view change. The writer owns both the delta
    /// bases and image delivery cache, including their eviction on selection.
    Message {
        request_id: Option<uuid::Uuid>,
        message: Box<ServerMessage>,
    },
}

#[derive(Default)]
struct OutboundState {
    items: VecDeque<Outbound>,
    finished: bool,
    failed: bool,
}

struct OutboundQueue {
    state: std::sync::Mutex<OutboundState>,
    ready: Notify,
    /// Also check writer-generated image/delta/chunk messages at the boundary,
    /// not only messages that pass through `send`.
    local: std::sync::OnceLock<local::Capabilities>,
    /// Set for remote peers without `large-screens.v1`: their frozen screen
    /// format embeds Kitty image pixels instead of referencing separate
    /// `kitty_image` frames.
    embed_images: std::sync::atomic::AtomicBool,
    /// Set for remote peers without `scroll-deltas.v1`: their frozen delta
    /// format has no row shift, so scrolling falls back to row diffs.
    unshifted_deltas: std::sync::atomic::AtomicBool,
    /// The largest screen this peer may receive: its negotiated visible-cell
    /// bound. Screens above [`crate::domain::MAX_VISIBLE_CELLS`] go out as
    /// row-range chunks, so peers without `chunked-screens.v1` keep a lower
    /// bound; a larger terminal shared with another client reaches them
    /// cropped until their own resize shrinks it.
    max_screen_cells: std::sync::atomic::AtomicUsize,
}

impl Default for OutboundQueue {
    fn default() -> Self {
        Self {
            state: Default::default(),
            ready: Default::default(),
            local: Default::default(),
            embed_images: Default::default(),
            unshifted_deltas: Default::default(),
            max_screen_cells: crate::domain::MAX_GRID_CELLS.into(),
        }
    }
}

impl ClientConnection {
    fn new(stream: UnixStream, writers: &WriterTasks) -> Self {
        let peer_pid = stream
            .peer_cred()
            .ok()
            .and_then(|credentials| credentials.pid().and_then(|pid| u32::try_from(pid).ok()));
        let (sink, reader) = Framed::new(stream, codec()).split();
        let outbound = Arc::new(OutboundQueue::default());
        writers
            .lock()
            .expect("writer registry lock poisoned")
            .spawn(write_outbound(sink, Arc::clone(&outbound)));
        Self {
            reader,
            outbound,
            peer_pid,
            remote: None,
            local: None,
        }
    }

    async fn next(&mut self) -> Option<std::io::Result<bytes::BytesMut>> {
        self.reader.next().await
    }

    /// Largest grid this connection's client may request; remote peers keep
    /// the bound of the screen formats they negotiated.
    fn max_visible_cells(&self) -> usize {
        self.remote.map_or(
            crate::domain::MAX_GRID_CELLS,
            Capabilities::max_visible_cells,
        )
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
            Outbound::Message {
                request_id: None,
                message,
            } if matches!(
                &*message,
                ServerMessage::CommandReply {
                    reply: crate::protocol::RemoteCommandReply::Screen { .. }
                }
            ) =>
            {
                if let Some(Outbound::Message {
                    request_id: None,
                    message: pending,
                }) = state.items.back_mut()
                    && matches!(
                        &**pending,
                        ServerMessage::CommandReply {
                            reply: crate::protocol::RemoteCommandReply::Screen { .. }
                        }
                    )
                {
                    *pending = message;
                } else {
                    state.items.push_back(Outbound::Message {
                        request_id: None,
                        message,
                    });
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

/// Choose between a full snapshot and a delta against `last_sent`, the
/// screen actually written to this connection last time — never against
/// whatever the attachment layer last produced, since coalescing can drop
/// frames the writer never sent. Updates `last_sent` to match what this call
/// returns. `shifts` permits deltas that move retained rows.
fn snapshot_message(
    terminal_id: TerminalId,
    screen: ScreenSnapshot,
    force_full: bool,
    shifts: bool,
    last_sent: &mut HashMap<TerminalId, ScreenSnapshot>,
) -> ServerMessage {
    if !force_full
        && let Some(previous) = last_sent.get(&terminal_id)
        && let Some(delta) = screen.delta_from(previous, shifts)
    {
        last_sent.insert(terminal_id, screen);
        return ServerMessage::SnapshotDelta { terminal_id, delta };
    }
    last_sent.insert(terminal_id, screen.clone());
    ServerMessage::Snapshot {
        terminal_id,
        screen,
    }
}

/// Kitty images a client holds, per terminal, when screens reference
/// separately sent pixels. Both sides retain exactly the latest complete
/// graphics on the wire, even when the client rejects that screen as stale.
/// Displayed screens own shared pixels independently of this delivery cache.
#[derive(Default)]
struct SentImages(HashMap<TerminalId, HashSet<(u32, u64)>>);

impl SentImages {
    /// Return `kitty_image` messages for images in `message` the client may
    /// lack, and replace the pixels in `message` with references.
    fn prepare(&mut self, message: &mut ServerMessage) -> Vec<ServerMessage> {
        let (terminal_id, graphics) = match message {
            ServerMessage::TargetSelected { selected } => {
                let visible: HashSet<_> =
                    selected.panes.iter().map(|pane| pane.terminal_id).collect();
                self.0.retain(|id, _| visible.contains(id));
                return Vec::new();
            }
            ServerMessage::Snapshot {
                terminal_id,
                screen,
            }
            | ServerMessage::CopyModeSnapshot {
                terminal_id,
                screen,
            }
            | ServerMessage::CopyModeFinalized {
                terminal_id,
                screen,
            }
            | ServerMessage::CopyModeCancelled {
                terminal_id,
                screen,
            } => (*terminal_id, &mut screen.graphics),
            ServerMessage::SnapshotDelta {
                terminal_id,
                delta:
                    ScreenDelta {
                        graphics: Some(graphics),
                        ..
                    },
            } => (*terminal_id, graphics),
            _ => return Vec::new(),
        };
        let held = self.0.remove(&terminal_id).unwrap_or_default();
        let mut images = Vec::new();
        let mut now_held = HashSet::with_capacity(graphics.images.len());
        for image in &graphics.images {
            let key = (image.id, image.generation);
            if !held.contains(&key) && !image.is_reference() {
                images.push(ServerMessage::KittyImage {
                    terminal_id,
                    image: image.clone(),
                });
            }
            now_held.insert(key);
        }
        graphics.strip_pixels();
        if !now_held.is_empty() {
            self.0.insert(terminal_id, now_held);
        }
        images
    }
}

async fn write_outbound(
    mut sink: SplitSink<Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>, Bytes>,
    queue: Arc<OutboundQueue>,
) {
    let mut last_sent: HashMap<TerminalId, ScreenSnapshot> = HashMap::new();
    let mut sent_images = SentImages::default();
    let max_screen_cells = || {
        queue
            .max_screen_cells
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    loop {
        let Some(item) = queue.pop() else {
            if queue.is_finished() {
                return;
            }
            queue.ready.notified().await;
            continue;
        };
        let (request_id, mut message) = match item {
            Outbound::Frame(bytes) => {
                if !send_outbound(&mut sink, &queue, Ok(bytes)).await {
                    return;
                }
                continue;
            }
            Outbound::Snapshot {
                terminal_id,
                mut screen,
                force_full,
            } => (
                None,
                snapshot_message(
                    terminal_id,
                    {
                        // Before diffing, so `last_sent` is what the peer has.
                        screen.crop_to(max_screen_cells());
                        screen
                    },
                    force_full,
                    !queue
                        .unshifted_deltas
                        .load(std::sync::atomic::Ordering::Relaxed),
                    &mut last_sent,
                ),
            ),
            Outbound::Message {
                request_id,
                mut message,
            } => {
                match &mut *message {
                    ServerMessage::CopyModeSnapshot { screen, .. }
                    | ServerMessage::CopyModeFinalized { screen, .. }
                    | ServerMessage::CopyModeCancelled { screen, .. } => {
                        screen.crop_to(max_screen_cells());
                    }
                    _ => {}
                }
                match &*message {
                    ServerMessage::TargetSelected { selected } => {
                        let visible: HashSet<_> =
                            selected.panes.iter().map(|pane| pane.terminal_id).collect();
                        last_sent.retain(|id, _| visible.contains(id));
                    }
                    ServerMessage::CopyModeSnapshot { terminal_id, .. }
                    | ServerMessage::CopyModeFinalized { terminal_id, .. }
                    | ServerMessage::CopyModeCancelled { terminal_id, .. } => {
                        // Copy-mode graphics can evict live images. The next
                        // live screen must state its graphics again, rather
                        // than diff against a pre-copy-mode delivery.
                        last_sent.remove(terminal_id);
                    }
                    _ => {}
                }
                (request_id, *message)
            }
        };
        if !queue
            .embed_images
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            for image in sent_images.prepare(&mut message) {
                if queue
                    .local
                    .get()
                    .is_some_and(|caps| !caps.allows_server(&image))
                {
                    tracing::error!(message_type = ?std::mem::discriminant(&image), "outbound image is not allowed by the negotiated local contract");
                    queue.fail();
                    return;
                }
                let payload = encode_payload(&Envelope {
                    request_id: None,
                    message: image,
                })
                .map(Bytes::from);
                if !send_outbound(&mut sink, &queue, payload).await {
                    return;
                }
            }
        }
        // Split only here, after coalescing: the queue holds whole screens,
        // and a sequence goes out contiguously before anything else.
        let frames = if max_screen_cells() > crate::domain::MAX_VISIBLE_CELLS {
            crate::protocol::chunked::split_screen(message, crate::domain::MAX_VISIBLE_CELLS)
        } else {
            vec![message]
        };
        let last = frames.len() - 1;
        for (index, message) in frames.into_iter().enumerate() {
            if queue
                .local
                .get()
                .is_some_and(|caps| !caps.allows_server(&message))
            {
                tracing::error!(message_type = ?std::mem::discriminant(&message), "outbound frame is not allowed by the negotiated local contract");
                queue.fail();
                return;
            }
            let payload = encode_payload(&Envelope {
                request_id: request_id.filter(|_| index == last),
                message,
            })
            .map(Bytes::from);
            if !send_outbound(&mut sink, &queue, payload).await {
                return;
            }
        }
    }
}

/// Write one encoded frame; on failure, fail the queue and return `false`.
async fn send_outbound(
    sink: &mut SplitSink<Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>, Bytes>,
    queue: &OutboundQueue,
    payload: Result<Bytes, crate::protocol::FrameError>,
) -> bool {
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
        return false;
    }
    true
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
    let frame = match frame {
        Ok(frame) => frame,
        Err(_) => {
            send(
                &mut connection,
                None,
                ServerMessage::EndpointError {
                    error: EndpointError::InvalidHandshake,
                },
            )
            .await?;
            return Ok(());
        }
    };
    let first: Envelope<ClientMessage> = match decode_payload(&frame) {
        Ok(first) => first,
        Err(_) => {
            send(
                &mut connection,
                None,
                ServerMessage::EndpointError {
                    error: EndpointError::InvalidHandshake,
                },
            )
            .await?;
            return Ok(());
        }
    };
    let mode = match first.message {
        ClientMessage::Hello { version, mode, .. } => {
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
            mode
        }
        ClientMessage::LocalHello(hello) => {
            let negotiated = remote::decode_handshake::<Envelope<ClientMessage>>(&frame)
                .and_then(|_| hello.negotiate());
            match negotiated {
                Ok(capabilities) => {
                    connection.local = Some(capabilities);
                    connection
                        .outbound
                        .local
                        .set(capabilities)
                        .expect("local handshake is only accepted once");
                }
                Err(error) => {
                    send(
                        &mut connection,
                        first.request_id,
                        ServerMessage::EndpointError { error },
                    )
                    .await?;
                    return Ok(());
                }
            }
            hello.mode
        }
        ClientMessage::RemoteHello(hello) => {
            let negotiated = remote::decode_handshake::<Envelope<ClientMessage>>(&frame)
                .and_then(|_| hello.negotiate(Capabilities::ALL));
            match negotiated {
                Ok(capabilities) => {
                    connection.remote = Some(capabilities);
                    connection.outbound.embed_images.store(
                        !capabilities.separate_images(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    connection.outbound.unshifted_deltas.store(
                        !capabilities.contains(Capability::ScrollDeltas),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    connection.outbound.max_screen_cells.store(
                        capabilities.max_visible_cells(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                Err(error) => {
                    send(
                        &mut connection,
                        first.request_id,
                        ServerMessage::EndpointError { error },
                    )
                    .await?;
                    return Ok(());
                }
            }
            hello.mode
        }
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
    let client = ClientId::new();
    let presence = shared.lock().await.presence.clone();
    let (leased, interactive_size) = match mode {
        ClientMode::Interactive { size, selector } => {
            if let Err(error) = size.validate_within(connection.max_visible_cells()) {
                send_error(
                    &mut connection,
                    first.request_id,
                    "invalid_size",
                    &error.to_string(),
                )
                .await?;
                return Ok(());
            }
            let attachment =
                match lease_view(&shared, selector, None, client, LeaseSize::Requested(size)).await
                {
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
            (Some(attachment), Some(size))
        }
        ClientMode::Control => (None, None),
    };
    let mut presence = leased
        .as_ref()
        .map(|attachment| presence.attach(client, attachment.focused.selected.session_id));
    let (extension_catalog, mut extension_changes) = {
        let state = shared.lock().await;
        (
            state.extension_catalog.borrow().clone(),
            state.extension_catalog.subscribe(),
        )
    };
    let selected = leased.as_ref().map(Attachment::selected);
    let welcome = match connection.remote {
        Some(capabilities) => ServerMessage::RemoteWelcome(RemoteWelcome {
            generation: remote::GENERATION,
            codec: remote::CODEC.into(),
            server_version: env!("CARGO_PKG_VERSION").into(),
            capabilities: capabilities.names(),
            selected,
            extension_catalog: capabilities
                .contains(Capability::ExtensionCatalog)
                .then_some(extension_catalog),
        }),
        None if connection.local.is_some() => ServerMessage::LocalWelcome(LocalWelcome {
            generation: local::GENERATION,
            codec: local::CODEC.into(),
            server_version: env!("CARGO_PKG_VERSION").into(),
            capabilities: connection
                .local
                .expect("negotiated local capabilities")
                .names(),
            selected,
            extension_catalog,
        }),
        None => ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").into(),
            selected,
            extension_catalog,
        },
    };
    send(&mut connection, first.request_id, welcome).await?;

    let Some(spawn_size) = interactive_size else {
        return control_loop(&mut connection, shared, exited, shutdown).await;
    };
    let mut attachment = leased.expect("interactive connection selected a tab view");
    let mut upload = crate::attachments::Upload::default();
    let attachment_store = shared.lock().await.attachment_store.clone();
    let mut prepared_extension_reload: Option<PreparedExtensionReload> = None;
    let mut alert_client_id = None;
    let mut alert_changes = None;

    let connection_result: Result<()> = async {
        loop {
            tokio::select! {
            frame = connection.next() => {
                let Some(frame) = frame else { break };
                let envelope: Envelope<ClientMessage> = decode_payload(&frame?)?;
                if reject_unnegotiated_method(&mut connection, &envelope).await? {
                    continue;
                }
                if let Some(operation) = fire_and_forget_operation(&envelope.message)
                    && reject_fire_and_forget_request_id(
                        &mut connection,
                        envelope.request_id,
                        operation,
                    ).await?
                {
                    if is_attachment_message(&envelope.message) {
                        upload.clear();
                    }
                    continue;
                }
                match envelope.message {
                    message @ (ClientMessage::AttachmentBegin { .. }
                    | ClientMessage::AttachmentChunk { .. }
                    | ClientMessage::AttachmentCommit { .. }
                    | ClientMessage::AttachmentCancel { .. }) => {
                        match receive_attachment(&shared, &attachment, &attachment_store, &mut upload, message).await {
                            Ok(true) => send(&mut connection, None, ServerMessage::CommandCompleted {
                                command: AcknowledgedCommand::Paste,
                            }).await?,
                            Ok(false) => {},
                            Err(error) => {
                                upload.clear();
                                send_error(&mut connection, None, "attachment_error", &error.message).await?;
                            }
                        }
                    }
                    ClientMessage::PrepareExtensionReload => {
                        if envelope.request_id.is_none() {
                            send_error(
                                &mut connection,
                                None,
                                "request_id_required",
                                "extension reload prepare requires a request ID",
                            ).await?;
                            continue;
                        }
                        match prepare_extension_reload(&shared).await {
                            Ok(prepared) => {
                                let base_generation = prepared.base_generation;
                                let catalog = prepared.catalog.clone();
                                prepared_extension_reload = Some(prepared);
                                send(
                                    &mut connection,
                                    envelope.request_id,
                                    ServerMessage::ExtensionReloadPrepared {
                                        base_generation,
                                        catalog,
                                    },
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
                    ClientMessage::CommitExtensionReload { base_generation } => {
                        if envelope.request_id.is_none() {
                            send_error(
                                &mut connection,
                                None,
                                "request_id_required",
                                "extension reload commit requires a request ID",
                            ).await?;
                            continue;
                        }
                        let result = match prepared_extension_reload.take() {
                            Some(prepared) => shared
                                .lock()
                                .await
                                .commit_extension_candidate(prepared, base_generation),
                            None => Err(DaemonError::new(
                                "extension_reload_not_prepared",
                                "prepare an extension reload on this connection before committing",
                            )),
                        };
                        match result {
                            Ok(reloaded) => send(
                                &mut connection,
                                envelope.request_id,
                                ServerMessage::ExtensionsReloaded {
                                    catalog: reloaded.catalog,
                                    changed: reloaded.changed,
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
                    ClientMessage::ReloadProjectConfig { session_id } => {
                        respond_to_project_config_reload(
                            &mut connection,
                            &shared,
                            envelope.request_id,
                            session_id,
                            true,
                        ).await?;
                    }
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
                    ClientMessage::KeyInput { event } => {
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
                                FocusedInput::Key(event),
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
                    ClientMessage::TerminalColors { terminal_id, colors } => {
                        if terminal_id != attachment.focused.selected.terminal_id {
                            continue;
                        }
                        if let Err(error) = attachment.set_focused_colors(colors).await {
                            respond_to_ui_event_error(
                                &mut connection,
                                envelope.request_id,
                                "terminal colors",
                                error,
                                UiEventPolicy::Disposable,
                            ).await?;
                        }
                    }
                    ClientMessage::Resize { terminal_id, size } => {
                        if let Err(error) = size.validate_within(connection.max_visible_cells()) {
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
                        )
                        .await
                        {
                            Ok(candidate) => {
                                if let Err(error) = attachment.close().await {
                                    send_command_error(&mut connection, envelope.request_id, error).await?;
                                    continue;
                                }
                                attachment = candidate;
                                presence
                                    .as_mut()
                                    .expect("interactive client has presence")
                                    .select(attachment.focused.selected.session_id);
                                send(
                                    &mut connection,
                                    envelope.request_id,
                                    ServerMessage::TargetSelected { selected: attachment.selected() },
                                ).await?;
                            }
                            Err(error) => send_error(&mut connection, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
                    ClientMessage::OpenLocation {
                        project,
                        name,
                        parent_workspace_id,
                        cwd,
                        program,
                        argv,
                    } => {
                        let (selected, disposition) = match recipe::open_location(
                            &shared,
                            &exited,
                            recipe::OpenLocationRequest {
                                project,
                                name,
                                parent_workspace_id,
                                cwd,
                                program,
                                argv,
                            },
                        ).await {
                            Ok(opened) => opened,
                            Err(error) => {
                                send_error(&mut connection, envelope.request_id, error.code, &error.message).await?;
                                continue;
                            }
                        };
                        let selector = TargetSelector::Terminal(selected.terminal_id);
                        let selection = match observe_selection(
                            &shared,
                            &selector,
                            None,
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
                        } = selection {
                            attachment.reconcile(
                                panes,
                                selected,
                                layout,
                                fallback_terminal_ids,
                                resource_revision,
                            );
                        } else {
                            let candidate = match switch_candidate(
                                &shared,
                                selector,
                                None,
                                client,
                            ).await {
                                Ok(candidate) => candidate,
                                Err(error) => {
                                    send_error(&mut connection, envelope.request_id, error.code, &error.message).await?;
                                    continue;
                                }
                            };
                            if let Err(error) = attachment.close().await {
                                send_command_error(&mut connection, envelope.request_id, error).await?;
                                continue;
                            }
                            attachment = candidate;
                            presence
                                .as_mut()
                                .expect("interactive client has presence")
                                .select(attachment.focused.selected.session_id);
                        }
                        send(
                            &mut connection,
                            envelope.request_id,
                            ServerMessage::LocationOpened {
                                selected,
                                disposition,
                            },
                        ).await?;
                        send(
                            &mut connection,
                            envelope.request_id,
                            ServerMessage::TargetSelected { selected: attachment.selected() },
                        ).await?;
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
                        let (snapshot, presence) = {
                            let state = shared.lock().await;
                            (state.resources.snapshot(), state.presence.snapshot())
                        };
                        send(&mut connection, envelope.request_id, ServerMessage::Resources { snapshot, presence }).await?;
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
                    ClientMessage::WatchAlerts { client_id } => {
                        if alert_client_id.is_some() {
                            send_error(&mut connection, envelope.request_id, "already_watching_alerts", "this connection already watches alerts").await?;
                            continue;
                        }
                        let snapshot = {
                            let mut state = shared.lock().await;
                            state.alerts.attach(client_id);
                            alert_changes = Some(state.alert_changes.subscribe());
                            state.alerts.snapshot(client_id)
                        };
                        alert_client_id = Some(client_id);
                        send(&mut connection, envelope.request_id, ServerMessage::AlertsChanged { snapshot }).await?;
                    }
                    ClientMessage::AcknowledgeAlerts { terminal_id, observed } => {
                        let Some(client_id) = alert_client_id else {
                            send_error(&mut connection, envelope.request_id, "alerts_not_watched", "watch alerts before acknowledging them").await?;
                            continue;
                        };
                        let result = {
                            let mut state = shared.lock().await;
                            let result = state.alerts.acknowledge(client_id, terminal_id, observed);
                            if matches!(result, Ok(true)) {
                                state.publish_alert_change();
                            }
                            result
                        };
                        match result {
                            Ok(_) => send(&mut connection, envelope.request_id, ServerMessage::CommandCompleted { command: AcknowledgedCommand::AcknowledgeAlerts }).await?,
                            Err(message) => send_error(&mut connection, envelope.request_id, "invalid_alert", message).await?,
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
                    ClientMessage::MoveWorkspace { workspace_id, position } => {
                        let result = {
                            let mut state = shared.lock().await;
                            match state.resources.session_id_for_workspace(workspace_id) {
                                Ok(id) if id == attachment.focused.selected.session_id => state.move_workspace(workspace_id, position),
                                Ok(_) => Err(DaemonError::new("outside_scope", "workspace is outside the active session")),
                                Err(error) => Err(error.into()),
                            }
                        };
                        match result {
                            Ok(resource_revision) => send(&mut connection, envelope.request_id,
                                ServerMessage::WorkspaceMoved { resource_revision }).await?,
                            Err(error) => send_error(&mut connection, envelope.request_id, error.code, &error.message).await?,
                        }
                    }
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
                    ClientMessage::OpenProject { .. } | ClientMessage::MovePane { .. } | ClientMessage::Contextual { .. } | ClientMessage::RetireWorkspace { .. } | ClientMessage::PublishToken { .. } | ClientMessage::ReloadExtensions | ClientMessage::ReportAgent { .. } | ClientMessage::ReportAutomaticAgent { .. } | ClientMessage::TerminalInput { .. } | ClientMessage::ReadTerminalOutput { .. } | ClientMessage::WaitTerminalOutput { .. } | ClientMessage::PromptAgent { .. } | ClientMessage::WaitAgent { .. } | ClientMessage::GetExtensionCatalog | ClientMessage::WatchResources | ClientMessage::Shutdown => send_error(&mut connection, envelope.request_id, "control_only", "command requires a control connection").await?,
                    ClientMessage::CommandDefaults { .. } | ClientMessage::CommandStart { .. } | ClientMessage::CommandOperation { .. } => send_error(&mut connection, envelope.request_id, "control_only", "commands require a dedicated control connection").await?,
                    ClientMessage::Hello { .. } | ClientMessage::RemoteHello(_) | ClientMessage::LocalHello(_) => send_error(&mut connection, envelope.request_id, "already_hello", "hello was already received").await?,
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
            changed = attachment.presence_changes.changed() => {
                if changed.is_err() {
                    break;
                }
                let presence = attachment.presence_changes.borrow_and_update().clone();
                send(
                    &mut connection,
                    None,
                    ServerMessage::PresenceChanged { presence },
                ).await?;
            }
            changed = watched_alert_change(&mut alert_changes) => {
                if changed.is_err() {
                    break;
                }
                if let Some(client_id) = alert_client_id {
                    let snapshot = shared.lock().await.alerts.snapshot(client_id);
                    send(&mut connection, None, ServerMessage::AlertsChanged { snapshot }).await?;
                }
            }
            changed = extension_changes.changed(), if connection.remote.is_none_or(|caps| caps.contains(Capability::ExtensionCatalog)) => {
                if changed.is_err() {
                    break;
                }
                let catalog = extension_changes.borrow_and_update().clone();
                send(
                    &mut connection,
                    None,
                    ServerMessage::ExtensionCatalogChanged { catalog },
                ).await?;
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
                                LeaseSize::Current,
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
                        presence
                            .as_mut()
                            .expect("interactive client has presence")
                            .select(attachment.focused.selected.session_id);
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
    if let Some(client_id) = alert_client_id {
        shared.lock().await.alerts.detach(client_id);
    }
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

/// Reject before dispatch, preserving the remote private-method restrictions
/// and the local baseline allowlist in interactive, control and command loops.
/// Legacy exact-version local peers retain the baseline, not optional methods.
async fn reject_unnegotiated_method(
    connection: &mut ClientConnection,
    envelope: &Envelope<ClientMessage>,
) -> Result<bool> {
    if connection
        .remote
        .is_some_and(|caps| !caps.allows_client(&envelope.message))
        || connection
            .local
            .is_some_and(|caps| !caps.allows_client(&envelope.message))
        || (connection.remote.is_none()
            && connection.local.is_none()
            && (matches!(envelope.message, ClientMessage::MoveWorkspace { .. })
                || envelope.message.requires_pane_titles()))
    {
        send(
            connection,
            envelope.request_id,
            ServerMessage::EndpointError {
                error: EndpointError::MethodNotNegotiated,
            },
        )
        .await?;
        return Ok(true);
    }
    Ok(false)
}

async fn control_loop(
    connection: &mut ClientConnection,
    shared: Shared,
    exited: mpsc::UnboundedSender<TerminalId>,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let mut watched_changes: Option<watch::Receiver<u64>> = None;
    let mut watched_presence: Option<watch::Receiver<ClientPresenceSnapshot>> = None;
    let mut watched_alerts: Option<watch::Receiver<u64>> = None;
    let mut alert_client_id = None;
    let mut retirement = None;
    let mut prepared_extension_reload: Option<PreparedExtensionReload> = None;
    let connection_result: Result<()> = async {
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
            changed = watched_presence_change(&mut watched_presence) => {
                if changed.is_err() {
                    break;
                }
                let presence = watched_presence
                    .as_mut()
                    .expect("presence receiver is installed")
                    .borrow_and_update()
                    .clone();
                send(connection, None, ServerMessage::PresenceChanged { presence }).await?;
                continue;
            }
            changed = watched_alert_change(&mut watched_alerts) => {
                if changed.is_err() {
                    break;
                }
                if let Some(client_id) = alert_client_id {
                    let snapshot = shared.lock().await.alerts.snapshot(client_id);
                    send(connection, None, ServerMessage::AlertsChanged { snapshot }).await?;
                }
                continue;
            }
        };
        let Some(frame) = frame else { break };
        let envelope: Envelope<ClientMessage> = decode_payload(&frame?)?;
        if reject_unnegotiated_method(connection, &envelope).await? {
            continue;
        }
        if let Some(operation) = fire_and_forget_operation(&envelope.message)
            && reject_fire_and_forget_request_id(connection, envelope.request_id, operation).await?
        {
            continue;
        }
        match envelope.message {
            ClientMessage::CommandDefaults { command, target } => {
                command::defaults(connection, &shared, envelope.request_id, command, target).await?;
            }
            ClientMessage::CommandStart { command, target, size, form } => {
                return command::serve(connection, &shared, envelope.request_id, command, target, size, form).await;
            }
            ClientMessage::CommandOperation { .. } => {
                command::reply(connection, envelope.request_id, crate::protocol::RemoteCommandReply::Error {
                    message: "start a command on a dedicated control connection first".into(),
                }).await?;
            }
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
            ClientMessage::GetExtensionCatalog => {
                let catalog = shared.lock().await.extension_catalog.borrow().clone();
                send(
                    connection,
                    envelope.request_id,
                    ServerMessage::ExtensionCatalog { catalog },
                )
                .await?;
            }
            ClientMessage::PrepareExtensionReload => {
                if envelope.request_id.is_none() {
                    send_error(
                        connection,
                        None,
                        "request_id_required",
                        "extension reload prepare requires a request ID",
                    ).await?;
                    continue;
                }
                match prepare_extension_reload(&shared).await {
                    Ok(prepared) => {
                        let base_generation = prepared.base_generation;
                        let catalog = prepared.catalog.clone();
                        prepared_extension_reload = Some(prepared);
                        send(
                            connection,
                            envelope.request_id,
                            ServerMessage::ExtensionReloadPrepared {
                                base_generation,
                                catalog,
                            },
                        ).await?;
                    }
                    Err(error) => {
                        send_error(connection, envelope.request_id, error.code, &error.message)
                            .await?;
                    }
                }
            }
            ClientMessage::CommitExtensionReload { base_generation } => {
                if envelope.request_id.is_none() {
                    send_error(
                        connection,
                        None,
                        "request_id_required",
                        "extension reload commit requires a request ID",
                    ).await?;
                    continue;
                }
                let result = match prepared_extension_reload.take() {
                    Some(prepared) => shared
                        .lock()
                        .await
                        .commit_extension_candidate(prepared, base_generation),
                    None => Err(DaemonError::new(
                        "extension_reload_not_prepared",
                        "prepare an extension reload on this connection before committing",
                    )),
                };
                match result {
                    Ok(reloaded) => send(
                        connection,
                        envelope.request_id,
                        ServerMessage::ExtensionsReloaded {
                            catalog: reloaded.catalog,
                            changed: reloaded.changed,
                        },
                    ).await?,
                    Err(error) => {
                        send_error(connection, envelope.request_id, error.code, &error.message)
                            .await?;
                    }
                }
            }
            ClientMessage::ReloadProjectConfig { session_id } => {
                respond_to_project_config_reload(
                    connection,
                    &shared,
                    envelope.request_id,
                    session_id,
                    false,
                ).await?;
            }
            ClientMessage::ReloadExtensions => match reload_extensions(&shared).await {
                Ok(reloaded) => {
                    send(
                        connection,
                        envelope.request_id,
                        ServerMessage::ExtensionsReloaded {
                            catalog: reloaded.catalog,
                            changed: reloaded.changed,
                        },
                    )
                    .await?
                }
                Err(error) => {
                    send_error(connection, envelope.request_id, error.code, &error.message).await?
                }
            },
            ClientMessage::OpenProject { project } => {
                match recipe::open_project(&shared, &exited, project).await {
                    Ok((selected, disposition)) => send(connection, envelope.request_id,
                        ServerMessage::LocationOpened { selected, disposition }).await?,
                    Err(error) => send_error(connection, envelope.request_id, error.code, &error.message).await?,
                }
            }
            ClientMessage::OpenLocation {
                project,
                name,
                parent_workspace_id,
                cwd,
                program,
                argv,
            } => match recipe::open_location(
                &shared,
                &exited,
                recipe::OpenLocationRequest {
                    project,
                    name,
                    parent_workspace_id,
                    cwd,
                    program,
                    argv,
                },
            )
            .await
            {
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
            ClientMessage::MoveWorkspace { workspace_id, position } => {
                let result = shared.lock().await.move_workspace(workspace_id, position);
                match result {
                    Ok(resource_revision) => send(connection, envelope.request_id,
                        ServerMessage::WorkspaceMoved { resource_revision }).await?,
                    Err(error) => send_error(connection, envelope.request_id, error.code, &error.message).await?,
                }
            }
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
                action,
            } => {
                let result = shared
                    .lock()
                    .await
                    .publish_token(&extension_id, &token, value, target, action);
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
                let (snapshot, presence) = {
                    let state = shared.lock().await;
                    (state.resources.snapshot(), state.presence.snapshot())
                };
                send(
                    connection,
                    envelope.request_id,
                    ServerMessage::Resources { snapshot, presence },
                )
                .await?;
            }
            ClientMessage::WatchResources => {
                // Subscribing and snapshotting under one lock leaves no gap in
                // which a change could go unstreamed.
                let (snapshot, presence) = {
                    let state = shared.lock().await;
                    watched_changes = Some(state.resource_changes.subscribe());
                    watched_presence = Some(state.presence.subscribe());
                    (state.resources.snapshot(), state.presence.snapshot())
                };
                send(
                    connection,
                    envelope.request_id,
                    ServerMessage::Resources { snapshot, presence },
                )
                .await?;
            }
            ClientMessage::WatchAlerts { client_id } => {
                if alert_client_id.is_some() {
                    send_error(
                        connection,
                        envelope.request_id,
                        "already_watching_alerts",
                        "this connection already watches alerts",
                    )
                    .await?;
                    continue;
                }
                let snapshot = {
                    let mut state = shared.lock().await;
                    state.alerts.attach(client_id);
                    watched_alerts = Some(state.alert_changes.subscribe());
                    state.alerts.snapshot(client_id)
                };
                alert_client_id = Some(client_id);
                send(
                    connection,
                    envelope.request_id,
                    ServerMessage::AlertsChanged { snapshot },
                )
                .await?;
            }
            ClientMessage::AcknowledgeAlerts {
                terminal_id,
                observed,
            } => {
                let Some(client_id) = alert_client_id else {
                    send_error(
                        connection,
                        envelope.request_id,
                        "alerts_not_watched",
                        "watch alerts before acknowledging them",
                    )
                    .await?;
                    continue;
                };
                let result = {
                    let mut state = shared.lock().await;
                    let result = state.alerts.acknowledge(client_id, terminal_id, observed);
                    if matches!(result, Ok(true)) {
                        state.publish_alert_change();
                    }
                    result
                };
                match result {
                    Ok(_) => {
                        send(
                            connection,
                            envelope.request_id,
                            ServerMessage::CommandCompleted {
                                command: AcknowledgedCommand::AcknowledgeAlerts,
                            },
                        )
                        .await?
                    }
                    Err(message) => {
                        send_error(
                            connection,
                            envelope.request_id,
                            "invalid_alert",
                            message,
                        )
                        .await?
                    }
                }
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
                source,
            } => {
                let result = shared.lock().await.report_external_agent(
                    terminal_id,
                    report,
                    source,
                );
                send_agent_report_result(connection, envelope.request_id, result).await?;
            }
            ClientMessage::ReportAutomaticAgent {
                terminal_id,
                report,
                source,
            } => {
                let agent_pid = shared
                    .lock()
                    .await
                    .automatic_agent_owner(terminal_id, connection.peer_pid);
                let Some(agent_pid) = agent_pid else {
                    send_error(connection, envelope.request_id, "invalid_agent_context",
                        "automatic agent reports must originate in the target terminal; external controllers must pass --terminal-id explicitly").await?;
                    continue;
                };
                let result = shared
                    .lock()
                    .await
                    .report_automatic_agent(terminal_id, report, source, agent_pid);
                send_agent_report_result(connection, envelope.request_id, result).await?;
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
                mode,
            } => match prompt_agent(&shared, terminal_id, text, mode).await {
                Ok(result) => {
                    let message = match result.status {
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
                            activity: result.status.expect("agent wait always returns status"),
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
            | ClientMessage::ResizeSplit { .. }
            | ClientMessage::TerminalColors { .. } => {}
            ClientMessage::CreateWorkspace { .. }
            | ClientMessage::Input { .. }
            | ClientMessage::KeyInput { .. }
            | ClientMessage::Paste { .. }
            | ClientMessage::AttachmentBegin { .. }
            | ClientMessage::AttachmentChunk { .. }
            | ClientMessage::AttachmentCommit { .. }
            | ClientMessage::AttachmentCancel { .. }
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
            ClientMessage::Hello { .. } | ClientMessage::RemoteHello(_) | ClientMessage::LocalHello(_) => {
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
    .await;
    if let Some(client_id) = alert_client_id {
        shared.lock().await.alerts.detach(client_id);
    }
    connection_result
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
        ContextualCommand::RenamePane { name } => {
            let result = {
                let mut state = shared.lock().await;
                state
                    .validate_terminal_context(context)
                    .and_then(|_| state.rename_target(RenameSelector::Pane(context.pane_id), name))
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

async fn watched_alert_change(
    changes: &mut Option<watch::Receiver<u64>>,
) -> Result<(), tokio::sync::watch::error::RecvError> {
    match changes {
        Some(changes) => changes.changed().await,
        None => std::future::pending().await,
    }
}

async fn watched_presence_change(
    changes: &mut Option<watch::Receiver<ClientPresenceSnapshot>>,
) -> Result<(), tokio::sync::watch::error::RecvError> {
    match changes {
        Some(changes) => changes.changed().await,
        None => std::future::pending().await,
    }
}

/// Geometry for the focused terminal's new lease.
#[derive(Clone, Copy)]
enum LeaseSize {
    /// The size the client requested when it connected.
    Requested(TerminalSize),
    /// The terminal's current size, kept until the client reports the size
    /// of the pane it now shows. Another pane's size would resize the PTY
    /// twice when the panes differ: the program gets two SIGWINCHes and the
    /// client may show one frame rendered at the wrong size.
    Current,
}

async fn lease_view(
    shared: &Shared,
    selector: Option<TargetSelector>,
    expected: Option<&SelectionExpectation>,
    client: ClientId,
    size: LeaseSize,
) -> Result<Attachment, DaemonError> {
    let state = shared.lock().await;
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
    let size = match size {
        LeaseSize::Requested(size) => size,
        LeaseSize::Current => focused_runtime.handle.subscribe_snapshots().borrow().size,
    };
    let acquisition = focused_runtime
        .lease
        .acquire(client, size, Arc::clone(&focused_runtime.handle))
        .ok_or_else(|| DaemonError::new("attachment_error", "client already attached"))?;
    let focused_target = LeasedTarget {
        selected: selected_target(focused, &focused_runtime.handle),
        terminal: Arc::clone(&focused_runtime.handle),
        lease: acquisition.guard,
        pending_configuration: Some(acquisition.configuration),
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
    let resource_changes = state.resource_changes.subscribe();
    let presence_changes = state.presence.subscribe();
    let mut attachment = Attachment::new(
        AttachmentInit {
            owner: client,
            panes,
            focused: focused_target,
            layout,
            fallback_terminal_ids,
            resource_revision: state.resources.revision(),
        },
        resource_changes,
        presence_changes,
    );
    drop(state);
    attachment.focused.apply_acquisition_configuration().await?;
    Ok(attachment)
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
        RenameSelector::Session(target) => {
            interactive_session_allowed(resources, session_id, target)
        }
        RenameSelector::Workspace(target) => resources
            .session_id_for_workspace(*target)
            .is_ok_and(|owner| owner == session_id),
        RenameSelector::Tab(target) => resources
            .workspace_id_for_tab(*target)
            .is_ok_and(|owner| owner == workspace_id),
        RenameSelector::Pane(target) => resources
            .workspace_id_for_pane(*target)
            .is_ok_and(|owner| owner == workspace_id),
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
        TargetSelector::Session(target) => {
            interactive_session_allowed(resources, focused.session_id, target)
        }
        TargetSelector::Terminal(_) => false,
    }
}

fn interactive_session_allowed(
    resources: &ResourceTree,
    attached_session_id: SessionId,
    target: &crate::resources::SessionSelector,
) -> bool {
    resources
        .resolve_session(target.clone())
        .is_ok_and(|target| target == attached_session_id)
}

async fn focus_leased_attachment(
    shared: &Shared,
    attachment: &mut Attachment,
    mut focused: LeasedTarget,
) -> Result<(), DaemonError> {
    focused.apply_acquisition_configuration().await?;
    attachment
        .clear_active_copy_mode()
        .await
        .map_err(|error| DaemonError::new(command_error_code(&error), error.to_string()))?;
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
) -> Result<Attachment, DaemonError> {
    let attachment =
        lease_view(shared, Some(selector), expected, client, LeaseSize::Current).await?;
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

async fn open_location_without_recipe(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    name: Option<String>,
    parent_workspace_id: Option<WorkspaceId>,
    resolved: ResolvedLocation,
    program: Option<PathBuf>,
    argv: Vec<String>,
) -> Result<(SelectedTarget, OpenDisposition), DaemonError> {
    let (terminal, selected, disposition, insertion_error) = {
        let mut state = shared.lock().await;
        let program = program.unwrap_or_else(|| state.default_shell.clone());
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        let mut destination = state
            .resources
            .checkout_destination(&resolved.project, &resolved.workspace_root)?;
        let mut replacing = None;
        let mut planned_resources = None;
        if let CheckoutDestination::Existing(workspace_id) = destination {
            if parent_workspace_id.is_some() {
                return Err(DaemonError::new(
                    "parent_workspace_requires_creation",
                    "--parent-workspace can only be used when creating a new workspace",
                ));
            }
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
                // Plan against a clone first. The live tree and daemon
                // acceptance state remain untouched until any replacement
                // process has spawned successfully.
                let mut planned = state.resources.clone();
                planned.terminal_exited(path.terminal_id)?;
                destination =
                    planned.checkout_destination(&resolved.project, &resolved.workspace_root)?;
                if let CheckoutDestination::Existing(workspace_id) = destination {
                    state.finalize_terminal_for_replacement(path.terminal_id)?;
                    state.accepting = true;
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
                replacing = Some(path.terminal_id);
                planned_resources = Some(planned);
            } else {
                let runtime = &state.runtimes[&path.terminal_id];
                return Ok((
                    selected_target(path, &runtime.handle),
                    OpenDisposition::Existing,
                ));
            }
        }
        let resources = planned_resources.as_ref().unwrap_or(&state.resources);
        let session_name = name
            .clone()
            .unwrap_or_else(|| resources.available_session_name(&resolved.suggested_session_name));
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
            parent_workspace_id,
            workspace_name,
            root: resolved.workspace_root.clone(),
            tab_id: TabId::new(),
            tab_name: String::new(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let mut validated = resources.clone();
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
                terminal: state.terminal_config.clone(),
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
        let commit = replacing
            .map(|terminal_id| state.finalize_terminal_for_replacement(terminal_id))
            .transpose();
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
                let insertion = commit
                    .and_then(|_| state.register_session(path.clone(), Arc::clone(&terminal)));
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
                let insertion = commit.and_then(|_| {
                    state.register_workspace(session_id, path.clone(), Arc::clone(&terminal))
                });
                (selected, OpenDisposition::WorkspaceCreated, insertion)
            }
            CheckoutDestination::Existing(_) => unreachable!(),
        };
        if replacing.is_some() && insertion.is_ok() {
            // A successful replacement keeps the daemon live even when the
            // finalized terminal was the last resource in the old tree.
            state.accepting = true;
        }
        let selected = selected_target(path, &terminal);
        (terminal, selected, disposition, insertion.err())
    };
    if let Some(error) = insertion_error {
        let _ = terminal.close().await;
        return Err(error);
    }
    watch_terminal(terminal, Arc::clone(shared), exited.clone());
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
    let (terminal, creation) = {
        let mut state = shared.lock().await;
        let program = program.unwrap_or_else(|| state.default_shell.clone());
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
            parent_workspace_id: None,
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
                terminal: state.terminal_config.clone(),
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
                    let acquisition = runtime
                        .lease
                        .acquire(client, size, Arc::clone(&terminal))
                        .expect("new terminal has an independent lease");
                    CreatedTerminal::Attached(LeasedTarget {
                        selected,
                        terminal: Arc::clone(&terminal),
                        lease: acquisition.guard,
                        pending_configuration: Some(acquisition.configuration),
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
    watch_terminal(terminal, Arc::clone(shared), exited.clone());
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
    let (terminal, creation) = {
        let mut state = shared.lock().await;
        let program = program.unwrap_or_else(|| state.default_shell.clone());
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
                terminal: state.terminal_config.clone(),
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
                    let acquisition = runtime
                        .lease
                        .acquire(client, size, Arc::clone(&terminal))
                        .expect("new terminal has an independent lease");
                    let target = LeasedTarget {
                        selected,
                        terminal: Arc::clone(&terminal),
                        lease: acquisition.guard,
                        pending_configuration: Some(acquisition.configuration),
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
    watch_terminal(terminal, Arc::clone(shared), exited.clone());
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
    let (terminal, creation) = {
        let mut state = shared.lock().await;
        let program = program.unwrap_or_else(|| state.default_shell.clone());
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
                terminal: state.terminal_config.clone(),
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
                    let acquisition = runtime
                        .lease
                        .acquire(client, size, Arc::clone(&terminal))
                        .expect("new terminal has an independent lease");
                    CreatedTerminal::Attached(LeasedTarget {
                        selected,
                        terminal: Arc::clone(&terminal),
                        lease: acquisition.guard,
                        pending_configuration: Some(acquisition.configuration),
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
    watch_terminal(terminal, Arc::clone(shared), exited.clone());
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
    let total = pending.handles.len();
    for (closed, handle) in pending.handles.into_iter().enumerate() {
        if let Err(error) = handle.close().await {
            let mut state = shared.lock().await;
            return Err(state.fail_target_close(pending.scope, closed, total, error));
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
    Key(crate::domain::TerminalKeyEvent),
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

#[derive(Debug)]
struct AgentCommandResult {
    barrier_revision: u64,
    working_revision: Option<u64>,
    status: Option<ProgramStatus>,
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
    mode: AgentPromptMode,
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
    let timeout_ms = match mode {
        AgentPromptMode::Submit => None,
        AgentPromptMode::Wait { timeout_ms } => {
            validate_agent_timeout(timeout_ms)?;
            Some(timeout_ms)
        }
    };

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
        let status = state.resources.program_status(terminal_id)?;
        if !status.is_reporting() {
            return Err(DaemonError::new(
                "not_an_agent",
                format!("terminal {terminal_id} has no agent integration"),
            ));
        }
        if status
            .records
            .iter()
            .any(|record| record.state == StatusState::Working)
        {
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
            status.revision,
        )
    };

    terminal
        .paste_and_input(text, vec![b'\r'])
        .await
        .map_err(agent_input_error)?;
    let Some(timeout_ms) = timeout_ms else {
        return Ok(AgentCommandResult {
            barrier_revision,
            working_revision: None,
            status: None,
        });
    };
    wait_for_agent_events(
        shared,
        terminal_id,
        AgentWaitSelection::fresh(barrier_revision),
        timeout_ms,
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
    let (status, events, mut lifecycle) = {
        let state = shared.lock().await;
        state
            .resources
            .resolve_terminal_target(Some(TargetSelector::Terminal(terminal_id)))?;
        let status = state.resources.program_status(terminal_id)?.clone();
        if !status.is_reporting() {
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
            status,
            state.agent_events.subscribe(),
            runtime.handle.subscribe_lifecycle(),
        )
    };
    let Some(selection) = AgentWaitSelection::current(&status) else {
        return Ok(AgentCommandResult {
            barrier_revision: status.revision,
            working_revision: None,
            status: Some(status),
        });
    };
    wait_for_agent_events(
        shared,
        terminal_id,
        selection,
        timeout_ms,
        events,
        &mut lifecycle,
    )
    .await
}

#[derive(Debug)]
struct AgentWaitSelection {
    barrier_revision: u64,
    selected: HashSet<String>,
    settled: HashSet<String>,
    working_revision: Option<u64>,
}

#[derive(Debug)]
enum AgentWaitObservation {
    Pending,
    Settled(ProgramStatus),
    Exited { terminal: bool },
}

impl AgentWaitSelection {
    fn fresh(barrier_revision: u64) -> Self {
        Self {
            barrier_revision,
            selected: HashSet::new(),
            settled: HashSet::new(),
            working_revision: None,
        }
    }

    fn current(status: &ProgramStatus) -> Option<Self> {
        let working = status
            .records
            .iter()
            .filter(|record| record.state == StatusState::Working)
            .collect::<Vec<_>>();
        if working.is_empty() {
            return None;
        }
        let selected = if working.iter().any(|record| record.id.is_empty()) {
            HashSet::from([String::new()])
        } else {
            working.iter().map(|record| record.id.clone()).collect()
        };
        let working_revision = working
            .iter()
            .filter(|record| selected.contains(&record.id))
            .map(|record| record.revision)
            .max();
        Some(Self {
            barrier_revision: status.revision,
            selected,
            settled: HashSet::new(),
            working_revision,
        })
    }

    fn observe(&mut self, update: &StatusLifecycleUpdate) -> AgentWaitObservation {
        if update.status.revision <= self.barrier_revision {
            return AgentWaitObservation::Pending;
        }

        let started = update
            .transitions
            .iter()
            .filter_map(|transition| match transition {
                StatusRecordLifecycle::Working(id) => Some(id.clone()),
                StatusRecordLifecycle::Settled(_) | StatusRecordLifecycle::Removed(_) => None,
            })
            .collect::<HashSet<_>>();
        if self.selected.is_empty() && !started.is_empty() {
            self.selected = if started.contains("") {
                HashSet::from([String::new()])
            } else {
                started
            };
        }
        if !self.selected.is_empty()
            && update.transitions.iter().any(|transition| {
                matches!(transition, StatusRecordLifecycle::Working(id) if self.selected.contains(id))
            })
        {
            self.working_revision = Some(
                self.working_revision
                    .map_or(update.status.revision, |revision| revision.max(update.status.revision)),
            );
        }

        for transition in &update.transitions {
            match transition {
                StatusRecordLifecycle::Working(id) if self.selected.contains(id) => {
                    self.settled.remove(id);
                }
                StatusRecordLifecycle::Settled(id) if self.selected.contains(id) => {
                    self.settled.insert(id.clone());
                }
                StatusRecordLifecycle::Removed(id) if self.selected.contains(id) => {
                    return AgentWaitObservation::Exited {
                        terminal: update.terminal_process_exit,
                    };
                }
                StatusRecordLifecycle::Working(_)
                | StatusRecordLifecycle::Settled(_)
                | StatusRecordLifecycle::Removed(_) => {}
            }
        }
        self.settled_status(&update.status)
            .map_or(AgentWaitObservation::Pending, AgentWaitObservation::Settled)
    }

    fn settled_status(&self, status: &ProgramStatus) -> Option<ProgramStatus> {
        if self.selected.is_empty() || self.settled.len() != self.selected.len() {
            return None;
        }
        if self.selected.iter().any(|id| {
            status
                .records
                .iter()
                .find(|record| record.id == *id)
                .is_none_or(|record| record.state == StatusState::Working)
        }) {
            return None;
        }
        Some(self.result_status(status))
    }

    fn settled_status_at_terminal_exit(&self, status: &ProgramStatus) -> Option<ProgramStatus> {
        let working_revision = self.working_revision?;
        if self.selected.is_empty()
            || self.selected.iter().any(|id| {
                status
                    .records
                    .iter()
                    .find(|record| record.id == *id)
                    .is_none_or(|record| {
                        record.state == StatusState::Working || record.revision < working_revision
                    })
            })
        {
            return None;
        }
        Some(self.result_status(status))
    }

    fn result_status(&self, status: &ProgramStatus) -> ProgramStatus {
        let mut result = status.clone();
        result
            .records
            .retain(|record| self.selected.contains(&record.id));
        result
    }
}

async fn wait_for_agent_events(
    shared: &Shared,
    terminal_id: TerminalId,
    mut selection: AgentWaitSelection,
    timeout_ms: u64,
    mut events: broadcast::Receiver<StatusLifecycleUpdate>,
    lifecycle: &mut watch::Receiver<TerminalLifecycle>,
) -> Result<AgentCommandResult, DaemonError> {
    let barrier_revision = selection.barrier_revision;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let current_lifecycle = lifecycle.borrow().clone();
        if let TerminalLifecycle::Exited { exit_code } = current_lifecycle {
            loop {
                match events.try_recv() {
                    Ok(update) => {
                        if update.terminal_id != terminal_id {
                            continue;
                        }
                        match selection.observe(&update) {
                            AgentWaitObservation::Pending => {}
                            AgentWaitObservation::Settled(status) => {
                                return Ok(AgentCommandResult {
                                    barrier_revision,
                                    working_revision: selection.working_revision,
                                    status: Some(status),
                                });
                            }
                            AgentWaitObservation::Exited { terminal: false } => {
                                return Err(agent_exited_error(terminal_id));
                            }
                            AgentWaitObservation::Exited { terminal: true } => {
                                return Err(terminal_agent_exit_error(terminal_id));
                            }
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                        return Err(DaemonError::new(
                            "agent_events_lagged",
                            format!(
                                "agent status receiver lagged by {skipped} updates; retry from a fresh state"
                            ),
                        ));
                    }
                    Err(broadcast::error::TryRecvError::Closed) => {
                        return Err(DaemonError::new(
                            "agent_events_closed",
                            "agent status stream closed",
                        ));
                    }
                }
            }
            let current = shared
                .lock()
                .await
                .resources
                .program_status(terminal_id)
                .cloned();
            if let Ok(status) = current
                && let Some(status) = selection.settled_status_at_terminal_exit(&status)
            {
                return Ok(AgentCommandResult {
                    barrier_revision,
                    working_revision: selection.working_revision,
                    status: Some(status),
                });
            }
            return Err(DaemonError::new(
                "terminal_exited",
                format!("terminal exited with status {exit_code:?} before the agent settled"),
            ));
        }
        tokio::select! {
            biased;
            update = events.recv() => match update {
                Ok(update) if update.terminal_id != terminal_id => {}
                Ok(update) => match selection.observe(&update) {
                    AgentWaitObservation::Pending => {}
                    AgentWaitObservation::Settled(status) => {
                        return Ok(AgentCommandResult {
                            barrier_revision,
                            working_revision: selection.working_revision,
                            status: Some(status),
                        });
                    }
                    AgentWaitObservation::Exited { terminal: false } => {
                        return Err(agent_exited_error(terminal_id));
                    }
                    AgentWaitObservation::Exited { terminal: true } => {
                        return Err(terminal_agent_exit_error(terminal_id));
                    }
                },
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    return Err(DaemonError::new(
                        "agent_events_lagged",
                        format!("agent status receiver lagged by {skipped} updates; retry from a fresh state"),
                    ));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(DaemonError::new("agent_events_closed", "agent status stream closed"));
                }
            },
            changed = lifecycle.changed() => {
                if changed.is_err() {
                    return Err(DaemonError::new("terminal_exited", "terminal lifecycle stream closed"));
                }
                // The loop head inspects the current value. This also handles
                // receivers first subscribed after the lifecycle reached Exited.
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

fn is_attachment_message(message: &ClientMessage) -> bool {
    matches!(
        message,
        ClientMessage::AttachmentBegin { .. }
            | ClientMessage::AttachmentChunk { .. }
            | ClientMessage::AttachmentCommit { .. }
            | ClientMessage::AttachmentCancel { .. }
    )
}

async fn receive_attachment(
    shared: &Shared,
    attachment: &Attachment,
    store: &crate::attachments::Store,
    upload: &mut crate::attachments::Upload,
    message: ClientMessage,
) -> Result<bool, DaemonError> {
    let upload_error =
        |error: anyhow::Error| DaemonError::new("attachment_error", error.to_string());
    match message {
        ClientMessage::AttachmentBegin {
            transfer_id,
            terminal_id,
            size,
        } => {
            upload.clear();
            {
                // Clipboard acquisition is asynchronous: the client may have
                // selected another tab before Begin arrives. Authorize the
                // explicit daemon-local target, not the current attachment view.
                let state = shared.lock().await;
                state
                    .resources
                    .resolve_terminal_target(Some(TargetSelector::Terminal(terminal_id)))?;
                if !state.accepting || !state.runtimes.contains_key(&terminal_id) {
                    return Err(DaemonError::new(
                        "attachment_error",
                        "attachment target is not running",
                    ));
                }
            }
            if attachment.owns_copy_mode(terminal_id) {
                return Err(DaemonError::new(
                    "copy_mode_active",
                    "terminal paste is disabled while copy mode is active",
                ));
            }
            upload
                .begin(store, transfer_id, terminal_id, size)
                .map_err(upload_error)?;
        }
        ClientMessage::AttachmentChunk { transfer_id, bytes } => {
            upload.chunk(transfer_id, bytes).map_err(upload_error)?;
        }
        ClientMessage::AttachmentCancel { transfer_id } => {
            upload.cancel(transfer_id).map_err(upload_error)?;
        }
        ClientMessage::AttachmentCommit { transfer_id } => {
            let (terminal_id, file) = upload.commit(transfer_id).await.map_err(upload_error)?;
            if attachment.owns_copy_mode(terminal_id) {
                return Err(DaemonError::new(
                    "copy_mode_active",
                    "terminal paste is disabled while copy mode is active",
                ));
            }
            let text = file.quoted_path().map_err(upload_error)?;
            targeted_terminal_input(shared, terminal_id, TerminalInputOperation::Text { text })
                .await?;
            store.retain(file);
            return Ok(true);
        }
        _ => unreachable!("only attachment messages reach the upload handler"),
    }
    Ok(false)
}

fn agent_exited_error(terminal_id: TerminalId) -> DaemonError {
    DaemonError::new(
        "agent_exited",
        format!("agent {terminal_id} stopped reporting before it settled"),
    )
}

fn terminal_agent_exit_error(terminal_id: TerminalId) -> DaemonError {
    DaemonError::new(
        "terminal_exited",
        format!("terminal {terminal_id} exited before the agent settled"),
    )
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
        TerminalInputOperation::Keys { events } => {
            let mut result = Ok(());
            for event in events {
                if let Err(error) = terminal.key_input(event).await {
                    result = Err(error);
                    break;
                }
            }
            result
        }
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
        FocusedInput::Key(event) => attachment.focused_terminal().key_input(event).await,
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
        // Input the program is not reading is lost either way; replying would
        // end an attached client, which has no request waiting for it.
        (UiEventPolicy::Input, CommandError::Stopped | CommandError::InputBacklogFull)
        | (
            UiEventPolicy::Disposable,
            CommandError::Busy | CommandError::Stopped | CommandError::InputBacklogFull,
        ) => UiEventErrorDisposition::DropTransient,
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

async fn send_agent_report_result(
    connection: &mut ClientConnection,
    request_id: Option<Uuid>,
    result: Result<(), DaemonError>,
) -> Result<()> {
    match result {
        Ok(()) => {
            send(
                connection,
                request_id,
                ServerMessage::CommandCompleted {
                    command: AcknowledgedCommand::ReportAgent,
                },
            )
            .await
        }
        Err(error) => send_error(connection, request_id, error.code, &error.message).await,
    }
}

fn fire_and_forget_operation(message: &ClientMessage) -> Option<&'static str> {
    match message {
        message if is_attachment_message(message) => Some("attachment upload"),
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
        CommandError::InputBacklogFull => "input_backlog_full",
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
    mut message: ServerMessage,
) -> Result<()> {
    if connection
        .remote
        .is_some_and(|caps| !caps.contains(Capability::NestedWorkspaces))
    {
        match &mut message {
            ServerMessage::Resources { snapshot, .. }
            | ServerMessage::ResourcesChanged { snapshot } => snapshot.clear_workspace_parents(),
            _ => {}
        }
    }
    let pane_titles = connection.local.is_some_and(|caps| caps.pane_titles())
        || connection
            .remote
            .is_some_and(|caps| caps.contains(Capability::PaneTitles));
    if !pane_titles {
        match &mut message {
            ServerMessage::Resources { snapshot, .. }
            | ServerMessage::ResourcesChanged { snapshot } => snapshot.clear_pane_names(),
            _ => {}
        }
    }
    if connection
        .remote
        .is_some_and(|caps| !caps.allows_server(&message))
        && !matches!(message, ServerMessage::RemoteWelcome(_))
        || connection
            .local
            .is_some_and(|caps| !caps.allows_server(&message))
    {
        if request_id.is_none() {
            return Ok(());
        }
        message = ServerMessage::EndpointError {
            error: EndpointError::MethodNotNegotiated,
        };
    }
    if matches!(
        message,
        ServerMessage::CopyModeSnapshot { .. }
            | ServerMessage::CopyModeFinalized { .. }
            | ServerMessage::CopyModeCancelled { .. }
            | ServerMessage::TargetSelected { .. }
            | ServerMessage::CommandReply {
                reply: crate::protocol::RemoteCommandReply::Screen { .. }
            }
    ) {
        return connection.enqueue(Outbound::Message {
            request_id,
            message: Box::new(message),
        });
    }
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

    #[test]
    fn shell_environment_preserves_unix_bytes_and_the_exact_socket_path() {
        use std::os::unix::ffi::OsStringExt;

        let invalid_value = OsString::from_vec(vec![b'x', 0xff]);
        let socket = PathBuf::from(OsString::from_vec(b"/tmp/fut-\xff.sock".to_vec()));
        let environment = shell_environment(
            &socket,
            [(OsString::from("NON_UNICODE"), invalid_value.clone())],
        );

        assert_eq!(
            environment.get(std::ffi::OsStr::new("NON_UNICODE")),
            Some(&invalid_value)
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("FUT_SOCKET")),
            Some(&socket.into_os_string())
        );
    }

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
        let message = snapshot_message(terminal_id, first, false, true, &mut last_sent);
        assert!(matches!(message, ServerMessage::Snapshot { .. }));

        let mut second_cells = cells;
        second_cells[0].contents = "x".into();
        let second = ScreenSnapshot::new(2, size, second_cells.clone(), cursor).unwrap();
        let message = snapshot_message(terminal_id, second, false, true, &mut last_sent);
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
        let message = snapshot_message(terminal_id, third, false, true, &mut last_sent);
        let ServerMessage::SnapshotDelta { delta, .. } = message else {
            panic!("cursor-only change should produce a delta")
        };
        assert!(delta.rows.is_empty());
        assert_eq!(delta.cursor.shape, CursorShape::Bar);
        assert!(delta.cursor.blinking);
        assert!(delta.mouse_tracking);
        assert_eq!(delta.graphics.unwrap().placements.len(), 1);
    }

    /// Deterministic xorshift, so a failing seed reproduces.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % bound as u64) as usize
        }
    }

    /// One random burst of terminal output: scrolling text, scroll regions,
    /// explicit scrolls in both directions, reverse index, line insertion and
    /// deletion, overwrites, erases, styling, hyperlinks, and Kitty images
    /// whose placements scroll with the text.
    fn random_output(rng: &mut Rng, size: TerminalSize) -> Vec<u8> {
        let rows = usize::from(size.rows);
        let columns = usize::from(size.columns);
        let mut out = String::new();
        let text = |rng: &mut Rng, out: &mut String| {
            let words = ["fut", "yes", "build", "", "x", "compiling", "≈", "界"];
            for _ in 0..rng.below(6) {
                if rng.below(4) == 0 {
                    out.push_str(&format!("\x1b[3{}m", rng.below(8)));
                }
                out.push_str(words[rng.below(words.len())]);
                out.push(' ');
                if rng.below(5) == 0 {
                    out.push_str("\x1b[0m");
                }
            }
        };
        for _ in 0..1 + rng.below(3) {
            match rng.below(13) {
                0..=3 => {
                    for _ in 0..1 + rng.below(rows + rows / 2) {
                        text(rng, &mut out);
                        out.push_str("\r\n");
                    }
                }
                4 => {
                    let top = 1 + rng.below(rows);
                    let bottom = top + rng.below(rows + 1 - top);
                    out.push_str(&format!("\x1b[{top};{bottom}r"));
                }
                5 => out.push_str("\x1b[r"),
                6 => out.push_str(&format!("\x1b[{}S", 1 + rng.below(rows))),
                7 => out.push_str(&format!("\x1b[{}T", 1 + rng.below(rows))),
                8 => {
                    out.push_str(&format!("\x1b[{};1H", 1 + rng.below(rows)));
                    for _ in 0..1 + rng.below(rows) {
                        out.push_str("\x1bM\r");
                        text(rng, &mut out);
                    }
                }
                9 => {
                    let operation = ["L", "M"][rng.below(2)];
                    out.push_str(&format!(
                        "\x1b[{};1H\x1b[{}{operation}",
                        1 + rng.below(rows),
                        1 + rng.below(rows)
                    ));
                }
                10 => {
                    out.push_str(&format!(
                        "\x1b[{};{}H",
                        1 + rng.below(rows),
                        1 + rng.below(columns)
                    ));
                    if rng.below(3) == 0 {
                        out.push_str("\x1b]8;;https://example.com/a\x1b\\link\x1b]8;;\x1b\\");
                    }
                    text(rng, &mut out);
                    out.push_str(["", "\x1b[K", "\x1b[1J", "\x1b[2K"][rng.below(4)]);
                }
                11 => {
                    // A 1x1 PNG placed over a few cells at the cursor.
                    out.push_str(&format!(
                        "\x1b_Ga=T,f=100,q=2,c={},r={},i={};\
                         iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAA\
                         DUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\",
                        1 + rng.below(3),
                        1 + rng.below(3),
                        1 + rng.below(3)
                    ));
                }
                _ => {
                    if rng.below(8) == 0 {
                        out.push_str(["\x1b[2J", "\x1b[?1049h", "\x1b[?1049l"][rng.below(3)]);
                    }
                    text(rng, &mut out);
                }
            }
        }
        out.into_bytes()
    }

    /// Send `message` the way the writer does, split into chunks of at most
    /// `max_cells` cells, encode and decode every frame, and reassemble.
    fn over_the_wire(
        message: ServerMessage,
        max_cells: usize,
        screens: &mut crate::protocol::chunked::ScreenAssembler,
    ) -> (ServerMessage, usize) {
        use crate::protocol::chunked::{Assembly, split_screen};
        let frames = split_screen(message, max_cells);
        let count = frames.len();
        let mut assembled = None;
        for frame in frames {
            let payload = crate::protocol::encode_payload(&Envelope {
                request_id: None,
                message: frame,
            })
            .unwrap();
            assert!(payload.len() <= crate::protocol::MAX_FRAME_LEN);
            let envelope: Envelope<ServerMessage> = decode_payload(&payload).unwrap();
            match screens.accept(envelope.message) {
                Assembly::Ready(message) => {
                    assert!(assembled.is_none(), "one message per sequence");
                    assembled = Some(message);
                }
                Assembly::Pending => {}
                Assembly::Discarded(discarded) => panic!("discarded {discarded:?}"),
            }
        }
        (assembled.expect("the sequence completed"), count)
    }

    /// Feed random VT output through `snapshot_message` and the wire, with
    /// some snapshots coalesced away, and check that the client's grid equals
    /// every screen it was sent. Returns counts of deltas checked, shifted,
    /// shifted with images, and full screens and deltas sent in chunks.
    fn random_round_trip(
        seed: u64,
        size: TerminalSize,
        steps: usize,
        max_cells: impl Fn(&mut Rng) -> usize,
        bases: usize,
    ) -> [usize; 5] {
        let mut counts = [0; 5];
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut vt = crate::terminal::bench::VtBench::with_scrollback(size, 4096).unwrap();
        let terminal_id = TerminalId::new();
        let mut last_sent = HashMap::new();
        let mut screens = crate::protocol::chunked::ScreenAssembler::default();
        let mut client: Option<ScreenSnapshot> = None;
        let mut history: Vec<ScreenSnapshot> = Vec::new();
        for step in 0..steps {
            let Some(screen) = vt.feed(&random_output(&mut rng, size)).unwrap() else {
                continue;
            };
            let context = format!("seed {seed} step {step}");
            for _ in 0..bases.min(history.len()) {
                let base = &history[rng.below(history.len())];
                for shifts in [true, false] {
                    let Some(delta) = screen.delta_from(base, shifts) else {
                        continue;
                    };
                    assert!(
                        shifts || delta.shift.is_none(),
                        "{context}: unnegotiated shift"
                    );
                    counts[1] += usize::from(delta.shift.is_some());
                    counts[2] += usize::from(
                        delta.shift.is_some() && !screen.graphics.placements.is_empty(),
                    );
                    counts[0] += 1;
                    let mut applied = base.clone();
                    applied.apply_delta(delta).unwrap();
                    assert_eq!(applied, screen, "{context} shifts {shifts}");
                }
            }
            history.push(screen.clone());
            // Coalescing drops snapshots the writer never sends.
            if rng.below(4) == 0 {
                continue;
            }
            let force_full = rng.below(40) == 0;
            let message = snapshot_message(
                terminal_id,
                screen.clone(),
                force_full,
                true,
                &mut last_sent,
            );
            // Survive the wire, as a client receives it.
            let (message, frames) = over_the_wire(message, max_cells(&mut rng), &mut screens);
            match message {
                ServerMessage::Snapshot { screen, .. } => {
                    counts[3] += usize::from(frames > 1);
                    client = Some(screen);
                }
                ServerMessage::SnapshotDelta { delta, .. } => {
                    counts[4] += usize::from(frames > 1);
                    client
                        .as_mut()
                        .expect("a delta follows a full screen")
                        .apply_delta(delta)
                        .unwrap_or_else(|error| panic!("{context}: {error}"));
                }
                other => panic!("unexpected {other:?}"),
            }
            assert_eq!(client.as_ref(), Some(&screen), "{context}");
            assert_eq!(last_sent.get(&terminal_id), Some(&screen), "{context}");
        }
        counts
    }

    #[test]
    fn deltas_from_random_vt_output_apply_to_exactly_the_full_screen() {
        // Test builds keep Ghostty's integrity checks, which make each feed
        // cost milliseconds; every screen is therefore also checked against
        // several earlier bases, which costs no further VT work. Messages are
        // also split into small chunks, as larger screens are.
        let sizes = [(80, 24), (13, 7), (40, 12), (3, 2), (1, 5)];
        let mut checked = 0;
        let mut shifted = 0;
        let mut shifted_images = 0;
        let mut chunked = 0;
        for seed in 1..=12_u64 {
            let (columns, rows) = sizes[seed as usize % sizes.len()];
            let size = TerminalSize { columns, rows };
            let cells = usize::from(columns) * usize::from(rows);
            let [deltas, shifts, shifted_with_images, full, split] = random_round_trip(
                seed,
                size,
                50,
                |rng| [usize::MAX, cells / 3, usize::from(columns), 1][rng.below(4)],
                4,
            );
            checked += deltas;
            shifted += shifts;
            shifted_images += shifted_with_images;
            chunked += full + split;
        }
        // The workload must actually exercise both kinds of delta.
        assert!(shifted > 50, "only {shifted} shifted deltas");
        assert!(
            shifted_images > 5,
            "only {shifted_images} shifted deltas with images"
        );
        assert!(checked > 2 * shifted, "only {checked} deltas checked");
        assert!(chunked > 100, "only {chunked} chunked messages");
    }

    #[test]
    fn random_vt_output_beyond_one_frame_round_trips_through_chunks() {
        // Above MAX_VISIBLE_CELLS: full screens split with the daemon's real
        // chunk budget, and a smaller one so that deltas split too.
        let size = TerminalSize {
            columns: 520,
            rows: 300,
        };
        assert!(size.cell_count().unwrap() > crate::domain::MAX_VISIBLE_CELLS);
        // Ghostty's integrity checks in test builds make some bursts (hundreds
        // of reverse indexes) take a minute at this size; this seed avoids
        // them and still splits both kinds of message.
        let [_, _, _, full, deltas] = random_round_trip(
            13,
            size,
            14,
            |rng| [crate::domain::MAX_VISIBLE_CELLS, 2_000][rng.below(2)],
            0,
        );
        assert!(full >= 2, "only {full} chunked full screens");
        assert!(deltas >= 2, "only {deltas} chunked deltas");
    }

    #[tokio::test]
    async fn writers_for_older_remote_peers_never_send_row_shifts() {
        let size = TerminalSize {
            columns: 20,
            rows: 6,
        };
        let mut vt = crate::terminal::bench::VtBench::new(size).unwrap();
        let mut screens = Vec::new();
        for line in 0..24 {
            screens.push(
                vt.feed(format!("line {line}\r\n").as_bytes())
                    .unwrap()
                    .unwrap(),
            );
        }
        for unshifted in [true, false] {
            let terminal_id = TerminalId::new();
            let (server, client) = UnixStream::pair().unwrap();
            let (sink, _) = Framed::new(server, codec()).split();
            let mut reader = Framed::new(client, codec());
            let queue = Arc::new(OutboundQueue::default());
            // As the remote handshake records a peer without `scroll-deltas.v1`.
            queue
                .unshifted_deltas
                .store(unshifted, std::sync::atomic::Ordering::Relaxed);
            let writer = tokio::spawn(write_outbound(sink, Arc::clone(&queue)));
            let mut shifts = 0;
            let mut received: Option<ScreenSnapshot> = None;
            for screen in &screens {
                queue
                    .push(Outbound::Snapshot {
                        terminal_id,
                        screen: screen.clone(),
                        force_full: false,
                    })
                    .unwrap();
                let frame = timeout(Duration::from_secs(2), reader.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let envelope: Envelope<ServerMessage> = decode_payload(&frame).unwrap();
                match envelope.message {
                    ServerMessage::Snapshot { screen, .. } => received = Some(screen),
                    ServerMessage::SnapshotDelta { delta, .. } => {
                        shifts += usize::from(delta.shift.is_some());
                        received.as_mut().unwrap().apply_delta(delta).unwrap();
                    }
                    other => panic!("unexpected {other:?}"),
                }
                assert_eq!(received.as_ref(), Some(screen));
            }
            if unshifted {
                assert_eq!(shifts, 0);
            } else {
                assert!(shifts > 0);
            }
            queue.finish();
            writer.await.unwrap();
        }
    }

    /// A screen whose rows all differ from any other revision's.
    fn numbered_screen(revision: u64, size: TerminalSize) -> ScreenSnapshot {
        use crate::domain::{Cell, Cursor};
        let columns = usize::from(size.columns);
        let cells = (0..columns * usize::from(size.rows))
            .map(|index| Cell {
                contents: if index % columns == 0 {
                    format!("{revision}").into()
                } else {
                    char::from_u32(0x4e00 + ((index as u64 + revision) % 20_000) as u32)
                        .unwrap()
                        .to_string()
                        .into()
                },
                ..Cell::default()
            })
            .collect();
        let cursor = Cursor {
            column: size.columns - 1,
            row: size.rows - 1,
            visible: true,
            shape: Default::default(),
            blinking: false,
        };
        ScreenSnapshot::new(revision, size, cells, cursor).unwrap()
    }

    /// Read frames until `done` matches one, checking that chunk sequences
    /// are contiguous and complete, and return the messages they stand for
    /// with the number of chunk frames seen.
    async fn read_assembled(
        reader: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
        done: impl Fn(&ServerMessage) -> bool,
    ) -> (Vec<ServerMessage>, usize) {
        use crate::protocol::chunked::{Assembly, ScreenAssembler};
        let mut screens = ScreenAssembler::default();
        let mut messages = Vec::new();
        let mut chunk_frames = 0;
        loop {
            let frame = timeout(Duration::from_secs(10), reader.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(frame.len() <= crate::protocol::MAX_FRAME_LEN);
            let envelope: Envelope<ServerMessage> = decode_payload(&frame).unwrap();
            let chunk = matches!(
                envelope.message,
                ServerMessage::ScreenBegin { .. }
                    | ServerMessage::ScreenRows { .. }
                    | ServerMessage::ScreenEnd { .. }
            );
            chunk_frames += usize::from(chunk);
            assert!(
                chunk || !screens.in_progress(),
                "{:?} interleaved with a chunk sequence",
                envelope.message
            );
            match screens.accept(envelope.message) {
                Assembly::Ready(message) => {
                    let finished = done(&message);
                    messages.push(message);
                    if finished {
                        return (messages, chunk_frames);
                    }
                }
                Assembly::Pending => {}
                Assembly::Discarded(discarded) => panic!("discarded {discarded:?}"),
            }
        }
    }

    #[tokio::test]
    async fn writer_sends_large_screens_as_whole_sequences_and_coalesces_only_whole_screens() {
        let size = TerminalSize {
            columns: 520,
            rows: 320,
        };
        let terminal_id = TerminalId::new();
        let (server, client) = UnixStream::pair().unwrap();
        let (sink, _) = Framed::new(server, codec()).split();
        let mut reader = Framed::new(client, codec());
        let queue = Arc::new(OutboundQueue::default());
        let writer = tokio::spawn(write_outbound(sink, Arc::clone(&queue)));
        let snapshot = |screen| Outbound::Snapshot {
            terminal_id,
            screen,
            force_full: false,
        };
        queue.push(snapshot(numbered_screen(1, size))).unwrap();
        // The writer is now mid-sequence: a screen this size is far larger
        // than the socket buffer.
        let first = timeout(Duration::from_secs(10), reader.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let first: Envelope<ServerMessage> = decode_payload(&first).unwrap();
        let ServerMessage::ScreenBegin { begin, .. } = first.message else {
            panic!("expected a chunked screen, received {:?}", first.message);
        };
        assert_eq!(begin.rows, 320);
        // Revision 2 is replaced by 3 while 1 is still being written, and an
        // ordered frame follows.
        queue.push(snapshot(numbered_screen(2, size))).unwrap();
        queue.push(snapshot(numbered_screen(3, size))).unwrap();
        queue
            .push(Outbound::Frame(Bytes::from(
                encode_payload(&Envelope {
                    request_id: None,
                    message: ServerMessage::Pong { daemon_pid: 1 },
                })
                .unwrap(),
            )))
            .unwrap();
        let mut rest = numbered_screen(1, size);
        let mut screens = crate::protocol::chunked::ScreenAssembler::default();
        // Finish revision 1 from the frames after its begin.
        assert!(matches!(
            screens.accept(ServerMessage::ScreenBegin { terminal_id, begin }),
            crate::protocol::chunked::Assembly::Pending
        ));
        let mut received = Vec::new();
        loop {
            let frame = timeout(Duration::from_secs(10), reader.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let envelope: Envelope<ServerMessage> = decode_payload(&frame).unwrap();
            match screens.accept(envelope.message) {
                crate::protocol::chunked::Assembly::Ready(ServerMessage::Snapshot {
                    screen,
                    ..
                }) => {
                    received.push(screen);
                    break;
                }
                crate::protocol::chunked::Assembly::Pending => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        let (messages, chunk_frames) = read_assembled(&mut reader, |message| {
            matches!(message, ServerMessage::Pong { .. })
        })
        .await;
        assert!(chunk_frames >= 4);
        for message in messages {
            match message {
                ServerMessage::Snapshot { screen, .. } => received.push(screen),
                ServerMessage::Pong { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(
            received
                .iter()
                .map(|screen| screen.revision)
                .collect::<Vec<_>>(),
            [1, 3],
            "revision 2 was coalesced away whole"
        );
        assert_eq!(received[0], rest);
        assert_eq!(received[1], numbered_screen(3, size));

        // The writer's base is the screen the client assembled: a one-row
        // change is a delta against revision 3.
        rest = numbered_screen(3, size);
        rest.revision = 4;
        rest.cells[7].contents = "z".into();
        queue.push(snapshot(rest.clone())).unwrap();
        let (messages, chunk_frames) = read_assembled(&mut reader, |_| true).await;
        assert_eq!(chunk_frames, 0, "a small delta fits one frame");
        let [ServerMessage::SnapshotDelta { delta, .. }] = &messages[..] else {
            panic!("expected a delta, received {messages:?}");
        };
        let mut applied = received.pop().unwrap();
        applied.apply_delta(delta.clone()).unwrap();
        assert_eq!(applied, rest);
        queue.finish();
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn writers_for_peers_without_chunked_screens_send_only_screens_within_their_bound() {
        let large = TerminalSize {
            columns: 520,
            rows: 320,
        };
        for (max_cells, embed_images) in [
            (crate::domain::MAX_VISIBLE_CELLS, false),
            (crate::domain::LEGACY_MAX_VISIBLE_CELLS, true),
        ] {
            let terminal_id = TerminalId::new();
            let (server, client) = UnixStream::pair().unwrap();
            let (sink, _) = Framed::new(server, codec()).split();
            let mut reader = Framed::new(client, codec());
            let queue = Arc::new(OutboundQueue::default());
            // As the remote handshake records an older peer.
            queue
                .max_screen_cells
                .store(max_cells, std::sync::atomic::Ordering::Relaxed);
            queue
                .embed_images
                .store(embed_images, std::sync::atomic::Ordering::Relaxed);
            let writer = tokio::spawn(write_outbound(sink, Arc::clone(&queue)));
            // A terminal shared with a larger client, before this peer's own
            // resize shrinks it: the peer sees the corner it can hold.
            queue
                .push(Outbound::Snapshot {
                    terminal_id,
                    screen: numbered_screen(1, large),
                    force_full: false,
                })
                .unwrap();
            queue
                .push(Outbound::Message {
                    request_id: Some(uuid::Uuid::new_v4()),
                    message: Box::new(ServerMessage::CopyModeSnapshot {
                        terminal_id,
                        screen: numbered_screen(2, large),
                    }),
                })
                .unwrap();
            let small = TerminalSize {
                columns: 200,
                rows: 50,
            };
            queue
                .push(Outbound::Snapshot {
                    terminal_id,
                    screen: numbered_screen(3, small),
                    force_full: false,
                })
                .unwrap();
            let (messages, chunk_frames) = read_assembled(&mut reader, |message| {
                matches!(message, ServerMessage::Snapshot { screen, .. } if screen.revision == 3)
            })
            .await;
            assert_eq!(chunk_frames, 0, "an older peer received chunks");
            let [
                ServerMessage::Snapshot { screen: live, .. },
                ServerMessage::CopyModeSnapshot { screen: copy, .. },
                ServerMessage::Snapshot { screen: last, .. },
            ] = &messages[..]
            else {
                panic!("unexpected {messages:?}");
            };
            for (screen, revision) in [(live, 1), (copy, 2)] {
                assert!(screen.cells.len() <= max_cells);
                assert_eq!(screen.size.columns, 520);
                assert_eq!(usize::from(screen.size.rows), max_cells / 520);
                assert!(!screen.cursor.visible, "the cursor was outside the corner");
                let mut expected = numbered_screen(revision, large);
                expected.crop_to(max_cells);
                assert_eq!(*screen, expected);
                assert_eq!(
                    screen.cells[..520],
                    numbered_screen(revision, large).cells[..520]
                );
            }
            assert_eq!(*last, numbered_screen(3, small));
            queue.finish();
            writer.await.unwrap();
        }
    }

    fn screen_with_image(revision: u64, generation: u64) -> ScreenSnapshot {
        use crate::domain::{Cell, Cursor, CursorShape};
        let size = TerminalSize {
            columns: 2,
            rows: 1,
        };
        let cursor = Cursor {
            column: 0,
            row: 0,
            visible: true,
            shape: CursorShape::Block,
            blinking: false,
        };
        let mut screen =
            ScreenSnapshot::new(revision, size, vec![Cell::default(); 2], cursor).unwrap();
        screen.graphics.images.push(crate::domain::KittyImage {
            id: 7,
            generation,
            png: vec![1, 2, 3].into(),
        });
        screen
            .graphics
            .placements
            .push(crate::domain::KittyPlacement {
                image_id: 7,
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
        screen
    }

    fn sent_image_generations(images: &[ServerMessage]) -> Vec<u64> {
        images
            .iter()
            .map(|message| match message {
                ServerMessage::KittyImage { image, .. } => {
                    assert!(!image.is_reference());
                    image.generation
                }
                other => panic!("expected kitty_image, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn sent_images_deliver_pixels_once_and_screens_carry_references() {
        let terminal_id = TerminalId::new();
        let mut sent = SentImages::default();
        let snapshot = |screen| ServerMessage::Snapshot {
            terminal_id,
            screen,
        };

        let mut first = snapshot(screen_with_image(1, 1));
        assert_eq!(sent_image_generations(&sent.prepare(&mut first)), [1]);
        let ServerMessage::Snapshot { screen, .. } = &first else {
            unreachable!()
        };
        assert!(screen.graphics.images[0].is_reference());

        // Unchanged pixels are never resent, including in copy-mode screens.
        let mut repeat = snapshot(screen_with_image(2, 1));
        assert!(sent.prepare(&mut repeat).is_empty());
        let mut copy = ServerMessage::CopyModeSnapshot {
            terminal_id,
            screen: screen_with_image(3, 1),
        };
        assert!(sent.prepare(&mut copy).is_empty());

        // A new generation is delivered; a delta without graphics changes
        // nothing about what the client holds.
        let mut delta = ServerMessage::SnapshotDelta {
            terminal_id,
            delta: ScreenDelta {
                revision: 4,
                base_revision: 3,
                size: TerminalSize {
                    columns: 2,
                    rows: 1,
                },
                shift: None,
                rows: Vec::new(),
                hyperlinks: Vec::new(),
                cursor: screen_with_image(4, 2).cursor,
                scroll: Default::default(),
                mouse_tracking: false,
                graphics: Some(screen_with_image(4, 2).graphics),
            },
        };
        assert_eq!(sent_image_generations(&sent.prepare(&mut delta)), [2]);
        let ServerMessage::SnapshotDelta {
            delta: sent_delta, ..
        } = &delta
        else {
            unreachable!()
        };
        assert!(sent_delta.graphics.as_ref().unwrap().images[0].is_reference());

        // The client prunes to the latest screen's images, so an earlier
        // generation must be delivered again.
        let mut previous = snapshot(screen_with_image(5, 1));
        assert_eq!(sent_image_generations(&sent.prepare(&mut previous)), [1]);

        // Forcing a grid refresh does not discard the image delivery cache.
        let mut forced = snapshot(screen_with_image(6, 1));
        assert!(sent.prepare(&mut forced).is_empty());

        let pane_id = PaneId::new();
        let focused = SelectedTarget {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            tab_id: TabId::new(),
            pane_id,
            terminal_id: TerminalId::new(),
            child_pid: 1,
        };
        let mut selected = ServerMessage::TargetSelected {
            selected: SelectedView {
                resource_revision: 1,
                focused: focused.clone(),
                panes: vec![focused],
                layout: SplitTree::leaf(pane_id),
            },
        };
        assert!(sent.prepare(&mut selected).is_empty());
        assert!(sent.0.is_empty());
        let mut returned = snapshot(screen_with_image(7, 1));
        assert_eq!(sent_image_generations(&sent.prepare(&mut returned)), [1]);
    }

    #[tokio::test]
    async fn copy_mode_invalidates_live_graphics_and_forced_snapshots_keep_images() {
        let terminal_id = TerminalId::new();
        let (server, client) = UnixStream::pair().unwrap();
        let (sink, _) = Framed::new(server, codec()).split();
        let mut reader = Framed::new(client, codec());
        let queue = Arc::new(OutboundQueue::default());
        queue
            .push(Outbound::Snapshot {
                terminal_id,
                screen: screen_with_image(1, 1),
                force_full: true,
            })
            .unwrap();
        for revision in [2, 3] {
            queue
                .push(Outbound::Message {
                    request_id: Some(uuid::Uuid::new_v4()),
                    message: Box::new(ServerMessage::CopyModeSnapshot {
                        terminal_id,
                        screen: screen_with_image(revision, 2),
                    }),
                })
                .unwrap();
        }
        queue
            .push(Outbound::Snapshot {
                terminal_id,
                screen: screen_with_image(4, 1),
                force_full: false,
            })
            .unwrap();
        let writer = tokio::spawn(write_outbound(sink, Arc::clone(&queue)));
        // Pixels precede their screen, copy-mode is an ordered barrier, and
        // returning live must restate graphics even though its cells match.
        for expected in [
            "image1", "snapshot", "image2", "copy", "copy", "image1", "snapshot",
        ] {
            let frame = timeout(Duration::from_secs(2), reader.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let envelope: Envelope<ServerMessage> = decode_payload(&frame).unwrap();
            match (expected, envelope.message) {
                ("image1", ServerMessage::KittyImage { image, .. }) => {
                    assert_eq!(image.generation, 1)
                }
                ("image2", ServerMessage::KittyImage { image, .. }) => {
                    assert_eq!(image.generation, 2)
                }
                ("snapshot", ServerMessage::Snapshot { screen, .. }) => {
                    assert!(screen.graphics.images[0].is_reference())
                }
                ("copy", ServerMessage::CopyModeSnapshot { screen, .. }) => {
                    assert!(envelope.request_id.is_some());
                    assert!(screen.graphics.images[0].is_reference());
                }
                (_, other) => panic!("expected {expected}, got {other:?}"),
            }
        }
        queue
            .push(Outbound::Snapshot {
                terminal_id,
                screen: screen_with_image(5, 1),
                force_full: true,
            })
            .unwrap();
        queue.finish();
        let frame = timeout(Duration::from_secs(2), reader.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let envelope: Envelope<ServerMessage> = decode_payload(&frame).unwrap();
        assert!(
            matches!(envelope.message, ServerMessage::Snapshot { .. }),
            "forced full resent image pixels"
        );
        writer.await.unwrap();
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
    fn uncorrelated_input_discards_only_stopped_and_unread_input() {
        assert_eq!(
            ui_event_error_disposition(UiEventPolicy::Input, None, &CommandError::Stopped),
            UiEventErrorDisposition::DropTransient
        );
        assert_eq!(
            ui_event_error_disposition(UiEventPolicy::Input, None, &CommandError::InputBacklogFull),
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
                &CommandError::InputBacklogFull
            ),
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
            (CommandError::InputBacklogFull, "input_backlog_full"),
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

    #[tokio::test]
    async fn attachment_pastes_to_original_target_when_view_changes_before_begin_without_enter() {
        let (mut state, path) = inconsistent_state();
        let (_original, terminal) =
            test_attachment("IFS= read -r line; printf ENTER_RECEIVED; sleep 60");
        state.runtimes.insert(
            path.terminal_id,
            RuntimeEntry {
                handle: terminal.clone(),
                lease: AttachmentLease::default(),
            },
        );
        let store = state.attachment_store.clone();
        let shared = Arc::new(Mutex::new(state));
        let mut upload = crate::attachments::Upload::default();
        let transfer_id = Uuid::new_v4();
        let (changed_view, other_terminal) = test_attachment("sleep 60");
        receive_attachment(
            &shared,
            &changed_view,
            &store,
            &mut upload,
            ClientMessage::AttachmentBegin {
                transfer_id,
                terminal_id: path.terminal_id,
                size: 8,
            },
        )
        .await
        .unwrap();
        receive_attachment(
            &shared,
            &changed_view,
            &store,
            &mut upload,
            ClientMessage::AttachmentChunk {
                transfer_id,
                bytes: b"\x89PNG\r\n\x1a\n".to_vec(),
            },
        )
        .await
        .unwrap();
        // This view's terminal has no resource/runtime entry. Success proves
        // commit did not redirect through the new focused terminal.
        assert!(
            receive_attachment(
                &shared,
                &changed_view,
                &store,
                &mut upload,
                ClientMessage::AttachmentCommit { transfer_id }
            )
            .await
            .unwrap()
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let output = terminal
                    .read_output(TerminalOutputSource::Visible, 24, false)
                    .await
                    .unwrap();
                if output.text.contains("fut-attachment-") {
                    assert!(!output.text.contains("ENTER_RECEIVED"));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        terminal.close().await.unwrap();
        other_terminal.close().await.unwrap();
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
        let acquisition = lease.acquire(owner, size, Arc::clone(&terminal)).unwrap();
        let (_, resource_changes) = watch::channel(0);
        let (_, presence_changes) = watch::channel(ClientPresenceSnapshot::default());
        let attachment = Attachment::new(
            AttachmentInit {
                owner,
                panes: vec![ObservedTarget {
                    selected: selected.clone(),
                    terminal: Arc::clone(&terminal),
                }],
                focused: LeasedTarget {
                    selected,
                    terminal: Arc::clone(&terminal),
                    lease: acquisition.guard,
                    pending_configuration: Some(acquisition.configuration),
                },
                layout: SplitTree::leaf(pane_id),
                fallback_terminal_ids: Vec::new(),
                resource_revision: 0,
            },
            resource_changes,
            presence_changes,
        );
        (attachment, terminal)
    }

    fn spawn_test_terminal(script: &str) -> TerminalHandle {
        spawn_terminal(SpawnSpec {
            terminal: crate::terminal::TerminalConfig::default(),
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
            parent_workspace_id: None,
            workspace_name: "peer".into(),
            root: "/peer".into(),
            tab_id: TabId::new(),
            tab_name: "shell".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let peer_workspace = peer.workspace_id;
        let peer_tab = peer.tab_id;
        let peer_pane = peer.pane_id;
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
        assert!(interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Session(crate::resources::SessionSelector::Id(path.session_id)),
        ));
        assert!(interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Session(crate::resources::SessionSelector::Name("test".into())),
        ));
        assert!(!interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Tab(peer_tab),
        ));
        assert!(interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Pane(path.pane_id),
        ));
        assert!(!interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Pane(peer_pane),
        ));
        assert!(!interactive_rename_allowed(
            &state.resources,
            path.session_id,
            path.workspace_id,
            &RenameSelector::Session(crate::resources::SessionSelector::Id(SessionId::new())),
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
            parent_workspace_id: None,
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
        assert!(interactive_close_allowed(
            &state.resources,
            &focused,
            &TargetSelector::Session(crate::resources::SessionSelector::Id(path.session_id)),
        ));
        assert!(interactive_close_allowed(
            &state.resources,
            &focused,
            &TargetSelector::Session(crate::resources::SessionSelector::Name("test".into())),
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
        assert!(!interactive_close_allowed(
            &state.resources,
            &focused,
            &TargetSelector::Session(crate::resources::SessionSelector::Id(SessionId::new())),
        ));
    }

    #[tokio::test]
    async fn clients_can_focus_different_panes_without_renaming_the_shared_tab() {
        let (mut state, path) = inconsistent_state();
        let second_pane = PaneId::new();
        let second_terminal_id = TerminalId::new();
        state
            .resources
            .rename_tab(path.tab_id, String::new())
            .unwrap();
        state
            .resources
            .split_pane(
                path.pane_id,
                SplitDirection::Right,
                second_pane,
                second_terminal_id,
            )
            .unwrap();
        state
            .resources
            .update_process_name(path.terminal_id, "anchor".into())
            .unwrap();
        state
            .resources
            .update_process_name(second_terminal_id, "other".into())
            .unwrap();

        let first_terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                terminal: crate::terminal::TerminalConfig::default(),
                id: path.terminal_id,
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "while :; do sleep 1; done".into()],
                cwd: "/".into(),
                env: HashMap::new(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .unwrap(),
        );
        let second_terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                terminal: crate::terminal::TerminalConfig::default(),
                id: second_terminal_id,
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "while :; do sleep 1; done".into()],
                cwd: "/".into(),
                env: HashMap::new(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .unwrap(),
        );
        state.runtimes.insert(
            path.terminal_id,
            RuntimeEntry {
                handle: Arc::clone(&first_terminal),
                lease: AttachmentLease::default(),
            },
        );
        state.runtimes.insert(
            second_terminal_id,
            RuntimeEntry {
                handle: Arc::clone(&second_terminal),
                lease: AttachmentLease::default(),
            },
        );
        let revision = state.resources.revision();
        let shared = Arc::new(Mutex::new(state));
        let size = TerminalSize {
            columns: 80,
            rows: 24,
        };

        let first_client = lease_view(
            &shared,
            Some(TargetSelector::Pane(path.pane_id)),
            None,
            ClientId::new(),
            LeaseSize::Requested(size),
        )
        .await
        .unwrap();
        let second_client = lease_view(
            &shared,
            Some(TargetSelector::Pane(second_pane)),
            None,
            ClientId::new(),
            LeaseSize::Requested(size),
        )
        .await
        .unwrap();

        assert_eq!(first_client.focused.selected.pane_id, path.pane_id);
        assert_eq!(second_client.focused.selected.pane_id, second_pane);
        let state = shared.lock().await;
        assert_eq!(state.resources.revision(), revision);
        assert_eq!(
            state.resources.snapshot().sessions[0].workspaces[0].tabs[0].name,
            "anchor"
        );
        drop(state);
        drop(first_client);
        drop(second_client);
        first_terminal.close().await.unwrap();
        second_terminal.close().await.unwrap();
    }

    #[tokio::test]
    async fn switching_panes_keeps_the_target_terminal_size() {
        let (mut state, path) = inconsistent_state();
        let second_pane = PaneId::new();
        let second_terminal_id = TerminalId::new();
        state
            .resources
            .split_pane(
                path.pane_id,
                SplitDirection::Right,
                second_pane,
                second_terminal_id,
            )
            .unwrap();
        let spawn = |id, size| {
            Arc::new(
                spawn_terminal(SpawnSpec {
                    terminal: crate::terminal::TerminalConfig::default(),
                    id,
                    program: "/bin/sh".into(),
                    argv: vec!["-c".into(), "while :; do sleep 1; done".into()],
                    cwd: "/".into(),
                    env: HashMap::new(),
                    size,
                })
                .unwrap(),
            )
        };
        let tall = TerminalSize {
            columns: 80,
            rows: 24,
        };
        let short = TerminalSize {
            columns: 80,
            rows: 12,
        };
        let first_terminal = spawn(path.terminal_id, tall);
        let second_terminal = spawn(second_terminal_id, short);
        for (terminal_id, handle) in [
            (path.terminal_id, &first_terminal),
            (second_terminal_id, &second_terminal),
        ] {
            state.runtimes.insert(
                terminal_id,
                RuntimeEntry {
                    handle: Arc::clone(handle),
                    lease: AttachmentLease::default(),
                },
            );
        }
        let shared = Arc::new(Mutex::new(state));
        let client = ClientId::new();
        let attachment = lease_view(
            &shared,
            Some(TargetSelector::Pane(path.pane_id)),
            None,
            client,
            LeaseSize::Requested(tall),
        )
        .await
        .unwrap();

        let candidate =
            switch_candidate(&shared, TargetSelector::Pane(second_pane), None, client).await;
        drop(attachment);
        let candidate = candidate.unwrap();

        // Queued behind any resize the switch requested.
        second_terminal.refresh_snapshot().await.unwrap();
        assert_eq!(second_terminal.subscribe_snapshots().borrow().size, short);
        drop(candidate);
        first_terminal.close().await.unwrap();
        second_terminal.close().await.unwrap();
    }

    fn status_report(id: &str, state: StatusState) -> StatusUpdate {
        StatusUpdate::Report(StatusReport {
            id: id.into(),
            state,
            kind: None,
            progress: None,
            app: Some("test".into()),
            title: None,
            message: None,
        })
    }

    #[test]
    fn old_error_does_not_mask_batched_fresh_working_done() {
        let (mut state, path) = inconsistent_state();
        let resource_changes = state.resource_changes.subscribe();
        let mut status_events = state.agent_events.subscribe();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("old", StatusState::Error)],
                StatusSource::Osc,
            )
            .unwrap();
        let barrier = state
            .resources
            .program_status(path.terminal_id)
            .unwrap()
            .revision;
        let mut wait = AgentWaitSelection::fresh(barrier);
        let _ = status_events.try_recv().unwrap();

        state
            .apply_status_updates(
                path.terminal_id,
                vec![
                    status_report("fresh", StatusState::Working),
                    status_report("fresh", StatusState::Done),
                ],
                StatusSource::Osc,
            )
            .unwrap();

        let status = state.resources.program_status(path.terminal_id).unwrap();
        assert_eq!(status.summary_state(), StatusState::Error);
        assert_eq!(status.working_revision, status.revision);
        assert!(resource_changes.has_changed().unwrap());
        let event = status_events.try_recv().unwrap();
        assert_eq!(event.terminal_id, path.terminal_id);
        assert_eq!(event.status, *status);
        assert_eq!(
            event.transitions,
            vec![
                StatusRecordLifecycle::Working("fresh".into()),
                StatusRecordLifecycle::Settled("fresh".into()),
            ]
        );
        let AgentWaitObservation::Settled(result) = wait.observe(&event) else {
            panic!("fresh record should settle its own wait");
        };
        assert_eq!(result.summary_state(), StatusState::Done);
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].id, "fresh");
        assert!(matches!(
            status_events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn unrelated_result_cannot_settle_selected_work() {
        let (mut state, path) = inconsistent_state();
        let source = StatusSource::Osc;
        let mut events = state.agent_events.subscribe();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("old", StatusState::Done)],
                source.clone(),
            )
            .unwrap();
        let barrier = state.resources.revision();
        let _ = events.try_recv().unwrap();
        let mut wait = AgentWaitSelection::fresh(barrier);
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("live", StatusState::Working)],
                source.clone(),
            )
            .unwrap();
        assert!(matches!(
            wait.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Pending
        ));
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("other", StatusState::Done)],
                source.clone(),
            )
            .unwrap();
        assert!(matches!(
            wait.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Pending
        ));
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("live", StatusState::Done)],
                source,
            )
            .unwrap();
        let AgentWaitObservation::Settled(result) = wait.observe(&events.try_recv().unwrap())
        else {
            panic!("selected record should settle on its own result");
        };
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].id, "live");
    }

    #[test]
    fn later_root_work_does_not_replace_standalone_or_fresh_child_selection() {
        let (mut state, path) = inconsistent_state();
        let mut events = state.agent_events.subscribe();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("child", StatusState::Working)],
                StatusSource::Osc,
            )
            .unwrap();
        let _ = events.try_recv().unwrap();
        let mut standalone =
            AgentWaitSelection::current(state.resources.program_status(path.terminal_id).unwrap())
                .unwrap();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![
                    status_report("", StatusState::Working),
                    status_report("", StatusState::Done),
                ],
                StatusSource::Osc,
            )
            .unwrap();
        assert!(matches!(
            standalone.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Pending
        ));
        assert_eq!(standalone.selected, HashSet::from(["child".to_owned()]));
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("child", StatusState::Done)],
                StatusSource::Osc,
            )
            .unwrap();
        assert!(matches!(
            standalone.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Settled(_)
        ));

        state
            .apply_status_updates(
                path.terminal_id,
                vec![
                    StatusUpdate::Clear { id: None },
                    status_report("old", StatusState::Error),
                ],
                StatusSource::Osc,
            )
            .unwrap();
        let barrier = state.resources.revision();
        let _ = events.try_recv().unwrap();
        let mut fresh = AgentWaitSelection::fresh(barrier);
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("prompt", StatusState::Working)],
                StatusSource::Osc,
            )
            .unwrap();
        assert!(matches!(
            fresh.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Pending
        ));
        state
            .apply_status_updates(
                path.terminal_id,
                vec![
                    status_report("", StatusState::Working),
                    status_report("", StatusState::Done),
                ],
                StatusSource::Osc,
            )
            .unwrap();
        assert!(matches!(
            fresh.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Pending
        ));
        assert_eq!(fresh.selected, HashSet::from(["prompt".to_owned()]));
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("prompt", StatusState::Done)],
                StatusSource::Osc,
            )
            .unwrap();
        assert!(matches!(
            fresh.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Settled(_)
        ));
    }

    #[test]
    fn selected_work_evicted_by_record_257_is_agent_exit() {
        let (mut state, path) = inconsistent_state();
        let mut initial = vec![status_report("selected", StatusState::Working)];
        initial
            .extend((0..255).map(|index| status_report(&format!("r{index}"), StatusState::Idle)));
        state
            .apply_status_updates(path.terminal_id, initial, StatusSource::Osc)
            .unwrap();
        let mut events = state.agent_events.subscribe();
        let mut wait =
            AgentWaitSelection::current(state.resources.program_status(path.terminal_id).unwrap())
                .unwrap();

        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("overflow", StatusState::Idle)],
                StatusSource::Osc,
            )
            .unwrap();
        let event = events.try_recv().unwrap();
        assert!(
            event
                .transitions
                .contains(&StatusRecordLifecycle::Removed("selected".into()))
        );
        assert!(matches!(
            wait.observe(&event),
            AgentWaitObservation::Exited { terminal: false }
        ));
    }

    #[test]
    fn wait_prefers_working_root_and_otherwise_tracks_all_active_records() {
        let (mut state, path) = inconsistent_state();
        let mut events = state.agent_events.subscribe();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![
                    status_report("", StatusState::Working),
                    status_report("child", StatusState::Working),
                ],
                StatusSource::Osc,
            )
            .unwrap();
        let _ = events.try_recv().unwrap();
        let status = state.resources.program_status(path.terminal_id).unwrap();
        let mut wait = AgentWaitSelection::current(status).unwrap();
        assert_eq!(wait.selected, HashSet::from([String::new()]));

        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("", StatusState::Done)],
                StatusSource::Osc,
            )
            .unwrap();
        let AgentWaitObservation::Settled(result) = wait.observe(&events.try_recv().unwrap())
        else {
            panic!("root result should settle without waiting for its child");
        };
        assert_eq!(result.records.len(), 1);
        assert!(result.records[0].id.is_empty());

        state
            .apply_status_updates(
                path.terminal_id,
                vec![
                    StatusUpdate::Clear { id: None },
                    status_report("one", StatusState::Working),
                    status_report("two", StatusState::Working),
                ],
                StatusSource::Osc,
            )
            .unwrap();
        let _ = events.try_recv().unwrap();
        let mut wait =
            AgentWaitSelection::current(state.resources.program_status(path.terminal_id).unwrap())
                .unwrap();
        assert_eq!(
            wait.selected,
            HashSet::from(["one".to_owned(), "two".to_owned()])
        );
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("one", StatusState::Done)],
                StatusSource::Osc,
            )
            .unwrap();
        assert!(matches!(
            wait.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Pending
        ));
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("two", StatusState::Done)],
                StatusSource::Osc,
            )
            .unwrap();
        let AgentWaitObservation::Settled(result) = wait.observe(&events.try_recv().unwrap())
        else {
            panic!("all selected records have settled");
        };
        assert_eq!(result.records.len(), 2);
    }

    #[tokio::test]
    async fn watcher_survives_close_claiming_the_status_stream_before_registration() {
        for exited_before_watch in [false, true] {
            let (state, path) = inconsistent_state();
            let cwd = tempfile::tempdir().unwrap();
            let handle = Arc::new(
                spawn_terminal(SpawnSpec {
                    terminal: crate::terminal::TerminalConfig::default(),
                    id: path.terminal_id,
                    program: "/bin/sh".into(),
                    argv: vec!["-c".into(), "sleep 60".into()],
                    cwd: cwd.path().into(),
                    env: HashMap::new(),
                    size: TerminalSize {
                        columns: 80,
                        rows: 24,
                    },
                })
                .unwrap(),
            );
            drop(handle.take_status_updates());
            if exited_before_watch {
                handle.close().await.unwrap();
            }
            let (exited, mut received) = mpsc::unbounded_channel();
            watch_terminal(Arc::clone(&handle), Arc::new(Mutex::new(state)), exited);
            if !exited_before_watch {
                tokio::task::yield_now().await;
                assert!(matches!(
                    received.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
                handle.close().await.unwrap();
            }
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), received.recv())
                    .await
                    .expect("watcher did not report the closed terminal"),
                Some(path.terminal_id)
            );
        }
    }

    #[test]
    fn external_reports_cannot_spoof_provenance_or_terminal_lifecycle() {
        let (mut state, path) = inconsistent_state();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("native", StatusState::Working)],
                StatusSource::Osc,
            )
            .unwrap();
        let before = state
            .resources
            .program_status(path.terminal_id)
            .unwrap()
            .clone();
        let revision = state.resources.revision();
        let report_source = StatusSource::Report {
            source: Some("test".into()),
            agent_session_id: None,
        };
        let submissions = [
            (StatusUpdate::PromptStart, report_source.clone()),
            (StatusUpdate::ProcessExit, report_source),
            (status_report("", StatusState::Working), StatusSource::Osc),
            (
                status_report("", StatusState::Working),
                StatusSource::Detection {
                    agent: "codex".into(),
                    rule: "forged".into(),
                },
            ),
        ];
        for (update, source) in submissions {
            assert_eq!(
                state
                    .report_external_agent(path.terminal_id, update.clone(), source.clone())
                    .unwrap_err()
                    .code,
                "invalid_agent_report"
            );
            assert_eq!(
                state
                    .report_automatic_agent(path.terminal_id, update, source, 42)
                    .unwrap_err()
                    .code,
                "invalid_agent_report"
            );
        }
        assert_eq!(
            state.resources.program_status(path.terminal_id).unwrap(),
            &before
        );
        assert_eq!(state.resources.revision(), revision);
        assert!(state.resources.automatic_status_owners().is_empty());
    }

    #[test]
    fn reported_process_exit_is_agent_exit_not_an_unrelated_result() {
        let (mut state, path) = inconsistent_state();
        let mut events = state.agent_events.subscribe();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![
                    status_report("old", StatusState::Done),
                    status_report("live", StatusState::Working),
                ],
                StatusSource::Osc,
            )
            .unwrap();
        let _ = events.try_recv().unwrap();
        let mut wait =
            AgentWaitSelection::current(state.resources.program_status(path.terminal_id).unwrap())
                .unwrap();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![StatusUpdate::ProcessExit],
                StatusSource::Osc,
            )
            .unwrap();
        assert!(matches!(
            wait.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Exited { terminal: false }
        ));
        let status = state.resources.program_status(path.terminal_id).unwrap();
        assert_eq!(status.records.len(), 1);
        assert_eq!(status.records[0].id, "old");
    }

    #[test]
    fn automatic_pid_exit_removes_selected_old_record_without_exiting_terminal() {
        let (mut state, path) = inconsistent_state();
        let source = StatusSource::Report {
            source: Some("cli".into()),
            agent_session_id: None,
        };
        state
            .report_automatic_agent(
                path.terminal_id,
                status_report("old", StatusState::Working),
                source,
                42,
            )
            .unwrap();
        let mut events = state.agent_events.subscribe();
        let mut wait =
            AgentWaitSelection::current(state.resources.program_status(path.terminal_id).unwrap())
                .unwrap();

        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("new", StatusState::Working)],
                StatusSource::Osc,
            )
            .unwrap();
        assert!(matches!(
            wait.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Pending
        ));
        state
            .release_automatic_agent_owner(path.terminal_id, 42)
            .unwrap();
        let event = events.try_recv().unwrap();
        assert!(!event.terminal_process_exit);
        assert!(
            event
                .transitions
                .contains(&StatusRecordLifecycle::Removed("old".into()))
        );
        assert!(matches!(
            wait.observe(&event),
            AgentWaitObservation::Exited { terminal: false }
        ));
        let status = state.resources.program_status(path.terminal_id).unwrap();
        assert_eq!(status.records.len(), 1);
        assert_eq!(status.records[0].id, "new");
        assert_eq!(status.records[0].state, StatusState::Working);
    }

    #[test]
    fn legacy_progress_batches_are_weak_and_their_clear_spares_explicit_records() {
        let (mut state, path) = inconsistent_state();
        let mut events = state.agent_events.subscribe();
        let legacy = |updates| NativeStatusBatch {
            updates,
            foreground_pid: None,
            source: crate::terminal::NativeStatusSource::Progress,
        };
        state
            .apply_status_updates(
                path.terminal_id,
                vec![
                    status_report("result", StatusState::Done),
                    StatusUpdate::PromptStart,
                ],
                StatusSource::Osc,
            )
            .unwrap();
        let _ = events.try_recv().unwrap();

        state
            .apply_terminal_status_updates(
                path.terminal_id,
                legacy(vec![status_report("", StatusState::Working)]),
            )
            .unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!(
            event.transitions,
            vec![StatusRecordLifecycle::Working(String::new())]
        );
        assert_eq!(event.status.records.len(), 2);

        state
            .apply_terminal_status_updates(
                path.terminal_id,
                legacy(vec![StatusUpdate::Clear { id: None }]),
            )
            .unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!(
            event.transitions,
            vec![StatusRecordLifecycle::Removed(String::new())]
        );
        let status = state.resources.program_status(path.terminal_id).unwrap();
        assert_eq!(status.records.len(), 1);
        assert_eq!(status.records[0].id, "result");
    }

    #[test]
    fn terminal_process_exit_is_distinct_and_retained_result_still_settles() {
        let (mut state, path) = inconsistent_state();
        let mut events = state.agent_events.subscribe();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("live", StatusState::Working)],
                StatusSource::Osc,
            )
            .unwrap();
        let _ = events.try_recv().unwrap();
        let mut wait =
            AgentWaitSelection::current(state.resources.program_status(path.terminal_id).unwrap())
                .unwrap();
        state
            .apply_terminal_status_updates(
                path.terminal_id,
                NativeStatusBatch {
                    updates: vec![StatusUpdate::ProcessExit],
                    foreground_pid: None,
                    source: crate::terminal::NativeStatusSource::Osc,
                },
            )
            .unwrap();
        assert!(matches!(
            wait.observe(&events.try_recv().unwrap()),
            AgentWaitObservation::Exited { terminal: true }
        ));

        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("live", StatusState::Working)],
                StatusSource::Osc,
            )
            .unwrap();
        let _ = events.try_recv().unwrap();
        let mut wait =
            AgentWaitSelection::current(state.resources.program_status(path.terminal_id).unwrap())
                .unwrap();
        state
            .apply_terminal_status_updates(
                path.terminal_id,
                NativeStatusBatch {
                    updates: vec![
                        status_report("live", StatusState::Done),
                        StatusUpdate::ProcessExit,
                    ],
                    foreground_pid: None,
                    source: crate::terminal::NativeStatusSource::Osc,
                },
            )
            .unwrap();
        let AgentWaitObservation::Settled(result) = wait.observe(&events.try_recv().unwrap())
        else {
            panic!("a retained selected result should win over process exit");
        };
        assert_eq!(result.summary_state(), StatusState::Done);
    }

    #[test]
    fn detection_waits_for_explicit_owner_exit_then_coexists_with_persistent_results() {
        let (mut state, path) = inconsistent_state();
        let report_source = StatusSource::Report {
            source: Some("pi".into()),
            agent_session_id: Some("session".into()),
        };
        state
            .apply_status_updates(
                path.terminal_id,
                vec![StatusUpdate::Report(StatusReport {
                    id: "result".into(),
                    state: StatusState::Done,
                    kind: None,
                    progress: None,
                    app: Some("pi".into()),
                    title: Some("finished".into()),
                    message: None,
                })],
                report_source.clone(),
            )
            .unwrap();
        let explicit_revision = state.resources.revision();
        let detection_source = StatusSource::Detection {
            agent: "codex".into(),
            rule: "working_indicator".into(),
        };
        let detected_working = StatusUpdate::Report(StatusReport {
            id: String::new(),
            state: StatusState::Working,
            kind: None,
            progress: None,
            app: Some("codex".into()),
            title: None,
            message: None,
        });
        state
            .apply_status_updates(
                path.terminal_id,
                vec![detected_working.clone()],
                detection_source.clone(),
            )
            .unwrap();
        assert_eq!(state.resources.revision(), explicit_revision);

        state
            .apply_status_updates(
                path.terminal_id,
                vec![StatusUpdate::ProcessExit],
                report_source,
            )
            .unwrap();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![detected_working],
                detection_source.clone(),
            )
            .unwrap();
        let status = state.resources.program_status(path.terminal_id).unwrap();
        assert_eq!(status.summary_state(), StatusState::Working);
        assert!(
            status
                .records
                .iter()
                .any(|record| { record.id == "result" && record.state == StatusState::Done })
        );

        state
            .apply_status_updates(
                path.terminal_id,
                vec![StatusUpdate::ProcessExit],
                detection_source,
            )
            .unwrap();
        let status = state.resources.program_status(path.terminal_id).unwrap();
        assert_eq!(status.summary_state(), StatusState::Done);
        assert_eq!(status.records.len(), 1);
    }

    #[test]
    fn explicit_takeover_clears_inferred_done_eligibility() {
        let (mut state, path) = inconsistent_state();
        let detection = StatusSource::Detection {
            agent: "codex".into(),
            rule: "working_indicator".into(),
        };
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("", StatusState::Working)],
                detection.clone(),
            )
            .unwrap();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("", StatusState::Idle)],
                detection,
            )
            .unwrap();
        assert_eq!(
            state
                .resources
                .program_status(path.terminal_id)
                .unwrap()
                .summary_state(),
            StatusState::Done
        );

        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("child", StatusState::Working)],
                StatusSource::Osc,
            )
            .unwrap();
        state
            .apply_status_updates(
                path.terminal_id,
                vec![StatusUpdate::Clear {
                    id: Some("child".into()),
                }],
                StatusSource::Osc,
            )
            .unwrap();
        let status = state.resources.program_status(path.terminal_id).unwrap();
        assert!(status.records.is_empty());
        assert!(!status.is_reporting());
    }

    #[tokio::test]
    async fn terminal_watcher_applies_osc_status_while_screen_is_detached() {
        let (mut state, path) = inconsistent_state();
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                terminal: crate::terminal::TerminalConfig::default(),
                id: path.terminal_id,
                program: "/bin/sh".into(),
                argv: vec![
                    "-c".into(),
                    "printf '\\033]7501;state=done:id=build:title=RmluaXNoZWQ=\\007'; sleep 60"
                        .into(),
                ],
                cwd: "/".into(),
                env: HashMap::new(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .unwrap(),
        );
        state.runtimes.insert(
            path.terminal_id,
            RuntimeEntry {
                handle: Arc::clone(&terminal),
                lease: AttachmentLease::default(),
            },
        );
        let shared = Arc::new(Mutex::new(state));
        let (exited, _exits) = mpsc::unbounded_channel();
        watch_terminal(Arc::clone(&terminal), Arc::clone(&shared), exited);

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let reported = shared
                    .lock()
                    .await
                    .resources
                    .program_status(path.terminal_id)
                    .is_ok_and(|status| status.summary_state() == StatusState::Done);
                if reported {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let state = shared.lock().await;
        let status = state.resources.program_status(path.terminal_id).unwrap();
        assert_eq!(status.summary_record().unwrap().id, "build");
        assert_eq!(
            status.summary_record().unwrap().title.as_deref(),
            Some("Finished")
        );
        drop(state);
        terminal.close().await.unwrap();
    }

    #[tokio::test]
    async fn wait_observes_terminal_that_already_exited() {
        let (mut state, path) = inconsistent_state();
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                terminal: crate::terminal::TerminalConfig::default(),
                id: path.terminal_id,
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "exit 0".into()],
                cwd: "/".into(),
                env: HashMap::new(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .unwrap(),
        );
        state.runtimes.insert(
            path.terminal_id,
            RuntimeEntry {
                handle: Arc::clone(&terminal),
                lease: AttachmentLease::default(),
            },
        );
        state
            .apply_status_updates(
                path.terminal_id,
                vec![status_report("", StatusState::Working)],
                StatusSource::Osc,
            )
            .unwrap();
        let mut lifecycle = terminal.subscribe_lifecycle();
        tokio::time::timeout(Duration::from_secs(5), async {
            while matches!(*lifecycle.borrow(), TerminalLifecycle::Running) {
                lifecycle.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let shared = Arc::new(Mutex::new(state));
        let error = wait_agent(&shared, path.terminal_id, 5_000)
            .await
            .unwrap_err();
        assert_eq!(error.code, "terminal_exited");
    }

    #[tokio::test]
    async fn exited_lifecycle_still_reports_lagged_and_closed_agent_streams() {
        let (state, path) = inconsistent_state();
        let shared = Arc::new(Mutex::new(state));
        let (events, receiver) = broadcast::channel(1);
        let update = StatusLifecycleUpdate {
            terminal_id: path.terminal_id,
            status: ProgramStatus {
                revision: 1,
                ..ProgramStatus::default()
            },
            transitions: Vec::new(),
            terminal_process_exit: false,
        };
        events.send(update.clone()).unwrap();
        events.send(update).unwrap();
        let (_lifecycle_sender, mut lifecycle) =
            watch::channel(TerminalLifecycle::Exited { exit_code: Some(0) });

        let error = wait_for_agent_events(
            &shared,
            path.terminal_id,
            AgentWaitSelection::fresh(0),
            1_000,
            receiver,
            &mut lifecycle,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "agent_events_lagged");

        let (events, receiver) = broadcast::channel(1);
        drop(events);
        let error = wait_for_agent_events(
            &shared,
            path.terminal_id,
            AgentWaitSelection::fresh(0),
            1_000,
            receiver,
            &mut lifecycle,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "agent_events_closed");
    }

    #[tokio::test]
    async fn configured_background_start_returns_done_with_start_request_id() {
        let (mut state, path) = inconsistent_state();
        state.config_location = global_config::ConfigLocation::disabled();
        state
            .child_env
            .insert("FUT_SOCKET".into(), "/tmp/fut-command-test.sock".into());
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                terminal: Default::default(),
                id: path.terminal_id,
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "sleep 60".into()],
                cwd: "/".into(),
                env: HashMap::new(),
                size: TerminalSize {
                    columns: 40,
                    rows: 5,
                },
            })
            .unwrap(),
        );
        state.runtimes.insert(
            path.terminal_id,
            RuntimeEntry {
                handle: Arc::clone(&terminal),
                lease: AttachmentLease::default(),
            },
        );
        let shared = Arc::new(Mutex::new(state));
        let (server, client) = UnixStream::pair().unwrap();
        let writers: WriterTasks = Default::default();
        let mut connection = ClientConnection::new(server, &writers);
        connection.remote = Some(Capabilities::ALL);
        let request_id = uuid::Uuid::new_v4();
        let task = tokio::spawn(async move {
            command::serve(
                &mut connection,
                &shared,
                Some(request_id),
                crate::protocol::RemoteCommandSpec {
                    title: "Server test".into(),
                    program: "/bin/sh".into(),
                    args: vec!["-c".into(), "echo failed >&2; exit 9".into()],
                    execution: crate::extensions::ExtensionCommandExecution::Background,
                    extension: None,
                    extension_args_override: false,
                },
                path.terminal_id,
                TerminalSize {
                    columns: 40,
                    rows: 5,
                },
                None,
            )
            .await
            .unwrap();
        });
        let mut client = Framed::new(client, codec());
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let envelope: Envelope<ServerMessage> = decode_payload(&frame).unwrap();
        assert_eq!(envelope.request_id, Some(request_id));
        let ServerMessage::CommandReply {
            reply:
                crate::protocol::RemoteCommandReply::Done {
                    exit_code,
                    activated_target,
                    failure_output,
                },
        } = envelope.message
        else {
            panic!("background start must reply Done, not Ready")
        };
        assert_eq!(exit_code, Some(9));
        assert_eq!(activated_target, None);
        assert!(failure_output.unwrap().contains("failed"));
        task.await.unwrap();
        terminal.close().await.unwrap();
    }

    #[test]
    fn configured_command_screens_coalesce_without_crossing_reply_barriers() {
        let queue = OutboundQueue::default();
        let screen = |revision| Outbound::Message {
            request_id: None,
            message: Box::new(ServerMessage::CommandReply {
                reply: crate::protocol::RemoteCommandReply::Screen {
                    screen: screen_with_image(revision, 1),
                },
            }),
        };
        queue.push(screen(1)).unwrap();
        queue.push(screen(2)).unwrap();
        assert_eq!(queue.state.lock().unwrap().items.len(), 1);
        queue.push(Outbound::Frame(Bytes::new())).unwrap();
        queue.push(screen(3)).unwrap();
        assert_eq!(queue.state.lock().unwrap().items.len(), 3);
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
        let extension_registry = Arc::new(
            crate::extensions::ExtensionRegistry::new(
                1,
                Vec::new(),
                global_config::ExtensionConfigCatalog::default(),
            )
            .unwrap(),
        );
        let extension_catalog = watch::channel(extension_registry.catalog().unwrap()).0;
        (
            SharedState {
                attachment_store: Default::default(),
                resources,
                runtimes: HashMap::new(),
                presence: ClientPresence::default(),
                expected_finalizations: HashSet::new(),
                exited_terminals: VecDeque::new(),
                resource_changes: watch::channel(1).0,
                alerts: AlertStore::default(),
                alert_changes: watch::channel(0).0,
                agent_events: broadcast::channel(AGENT_EVENT_CAPACITY).0,
                child_env: HashMap::new(),
                terminal_config: crate::terminal::TerminalConfig::default(),
                default_shell: PathBuf::from("/bin/sh"),
                config_location: global_config::ConfigLocation {
                    path: None,
                    explicit: false,
                    source: "test",
                },
                extension_registry,
                extension_catalog,
                hook_queue: crate::extensions::hook_queue().0,
                accepting: true,
            },
            path,
        )
    }

    fn reload_fixture(root: &Path) -> (PathBuf, global_config::ConfigLocation) {
        let extension = root.join("extension");
        fs::create_dir(&extension).unwrap();
        fs::write(
            extension.join(crate::extensions::MANIFEST_FILE_NAME),
            r#"api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
id = "reload-test"
capabilities = ["commands", "presentation_tokens"]
[commands.logs]
title = "Open logs"
argv = ["true"]
[[presentation_tokens]]
name = "state"
scope = "workspace"
"#,
        )
        .unwrap();
        let config = root.join("config.toml");
        fs::write(
            &config,
            format!("extensions = [{:?}]\n", extension.display().to_string()),
        )
        .unwrap();
        let location = global_config::ConfigLocation {
            path: Some(config.clone()),
            explicit: true,
            source: "test",
        };
        (config, location)
    }

    #[test]
    fn token_actions_are_validated_and_materialize_publisher_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let (_, location) = reload_fixture(temporary.path());
        let registry = Arc::new(load_reload_registry(&location, 1));
        let (mut state, path) = inconsistent_state();
        state.extension_registry = Arc::clone(&registry);
        state.extension_catalog = watch::channel(registry.catalog().unwrap()).0;
        let sibling_pane = PaneId::new();
        state
            .resources
            .add_pane(path.tab_id, sibling_pane, TerminalId::new())
            .unwrap();
        let peer = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            parent_workspace_id: None,
            workspace_name: "peer".into(),
            root: "/peer".into(),
            tab_id: TabId::new(),
            tab_name: "peer".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let peer_pane = peer.pane_id;
        state
            .resources
            .add_workspace(path.session_id, peer)
            .unwrap();
        let target = PresentationTokenTarget::Workspace(path.workspace_id);

        let published = state
            .publish_token(
                "reload-test",
                "state",
                "ready".into(),
                target,
                Some(PresentationTokenPublishAction::ExtensionCommand {
                    command: "logs".into(),
                }),
            )
            .unwrap();
        assert!(published.changed);
        assert_eq!(
            state.resources.snapshot().sessions[0].workspaces[0].tokens["workspace.extension.reload-test.state"],
            MaterializedTokenValue::new(
                "ready".into(),
                Some(PresentationTokenAction::ExtensionCommand {
                    extension_id: "reload-test".into(),
                    command: "logs".into(),
                })
            )
        );

        for (value, action) in [
            (
                "",
                PresentationTokenPublishAction::ExtensionCommand {
                    command: "logs".into(),
                },
            ),
            (
                "ready",
                PresentationTokenPublishAction::ExtensionCommand {
                    command: "missing".into(),
                },
            ),
            (
                "ready",
                PresentationTokenPublishAction::Pane { pane_id: peer_pane },
            ),
        ] {
            let error = state
                .publish_token("reload-test", "state", value.into(), target, Some(action))
                .unwrap_err();
            assert_eq!(error.code, "invalid_token_action");
        }

        let error = state
            .publish_token(
                "reload-test",
                "state",
                "ready".into(),
                target,
                Some(PresentationTokenPublishAction::Pane {
                    pane_id: PaneId::new(),
                }),
            )
            .unwrap_err();
        assert_eq!(error.code, "not_found");

        state.resources.close_pane(sibling_pane).unwrap();
        let error = state
            .publish_token(
                "reload-test",
                "state",
                "ready".into(),
                target,
                Some(PresentationTokenPublishAction::Pane {
                    pane_id: sibling_pane,
                }),
            )
            .unwrap_err();
        assert_eq!(error.code, "target_closing");
    }

    fn load_reload_registry(
        location: &global_config::ConfigLocation,
        generation: u64,
    ) -> crate::extensions::ExtensionRegistry {
        let loaded = global_config::load_extensions_location(location).unwrap();
        crate::extensions::ExtensionRegistry::new(generation, loaded.extensions, loaded.config)
            .unwrap()
    }

    #[tokio::test]
    async fn extension_activation_swaps_generation_prunes_and_snapshots_hooks_atomically() {
        let temporary = tempfile::tempdir().unwrap();
        let (_, location) = reload_fixture(temporary.path());
        let candidate = load_reload_registry(&location, 1);
        let equivalent = load_reload_registry(&location, 1);
        let (mut state, path) = inconsistent_state();
        state.config_location = location;
        let (hook_queue, mut hook_receiver) = crate::extensions::hook_queue();
        state.hook_queue = hook_queue;
        let old_fingerprint = state.extension_registry.fingerprint().to_owned();
        state
            .resources
            .publish_presentation_token(
                PresentationTokenTarget::Workspace(path.workspace_id),
                "workspace.extension.removed.state".into(),
                "stale".into(),
            )
            .unwrap();
        let before_revision = state.resources.revision();
        let mut resource_changes = state.resource_changes.subscribe();
        let mutation = |revision| Mutation {
            revision,
            events: vec![crate::resources::ResourceEvent::WorkspaceCreated {
                session_id: path.session_id,
                id: path.workspace_id,
                name: "main".into(),
                root: path.root.clone(),
                trusted_project_config: None,
            }],
            terminals_to_close: Vec::new(),
            multiplexer_empty: false,
        };
        state.enqueue_committed_mutation(&mutation(10));

        let activated = state.activate_extension_candidate(candidate).unwrap();
        state.enqueue_committed_mutation(&mutation(11));

        assert!(activated.changed);
        assert_eq!(activated.catalog.generation, 2);
        assert_eq!(state.extension_registry.generation(), 2);
        assert_eq!(state.resources.revision(), before_revision + 1);
        assert!(
            state.resources.snapshot().sessions[0].workspaces[0]
                .tokens
                .is_empty()
        );
        assert!(resource_changes.has_changed().unwrap());
        assert_eq!(
            *resource_changes.borrow_and_update(),
            state.resources.revision()
        );
        assert_eq!(
            hook_receiver.receive_snapshot_identity().await.unwrap(),
            (10, 1, old_fingerprint)
        );
        assert_eq!(
            hook_receiver.receive_snapshot_identity().await.unwrap(),
            (11, 2, activated.catalog.fingerprint.clone())
        );

        let active = Arc::clone(&state.extension_registry);
        let revision = state.resources.revision();
        let unchanged = state.activate_extension_candidate(equivalent).unwrap();
        assert_eq!(
            unchanged,
            ExtensionReload {
                catalog: activated.catalog,
                changed: false,
            }
        );
        assert!(Arc::ptr_eq(&active, &state.extension_registry));
        assert_eq!(state.resources.revision(), revision);
        assert!(!resource_changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn failed_extension_reload_preserves_registry_generation_and_tokens_exactly() {
        let temporary = tempfile::tempdir().unwrap();
        let (config, location) = reload_fixture(temporary.path());
        let registry = Arc::new(load_reload_registry(&location, 7));
        let (mut state, path) = inconsistent_state();
        state.config_location = location;
        state.extension_registry = Arc::clone(&registry);
        state.extension_catalog = watch::channel(registry.catalog().unwrap()).0;
        state
            .publish_token(
                "reload-test",
                "state",
                "ready".into(),
                PresentationTokenTarget::Workspace(path.workspace_id),
                None,
            )
            .unwrap();
        let before = state.resources.snapshot();
        fs::write(&config, "extensions = [\n").unwrap();
        let shared = Arc::new(Mutex::new(state));

        let error = reload_extensions(&shared).await.unwrap_err();

        assert_eq!(error.code, "extension_reload_failed");
        assert!(error.message.contains(&config.display().to_string()));
        assert!(error.message.contains("fix the configuration"));
        assert!(error.message.contains("left unchanged"));
        let state = shared.lock().await;
        assert!(Arc::ptr_eq(&registry, &state.extension_registry));
        assert_eq!(state.extension_registry.generation(), 7);
        assert_eq!(
            state.extension_registry.fingerprint(),
            registry.fingerprint()
        );
        assert_eq!(state.resources.snapshot(), before);
    }

    #[test]
    fn prepared_extension_candidate_is_invisible_and_stale_commit_cannot_overwrite_winner() {
        let temporary = tempfile::tempdir().unwrap();
        let (_, location) = reload_fixture(temporary.path());
        let prepared_candidate = load_reload_registry(&location, 1);
        let winning_candidate = load_reload_registry(&location, 1);
        let (mut state, _) = inconsistent_state();
        let mut catalogs = state.extension_catalog.subscribe();

        let prepared = state
            .prepare_extension_candidate(prepared_candidate)
            .unwrap();
        assert_eq!(prepared.base_generation, 1);
        assert_eq!(prepared.catalog.generation, 2);
        assert_eq!(state.extension_registry.generation(), 1);
        assert!(!catalogs.has_changed().unwrap());

        let winner = state
            .activate_extension_candidate(winning_candidate)
            .unwrap();
        assert_eq!(winner.catalog.generation, 2);
        assert!(catalogs.has_changed().unwrap());
        assert_eq!(catalogs.borrow_and_update().generation, 2);
        let active_fingerprint = state.extension_registry.fingerprint().to_owned();

        let error = state.commit_extension_candidate(prepared, 1).unwrap_err();
        assert_eq!(error.code, "stale_extension_generation");
        assert_eq!(state.extension_registry.generation(), 2);
        assert_eq!(state.extension_registry.fingerprint(), active_fingerprint);
        assert!(!catalogs.has_changed().unwrap());
    }

    #[test]
    fn exhausted_extension_generation_rejects_activation_before_pruning() {
        let temporary = tempfile::tempdir().unwrap();
        let (_, location) = reload_fixture(temporary.path());
        let candidate = load_reload_registry(&location, 1);
        let (mut state, path) = inconsistent_state();
        state.extension_registry = Arc::new(
            crate::extensions::ExtensionRegistry::new(
                u64::MAX,
                Vec::new(),
                global_config::ExtensionConfigCatalog::default(),
            )
            .unwrap(),
        );
        state.extension_catalog = watch::channel(state.extension_registry.catalog().unwrap()).0;
        state
            .resources
            .publish_presentation_token(
                PresentationTokenTarget::Workspace(path.workspace_id),
                "workspace.extension.removed.state".into(),
                "stale".into(),
            )
            .unwrap();
        let active = Arc::clone(&state.extension_registry);
        let before = state.resources.snapshot();

        let error = state.activate_extension_candidate(candidate).unwrap_err();

        assert_eq!(error.code, "extension_reload_failed");
        assert!(error.message.contains("generation is exhausted"));
        assert!(Arc::ptr_eq(&active, &state.extension_registry));
        assert_eq!(state.extension_registry.generation(), u64::MAX);
        assert_eq!(state.resources.snapshot(), before);
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
    fn close_failure_after_an_exit_preserves_partial_closing_state() {
        let (mut state, path) = inconsistent_state();
        state
            .resources
            .add_pane(path.tab_id, PaneId::new(), TerminalId::new())
            .unwrap();
        let scope = CloseScope::Tab(path.tab_id);
        scope.begin(&mut state.resources).unwrap();
        let revision = state.resources.revision();

        let error = state.fail_target_close(scope, 1, 2, CommandError::Stopped);

        assert_eq!(error.code, "close_partial");
        assert!(error.message.contains("closed 1 of 2 terminals"));
        assert!(error.message.contains("cannot be rolled back"));
        assert_eq!(state.resources.revision(), revision);
        let tab = &state.resources.snapshot().sessions[0].workspaces[0].tabs[0];
        assert!(tab.closing);
        assert!(tab.panes.iter().all(|pane| pane.closing));
    }

    #[test]
    fn close_failure_before_termination_still_rolls_back_marker() {
        let (mut state, path) = inconsistent_state();
        let scope = CloseScope::Pane(path.pane_id);
        scope.begin(&mut state.resources).unwrap();

        let error = state.fail_target_close(scope, 0, 1, CommandError::Stopped);

        assert_eq!(error.code, "close_failed");
        assert!(!state.resources.snapshot().sessions[0].workspaces[0].tabs[0].panes[0].closing);
    }

    #[test]
    fn close_timeout_preserves_marker_after_termination_started() {
        let (mut state, path) = inconsistent_state();
        let scope = CloseScope::Pane(path.pane_id);
        scope.begin(&mut state.resources).unwrap();
        let revision = state.resources.revision();

        let error = state.fail_target_close(scope, 0, 1, CommandError::CloseTimeout);

        assert_eq!(error.code, "close_incomplete");
        assert!(error.message.contains("termination cannot be rolled back"));
        assert_eq!(state.resources.revision(), revision);
        assert!(state.resources.snapshot().sessions[0].workspaces[0].tabs[0].panes[0].closing);
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

        assert_eq!(
            state.finalize_terminal(path.terminal_id).unwrap(),
            FinalizationOutcome::AlreadyFinalized
        );
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
                terminal: crate::terminal::TerminalConfig::default(),
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

        assert_eq!(
            state.finalize_terminal_for_replacement(old_id).unwrap(),
            FinalizationOutcome::MultiplexerEmpty
        );
        assert!(!state.accepting);
        state.accepting = true;

        assert!(state.accepting);
        assert_eq!(
            state.finalize_terminal(old_id).unwrap(),
            FinalizationOutcome::AlreadyFinalized
        );
        assert!(state.accepting);
    }

    #[tokio::test]
    async fn failed_last_terminal_replacement_leaves_live_state_untouched() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let resolved = ResolvedLocation {
            cwd: root.clone(),
            project: crate::resources::Project {
                identity: crate::resources::ProjectIdentity::CanonicalDirectory(root.clone()),
            },
            workspace_root: root.clone(),
            suggested_session_name: "replacement".into(),
            workspace_kind: crate::project::WorkspaceKind::Directory,
        };
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                terminal: crate::terminal::TerminalConfig::default(),
                id: TerminalId::new(),
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "exit 0".into()],
                cwd: root.clone(),
                env: HashMap::new(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
            })
            .unwrap(),
        );
        let path = initial_path(&resolved, "replacement".into(), terminal.id());
        let (mut state, _) = inconsistent_state();
        state.resources = ResourceTree::default();
        state
            .register_session(path.clone(), Arc::clone(&terminal))
            .unwrap();
        let mut lifecycle = terminal.subscribe_lifecycle();
        tokio::time::timeout(Duration::from_secs(5), async {
            while matches!(*lifecycle.borrow_and_update(), TerminalLifecycle::Running) {
                lifecycle.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let before = state.resources.snapshot();
        let shared = Arc::new(Mutex::new(state));
        let (exited, _) = mpsc::unbounded_channel();

        let error = recipe::open_location(
            &shared,
            &exited,
            recipe::OpenLocationRequest {
                project: None,
                name: None,
                parent_workspace_id: None,
                cwd: root,
                program: Some("/definitely/missing/fut-shell".into()),
                argv: Vec::new(),
            },
        )
        .await
        .unwrap_err();

        assert_eq!(error.code, "spawn_failed");
        let state = shared.lock().await;
        assert!(state.accepting);
        assert_eq!(state.resources.snapshot(), before);
        assert!(state.runtimes.contains_key(&path.terminal_id));
        assert!(!state.expected_finalizations.contains(&path.terminal_id));
    }

    #[tokio::test]
    async fn registered_pane_has_independent_runtime_and_finalization() {
        let (mut state, path) = inconsistent_state();
        let terminal = Arc::new(
            spawn_terminal(SpawnSpec {
                terminal: crate::terminal::TerminalConfig::default(),
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
        assert_eq!(
            state.finalize_terminal(terminal_id).unwrap(),
            FinalizationOutcome::ResourcesRemain
        );
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
                    terminal: crate::terminal::TerminalConfig::default(),
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
        let extension_registry = Arc::new(
            crate::extensions::ExtensionRegistry::new(
                1,
                Vec::new(),
                global_config::ExtensionConfigCatalog::default(),
            )
            .unwrap(),
        );
        let extension_catalog = watch::channel(extension_registry.catalog().unwrap()).0;
        let mut state = SharedState {
            attachment_store: Default::default(),
            resources: ResourceTree::default(),
            runtimes: HashMap::new(),
            presence: ClientPresence::default(),
            expected_finalizations: HashSet::new(),
            exited_terminals: VecDeque::new(),
            resource_changes,
            alerts: AlertStore::default(),
            alert_changes: watch::channel(0).0,
            agent_events: broadcast::channel(AGENT_EVENT_CAPACITY).0,
            child_env: HashMap::new(),
            terminal_config: crate::terminal::TerminalConfig::default(),
            default_shell: PathBuf::from("/bin/sh"),
            config_location: global_config::ConfigLocation {
                path: None,
                explicit: false,
                source: "test",
            },
            extension_registry,
            extension_catalog,
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
            LeaseSize::Requested(TerminalSize {
                columns: 80,
                rows: 24,
            }),
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
