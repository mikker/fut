use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};

use crate::{
    domain::{PaneId, WorkspaceId},
    protocol::SelectedTarget,
    resources::ResourceSnapshot,
};

use super::{
    chrome::sanitize,
    config::{SemanticStyle, UiConfig, WorkspaceSidebarPosition},
    navigation::NavigationHistory,
    notifications::{ActivityIndicator, NotificationState},
    presentation::{ItemState, TokenValue, apply_item_state, render_token_segments, truncate_line},
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceItem {
    id: WorkspaceId,
    name: String,
    root: std::path::PathBuf,
    index: usize,
    tab_count: usize,
    current: bool,
    closing: bool,
    destination: Option<PaneId>,
    activity: Option<ActivityIndicator>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct WorkspaceModel {
    session_name: String,
    items: Vec<WorkspaceItem>,
}

impl WorkspaceModel {
    fn from_snapshot(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
        notifications: &NotificationState,
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
            session_name: sanitize(&session.name),
            items: session
                .workspaces
                .iter()
                .enumerate()
                .map(|(index, workspace)| {
                    let closing = session.closing || workspace.closing;
                    WorkspaceItem {
                        id: workspace.id,
                        name: workspace.name.clone(),
                        root: workspace.root.clone(),
                        index,
                        tab_count: workspace.tabs.len(),
                        current: workspace.id == workspace_id,
                        closing,
                        destination: (!closing)
                            .then(|| history.workspace_destination(workspace))
                            .flatten(),
                        activity: notifications.indicator(
                            &workspace
                                .tabs
                                .iter()
                                .flat_map(|tab| &tab.panes)
                                .copied()
                                .collect::<Vec<_>>(),
                        ),
                    }
                })
                .collect(),
        }
    }
}

fn switch_to(item: &WorkspaceItem) -> WorkspaceSidebarAction {
    if item.current {
        WorkspaceSidebarAction::Close
    } else if let Some(destination) = item.destination {
        WorkspaceSidebarAction::Select(destination)
    } else {
        WorkspaceSidebarAction::Stay
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
                        .any(|pane| pane.id == focused.pane_id)
                        .then_some((session.id, workspace.id))
                })
            })
        })
        .unwrap_or((focused.session_id, focused.workspace_id))
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
    Create,
    ToggleAutoHide,
    Rename(WorkspaceId, String),
    Select(PaneId),
}

impl WorkspaceSidebarState {
    pub fn open(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
        notifications: &NotificationState,
    ) -> Option<Self> {
        let model = WorkspaceModel::from_snapshot(snapshot, focused, history, notifications);
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
        history: &NavigationHistory,
        notifications: &NotificationState,
    ) {
        let previous = self.selected;
        let previous_current = self
            .model
            .items
            .iter()
            .find(|item| item.current)
            .map(|item| item.id);
        self.model = WorkspaceModel::from_snapshot(snapshot, focused, history, notifications);
        let current = self
            .model
            .items
            .iter()
            .find(|item| item.current)
            .map(|item| item.id);
        self.selected = current
            .filter(|current| Some(*current) != previous_current)
            .or_else(|| previous.filter(|id| self.selectable(*id)))
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
            KeyCode::Char('c') if key.modifiers == KeyModifiers::NONE => {
                WorkspaceSidebarAction::Create
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self
                .selected_item()
                .map(|item| WorkspaceSidebarAction::Rename(item.id, item.name.clone()))
                .unwrap_or(WorkspaceSidebarAction::Stay),
            KeyCode::Char('h') if key.modifiers == KeyModifiers::NONE => {
                WorkspaceSidebarAction::ToggleAutoHide
            }
            KeyCode::Char(digit)
                if digit.is_ascii_digit() && key.modifiers == KeyModifiers::NONE =>
            {
                let index = if digit == '0' {
                    9
                } else {
                    usize::from(digit as u8 - b'1')
                };
                self.model
                    .items
                    .get(index)
                    .map(switch_to)
                    .unwrap_or(WorkspaceSidebarAction::Stay)
            }
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
            KeyCode::Enter => self
                .selected
                .and_then(|id| self.model.items.iter().find(|item| item.id == id))
                .map(switch_to)
                .unwrap_or(WorkspaceSidebarAction::Stay),
            _ => WorkspaceSidebarAction::Stay,
        }
    }

