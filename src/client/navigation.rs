use std::collections::HashMap;
use std::hash::Hash;

use crate::{
    domain::{PaneId, SessionId, TabId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, SessionSnapshot, TabSnapshot, WorkspaceSnapshot},
};

#[derive(Default)]
pub(super) struct NavigationHistory {
    panes_by_tab: RecentChildren<TabId, PaneId>,
    tabs_by_workspace: RecentChildren<WorkspaceId, TabId>,
    workspaces_by_session: RecentChildren<SessionId, WorkspaceId>,
    last_panes: HashMap<TabId, PaneId>,
    last_tabs: HashMap<WorkspaceId, TabId>,
    last_workspaces: HashMap<SessionId, WorkspaceId>,
    last_session: Option<SessionId>,
}

impl NavigationHistory {
    pub fn record(&mut self, target: &SelectedTarget) {
        self.panes_by_tab.record(target.tab_id, target.pane_id);
        self.tabs_by_workspace
            .record(target.workspace_id, target.tab_id);
        self.workspaces_by_session
            .record(target.session_id, target.workspace_id);
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
                && let Some(pane_id) = self.tab_destination(tab)
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
            .then(|| self.tab_destination(tab))
            .flatten()
    }

    pub fn adjacent_workspace(
        &self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        forward: bool,
    ) -> Option<PaneId> {
        let path = focused_path(snapshot, focused.pane_id)?;
        let workspaces = &path.session.workspaces;
        if workspaces.len() < 2 {
            return None;
        }
        let current = workspaces
            .iter()
            .position(|workspace| workspace.id == path.workspace.id)?;
        for offset in 1..workspaces.len() {
            let index = if forward {
                (current + offset) % workspaces.len()
            } else {
                (current + workspaces.len() - offset) % workspaces.len()
            };
            let workspace = &workspaces[index];
            if !workspace.closing
                && let Some(pane_id) = self.workspace_destination(workspace)
            {
                return Some(pane_id);
            }
        }
        None
    }

    pub fn last_pane(
        &self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
    ) -> Option<PaneId> {
        let path = focused_path(snapshot, focused.pane_id)?;
        let pane_id = *self.last_panes.get(&path.tab.id)?;
        (pane_id != path.pane_id && pane_is_open(path.tab, pane_id)).then_some(pane_id)
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
        self.tab_destination(tab)
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
        self.tabs_by_workspace
            .recent(&workspace.id)
            .find_map(|tab_id| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| !tab.closing && tab.id == *tab_id)
                    .and_then(|tab| self.tab_destination(tab))
            })
            .or_else(|| {
                workspace
                    .tabs
                    .iter()
                    .filter(|tab| !tab.closing)
                    .find_map(|tab| self.tab_destination(tab))
            })
    }

    pub fn tab_destination(&self, tab: &TabSnapshot) -> Option<PaneId> {
        self.panes_by_tab
            .recent(&tab.id)
            .copied()
            .find(|pane_id| pane_is_open(tab, *pane_id))
            .or_else(|| first_open_pane(tab))
    }

    pub fn session_destination(&self, session: &SessionSnapshot) -> Option<PaneId> {
        self.workspaces_by_session
            .recent(&session.id)
            .find_map(|workspace_id| {
                session
                    .workspaces
                    .iter()
                    .find(|workspace| !workspace.closing && workspace.id == *workspace_id)
                    .and_then(|workspace| self.workspace_destination(workspace))
            })
            .or_else(|| {
                session
                    .workspaces
                    .iter()
                    .filter(|workspace| !workspace.closing)
                    .find_map(|workspace| self.workspace_destination(workspace))
            })
    }
}

struct RecentChildren<K, V> {
    by_parent: HashMap<K, Vec<V>>,
}

impl<K, V> Default for RecentChildren<K, V> {
    fn default() -> Self {
        Self {
            by_parent: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash, V: Eq> RecentChildren<K, V> {
    fn record(&mut self, parent: K, child: V) {
        let children = self.by_parent.entry(parent).or_default();
        children.retain(|candidate| *candidate != child);
        children.push(child);
    }

    fn recent<'a>(&'a self, parent: &K) -> impl Iterator<Item = &'a V> {
        self.by_parent
            .get(parent)
            .into_iter()
            .flat_map(|children| children.iter().rev())
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
                            pane_is_open(tab, pane_id).then_some(FocusedPath {
                                session,
                                workspace,
                                tab,
                                pane_id,
                            })
                        })
                })
        })
}

fn first_open_pane(tab: &TabSnapshot) -> Option<PaneId> {
    tab.panes
        .iter()
        .find(|pane| !pane.closing)
        .map(|pane| pane.id)
}

