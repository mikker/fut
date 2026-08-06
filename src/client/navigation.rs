use std::collections::HashMap;

use crate::{
    domain::{PaneId, SessionId, TabId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, SessionSnapshot, TabSnapshot, WorkspaceSnapshot},
};

#[derive(Default)]
pub(super) struct NavigationHistory {
    tab_destinations: HashMap<TabId, PaneId>,
    workspace_destinations: HashMap<WorkspaceId, PaneId>,
    session_destinations: HashMap<SessionId, PaneId>,
    last_panes: HashMap<TabId, PaneId>,
    last_tabs: HashMap<WorkspaceId, TabId>,
    last_workspaces: HashMap<SessionId, WorkspaceId>,
    last_session: Option<SessionId>,
}

impl NavigationHistory {
    pub fn record(&mut self, target: &SelectedTarget) {
        self.tab_destinations.insert(target.tab_id, target.pane_id);
        self.workspace_destinations
            .insert(target.workspace_id, target.pane_id);
        self.session_destinations
            .insert(target.session_id, target.pane_id);
    }

    pub fn record_transition(&mut self, previous: &SelectedTarget, target: &SelectedTarget) {
        if previous.tab_id == target.tab_id {
            if previous.pane_id != target.pane_id {
                self.last_panes.insert(target.tab_id, previous.pane_id);
            }
        } else if previous.workspace_id == target.workspace_id {
            self.last_tabs.insert(target.workspace_id, previous.tab_id);
        } else if previous.session_id == target.session_id {
            self.last_workspaces
                .insert(target.session_id, previous.workspace_id);
        } else {
            self.last_session = Some(previous.session_id);
        }
        self.record(target);
    }

    pub fn adjacent_tab(
        &self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        forward: bool,
    ) -> Option<PaneId> {
        let path = focused_path(snapshot, focused.pane_id)?;
        if path.workspace.tabs.len() < 2 {
            return None;
        }
        let current = path
            .workspace
            .tabs
            .iter()
            .position(|tab| tab.id == path.tab.id)?;
        for offset in 1..path.workspace.tabs.len() {
            let index = if forward {
                (current + offset) % path.workspace.tabs.len()
            } else {
                (current + path.workspace.tabs.len() - offset) % path.workspace.tabs.len()
            };
            let tab = &path.workspace.tabs[index];
            if !tab.closing
                && let Some(pane_id) =
                    tab_destination(tab, self.tab_destinations.get(&tab.id).copied())
            {
                return Some(pane_id);
            }
        }
        None
    }

    pub fn numbered_tab(
        &self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        number: u8,
    ) -> Option<PaneId> {
        let path = focused_path(snapshot, focused.pane_id)?;
        let tab = path
            .workspace
            .tabs
            .get(usize::from(number.checked_sub(1)?))?;
        (!tab.closing && tab.id != path.tab.id)
            .then(|| tab_destination(tab, self.tab_destinations.get(&tab.id).copied()))
            .flatten()
    }

    pub fn last_pane(
        &self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
    ) -> Option<PaneId> {
        let path = focused_path(snapshot, focused.pane_id)?;
        let pane_id = *self.last_panes.get(&path.tab.id)?;
        (pane_id != path.pane_id && open_pane(path.tab, pane_id).is_some()).then_some(pane_id)
    }

    pub fn last_tab(
        &self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
    ) -> Option<PaneId> {
        let path = focused_path(snapshot, focused.pane_id)?;
        let tab_id = *self.last_tabs.get(&path.workspace.id)?;
        let tab = path
            .workspace
            .tabs
            .iter()
            .find(|tab| !tab.closing && tab.id == tab_id && tab.id != path.tab.id)?;
        tab_destination(tab, self.tab_destinations.get(&tab.id).copied())
    }

    pub fn last_workspace(
        &self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
    ) -> Option<PaneId> {
        let path = focused_path(snapshot, focused.pane_id)?;
        let workspace_id = *self.last_workspaces.get(&path.session.id)?;
        let workspace = path.session.workspaces.iter().find(|workspace| {
            !workspace.closing && workspace.id == workspace_id && workspace.id != path.workspace.id
        })?;
        self.workspace_destination(workspace)
    }

    pub fn last_session(
        &self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
    ) -> Option<PaneId> {
        let path = focused_path(snapshot, focused.pane_id)?;
        let session_id = self.last_session?;
        let session = snapshot.sessions.iter().find(|session| {
            !session.closing && session.id == session_id && session.id != path.session.id
        })?;
        self.session_destination(session)
    }

