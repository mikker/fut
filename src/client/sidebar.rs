use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use crate::{
    domain::{TerminalId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, WorkspaceSnapshot},
};

use super::{
    chrome::{sanitize, truncate},
    config::WorkspaceSidebarPosition,
};

#[derive(Default)]
pub(super) struct WorkspaceHistory {
    terminals: HashMap<WorkspaceId, TerminalId>,
}

impl WorkspaceHistory {
    pub fn record(&mut self, target: &SelectedTarget) {
        self.terminals
            .insert(target.workspace_id, target.terminal_id);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceItem {
    id: WorkspaceId,
    name: String,
    current: bool,
    closing: bool,
    destination: Option<TerminalId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct WorkspaceModel {
    items: Vec<WorkspaceItem>,
}

impl WorkspaceModel {
    fn from_snapshot(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &WorkspaceHistory,
    ) -> Self {
        let (session_id, workspace_id) = focused_ancestry(snapshot, focused);
        let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return Self::default();
        };

        Self {
            items: session
                .workspaces
                .iter()
                .map(|workspace| {
                    let closing = session.closing || workspace.closing;
                    WorkspaceItem {
                        id: workspace.id,
                        name: sanitize(&workspace.name),
                        current: workspace.id == workspace_id,
                        closing,
                        destination: (!closing)
                            .then(|| workspace_destination(workspace, history))
                            .flatten(),
                    }
                })
                .collect(),
        }
    }
}

fn focused_ancestry(
    snapshot: &ResourceSnapshot,
    focused: &SelectedTarget,
) -> (crate::domain::SessionId, WorkspaceId) {
    snapshot
        .sessions
        .iter()
        .find_map(|session| {
            session.workspaces.iter().find_map(|workspace| {
                workspace.tabs.iter().find_map(|tab| {
                    tab.panes
                        .iter()
                        .any(|pane| pane.terminal_id == focused.terminal_id)
                        .then_some((session.id, workspace.id))
                })
            })
        })
        .unwrap_or((focused.session_id, focused.workspace_id))
}

fn workspace_destination(
    workspace: &WorkspaceSnapshot,
    history: &WorkspaceHistory,
) -> Option<TerminalId> {
    let available = |terminal_id| {
        workspace.tabs.iter().any(|tab| {
            !tab.closing
                && tab
                    .panes
                    .iter()
                    .any(|pane| !pane.closing && pane.terminal_id == terminal_id)
        })
    };
    history
        .terminals
        .get(&workspace.id)
        .copied()
        .filter(|terminal_id| available(*terminal_id))
        .or_else(|| {
            workspace
                .tabs
                .iter()
                .filter(|tab| !tab.closing)
                .flat_map(|tab| &tab.panes)
                .find(|pane| !pane.closing)
                .map(|pane| pane.terminal_id)
        })
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkspaceStatus {
    Ready,
    Switching,
    Error(String),
}

pub(super) struct WorkspaceSidebarState {
    model: WorkspaceModel,
    selected: Option<WorkspaceId>,
    status: WorkspaceStatus,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum WorkspaceSidebarAction {
    Stay,
    Close,
    Select(TerminalId),
}

impl WorkspaceSidebarState {
    pub fn open(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &WorkspaceHistory,
    ) -> Option<Self> {
        let model = WorkspaceModel::from_snapshot(snapshot, focused, history);
        let selected = model
            .items
            .iter()
            .find(|item| item.current && item.destination.is_some())
            .or_else(|| model.items.iter().find(|item| item.destination.is_some()))?
            .id;
        Some(Self {
            model,
            selected: Some(selected),
            status: WorkspaceStatus::Ready,
        })
    }

    pub fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &WorkspaceHistory,
    ) {
        let previous = self.selected;
        self.model = WorkspaceModel::from_snapshot(snapshot, focused, history);
        self.selected = previous
            .filter(|id| self.selectable(*id))
            .or_else(|| {
                self.model
                    .items
                    .iter()
                    .find(|item| item.current && item.destination.is_some())
                    .map(|item| item.id)
            })
            .or_else(|| {
                self.model
                    .items
                    .iter()
                    .find(|item| item.destination.is_some())
                    .map(|item| item.id)
            });
    }

    pub fn key(&mut self, key: KeyEvent) -> WorkspaceSidebarAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || matches!(self.status, WorkspaceStatus::Switching)
        {
            return WorkspaceSidebarAction::Stay;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => WorkspaceSidebarAction::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(false);
                WorkspaceSidebarAction::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(true);
                WorkspaceSidebarAction::Stay
            }
            KeyCode::Home => {
                self.selected = self
                    .model
                    .items
                    .iter()
                    .find(|item| item.destination.is_some())
                    .map(|item| item.id);
                self.status = WorkspaceStatus::Ready;
                WorkspaceSidebarAction::Stay
            }
            KeyCode::End => {
                self.selected = self
                    .model
                    .items
                    .iter()
                    .rev()
                    .find(|item| item.destination.is_some())
                    .map(|item| item.id);
                self.status = WorkspaceStatus::Ready;
                WorkspaceSidebarAction::Stay
            }
            KeyCode::Enter => {
                let Some(item) = self
                    .selected
                    .and_then(|id| self.model.items.iter().find(|item| item.id == id))
                else {
                    return WorkspaceSidebarAction::Stay;
                };
                if item.current {
                    WorkspaceSidebarAction::Close
                } else if let Some(destination) = item.destination {
                    WorkspaceSidebarAction::Select(destination)
                } else {
                    WorkspaceSidebarAction::Stay
                }
            }
            _ => WorkspaceSidebarAction::Stay,
        }
    }

    pub fn begin_switch(&mut self) {
        self.status = WorkspaceStatus::Switching;
    }

    pub fn switch_error(&mut self, message: String) {
        self.status = WorkspaceStatus::Error(sanitize(&message));
    }

    pub fn render(&self, area: Rect, position: WorkspaceSidebarPosition, buffer: &mut Buffer) {
        render_model(
            &self.model,
            self.selected,
            Some(&self.status),
            area,
            position,
            buffer,
        );
    }

    fn selectable(&self, id: WorkspaceId) -> bool {
        self.model
            .items
            .iter()
            .any(|item| item.id == id && item.destination.is_some())
    }

    fn move_selection(&mut self, forward: bool) {
        let available = self
            .model
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
        self.status = WorkspaceStatus::Ready;
    }
}

pub(super) fn render_workspace_sidebar(
    snapshot: Option<&ResourceSnapshot>,
    focused: &SelectedTarget,
    history: &WorkspaceHistory,
    area: Rect,
    position: WorkspaceSidebarPosition,
    buffer: &mut Buffer,
) {
    let model = snapshot
        .map(|snapshot| WorkspaceModel::from_snapshot(snapshot, focused, history))
        .unwrap_or_default();
    render_model(&model, None, None, area, position, buffer);
}

fn render_model(
    model: &WorkspaceModel,
    selected: Option<WorkspaceId>,
    status: Option<&WorkspaceStatus>,
    area: Rect,
    position: WorkspaceSidebarPosition,
    buffer: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    clear(area, buffer);
    let (content, divider_x) = if area.width == 1 {
        (area, None)
    } else {
        match position {
            WorkspaceSidebarPosition::Left => (
                Rect::new(area.x, area.y, area.width - 1, area.height),
                Some(area.x.saturating_add(area.width - 1)),
            ),
            WorkspaceSidebarPosition::Right => (
                Rect::new(
                    area.x.saturating_add(1),
                    area.y,
                    area.width - 1,
                    area.height,
                ),
                Some(area.x),
            ),
        }
    };
    if let Some(divider_x) = divider_x {
        for row in area.y..area.y.saturating_add(area.height) {
            if let Some(cell) = buffer.cell_mut((divider_x, row)) {
                cell.set_symbol("│").set_style(muted_style());
            }
        }
    }
    if content.width == 0 {
        return;
    }

    let footer =
        status.filter(|status| content.height >= 5 || !matches!(status, WorkspaceStatus::Ready));
    let row_height = content.height.saturating_sub(u16::from(footer.is_some()));
    if model.items.is_empty() {
        let fallback = format_workspace("workspace", true, false, usize::from(content.width));
        buffer.set_stringn(
            content.x,
            content.y,
            fallback,
            usize::from(content.width),
            active_style(),
        );
    } else {
        let anchor = selected
            .and_then(|id| model.items.iter().position(|item| item.id == id))
            .or_else(|| model.items.iter().position(|item| item.current))
            .unwrap_or(0);
        for (offset, row) in visible_rows(model.items.len(), anchor, usize::from(row_height))
            .into_iter()
            .enumerate()
        {
            let y = content
                .y
                .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
            match row {
                VisibleRow::Ellipsis => {
                    buffer.set_stringn(
                        content.x,
                        y,
                        " …",
                        usize::from(content.width),
                        muted_style(),
                    );
                }
                VisibleRow::Item(index) => {
                    let item = &model.items[index];
                    let text = format_workspace(
                        &item.name,
                        item.current,
                        item.closing,
                        usize::from(content.width),
                    );
                    let mut style = Style::default();
                    if item.current {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if item.closing {
                        style = style.add_modifier(Modifier::DIM);
                    }
                    if selected == Some(item.id) {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    buffer.set_stringn(content.x, y, text, usize::from(content.width), style);
                }
            }
        }
    }

    if let Some(status) = footer {
        let text = match status {
            WorkspaceStatus::Ready => " ↑↓ enter · esc".into(),
            WorkspaceStatus::Switching => " switching…".into(),
            WorkspaceStatus::Error(message) => format!(" {message} · retry"),
        };
        let text = truncate(&text, usize::from(content.width));
        buffer.set_stringn(
            content.x,
            content.y.saturating_add(content.height - 1),
            &text,
            usize::from(content.width),
            muted_style(),
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisibleRow {
    Ellipsis,
    Item(usize),
}

fn visible_rows(length: usize, anchor: usize, height: usize) -> Vec<VisibleRow> {
    if length == 0 || height == 0 {
        return Vec::new();
    }
    let anchor = anchor.min(length - 1);
    if length <= height {
        return (0..length).map(VisibleRow::Item).collect();
    }
    if height == 1 {
        return vec![VisibleRow::Item(anchor)];
    }
    for count in (1..=height.min(length)).rev() {
        let minimum = anchor.saturating_add(1).saturating_sub(count);
        let maximum = anchor.min(length - count);
        let desired = anchor.saturating_sub(count / 2).clamp(minimum, maximum);
        let candidates = [desired, minimum, maximum];
        if let Some(first) = candidates.into_iter().find(|first| {
            count + usize::from(*first > 0) + usize::from(*first + count < length) <= height
        }) {
            let mut rows = Vec::with_capacity(height);
            if first > 0 {
                rows.push(VisibleRow::Ellipsis);
            }
            rows.extend((first..first + count).map(VisibleRow::Item));
            if first + count < length {
                rows.push(VisibleRow::Ellipsis);
            }
            return rows;
        }
    }

    if anchor >= length / 2 {
        vec![VisibleRow::Ellipsis, VisibleRow::Item(anchor)]
            .into_iter()
            .take(height)
            .collect()
    } else {
        vec![VisibleRow::Item(anchor), VisibleRow::Ellipsis]
            .into_iter()
            .take(height)
            .collect()
    }
}

fn format_workspace(name: &str, current: bool, closing: bool, width: usize) -> String {
    match width {
        0 => String::new(),
        1 => {
            if current {
                "●".into()
            } else if closing {
                "×".into()
            } else {
                " ".into()
            }
        }
        _ => {
            let marker = if current { "●" } else { " " };
            let suffix = if closing && width >= 5 { " ×" } else { "" };
            let available = width.saturating_sub(2 + unicode_width::UnicodeWidthStr::width(suffix));
            format!("{marker} {}{suffix}", truncate(name, available))
        }
    }
}

fn clear(area: Rect, buffer: &mut Buffer) {
    for row in area.y..area.y.saturating_add(area.height) {
        for column in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buffer.cell_mut((column, row)) {
                cell.reset();
            }
        }
    }
}

fn active_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn muted_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::{
        domain::{PaneId, SessionId, TabId},
        resources::{
            PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
        },
    };

    fn fixture(names: &[&str], current: usize) -> (ResourceSnapshot, SelectedTarget) {
        let session_id = SessionId::new();
        let workspaces = names
            .iter()
            .enumerate()
            .map(|(index, name)| WorkspaceSnapshot {
                id: WorkspaceId::new(),
                name: (*name).into(),
                root: PathBuf::from(format!("/project/{index}")),
                closing: false,
                tabs: vec![TabSnapshot {
                    id: TabId::new(),
                    name: "shell".into(),
                    closing: false,
                    panes: vec![PaneSnapshot {
                        id: PaneId::new(),
                        terminal_id: TerminalId::new(),
                        closing: false,
                    }],
                }],
            })
            .collect::<Vec<_>>();
        let workspace = &workspaces[current];
        let tab = &workspace.tabs[0];
        let pane = tab.panes[0];
        let workspace_id = workspace.id;
        let tab_id = tab.id;
        let pane_id = pane.id;
        let terminal_id = pane.terminal_id;
        (
            ResourceSnapshot {
                revision: 1,
                sessions: vec![SessionSnapshot {
                    id: session_id,
                    name: "project".into(),
                    project: Project {
                        identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/project")),
                    },
                    closing: false,
                    workspaces,
                }],
            },
            SelectedTarget {
                session_id,
                workspace_id,
                tab_id,
                pane_id,
                terminal_id,
                child_pid: 1,
            },
        )
    }

    fn rendered(
        model: &WorkspaceModel,
        selected: Option<WorkspaceId>,
        status: Option<&WorkspaceStatus>,
        width: u16,
        height: u16,
        position: WorkspaceSidebarPosition,
    ) -> (String, Buffer) {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        render_model(model, selected, status, area, position, &mut buffer);
        let text = (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, buffer)
    }

    #[test]
    fn model_uses_fresh_terminal_ancestry_and_remembered_destinations() {
        let (mut snapshot, mut focused) = fixture(&["main", "feature"], 1);
        focused.workspace_id = snapshot.sessions[0].workspaces[0].id;
        let feature = &mut snapshot.sessions[0].workspaces[1];
        let remembered = PaneSnapshot {
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing: false,
        };
        feature.tabs.push(TabSnapshot {
            id: TabId::new(),
            name: "remembered".into(),
            closing: false,
            panes: vec![remembered],
        });
        let mut history = WorkspaceHistory::default();
        let mut remembered_target = focused.clone();
        remembered_target.workspace_id = feature.id;
        remembered_target.terminal_id = remembered.terminal_id;
        history.record(&remembered_target);

        let model = WorkspaceModel::from_snapshot(&snapshot, &focused, &history);
        assert!(!model.items[0].current);
        assert!(model.items[1].current);
        assert_eq!(model.items[1].destination, Some(remembered.terminal_id));

        snapshot.sessions[0].workspaces[1].tabs[1].panes[0].closing = true;
        let fallback = WorkspaceModel::from_snapshot(&snapshot, &focused, &history);
        assert_eq!(
            fallback.items[1].destination,
            Some(snapshot.sessions[0].workspaces[1].tabs[0].panes[0].terminal_id)
        );
    }

    #[test]
    fn selection_wraps_skips_closing_rows_and_switching_blocks_input_until_error() {
        let (mut snapshot, focused) = fixture(&["main", "retiring", "feature"], 0);
        snapshot.sessions[0].workspaces[1].closing = true;
        let history = WorkspaceHistory::default();
        let mut state = WorkspaceSidebarState::open(&snapshot, &focused, &history).unwrap();
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Stay
        );
        let feature_terminal = snapshot.sessions[0].workspaces[2].tabs[0].panes[0].terminal_id;
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Select(feature_terminal)
        );

        state.begin_switch();
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Stay
        );
        state.switch_error("busy".into());
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Select(feature_terminal)
        );

        snapshot.sessions[0].workspaces.pop();
        state.accept_resources(&snapshot, &focused, &history);
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Close
        );
    }

    #[test]
    fn passive_render_is_borderless_ordered_and_mirrors_its_divider() {
        let (mut snapshot, focused) = fixture(&["main", "bad\nname", "closing"], 0);
        snapshot.sessions[0].workspaces[2].closing = true;
        let model =
            WorkspaceModel::from_snapshot(&snapshot, &focused, &WorkspaceHistory::default());
        let (left, left_buffer) =
            rendered(&model, None, None, 24, 3, WorkspaceSidebarPosition::Left);
        assert!(left.contains("● main"));
        assert!(left.contains("bad�name"));
        assert!(left.contains("closing ×"));
        assert!(left_buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(left_buffer[(23, 0)].symbol(), "│");
        assert!(left_buffer[(23, 0)].modifier.contains(Modifier::DIM));

        let (right, right_buffer) =
            rendered(&model, None, None, 24, 3, WorkspaceSidebarPosition::Right);
        assert!(right.contains("● main"));
        assert_eq!(right_buffer[(0, 0)].symbol(), "│");
        assert_eq!(right_buffer[(1, 0)].symbol(), "●");
    }

    #[test]
    fn overflow_and_tiny_unicode_rendering_always_keep_the_anchor() {
        for length in 1..12 {
            for anchor in 0..length {
                for height in 1..8 {
                    let rows = visible_rows(length, anchor, height);
                    assert!(rows.contains(&VisibleRow::Item(anchor)));
                    assert!(rows.len() <= height);
                }
            }
        }

        let (snapshot, focused) = fixture(
            &["one", "two", "three", "👩🏽‍💻 very long workspace", "five"],
            3,
        );
        let model =
            WorkspaceModel::from_snapshot(&snapshot, &focused, &WorkspaceHistory::default());
        for width in 1..24 {
            let (text, _) = rendered(&model, None, None, width, 3, WorkspaceSidebarPosition::Left);
            assert!(text.contains('●'), "width {width}: {text:?}");
        }
    }

    #[test]
    fn active_render_marks_selection_and_exposes_compact_help() {
        let (snapshot, focused) = fixture(&["main", "feature"], 0);
        let history = WorkspaceHistory::default();
        let mut state = WorkspaceSidebarState::open(&snapshot, &focused, &history).unwrap();
        state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let selected = state.selected.unwrap();
        let (ready, buffer) = rendered(
            &state.model,
            state.selected,
            Some(&state.status),
            24,
            6,
            WorkspaceSidebarPosition::Left,
        );
        assert!(ready.contains("↑↓ enter · esc"));
        let index = state
            .model
            .items
            .iter()
            .position(|item| item.id == selected)
            .unwrap();
        assert!(
            buffer[(0, index as u16)]
                .modifier
                .contains(Modifier::REVERSED)
        );

        state.begin_switch();
        let (switching, _) = rendered(
            &state.model,
            state.selected,
            Some(&state.status),
            24,
            6,
            WorkspaceSidebarPosition::Left,
        );
        assert!(switching.contains("switching…"));

        state.switch_error("destination busy".into());
        let (tiny_error, _) = rendered(
            &state.model,
            state.selected,
            Some(&state.status),
            24,
            1,
            WorkspaceSidebarPosition::Left,
        );
        assert!(tiny_error.contains("destination busy"));
    }
}
