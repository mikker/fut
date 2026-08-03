//! Pure, synchronous ownership tree for Fut's live resources.
//!
//! Project identities and workspace roots are boundary values: callers must resolve and
//! canonicalize them before inserting them here. This tree deliberately performs no I/O.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{PaneId, SessionId, TabId, TerminalId, WorkspaceId};

fn disambiguate(suggested: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(suggested) {
        return suggested.to_owned();
    }
    (2..)
        .map(|suffix| format!("{suggested}-{suffix}"))
        .find(|candidate| !exists(candidate))
        .expect("an unbounded suffix must produce a unique name")
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
    pub closing: bool,
    pub workspaces: Vec<WorkspaceSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    pub name: String,
    pub root: PathBuf,
    pub closing: bool,
    pub tabs: Vec<TabSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub id: TabId,
    pub name: String,
    pub closing: bool,
    pub panes: Vec<PaneSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub id: PaneId,
    pub terminal_id: TerminalId,
    pub closing: bool,
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
    },
    WorkspaceRenamed {
        session_id: SessionId,
        id: WorkspaceId,
        old_name: String,
        new_name: String,
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
        workspace_id: WorkspaceId,
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
    #[error("resource tree invariant violated: {0}")]
    Invariant(String),
}

#[derive(Clone, Debug)]
struct Session {
    name: String,
    project: Project,
    closing: bool,
    workspaces: Vec<WorkspaceId>,
}
#[derive(Clone, Debug)]
struct Workspace {
    session_id: SessionId,
    name: String,
    root: PathBuf,
    closing: bool,
    tabs: Vec<TabId>,
}
#[derive(Clone, Debug)]
struct Tab {
    workspace_id: WorkspaceId,
    name: String,
    closing: bool,
    panes: Vec<PaneId>,
}
#[derive(Clone, Copy, Debug)]
struct Pane {
    tab_id: TabId,
    terminal_id: TerminalId,
    closing: bool,
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
}

