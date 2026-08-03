//! Pure, synchronous ownership tree for Fut's live resources.
//!
//! Project identities and workspace roots are boundary values: callers must resolve and
//! canonicalize them before inserting them here. This tree deliberately performs no I/O.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{PaneId, SessionId, TabId, TerminalId, WorkspaceId};

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
    pub tabs: Vec<TabSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub id: TabId,
    pub name: String,
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
    WorkspaceCreated {
        session_id: SessionId,
        id: WorkspaceId,
        name: String,
        root: PathBuf,
    },
    TabCreated {
        workspace_id: WorkspaceId,
        id: TabId,
        name: String,
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
    SessionCloseRequested {
        session_id: SessionId,
    },
    PaneCloseCancelled {
        pane_id: PaneId,
        terminal_id: TerminalId,
    },
    SessionCloseCancelled {
        session_id: SessionId,
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
    #[error("source and destination tabs are the same")]
    SameTab,
    #[error("panes may only move between tabs in the same workspace")]
    DifferentWorkspace,
    #[error("resource is closing: {0}")]
    Closing(&'static str),
    #[error("a session target must be selected")]
    TargetRequired,
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
    tabs: Vec<TabId>,
}
#[derive(Clone, Debug)]
struct Tab {
    workspace_id: WorkspaceId,
    name: String,
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
        selector: Option<SessionSelector>,
    ) -> Result<ResolvedTerminalPath, ResourceError> {
        let session_id = match selector {
            Some(selector) => self.resolve_session(selector)?,
            None => {
                let mut open = self.session_order.iter().copied().filter(|id| {
                    self.sessions
                        .get(id)
                        .is_some_and(|session| !session.closing)
                });
                let session_id = open.next().ok_or(ResourceError::TargetRequired)?;
                if open.next().is_some() {
                    return Err(ResourceError::TargetRequired);
                }
                session_id
            }
        };
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(ResourceError::NotFound("session"))?;
        if session.closing {
            return Err(ResourceError::Closing("session"));
        }
        let workspace_id = *session
            .workspaces
            .first()
            .ok_or_else(|| ResourceError::Invariant("session has no workspace".into()))?;
        let workspace = self.workspaces.get(&workspace_id).ok_or_else(|| {
            ResourceError::Invariant("session references missing workspace".into())
        })?;
        let tab_id = *workspace
            .tabs
            .first()
            .ok_or_else(|| ResourceError::Invariant("workspace has no tab".into()))?;
        let tab = self
            .tabs
            .get(&tab_id)
            .ok_or_else(|| ResourceError::Invariant("workspace references missing tab".into()))?;
        let pane_id = *tab
            .panes
            .first()
            .ok_or_else(|| ResourceError::Invariant("tab has no pane".into()))?;
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| ResourceError::Invariant("tab references missing pane".into()))?;
        Ok(ResolvedTerminalPath {
            session_id,
            workspace_id,
            tab_id,
            pane_id,
            terminal_id: pane.terminal_id,
        })
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
                tabs: vec![path.tab_id],
            },
        );
        self.tabs.insert(
            path.tab_id,
            Tab {
                workspace_id: path.workspace_id,
                name: path.tab_name,
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
                tabs: vec![path.tab_id],
            },
        );
        self.tabs.insert(
            path.tab_id,
            Tab {
                workspace_id: path.workspace_id,
                name: path.tab_name,
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
        let destination_tab = self
            .tabs
            .get(&destination)
            .ok_or(ResourceError::NotFound("tab"))?;
        if pane.closing {
            return Err(ResourceError::Closing("pane"));
        }
        if pane.tab_id == destination {
            return Err(ResourceError::SameTab);
        }
        let source_workspace = self.tabs[&pane.tab_id].workspace_id;
        if source_workspace != destination_tab.workspace_id {
            return Err(ResourceError::DifferentWorkspace);
        }
        if self.session_for_workspace(source_workspace).closing {
            return Err(ResourceError::Closing("session"));
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
        let pane = self
            .panes
            .get_mut(&pane_id)
            .ok_or(ResourceError::NotFound("pane"))?;
        if pane.closing {
            return Err(ResourceError::Closing("pane"));
        }
        pane.closing = true;
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
        for pane_id in panes {
            self.panes.get_mut(&pane_id).unwrap().closing = false;
        }
        Ok(self.finish(
            vec![ResourceEvent::SessionCloseCancelled { session_id }],
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
                        {
                            return self.invalid("pane fields, parent, or closing state");
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
            tabs: w.tabs.iter().map(|id| self.tab_snapshot(*id)).collect(),
        }
    }
    fn tab_snapshot(&self, id: TabId) -> TabSnapshot {
        let t = &self.tabs[&id];
        TabSnapshot {
            id,
            name: t.name.clone(),
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
            tree.resolve_terminal_target(None),
            Err(ResourceError::TargetRequired)
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
        assert_eq!(tree.resolve_terminal_target(None), Ok(expected));

        let second = initial("second", "/second");
        let second_id = second.session_id;
        tree.create_session(second).unwrap();
        assert_eq!(
            tree.resolve_terminal_target(None),
            Err(ResourceError::TargetRequired)
        );
        tree.close_session(second_id).unwrap();
        assert_eq!(tree.resolve_terminal_target(None), Ok(expected));
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
            tree.resolve_terminal_target(Some(SessionSelector::Id(first_id))),
            Ok(expected)
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(SessionSelector::Name("first".into()))),
            Ok(expected)
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(SessionSelector::Name("First".into()))),
            Err(ResourceError::NotFound("session"))
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(SessionSelector::Id(SessionId::new()))),
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
            tree.resolve_terminal_target(None),
            Err(ResourceError::TargetRequired)
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(SessionSelector::Id(session_id))),
            Err(ResourceError::Closing("session"))
        );
        assert_eq!(
            tree.resolve_terminal_target(Some(SessionSelector::Name("closing".into()))),
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
            tree.resolve_terminal_target(None),
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
    fn moves_preserve_ids_and_reject_other_workspaces_and_sessions() {
        let mut tree = ResourceTree::default();
        let p = initial("a", "/a");
        let pane = p.pane_id;
        let terminal = p.terminal_id;
        let wid = p.workspace_id;
        tree.create_session(p).unwrap();
        let same = TabPath {
            tab_id: TabId::new(),
            tab_name: "same".into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let same_id = same.tab_id;
        tree.add_tab(wid, same).unwrap();
        tree.move_pane(pane, same_id).unwrap();
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
        let sid = tree.session_order[0];
        tree.add_workspace(sid, peer).unwrap();
        let before = tree.snapshot();
        let result = tree.move_pane(pane, peer_tab);
        assert_eq!(result, Err(ResourceError::DifferentWorkspace));
        assert_eq!(tree.snapshot(), before);
        assert_valid(&tree, &result);
        let other = initial("b", "/b");
        let other_tab = other.tab_id;
        tree.create_session(other).unwrap();
        let result = tree.move_pane(pane, other_tab);
        assert_eq!(result, Err(ResourceError::DifferentWorkspace));
        assert_valid(&tree, &result);
        let snapshot = tree.snapshot();
        let found = snapshot.sessions[0].workspaces[0]
            .tabs
            .iter()
            .flat_map(|t| &t.panes)
            .find(|p| p.id == pane)
            .unwrap();
        assert_eq!(found.terminal_id, terminal);
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
}
