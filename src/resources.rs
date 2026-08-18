//! Pure, synchronous ownership tree for Fut's live resources.
//!
//! Project identities and workspace roots are boundary values: callers must resolve and
//! canonicalize them before inserting them here. This tree deliberately performs no I/O.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    domain::{
        AgentActivity, AgentDetection, AgentEvent, AgentIntegration, AgentReport,
        AgentReportMetadata, AgentState, PaneId, SessionId, SplitId, TabId, TerminalId,
        WorkspaceId,
    },
    splits::{SplitDirection, SplitRatio, SplitTree},
};

/// A daemon-wide bound on materialized extension presentation values. Token
/// declarations are bounded separately while loading extension manifests.
pub const MAX_MATERIALIZED_TOKEN_VALUES: usize = 4096;
/// Token values are presentation text, never style or executable content.
pub const MAX_MATERIALIZED_TOKEN_VALUE_BYTES: usize = 1024;

pub type MaterializedTokenMap = BTreeMap<String, String>;
pub type ExtensionConfigTable = serde_json::Map<String, serde_json::Value>;

/// Extension configuration approved as part of the exact project recipe and
/// captured for the lifetime of its live session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrustedProjectConfig {
    pub source: PathBuf,
    pub extension: BTreeMap<String, ExtensionConfigTable>,
}

pub(crate) const WORKSPACE_GIT_BRANCH_TOKEN: &str = "workspace.git_branch";
pub(crate) const WORKSPACE_GIT_ADDED_TOKEN: &str = "workspace.git_added";
pub(crate) const WORKSPACE_GIT_DELETED_TOKEN: &str = "workspace.git_deleted";

/// Presented in place of a location when a workspace's panes disagree.
pub const MULTIPLE_LOCATIONS: &str = "multiple";

/// The single live location every open pane shares: the Git work tree
/// containing a pane's observed working directory when there is one, the
/// directory itself otherwise, and the creation `root` until the daemon has
/// observed anything. `None` when panes disagree.
pub fn shared_live_location<'a>(root: &'a Path, tabs: &'a [TabSnapshot]) -> Option<&'a Path> {
    let mut location = None;
    let panes = tabs
        .iter()
        .filter(|tab| !tab.closing)
        .flat_map(|tab| &tab.panes)
        .filter(|pane| !pane.closing);
    for pane in panes {
        let place = pane
            .worktree
            .as_deref()
            .or(pane.cwd.as_deref())
            .unwrap_or(root);
        match location {
            None => location = Some(place),
            Some(current) if current == place => {}
            Some(_) => return None,
        }
    }
    Some(location.unwrap_or(root))
}

/// Workspaces and tabs may be unnamed (empty), in which case they present an
/// automatic name; any other name must carry visible characters.
fn check_optional_name(name: &str) -> Result<(), ResourceError> {
    if name.is_empty() || !name.trim().is_empty() {
        Ok(())
    } else {
        Err(ResourceError::EmptyName)
    }
}

fn disambiguate(suggested: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(suggested) {
        return suggested.to_owned();
    }
    (2..)
        .map(|suffix| format!("{suggested}-{suffix}"))
        .find(|candidate| !exists(candidate))
        .expect("an unbounded suffix must produce a unique name")
}

fn is_builtin_token(name: &str) -> bool {
    matches!(
        name,
        WORKSPACE_GIT_BRANCH_TOKEN | WORKSPACE_GIT_ADDED_TOKEN | WORKSPACE_GIT_DELETED_TOKEN
    )
}