    pub fn workspace_destination(&self, workspace: &WorkspaceSnapshot) -> Option<PaneId> {
        self.workspace_destinations
            .get(&workspace.id)
            .copied()
            .filter(|pane_id| workspace_has_open_pane(workspace, *pane_id))
            .or_else(|| {
                workspace
                    .tabs
                    .iter()
                    .filter(|tab| !tab.closing)
                    .find_map(|tab| tab_destination(tab, None))
            })
    }

    pub fn tab_destination(&self, tab: &TabSnapshot) -> Option<PaneId> {
        self.tab_destinations
            .get(&tab.id)
            .copied()
            .filter(|pane_id| open_pane(tab, *pane_id).is_some())
            .or_else(|| tab_destination(tab, None))
    }

    fn session_destination(&self, session: &SessionSnapshot) -> Option<PaneId> {
        self.session_destinations
            .get(&session.id)
            .copied()
            .filter(|pane_id| session_has_open_pane(session, *pane_id))
            .or_else(|| {
                session
                    .workspaces
                    .iter()
                    .filter(|workspace| !workspace.closing)
                    .find_map(|workspace| self.workspace_destination(workspace))
            })
    }
}

struct FocusedPath<'a> {
    session: &'a SessionSnapshot,
    workspace: &'a WorkspaceSnapshot,
    tab: &'a TabSnapshot,
    pane_id: PaneId,
}

fn focused_path(snapshot: &ResourceSnapshot, pane_id: PaneId) -> Option<FocusedPath<'_>> {
    snapshot
        .sessions
        .iter()
        .filter(|session| !session.closing)
        .find_map(|session| {
            session
                .workspaces
                .iter()
                .filter(|workspace| !workspace.closing)
                .find_map(|workspace| {
                    workspace
                        .tabs
                        .iter()
                        .filter(|tab| !tab.closing)
                        .find_map(|tab| {
                            open_pane(tab, pane_id).map(|_| FocusedPath {
                                session,
                                workspace,
                                tab,
                                pane_id,
                            })
                        })
                })
        })
}

fn tab_destination(tab: &TabSnapshot, preferred: Option<PaneId>) -> Option<PaneId> {
    preferred
        .filter(|pane_id| open_pane(tab, *pane_id).is_some())
        .or_else(|| {
            tab.panes
                .iter()
                .find(|pane| !pane.closing)
                .map(|pane| pane.id)
        })
}

fn open_pane(tab: &TabSnapshot, pane_id: PaneId) -> Option<()> {
    tab.panes
        .iter()
        .any(|pane| !pane.closing && pane.id == pane_id)
        .then_some(())
}

fn workspace_has_open_pane(workspace: &WorkspaceSnapshot, pane_id: PaneId) -> bool {
    workspace
        .tabs
        .iter()
        .filter(|tab| !tab.closing)
        .any(|tab| open_pane(tab, pane_id).is_some())
}