fn pane_is_open(tab: &TabSnapshot, pane_id: PaneId) -> bool {
    tab.panes
        .iter()
        .any(|pane| !pane.closing && pane.id == pane_id)
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
            tokens: Default::default(),
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing: false,
            activity: Default::default(),
            cwd: None,
            worktree: None,
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
            tokens: Default::default(),
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
                tokens: Default::default(),
                id: SessionId::new(),
                name: "one".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/one")),
                },
                trusted_project_config: None,
                closing: false,
                workspaces: vec![
                    WorkspaceSnapshot {
                        tokens: Default::default(),
                        id: WorkspaceId::new(),
                        name: "main".into(),
                        root: PathBuf::from("/one/main"),
                        closing: false,
                        tabs: vec![tab("a", vec![first, second]), tab("b", vec![pane()])],
                    },
                    WorkspaceSnapshot {
                        tokens: Default::default(),
                        id: WorkspaceId::new(),
                        name: "feature".into(),
                        root: PathBuf::from("/one/feature"),
                        closing: false,
                        tabs: vec![tab("c", vec![pane()])],
                    },
                ],
            },
            SessionSnapshot {
                tokens: Default::default(),
                id: SessionId::new(),
                name: "two".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/two")),
                },
                trusted_project_config: None,
                closing: false,
                workspaces: vec![WorkspaceSnapshot {
                    tokens: Default::default(),
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
        let a1 = target(session_one, main, tab_a, tab_a.panes[0].clone());
        let a2 = target(session_one, main, tab_a, tab_a.panes[1].clone());
        let b = target(session_one, main, tab_b, tab_b.panes[0].clone());
        let feature = &session_one.workspaces[1];
        let c = target(
            session_one,
            feature,
            &feature.tabs[0],
            feature.tabs[0].panes[0].clone(),
        );
        let session_two = &snapshot.sessions[1];
        let two_main = &session_two.workspaces[0];
        let d = target(
            session_two,
            two_main,
            &two_main.tabs[0],
            two_main.tabs[0].panes[0].clone(),
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
    fn parent_destinations_follow_recent_open_children_at_each_level() {
        let mut snapshot = fixture();
        let session = &snapshot.sessions[0];
        let main = &session.workspaces[0];
        let tab_a = &main.tabs[0];
        let a1 = target(session, main, tab_a, tab_a.panes[0].clone());
        let a2 = target(session, main, tab_a, tab_a.panes[1].clone());
        let tab_b = &main.tabs[1];
        let b = target(session, main, tab_b, tab_b.panes[0].clone());

        let mut history = NavigationHistory::default();
        history.record(&a1);
        history.record(&a2);
        history.record(&b);

        snapshot.sessions[0].workspaces[0].tabs[1].closing = true;
        snapshot.sessions[0].workspaces[0].tabs[0].panes[1].closing = true;

        assert_eq!(
            history.session_destination(&snapshot.sessions[0]),
            Some(a1.pane_id),
            "the session descends through the most recent open workspace, tab, and pane"
        );
    }

    #[test]
    fn moving_a_remembered_pane_does_not_change_its_remembered_tab() {
        let mut snapshot = fixture();
        let session = &snapshot.sessions[0];
        let main = &session.workspaces[0];
        let tab = &main.tabs[0];
        let remembered = target(session, main, tab, tab.panes[1].clone());
        let expected = tab.panes[0].id;
        let mut history = NavigationHistory::default();
        history.record(&remembered);

        let moved = snapshot.sessions[0].workspaces[0].tabs[0].panes.remove(1);
        snapshot.sessions[0].workspaces[0].tabs[1].panes.push(moved);

        assert_eq!(
            history.session_destination(&snapshot.sessions[0]),
            Some(expected),
            "parent history descends through the remembered tab rather than following the moved pane"
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
            workspace.tabs[0].panes[0].clone(),
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

    #[test]
    fn adjacent_workspaces_wrap_within_the_focused_session() {
        let mut snapshot = fixture();
        let session = &snapshot.sessions[0];
        let workspace = &session.workspaces[0];
        let focused = target(
            session,
            workspace,
            &workspace.tabs[0],
            workspace.tabs[0].panes[0].clone(),
        );
        let feature = session.workspaces[1].tabs[0].panes[0].id;
        let history = NavigationHistory::default();

        assert_eq!(
            history.adjacent_workspace(&snapshot, &focused, true),
            Some(feature)
        );
        assert_eq!(
            history.adjacent_workspace(&snapshot, &focused, false),
            Some(feature)
        );

        snapshot.sessions[0].workspaces[1].closing = true;
        assert_eq!(history.adjacent_workspace(&snapshot, &focused, true), None);
    }
}