    pub fn begin_switch(&mut self) {
        self.status = WorkspaceStatus::Switching;
    }

    pub fn switch_error(&mut self, message: String) {
        self.status = WorkspaceStatus::Error(sanitize(&message));
    }

    pub fn render(
        &self,
        area: Rect,
        position: WorkspaceSidebarPosition,
        ui: &UiConfig,
        spinner_frame: usize,
        buffer: &mut Buffer,
    ) {
        render_model(
            &self.model,
            self.selected,
            Some(&self.status),
            spinner_frame,
            area,
            position,
            ui,
            buffer,
        );
    }

    fn selectable(&self, id: WorkspaceId) -> bool {
        self.model
            .items
            .iter()
            .any(|item| item.id == id && item.destination.is_some())
    }

    fn selected_item(&self) -> Option<&WorkspaceItem> {
        self.selected
            .and_then(|id| self.model.items.iter().find(|item| item.id == id))
            .filter(|item| !item.closing)
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

#[allow(
    clippy::too_many_arguments,
    reason = "the renderer keeps resource, client, configuration, and target inputs explicit"
)]
pub(super) fn render_workspace_sidebar(
    snapshot: Option<&ResourceSnapshot>,
    focused: &SelectedTarget,
    history: &NavigationHistory,
    notifications: &NotificationState,
    spinner_frame: usize,
    area: Rect,
    position: WorkspaceSidebarPosition,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    let model = snapshot
        .map(|snapshot| WorkspaceModel::from_snapshot(snapshot, focused, history, notifications))
        .unwrap_or_default();
    render_model(
        &model,
        None,
        None,
        spinner_frame,
        area,
        position,
        ui,
        buffer,
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "passive and interactive sidebar rendering share one explicit model renderer"
)]
fn render_model(
    model: &WorkspaceModel,
    selected: Option<WorkspaceId>,
    status: Option<&WorkspaceStatus>,
    spinner_frame: usize,
    area: Rect,
    position: WorkspaceSidebarPosition,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    clear(
        area,
        ui.styles.apply(SemanticStyle::Normal, Style::default()),
        buffer,
    );
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
        let icons = ui.icons.resolve();
        for row in area.y..area.y.saturating_add(area.height) {
            if let Some(cell) = buffer.cell_mut((divider_x, row)) {
                let style = ui.styles.apply(SemanticStyle::Normal, Style::default());
                cell.set_symbol(&icons.vertical_divider)
                    .set_style(ui.styles.apply(SemanticStyle::Divider, style));
            }
        }
    }
    if content.width == 0 {
        return;
    }

    let header_line = render_sidebar_chrome(&ui.workspace_sidebar.header, model, status, ui);
    let footer_line = render_sidebar_chrome(&ui.workspace_sidebar.footer, model, status, ui);
    let header = (!header_line.spans.is_empty() && content.height >= 3).then_some(header_line);
    let footer_allowed = status
        .is_none_or(|status| content.height >= 5 || !matches!(status, WorkspaceStatus::Ready));
    let urgent_footer = matches!(
        status,
        Some(WorkspaceStatus::Switching | WorkspaceStatus::Error(_))
    );
    let footer =
        (!footer_line.spans.is_empty() && footer_allowed && (content.height >= 2 || urgent_footer))
            .then_some(footer_line);
    let row_y = content.y.saturating_add(u16::from(header.is_some()));
    let row_height = content
        .height
        .saturating_sub(u16::from(header.is_some()) + u16::from(footer.is_some()));
    if let Some(header) = header.as_ref() {
        buffer.set_line(
            content.x,
            content.y,
            &truncate_line(header, usize::from(content.width)),
            content.width,
        );
    }
    if model.items.is_empty() {
        let item = WorkspaceItem {
            id: WorkspaceId::new(),
            name: "workspace".into(),
            root: std::path::PathBuf::new(),
            index: 0,
            tab_count: 0,
            current: true,
            closing: false,
            destination: None,
            activity: None,
        };
        render_workspace_row(
            &item,
            false,
            spinner_frame,
            Rect::new(content.x, row_y, content.width, row_height.min(2)),
            ui,
            buffer,
        );
    } else {
        let item_height = if ui.workspace_sidebar.row.detail.is_empty() {
            1
        } else {
            2
        };
        let anchor = selected
            .and_then(|id| model.items.iter().position(|item| item.id == id))
            .or_else(|| model.items.iter().position(|item| item.current))
            .unwrap_or(0);
        let mut y = row_y;
        let row_bottom = row_y.saturating_add(row_height);
        for row in visible_rows_with_item_height(
            model.items.len(),
            anchor,
            usize::from(row_height),
            item_height,
        ) {
            match row {
                VisibleRow::Ellipsis => {
                    buffer.set_stringn(
                        content.x,
                        y,
                        format!(" {}", ui.icons.resolve().overflow),
                        usize::from(content.width),
                        ui.styles.apply(
                            SemanticStyle::Muted,
                            ui.styles.apply(SemanticStyle::Normal, Style::default()),
                        ),
                    );
                    y = y.saturating_add(1);
                }
                VisibleRow::Item(index) => {
                    let item = &model.items[index];
                    render_workspace_row(
                        item,
                        selected == Some(item.id),
                        spinner_frame,
                        Rect::new(
                            content.x,
                            y,
                            content.width,
                            u16::try_from(item_height)
                                .unwrap_or(u16::MAX)
                                .min(row_bottom.saturating_sub(y)),
                        ),
                        ui,
                        buffer,
                    );
                    y = y.saturating_add(u16::try_from(item_height).unwrap_or(u16::MAX));
                }
            }
        }
    }

    if let Some(footer) = footer.as_ref() {
        buffer.set_line(
            content.x,
            content.y.saturating_add(content.height - 1),
            &truncate_line(footer, usize::from(content.width)),
            content.width,
        );
    }
}

