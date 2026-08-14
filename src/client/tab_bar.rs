use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};

use crate::{
    domain::{PaneId, TabId, WorkspaceId},
    protocol::SelectedTarget,
    resources::ResourceSnapshot,
};

use super::config::UiConfig;
use super::{
    chrome::render_tab_bar, navigation::NavigationHistory, notifications::NotificationState,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct TabItem {
    id: TabId,
    name: String,
    closing: bool,
    destination: Option<PaneId>,
}

pub(super) struct TabBarState {
    workspace_id: WorkspaceId,
    focused_tab: TabId,
    items: Vec<TabItem>,
    selected: Option<TabId>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum TabBarAction {
    Stay,
    Close,
    Create,
    Rename(TabId, String),
    Select(PaneId),
}

impl TabBarState {
    pub fn open(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
    ) -> Option<Self> {
        let workspace_id = focused_workspace(snapshot, focused)?.id;
        let items = tab_items(snapshot, workspace_id, history);
        let selected = items
            .iter()
            .find(|item| item.id == focused.tab_id && item.destination.is_some())
            .or_else(|| items.iter().find(|item| item.destination.is_some()))?
            .id;
        Some(Self {
            workspace_id,
            focused_tab: focused.tab_id,
            items,
            selected: Some(selected),
        })
    }

    pub fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
    ) {
        let Some(workspace) = focused_workspace(snapshot, focused) else {
            self.items.clear();
            self.selected = None;
            return;
        };
        let previous_focused_tab = self.focused_tab;
        self.workspace_id = workspace.id;
        self.focused_tab = focused.tab_id;
        self.items = tab_items(snapshot, workspace.id, history);
        self.selected = (self.focused_tab != previous_focused_tab)
            .then_some(self.focused_tab)
            .filter(|id| self.selectable(*id))
            .or_else(|| self.selected.filter(|id| self.selectable(*id)))
            .or_else(|| {
                self.items
                    .iter()
                    .find(|item| item.id == focused.tab_id && item.destination.is_some())
                    .map(|item| item.id)
            })
            .or_else(|| {
                self.items
                    .iter()
                    .find(|item| item.destination.is_some())
                    .map(|item| item.id)
            });
    }

    pub fn key(&mut self, key: KeyEvent) -> TabBarAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return TabBarAction::Stay;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => TabBarAction::Close,
            KeyCode::Char('c') if key.modifiers == KeyModifiers::NONE => TabBarAction::Create,
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self
                .selected_item()
                .map(|item| TabBarAction::Rename(item.id, item.name.clone()))
                .unwrap_or(TabBarAction::Stay),
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                self.move_selection(false);
                TabBarAction::Stay
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Char('l') | KeyCode::Char('j') => {
                self.move_selection(true);
                TabBarAction::Stay
            }
            KeyCode::Home => {
                self.selected = self
                    .items
                    .iter()
                    .find(|item| item.destination.is_some())
                    .map(|item| item.id);
                TabBarAction::Stay
            }
            KeyCode::End => {
                self.selected = self
                    .items
                    .iter()
                    .rev()
                    .find(|item| item.destination.is_some())
                    .map(|item| item.id);
                TabBarAction::Stay
            }
            KeyCode::Enter => {
                let Some(item) = self.selected_item() else {
                    return TabBarAction::Stay;
                };
                if item.id == self.focused_tab {
                    TabBarAction::Close
                } else {
                    item.destination
                        .map(TabBarAction::Select)
                        .unwrap_or(TabBarAction::Stay)
                }
            }
            _ => TabBarAction::Stay,
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the active tab bar forwards the complete passive renderer context"
    )]
    pub fn render(
        &self,
        snapshot: Option<&ResourceSnapshot>,
        focused: &SelectedTarget,
        zoomed: bool,
        ui: &UiConfig,
        notifications: &NotificationState,
        spinner_frame: usize,
        area: Rect,
        buffer: &mut Buffer,
    ) {
        render_tab_bar(
            snapshot,
            focused,
            zoomed,
            self.selected,
            notifications,
            spinner_frame,
            ui,
            area,
            buffer,
        );
    }

    fn selectable(&self, id: TabId) -> bool {
        self.items
            .iter()
            .any(|item| item.id == id && item.destination.is_some())
    }

    fn selected_item(&self) -> Option<&TabItem> {
        self.selected
            .and_then(|id| self.items.iter().find(|item| item.id == id))
            .filter(|item| !item.closing)
    }

    fn move_selection(&mut self, forward: bool) {
        let available = self
            .items
            .iter()
            .filter(|item| item.destination.is_some())
            .map(|item| item.id)
            .collect::<Vec<_>>();
        if available.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|selected| available.iter().position(|id| *id == selected))
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % available.len()
        } else if current == 0 {
            available.len() - 1
        } else {
            current - 1
        };
        self.selected = Some(available[next]);
    }
}

