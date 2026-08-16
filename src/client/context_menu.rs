use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};

use crate::{
    domain::{PaneId, TabId, WorkspaceId},
    protocol::{RenameSelector, SelectedTarget},
    resources::{ResourceSnapshot, TargetSelector},
};

use super::{
    config::{SemanticStyle, StylesConfig, WorkspaceSidebarDisplay, WorkspaceSidebarVisibility},
    dialog::{frame_inner, render_frame},
    navigation::NavigationHistory,
};

const MAX_WIDTH: u16 = 30;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ContextMenuAction {
    Stay,
    Dismiss,
    SwitchTab(PaneId),
    SwitchWorkspace(PaneId),
    CreateTab(WorkspaceId),
    CreateWorkspace(crate::domain::SessionId),
    Rename(RenameSelector, String),
    Close(TargetSelector, &'static str),
    SetDisplay(WorkspaceSidebarDisplay),
    SetVisibility(WorkspaceSidebarVisibility),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MenuRow {
    Action {
        label: &'static str,
        checked: bool,
        action: ContextMenuAction,
    },
    Heading(&'static str),
    Separator,
}

impl MenuRow {
    fn selectable(&self) -> bool {
        matches!(self, Self::Action { .. })
    }

    fn width(&self) -> usize {
        match self {
            Self::Action { label, .. } => label.len() + 4,
            Self::Heading(label) => label.len() + 2,
            Self::Separator => 1,
        }
    }
}

pub(super) struct ContextMenuState {
    anchor: (u16, u16),
    rows: Vec<MenuRow>,
    selected: usize,
}

impl ContextMenuState {
    pub fn for_tab(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
        tab_id: TabId,
        anchor: (u16, u16),
    ) -> Option<Self> {
        let (workspace, tab) = snapshot.sessions.iter().find_map(|session| {
            session.workspaces.iter().find_map(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .map(|tab| (workspace, tab))
            })
        })?;
        if workspace.closing || tab.closing {
            return None;
        }
        let mut rows = Vec::new();
        if tab.id != focused.tab_id
            && let Some(destination) = history.tab_destination(tab)
        {
            rows.push(action(
                "Switch to Tab",
                ContextMenuAction::SwitchTab(destination),
            ));
            rows.push(MenuRow::Separator);
        }
        rows.extend([
            action("New Tab", ContextMenuAction::CreateTab(workspace.id)),
            action(
                "Rename",
                ContextMenuAction::Rename(RenameSelector::Tab(tab.id), tab.name.clone()),
            ),
            action(
                "Close",
                ContextMenuAction::Close(TargetSelector::Tab(tab.id), "tab"),
            ),
        ]);
        Self::new(anchor, rows)
    }

    pub fn for_workspace(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
        workspace_id: WorkspaceId,
        display: WorkspaceSidebarDisplay,
        visibility: WorkspaceSidebarVisibility,
        anchor: (u16, u16),
    ) -> Option<Self> {
        let (session, workspace) = snapshot.sessions.iter().find_map(|session| {
            session
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(|workspace| (session, workspace))
        })?;
        if session.closing || workspace.closing {
            return None;
        }
        let mut rows = Vec::new();
        if workspace.id != focused.workspace_id
            && let Some(destination) = history.workspace_destination(workspace)
        {
            rows.push(action(
                "Switch to Workspace",
                ContextMenuAction::SwitchWorkspace(destination),
            ));
            rows.push(MenuRow::Separator);
        }
        rows.extend([
            action(
                "New Workspace",
                ContextMenuAction::CreateWorkspace(session.id),
            ),
            action(
                "Rename",
                ContextMenuAction::Rename(
                    RenameSelector::Workspace(workspace.id),
                    workspace.name.clone(),
                ),
            ),
            action(
                "Close",
                ContextMenuAction::Close(TargetSelector::Workspace(workspace.id), "workspace"),
            ),
            MenuRow::Separator,
            MenuRow::Heading("Display"),
            checked_action(
                "Expanded",
                display == WorkspaceSidebarDisplay::Expanded,
                ContextMenuAction::SetDisplay(WorkspaceSidebarDisplay::Expanded),
            ),
            checked_action(
                "Minimized",
                display == WorkspaceSidebarDisplay::Minimized,
                ContextMenuAction::SetDisplay(WorkspaceSidebarDisplay::Minimized),
            ),
            MenuRow::Heading("Visibility"),
            checked_action(
                "Visible",
                visibility == WorkspaceSidebarVisibility::Visible,
                ContextMenuAction::SetVisibility(WorkspaceSidebarVisibility::Visible),
            ),
            checked_action(
                "Auto-hide",
                visibility == WorkspaceSidebarVisibility::AutoHideWhenSingle,
                ContextMenuAction::SetVisibility(WorkspaceSidebarVisibility::AutoHideWhenSingle),
            ),
            checked_action(
                "Hidden",
                visibility == WorkspaceSidebarVisibility::Hidden,
                ContextMenuAction::SetVisibility(WorkspaceSidebarVisibility::Hidden),
            ),
        ]);
        Self::new(anchor, rows)
    }

    fn new(anchor: (u16, u16), rows: Vec<MenuRow>) -> Option<Self> {
        let selected = rows.iter().position(MenuRow::selectable)?;
        Some(Self {
            anchor,
            rows,
            selected,
        })
    }

    pub fn key(&mut self, key: KeyEvent) -> ContextMenuAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ContextMenuAction::Stay;
        }
        match key.code {
            KeyCode::Esc => ContextMenuAction::Dismiss,
            KeyCode::Up => {
                self.move_selection(false);
                ContextMenuAction::Stay
            }
            KeyCode::Down => {
                self.move_selection(true);
                ContextMenuAction::Stay
            }
            KeyCode::Enter => self.activate_selected(),
            _ => ContextMenuAction::Stay,
        }
    }

    pub fn mouse_move(&mut self, host: Rect, column: u16, row: u16) {
        if let Some(index) = self.row_at(host, column, row)
            && self.rows[index].selectable()
        {
            self.selected = index;
        }
    }

    pub fn click(&mut self, host: Rect, column: u16, row: u16) -> ContextMenuAction {
        let Some(index) = self.row_at(host, column, row) else {
            return ContextMenuAction::Dismiss;
        };
        if self.rows[index].selectable() {
            self.selected = index;
            self.activate_selected()
        } else {
            ContextMenuAction::Stay
        }
    }

    pub fn area(&self, host: Rect) -> Rect {
        let desired_width = self
            .rows
            .iter()
            .map(MenuRow::width)
            .max()
            .unwrap_or(1)
            .saturating_add(2)
            .min(usize::from(MAX_WIDTH));
        let width = host
            .width
            .min(u16::try_from(desired_width).unwrap_or(MAX_WIDTH));
        let desired_height = u16::try_from(self.rows.len().saturating_add(2)).unwrap_or(u16::MAX);
        let height = host.height.min(desired_height);
        let x = place_axis(host.x, host.width, self.anchor.0, width);
        let y = place_axis(host.y, host.height, self.anchor.1, height);
        Rect::new(x, y, width, height)
    }

    pub fn render(&self, host: Rect, styles: &StylesConfig, buffer: &mut Buffer) {
        let area = self.area(host);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let inner = render_frame(area, buffer);
        let (start, end) = self.visible_rows(inner.height);
        for (offset, index) in (start..end).enumerate() {
            let row = Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
                inner.width,
                1,
            );
            let base = match &self.rows[index] {
                MenuRow::Heading(_) | MenuRow::Separator => {
                    styles.apply(SemanticStyle::Muted, Style::default())
                }
                MenuRow::Action { .. } if index == self.selected => {
                    styles.apply(SemanticStyle::Selected, Style::default())
                }
                MenuRow::Action { .. } => styles.apply(SemanticStyle::Normal, Style::default()),
            };
            for column in row.x..row.right() {
                if let Some(cell) = buffer.cell_mut((column, row.y)) {
                    cell.set_symbol(" ").set_style(base);
                }
            }
            let text = match &self.rows[index] {
                MenuRow::Action { label, checked, .. } => {
                    format!("{} {label}", if *checked { "✓" } else { " " })
                }
                MenuRow::Heading(label) => format!(" {label}"),
                MenuRow::Separator => "─".repeat(usize::from(inner.width)),
            };
            buffer.set_stringn(row.x, row.y, text, usize::from(row.width), base);
        }
    }

    fn activate_selected(&self) -> ContextMenuAction {
        match &self.rows[self.selected] {
            MenuRow::Action { action, .. } => action.clone(),
            MenuRow::Heading(_) | MenuRow::Separator => ContextMenuAction::Stay,
        }
    }

    fn move_selection(&mut self, forward: bool) {
        let selectable = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.selectable().then_some(index))
            .collect::<Vec<_>>();
        let current = selectable
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        self.selected = if forward {
            selectable[(current + 1) % selectable.len()]
        } else {
            selectable[(current + selectable.len() - 1) % selectable.len()]
        };
    }

    fn visible_rows(&self, height: u16) -> (usize, usize) {
        let capacity = usize::from(height).min(self.rows.len());
        let start = self
            .selected
            .saturating_sub(capacity.saturating_sub(1))
            .min(self.rows.len().saturating_sub(capacity));
        (start, start + capacity)
    }

    fn row_at(&self, host: Rect, column: u16, row: u16) -> Option<usize> {
        let inner = frame_inner(self.area(host));
        if column < inner.x || column >= inner.right() || row < inner.y || row >= inner.bottom() {
            return None;
        }
        let (start, end) = self.visible_rows(inner.height);
        let index = start + usize::from(row - inner.y);
        (index < end).then_some(index)
    }
}