impl ResourceTree {
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
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
            .iter()
            .find(|(_, workspace)| workspace.root == root);
        let Some(&session_id) = matching_sessions.first() else {
            return if root_owner.is_some() {
                self.invalid("workspace root belongs to another project")
            } else {
                Ok(CheckoutDestination::CreateSession)
            };
        };
        let session = &self.sessions[&session_id];
        if session.closing {
            return Err(ResourceError::Closing("session"));
        }
        if let Some((&workspace_id, workspace)) = root_owner {
            if workspace.session_id != session_id {
                return self.invalid("workspace root belongs to another project");
            }
            if workspace.closing {
                return Err(ResourceError::Closing("workspace"));
            }
            return Ok(CheckoutDestination::Existing(workspace_id));
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

    pub fn available_session_name(&self, suggested: &str) -> String {
        disambiguate(suggested, |name| {
            self.sessions.values().any(|item| item.name == name)
        })
    }

    pub fn available_workspace_name(&self, session_id: SessionId, suggested: &str) -> String {
        disambiguate(suggested, |name| {
            self.sessions.get(&session_id).is_some_and(|session| {
                session
                    .workspaces
                    .iter()
                    .any(|id| self.workspaces[id].name == name)
            })
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
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if tab.closing {
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

    pub fn create_session(&mut self, path: InitialPath) -> Result<Mutation, ResourceError> {
        self.check_names([&path.session_name, &path.workspace_name, &path.tab_name])?;
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
        if self.workspaces.values().any(|w| w.root == path.root) {
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
        };
        self.sessions.insert(
            path.session_id,
            Session {
                name: path.session_name,
                project: path.project,
                closing: false,
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
                closing: false,
                tabs: vec![path.tab_id],
            },
        );
        self.tabs.insert(
            path.tab_id,
            Tab {
                workspace_id: path.workspace_id,
                name: path.tab_name,
                closing: false,
                panes: vec![path.pane_id],
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
        if session.closing {
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
        self.check_names([&new_name])?;
        let workspace = self
            .workspaces
            .get(&workspace_id)
            .ok_or(ResourceError::NotFound("workspace"))?;
        let session_id = workspace.session_id;
        if self.sessions[&session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if workspace.name == new_name {
            return Ok(self.unchanged());
        }
        if self.sessions[&session_id]
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
        Ok(self.finish(
            vec![ResourceEvent::WorkspaceRenamed {
                session_id,
                id: workspace_id,
                old_name,
                new_name,
            }],
            vec![],
        ))
    }

    pub fn rename_tab(
        &mut self,
        tab_id: TabId,
        new_name: String,
    ) -> Result<Mutation, ResourceError> {
        self.check_names([&new_name])?;
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        let workspace_id = tab.workspace_id;
        let workspace = &self.workspaces[&workspace_id];
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if tab.name == new_name {
            return Ok(self.unchanged());
        }
        if tab.closing {
            return Err(ResourceError::Closing("tab"));
        }
        if workspace
            .tabs
            .iter()
            .any(|id| *id != tab_id && self.tabs[id].name == new_name)
        {
            return Err(ResourceError::Duplicate("tab name"));
        }

        let old_name = std::mem::replace(
            &mut self.tabs.get_mut(&tab_id).unwrap().name,
            new_name.clone(),
        );
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
        self.check_names([&path.workspace_name, &path.tab_name])?;
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if session.closing {
            return Err(ResourceError::Closing("session"));
        }
        if session
            .workspaces
            .iter()
            .any(|id| self.workspaces[id].name == path.workspace_name)
        {
            return Err(ResourceError::Duplicate("workspace name"));
        }
        if self.workspaces.values().any(|w| w.root == path.root) {
            return Err(ResourceError::Duplicate("workspace root"));
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
                closing: false,
                tabs: vec![path.tab_id],
            },
        );
        self.tabs.insert(
            path.tab_id,
            Tab {
                workspace_id: path.workspace_id,
                name: path.tab_name,
                closing: false,
                panes: vec![path.pane_id],
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
        self.check_names([&path.tab_name])?;
        let workspace = self
            .workspaces
            .get(&workspace_id)
            .ok_or(ResourceError::NotFound("workspace"))?;
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if workspace
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
                closing: false,
                panes: vec![path.pane_id],
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
        if self.session_for_workspace(tab.workspace_id).closing {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspaces[&tab.workspace_id].closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if tab.closing {
            return Err(ResourceError::Closing("tab"));
        }
        self.check_pane_ids(pane_id, terminal_id)?;
        self.tabs.get_mut(&tab_id).unwrap().panes.push(pane_id);
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

    pub fn move_pane(
        &mut self,
        pane_id: PaneId,
        destination: TabId,
    ) -> Result<Mutation, ResourceError> {
        let pane = *self
            .panes
            .get(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?;
        let source_tab = &self.tabs[&pane.tab_id];
        let destination_tab = self
            .tabs
            .get(&destination)
            .ok_or(ResourceError::NotFound("tab"))?;
        if source_tab.closing || destination_tab.closing {
            return Err(ResourceError::Closing("tab"));
        }
        if pane.closing {
            return Err(ResourceError::Closing("pane"));
        }
        let source_workspace = source_tab.workspace_id;
        if source_workspace != destination_tab.workspace_id {
            return Err(ResourceError::DifferentWorkspace);
        }
        if self.session_for_workspace(source_workspace).closing {
            return Err(ResourceError::Closing("session"));
        }
        if self.workspaces[&source_workspace].closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if pane.tab_id == destination {
            return Ok(self.unchanged());
        }
        self.tabs
            .get_mut(&pane.tab_id)
            .unwrap()
            .panes
            .retain(|id| *id != pane_id);
        self.tabs.get_mut(&destination).unwrap().panes.push(pane_id);
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
        let pane = *self
            .panes
            .get(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?;
        let tab = &self.tabs[&pane.tab_id];
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if tab.closing {
            return Err(ResourceError::Closing("tab"));
        }
        if pane.closing {
            return Err(ResourceError::Closing("pane"));
        }
        self.panes.get_mut(&pane_id).unwrap().closing = true;
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
        let pane = *self
            .panes
            .get(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?;
        let session = self.session_for_tab(pane.tab_id);
        if session.closing {
            return Err(ResourceError::Closing("session"));
        }
        let workspace_id = self.tabs[&pane.tab_id].workspace_id;
        if self.workspaces[&workspace_id].closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if self.tabs[&pane.tab_id].closing {
            return Err(ResourceError::Closing("tab"));
        }
        if !pane.closing {
            return Err(ResourceError::NotFound("pending pane close"));
        }
        self.panes.get_mut(&pane_id).unwrap().closing = false;
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
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if tab.closing {
            return Err(ResourceError::Closing("tab"));
        }
        if tab.panes.iter().any(|id| self.panes[id].closing) {
            return Err(ResourceError::Closing("pane"));
        }
        let panes = tab.panes.clone();
        let terminals = panes.iter().map(|id| self.panes[id].terminal_id).collect();
        self.tabs.get_mut(&tab_id).unwrap().closing = true;
        for pane_id in panes {
            self.panes.get_mut(&pane_id).unwrap().closing = true;
        }
        Ok(self.finish(vec![ResourceEvent::TabCloseRequested { tab_id }], terminals))
    }

    pub fn cancel_close_tab(&mut self, tab_id: TabId) -> Result<Mutation, ResourceError> {
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or(ResourceError::NotFound("tab"))?;
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if !tab.closing {
            return Err(ResourceError::NotFound("pending tab close"));
        }
        let panes = tab.panes.clone();
        self.tabs.get_mut(&tab_id).unwrap().closing = false;
        for pane_id in panes {
            self.panes.get_mut(&pane_id).unwrap().closing = false;
        }
        Ok(self.finish(vec![ResourceEvent::TabCloseCancelled { tab_id }], vec![]))
    }

    pub fn close_session(&mut self, session_id: SessionId) -> Result<Mutation, ResourceError> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if session.closing {
            return Err(ResourceError::Closing("session"));
        }
        let panes = self.session_panes(session_id);
        if panes.iter().any(|id| self.panes[id].closing) {
            return Err(ResourceError::Closing("pane"));
        }
        let terminals: Vec<_> = panes.iter().map(|id| self.panes[id].terminal_id).collect();
        self.sessions.get_mut(&session_id).unwrap().closing = true;
        let workspaces = self.sessions[&session_id].workspaces.clone();
        for workspace_id in workspaces {
            self.workspaces.get_mut(&workspace_id).unwrap().closing = true;
        }
        for pane_id in panes {
            self.panes.get_mut(&pane_id).unwrap().closing = true;
        }
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
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if !session.closing {
            return Err(ResourceError::NotFound("pending session close"));
        }
        let panes = self.session_panes(session_id);
        self.sessions.get_mut(&session_id).unwrap().closing = false;
        let workspaces = self.sessions[&session_id].workspaces.clone();
        for workspace_id in workspaces {
            self.workspaces.get_mut(&workspace_id).unwrap().closing = false;
        }
        for pane_id in panes {
            self.panes.get_mut(&pane_id).unwrap().closing = false;
        }
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
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        let panes = self.workspace_panes(workspace_id);
        if panes.iter().any(|id| self.panes[id].closing) {
            return Err(ResourceError::Closing("pane"));
        }
        let terminals = panes.iter().map(|id| self.panes[id].terminal_id).collect();
        self.workspaces.get_mut(&workspace_id).unwrap().closing = true;
        for pane in panes {
            self.panes.get_mut(&pane).unwrap().closing = true;
        }
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
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if !workspace.closing {
            return Err(ResourceError::NotFound("pending workspace close"));
        }
        let panes = self.workspace_panes(workspace_id);
        self.workspaces.get_mut(&workspace_id).unwrap().closing = false;
        for pane in panes {
            self.panes.get_mut(&pane).unwrap().closing = false;
        }
        Ok(self.finish(
            vec![ResourceEvent::WorkspaceCloseCancelled { workspace_id }],
            vec![],
        ))
    }

    pub fn terminal_exited(&mut self, terminal_id: TerminalId) -> Result<Mutation, ResourceError> {
        let Some(&pane_id) = self.terminals.get(&terminal_id) else {
            return Err(ResourceError::NotFound("terminal"));
        };
        let requested = self.panes[&pane_id].closing;
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

    pub fn validate(&self) -> Result<(), ResourceError> {
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
        let mut roots = BTreeSet::new();
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
                    || workspace.name.trim().is_empty()
                    || workspace.tabs.is_empty()
                    || !workspace_names.insert(&workspace.name)
                    || !roots.insert(&workspace.root)
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
                        || tab.name.trim().is_empty()
                        || tab.panes.is_empty()
                        || !tab_names.insert(&tab.name)
                        || !seen_tabs.insert(*tid)
                        || !unique(&tab.panes)
                    {
                        return self.invalid("tab fields or parent");
                    }
                    for pid in &tab.panes {
                        let Some(pane) = self.panes.get(pid) else {
                            return self.invalid("missing pane");
                        };
                        if pane.tab_id != *tid
                            || !seen_panes.insert(*pid)
                            || self.terminals.get(&pane.terminal_id) != Some(pid)
                            || (session.closing && !pane.closing)
                            || (workspace.closing && !pane.closing)
                            || (tab.closing && !pane.closing)
                        {
                            return self.invalid("pane fields, parent, or closing state");
                        }
                    }
                }
                if session.closing && !workspace.closing {
                    return self.invalid("closing session has open workspace");
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
        Ok(())
    }

    fn remove_pane(&mut self, pane_id: PaneId) -> TabId {
        let pane = self.panes.remove(&pane_id).unwrap();
        self.terminals.remove(&pane.terminal_id);
        self.tabs
            .get_mut(&pane.tab_id)
            .unwrap()
            .panes
            .retain(|id| *id != pane_id);
        pane.tab_id
    }

    fn cascade_empty(&mut self, tab_id: TabId, events: &mut Vec<ResourceEvent>) {
        if !self.tabs[&tab_id].panes.is_empty() {
            return;
        }
        let workspace_id = self.tabs.remove(&tab_id).unwrap().workspace_id;
        self.workspaces
            .get_mut(&workspace_id)
            .unwrap()
            .tabs
            .retain(|id| *id != tab_id);
        events.push(ResourceEvent::TabClosed { tab_id });
        if !self.workspaces[&workspace_id].tabs.is_empty() {
            return;
        }
        let session_id = self.workspaces.remove(&workspace_id).unwrap().session_id;
        self.sessions
            .get_mut(&session_id)
            .unwrap()
            .workspaces
            .retain(|id| *id != workspace_id);
        events.push(ResourceEvent::WorkspaceClosed { workspace_id });
        if !self.sessions[&session_id].workspaces.is_empty() {
            return;
        }
        self.sessions.remove(&session_id);
        self.session_order.retain(|id| *id != session_id);
        events.push(ResourceEvent::SessionClosed { session_id });
    }

    fn insert_pane(&mut self, tab_id: TabId, pane_id: PaneId, terminal_id: TerminalId) {
        self.panes.insert(
            pane_id,
            Pane {
                tab_id,
                terminal_id,
                closing: false,
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
            closing: s.closing,
            workspaces: s
                .workspaces
                .iter()
                .map(|id| self.workspace_snapshot(*id))
                .collect(),
        }
    }
    fn workspace_snapshot(&self, id: WorkspaceId) -> WorkspaceSnapshot {
        let w = &self.workspaces[&id];
        WorkspaceSnapshot {
            id,
            name: w.name.clone(),
            root: w.root.clone(),
            closing: w.closing,
            tabs: w.tabs.iter().map(|id| self.tab_snapshot(*id)).collect(),
        }
    }
    fn tab_snapshot(&self, id: TabId) -> TabSnapshot {
        let t = &self.tabs[&id];
        TabSnapshot {
            id,
            name: t.name.clone(),
            closing: t.closing,
            panes: t.panes.iter().map(|id| self.pane_snapshot(*id)).collect(),
        }
    }
    fn pane_snapshot(&self, id: PaneId) -> PaneSnapshot {
        let p = self.panes[&id];
        PaneSnapshot {
            id,
            terminal_id: p.terminal_id,
            closing: p.closing,
        }
    }
    fn session_for_workspace(&self, workspace_id: WorkspaceId) -> &Session {
        &self.sessions[&self.workspaces[&workspace_id].session_id]
    }
    fn session_for_tab(&self, tab_id: TabId) -> &Session {
        self.session_for_workspace(self.tabs[&tab_id].workspace_id)
    }
    fn session_panes(&self, session_id: SessionId) -> Vec<PaneId> {
        self.sessions[&session_id]
            .workspaces
            .iter()
            .flat_map(|w| &self.workspaces[w].tabs)
            .flat_map(|t| &self.tabs[t].panes)
            .copied()
            .collect()
    }
    fn workspace_panes(&self, workspace_id: WorkspaceId) -> Vec<PaneId> {
        self.workspaces[&workspace_id]
            .tabs
            .iter()
            .flat_map(|tab| &self.tabs[tab].panes)
            .copied()
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
        if session.closing {
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
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
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
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if tab.closing {
            return Err(ResourceError::Closing("tab"));
        }
        if tab.panes.is_empty() {
            return self.invalid("tab has no pane");
        }
        tab.panes.iter().map(|id| self.path_for_pane(*id)).collect()
    }
    fn path_for_pane(&self, pane_id: PaneId) -> Result<ResolvedTerminalPath, ResourceError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?;
        if pane.closing {
            return Err(ResourceError::Closing("pane"));
        }
        let tab = &self.tabs[&pane.tab_id];
        let workspace = &self.workspaces[&tab.workspace_id];
        if self.sessions[&workspace.session_id].closing {
            return Err(ResourceError::Closing("session"));
        }
        if workspace.closing {
            return Err(ResourceError::Closing("workspace"));
        }
        if tab.closing {
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
            workspace_id: WorkspaceId::new(),
            workspace_name: "main".into(),
            root: format!("{project}/main").into(),
            tab_id: TabId::new(),
            tab_name: "shell".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        }
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
    fn roots_are_snapshotted_and_unique_across_peer_workspaces() {
        let mut tree = ResourceTree::default();
        let p = initial("s", "/project");
        let sid = p.session_id;
        let root = p.root.clone();
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
        let before = tree.snapshot();
        let result = tree.add_workspace(sid, duplicate);
        assert_eq!(result, Err(ResourceError::Duplicate("workspace root")));
        assert_eq!(tree.snapshot(), before);
        assert_valid(&tree, &result);
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
        assert_eq!(tree.tabs[&source].panes, vec![source_sibling]);
        assert_eq!(
            tree.tabs[&destination].panes,
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
                    }
                },
                ResourceEvent::WorkspaceCreated {
                    session_id: ids.0,
                    id: ids.1,
                    name: "main".into(),
                    root: "/p/main".into()
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
        let session_id = first.session_id;
        tree.create_session(first).unwrap();
        let second = initial("project-2", "/second");
        tree.create_session(second).unwrap();
        assert_eq!(tree.available_session_name("project"), "project-3");

        tree.add_workspace(
            session_id,
            WorkspacePath {
                workspace_id: WorkspaceId::new(),
                workspace_name: "main-2".into(),
                root: "/first/peer".into(),
                tab_id: TabId::new(),
                tab_name: "shell".into(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
            },
        )
        .unwrap();
        assert_eq!(tree.available_workspace_name(session_id, "main"), "main-3");
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
                old_name: "main".into(),
                new_name: "  Main  ".into(),
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
    fn ancestor_close_does_not_mark_tabs_closing_and_validation_checks_one_way_invariant() {
        let mut tree = ResourceTree::default();
        let path = initial("session", "/project");
        let (workspace_id, tab_id) = (path.workspace_id, path.tab_id);
        tree.create_session(path).unwrap();
        tree.close_workspace(workspace_id).unwrap();
        assert!(!tree.snapshot().sessions[0].workspaces[0].tabs[0].closing);
        tree.cancel_close_workspace(workspace_id).unwrap();

        tree.tabs.get_mut(&tab_id).unwrap().closing = true;
        assert!(matches!(tree.validate(), Err(ResourceError::Invariant(_))));
    }
}