fn focused_workspace<'a>(
    snapshot: &'a ResourceSnapshot,
    focused: &SelectedTarget,
) -> Option<&'a crate::resources::WorkspaceSnapshot> {
    snapshot.sessions.iter().find_map(|session| {
        session.workspaces.iter().find(|workspace| {
            workspace
                .tabs
                .iter()
                .any(|tab| tab.panes.iter().any(|pane| pane.id == focused.pane_id))
        })
    })
}

fn tab_items(
    snapshot: &ResourceSnapshot,
    workspace_id: WorkspaceId,
    history: &NavigationHistory,
) -> Vec<TabItem> {
    snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.workspaces)
        .find(|workspace| workspace.id == workspace_id)
        .map(|workspace| {
            workspace
                .tabs
                .iter()
                .map(|tab| {
                    let closing = workspace.closing || tab.closing;
                    TabItem {
                        id: tab.id,
                        name: tab.name.clone(),
                        closing,
                        destination: (!closing).then(|| history.tab_destination(tab)).flatten(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        domain::{SessionId, TerminalId},
        resources::{
            PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
        },
        splits::SplitTree,
    };

    fn fixture() -> (ResourceSnapshot, SelectedTarget, NavigationHistory) {
        let session_id = SessionId::new();
        let workspace_id = WorkspaceId::new();
        let first_pane = PaneId::new();
        let second_pane = PaneId::new();
        let first_terminal = TerminalId::new();
        let second_terminal = TerminalId::new();
        let first_tab = TabId::new();
        let second_tab = TabId::new();
        let tabs = vec![
            TabSnapshot {
                tokens: Default::default(),
                id: first_tab,
                name: "shell".into(),
                closing: false,
                layout: SplitTree::leaf(first_pane),
                panes: vec![PaneSnapshot {
                    tokens: Default::default(),
                    id: first_pane,
                    terminal_id: first_terminal,
                    closing: false,
                    activity: Default::default(),
                    cwd: None,
                    worktree: None,
                }],
            },
            TabSnapshot {
                tokens: Default::default(),
                id: second_tab,
                name: "tests".into(),
                closing: false,
                layout: SplitTree::leaf(second_pane),
                panes: vec![PaneSnapshot {
                    tokens: Default::default(),
                    id: second_pane,
                    terminal_id: second_terminal,
                    closing: false,
                    activity: Default::default(),
                    cwd: None,
                    worktree: None,
                }],
            },
        ];
        let snapshot = ResourceSnapshot {
            revision: 1,
            sessions: vec![SessionSnapshot {
                tokens: Default::default(),
                id: session_id,
                name: "project".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/project")),
                },
                closing: false,
                workspaces: vec![WorkspaceSnapshot {
                    tokens: Default::default(),
                    id: workspace_id,
                    name: "main".into(),
                    root: PathBuf::from("/project"),
                    closing: false,
                    tabs,
                }],
            }],
        };
        let focused = SelectedTarget {
            session_id,
            workspace_id,
            tab_id: first_tab,
            pane_id: first_pane,
            terminal_id: first_terminal,
            child_pid: 1,
        };
        let mut history = NavigationHistory::default();
        history.record(&focused);
        let mut second = focused.clone();
        second.tab_id = second_tab;
        second.pane_id = second_pane;
        second.terminal_id = second_terminal;
        history.record(&second);
        (snapshot, focused, history)
    }

    #[test]
    fn focus_navigation_create_and_rename_are_contextual() {
        let (snapshot, focused, history) = fixture();
        let mut state = TabBarState::open(&snapshot, &focused, &history).unwrap();
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            TabBarAction::Create
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            TabBarAction::Rename(focused.tab_id, "shell".into())
        );
        state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TabBarAction::Select(_)
        ));
        state.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TabBarAction::Close
        );
    }

    #[test]
    fn active_render_marks_selection_and_shows_contextual_help() {
        let (snapshot, focused, history) = fixture();
        let mut state = TabBarState::open(&snapshot, &focused, &history).unwrap();
        state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let area = Rect::new(0, 0, 80, 1);
        let mut buffer = Buffer::empty(area);
        state.render(
            Some(&snapshot),
            &focused,
            false,
            &UiConfig::default(),
            &NotificationState::default(),
            0,
            area,
            &mut buffer,
        );
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(text.contains("c new · r rename · esc"));
        assert_eq!(buffer[(13, 0)].bg, ratatui::style::Color::DarkGray);
        assert!(
            !buffer[(13, 0)]
                .modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "selection reads as a background, never an underline"
        );

        let tiny = Rect::new(0, 0, 4, 1);
        let mut tiny_buffer = Buffer::empty(tiny);
        state.render(
            Some(&snapshot),
            &focused,
            false,
            &UiConfig::default(),
            &NotificationState::default(),
            0,
            tiny,
            &mut tiny_buffer,
        );
        assert_eq!(tiny_buffer[(1, 0)].symbol(), "2");
        assert_eq!(tiny_buffer[(1, 0)].bg, ratatui::style::Color::DarkGray);
        assert!(
            !tiny_buffer[(1, 0)]
                .modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        );
    }
}