fn action(label: &'static str, action: ContextMenuAction) -> MenuRow {
    checked_action(label, false, action)
}

fn checked_action(label: &'static str, checked: bool, action: ContextMenuAction) -> MenuRow {
    MenuRow::Action {
        label,
        checked,
        action,
    }
}

fn place_axis(origin: u16, available: u16, anchor: u16, size: u16) -> u16 {
    let end = origin.saturating_add(available);
    let max_start = end.saturating_sub(size).max(origin);
    let candidate = if anchor.saturating_add(size) <= end {
        anchor.max(origin)
    } else {
        anchor.saturating_add(1).saturating_sub(size).max(origin)
    };
    candidate.min(max_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(anchor: (u16, u16)) -> ContextMenuState {
        ContextMenuState::new(
            anchor,
            vec![
                action("First", ContextMenuAction::Dismiss),
                MenuRow::Separator,
                action("Second", ContextMenuAction::Stay),
            ],
        )
        .unwrap()
    }

    #[test]
    fn placement_flips_and_clamps_to_the_host() {
        let state = menu((19, 9));
        let area = state.area(Rect::new(0, 0, 20, 10));
        assert_eq!(area.right(), 20);
        assert_eq!(area.bottom(), 10);

        let tiny = state.area(Rect::new(4, 3, 2, 1));
        assert_eq!(tiny, Rect::new(4, 3, 2, 1));
    }

    #[test]
    fn keyboard_skips_structural_rows_and_activates() {
        let mut state = menu((0, 0));
        state.key(KeyEvent::new(
            KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(state.selected, 2);
        assert_eq!(
            state.key(KeyEvent::new(
                KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE
            )),
            ContextMenuAction::Stay
        );
        assert_eq!(
            state.key(KeyEvent::new(
                KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE
            )),
            ContextMenuAction::Dismiss
        );
    }

    #[test]
    fn outside_click_dismisses_and_hover_selects() {
        let host = Rect::new(0, 0, 40, 20);
        let mut state = menu((5, 5));
        let inner = frame_inner(state.area(host));
        state.mouse_move(host, inner.x, inner.y + 2);
        assert_eq!(state.selected, 2);
        assert_eq!(state.click(host, 0, 0), ContextMenuAction::Dismiss);
    }

    #[test]
    fn rendering_shows_checks_and_keeps_tiny_clients_safe() {
        let state = ContextMenuState::new(
            (2, 2),
            vec![checked_action(
                "Visible",
                true,
                ContextMenuAction::SetVisibility(WorkspaceSidebarVisibility::Visible),
            )],
        )
        .unwrap();
        let host = Rect::new(0, 0, 20, 8);
        let mut buffer = Buffer::empty(host);
        state.render(
            host,
            &crate::client::config::UiConfig::default().styles,
            &mut buffer,
        );
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("✓ Visible"));

        let tiny = Rect::new(0, 0, 1, 1);
        state.render(
            tiny,
            &crate::client::config::UiConfig::default().styles,
            &mut Buffer::empty(tiny),
        );
    }
}