fn session_has_open_pane(session: &SessionSnapshot, pane_id: PaneId) -> bool {
    session
        .workspaces
        .iter()
        .filter(|workspace| !workspace.closing)
        .any(|workspace| workspace_has_open_pane(workspace, pane_id))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        domain::TerminalId,
        resources::{
            PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
        },
        splits::SplitTree,
    };

    fn pane() -> crate::resources::PaneSnapshot {
        PaneSnapshot {
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing: false,
            activity: Default::default(),
        }
    }

    fn tab(name: &str, panes: Vec<PaneSnapshot>) -> TabSnapshot {
        let mut layout = SplitTree::leaf(panes[0].id);
        for pane in &panes[1..] {
            assert!(layout.split(
                layout.leaf_ids()[0],
                crate::splits::SplitDirection::Right,
                pane.id
            ));
        }
        TabSnapshot {
            id: TabId::new(),
            name: name.into(),
            closing: false,
            layout,
            panes,
        }
    }

    fn target(
        session: &SessionSnapshot,
        workspace: &WorkspaceSnapshot,
        tab: &TabSnapshot,
        pane: PaneSnapshot,
    ) -> SelectedTarget {
        SelectedTarget {
            session_id: session.id,
            workspace_id: workspace.id,
            tab_id: tab.id,
            pane_id: pane.id,
            terminal_id: pane.terminal_id,
            child_pid: 1,
        }
    }

    fn fixture() -> ResourceSnapshot {
        let first = pane();
        let second = pane();
        let sessions = vec![
            SessionSnapshot {
                id: SessionId::new(),
                name: "one".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/one")),
                },
                closing: false,
                workspaces: vec![
                    WorkspaceSnapshot {
                        id: WorkspaceId::new(),
                        name: "main".into(),
                        root: PathBuf::from("/one/main"),
                        closing: false,
                        tabs: vec![tab("a", vec![first, second]), tab("b", vec![pane()])],
                    },
                    WorkspaceSnapshot {
                        id: WorkspaceId::new(),
                        name: "feature".into(),
                        root: PathBuf::from("/one/feature"),
                        closing: false,
                        tabs: vec![tab("c", vec![pane()])],
                    },
                ],
            },
            SessionSnapshot {
                id: SessionId::new(),
                name: "two".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/two")),
                },
                closing: false,
                workspaces: vec![WorkspaceSnapshot {
                    id: WorkspaceId::new(),
                    name: "main".into(),
                    root: PathBuf::from("/two/main"),
                    closing: false,
                    tabs: vec![tab("d", vec![pane()])],
                }],
            },
        ];
        ResourceSnapshot {
            revision: 1,
            sessions,
        }
    }

    #[test]
    fn last_navigation_toggles_each_scope_and_uses_stale_descendant_fallbacks() {
        let mut snapshot = fixture();
        let session_one = &snapshot.sessions[0];
        let main = &session_one.workspaces[0];
        let tab_a = &main.tabs[0];
        let tab_b = &main.tabs[1];
        let a1 = target(session_one, main, tab_a, tab_a.panes[0]);
        let a2 = target(session_one, main, tab_a, tab_a.panes[1]);
        let b = target(session_one, main, tab_b, tab_b.panes[0]);
        let feature = &session_one.workspaces[1];
        let c = target(
            session_one,
            feature,
            &feature.tabs[0],
            feature.tabs[0].panes[0],
        );
        let session_two = &snapshot.sessions[1];
        let two_main = &session_two.workspaces[0];
        let d = target(
            session_two,
            two_main,
            &two_main.tabs[0],
            two_main.tabs[0].panes[0],
        );

        let mut history = NavigationHistory::default();
        history.record(&a1);
        history.record_transition(&a1, &a2);
        assert_eq!(history.last_pane(&snapshot, &a2), Some(a1.pane_id));
        history.record_transition(&a2, &a1);
        assert_eq!(history.last_pane(&snapshot, &a1), Some(a2.pane_id));

        history.record_transition(&a1, &b);
        assert_eq!(history.last_tab(&snapshot, &b), Some(a1.pane_id));
        history.record_transition(&b, &a1);
        assert_eq!(history.last_tab(&snapshot, &a1), Some(b.pane_id));

        history.record_transition(&a1, &c);
        assert_eq!(history.last_workspace(&snapshot, &c), Some(a1.pane_id));
        history.record_transition(&c, &a1);
        assert_eq!(history.last_workspace(&snapshot, &a1), Some(c.pane_id));

        history.record_transition(&a1, &d);
        assert_eq!(history.last_session(&snapshot, &d), Some(a1.pane_id));
        history.record_transition(&d, &a1);
        assert_eq!(history.last_session(&snapshot, &a1), Some(d.pane_id));

        history.record_transition(&a1, &b);
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0].closing = true;
        assert_eq!(
            history.last_tab(&snapshot, &b),
            Some(snapshot.sessions[0].workspaces[0].tabs[0].panes[1].id),
            "a valid remembered tab falls back to its first open pane"
        );
    }

    #[test]
    fn numbered_and_adjacent_tabs_use_exact_resource_slots_and_fresh_ancestry() {
        let mut snapshot = fixture();
        let session = &snapshot.sessions[0];
        let workspace = &session.workspaces[0];
        let focused = target(
            session,
            workspace,
            &workspace.tabs[0],
            workspace.tabs[0].panes[0],
        );
        let second = workspace.tabs[1].panes[0].id;
        let history = NavigationHistory::default();

        assert_eq!(history.numbered_tab(&snapshot, &focused, 2), Some(second));
        assert_eq!(history.numbered_tab(&snapshot, &focused, 1), None);
        assert_eq!(history.numbered_tab(&snapshot, &focused, 0), None);
        assert_eq!(
            history.adjacent_tab(&snapshot, &focused, true),
            Some(second)
        );
        assert_eq!(
            history.adjacent_tab(&snapshot, &focused, false),
            Some(second)
        );

        snapshot.sessions[0].workspaces[0].tabs[1].closing = true;
        assert_eq!(history.numbered_tab(&snapshot, &focused, 2), None);
        assert_eq!(history.adjacent_tab(&snapshot, &focused, true), None);
    }
}