fn render_sidebar_chrome(
    segments: &[super::config::SegmentConfig],
    model: &WorkspaceModel,
    status: Option<&WorkspaceStatus>,
    ui: &UiConfig,
) -> ratatui::text::Line<'static> {
    let current = model.items.iter().find(|item| item.current);
    let icons = ui.icons.resolve();
    render_token_segments(
        segments,
        None,
        ItemState::default(),
        &ui.styles,
        |token| match token {
            "fut" => TokenValue::plain("fut"),
            "session.name" => TokenValue::plain(model.session_name.clone()),
            "workspace.name" => {
                TokenValue::plain(sanitize(current.map_or("", |item| item.name.as_str())))
            }
            "workspace.icon" => TokenValue::plain(icons.workspace.clone()),
            "sidebar.status" => match status {
                Some(WorkspaceStatus::Ready) => {
                    TokenValue::plain(" ↑↓ ↵ c new · r rename · h hide · 1-9 pick")
                }
                Some(WorkspaceStatus::Switching) => TokenValue::plain(" switching…"),
                Some(WorkspaceStatus::Error(message)) => {
                    TokenValue::styled(format!(" {message} · retry"), SemanticStyle::Error)
                }
                None => TokenValue::plain(""),
            },
            _ => TokenValue::plain(""),
        },
    )
}