fn extension_token_count(tokens: &MaterializedTokenMap) -> usize {
    tokens.keys().filter(|name| !is_builtin_token(name)).count()
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum ProjectIdentity {
    GitCommonDir(PathBuf),
    CanonicalDirectory(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub identity: ProjectIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SessionSelector {
    Id(SessionId),
    Name(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum TargetSelector {
    Session(SessionSelector),
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
    Terminal(TerminalId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckoutDestination {
    Existing(WorkspaceId),
    AddWorkspace { session_id: SessionId },
    CreateSession,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitialPath {
    pub session_id: SessionId,
    pub session_name: String,
    pub project: Project,
    pub trusted_project_config: Option<TrustedProjectConfig>,
    pub workspace_id: WorkspaceId,
    pub workspace_name: String,
    pub root: PathBuf,
    pub tab_id: TabId,
    pub tab_name: String,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspacePath {
    pub workspace_id: WorkspaceId,
    pub workspace_name: String,
    pub root: PathBuf,
    pub tab_id: TabId,
    pub tab_name: String,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabPath {
    pub tab_id: TabId,
    pub tab_name: String,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedTerminalPath {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub revision: u64,
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub name: String,
    pub project: Project,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_project_config: Option<TrustedProjectConfig>,
    pub closing: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: MaterializedTokenMap,
    pub workspaces: Vec<WorkspaceSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    pub name: String,
    pub root: PathBuf,
    pub closing: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: MaterializedTokenMap,
    pub tabs: Vec<TabSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub id: TabId,
    pub name: String,
    pub closing: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: MaterializedTokenMap,
    pub layout: SplitTree,
    pub panes: Vec<PaneSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub id: PaneId,
    pub terminal_id: TerminalId,
    pub closing: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens: MaterializedTokenMap,
    pub activity: AgentActivity,
    /// Observed working directory of the pane's foreground process; absent
    /// until the daemon's process observer has resolved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Top level of the Git work tree containing `cwd`, when there is one, so
    /// panes in different subdirectories of a checkout share a location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
}

/// A pane together with its borrowed ancestry in a resource snapshot.
///
/// Projectors can filter this traversal without cloning resource collections
/// or materialized presentation tokens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PanePathRef<'a> {
    pub session: &'a SessionSnapshot,
    pub workspace: &'a WorkspaceSnapshot,
    pub tab: &'a TabSnapshot,
    pub pane: &'a PaneSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "id", rename_all = "snake_case")]
pub enum PresentationTokenTarget {
    Session(SessionId),
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenPublication {
    pub revision: u64,
    pub changed: bool,
}

impl ResourceSnapshot {
    /// Traverse every pane together with its session, workspace, and tab.
    pub fn pane_paths(&self) -> impl Iterator<Item = PanePathRef<'_>> {
        self.sessions.iter().flat_map(|session| {
            session.workspaces.iter().flat_map(move |workspace| {
                workspace.tabs.iter().flat_map(move |tab| {
                    tab.panes.iter().map(move |pane| PanePathRef {
                        session,
                        workspace,
                        tab,
                        pane,
                    })
                })
            })
        })
    }

    /// Resolve a terminal's current live ancestry from this snapshot. Terminal
    /// identity is stable across pane moves; ancestor IDs captured in a child
    /// environment are not.
    pub fn live_terminal_path(
        &self,
        terminal_id: TerminalId,
    ) -> Result<ResolvedTerminalPath, ResourceError> {
        let mut found = None;
        for path in self.pane_paths() {
            if path.pane.terminal_id != terminal_id {
                continue;
            }
            if found.is_some() {
                return Err(ResourceError::Invariant(
                    "terminal appears more than once in resource snapshot".into(),
                ));
            }
            if path.session.closing {
                return Err(ResourceError::Closing("session"));
            }
            if path.workspace.closing {
                return Err(ResourceError::Closing("workspace"));
            }
            if path.tab.closing {
                return Err(ResourceError::Closing("tab"));
            }
            if path.pane.closing {
                return Err(ResourceError::Closing("pane"));
            }
            found = Some(ResolvedTerminalPath {
                session_id: path.session.id,
                workspace_id: path.workspace.id,
                tab_id: path.tab.id,
                pane_id: path.pane.id,
                terminal_id,
            });
        }
        found.ok_or(ResourceError::NotFound("terminal"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CloseCause {
    Requested,
    TerminalExited,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ResourceEvent {
    SessionCreated {
        id: SessionId,
        name: String,
        project: Project,
        trusted_project_config: Option<TrustedProjectConfig>,
    },
    SessionRenamed {
        id: SessionId,
        old_name: String,
        new_name: String,
    },
    WorkspaceCreated {
        session_id: SessionId,
        id: WorkspaceId,
        name: String,
        root: PathBuf,
        trusted_project_config: Option<TrustedProjectConfig>,
    },
    WorkspaceRenamed {
        session_id: SessionId,
        id: WorkspaceId,
        root: PathBuf,
        old_name: String,
        new_name: String,
        trusted_project_config: Option<TrustedProjectConfig>,
    },
    TabCreated {
        workspace_id: WorkspaceId,
        id: TabId,
        name: String,
    },
    TabRenamed {
        workspace_id: WorkspaceId,
        id: TabId,
        old_name: String,
        new_name: String,
    },
    PaneCreated {
        tab_id: TabId,
        id: PaneId,
        terminal_id: TerminalId,
        closing: bool,
    },
    PaneMoved {
        pane_id: PaneId,
        terminal_id: TerminalId,
        from: TabId,
        to: TabId,
    },
    SplitResized {
        tab_id: TabId,
        split_id: SplitId,
        ratio: SplitRatio,
    },
    PaneCloseRequested {
        pane_id: PaneId,
        terminal_id: TerminalId,
    },
    TabCloseRequested {
        tab_id: TabId,
    },
    SessionCloseRequested {
        session_id: SessionId,
    },
    WorkspaceCloseRequested {
        workspace_id: WorkspaceId,
    },
    PaneCloseCancelled {
        pane_id: PaneId,
        terminal_id: TerminalId,
    },
    TabCloseCancelled {
        tab_id: TabId,
    },
    SessionCloseCancelled {
        session_id: SessionId,
    },
    WorkspaceCloseCancelled {
        workspace_id: WorkspaceId,
    },
    PaneClosed {
        pane_id: PaneId,
        terminal_id: TerminalId,
        cause: CloseCause,
    },
    TabClosed {
        tab_id: TabId,
    },
    WorkspaceClosed {
        session_id: SessionId,
        workspace_id: WorkspaceId,
        name: String,
        root: PathBuf,
        trusted_project_config: Option<TrustedProjectConfig>,
    },
    SessionClosed {
        session_id: SessionId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Mutation {
    pub revision: u64,
    pub events: Vec<ResourceEvent>,
    pub terminals_to_close: Vec<TerminalId>,
    pub multiplexer_empty: bool,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ResourceError {
    #[error("resource not found: {0}")]
    NotFound(&'static str),
    #[error("duplicate {0}")]
    Duplicate(&'static str),
    #[error("name must not be blank")]
    EmptyName,
    #[error("panes may only move between tabs in the same workspace")]
    DifferentWorkspace,
    #[error("resource is closing: {0}")]
    Closing(&'static str),
    #[error("a session target must be selected")]
    TargetRequired,
    #[error("target is ambiguous")]
    AmbiguousTarget,
    #[error("invalid agent report: {0}")]
    InvalidAgentReport(&'static str),
    #[error("invalid split ratio")]
    InvalidSplitRatio,
    #[error("maximum of {MAX_MATERIALIZED_TOKEN_VALUES} materialized token values reached")]
    TooManyMaterializedTokens,
    #[error("resource tree invariant violated: {0}")]
    Invariant(String),
}

#[derive(Clone, Debug)]
struct Session {
    name: String,
    project: Project,
    trusted_project_config: Option<TrustedProjectConfig>,
    tokens: MaterializedTokenMap,
    workspaces: Vec<WorkspaceId>,
}
#[derive(Clone, Debug)]
struct Workspace {
    session_id: SessionId,
    name: String,
    root: PathBuf,
    tokens: MaterializedTokenMap,
    tabs: Vec<TabId>,
}
#[derive(Clone, Debug)]
struct Tab {
    workspace_id: WorkspaceId,
    /// A non-empty user title wins permanently until the user clears it.
    name: String,
    automatic_name: String,
    focused_pane: PaneId,
    tokens: MaterializedTokenMap,
    /// `None` only while removing the final pane, before empty-resource
    /// cascading removes the tab in the same mutation.
    layout: Option<SplitTree>,
}
#[derive(Clone, Debug)]
struct Pane {
    tab_id: TabId,
    terminal_id: TerminalId,
    tokens: MaterializedTokenMap,
    activity: AgentActivity,
    cwd: Option<PathBuf>,
    worktree: Option<PathBuf>,
}

/// An explicitly requested close. Descendants inherit this state through
/// their ownership ancestry, but never store a copied close flag themselves.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CloseIntent {
    Session(SessionId),
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
}

#[derive(Clone, Debug, Default)]
pub struct ResourceTree {
    revision: u64,
    session_order: Vec<SessionId>,
    sessions: BTreeMap<SessionId, Session>,
    workspaces: BTreeMap<WorkspaceId, Workspace>,
    tabs: BTreeMap<TabId, Tab>,
    panes: BTreeMap<PaneId, Pane>,
    terminals: BTreeMap<TerminalId, PaneId>,
    close_intents: BTreeSet<CloseIntent>,
}

impl ResourceTree {
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn session_is_closing(&self, session_id: SessionId) -> bool {
        self.close_intents
            .contains(&CloseIntent::Session(session_id))
    }

    fn workspace_is_closing(&self, workspace_id: WorkspaceId) -> bool {
        let workspace = &self.workspaces[&workspace_id];
        self.session_is_closing(workspace.session_id)
            || self
                .close_intents
                .contains(&CloseIntent::Workspace(workspace_id))
    }

    fn tab_is_closing(&self, tab_id: TabId) -> bool {
        let tab = &self.tabs[&tab_id];
        self.workspace_is_closing(tab.workspace_id)
            || self.close_intents.contains(&CloseIntent::Tab(tab_id))
    }

    fn pane_is_closing(&self, pane_id: PaneId) -> bool {
        let pane = &self.panes[&pane_id];
        self.tab_is_closing(pane.tab_id) || self.close_intents.contains(&CloseIntent::Pane(pane_id))
    }

    fn tab_panes(&self, tab_id: TabId) -> Vec<PaneId> {
        self.tabs[&tab_id]
            .layout
            .as_ref()
            .map_or_else(Vec::new, SplitTree::leaf_ids)
    }

    #[must_use]
    pub fn automatic_name_terminal_ids(&self) -> Vec<TerminalId> {
        self.tabs
            .iter()
            .filter(|(id, tab)| tab.name.is_empty() && !self.tab_is_closing(**id))
            .filter_map(|(_, tab)| {
                (!self.pane_is_closing(tab.focused_pane))
                    .then_some(self.panes[&tab.focused_pane].terminal_id)
            })
            .collect()
    }

    pub fn resolve_session(&self, selector: SessionSelector) -> Result<SessionId, ResourceError> {
        match selector {
            SessionSelector::Id(id) if self.sessions.contains_key(&id) => Ok(id),
            SessionSelector::Id(_) => Err(ResourceError::NotFound("session")),
            SessionSelector::Name(name) => self
                .session_order
                .iter()
                .copied()
                .find(|id| self.sessions[id].name == name)
                .ok_or(ResourceError::NotFound("session")),
        }
    }

    pub fn resolve_terminal_target(
        &self,
        selector: Option<TargetSelector>,
    ) -> Result<ResolvedTerminalPath, ResourceError> {
        let candidates = match selector {
            None => self.open_paths()?,
            Some(TargetSelector::Session(selector)) => {
                let id = self.resolve_session(selector)?;
                self.paths_for_session(id)?
            }
            Some(TargetSelector::Workspace(id)) => self.paths_for_workspace(id)?,
            Some(TargetSelector::Tab(id)) => self.paths_for_tab(id)?,
            Some(TargetSelector::Pane(id)) => vec![self.path_for_pane(id)?],
            Some(TargetSelector::Terminal(id)) => {
                let pane = *self
                    .terminals
                    .get(&id)
                    .ok_or(ResourceError::NotFound("terminal"))?;
                vec![self.path_for_pane(pane)?]
            }
        };
        match candidates.as_slice() {
            [path] => Ok(*path),
            _ => Err(ResourceError::AmbiguousTarget),
        }
    }

    /// Plans a checkout without reserving names, roots, or identifiers.
    pub fn checkout_destination(
        &self,
        project: &Project,
        root: &Path,
    ) -> Result<CheckoutDestination, ResourceError> {
        let matching_sessions: Vec<_> = self
            .session_order
            .iter()
            .copied()
            .filter(|id| self.sessions[id].project.identity == project.identity)
            .collect();
        if matching_sessions.len() > 1 {
            return self.invalid("project identity belongs to multiple sessions");
        }
        let root_owner = self
            .workspaces
            .values()
            .find(|workspace| workspace.root == root);
        let Some(&session_id) = matching_sessions.first() else {
            return if root_owner.is_some() {
                self.invalid("workspace root belongs to another project")
            } else {
                Ok(CheckoutDestination::CreateSession)
            };
        };
        let session = &self.sessions[&session_id];
        if self.session_is_closing(session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if root_owner.is_some_and(|workspace| workspace.session_id != session_id) {
            return self.invalid("workspace root belongs to another project");
        }
        let matching_roots = session
            .workspaces
            .iter()
            .filter(|workspace_id| self.workspaces[workspace_id].root == root)
            .copied()
            .collect::<Vec<_>>();
        if let Some(workspace_id) = matching_roots
            .iter()
            .find(|workspace_id| !self.workspace_is_closing(**workspace_id))
        {
            return Ok(CheckoutDestination::Existing(*workspace_id));
        }
        if !matching_roots.is_empty() {
            return Err(ResourceError::Closing("workspace"));
        }
        Ok(CheckoutDestination::AddWorkspace { session_id })
    }

    /// The first terminal in insertion order, used only when opening a checkout.
    pub fn initial_terminal_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<ResolvedTerminalPath, ResourceError> {
        self.paths_for_workspace(workspace_id)?
            .first()
            .copied()
            .ok_or_else(|| ResourceError::Invariant("workspace has no terminal".into()))
    }

    pub fn open_terminal_paths_for_tab(
        &self,
        tab_id: TabId,
    ) -> Result<Vec<ResolvedTerminalPath>, ResourceError> {
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        Ok(self
            .tab_panes(tab_id)
            .iter()
            .filter_map(|pane_id| {
                let pane = &self.panes[pane_id];
                (!self.pane_is_closing(*pane_id)).then_some(ResolvedTerminalPath {
                    session_id: workspace.session_id,
                    workspace_id: tab.workspace_id,
                    tab_id,
                    pane_id: *pane_id,
                    terminal_id: pane.terminal_id,
                })
            })
            .collect())
    }

    pub fn open_layout_for_tab(&self, tab_id: TabId) -> Result<SplitTree, ResourceError> {
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        tab.layout
            .as_ref()
            .expect("live tab has a layout")
            .retained(|pane_id| self.panes.contains_key(&pane_id) && !self.pane_is_closing(pane_id))
            .ok_or_else(|| ResourceError::Invariant("tab has no open pane layout".into()))
    }

    pub fn available_session_name(&self, suggested: &str) -> String {
        disambiguate(suggested, |name| {
            self.sessions.values().any(|item| item.name == name)
        })
    }

    pub fn workspace_root(&self, workspace_id: WorkspaceId) -> Result<&Path, ResourceError> {
        self.workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.root.as_path())
            .ok_or(ResourceError::NotFound("workspace"))
    }

    pub fn session_id_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<SessionId, ResourceError> {
        self.workspaces
            .get(&workspace_id)
            .map(|workspace| workspace.session_id)
            .ok_or(ResourceError::NotFound("workspace"))
    }

    pub fn workspace_id_for_tab(&self, tab_id: TabId) -> Result<WorkspaceId, ResourceError> {
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        Ok(tab.workspace_id)
    }

    pub fn available_tab_name(
        &self,
        workspace_id: WorkspaceId,
        suggested: &str,
    ) -> Result<String, ResourceError> {
        let workspace = self
            .workspaces
            .get(&workspace_id)
            .ok_or(ResourceError::NotFound("workspace"))?;
        Ok(disambiguate(suggested, |name| {
            workspace.tabs.iter().any(|id| self.tabs[id].name == name)
        }))
    }

    #[must_use]
    pub fn snapshot(&self) -> ResourceSnapshot {
        ResourceSnapshot {
            revision: self.revision,
            sessions: self
                .session_order
                .iter()
                .map(|id| self.session_snapshot(*id))
                .collect(),
        }
    }

    /// Materialize already-declared extension presentation text on one live
    /// resource. Declaration and text validation live at the daemon boundary;
    /// this pure tree owns target/closing checks, cardinality, revisioning,
    /// snapshot publication, and cleanup through ordinary resource removal.
    pub fn publish_presentation_token(
        &mut self,
        target: PresentationTokenTarget,
        qualified_name: String,
        value: String,
    ) -> Result<TokenPublication, ResourceError> {
        self.replace_presentation_tokens(target, [(qualified_name, Some(value))])
    }

    /// Remove materialized extension values whose qualified token names are no
    /// longer declared. Built-in values are outside the extension registry and
    /// are retained. All resource scopes are pruned in one revision.
    pub(crate) fn prune_extension_presentation_tokens(
        &mut self,
        declared: &HashSet<String>,
    ) -> TokenPublication {
        let is_stale = |name: &String| !is_builtin_token(name) && !declared.contains(name);
        let changed = self
            .sessions
            .values()
            .any(|resource| resource.tokens.keys().any(is_stale))
            || self
                .workspaces
                .values()
                .any(|resource| resource.tokens.keys().any(is_stale))
            || self
                .tabs
                .values()
                .any(|resource| resource.tokens.keys().any(is_stale))
            || self
                .panes
                .values()
                .any(|resource| resource.tokens.keys().any(is_stale));
        if !changed {
            return TokenPublication {
                revision: self.revision,
                changed: false,
            };
        }

        self.revision += 1;
        let retain = |tokens: &mut MaterializedTokenMap| {
            tokens.retain(|name, _| is_builtin_token(name) || declared.contains(name));
        };
        for resource in self.sessions.values_mut() {
            retain(&mut resource.tokens);
        }
        for resource in self.workspaces.values_mut() {
            retain(&mut resource.tokens);
        }
        for resource in self.tabs.values_mut() {
            retain(&mut resource.tokens);
        }
        for resource in self.panes.values_mut() {
            retain(&mut resource.tokens);
        }
        TokenPublication {
            revision: self.revision,
            changed: true,
        }
    }

    /// Atomically replace Fut's internal Git presentation values for one live
    /// workspace. Empty values are absent from the snapshot, matching token
    /// rendering semantics and ensuring an error or vanished repository clears
    /// a previously materialized status in the same revision.
    pub(crate) fn publish_workspace_git_tokens(
        &mut self,
        workspace_id: WorkspaceId,
        branch: Option<String>,
        added: Option<String>,
        deleted: Option<String>,
    ) -> Result<TokenPublication, ResourceError> {
        self.replace_presentation_tokens(
            PresentationTokenTarget::Workspace(workspace_id),
            [
                (WORKSPACE_GIT_BRANCH_TOKEN.to_owned(), branch),
                (WORKSPACE_GIT_ADDED_TOKEN.to_owned(), added),
                (WORKSPACE_GIT_DELETED_TOKEN.to_owned(), deleted),
            ],
        )
    }

    fn replace_presentation_tokens(
        &mut self,
        target: PresentationTokenTarget,
        replacements: impl IntoIterator<Item = (String, Option<String>)>,
    ) -> Result<TokenPublication, ResourceError> {
        let replacements = replacements.into_iter().collect::<BTreeMap<_, _>>();
        let tokens = self.presentation_tokens_for(target)?;
        let changed = replacements.iter().any(|(name, value)| match value {
            Some(value) => tokens.get(name) != Some(value),
            None => tokens.contains_key(name),
        });
        if !changed {
            return Ok(TokenPublication {
                revision: self.revision,
                changed: false,
            });
        }

        let removed_extension_values = replacements
            .iter()
            .filter(|(name, value)| {
                value.is_none() && tokens.contains_key(*name) && !is_builtin_token(name)
            })
            .count();
        let added_extension_values = replacements
            .iter()
            .filter(|(name, value)| {
                value.is_some() && !tokens.contains_key(*name) && !is_builtin_token(name)
            })
            .count();
        if self
            .materialized_extension_token_value_count()
            .saturating_sub(removed_extension_values)
            .saturating_add(added_extension_values)
            > MAX_MATERIALIZED_TOKEN_VALUES
        {
            return Err(ResourceError::TooManyMaterializedTokens);
        }

        self.revision += 1;
        let tokens = self.presentation_tokens_for_mut(target);
        for (name, value) in replacements {
            if let Some(value) = value {
                tokens.insert(name, value);
            } else {
                tokens.remove(&name);
            }
        }
        Ok(TokenPublication {
            revision: self.revision,
            changed: true,
        })
    }

    fn materialized_extension_token_value_count(&self) -> usize {
        self.sessions
            .values()
            .map(|resource| extension_token_count(&resource.tokens))
            .chain(
                self.workspaces
                    .values()
                    .map(|resource| extension_token_count(&resource.tokens)),
            )
            .chain(
                self.tabs
                    .values()
                    .map(|resource| extension_token_count(&resource.tokens)),
            )
            .chain(
                self.panes
                    .values()
                    .map(|resource| extension_token_count(&resource.tokens)),
            )
            .sum()
    }

    fn presentation_tokens_for(
        &self,
        target: PresentationTokenTarget,
    ) -> Result<&MaterializedTokenMap, ResourceError> {
        match target {
            PresentationTokenTarget::Session(id) => {
                let session = self
                    .sessions
                    .get(&id)
                    .ok_or(ResourceError::NotFound("session"))?;
                if self.session_is_closing(id) {
                    return Err(ResourceError::Closing("session"));
                }
                Ok(&session.tokens)
            }
            PresentationTokenTarget::Workspace(id) => {
                let workspace = self
                    .workspaces
                    .get(&id)
                    .ok_or(ResourceError::NotFound("workspace"))?;
                if self.session_is_closing(workspace.session_id) {
                    return Err(ResourceError::Closing("session"));
                }
                if self.workspace_is_closing(id) {
                    return Err(ResourceError::Closing("workspace"));
                }
                Ok(&workspace.tokens)
            }
            PresentationTokenTarget::Tab(id) => {
                let tab = self.tabs.get(&id).ok_or(ResourceError::NotFound("tab"))?;
                let workspace = &self.workspaces[&tab.workspace_id];
                if self.session_is_closing(workspace.session_id) {
                    return Err(ResourceError::Closing("session"));
                }
                if self.workspace_is_closing(tab.workspace_id) {
                    return Err(ResourceError::Closing("workspace"));
                }
                if self.tab_is_closing(id) {
                    return Err(ResourceError::Closing("tab"));
                }
                Ok(&tab.tokens)
            }
            PresentationTokenTarget::Pane(id) => {
                let pane = self.panes.get(&id).ok_or(ResourceError::NotFound("pane"))?;
                let tab = &self.tabs[&pane.tab_id];
                let workspace = &self.workspaces[&tab.workspace_id];
                if self.session_is_closing(workspace.session_id) {
                    return Err(ResourceError::Closing("session"));
                }
                if self.workspace_is_closing(tab.workspace_id) {
                    return Err(ResourceError::Closing("workspace"));
                }
                if self.tab_is_closing(pane.tab_id) {
                    return Err(ResourceError::Closing("tab"));
                }
                if self.pane_is_closing(id) {
                    return Err(ResourceError::Closing("pane"));
                }
                Ok(&pane.tokens)
            }
        }
    }

    fn presentation_tokens_for_mut(
        &mut self,
        target: PresentationTokenTarget,
    ) -> &mut MaterializedTokenMap {
        match target {
            PresentationTokenTarget::Session(id) => &mut self.sessions.get_mut(&id).unwrap().tokens,
            PresentationTokenTarget::Workspace(id) => {
                &mut self.workspaces.get_mut(&id).unwrap().tokens
            }
            PresentationTokenTarget::Tab(id) => &mut self.tabs.get_mut(&id).unwrap().tokens,
            PresentationTokenTarget::Pane(id) => &mut self.panes.get_mut(&id).unwrap().tokens,
        }
    }

    pub fn report_agent(
        &mut self,
        terminal_id: TerminalId,
        report: AgentReport,
        now_ms: u64,
    ) -> Result<u64, ResourceError> {
        self.report_agent_with_metadata(terminal_id, report, AgentReportMetadata::default(), now_ms)
    }

    pub fn agent_activity(&self, terminal_id: TerminalId) -> Result<&AgentActivity, ResourceError> {
        let pane_id = self
            .terminals
            .get(&terminal_id)
            .ok_or(ResourceError::NotFound("terminal"))?;
        self.panes
            .get(pane_id)
            .map(|pane| &pane.activity)
            .ok_or_else(|| ResourceError::Invariant("terminal pane is missing".into()))
    }

    pub fn acknowledge_agent(
        &mut self,
        terminal_id: TerminalId,
        event_revision: u64,
    ) -> Result<Option<u64>, ResourceError> {
        let pane_id = *self
            .terminals
            .get(&terminal_id)
            .ok_or(ResourceError::NotFound("terminal"))?;
        let activity = &mut self
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| ResourceError::Invariant("terminal pane is missing".into()))?
            .activity;
        let Some(attention) = activity.attention() else {
            return Ok(None);
        };
        if attention.revision != event_revision || activity.read_revision >= event_revision {
            return Ok(None);
        }
        activity.read_revision = event_revision;
        self.revision += 1;
        Ok(Some(self.revision))
    }

    pub fn update_agent_detection(
        &mut self,
        terminal_id: TerminalId,
        detection: Option<AgentDetection>,
        state: AgentState,
        now_ms: u64,
    ) -> Result<u64, ResourceError> {
        let pane_id = *self
            .terminals
            .get(&terminal_id)
            .ok_or(ResourceError::NotFound("terminal"))?;
        let activity = &mut self
            .panes
            .get_mut(&pane_id)
            .expect("terminal pane exists")
            .activity;
        if activity.has_active_integration()
            || (activity.detection == detection && activity.state == state)
        {
            return Ok(self.revision);
        }
        let completed = activity.detection.is_some()
            && activity.state == AgentState::Working
            && state == AgentState::Idle
            && detection.is_some();
        self.revision += 1;
        activity.detection = detection;
        activity.state = state;
        activity.revision = self.revision;
        activity.updated_at_ms = now_ms;
        if completed {
            activity.last_event = Some(AgentEvent {
                revision: self.revision,
                kind: AgentReport::Completed,
                occurred_at_ms: now_ms,
                turn_id: None,
            });
        }
        Ok(self.revision)
    }

    /// Record the readable foreground process for a pane. Automatic tab names
    /// belong to the resource tree, rather than being guessed independently by
    /// each client, and only follow the tab's focused pane.
    pub fn update_process_name(
        &mut self,
        terminal_id: TerminalId,
        process_name: String,
    ) -> Result<u64, ResourceError> {
        let pane_id = *self
            .terminals
            .get(&terminal_id)
            .ok_or(ResourceError::NotFound("terminal"))?;
        let tab_id = self.panes[&pane_id].tab_id;
        let tab = self.tabs.get_mut(&tab_id).expect("pane tab exists");
        if !tab.name.is_empty() || tab.focused_pane != pane_id || tab.automatic_name == process_name
        {
            return Ok(self.revision);
        }
        tab.automatic_name = process_name;
        self.revision += 1;
        Ok(self.revision)
    }

    /// Record the observed working directory of a pane's foreground process so
    /// clients can present live locations without probing terminals themselves.
    pub fn update_pane_cwd(
        &mut self,
        terminal_id: TerminalId,
        cwd: PathBuf,
        worktree: Option<PathBuf>,
    ) -> Result<u64, ResourceError> {
        let pane_id = *self
            .terminals
            .get(&terminal_id)
            .ok_or(ResourceError::NotFound("terminal"))?;
        let pane = self.panes.get_mut(&pane_id).expect("terminal pane exists");
        if pane.cwd.as_ref() == Some(&cwd) && pane.worktree == worktree {
            return Ok(self.revision);
        }
        pane.cwd = Some(cwd);
        pane.worktree = worktree;
        self.revision += 1;
        Ok(self.revision)
    }

    /// Select which pane supplies an unnamed tab's automatic title.
    pub fn focus_pane(&mut self, pane_id: PaneId) -> Result<u64, ResourceError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?;
        let tab = self.tabs.get_mut(&pane.tab_id).expect("pane tab exists");
        if tab.focused_pane == pane_id {
            return Ok(self.revision);
        }
        tab.focused_pane = pane_id;
        tab.automatic_name.clear();
        self.revision += 1;
        Ok(self.revision)
    }

    pub fn report_agent_with_metadata(
        &mut self,
        terminal_id: TerminalId,
        report: AgentReport,
        metadata: AgentReportMetadata,
        now_ms: u64,
    ) -> Result<u64, ResourceError> {
        metadata
            .validate()
            .map_err(ResourceError::InvalidAgentReport)?;
        let pane_id = *self
            .terminals
            .get(&terminal_id)
            .ok_or(ResourceError::NotFound("terminal"))?;
        self.revision += 1;
        let revision = self.revision;
        let activity = &mut self
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| ResourceError::Invariant("terminal pane is missing".into()))?
            .activity;
        let integration = activity
            .integration
            .get_or_insert_with(AgentIntegration::default);
        integration.active = report != AgentReport::Exited;
        activity.detection = None;
        if metadata.source.is_some() {
            integration.source.clone_from(&metadata.source);
        }
        if metadata.agent_session_id.is_some() {
            integration
                .agent_session_id
                .clone_from(&metadata.agent_session_id);
        }
        activity.state = match report {
            AgentReport::Idle | AgentReport::Completed | AgentReport::Exited => AgentState::Idle,
            AgentReport::Working => AgentState::Working,
            AgentReport::Blocked => AgentState::Blocked,
        };
        activity.revision = revision;
        activity.updated_at_ms = now_ms;
        activity.last_event = Some(AgentEvent {
            revision,
            kind: report,
            occurred_at_ms: now_ms,
            turn_id: metadata.turn_id,
        });
        Ok(revision)
    }

    pub fn create_session(&mut self, path: InitialPath) -> Result<Mutation, ResourceError> {
        self.check_names([&path.session_name])?;
        check_optional_name(&path.workspace_name)?;
        check_optional_name(&path.tab_name)?;
        if self.sessions.values().any(|s| s.name == path.session_name) {
            return Err(ResourceError::Duplicate("session name"));
        }
        if self
            .sessions
            .values()
            .any(|s| s.project.identity == path.project.identity)
        {
            return Err(ResourceError::Duplicate("project identity"));
        }
        if self
            .workspaces
            .values()
            .any(|workspace| workspace.root == path.root)
        {
            return Err(ResourceError::Duplicate("workspace root"));
        }
        self.check_ids(
            path.session_id,
            path.workspace_id,
            path.tab_id,
            path.pane_id,
            path.terminal_id,
        )?;

        let session_event = ResourceEvent::SessionCreated {
            id: path.session_id,
            name: path.session_name.clone(),
            project: path.project.clone(),
            trusted_project_config: path.trusted_project_config.clone(),
        };
        self.sessions.insert(
            path.session_id,
            Session {
                name: path.session_name,
                project: path.project,
                trusted_project_config: path.trusted_project_config.clone(),
                tokens: BTreeMap::new(),
                workspaces: vec![path.workspace_id],
            },
        );
        self.session_order.push(path.session_id);
        self.workspaces.insert(
            path.workspace_id,
            Workspace {
                session_id: path.session_id,
                name: path.workspace_name,
                root: path.root,
                tokens: BTreeMap::new(),
                tabs: vec![path.tab_id],
            },
        );
        self.tabs.insert(
            path.tab_id,
            Tab {
                workspace_id: path.workspace_id,
                name: path.tab_name,
                automatic_name: String::new(),
                focused_pane: path.pane_id,
                tokens: BTreeMap::new(),
                layout: Some(SplitTree::leaf(path.pane_id)),
            },
        );
        self.insert_pane(path.tab_id, path.pane_id, path.terminal_id);
        Ok(self.finish(
            vec![
                session_event,
                ResourceEvent::WorkspaceCreated {
                    session_id: path.session_id,
                    id: path.workspace_id,
                    name: self.workspaces[&path.workspace_id].name.clone(),
                    root: self.workspaces[&path.workspace_id].root.clone(),
                    trusted_project_config: path.trusted_project_config,
                },
                ResourceEvent::TabCreated {
                    workspace_id: path.workspace_id,
                    id: path.tab_id,
                    name: self.tabs[&path.tab_id].name.clone(),
                },
                ResourceEvent::PaneCreated {
                    tab_id: path.tab_id,
                    id: path.pane_id,
                    terminal_id: path.terminal_id,
                    closing: false,
                },
            ],
            vec![],
        ))
    }

    pub fn rename_session(
        &mut self,
        session_id: SessionId,
        new_name: String,
    ) -> Result<Mutation, ResourceError> {
        self.check_names([&new_name])?;
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if self.session_is_closing(session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if session.name == new_name {
            return Ok(self.unchanged());
        }
        if self
            .sessions
            .values()
            .any(|session| session.name == new_name)
        {
            return Err(ResourceError::Duplicate("session name"));
        }

        let old_name = std::mem::replace(
            &mut self.sessions.get_mut(&session_id).unwrap().name,
            new_name.clone(),
        );
        Ok(self.finish(
            vec![ResourceEvent::SessionRenamed {
                id: session_id,
                old_name,
                new_name,
            }],
            vec![],
        ))
    }

    pub fn rename_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        new_name: String,
    ) -> Result<Mutation, ResourceError> {
        check_optional_name(&new_name)?;
        let workspace = self
            .workspaces
            .get(&workspace_id)
            .ok_or(ResourceError::NotFound("workspace"))?;
        let session_id = workspace.session_id;
        if self.session_is_closing(session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if workspace.name == new_name {
            return Ok(self.unchanged());
        }
        if !new_name.is_empty()
            && self.sessions[&session_id]
                .workspaces
                .iter()
                .any(|id| *id != workspace_id && self.workspaces[id].name == new_name)
        {
            return Err(ResourceError::Duplicate("workspace name"));
        }

        let old_name = std::mem::replace(
            &mut self.workspaces.get_mut(&workspace_id).unwrap().name,
            new_name.clone(),
        );
        let root = self.workspaces[&workspace_id].root.clone();
        let trusted_project_config = self.sessions[&session_id].trusted_project_config.clone();
        Ok(self.finish(
            vec![ResourceEvent::WorkspaceRenamed {
                session_id,
                id: workspace_id,
                root,
                old_name,
                new_name,
                trusted_project_config,
            }],
            vec![],
        ))
    }

    pub fn rename_tab(
        &mut self,
        tab_id: TabId,
        new_name: String,
    ) -> Result<Mutation, ResourceError> {
        check_optional_name(&new_name)?;
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        let workspace_id = tab.workspace_id;
        let workspace = &self.workspaces[&workspace_id];
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if tab.name == new_name {
            return Ok(self.unchanged());
        }
        if self.tab_is_closing(tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        if !new_name.is_empty()
            && workspace
                .tabs
                .iter()
                .any(|id| *id != tab_id && self.tabs[id].name == new_name)
        {
            return Err(ResourceError::Duplicate("tab name"));
        }

        let tab = self.tabs.get_mut(&tab_id).unwrap();
        let old_name = std::mem::replace(&mut tab.name, new_name.clone());
        tab.automatic_name.clear();
        Ok(self.finish(
            vec![ResourceEvent::TabRenamed {
                workspace_id,
                id: tab_id,
                old_name,
                new_name,
            }],
            vec![],
        ))
    }

    pub fn add_workspace(
        &mut self,
        session_id: SessionId,
        path: WorkspacePath,
    ) -> Result<Mutation, ResourceError> {
        check_optional_name(&path.workspace_name)?;
        check_optional_name(&path.tab_name)?;
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if self.session_is_closing(session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if !path.workspace_name.is_empty()
            && session
                .workspaces
                .iter()
                .any(|id| self.workspaces[id].name == path.workspace_name)
        {
            return Err(ResourceError::Duplicate("workspace name"));
        }
        if self
            .workspaces
            .values()
            .any(|workspace| workspace.root == path.root && workspace.session_id != session_id)
        {
            return Err(ResourceError::Duplicate(
                "workspace root in another session",
            ));
        }
        self.check_child_ids(
            path.workspace_id,
            path.tab_id,
            path.pane_id,
            path.terminal_id,
        )?;
        self.sessions
            .get_mut(&session_id)
            .unwrap()
            .workspaces
            .push(path.workspace_id);
        self.workspaces.insert(
            path.workspace_id,
            Workspace {
                session_id,
                name: path.workspace_name,
                root: path.root,
                tokens: BTreeMap::new(),
                tabs: vec![path.tab_id],
            },
        );
        self.tabs.insert(
            path.tab_id,
            Tab {
                workspace_id: path.workspace_id,
                name: path.tab_name,
                automatic_name: String::new(),
                focused_pane: path.pane_id,
                tokens: BTreeMap::new(),
                layout: Some(SplitTree::leaf(path.pane_id)),
            },
        );
        self.insert_pane(path.tab_id, path.pane_id, path.terminal_id);
        Ok(self.finish(
            vec![
                ResourceEvent::WorkspaceCreated {
                    session_id,
                    id: path.workspace_id,
                    name: self.workspaces[&path.workspace_id].name.clone(),
                    root: self.workspaces[&path.workspace_id].root.clone(),
                    trusted_project_config: self.sessions[&session_id]
                        .trusted_project_config
                        .clone(),
                },
                ResourceEvent::TabCreated {
                    workspace_id: path.workspace_id,
                    id: path.tab_id,
                    name: self.tabs[&path.tab_id].name.clone(),
                },
                ResourceEvent::PaneCreated {
                    tab_id: path.tab_id,
                    id: path.pane_id,
                    terminal_id: path.terminal_id,
                    closing: false,
                },
            ],
            vec![],
        ))
    }

    pub fn add_tab(
        &mut self,
        workspace_id: WorkspaceId,
        path: TabPath,
    ) -> Result<Mutation, ResourceError> {
        check_optional_name(&path.tab_name)?;
        let workspace = self
            .workspaces
            .get(&workspace_id)
            .ok_or(ResourceError::NotFound("workspace"))?;
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if !path.tab_name.is_empty()
            && workspace
                .tabs
                .iter()
                .any(|id| self.tabs[id].name == path.tab_name)
        {
            return Err(ResourceError::Duplicate("tab name"));
        }
        if self.tabs.contains_key(&path.tab_id) {
            return Err(ResourceError::Duplicate("tab id"));
        }
        self.check_pane_ids(path.pane_id, path.terminal_id)?;
        self.workspaces
            .get_mut(&workspace_id)
            .unwrap()
            .tabs
            .push(path.tab_id);
        self.tabs.insert(
            path.tab_id,
            Tab {
                workspace_id,
                name: path.tab_name,
                automatic_name: String::new(),
                focused_pane: path.pane_id,
                tokens: BTreeMap::new(),
                layout: Some(SplitTree::leaf(path.pane_id)),
            },
        );
        self.insert_pane(path.tab_id, path.pane_id, path.terminal_id);
        Ok(self.finish(
            vec![
                ResourceEvent::TabCreated {
                    workspace_id,
                    id: path.tab_id,
                    name: self.tabs[&path.tab_id].name.clone(),
                },
                ResourceEvent::PaneCreated {
                    tab_id: path.tab_id,
                    id: path.pane_id,
                    terminal_id: path.terminal_id,
                    closing: false,
                },
            ],
            vec![],
        ))
    }

    pub fn add_pane(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        terminal_id: TerminalId,
    ) -> Result<Mutation, ResourceError> {
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        if self.session_is_closing(self.workspaces[&tab.workspace_id].session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        self.check_pane_ids(pane_id, terminal_id)?;
        let anchor = tab
            .layout
            .as_ref()
            .expect("live tab has a layout")
            .leaf_ids()
            .last()
            .copied()
            .expect("live tab has a pane");
        let tab = self.tabs.get_mut(&tab_id).unwrap();
        assert!(tab.layout.as_mut().expect("live tab has a layout").split(
            anchor,
            SplitDirection::Right,
            pane_id
        ));
        self.insert_pane(tab_id, pane_id, terminal_id);
        Ok(self.finish(
            vec![ResourceEvent::PaneCreated {
                tab_id,
                id: pane_id,
                terminal_id,
                closing: false,
            }],
            vec![],
        ))
    }

    pub fn split_pane(
        &mut self,
        anchor: PaneId,
        direction: SplitDirection,
        pane_id: PaneId,
        terminal_id: TerminalId,
    ) -> Result<Mutation, ResourceError> {
        let anchor_pane = self
            .panes
            .get(&anchor)
            .ok_or(ResourceError::NotFound("pane"))?
            .clone();
        if self.pane_is_closing(anchor) {
            return Err(ResourceError::Closing("pane"));
        }
        let tab_id = anchor_pane.tab_id;
        let tab = &self.tabs[&tab_id];
        if self.session_is_closing(self.workspaces[&tab.workspace_id].session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        self.check_pane_ids(pane_id, terminal_id)?;
        let tab = self.tabs.get_mut(&tab_id).unwrap();
        assert!(
            tab.layout
                .as_mut()
                .expect("live tab has a layout")
                .split(anchor, direction, pane_id)
        );
        self.insert_pane(tab_id, pane_id, terminal_id);
        Ok(self.finish(
            vec![ResourceEvent::PaneCreated {
                tab_id,
                id: pane_id,
                terminal_id,
                closing: false,
            }],
            vec![],
        ))
    }

    pub fn resize_split(
        &mut self,
        tab_id: TabId,
        split_id: SplitId,
        ratio: SplitRatio,
    ) -> Result<Mutation, ResourceError> {
        if !ratio.is_valid() {
            return Err(ResourceError::InvalidSplitRatio);
        }
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        if self.session_is_closing(self.workspaces[&tab.workspace_id].session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        let current = tab
            .layout
            .as_ref()
            .expect("live tab has a layout")
            .ratio(split_id)
            .ok_or(ResourceError::NotFound("split"))?;
        if current == ratio {
            return Ok(self.unchanged());
        }
        assert!(
            self.tabs
                .get_mut(&tab_id)
                .expect("validated tab exists")
                .layout
                .as_mut()
                .expect("live tab has a layout")
                .resize(split_id, ratio)
        );
        Ok(self.finish(
            vec![ResourceEvent::SplitResized {
                tab_id,
                split_id,
                ratio,
            }],
            vec![],
        ))
    }

    pub fn move_pane(
        &mut self,
        pane_id: PaneId,
        destination: TabId,
    ) -> Result<Mutation, ResourceError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?
            .clone();
        let source_tab = &self.tabs[&pane.tab_id];
        let destination_tab = self
            .tabs
            .get(&destination)
            .ok_or(ResourceError::NotFound("tab"))?;
        if self.tab_is_closing(pane.tab_id) || self.tab_is_closing(destination) {
            return Err(ResourceError::Closing("tab"));
        }
        if self.pane_is_closing(pane_id) {
            return Err(ResourceError::Closing("pane"));
        }
        let source_workspace = source_tab.workspace_id;
        if source_workspace != destination_tab.workspace_id {
            return Err(ResourceError::DifferentWorkspace);
        }
        if self.session_is_closing(self.workspaces[&source_workspace].session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(source_workspace) {
            return Err(ResourceError::Closing("workspace"));
        }
        if pane.tab_id == destination {
            return Ok(self.unchanged());
        }
        let source = self.tabs.get_mut(&pane.tab_id).unwrap();
        source.layout = source
            .layout
            .take()
            .expect("live source tab has a layout")
            .without(pane_id);
        if source.focused_pane == pane_id
            && let Some(replacement) = source
                .layout
                .as_ref()
                .and_then(|layout| layout.leaf_ids().last().copied())
        {
            source.focused_pane = replacement;
            source.automatic_name.clear();
        }
        let destination_tab = self.tabs.get_mut(&destination).unwrap();
        let anchor = *destination_tab
            .layout
            .as_ref()
            .expect("live destination tab has a layout")
            .leaf_ids()
            .last()
            .expect("destination tab has a pane");
        assert!(
            destination_tab
                .layout
                .as_mut()
                .expect("live destination tab has a layout")
                .split(anchor, SplitDirection::Right, pane_id)
        );
        self.panes.get_mut(&pane_id).unwrap().tab_id = destination;
        let mut events = vec![ResourceEvent::PaneMoved {
            pane_id,
            terminal_id: pane.terminal_id,
            from: pane.tab_id,
            to: destination,
        }];
        self.cascade_empty(pane.tab_id, &mut events);
        Ok(self.finish(events, vec![]))
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> Result<Mutation, ResourceError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?
            .clone();
        let tab = &self.tabs[&pane.tab_id];
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(pane.tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        if self.pane_is_closing(pane_id) {
            return Err(ResourceError::Closing("pane"));
        }
        self.close_intents.insert(CloseIntent::Pane(pane_id));
        let terminal_id = pane.terminal_id;
        Ok(self.finish(
            vec![ResourceEvent::PaneCloseRequested {
                pane_id,
                terminal_id,
            }],
            vec![terminal_id],
        ))
    }

    pub fn cancel_close_pane(&mut self, pane_id: PaneId) -> Result<Mutation, ResourceError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?
            .clone();
        let session_id = self.workspaces[&self.tabs[&pane.tab_id].workspace_id].session_id;
        if self.session_is_closing(session_id) {
            return Err(ResourceError::Closing("session"));
        }
        let workspace_id = self.tabs[&pane.tab_id].workspace_id;
        if self.workspace_is_closing(workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(pane.tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        if !self.close_intents.contains(&CloseIntent::Pane(pane_id)) {
            return Err(ResourceError::NotFound("pending pane close"));
        }
        self.close_intents.remove(&CloseIntent::Pane(pane_id));
        Ok(self.finish(
            vec![ResourceEvent::PaneCloseCancelled {
                pane_id,
                terminal_id: pane.terminal_id,
            }],
            vec![],
        ))
    }

    pub fn close_tab(&mut self, tab_id: TabId) -> Result<Mutation, ResourceError> {
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        if self
            .tab_panes(tab_id)
            .iter()
            .any(|id| self.pane_is_closing(*id))
        {
            return Err(ResourceError::Closing("pane"));
        }
        let panes = tab
            .layout
            .as_ref()
            .expect("live tab has a layout")
            .leaf_ids();
        let terminals = panes.iter().map(|id| self.panes[id].terminal_id).collect();
        self.close_intents.insert(CloseIntent::Tab(tab_id));
        Ok(self.finish(vec![ResourceEvent::TabCloseRequested { tab_id }], terminals))
    }

    pub fn cancel_close_tab(&mut self, tab_id: TabId) -> Result<Mutation, ResourceError> {
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if !self.close_intents.contains(&CloseIntent::Tab(tab_id)) {
            return Err(ResourceError::NotFound("pending tab close"));
        }
        self.close_intents.remove(&CloseIntent::Tab(tab_id));
        Ok(self.finish(vec![ResourceEvent::TabCloseCancelled { tab_id }], vec![]))
    }

    pub fn close_session(&mut self, session_id: SessionId) -> Result<Mutation, ResourceError> {
        self.sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if self.session_is_closing(session_id) {
            return Err(ResourceError::Closing("session"));
        }
        let panes = self.session_panes(session_id);
        if panes.iter().any(|id| self.pane_is_closing(*id)) {
            return Err(ResourceError::Closing("pane"));
        }
        let terminals: Vec<_> = panes.iter().map(|id| self.panes[id].terminal_id).collect();
        self.close_intents.insert(CloseIntent::Session(session_id));
        Ok(self.finish(
            vec![ResourceEvent::SessionCloseRequested { session_id }],
            terminals,
        ))
    }

    /// Rolls back the pending state if the runtime could not submit a session close request.
    pub fn cancel_close_session(
        &mut self,
        session_id: SessionId,
    ) -> Result<Mutation, ResourceError> {
        self.sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if !self
            .close_intents
            .contains(&CloseIntent::Session(session_id))
        {
            return Err(ResourceError::NotFound("pending session close"));
        }
        self.close_intents.remove(&CloseIntent::Session(session_id));
        Ok(self.finish(
            vec![ResourceEvent::SessionCloseCancelled { session_id }],
            vec![],
        ))
    }

    pub fn close_workspace(
        &mut self,
        workspace_id: WorkspaceId,
    ) -> Result<Mutation, ResourceError> {
        let workspace = self
            .workspaces
            .get(&workspace_id)
            .ok_or(ResourceError::NotFound("workspace"))?;
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        let panes = self.workspace_panes(workspace_id);
        if panes.iter().any(|id| self.pane_is_closing(*id)) {
            return Err(ResourceError::Closing("pane"));
        }
        let terminals = panes.iter().map(|id| self.panes[id].terminal_id).collect();
        self.close_intents
            .insert(CloseIntent::Workspace(workspace_id));
        Ok(self.finish(
            vec![ResourceEvent::WorkspaceCloseRequested { workspace_id }],
            terminals,
        ))
    }

    pub fn cancel_close_workspace(
        &mut self,
        workspace_id: WorkspaceId,
    ) -> Result<Mutation, ResourceError> {
        let workspace = self
            .workspaces
            .get(&workspace_id)
            .ok_or(ResourceError::NotFound("workspace"))?;
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if !self
            .close_intents
            .contains(&CloseIntent::Workspace(workspace_id))
        {
            return Err(ResourceError::NotFound("pending workspace close"));
        }
        self.close_intents
            .remove(&CloseIntent::Workspace(workspace_id));
        Ok(self.finish(
            vec![ResourceEvent::WorkspaceCloseCancelled { workspace_id }],
            vec![],
        ))
    }

    pub fn terminal_exited(&mut self, terminal_id: TerminalId) -> Result<Mutation, ResourceError> {
        let Some(&pane_id) = self.terminals.get(&terminal_id) else {
            return Err(ResourceError::NotFound("terminal"));
        };
        let requested = self.pane_is_closing(pane_id);
        let cause = if requested {
            CloseCause::Requested
        } else {
            CloseCause::TerminalExited
        };
        let tab_id = self.remove_pane(pane_id);
        let mut events = vec![ResourceEvent::PaneClosed {
            pane_id,
            terminal_id,
            cause,
        }];
        self.cascade_empty(tab_id, &mut events);
        Ok(self.finish(events, vec![]))
    }

    pub fn fallback_terminal_ids(
        &self,
        terminal_id: TerminalId,
    ) -> Result<Vec<TerminalId>, ResourceError> {
        let pane_id = *self
            .terminals
            .get(&terminal_id)
            .ok_or(ResourceError::NotFound("terminal"))?;
        let tab_id = self.panes[&pane_id].tab_id;
        let workspace_id = self.tabs[&tab_id].workspace_id;
        let session_id = self.workspaces[&workspace_id].session_id;
        let panes = self.tab_panes(tab_id);
        let pane_index = panes
            .iter()
            .position(|candidate| *candidate == pane_id)
            .ok_or_else(|| ResourceError::Invariant("pane missing from tab".into()))?;
        let mut fallback = panes[..pane_index]
            .iter()
            .rev()
            .chain(panes[pane_index + 1..].iter())
            .filter_map(|pane_id| {
                let pane = &self.panes[pane_id];
                (!self.pane_is_closing(*pane_id)).then_some(pane.terminal_id)
            })
            .collect::<Vec<_>>();

        let workspace = &self.workspaces[&workspace_id];
        let tab_index = workspace
            .tabs
            .iter()
            .position(|candidate| *candidate == tab_id)
            .ok_or_else(|| ResourceError::Invariant("tab missing from workspace".into()))?;
        for tab_id in workspace.tabs[..tab_index]
            .iter()
            .rev()
            .chain(workspace.tabs[tab_index + 1..].iter())
        {
            if !self.tab_is_closing(*tab_id) {
                fallback.extend(self.tab_panes(*tab_id).iter().filter_map(|pane_id| {
                    let pane = &self.panes[pane_id];
                    (!self.pane_is_closing(*pane_id)).then_some(pane.terminal_id)
                }));
            }
        }

        let session = &self.sessions[&session_id];
        let workspace_index = session
            .workspaces
            .iter()
            .position(|candidate| *candidate == workspace_id)
            .ok_or_else(|| ResourceError::Invariant("workspace missing from session".into()))?;
        for workspace_id in session.workspaces[..workspace_index]
            .iter()
            .rev()
            .chain(session.workspaces[workspace_index + 1..].iter())
        {
            if self.workspace_is_closing(*workspace_id) {
                continue;
            }
            for tab_id in &self.workspaces[workspace_id].tabs {
                if self.tab_is_closing(*tab_id) {
                    continue;
                }
                for pane_id in self.tab_panes(*tab_id) {
                    let pane = &self.panes[&pane_id];
                    if !self.pane_is_closing(pane_id) {
                        fallback.push(pane.terminal_id);
                    }
                }
            }
        }
        Ok(fallback)
    }

    pub fn open_terminal_ids_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<TerminalId>, ResourceError> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if self.session_is_closing(session_id) {
            return Ok(Vec::new());
        }
        let mut terminals = Vec::new();
        for workspace_id in &session.workspaces {
            if self.workspace_is_closing(*workspace_id) {
                continue;
            }
            for tab_id in &self.workspaces[workspace_id].tabs {
                if self.tab_is_closing(*tab_id) {
                    continue;
                }
                terminals.extend(self.tab_panes(*tab_id).iter().filter_map(|pane_id| {
                    let pane = &self.panes[pane_id];
                    (!self.pane_is_closing(*pane_id)).then_some(pane.terminal_id)
                }));
            }
        }
        Ok(terminals)
    }

    pub fn validate(&self) -> Result<(), ResourceError> {
        if self.materialized_extension_token_value_count() > MAX_MATERIALIZED_TOKEN_VALUES {
            return Err(ResourceError::TooManyMaterializedTokens);
        }
        if self.session_order.len() != self.sessions.len()
            || self
                .session_order
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != self.sessions.len()
            || self
                .session_order
                .iter()
                .any(|id| !self.sessions.contains_key(id))
        {
            return self.invalid("session order coverage");
        }
        let mut session_names = BTreeSet::new();
        let mut projects = BTreeSet::new();
        let mut roots = BTreeMap::new();
        let mut seen_workspaces = BTreeSet::new();
        let mut seen_tabs = BTreeSet::new();
        let mut seen_panes = BTreeSet::new();
        for sid in &self.session_order {
            let session = &self.sessions[sid];
            if session.name.trim().is_empty()
                || session.workspaces.is_empty()
                || !session_names.insert(&session.name)
                || !projects.insert(&session.project.identity)
                || !unique(&session.workspaces)
            {
                return self.invalid("session fields or children");
            }
            let mut workspace_names = BTreeSet::new();
            for wid in &session.workspaces {
                let Some(workspace) = self.workspaces.get(wid) else {
                    return self.invalid("missing workspace");
                };
                if workspace.session_id != *sid
                    || check_optional_name(&workspace.name).is_err()
                    || workspace.tabs.is_empty()
                    || (!workspace.name.is_empty() && !workspace_names.insert(&workspace.name))
                    || roots
                        .insert(&workspace.root, *sid)
                        .is_some_and(|owner| owner != *sid)
                    || !seen_workspaces.insert(*wid)
                    || !unique(&workspace.tabs)
                {
                    return self.invalid("workspace fields or parent");
                }
                let mut tab_names = BTreeSet::new();
                for tid in &workspace.tabs {
                    let Some(tab) = self.tabs.get(tid) else {
                        return self.invalid("missing tab");
                    };
                    if tab.workspace_id != *wid
                        || check_optional_name(&tab.name).is_err()
                        || !tab.layout.as_ref().is_some_and(SplitTree::validate)
                        || (!tab.name.is_empty() && !tab_names.insert(&tab.name))
                        || !seen_tabs.insert(*tid)
                    {
                        return self.invalid("tab fields or parent");
                    }
                    for pid in self.tab_panes(*tid) {
                        let Some(pane) = self.panes.get(&pid) else {
                            return self.invalid("missing pane");
                        };
                        if pane.tab_id != *tid
                            || !seen_panes.insert(pid)
                            || self.terminals.get(&pane.terminal_id) != Some(&pid)
                        {
                            return self.invalid("pane fields or parent");
                        }
                    }
                }
            }
        }
        if seen_workspaces.len() != self.workspaces.len()
            || seen_tabs.len() != self.tabs.len()
            || seen_panes.len() != self.panes.len()
            || self.terminals.len() != self.panes.len()
        {
            return self.invalid("unreachable resource or cardinality");
        }
        if self.terminals.iter().any(|(terminal, pane)| {
            self.panes
                .get(pane)
                .is_none_or(|p| p.terminal_id != *terminal)
        }) {
            return self.invalid("terminal reverse reference");
        }
        if self.close_intents.iter().any(|intent| match intent {
            CloseIntent::Session(id) => !self.sessions.contains_key(id),
            CloseIntent::Workspace(id) => !self.workspaces.contains_key(id),
            CloseIntent::Tab(id) => !self.tabs.contains_key(id),
            CloseIntent::Pane(id) => !self.panes.contains_key(id),
        }) {
            return self.invalid("close intent refers to missing resource");
        }
        Ok(())
    }

    fn remove_pane(&mut self, pane_id: PaneId) -> TabId {
        let pane = self.panes.remove(&pane_id).unwrap();
        self.terminals.remove(&pane.terminal_id);
        self.close_intents.remove(&CloseIntent::Pane(pane_id));
        let tab = self.tabs.get_mut(&pane.tab_id).unwrap();
        tab.layout = tab
            .layout
            .take()
            .expect("pane tab has a layout")
            .without(pane_id);
        if tab.focused_pane == pane_id
            && let Some(replacement) = tab
                .layout
                .as_ref()
                .and_then(|layout| layout.leaf_ids().last().copied())
        {
            tab.focused_pane = replacement;
            tab.automatic_name.clear();
        }
        pane.tab_id
    }

    fn cascade_empty(&mut self, tab_id: TabId, events: &mut Vec<ResourceEvent>) {
        if self.tabs[&tab_id].layout.is_some() {
            return;
        }
        let workspace_id = self.tabs.remove(&tab_id).unwrap().workspace_id;
        self.close_intents.remove(&CloseIntent::Tab(tab_id));
        self.workspaces
            .get_mut(&workspace_id)
            .unwrap()
            .tabs
            .retain(|id| *id != tab_id);
        events.push(ResourceEvent::TabClosed { tab_id });
        if !self.workspaces[&workspace_id].tabs.is_empty() {
            return;
        }
        let workspace = self.workspaces.remove(&workspace_id).unwrap();
        self.close_intents
            .remove(&CloseIntent::Workspace(workspace_id));
        let session_id = workspace.session_id;
        let trusted_project_config = self.sessions[&session_id].trusted_project_config.clone();
        self.sessions
            .get_mut(&session_id)
            .unwrap()
            .workspaces
            .retain(|id| *id != workspace_id);
        events.push(ResourceEvent::WorkspaceClosed {
            session_id,
            workspace_id,
            name: workspace.name,
            root: workspace.root,
            trusted_project_config,
        });
        if !self.sessions[&session_id].workspaces.is_empty() {
            return;
        }
        self.sessions.remove(&session_id);
        self.close_intents.remove(&CloseIntent::Session(session_id));
        self.session_order.retain(|id| *id != session_id);
        events.push(ResourceEvent::SessionClosed { session_id });
    }

    fn insert_pane(&mut self, tab_id: TabId, pane_id: PaneId, terminal_id: TerminalId) {
        self.panes.insert(
            pane_id,
            Pane {
                tab_id,
                terminal_id,
                tokens: BTreeMap::new(),
                activity: AgentActivity::default(),
                cwd: None,
                worktree: None,
            },
        );
        self.terminals.insert(terminal_id, pane_id);
    }
    fn finish(
        &mut self,
        events: Vec<ResourceEvent>,
        terminals_to_close: Vec<TerminalId>,
    ) -> Mutation {
        self.revision += 1;
        Mutation {
            revision: self.revision,
            events,
            terminals_to_close,
            multiplexer_empty: self.sessions.is_empty(),
        }
    }
    fn unchanged(&self) -> Mutation {
        Mutation {
            revision: self.revision,
            events: vec![],
            terminals_to_close: vec![],
            multiplexer_empty: self.sessions.is_empty(),
        }
    }
    fn session_snapshot(&self, id: SessionId) -> SessionSnapshot {
        let s = &self.sessions[&id];
        SessionSnapshot {
            id,
            name: s.name.clone(),
            project: s.project.clone(),
            trusted_project_config: s.trusted_project_config.clone(),
            closing: self.session_is_closing(id),
            tokens: s.tokens.clone(),
            workspaces: s
                .workspaces
                .iter()
                .map(|id| self.workspace_snapshot(*id))
                .collect(),
        }
    }
    fn workspace_snapshot(&self, id: WorkspaceId) -> WorkspaceSnapshot {
        let w = &self.workspaces[&id];
        let tabs = w
            .tabs
            .iter()
            .map(|id| self.tab_snapshot(*id))
            .collect::<Vec<_>>();
        WorkspaceSnapshot {
            id,
            // Unnamed workspaces present as the place they are: the final
            // component of the location every open pane shares, or "multiple".
            name: if w.name.is_empty() {
                shared_live_location(&w.root, &tabs).map_or_else(
                    || MULTIPLE_LOCATIONS.into(),
                    |location| {
                        location.file_name().map_or_else(
                            || location.display().to_string(),
                            |name| name.to_string_lossy().into_owned(),
                        )
                    },
                )
            } else {
                w.name.clone()
            },
            root: w.root.clone(),
            closing: self.workspace_is_closing(id),
            tokens: w.tokens.clone(),
            tabs,
        }
    }
    fn tab_snapshot(&self, id: TabId) -> TabSnapshot {
        let t = &self.tabs[&id];
        TabSnapshot {
            id,
            name: if t.name.is_empty() {
                t.automatic_name.clone()
            } else {
                t.name.clone()
            },
            closing: self.tab_is_closing(id),
            tokens: t.tokens.clone(),
            layout: t.layout.clone().expect("snapshotted tab has a layout"),
            panes: t
                .layout
                .as_ref()
                .expect("snapshotted tab has a layout")
                .leaf_ids()
                .into_iter()
                .map(|id| self.pane_snapshot(id))
                .collect(),
        }
    }
    fn pane_snapshot(&self, id: PaneId) -> PaneSnapshot {
        let p = &self.panes[&id];
        PaneSnapshot {
            id,
            terminal_id: p.terminal_id,
            closing: self.pane_is_closing(id),
            tokens: p.tokens.clone(),
            activity: p.activity.clone(),
            cwd: p.cwd.clone(),
            worktree: p.worktree.clone(),
        }
    }
    fn session_panes(&self, session_id: SessionId) -> Vec<PaneId> {
        self.sessions[&session_id]
            .workspaces
            .iter()
            .flat_map(|w| &self.workspaces[w].tabs)
            .flat_map(|t| self.tab_panes(*t))
            .collect()
    }
    fn workspace_panes(&self, workspace_id: WorkspaceId) -> Vec<PaneId> {
        self.workspaces[&workspace_id]
            .tabs
            .iter()
            .flat_map(|tab| self.tab_panes(*tab))
            .collect()
    }
    fn open_paths(&self) -> Result<Vec<ResolvedTerminalPath>, ResourceError> {
        let mut paths = Vec::new();
        for id in &self.session_order {
            match self.paths_for_session(*id) {
                Ok(found) => paths.extend(found),
                Err(ResourceError::Closing(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(paths)
    }
    fn paths_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<ResolvedTerminalPath>, ResourceError> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if self.session_is_closing(session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if session.workspaces.is_empty() {
            return self.invalid("session has no workspace");
        }
        let mut paths = Vec::new();
        for workspace in &session.workspaces {
            match self.paths_for_workspace(*workspace) {
                Ok(found) => paths.extend(found),
                Err(ResourceError::Closing("workspace")) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(paths)
    }
    fn paths_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ResolvedTerminalPath>, ResourceError> {
        let workspace = self
            .workspaces
            .get(&workspace_id)
            .ok_or(ResourceError::NotFound("workspace"))?;
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if workspace.tabs.is_empty() {
            return self.invalid("workspace has no tab");
        }
        let mut paths = Vec::new();
        for tab in &workspace.tabs {
            match self.paths_for_tab(*tab) {
                Ok(found) => paths.extend(found),
                Err(ResourceError::Closing("tab")) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(paths)
    }
    fn paths_for_tab(&self, tab_id: TabId) -> Result<Vec<ResolvedTerminalPath>, ResourceError> {
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        if tab.layout.is_none() {
            return self.invalid("tab has no pane");
        }
        self.tab_panes(tab_id)
            .into_iter()
            .map(|id| self.path_for_pane(id))
            .collect()
    }
    fn path_for_pane(&self, pane_id: PaneId) -> Result<ResolvedTerminalPath, ResourceError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?;
        if self.pane_is_closing(pane_id) {
            return Err(ResourceError::Closing("pane"));
        }
        let tab = &self.tabs[&pane.tab_id];
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.session_is_closing(workspace.session_id) {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspace_is_closing(tab.workspace_id) {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tab_is_closing(pane.tab_id) {
            return Err(ResourceError::Closing("tab"));
        }
        Ok(ResolvedTerminalPath {
            session_id: workspace.session_id,
            workspace_id: tab.workspace_id,
            tab_id: pane.tab_id,
            pane_id,
            terminal_id: pane.terminal_id,
        })
    }
    fn check_names<'a>(
        &self,
        names: impl IntoIterator<Item = &'a String>,
    ) -> Result<(), ResourceError> {
        if names.into_iter().any(|n| n.trim().is_empty()) {
            Err(ResourceError::EmptyName)
        } else {
            Ok(())
        }
    }
    fn check_ids(
        &self,
        sid: SessionId,
        wid: WorkspaceId,
        tid: TabId,
        pid: PaneId,
        terminal: TerminalId,
    ) -> Result<(), ResourceError> {
        if self.sessions.contains_key(&sid) {
            return Err(ResourceError::Duplicate("session id"));
        }
        self.check_child_ids(wid, tid, pid, terminal)
    }
    fn check_child_ids(
        &self,
        wid: WorkspaceId,
        tid: TabId,
        pid: PaneId,
        terminal: TerminalId,
    ) -> Result<(), ResourceError> {
        if self.workspaces.contains_key(&wid) {
            return Err(ResourceError::Duplicate("workspace id"));
        }
        if self.tabs.contains_key(&tid) {
            return Err(ResourceError::Duplicate("tab id"));
        }
        self.check_pane_ids(pid, terminal)
    }
    fn check_pane_ids(&self, pid: PaneId, terminal: TerminalId) -> Result<(), ResourceError> {
        if self.panes.contains_key(&pid) {
            return Err(ResourceError::Duplicate("pane id"));
        }
        if self.terminals.contains_key(&terminal) {
            return Err(ResourceError::Duplicate("terminal id"));
        }
        Ok(())
    }
    fn invalid<T>(&self, message: &str) -> Result<T, ResourceError> {
        Err(ResourceError::Invariant(message.into()))
    }
}

fn unique<T: Ord + Copy>(values: &[T]) -> bool {
    values.iter().copied().collect::<BTreeSet<_>>().len() == values.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initial(name: &str, project: &str) -> InitialPath {
        InitialPath {
            session_id: SessionId::new(),
            session_name: name.into(),
            project: Project {
                identity: ProjectIdentity::CanonicalDirectory(project.into()),
            },
            trusted_project_config: None,
            workspace_id: WorkspaceId::new(),
            workspace_name: "main".into(),
            root: format!("{project}/main").into(),
            tab_id: TabId::new(),
            tab_name: "shell".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        }
    }

    #[test]
    fn materialized_tokens_are_revisioned_snapshotted_and_owned_by_resources() {
        let mut tree = ResourceTree::default();
        let path = initial("tokens", "/tokens");
        let (session_id, workspace_id, tab_id, pane_id, terminal_id) = (
            path.session_id,
            path.workspace_id,
            path.tab_id,
            path.pane_id,
            path.terminal_id,
        );
        tree.create_session(path).unwrap();
        let before = tree.revision();

        let first = tree
            .publish_presentation_token(
                PresentationTokenTarget::Workspace(workspace_id),
                "workspace.extension.status.state".into(),
                "ready".into(),
            )
            .unwrap();
        assert!(first.changed);
        assert_eq!(first.revision, before + 1);
        let unchanged = tree
            .publish_presentation_token(
                PresentationTokenTarget::Workspace(workspace_id),
                "workspace.extension.status.state".into(),
                "ready".into(),
            )
            .unwrap();
        assert!(!unchanged.changed);
        assert_eq!(unchanged.revision, first.revision);

        for (target, name) in [
            (
                PresentationTokenTarget::Session(session_id),
                "session.extension.status.state",
            ),
            (
                PresentationTokenTarget::Tab(tab_id),
                "tab.extension.status.state",
            ),
            (
                PresentationTokenTarget::Pane(pane_id),
                "pane.extension.status.state",
            ),
        ] {
            tree.publish_presentation_token(target, name.into(), "value".into())
                .unwrap();
        }
        let snapshot = tree.snapshot();
        let session = &snapshot.sessions[0];
        let workspace = &session.workspaces[0];
        let tab = &workspace.tabs[0];
        assert_eq!(session.tokens.len(), 1);
        assert_eq!(workspace.tokens.len(), 1);
        assert_eq!(tab.tokens.len(), 1);
        assert_eq!(tab.panes[0].tokens.len(), 1);

        tree.close_pane(pane_id).unwrap();
        assert_eq!(
            tree.publish_presentation_token(
                PresentationTokenTarget::Pane(pane_id),
                "pane.extension.status.state".into(),
                "other".into(),
            ),
            Err(ResourceError::Closing("pane"))
        );
        tree.terminal_exited(terminal_id).unwrap();
        assert!(tree.snapshot().sessions.is_empty());
    }

    #[test]
    fn extension_token_pruning_covers_every_scope_in_one_revision() {
        let mut tree = ResourceTree::default();
        let path = initial("token-prune", "/token-prune");
        let (session_id, workspace_id, tab_id, pane_id) = (
            path.session_id,
            path.workspace_id,
            path.tab_id,
            path.pane_id,
        );
        tree.create_session(path).unwrap();
        let scoped = [
            (
                PresentationTokenTarget::Session(session_id),
                "session.extension.keep.state",
                "session.extension.removed.state",
            ),
            (
                PresentationTokenTarget::Workspace(workspace_id),
                "workspace.extension.keep.state",
                "workspace.extension.removed.state",
            ),
            (
                PresentationTokenTarget::Tab(tab_id),
                "tab.extension.keep.state",
                "tab.extension.removed.state",
            ),
            (
                PresentationTokenTarget::Pane(pane_id),
                "pane.extension.keep.state",
                "pane.extension.removed.state",
            ),
        ];
        for (target, retained, removed) in scoped {
            tree.publish_presentation_token(target, retained.into(), "keep".into())
                .unwrap();
            tree.publish_presentation_token(target, removed.into(), "remove".into())
                .unwrap();
        }
        tree.publish_workspace_git_tokens(workspace_id, Some("main".into()), None, None)
            .unwrap();
        let before = tree.revision();
        let declared = scoped
            .into_iter()
            .map(|(_, retained, _)| retained.to_owned())
            .collect::<HashSet<_>>();

        let pruned = tree.prune_extension_presentation_tokens(&declared);

        assert!(pruned.changed);
        assert_eq!(pruned.revision, before + 1);
        let snapshot = tree.snapshot();
        let session = &snapshot.sessions[0];
        let workspace = &session.workspaces[0];
        let tab = &workspace.tabs[0];
        let pane = &tab.panes[0];
        for tokens in [
            &session.tokens,
            &workspace.tokens,
            &tab.tokens,
            &pane.tokens,
        ] {
            assert!(tokens.keys().all(|name| !name.contains(".removed.")));
            assert!(tokens.values().any(|value| value == "keep"));
        }
        assert_eq!(
            workspace.tokens.get(WORKSPACE_GIT_BRANCH_TOKEN),
            Some(&"main".to_owned())
        );

        let unchanged = tree.prune_extension_presentation_tokens(&declared);
        assert!(!unchanged.changed);
        assert_eq!(unchanged.revision, pruned.revision);
    }

    #[test]
    fn workspace_git_tokens_publish_and_clear_atomically_only_when_changed() {
        let mut tree = ResourceTree::default();
        let path = initial("git-tokens", "/git-tokens");
        let workspace_id = path.workspace_id;
        tree.create_session(path).unwrap();
        let before = tree.revision();

        let published = tree
            .publish_workspace_git_tokens(
                workspace_id,
                Some("main".into()),
                Some("+3".into()),
                Some("-2".into()),
            )
            .unwrap();
        assert_eq!(published.revision, before + 1);
        assert!(published.changed);
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tokens,
            BTreeMap::from([
                (WORKSPACE_GIT_BRANCH_TOKEN.into(), "main".into()),
                (WORKSPACE_GIT_ADDED_TOKEN.into(), "+3".into()),
                (WORKSPACE_GIT_DELETED_TOKEN.into(), "-2".into()),
            ])
        );

        let unchanged = tree
            .publish_workspace_git_tokens(
                workspace_id,
                Some("main".into()),
                Some("+3".into()),
                Some("-2".into()),
            )
            .unwrap();
        assert_eq!(unchanged.revision, published.revision);
        assert!(!unchanged.changed);

        let cleared = tree
            .publish_workspace_git_tokens(workspace_id, None, None, None)
            .unwrap();
        assert_eq!(cleared.revision, published.revision + 1);
        assert!(cleared.changed);
        assert!(tree.snapshot().sessions[0].workspaces[0].tokens.is_empty());

        tree.close_workspace(workspace_id).unwrap();
        assert_eq!(
            tree.publish_workspace_git_tokens(workspace_id, Some("stale".into()), None, None,),
            Err(ResourceError::Closing("workspace"))
        );
    }

    #[test]
    fn materialized_token_value_count_is_bounded_but_existing_values_can_update() {
        let mut tree = ResourceTree::default();
        let path = initial("token-limit", "/token-limit");
        let session_id = path.session_id;
        tree.create_session(path).unwrap();
        for index in 0..MAX_MATERIALIZED_TOKEN_VALUES {
            tree.publish_presentation_token(
                PresentationTokenTarget::Session(session_id),
                format!("session.extension.test.value-{index}"),
                "value".into(),
            )
            .unwrap();
        }
        assert_eq!(
            tree.publish_presentation_token(
                PresentationTokenTarget::Session(session_id),
                "session.extension.test.overflow".into(),
                "value".into(),
            ),
            Err(ResourceError::TooManyMaterializedTokens)
        );
        assert!(
            tree.publish_presentation_token(
                PresentationTokenTarget::Session(session_id),
                "session.extension.test.value-0".into(),
                "updated".into(),
            )
            .unwrap()
            .changed
        );
    }

    #[test]
    fn observed_pane_cwds_publish_once_per_change() {
        let mut tree = ResourceTree::default();
        let path = initial("cwds", "/cwds");
        let terminal_id = path.terminal_id;
        tree.create_session(path).unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0].panes[0].cwd,
            None
        );

        let revision = tree
            .update_pane_cwd(
                terminal_id,
                "/cwds/feature/src".into(),
                Some("/cwds/feature".into()),
            )
            .unwrap();
        assert!(revision > 0);
        let pane = tree.snapshot().sessions[0].workspaces[0].tabs[0].panes[0].clone();
        assert_eq!(pane.cwd, Some(PathBuf::from("/cwds/feature/src")));
        assert_eq!(pane.worktree, Some(PathBuf::from("/cwds/feature")));

        let unchanged = tree
            .update_pane_cwd(
                terminal_id,
                "/cwds/feature/src".into(),
                Some("/cwds/feature".into()),
            )
            .unwrap();
        assert_eq!(
            unchanged, revision,
            "same location does not bump the revision"
        );

        assert!(matches!(
            tree.update_pane_cwd(TerminalId::new(), "/elsewhere".into(), None),
            Err(ResourceError::NotFound("terminal"))
        ));
    }

    #[test]
    fn unnamed_workspaces_present_their_shared_location_or_multiple() {
        let mut tree = ResourceTree::default();
        let mut path = initial("places", "/places");
        path.workspace_name = String::new();
        let workspace_id = path.workspace_id;
        let first_pane = path.pane_id;
        let first_terminal = path.terminal_id;
        tree.create_session(path).unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].name,
            "main",
            "unobserved unnamed workspaces present their root's basename"
        );

        tree.update_pane_cwd(
            first_terminal,
            "/places/worktrees/feature/src".into(),
            Some("/places/worktrees/feature".into()),
        )
        .unwrap();
        assert_eq!(tree.snapshot().sessions[0].workspaces[0].name, "feature");

        let second_pane = PaneId::new();
        let second_terminal = TerminalId::new();
        tree.split_pane(
            first_pane,
            SplitDirection::Right,
            second_pane,
            second_terminal,
        )
        .unwrap();
        tree.update_pane_cwd(second_terminal, "/elsewhere".into(), None)
            .unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].name,
            MULTIPLE_LOCATIONS
        );

        tree.rename_workspace(workspace_id, "pinned".into())
            .unwrap();
        assert_eq!(tree.snapshot().sessions[0].workspaces[0].name, "pinned");
        tree.rename_workspace(workspace_id, String::new()).unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].name,
            MULTIPLE_LOCATIONS,
            "clearing the name resumes automatic presentation"
        );

        let sibling = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name: String::new(),
            root: "/places/other".into(),
            tab_id: TabId::new(),
            tab_name: String::new(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let session_id = tree.snapshot().sessions[0].id;
        tree.add_workspace(session_id, sibling)
            .expect("a second unnamed workspace is not a duplicate");
    }

    #[test]
    fn unnamed_tabs_follow_the_focused_process_but_explicit_titles_stay_stable() {
        let mut tree = ResourceTree::default();
        let path = initial("processes", "/processes");
        let tab_id = path.tab_id;
        let first_pane = path.pane_id;
        let first_terminal = path.terminal_id;
        tree.create_session(path).unwrap();

        tree.update_process_name(first_terminal, "zsh".into())
            .unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0].name,
            "shell"
        );

        tree.rename_tab(tab_id, String::new()).unwrap();
        tree.update_process_name(first_terminal, "zsh".into())
            .unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0].name,
            "zsh"
        );

        let second_pane = PaneId::new();
        let second_terminal = TerminalId::new();
        tree.split_pane(
            first_pane,
            SplitDirection::Right,
            second_pane,
            second_terminal,
        )
        .unwrap();
        tree.update_process_name(second_terminal, "vim".into())
            .unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0].name,
            "zsh"
        );
        tree.focus_pane(second_pane).unwrap();
        tree.update_process_name(second_terminal, "vim".into())
            .unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0].name,
            "vim"
        );

        tree.rename_tab(tab_id, "editor".into()).unwrap();
        tree.update_process_name(second_terminal, "less".into())
            .unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0].name,
            "editor"
        );

        tree.rename_tab(tab_id, String::new()).unwrap();
        tree.terminal_exited(second_terminal).unwrap();
        tree.update_process_name(first_terminal, "fish".into())
            .unwrap();
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0].name,
            "fish"
        );
    }

    #[test]
    fn lifecycle_reports_replace_and_override_screen_detection() {
        let mut tree = ResourceTree::default();
        let path = initial("agent", "/agent");
        let terminal_id = path.terminal_id;
        tree.create_session(path).unwrap();

        let detection = AgentDetection {
            agent: "codex".into(),
            rule: "working_indicator".into(),
        };
        tree.update_agent_detection(terminal_id, Some(detection), AgentState::Working, 10)
            .unwrap();
        let detected = tree.agent_activity(terminal_id).unwrap();
        assert_eq!(detected.state, AgentState::Working);
        assert!(detected.detection.is_some());
        assert!(detected.integration.is_none());
        assert!(detected.last_event.is_none());

        tree.update_agent_detection(
            terminal_id,
            Some(AgentDetection {
                agent: "codex".into(),
                rule: "idle_fallback".into(),
            }),
            AgentState::Idle,
            15,
        )
        .unwrap();
        let completed = tree.agent_activity(terminal_id).unwrap();
        assert_eq!(
            completed.last_event.as_ref().unwrap().kind,
            AgentReport::Completed
        );

        tree.report_agent(terminal_id, AgentReport::Idle, 20)
            .unwrap();
        let integrated_revision = tree.revision();
        let integrated = tree.agent_activity(terminal_id).unwrap();
        assert_eq!(integrated.state, AgentState::Idle);
        assert!(integrated.detection.is_none());
        assert!(integrated.integration.is_some());

        tree.update_agent_detection(
            terminal_id,
            Some(AgentDetection {
                agent: "codex".into(),
                rule: "live_blocker".into(),
            }),
            AgentState::Blocked,
            30,
        )
        .unwrap();
        assert_eq!(tree.revision(), integrated_revision);
        assert_eq!(
            tree.agent_activity(terminal_id).unwrap().state,
            AgentState::Idle
        );
    }

    #[test]
    fn agent_reports_are_revisioned_and_completion_remains_bounded() {
        let mut tree = ResourceTree::default();
        let path = initial("agent", "/agent");
        let terminal_id = path.terminal_id;
        tree.create_session(path).unwrap();

        let initial_snapshot = tree.snapshot();
        let unintegrated = &initial_snapshot.sessions[0].workspaces[0].tabs[0].panes[0].activity;
        assert_eq!(unintegrated.state, AgentState::Idle);
        assert_eq!(unintegrated.integration, None);
        assert_eq!(unintegrated.last_event, None);

        let working_revision = tree
            .report_agent_with_metadata(
                terminal_id,
                AgentReport::Working,
                AgentReportMetadata {
                    source: Some("pi".into()),
                    agent_session_id: Some("session-1".into()),
                    turn_id: Some("turn-1".into()),
                },
                10,
            )
            .unwrap();
        let working = tree.snapshot().sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .clone();
        assert_eq!(working.state, AgentState::Working);
        assert_eq!(working.revision, working_revision);
        assert_eq!(
            working.integration.as_ref().unwrap().source.as_deref(),
            Some("pi")
        );
        assert_eq!(
            working.last_event,
            Some(AgentEvent {
                revision: working_revision,
                kind: AgentReport::Working,
                occurred_at_ms: 10,
                turn_id: Some("turn-1".into()),
            })
        );

        let completed_revision = tree
            .report_agent(terminal_id, AgentReport::Completed, 20)
            .unwrap();
        let completed = tree.snapshot().sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .clone();
        assert_eq!(completed.state, AgentState::Idle);
        assert_eq!(
            completed.last_event,
            Some(AgentEvent {
                revision: completed_revision,
                kind: AgentReport::Completed,
                occurred_at_ms: 20,
                turn_id: None,
            })
        );
        let serialized_snapshot = serde_json::to_value(tree.snapshot()).unwrap();
        let serialized_activity =
            &serialized_snapshot["sessions"][0]["workspaces"][0]["tabs"][0]["panes"][0]["activity"];
        assert!(serialized_activity.get("integration").is_some());
        assert_eq!(serialized_activity["last_event"]["kind"], "completed");
        assert!(serialized_activity.get("attention").is_none());
        assert!(completed.has_unread_attention());
        let acknowledged_revision = tree
            .acknowledge_agent(terminal_id, completed_revision)
            .unwrap()
            .unwrap();
        let acknowledged = tree.agent_activity(terminal_id).unwrap();
        assert_eq!(acknowledged.read_revision, completed_revision);
        assert!(!acknowledged.has_unread_attention());
        assert_eq!(
            tree.acknowledge_agent(terminal_id, completed_revision)
                .unwrap(),
            None
        );
        assert_eq!(tree.revision(), acknowledged_revision);
        tree.report_agent(terminal_id, AgentReport::Blocked, 30)
            .unwrap();
        let blocked = tree.snapshot().sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .clone();
        assert_eq!(blocked.state, AgentState::Blocked);
        assert_eq!(
            blocked.last_event.as_ref().unwrap().kind,
            AgentReport::Blocked
        );
        assert!(blocked.has_unread_attention());
        assert_eq!(
            blocked.integration.as_ref().unwrap().source.as_deref(),
            Some("pi")
        );

        let before_rejected_report = tree.revision();
        assert_eq!(
            tree.report_agent_with_metadata(
                terminal_id,
                AgentReport::Working,
                AgentReportMetadata {
                    source: Some("x".repeat(crate::domain::MAX_AGENT_METADATA_VALUE_BYTES + 1)),
                    ..AgentReportMetadata::default()
                },
                40,
            ),
            Err(ResourceError::InvalidAgentReport(
                "agent report metadata value is too long"
            ))
        );
        assert_eq!(tree.revision(), before_rejected_report);
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0].panes[0]
                .activity
                .last_event
                .as_ref()
                .unwrap()
                .kind,
            AgentReport::Blocked
        );
    }

    #[test]
    fn exited_agent_integration_becomes_inactive_until_the_next_report() {
        let mut tree = ResourceTree::default();
        let path = initial("agent", "/agent");
        let terminal_id = path.terminal_id;
        tree.create_session(path).unwrap();

        tree.report_agent_with_metadata(
            terminal_id,
            AgentReport::Idle,
            AgentReportMetadata {
                source: Some("pi".into()),
                agent_session_id: Some("session-1".into()),
                turn_id: None,
            },
            10,
        )
        .unwrap();
        assert!(
            tree.agent_activity(terminal_id)
                .unwrap()
                .has_active_integration()
        );

        tree.report_agent(terminal_id, AgentReport::Exited, 20)
            .unwrap();
        let exited = tree.agent_activity(terminal_id).unwrap();
        assert!(!exited.has_active_integration());
        assert_eq!(exited.state, AgentState::Idle);
        assert_eq!(
            exited.last_event.as_ref().unwrap().kind,
            AgentReport::Exited
        );
        assert_eq!(
            exited.integration.as_ref().unwrap().source.as_deref(),
            Some("pi")
        );

        tree.update_agent_detection(
            terminal_id,
            Some(AgentDetection {
                agent: "codex".into(),
                rule: "working_indicator".into(),
            }),
            AgentState::Working,
            25,
        )
        .unwrap();
        tree.update_agent_detection(
            terminal_id,
            Some(AgentDetection {
                agent: "codex".into(),
                rule: "idle_fallback".into(),
            }),
            AgentState::Idle,
            26,
        )
        .unwrap();
        assert!(matches!(
            tree.agent_activity(terminal_id)
                .unwrap()
                .last_event
                .as_ref()
                .map(|event| event.kind),
            Some(AgentReport::Completed)
        ));
        assert!(
            !tree
                .agent_activity(terminal_id)
                .unwrap()
                .has_active_integration(),
            "screen inference must not reactivate exited integration metadata"
        );

        tree.report_agent(terminal_id, AgentReport::Idle, 30)
            .unwrap();
        assert!(
            tree.agent_activity(terminal_id)
                .unwrap()
                .has_active_integration()
        );
    }

    fn assert_valid<T>(tree: &ResourceTree, result: &Result<T, ResourceError>) {
        tree.validate().unwrap();
        if result.is_err() {
            tree.validate().unwrap();
        }
    }

    #[test]
    fn selector_is_owned_tagged_and_round_trips_escaped_unicode() {
        let selector = SessionSelector::Name("λ \"tab\"\n雪".into());
        let json = serde_json::to_string(&selector).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionSelector>(&json).unwrap(),
            selector
        );
        assert!(json.contains("\"type\":\"name\""));
    }

    #[test]
    fn terminal_target_requires_one_open_session_without_a_selector() {
        let mut tree = ResourceTree::default();
        assert_eq!(
            tree.resolve_terminal_target(None::<TargetSelector>),
            Err(ResourceError::AmbiguousTarget)
        );

        let first = initial("first", "/first");
        let expected = ResolvedTerminalPath {
            session_id: first.session_id,
            workspace_id: first.workspace_id,
            tab_id: first.tab_id,
            pane_id: first.pane_id,
            terminal_id: first.terminal_id,
        };
        tree.create_session(first).unwrap();
        assert_eq!(
            tree.resolve_terminal_target(None::<TargetSelector>),
            Ok(expected)
        );

        let second = initial("second", "/second");
        let second_id = second.session_id;
        tree.create_session(second).unwrap();
        assert_eq!(
            tree.resolve_terminal_target(None::<TargetSelector>),
            Err(ResourceError::AmbiguousTarget)
        );
        tree.close_session(second_id).unwrap();
        assert_eq!(
            tree.resolve_terminal_target(None::<TargetSelector>),
            Ok(expected)
        );
    }

    #[test]
    fn open_terminal_paths_for_tab_preserves_order_and_omits_closing_panes() {
        let mut tree = ResourceTree::default();
        let path = initial("ordered", "/ordered");
        let tab_id = path.tab_id;
        let first_pane = path.pane_id;
        let second_pane = PaneId::new();
        let second_terminal = TerminalId::new();
        tree.create_session(path).unwrap();
        tree.add_pane(tab_id, second_pane, second_terminal).unwrap();

        let paths = tree.open_terminal_paths_for_tab(tab_id).unwrap();
        assert_eq!(
            paths.iter().map(|path| path.pane_id).collect::<Vec<_>>(),
            vec![first_pane, second_pane]
        );

        tree.close_pane(second_pane).unwrap();
        assert_eq!(
            tree.open_terminal_paths_for_tab(tab_id).unwrap(),
            [tree
                .resolve_terminal_target(Some(TargetSelector::Pane(first_pane)))
                .unwrap()]
        );
    }

    #[test]
    fn authored_split_inserts_after_anchor_and_collapses_on_exit() {
        let mut tree = ResourceTree::default();
        let first = initial("split", "/split");
        let pane_a = first.pane_id;
        let pane_b = PaneId::new();
        let terminal_b = TerminalId::new();
        tree.create_session(first).unwrap();

        tree.split_pane(pane_a, SplitDirection::Down, pane_b, terminal_b)
            .unwrap();
        let tab = &tree.snapshot().sessions[0].workspaces[0].tabs[0];
        assert_eq!(
            tab.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [pane_a, pane_b]
        );
        assert!(matches!(
            tab.layout,
            SplitTree::Branch {
                axis: crate::splits::SplitAxis::Vertical,
                ..
            }
        ));

        tree.terminal_exited(terminal_b).unwrap();
        let tab = &tree.snapshot().sessions[0].workspaces[0].tabs[0];
        assert_eq!(tab.layout, SplitTree::leaf(pane_a));
        tree.validate().unwrap();
    }

    #[test]
    fn split_resize_is_revisioned_exact_persistent_and_no_ops_do_not_churn() {
        let mut tree = ResourceTree::default();
        let first = initial("resize-split", "/resize-split");
        let tab_id = first.tab_id;
        let pane_a = first.pane_id;
        tree.create_session(first).unwrap();
        tree.split_pane(
            pane_a,
            SplitDirection::Right,
            PaneId::new(),
            TerminalId::new(),
        )
        .unwrap();
        let split_id = match tree.open_layout_for_tab(tab_id).unwrap() {
            SplitTree::Branch { split_id, .. } => split_id,
            _ => panic!("split operation did not create a branch"),
        };
        let before = tree.revision();
        let ratio = SplitRatio::from_cells(37, 79).unwrap();
        let mutation = tree.resize_split(tab_id, split_id, ratio).unwrap();
        assert_eq!(mutation.revision, before + 1);
        assert_eq!(
            mutation.events,
            [ResourceEvent::SplitResized {
                tab_id,
                split_id,
                ratio,
            }]
        );
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0]
                .layout
                .ratio(split_id),
            Some(ratio)
        );

        let no_op = tree.resize_split(tab_id, split_id, ratio).unwrap();
        assert_eq!(no_op.revision, mutation.revision);
        assert!(no_op.events.is_empty());
        let equivalent =
            serde_json::from_str::<SplitRatio>(r#"{"numerator":74,"denominator":158}"#).unwrap();
        assert_eq!(equivalent, ratio);
        let equivalent_no_op = tree.resize_split(tab_id, split_id, equivalent).unwrap();
        assert_eq!(equivalent_no_op.revision, mutation.revision);
        assert!(equivalent_no_op.events.is_empty());
        assert_eq!(
            tree.resize_split(tab_id, SplitId::new(), ratio),
            Err(ResourceError::NotFound("split"))
        );
        assert_eq!(tree.revision(), mutation.revision);
        tree.validate().unwrap();
    }

    #[test]
    fn fallback_terminals_prefer_the_current_collection_and_never_cross_sessions() {
        let mut tree = ResourceTree::default();
        let first = initial("first", "/first");
        let session_id = first.session_id;
        let workspace_id = first.workspace_id;
        let tab_id = first.tab_id;
        let terminal_a = first.terminal_id;
        let pane_b = PaneId::new();
        let terminal_b = TerminalId::new();
        tree.create_session(first).unwrap();
        tree.add_pane(tab_id, pane_b, terminal_b).unwrap();

        let terminal_c = TerminalId::new();
        tree.add_tab(
            workspace_id,
            TabPath {
                tab_id: TabId::new(),
                tab_name: "second".into(),
                pane_id: PaneId::new(),
                terminal_id: terminal_c,
            },
        )
        .unwrap();
        let terminal_d = TerminalId::new();
        tree.add_workspace(
            session_id,
            WorkspacePath {
                workspace_id: WorkspaceId::new(),
                workspace_name: "peer".into(),
                root: "/first/peer".into(),
                tab_id: TabId::new(),
                tab_name: "shell".into(),
                pane_id: PaneId::new(),
                terminal_id: terminal_d,
            },
        )
        .unwrap();
        let other = initial("other", "/other");
        let other_terminal = other.terminal_id;
        tree.create_session(other).unwrap();

        assert_eq!(
            tree.open_terminal_ids_for_session(session_id).unwrap(),
            vec![terminal_a, terminal_b, terminal_c, terminal_d]
        );

        assert_eq!(
            tree.fallback_terminal_ids(terminal_c).unwrap(),
            vec![terminal_a, terminal_b, terminal_d]
        );
        assert_eq!(
            tree.fallback_terminal_ids(terminal_a).unwrap(),
            vec![terminal_b, terminal_c, terminal_d]
        );
        assert_eq!(
            tree.fallback_terminal_ids(terminal_b).unwrap(),
            vec![terminal_a, terminal_c, terminal_d]
        );
        assert!(
            !tree
                .fallback_terminal_ids(terminal_c)
                .unwrap()
                .contains(&other_terminal)
        );

        tree.close_pane(pane_b).unwrap();
        assert_eq!(
            tree.open_terminal_ids_for_session(session_id).unwrap(),
            vec![terminal_a, terminal_c, terminal_d]
        );
        assert_eq!(
            tree.fallback_terminal_ids(terminal_c).unwrap(),
            vec![terminal_a, terminal_d]
        );
    }

    #[test]
    fn terminal_target_uses_first_ordered_children_and_exact_explicit_selector() {
        let mut tree = ResourceTree::default();
        let first = initial("first", "/first");
        let first_id = first.session_id;
        let expected = ResolvedTerminalPath {
            session_id: first.session_id,
            workspace_id: first.workspace_id,
            tab_id: first.tab_id,
            pane_id: first.pane_id,
            terminal_id: first.terminal_id,
        };
        tree.create_session(first).unwrap();
        tree.create_session(initial("second", "/second")).unwrap();

        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Session(SessionSelector::Id(
                first_id
            )))),
            Ok(expected)
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Session(SessionSelector::Name(
                "first".into()
            )))),
            Ok(expected)
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Session(SessionSelector::Name(
                "First".into()
            )))),
            Err(ResourceError::NotFound("session"))
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Session(SessionSelector::Id(
                SessionId::new()
            )))),
            Err(ResourceError::NotFound("session"))
        );
    }

    #[test]
    fn terminal_target_rejects_closing_sessions_implicitly_and_explicitly() {
        let mut tree = ResourceTree::default();
        let path = initial("closing", "/closing");
        let session_id = path.session_id;
        tree.create_session(path).unwrap();
        tree.close_session(session_id).unwrap();

        assert_eq!(
            tree.resolve_terminal_target(None::<TargetSelector>),
            Err(ResourceError::AmbiguousTarget)
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Session(SessionSelector::Id(
                session_id
            )))),
            Err(ResourceError::Closing("session"))
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Session(SessionSelector::Name(
                "closing".into()
            )))),
            Err(ResourceError::Closing("session"))
        );
    }

    #[test]
    fn terminal_target_reports_broken_child_invariants() {
        let mut tree = ResourceTree::default();
        let path = initial("broken", "/broken");
        let session_id = path.session_id;
        tree.create_session(path).unwrap();
        tree.sessions
            .get_mut(&session_id)
            .unwrap()
            .workspaces
            .clear();

        assert!(matches!(
            tree.resolve_terminal_target(None::<TargetSelector>),
            Err(ResourceError::Invariant(_))
        ));
    }

    #[test]
    fn roots_are_snapshotted_and_may_repeat_within_one_session() {
        let mut tree = ResourceTree::default();
        let p = initial("s", "/project");
        let sid = p.session_id;
        let root = p.root.clone();
        let project = p.project.clone();
        tree.create_session(p).unwrap();
        let peer = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name: "peer".into(),
            root: "/project/peer".into(),
            tab_id: TabId::new(),
            tab_name: "shell".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let peer_id = peer.workspace_id;
        tree.add_workspace(sid, peer).unwrap();
        assert_eq!(
            tree.snapshot().sessions[0]
                .workspaces
                .iter()
                .map(|w| w.root.clone())
                .collect::<Vec<_>>(),
            vec![root, PathBuf::from("/project/peer")]
        );
        let duplicate = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name: "bad".into(),
            root: "/project/peer".into(),
            tab_id: TabId::new(),
            tab_name: "x".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let duplicate_id = duplicate.workspace_id;
        tree.add_workspace(sid, duplicate).unwrap();
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.sessions[0].workspaces.len(), 3);
        assert_eq!(
            snapshot.sessions[0].workspaces[1].root,
            snapshot.sessions[0].workspaces[2].root
        );
        assert_eq!(
            tree.checkout_destination(&project, Path::new("/project/peer")),
            Ok(CheckoutDestination::Existing(peer_id))
        );
        tree.close_workspace(peer_id).unwrap();
        assert_eq!(
            tree.checkout_destination(&project, Path::new("/project/peer")),
            Ok(CheckoutDestination::Existing(duplicate_id))
        );
        tree.validate().unwrap();
    }

    #[test]
    fn requested_close_is_two_phase_and_duplicate_exit_is_not_found_without_revision() {
        let mut tree = ResourceTree::default();
        let p = initial("s", "/p");
        let pane = p.pane_id;
        let terminal = p.terminal_id;
        tree.create_session(p).unwrap();
        let request = tree.close_pane(pane).unwrap();
        assert_eq!(request.terminals_to_close, vec![terminal]);
        assert!(!request.multiplexer_empty);
        assert!(tree.snapshot().sessions[0].workspaces[0].tabs[0].panes[0].closing);
        let exit = tree.terminal_exited(terminal).unwrap();
        assert!(exit.multiplexer_empty);
        assert!(matches!(
            exit.events.as_slice(),
            [
                ResourceEvent::PaneClosed {
                    cause: CloseCause::Requested,
                    ..
                },
                ResourceEvent::TabClosed { .. },
                ResourceEvent::WorkspaceClosed { .. },
                ResourceEvent::SessionClosed { .. }
            ]
        ));
        let revision = tree.revision();
        assert_eq!(
            tree.terminal_exited(terminal),
            Err(ResourceError::NotFound("terminal"))
        );
        assert_eq!(tree.revision(), revision);
        tree.validate().unwrap();
    }

    #[test]
    fn natural_exit_is_direct_and_multi_terminal_session_waits_for_last_exit() {
        let mut tree = ResourceTree::default();
        let p = initial("s", "/p");
        let tab = p.tab_id;
        let first = p.terminal_id;
        tree.create_session(p).unwrap();
        let second = TerminalId::new();
        tree.add_pane(tab, PaneId::new(), second).unwrap();
        let sid = tree.session_order[0];
        let request = tree.close_session(sid).unwrap();
        assert_eq!(request.terminals_to_close, vec![first, second]);
        assert_eq!(
            request.events,
            vec![ResourceEvent::SessionCloseRequested { session_id: sid }]
        );
        assert!(!request.multiplexer_empty);
        let one = tree.terminal_exited(first).unwrap();
        assert!(!one.multiplexer_empty);
        assert_eq!(one.events.len(), 1);
        tree.validate().unwrap();
        assert!(tree.terminal_exited(second).unwrap().multiplexer_empty);
        tree.validate().unwrap();
        let p = initial("natural", "/natural");
        let terminal = p.terminal_id;
        tree.create_session(p).unwrap();
        assert!(matches!(
            tree.terminal_exited(terminal).unwrap().events[0],
            ResourceEvent::PaneClosed {
                cause: CloseCause::TerminalExited,
                ..
            }
        ));
    }

    #[test]
    fn close_can_be_rolled_back_and_closing_session_rejects_adds() {
        let mut tree = ResourceTree::default();
        let p = initial("s", "/p");
        let sid = p.session_id;
        let tab = p.tab_id;
        tree.create_session(p).unwrap();
        tree.close_session(sid).unwrap();
        let result = tree.add_pane(tab, PaneId::new(), TerminalId::new());
        assert_eq!(result, Err(ResourceError::Closing("session")));
        assert_valid(&tree, &result);
        tree.cancel_close_session(sid).unwrap();
        assert!(!tree.snapshot().sessions[0].closing);
        tree.add_pane(tab, PaneId::new(), TerminalId::new())
            .unwrap();
        tree.validate().unwrap();
    }

    #[test]
    fn session_close_rejects_pending_pane_close_atomically() {
        let mut tree = ResourceTree::default();
        let p = initial("s", "/p");
        let sid = p.session_id;
        let pane = p.pane_id;
        tree.create_session(p).unwrap();
        tree.close_pane(pane).unwrap();
        let before = tree.snapshot();

        assert_eq!(tree.close_session(sid), Err(ResourceError::Closing("pane")));
        assert_eq!(tree.snapshot(), before);
        assert_eq!(tree.revision(), before.revision);
        tree.validate().unwrap();
    }

    #[test]
    fn move_pane_preserves_order_ids_and_reverse_resolution() {
        let mut tree = ResourceTree::default();
        let path = initial("a", "/a");
        let (workspace, source, pane, terminal) = (
            path.workspace_id,
            path.tab_id,
            path.pane_id,
            path.terminal_id,
        );
        tree.create_session(path).unwrap();
        let source_sibling = PaneId::new();
        tree.add_pane(source, source_sibling, TerminalId::new())
            .unwrap();
        let destination = TabId::new();
        let destination_sibling = PaneId::new();
        tree.add_tab(
            workspace,
            TabPath {
                tab_id: destination,
                tab_name: "destination".into(),
                pane_id: destination_sibling,
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();
        let destination_tail = PaneId::new();
        tree.add_pane(destination, destination_tail, TerminalId::new())
            .unwrap();

        let moved = tree.move_pane(pane, destination).unwrap();
        assert_eq!(
            moved.events,
            vec![ResourceEvent::PaneMoved {
                pane_id: pane,
                terminal_id: terminal,
                from: source,
                to: destination,
            }]
        );
        assert!(moved.terminals_to_close.is_empty());
        assert_eq!(tree.tab_panes(source), vec![source_sibling]);
        assert_eq!(
            tree.tab_panes(destination),
            vec![destination_sibling, destination_tail, pane]
        );
        assert_eq!(tree.panes[&pane].terminal_id, terminal);
        assert_eq!(tree.terminals[&terminal], pane);
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Terminal(terminal)))
                .unwrap()
                .pane_id,
            pane
        );

        let before = tree.snapshot();
        let retry = tree.move_pane(pane, destination).unwrap();
        assert_eq!(retry.revision, before.revision);
        assert!(retry.events.is_empty());
        assert!(retry.terminals_to_close.is_empty());
        assert_eq!(tree.snapshot(), before);
        tree.validate().unwrap();
    }

    #[test]
    fn moving_last_pane_emits_pane_moved_then_tab_closed_without_closing_terminal() {
        let mut tree = ResourceTree::default();
        let path = initial("a", "/a");
        let (workspace, source, pane, terminal) = (
            path.workspace_id,
            path.tab_id,
            path.pane_id,
            path.terminal_id,
        );
        tree.create_session(path).unwrap();
        let destination = TabId::new();
        tree.add_tab(
            workspace,
            TabPath {
                tab_id: destination,
                tab_name: "destination".into(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();

        let moved = tree.move_pane(pane, destination).unwrap();
        assert_eq!(
            moved.events,
            vec![
                ResourceEvent::PaneMoved {
                    pane_id: pane,
                    terminal_id: terminal,
                    from: source,
                    to: destination,
                },
                ResourceEvent::TabClosed { tab_id: source },
            ]
        );
        assert!(moved.terminals_to_close.is_empty());
        assert!(!tree.tabs.contains_key(&source));
        assert_eq!(tree.terminals[&terminal], pane);
        tree.validate().unwrap();
    }

    #[test]
    fn move_pane_rejects_other_workspaces_and_sessions_atomically() {
        let mut tree = ResourceTree::default();
        let path = initial("a", "/a");
        let (session, pane) = (path.session_id, path.pane_id);
        tree.create_session(path).unwrap();
        let peer = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name: "peer".into(),
            root: "/a/peer".into(),
            tab_id: TabId::new(),
            tab_name: "peer".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let peer_tab = peer.tab_id;
        tree.add_workspace(session, peer).unwrap();
        let other = initial("b", "/b");
        let other_tab = other.tab_id;
        tree.create_session(other).unwrap();

        for destination in [peer_tab, other_tab] {
            let before = tree.snapshot();
            let result = tree.move_pane(pane, destination);
            assert_eq!(result, Err(ResourceError::DifferentWorkspace));
            assert_eq!(tree.snapshot(), before);
            assert_valid(&tree, &result);
        }
    }

    #[test]
    fn duplicate_paths_are_atomic_and_creation_events_are_complete_and_ordered() {
        let mut tree = ResourceTree::default();
        let p = initial("s", "/p");
        let ids = (
            p.session_id,
            p.workspace_id,
            p.tab_id,
            p.pane_id,
            p.terminal_id,
        );
        let created = tree.create_session(p).unwrap();
        assert_eq!(
            created.events,
            vec![
                ResourceEvent::SessionCreated {
                    id: ids.0,
                    name: "s".into(),
                    project: Project {
                        identity: ProjectIdentity::CanonicalDirectory("/p".into())
                    },
                    trusted_project_config: None,
                },
                ResourceEvent::WorkspaceCreated {
                    session_id: ids.0,
                    id: ids.1,
                    name: "main".into(),
                    root: "/p/main".into(),
                    trusted_project_config: None,
                },
                ResourceEvent::TabCreated {
                    workspace_id: ids.1,
                    id: ids.2,
                    name: "shell".into()
                },
                ResourceEvent::PaneCreated {
                    tab_id: ids.2,
                    id: ids.3,
                    terminal_id: ids.4,
                    closing: false
                },
            ]
        );
        let mut cases = Vec::new();
        let mut d = initial("x", "/x");
        d.session_id = ids.0;
        cases.push(d);
        let mut d = initial("x", "/x2");
        d.workspace_id = ids.1;
        cases.push(d);
        let mut d = initial("x", "/x3");
        d.tab_id = ids.2;
        cases.push(d);
        let mut d = initial("x", "/x4");
        d.pane_id = ids.3;
        cases.push(d);
        let mut d = initial("x", "/x5");
        d.terminal_id = ids.4;
        cases.push(d);
        for d in cases {
            let before = tree.snapshot();
            let result = tree.create_session(d);
            assert!(result.is_err());
            assert_eq!(tree.snapshot(), before);
            assert_valid(&tree, &result);
        }
        let sid = ids.0;
        let bad_workspace = WorkspacePath {
            workspace_id: ids.1,
            workspace_name: "w".into(),
            root: "/unique".into(),
            tab_id: TabId::new(),
            tab_name: "t".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let result = tree.add_workspace(sid, bad_workspace);
        assert!(result.is_err());
        assert_valid(&tree, &result);
        let bad_tab = TabPath {
            tab_id: ids.2,
            tab_name: "t".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let result = tree.add_tab(ids.1, bad_tab);
        assert!(result.is_err());
        assert_valid(&tree, &result);
        let result = tree.add_pane(ids.2, PaneId::new(), ids.4);
        assert!(result.is_err());
        assert_valid(&tree, &result);
    }

    #[test]
    fn deterministic_generated_sequence_stays_valid_and_ordered() {
        let mut tree = ResourceTree::default();
        let mut terminals = Vec::new();
        for i in 0..24 {
            let p = initial(&format!("s{i}"), &format!("/{i}"));
            terminals.push(p.terminal_id);
            tree.create_session(p).unwrap();
            tree.validate().unwrap();
        }
        assert_eq!(
            tree.snapshot()
                .sessions
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            (0..24).map(|i| format!("s{i}")).collect::<Vec<_>>()
        );
        for terminal in terminals {
            tree.terminal_exited(terminal).unwrap();
            tree.validate().unwrap();
        }
        assert!(tree.snapshot().sessions.is_empty());
    }

    #[test]
    fn typed_targets_checkout_planning_and_workspace_close() {
        let mut tree = ResourceTree::default();
        let first = initial("s", "/p");
        let project = first.project.clone();
        let root = first.root.clone();
        let sid = first.session_id;
        let wid = first.workspace_id;
        let tab = first.tab_id;
        let pane = first.pane_id;
        let terminal = first.terminal_id;
        let expected = ResolvedTerminalPath {
            session_id: sid,
            workspace_id: wid,
            tab_id: tab,
            pane_id: pane,
            terminal_id: terminal,
        };
        tree.create_session(first).unwrap();
        for selector in [
            TargetSelector::Session(SessionSelector::Id(sid)),
            TargetSelector::Workspace(wid),
            TargetSelector::Tab(tab),
            TargetSelector::Pane(pane),
            TargetSelector::Terminal(terminal),
        ] {
            assert_eq!(
                tree.resolve_terminal_target(Some(selector.clone())),
                Ok(expected)
            );
            assert_eq!(
                serde_json::from_str::<TargetSelector>(&serde_json::to_string(&selector).unwrap())
                    .unwrap(),
                selector
            );
        }
        let revision = tree.revision();
        assert_eq!(
            tree.checkout_destination(&project, &root).unwrap(),
            CheckoutDestination::Existing(expected.workspace_id)
        );
        assert_eq!(
            tree.checkout_destination(&project, Path::new("/p/peer"))
                .unwrap(),
            CheckoutDestination::AddWorkspace { session_id: sid }
        );
        assert_eq!(
            tree.checkout_destination(
                &Project {
                    identity: ProjectIdentity::CanonicalDirectory("/other".into())
                },
                Path::new("/other/main")
            )
            .unwrap(),
            CheckoutDestination::CreateSession
        );
        assert_eq!(tree.revision(), revision);

        let peer = WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name: "peer".into(),
            root: "/p/peer".into(),
            tab_id: TabId::new(),
            tab_name: "shell".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let peer_terminal = peer.terminal_id;
        tree.add_workspace(sid, peer).unwrap();
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Session(SessionSelector::Id(sid)))),
            Err(ResourceError::AmbiguousTarget)
        );
        assert_eq!(
            tree.resolve_terminal_target(None::<TargetSelector>),
            Err(ResourceError::AmbiguousTarget)
        );
        let request = tree.close_workspace(wid).unwrap();
        assert_eq!(request.terminals_to_close, vec![terminal]);
        assert!(tree.snapshot().sessions[0].workspaces[0].closing);
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Workspace(wid))),
            Err(ResourceError::Closing("workspace"))
        );
        assert_eq!(
            tree.resolve_terminal_target(None),
            tree.resolve_terminal_target(Some(TargetSelector::Terminal(peer_terminal)))
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Session(SessionSelector::Id(sid)))),
            tree.resolve_terminal_target(Some(TargetSelector::Terminal(peer_terminal)))
        );
        tree.cancel_close_workspace(wid).unwrap();
        assert!(!tree.snapshot().sessions[0].workspaces[0].closing);
        tree.close_workspace(wid).unwrap();
        let exit = tree.terminal_exited(terminal).unwrap();
        assert!(matches!(
            exit.events.as_slice(),
            [
                ResourceEvent::PaneClosed {
                    cause: CloseCause::Requested,
                    ..
                },
                ResourceEvent::TabClosed { .. },
                ResourceEvent::WorkspaceClosed { .. }
            ]
        ));
        assert!(!exit.multiplexer_empty);
        tree.validate().unwrap();
        assert!(
            tree.terminal_exited(peer_terminal)
                .unwrap()
                .multiplexer_empty
        );
        tree.validate().unwrap();
    }

    #[test]
    fn existing_checkout_is_workspace_idempotent_with_multiple_terminals() {
        let mut tree = ResourceTree::default();
        let path = initial("project", "/project");
        let project = path.project.clone();
        let workspace_id = path.workspace_id;
        let first_terminal = path.terminal_id;
        tree.create_session(path).unwrap();
        tree.add_tab(
            workspace_id,
            TabPath {
                tab_id: TabId::new(),
                tab_name: "second".into(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();
        let revision = tree.revision();

        assert_eq!(
            tree.checkout_destination(&project, Path::new("/project/main")),
            Ok(CheckoutDestination::Existing(workspace_id))
        );
        assert_eq!(
            tree.initial_terminal_for_workspace(workspace_id)
                .unwrap()
                .terminal_id,
            first_terminal
        );
        assert_eq!(tree.revision(), revision);
        assert_eq!(
            tree.resolve_terminal_target(Some(TargetSelector::Workspace(workspace_id))),
            Err(ResourceError::AmbiguousTarget)
        );
    }

    #[test]
    fn implicit_display_names_are_deterministically_disambiguated() {
        let mut tree = ResourceTree::default();
        let first = initial("project", "/first");
        tree.create_session(first).unwrap();
        let second = initial("project-2", "/second");
        tree.create_session(second).unwrap();
        assert_eq!(tree.available_session_name("project"), "project-3");
    }

    #[test]
    fn workspace_root_and_available_tab_names_are_workspace_scoped() {
        let mut tree = ResourceTree::default();
        let first = initial("first", "/first");
        let session_id = first.session_id;
        let workspace_id = first.workspace_id;
        let root = first.root.clone();
        tree.create_session(first).unwrap();
        tree.add_tab(
            workspace_id,
            TabPath {
                tab_id: TabId::new(),
                tab_name: "shell-2".into(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();
        tree.create_session(initial("second", "/second")).unwrap();

        assert_eq!(tree.workspace_root(workspace_id), Ok(root.as_path()));
        assert_eq!(tree.session_id_for_workspace(workspace_id), Ok(session_id));
        assert_eq!(
            tree.available_tab_name(workspace_id, "shell"),
            Ok("shell-3".into())
        );
        let missing = WorkspaceId::new();
        assert_eq!(
            tree.workspace_root(missing),
            Err(ResourceError::NotFound("workspace"))
        );
        assert_eq!(
            tree.session_id_for_workspace(missing),
            Err(ResourceError::NotFound("workspace"))
        );
        assert_eq!(
            tree.available_tab_name(missing, "tab"),
            Err(ResourceError::NotFound("workspace"))
        );
    }

    #[test]
    fn add_tab_is_atomic_and_preserves_order_identity_and_closing_rules() {
        let mut tree = ResourceTree::default();
        let first = initial("session", "/project");
        let session_id = first.session_id;
        let workspace_id = first.workspace_id;
        let first_tab = first.tab_id;
        let added = TabPath {
            tab_id: TabId::new(),
            tab_name: "editor".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let added_ids = (added.tab_id, added.pane_id, added.terminal_id);
        tree.create_session(first).unwrap();

        let mutation = tree.add_tab(workspace_id, added).unwrap();
        assert_eq!(
            mutation.events,
            vec![
                ResourceEvent::TabCreated {
                    workspace_id,
                    id: added_ids.0,
                    name: "editor".into(),
                },
                ResourceEvent::PaneCreated {
                    tab_id: added_ids.0,
                    id: added_ids.1,
                    terminal_id: added_ids.2,
                    closing: false,
                },
            ]
        );
        let tabs = &tree.snapshot().sessions[0].workspaces[0].tabs;
        assert_eq!(
            tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![first_tab, added_ids.0]
        );
        assert_eq!(tabs[1].panes[0].id, added_ids.1);
        assert_eq!(tabs[1].panes[0].terminal_id, added_ids.2);

        for (workspace, path, expected) in [
            (
                WorkspaceId::new(),
                TabPath {
                    tab_id: TabId::new(),
                    tab_name: "missing".into(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                },
                ResourceError::NotFound("workspace"),
            ),
            (
                workspace_id,
                TabPath {
                    tab_id: TabId::new(),
                    tab_name: "editor".into(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                },
                ResourceError::Duplicate("tab name"),
            ),
            (
                workspace_id,
                TabPath {
                    tab_id: added_ids.0,
                    tab_name: "other".into(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                },
                ResourceError::Duplicate("tab id"),
            ),
        ] {
            let before = tree.snapshot();
            assert_eq!(tree.add_tab(workspace, path), Err(expected));
            assert_eq!(tree.snapshot(), before);
        }

        tree.close_session(session_id).unwrap();
        let before = tree.snapshot();
        assert_eq!(
            tree.add_tab(
                workspace_id,
                TabPath {
                    tab_id: TabId::new(),
                    tab_name: "closed".into(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new()
                }
            ),
            Err(ResourceError::Closing("session"))
        );
        assert_eq!(tree.snapshot(), before);
        tree.validate().unwrap();
    }

    #[test]
    fn renames_preserve_identity_order_and_emit_exact_events() {
        let mut tree = ResourceTree::default();
        let path = initial("Session", "/project");
        let (session_id, workspace_id, tab_id) = (path.session_id, path.workspace_id, path.tab_id);
        tree.create_session(path).unwrap();
        let revision = tree.revision();

        assert_eq!(
            tree.rename_session(session_id, "session".into()).unwrap(),
            Mutation {
                revision: revision + 1,
                events: vec![ResourceEvent::SessionRenamed {
                    id: session_id,
                    old_name: "Session".into(),
                    new_name: "session".into(),
                }],
                terminals_to_close: vec![],
                multiplexer_empty: false,
            }
        );
        assert_eq!(
            tree.resolve_session(SessionSelector::Name("Session".into())),
            Err(ResourceError::NotFound("session"))
        );
        assert_eq!(
            tree.resolve_session(SessionSelector::Name("session".into())),
            Ok(session_id)
        );
        let workspace_rename = tree
            .rename_workspace(workspace_id, "  Main  ".into())
            .unwrap();
        assert_eq!(workspace_rename.revision, revision + 2);
        assert_eq!(
            workspace_rename.events,
            vec![ResourceEvent::WorkspaceRenamed {
                session_id,
                id: workspace_id,
                root: "/project/main".into(),
                old_name: "main".into(),
                new_name: "  Main  ".into(),
                trusted_project_config: None,
            }]
        );
        let tab_rename = tree.rename_tab(tab_id, "Shell".into()).unwrap();
        assert_eq!(tab_rename.revision, revision + 3);
        assert_eq!(
            tab_rename.events,
            vec![ResourceEvent::TabRenamed {
                workspace_id,
                id: tab_id,
                old_name: "shell".into(),
                new_name: "Shell".into(),
            }]
        );

        let snapshot = tree.snapshot();
        assert_eq!(snapshot.sessions[0].id, session_id);
        assert_eq!(snapshot.sessions[0].workspaces[0].id, workspace_id);
        assert_eq!(snapshot.sessions[0].workspaces[0].tabs[0].id, tab_id);
        tree.validate().unwrap();
    }

    #[test]
    fn exact_rename_is_a_true_no_op_even_when_a_pane_is_closing() {
        let mut tree = ResourceTree::default();
        let path = initial("session", "/project");
        let (session_id, tab_id, pane_id) = (path.session_id, path.tab_id, path.pane_id);
        tree.create_session(path).unwrap();
        tree.close_pane(pane_id).unwrap();
        let before = tree.snapshot();
        let revision = tree.revision();

        let mutation = tree.rename_tab(tab_id, "shell".into()).unwrap();
        assert_eq!(mutation.revision, revision);
        assert!(mutation.events.is_empty());
        assert!(mutation.terminals_to_close.is_empty());
        assert_eq!(tree.snapshot(), before);
        assert_eq!(
            tree.rename_session(session_id, "session".into())
                .unwrap()
                .revision,
            revision
        );
        tree.validate().unwrap();
    }

    #[test]
    fn rename_errors_are_scoped_atomic_and_reject_closing_ancestors() {
        let mut tree = ResourceTree::default();
        let first = initial("first", "/first");
        let (session_id, workspace_id, tab_id) =
            (first.session_id, first.workspace_id, first.tab_id);
        tree.create_session(first).unwrap();
        let second = initial("second", "/second");
        let second_tab = second.tab_id;
        tree.create_session(second).unwrap();
        tree.add_tab(
            workspace_id,
            TabPath {
                tab_id: TabId::new(),
                tab_name: "peer".into(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();

        for result in [
            tree.rename_session(session_id, "second".into()),
            tree.rename_tab(tab_id, "peer".into()),
            tree.rename_workspace(WorkspaceId::new(), "name".into()),
            tree.rename_tab(TabId::new(), "name".into()),
            tree.rename_session(session_id, " \n ".into()),
        ] {
            assert!(result.is_err());
        }
        assert_eq!(tree.snapshot().sessions[0].name, "first");
        assert_eq!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0].name,
            "shell"
        );

        // Identical names in another parent are not duplicates.
        tree.rename_tab(second_tab, "peer".into()).unwrap();
        tree.close_workspace(workspace_id).unwrap();
        let before = tree.snapshot();
        assert_eq!(
            tree.rename_workspace(workspace_id, "renamed".into()),
            Err(ResourceError::Closing("workspace"))
        );
        assert_eq!(
            tree.rename_tab(tab_id, "renamed".into()),
            Err(ResourceError::Closing("workspace"))
        );
        assert_eq!(tree.snapshot(), before);
        tree.close_session(tree.snapshot().sessions[1].id).unwrap();
        assert_eq!(
            tree.rename_tab(second_tab, "again".into()),
            Err(ResourceError::Closing("session"))
        );
        tree.validate().unwrap();
    }

    #[test]
    fn whole_tab_close_is_ordered_atomic_and_cascades_only_after_the_last_exit() {
        let mut tree = ResourceTree::default();
        let path = initial("session", "/project");
        let (workspace_id, tab_id, first_pane, first_terminal) = (
            path.workspace_id,
            path.tab_id,
            path.pane_id,
            path.terminal_id,
        );
        tree.create_session(path).unwrap();
        let second_pane = PaneId::new();
        let second_terminal = TerminalId::new();
        tree.add_pane(tab_id, second_pane, second_terminal).unwrap();
        tree.add_tab(
            workspace_id,
            TabPath {
                tab_id: TabId::new(),
                tab_name: "peer".into(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();

        let close = tree.close_tab(tab_id).unwrap();
        assert_eq!(
            close.terminals_to_close,
            vec![first_terminal, second_terminal]
        );
        assert_eq!(
            close.events,
            vec![ResourceEvent::TabCloseRequested { tab_id }]
        );
        assert!(
            tree.snapshot().sessions[0].workspaces[0].tabs[0]
                .panes
                .iter()
                .all(|pane| pane.closing)
        );
        let before = tree.snapshot();
        assert_eq!(tree.close_tab(tab_id), Err(ResourceError::Closing("tab")));
        assert_eq!(tree.snapshot(), before);

        let first_exit = tree.terminal_exited(first_terminal).unwrap();
        assert_eq!(
            first_exit.events,
            vec![ResourceEvent::PaneClosed {
                pane_id: first_pane,
                terminal_id: first_terminal,
                cause: CloseCause::Requested,
            }]
        );
        let last_exit = tree.terminal_exited(second_terminal).unwrap();
        assert_eq!(
            last_exit.events,
            vec![
                ResourceEvent::PaneClosed {
                    pane_id: second_pane,
                    terminal_id: second_terminal,
                    cause: CloseCause::Requested,
                },
                ResourceEvent::TabClosed { tab_id },
            ]
        );
        tree.validate().unwrap();
    }

    #[test]
    fn whole_tab_close_can_cancel_remaining_panes_and_respects_ancestors() {
        let mut tree = ResourceTree::default();
        let path = initial("session", "/project");
        let (session_id, workspace_id, tab_id, first_terminal) = (
            path.session_id,
            path.workspace_id,
            path.tab_id,
            path.terminal_id,
        );
        tree.create_session(path).unwrap();
        let pane_id = PaneId::new();
        let terminal_id = TerminalId::new();
        tree.add_pane(tab_id, pane_id, terminal_id).unwrap();
        assert_eq!(
            tree.cancel_close_tab(tab_id),
            Err(ResourceError::NotFound("pending tab close"))
        );
        tree.close_tab(tab_id).unwrap();
        tree.terminal_exited(first_terminal).unwrap();
        let cancel = tree.cancel_close_tab(tab_id).unwrap();
        assert_eq!(
            cancel.events,
            vec![ResourceEvent::TabCloseCancelled { tab_id }]
        );
        let tab = &tree.snapshot().sessions[0].workspaces[0].tabs[0];
        assert!(!tab.closing);
        assert!(!tab.panes[0].closing);

        tree.close_workspace(workspace_id).unwrap();
        assert_eq!(
            tree.cancel_close_tab(tab_id),
            Err(ResourceError::Closing("workspace"))
        );
        tree.cancel_close_workspace(workspace_id).unwrap();
        tree.close_session(session_id).unwrap();
        assert_eq!(
            tree.cancel_close_tab(tab_id),
            Err(ResourceError::Closing("session"))
        );
        tree.validate().unwrap();
    }

    #[test]
    fn explicit_tab_closing_controls_tab_operations_and_rolls_back_atomically() {
        let mut tree = ResourceTree::default();
        let path = initial("session", "/project");
        let (workspace_id, source, first_pane) = (path.workspace_id, path.tab_id, path.pane_id);
        tree.create_session(path).unwrap();
        let second_pane = PaneId::new();
        tree.add_pane(source, second_pane, TerminalId::new())
            .unwrap();
        let destination = TabId::new();
        tree.add_tab(
            workspace_id,
            TabPath {
                tab_id: destination,
                tab_name: "destination".into(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();

        tree.close_tab(source).unwrap();
        let closed = tree.snapshot();
        assert!(closed.sessions[0].workspaces[0].tabs[0].closing);
        assert_eq!(
            tree.workspace_id_for_tab(source),
            Err(ResourceError::Closing("tab"))
        );
        assert_eq!(
            tree.add_pane(source, PaneId::new(), TerminalId::new()),
            Err(ResourceError::Closing("tab"))
        );
        assert_eq!(
            tree.close_pane(first_pane),
            Err(ResourceError::Closing("tab"))
        );
        assert_eq!(
            tree.cancel_close_pane(first_pane),
            Err(ResourceError::Closing("tab"))
        );
        assert_eq!(
            tree.move_pane(first_pane, destination),
            Err(ResourceError::Closing("tab"))
        );
        assert_eq!(
            tree.move_pane(first_pane, source),
            Err(ResourceError::Closing("tab"))
        );
        assert_eq!(
            tree.rename_tab(source, "changed".into()),
            Err(ResourceError::Closing("tab"))
        );
        let revision = tree.revision();
        assert_eq!(
            tree.rename_tab(source, "shell".into()).unwrap().revision,
            revision
        );
        assert_eq!(tree.snapshot(), closed);

        tree.cancel_close_tab(source).unwrap();
        let tab = &tree.snapshot().sessions[0].workspaces[0].tabs[0];
        assert!(!tab.closing && tab.panes.iter().all(|pane| !pane.closing));
        tree.close_pane(first_pane).unwrap();
        assert!(!tree.snapshot().sessions[0].workspaces[0].tabs[0].closing);
        assert_eq!(
            tree.cancel_close_tab(source),
            Err(ResourceError::NotFound("pending tab close"))
        );
        tree.cancel_close_pane(first_pane).unwrap();

        tree.close_tab(destination).unwrap();
        assert_eq!(
            tree.move_pane(first_pane, destination),
            Err(ResourceError::Closing("tab"))
        );
        tree.cancel_close_tab(destination).unwrap();
        tree.validate().unwrap();
    }

    #[test]
    fn ancestor_close_is_inherited_without_copying_close_intents() {
        let mut tree = ResourceTree::default();
        let path = initial("session", "/project");
        let (workspace_id, tab_id) = (path.workspace_id, path.tab_id);
        tree.create_session(path).unwrap();
        tree.close_workspace(workspace_id).unwrap();
        assert!(tree.snapshot().sessions[0].workspaces[0].tabs[0].closing);
        assert!(
            tree.close_intents
                .contains(&CloseIntent::Workspace(workspace_id))
        );
        assert!(!tree.close_intents.contains(&CloseIntent::Tab(tab_id)));
        tree.cancel_close_workspace(workspace_id).unwrap();
        assert!(!tree.snapshot().sessions[0].workspaces[0].tabs[0].closing);
        tree.validate().unwrap();
    }

    #[test]
    fn pane_paths_borrow_each_pane_and_its_snapshot_ancestry() {
        let mut tree = ResourceTree::default();
        let path = initial("session", "/project");
        let first_pane_id = path.pane_id;
        let tab_id = path.tab_id;
        tree.create_session(path).unwrap();
        let second_pane_id = PaneId::new();
        tree.add_pane(tab_id, second_pane_id, TerminalId::new())
            .unwrap();

        let mut snapshot = tree.snapshot();
        snapshot.sessions[0]
            .tokens
            .insert("session.test".into(), "borrowed".into());
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .tokens
            .insert("pane.test".into(), "borrowed".into());

        let paths = snapshot.pane_paths().collect::<Vec<_>>();
        assert_eq!(
            paths.iter().map(|path| path.pane.id).collect::<Vec<_>>(),
            vec![first_pane_id, second_pane_id]
        );
        for path in &paths {
            assert!(std::ptr::eq(path.session, &snapshot.sessions[0]));
            assert!(std::ptr::eq(
                path.workspace,
                &snapshot.sessions[0].workspaces[0]
            ));
            assert!(std::ptr::eq(
                path.tab,
                &snapshot.sessions[0].workspaces[0].tabs[0]
            ));
        }
        assert!(std::ptr::eq(
            paths[0].pane,
            &snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
        ));
        assert_eq!(paths[0].session.tokens["session.test"], "borrowed");
        assert_eq!(paths[0].pane.tokens["pane.test"], "borrowed");
    }

    #[test]
    fn snapshots_resolve_live_ancestry_from_stable_terminal_identity() {
        let mut tree = ResourceTree::default();
        let path = initial("session", "/project");
        let terminal_id = path.terminal_id;
        let pane_id = path.pane_id;
        let workspace_id = path.workspace_id;
        let original_tab_id = path.tab_id;
        tree.create_session(path).unwrap();
        assert_eq!(
            tree.snapshot().live_terminal_path(terminal_id).unwrap(),
            ResolvedTerminalPath {
                session_id: tree.snapshot().sessions[0].id,
                workspace_id,
                tab_id: original_tab_id,
                pane_id,
                terminal_id,
            }
        );

        let destination = TabId::new();
        tree.add_tab(
            workspace_id,
            TabPath {
                tab_id: destination,
                tab_name: "destination".into(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();
        tree.move_pane(pane_id, destination).unwrap();
        assert_eq!(
            tree.snapshot()
                .live_terminal_path(terminal_id)
                .unwrap()
                .tab_id,
            destination
        );

        tree.close_pane(pane_id).unwrap();
        assert_eq!(
            tree.snapshot().live_terminal_path(terminal_id),
            Err(ResourceError::Closing("pane"))
        );
        assert_eq!(
            tree.snapshot().live_terminal_path(TerminalId::new()),
            Err(ResourceError::NotFound("terminal"))
        );
    }
}