fn render_workspace_row(
    item: &WorkspaceItem,
    selected: bool,
    spinner_frame: usize,
    area: Rect,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    if area.width == 0 {
        return;
    }
    let icons = ui.icons.resolve();
    let state = ItemState {
        // The workspace marker carries active state so keyboard selection can own
        // the row background without making two rows look selected at once.
        current: false,
        selected,
        closing: item.closing,
        attention: matches!(
            item.activity,
            Some(ActivityIndicator::Blocked | ActivityIndicator::Completed)
        ),
    };
    clear(
        area,
        apply_item_state(
            &ui.styles,
            state,
            ui.styles.apply(SemanticStyle::Normal, Style::default()),
        ),
        buffer,
    );
    let resolve = |token: &str| match token {
        "workspace.marker" if item.current => TokenValue::plain(icons.current.clone()),
        "workspace.marker" => TokenValue::plain(" "),
        "workspace.index" => TokenValue::plain((item.index + 1).to_string()),
        "workspace.name" => TokenValue::plain(sanitize(&item.name)),
        "workspace.id" => TokenValue::plain(item.id.to_string()),
        "workspace.root" => TokenValue::plain(sanitize(&item.root.to_string_lossy())),
        "workspace.root_name" => TokenValue::plain(sanitize(
            &item
                .root
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        )),
        "workspace.closing" if item.closing => {
            TokenValue::styled(icons.closing.clone(), SemanticStyle::Closing)
        }
        "workspace.tab_count" => TokenValue::plain(item.tab_count.to_string()),
        "workspace.icon" => TokenValue::plain(icons.workspace.clone()),
        "workspace.activity" => item.activity.map_or_else(
            || TokenValue::plain(""),
            |activity| {
                let style = match activity {
                    ActivityIndicator::Working => SemanticStyle::Activity,
                    ActivityIndicator::Blocked | ActivityIndicator::Completed => {
                        SemanticStyle::Attention
                    }
                };
                TokenValue::styled(activity.marker(spinner_frame), style)
            },
        ),
        _ => TokenValue::plain(""),
    };
    let left = render_token_segments(
        &ui.workspace_sidebar.row.left,
        None,
        state,
        &ui.styles,
        resolve,
    );
    let body = render_token_segments(
        &ui.workspace_sidebar.row.body,
        None,
        state,
        &ui.styles,
        resolve,
    );
    let right = render_token_segments(
        &ui.workspace_sidebar.row.right,
        None,
        state,
        &ui.styles,
        resolve,
    );
    let width = usize::from(area.width);
    let left_width = left.width().min(width);
    let right_width = right.width().min(width.saturating_sub(left_width));
    let body_width = width.saturating_sub(left_width + right_width);
    buffer.set_line(
        area.x,
        area.y,
        &truncate_line(&left, left_width),
        left_width as u16,
    );
    buffer.set_line(
        area.x.saturating_add(left_width as u16),
        area.y,
        &truncate_line(&body, body_width),
        body_width as u16,
    );
    buffer.set_line(
        area.x.saturating_add((width - right_width) as u16),
        area.y,
        &truncate_line(&right, right_width),
        right_width as u16,
    );
    if area.height > 1 && !ui.workspace_sidebar.row.detail.is_empty() {
        let detail = render_token_segments(
            &ui.workspace_sidebar.row.detail,
            None,
            state,
            &ui.styles,
            resolve,
        );
        buffer.set_line(
            area.x,
            area.y.saturating_add(1),
            &truncate_line(&detail, width),
            area.width,
        );
    }
    if item.current {
        for row in area.y..area.y.saturating_add(area.height) {
            for column in area.x..area.x.saturating_add(area.width) {
                if let Some(cell) = buffer.cell_mut((column, row)) {
                    cell.set_style(Style::default().add_modifier(ratatui::style::Modifier::BOLD));
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisibleRow {
    Ellipsis,
    Item(usize),
}

fn visible_rows_with_item_height(
    length: usize,
    anchor: usize,
    height: usize,
    item_height: usize,
) -> Vec<VisibleRow> {
    if length == 0 || height == 0 {
        return Vec::new();
    }
    let item_height = item_height.max(1);
    let anchor = anchor.min(length - 1);
    if length.saturating_mul(item_height) <= height {
        return (0..length).map(VisibleRow::Item).collect();
    }
    if height <= item_height {
        return vec![VisibleRow::Item(anchor)];
    }
    for count in (1..=(height / item_height).min(length)).rev() {
        let minimum = anchor.saturating_add(1).saturating_sub(count);
        let maximum = anchor.min(length - count);
        let desired = anchor.saturating_sub(count / 2).clamp(minimum, maximum);
        let candidates = [desired, minimum, maximum];
        if let Some(first) = candidates.into_iter().find(|first| {
            count * item_height + usize::from(*first > 0) + usize::from(*first + count < length)
                <= height
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

fn clear(area: Rect, style: Style, buffer: &mut Buffer) {
    for row in area.y..area.y.saturating_add(area.height) {
        for column in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buffer.cell_mut((column, row)) {
                cell.reset();
                cell.set_style(style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::KeyModifiers;
    use ratatui::style::Modifier;

    use super::*;
    use crate::{
        domain::{PaneId, SessionId, TabId, TerminalId},
        resources::{
            PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
        },
    };

    fn fixture(names: &[&str], current: usize) -> (ResourceSnapshot, SelectedTarget) {
        let session_id = SessionId::new();
        let workspaces = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let pane_id = PaneId::new();
                WorkspaceSnapshot {
                    id: WorkspaceId::new(),
                    name: (*name).into(),
                    root: PathBuf::from(format!("/project/{index}")),
                    closing: false,
                    tabs: vec![TabSnapshot {
                        id: TabId::new(),
                        name: "shell".into(),
                        closing: false,
                        layout: crate::splits::SplitTree::leaf(pane_id),
                        panes: vec![PaneSnapshot {
                            id: pane_id,
                            terminal_id: TerminalId::new(),
                            closing: false,
                            activity: Default::default(),
                        }],
                    }],
                }
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
        render_model(
            model,
            selected,
            status,
            0,
            area,
            position,
            &UiConfig::default(),
            &mut buffer,
        );
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
            activity: Default::default(),
        };
        feature.tabs.push(TabSnapshot {
            id: TabId::new(),
            name: "remembered".into(),
            closing: false,
            layout: crate::splits::SplitTree::leaf(remembered.id),
            panes: vec![remembered],
        });
        let mut history = NavigationHistory::default();
        let mut remembered_target = focused.clone();
        remembered_target.workspace_id = feature.id;
        remembered_target.tab_id = feature.tabs[1].id;
        remembered_target.pane_id = remembered.id;
        remembered_target.terminal_id = remembered.terminal_id;
        history.record(&remembered_target);

        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &history,
            &NotificationState::default(),
        );
        assert!(!model.items[0].current);
        assert!(model.items[1].current);
        assert_eq!(model.items[1].destination, Some(remembered.id));

        snapshot.sessions[0].workspaces[1].tabs[1].panes[0].closing = true;
        let fallback = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &history,
            &NotificationState::default(),
        );
        assert_eq!(
            fallback.items[1].destination,
            Some(snapshot.sessions[0].workspaces[1].tabs[0].panes[0].id)
        );
    }

    #[test]
    fn selection_wraps_skips_closing_rows_and_switching_blocks_input_until_error() {
        let (mut snapshot, focused) = fixture(&["main", "retiring", "feature"], 0);
        snapshot.sessions[0].workspaces[1].closing = true;
        let history = NavigationHistory::default();
        let mut state = WorkspaceSidebarState::open(
            &snapshot,
            &focused,
            &history,
            &NotificationState::default(),
        )
        .unwrap();
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::Create
        );
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::Rename(_, ref name) if name == "main"
        ));
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::ToggleAutoHide
        );
        let feature_pane = snapshot.sessions[0].workspaces[2].tabs[0].panes[0].id;
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::Select(feature_pane)
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::Stay,
            "a closing workspace has no destination"
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::Close,
            "the current workspace closes the sidebar"
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::Stay
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Stay
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Select(feature_pane)
        );

        state.begin_switch();
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Stay
        );
        state.switch_error("busy".into());
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Select(feature_pane)
        );

        snapshot.sessions[0].workspaces.pop();
        state.accept_resources(&snapshot, &focused, &history, &NotificationState::default());
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            WorkspaceSidebarAction::Close
        );
    }

    #[test]
    fn passive_render_is_borderless_ordered_and_mirrors_its_divider() {
        let (mut snapshot, focused) = fixture(&["main", "bad\nname", "closing"], 0);
        snapshot.sessions[0].workspaces[2].closing = true;
        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        let (left, left_buffer) =
            rendered(&model, None, None, 24, 6, WorkspaceSidebarPosition::Left);
        assert!(left.contains("main"));
        assert!(left.contains("/project/0"));
        assert!(left.contains("bad�name"));
        assert!(left.contains("closing"));
        assert!(left.contains('×'));
        assert_eq!(left_buffer[(0, 0)].symbol(), "●");
        assert!(!left_buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert!(left_buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(left_buffer[(23, 0)].symbol(), "│");
        assert_eq!(left_buffer[(23, 0)].fg, ratatui::style::Color::DarkGray);

        let (right, right_buffer) =
            rendered(&model, None, None, 24, 6, WorkspaceSidebarPosition::Right);
        assert!(right.contains("main"));
        assert_eq!(right_buffer[(0, 0)].symbol(), "│");
        assert_eq!(right_buffer[(1, 0)].symbol(), "●");
    }

    #[test]
    fn overflow_and_tiny_unicode_rendering_always_keep_the_anchor() {
        for length in 1..12 {
            for anchor in 0..length {
                for height in 1..8 {
                    let rows = visible_rows_with_item_height(length, anchor, height, 1);
                    assert!(rows.contains(&VisibleRow::Item(anchor)));
                    assert!(rows.len() <= height);
                }
            }
        }

        let (snapshot, focused) = fixture(
            &["one", "two", "three", "👩🏽‍💻 very long workspace", "five"],
            3,
        );
        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        for width in 1..24 {
            let (text, _) = rendered(&model, None, None, width, 3, WorkspaceSidebarPosition::Left);
            assert!(text.contains('●'), "width {width}: {text:?}");
        }
    }

    #[test]
    fn active_render_marks_selection_and_exposes_compact_help() {
        let (snapshot, focused) = fixture(&["main", "feature"], 0);
        let history = NavigationHistory::default();
        let mut state = WorkspaceSidebarState::open(
            &snapshot,
            &focused,
            &history,
            &NotificationState::default(),
        )
        .unwrap();
        state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let (ready, buffer) = rendered(
            &state.model,
            state.selected,
            Some(&state.status),
            24,
            6,
            WorkspaceSidebarPosition::Left,
        );
        assert!(ready.contains("c new · r rename"));
        assert_eq!(buffer[(0, 2)].bg, ratatui::style::Color::DarkGray);
        assert!(buffer[(0, 2)].modifier.contains(Modifier::UNDERLINED));

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

    #[test]
    fn passive_static_footer_never_replaces_the_only_workspace_row() {
        let (snapshot, focused) = fixture(&["main"], 0);
        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        let ui: UiConfig =
            toml::from_str("[workspace_sidebar]\nfooter = [{ text = 'FOOTER' }]\n").unwrap();
        let area = Rect::new(0, 0, 24, 1);
        let mut buffer = Buffer::empty(area);
        render_model(
            &model,
            None,
            None,
            0,
            area,
            WorkspaceSidebarPosition::Left,
            &ui,
            &mut buffer,
        );
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(text.contains("main"));
        assert!(!text.contains("FOOTER"));
    }
}
