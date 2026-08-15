use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
};

use crate::{
    domain::{PaneId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{MaterializedTokenMap, ResourceSnapshot},
};

use super::{
    chrome::{sanitize, truncate},
    config::{
        IconPreset, SemanticStyle, UiConfig, WorkspaceSidebarDisplay, WorkspaceSidebarPosition,
        WorkspaceSidebarVisibility,
    },
    hotkey::{HotkeyButton, HotkeyLine},
    navigation::NavigationHistory,
    notifications::{ActivityIndicator, NotificationState},
    presentation::{ItemState, TokenValue, apply_item_state, render_token_segments, truncate_line},
};

/// The current workspace is marked with a bullet instead of an icon so the
/// sidebar reads the same at every icon preset.
const CURRENT_MARKER: &str = "•";
const SIDEBAR_HEADER_HEIGHT: u16 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceItem {
    id: WorkspaceId,
    name: String,
    /// The live location every open pane shares; `None` when panes disagree.
    location: Option<std::path::PathBuf>,
    index: usize,
    tab_count: usize,
    current: bool,
    closing: bool,
    tokens: MaterializedTokenMap,
    destination: Option<PaneId>,
    activity: Option<ActivityIndicator>,
}

impl WorkspaceItem {
    fn token_value(&self, token: &str) -> &str {
        self.tokens.get(token).map_or("", String::as_str)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct WorkspaceModel {
    session_name: String,
    session_tokens: MaterializedTokenMap,
    tab_tokens: MaterializedTokenMap,
    pane_tokens: MaterializedTokenMap,
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

        let focused_workspace = session
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id);
        let focused_tab = focused_workspace
            .and_then(|workspace| workspace.tabs.iter().find(|tab| tab.id == focused.tab_id));
        Self {
            session_name: sanitize(&session.name),
            session_tokens: session.tokens.clone(),
            tab_tokens: focused_tab
                .map_or_else(MaterializedTokenMap::new, |tab| tab.tokens.clone()),
            pane_tokens: focused_tab
                .and_then(|tab| tab.panes.iter().find(|pane| pane.id == focused.pane_id))
                .map_or_else(MaterializedTokenMap::new, |pane| pane.tokens.clone()),
            items: session
                .workspaces
                .iter()
                .enumerate()
                .map(|(index, workspace)| {
                    let closing = session.closing || workspace.closing;
                    WorkspaceItem {
                        id: workspace.id,
                        name: workspace.name.clone(),
                        location: crate::resources::shared_live_location(
                            &workspace.root,
                            &workspace.tabs,
                        )
                        .map(std::path::Path::to_path_buf),
                        index,
                        tab_count: workspace.tabs.len(),
                        current: workspace.id == workspace_id,
                        closing,
                        tokens: workspace.tokens.clone(),
                        destination: (!closing)
                            .then(|| history.workspace_destination(workspace))
                            .flatten(),
                        activity: notifications.indicator(
                            &workspace
                                .tabs
                                .iter()
                                .flat_map(|tab| &tab.panes)
                                .cloned()
                                .collect::<Vec<_>>(),
                        ),
                    }
                })
                .collect(),
        }
    }

    fn extension_value(&self, token: &str) -> &str {
        self.session_tokens
            .get(token)
            .or_else(|| {
                self.items
                    .iter()
                    .find(|item| item.current)
                    .and_then(|item| item.tokens.get(token))
            })
            .or_else(|| self.tab_tokens.get(token))
            .or_else(|| self.pane_tokens.get(token))
            .map_or("", String::as_str)
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

#[derive(Clone, Copy)]
enum WorkspaceHotkey {
    CycleVisibility,
    ToggleDisplay,
    OpenHelp,
    Back,
}

pub(super) struct WorkspaceSidebarState {
    model: WorkspaceModel,
    selected: Option<WorkspaceId>,
    status: WorkspaceStatus,
    help: bool,
}

const HELP_KEYS: [(&str, &str); 8] = [
    ("↵", "switch to workspace"),
    ("↑↓ j k", "move selection"),
    ("1-9 0", "pick by number"),
    ("c", "new workspace"),
    ("r", "rename"),
    ("h", "cycle visibility"),
    ("m", "toggle minimized"),
    ("q esc", "close"),
];

#[derive(Debug, Eq, PartialEq)]
pub(super) enum WorkspaceSidebarAction {
    Stay,
    Close,
    Create,
    CycleVisibility,
    ToggleDisplay,
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
            help: false,
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
        if self.help {
            self.help = false;
            return WorkspaceSidebarAction::Stay;
        }
        match key.code {
            KeyCode::Char('?') => {
                self.help = true;
                WorkspaceSidebarAction::Stay
            }
            KeyCode::Esc | KeyCode::Char('q') => WorkspaceSidebarAction::Close,
            KeyCode::Char('c') if key.modifiers == KeyModifiers::NONE => {
                WorkspaceSidebarAction::Create
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self
                .selected_item()
                .map(|item| WorkspaceSidebarAction::Rename(item.id, item.name.clone()))
                .unwrap_or(WorkspaceSidebarAction::Stay),
            KeyCode::Char('h') if key.modifiers == KeyModifiers::NONE => {
                WorkspaceSidebarAction::CycleVisibility
            }
            KeyCode::Char('m') if key.modifiers == KeyModifiers::NONE => {
                WorkspaceSidebarAction::ToggleDisplay
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

    pub fn click(
        &mut self,
        area: Rect,
        position: WorkspaceSidebarPosition,
        ui: &UiConfig,
        column: u16,
        row: u16,
    ) -> WorkspaceSidebarAction {
        if matches!(self.status, WorkspaceStatus::Switching) {
            return WorkspaceSidebarAction::Stay;
        }
        if let Some(hotkey) = workspace_hotkey_at(
            &self.model,
            &self.status,
            self.help,
            area,
            position,
            ui,
            column,
            row,
        ) {
            return match hotkey {
                WorkspaceHotkey::CycleVisibility => WorkspaceSidebarAction::CycleVisibility,
                WorkspaceHotkey::ToggleDisplay => WorkspaceSidebarAction::ToggleDisplay,
                WorkspaceHotkey::OpenHelp => {
                    self.help = true;
                    WorkspaceSidebarAction::Stay
                }
                WorkspaceHotkey::Back => {
                    self.help = false;
                    WorkspaceSidebarAction::Stay
                }
            };
        }
        if self.help {
            return WorkspaceSidebarAction::Stay;
        }
        workspace_item_at(
            &self.model,
            self.selected,
            Some(&self.status),
            area,
            position,
            ui,
            column,
            row,
        )
        .map(switch_to)
        .unwrap_or(WorkspaceSidebarAction::Stay)
    }

    pub fn passive_click(
        &self,
        area: Rect,
        position: WorkspaceSidebarPosition,
        ui: &UiConfig,
        column: u16,
        row: u16,
    ) -> WorkspaceSidebarAction {
        if ui.workspace_sidebar.display == WorkspaceSidebarDisplay::Expanded {
            return workspace_item_at(&self.model, None, None, area, position, ui, column, row)
                .map(switch_to)
                .unwrap_or(WorkspaceSidebarAction::Stay);
        }
        let Some(content) = sidebar_content(area, position) else {
            return WorkspaceSidebarAction::Stay;
        };
        if column < content.x || column >= content.right() {
            return WorkspaceSidebarAction::Stay;
        }
        let header = render_sidebar_chrome(&ui.workspace_sidebar.header, &self.model, None, ui);
        let header_height = if header.spans.is_empty() || content.height <= SIDEBAR_HEADER_HEIGHT {
            0
        } else {
            SIDEBAR_HEADER_HEIGHT
        };
        let rows_y = content.y.saturating_add(header_height);
        let rows_height = content.height.saturating_sub(header_height);
        let anchor = self
            .model
            .items
            .iter()
            .position(|item| item.current)
            .unwrap_or(0);
        let mut y = rows_y;
        for visible in visible_rows_with_item_height(
            self.model.items.len(),
            anchor,
            usize::from(rows_height),
            1,
        ) {
            match visible {
                VisibleRow::Ellipsis => y = y.saturating_add(1),
                VisibleRow::Item(index) => {
                    if row == y {
                        return self
                            .model
                            .items
                            .get(index)
                            .filter(|item| !item.closing)
                            .map(switch_to)
                            .unwrap_or(WorkspaceSidebarAction::Stay);
                    }
                    y = y.saturating_add(1);
                }
            }
        }
        WorkspaceSidebarAction::Stay
    }

    pub fn begin_switch(&mut self) {
        self.status = WorkspaceStatus::Switching;
    }

    pub fn switch_error(&mut self, message: String) {
        self.status = WorkspaceStatus::Error(sanitize(&message));
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the renderer keeps geometry, configuration, and cached inputs explicit"
    )]
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
            self.help,
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
    reason = "hotkey hit testing uses the renderer's complete configurable geometry"
)]
fn workspace_hotkey_at(
    model: &WorkspaceModel,
    status: &WorkspaceStatus,
    help: bool,
    area: Rect,
    position: WorkspaceSidebarPosition,
    ui: &UiConfig,
    column: u16,
    row: u16,
) -> Option<WorkspaceHotkey> {
    if !matches!(status, WorkspaceStatus::Ready) {
        return None;
    }
    let lines = workspace_hotkey_lines(help, ui);
    if lines.is_empty() {
        return None;
    }
    let content = sidebar_content(area, position)?;
    if column < content.x || column >= content.right() {
        return None;
    }
    let header = render_sidebar_chrome(&ui.workspace_sidebar.header, model, Some(status), ui);
    let header_height = if header.spans.is_empty() || content.height <= SIDEBAR_HEADER_HEIGHT {
        0
    } else {
        SIDEBAR_HEADER_HEIGHT
    };
    if content.height < 5 {
        return None;
    }
    let footer_height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(content.height.saturating_sub(header_height + 1));
    let footer_y = content.bottom().saturating_sub(footer_height);
    if row < footer_y || row >= content.bottom() {
        return None;
    }
    lines
        .get(usize::from(row - footer_y))?
        .action_at(usize::from(column - content.x))
}

#[allow(
    clippy::too_many_arguments,
    reason = "hit testing uses the renderer's complete configurable geometry"
)]
fn workspace_item_at<'a>(
    model: &'a WorkspaceModel,
    selected: Option<WorkspaceId>,
    status: Option<&WorkspaceStatus>,
    area: Rect,
    position: WorkspaceSidebarPosition,
    ui: &UiConfig,
    column: u16,
    row: u16,
) -> Option<&'a WorkspaceItem> {
    let content = sidebar_content(area, position)?;
    if column < content.x || column >= content.right() || row < content.y || row >= content.bottom()
    {
        return None;
    }
    let header = render_sidebar_chrome(&ui.workspace_sidebar.header, model, status, ui);
    let header_height = if header.spans.is_empty() || content.height <= SIDEBAR_HEADER_HEIGHT {
        0
    } else {
        SIDEBAR_HEADER_HEIGHT
    };
    let footer_lines = render_sidebar_footer(model, status, false, ui);
    let footer_allowed = status
        .is_none_or(|status| content.height >= 5 || !matches!(status, WorkspaceStatus::Ready));
    let urgent_footer = matches!(
        status,
        Some(WorkspaceStatus::Switching | WorkspaceStatus::Error(_))
    );
    let footer_capacity = if urgent_footer {
        content.height
    } else {
        content.height.saturating_sub(header_height + 1)
    };
    let footer_height = if footer_allowed && (content.height >= 2 || urgent_footer) {
        u16::try_from(footer_lines.len().min(usize::from(footer_capacity))).unwrap_or(u16::MAX)
    } else {
        0
    };
    let rows_y = content.y.saturating_add(header_height);
    let rows_height = content.height.saturating_sub(header_height + footer_height);
    if row < rows_y || row >= rows_y.saturating_add(rows_height) {
        return None;
    }
    let item_height = if ui.workspace_sidebar.row.detail.is_empty() {
        1
    } else {
        2
    };
    let anchor = selected
        .and_then(|id| model.items.iter().position(|item| item.id == id))
        .or_else(|| model.items.iter().position(|item| item.current))
        .unwrap_or(0);
    let mut y = rows_y;
    for visible in visible_rows_with_item_height(
        model.items.len(),
        anchor,
        usize::from(rows_height),
        item_height,
    ) {
        match visible {
            VisibleRow::Ellipsis => y = y.saturating_add(1),
            VisibleRow::Item(index) => {
                let height = u16::try_from(item_height).unwrap_or(u16::MAX);
                if row >= y && row < y.saturating_add(height) {
                    return model.items.get(index).filter(|item| !item.closing);
                }
                y = y.saturating_add(height);
            }
        }
    }
    None
}

fn sidebar_content(area: Rect, position: WorkspaceSidebarPosition) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if area.width == 1 {
        return Some(area);
    }
    Some(match position {
        WorkspaceSidebarPosition::Left => Rect::new(area.x, area.y, area.width - 1, area.height),
        WorkspaceSidebarPosition::Right => Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width - 1,
            area.height,
        ),
    })
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
    if ui.workspace_sidebar.display == WorkspaceSidebarDisplay::Minimized {
        render_minimized_model(&model, spinner_frame, area, position, ui, buffer);
        return;
    }
    render_model(
        &model,
        None,
        None,
        false,
        spinner_frame,
        area,
        position,
        ui,
        buffer,
    );
}

fn render_minimized_model(
    model: &WorkspaceModel,
    spinner_frame: usize,
    area: Rect,
    position: WorkspaceSidebarPosition,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    let Some(content) = render_sidebar_frame(area, position, ui, buffer) else {
        return;
    };
    let header = render_sidebar_chrome(&ui.workspace_sidebar.header, model, None, ui);
    let header_height = render_sidebar_header(&header, content, 2, buffer);
    let rows = Rect::new(
        content.x,
        content.y.saturating_add(header_height),
        content.width,
        content.height.saturating_sub(header_height),
    );
    let anchor = model
        .items
        .iter()
        .position(|item| item.current)
        .unwrap_or(0);
    for (offset, row) in
        visible_rows_with_item_height(model.items.len(), anchor, usize::from(rows.height), 1)
            .into_iter()
            .enumerate()
    {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        let y = rows.y.saturating_add(offset);
        match row {
            VisibleRow::Ellipsis => {
                buffer.set_line(rows.x, y, &Line::from(" …  "), rows.width);
            }
            VisibleRow::Item(index) => render_minimized_workspace_row(
                &model.items[index],
                spinner_frame,
                Rect::new(rows.x, y, rows.width, 1),
                ui,
                buffer,
            ),
        }
    }
}

fn render_minimized_workspace_row(
    item: &WorkspaceItem,
    spinner_frame: usize,
    area: Rect,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    if area.width == 0 {
        return;
    }
    let attention = matches!(
        item.activity,
        Some(ActivityIndicator::Blocked | ActivityIndicator::Completed)
    );
    let state = ItemState {
        current: false,
        selected: false,
        closing: item.closing,
        attention,
    };
    let base = apply_item_state(
        &ui.styles,
        state,
        ui.styles.apply(SemanticStyle::Normal, Style::default()),
    );
    clear(area, base, buffer);
    let marker = if item.current { CURRENT_MARKER } else { " " };
    let index = match item.index {
        0..=8 => (item.index + 1).to_string(),
        9 => "0".into(),
        _ => "…".into(),
    };
    let (status, status_style) = if item.closing {
        (
            truncate(&ui.icons.resolve().closing, 1),
            ui.styles.apply(SemanticStyle::Closing, base),
        )
    } else if let Some(activity) = item.activity {
        let role = match activity {
            ActivityIndicator::Working => SemanticStyle::Activity,
            ActivityIndicator::Blocked | ActivityIndicator::Completed => SemanticStyle::Attention,
        };
        (
            activity.marker(spinner_frame).into(),
            ui.styles.apply(role, base),
        )
    } else {
        (" ".into(), base)
    };
    buffer.set_line(
        area.x,
        area.y,
        &Line::from(vec![
            Span::styled(marker, base),
            Span::styled(" ", base),
            Span::styled(index, base),
            Span::styled(status, status_style),
            Span::styled(" ", base),
        ]),
        area.width,
    );
    if item.current {
        for column in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buffer.cell_mut((column, area.y)) {
                cell.modifier.insert(ratatui::style::Modifier::BOLD);
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "passive and interactive sidebar rendering share one explicit model renderer"
)]
fn render_model(
    model: &WorkspaceModel,
    selected: Option<WorkspaceId>,
    status: Option<&WorkspaceStatus>,
    help: bool,
    spinner_frame: usize,
    area: Rect,
    position: WorkspaceSidebarPosition,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    let Some(content) = render_sidebar_frame(area, position, ui, buffer) else {
        return;
    };

    let header_line = render_sidebar_chrome(&ui.workspace_sidebar.header, model, status, ui);
    let footer_lines = render_sidebar_footer(model, status, help, ui);
    let header_height = render_sidebar_header(&header_line, content, content.width, buffer);
    let footer_allowed = status
        .is_none_or(|status| content.height >= 5 || !matches!(status, WorkspaceStatus::Ready));
    let urgent_footer = matches!(
        status,
        Some(WorkspaceStatus::Switching | WorkspaceStatus::Error(_))
    );
    let footer_capacity = if urgent_footer {
        content.height
    } else {
        content.height.saturating_sub(header_height + 1)
    };
    let footer = if footer_allowed && (content.height >= 2 || urgent_footer) {
        footer_lines
            .into_iter()
            .take(usize::from(footer_capacity))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let footer_height = u16::try_from(footer.len()).unwrap_or(u16::MAX);
    let row_y = content.y.saturating_add(header_height);
    let row_height = content.height.saturating_sub(header_height + footer_height);
    if help {
        render_help(
            Rect::new(content.x, row_y, content.width, row_height),
            ui,
            buffer,
        );
    } else if model.items.is_empty() {
        let item = WorkspaceItem {
            id: WorkspaceId::new(),
            tokens: Default::default(),
            name: "workspace".into(),
            location: Some(std::path::PathBuf::new()),
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

    let footer_y = content
        .y
        .saturating_add(content.height.saturating_sub(footer_height));
    for (offset, footer) in footer.iter().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        buffer.set_line(
            content.x,
            footer_y.saturating_add(offset),
            &truncate_line(footer, usize::from(content.width)),
            content.width,
        );
    }
}

fn render_sidebar_footer(
    model: &WorkspaceModel,
    status: Option<&WorkspaceStatus>,
    help: bool,
    ui: &UiConfig,
) -> Vec<Line<'static>> {
    if help
        || matches!(status, Some(WorkspaceStatus::Ready))
            && ui.workspace_sidebar.uses_default_footer()
    {
        return workspace_hotkey_lines(help, ui)
            .into_iter()
            .map(|hotkey| hotkey.line)
            .collect();
    }
    let footer = render_sidebar_chrome(&ui.workspace_sidebar.footer, model, status, ui);
    (!footer.spans.is_empty())
        .then_some(footer)
        .into_iter()
        .collect()
}

fn workspace_hotkey_lines(help: bool, ui: &UiConfig) -> Vec<HotkeyLine<WorkspaceHotkey>> {
    let normal = ui.styles.apply(SemanticStyle::Normal, Style::default());
    let muted = ui.styles.apply(SemanticStyle::Muted, normal);
    if help {
        return vec![HotkeyLine::inline(
            &[HotkeyButton::new("esc", "back", WorkspaceHotkey::Back)],
            " ",
            "",
            "",
            normal,
            muted,
        )];
    }
    if !ui.workspace_sidebar.uses_default_footer() {
        return Vec::new();
    }
    let nerd_font = ui.icons.preset == IconPreset::NerdFont;
    let visibility_icon = if nerd_font {
        match ui.workspace_sidebar.visibility {
            WorkspaceSidebarVisibility::Visible => "󰈈",
            WorkspaceSidebarVisibility::AutoHideWhenSingle => "󱥼",
            WorkspaceSidebarVisibility::Hidden => "󰈉",
        }
    } else {
        ""
    };
    let display_icon = if nerd_font {
        match ui.workspace_sidebar.display {
            WorkspaceSidebarDisplay::Expanded => "󰡎",
            WorkspaceSidebarDisplay::Minimized => "󰡌",
        }
    } else {
        ""
    };
    [
        (
            HotkeyButton::new(
                "h",
                ui.workspace_sidebar.visibility.label(),
                WorkspaceHotkey::CycleVisibility,
            ),
            visibility_icon,
        ),
        (
            HotkeyButton::new(
                "m",
                ui.workspace_sidebar.display.label(),
                WorkspaceHotkey::ToggleDisplay,
            ),
            display_icon,
        ),
        (
            HotkeyButton::new("?", "hotkeys", WorkspaceHotkey::OpenHelp),
            if nerd_font { "󰘥" } else { "" },
        ),
    ]
    .into_iter()
    .map(|(button, icon)| HotkeyLine::row(&button, icon, normal, muted))
    .collect()
}

fn render_sidebar_frame(
    area: Rect,
    position: WorkspaceSidebarPosition,
    ui: &UiConfig,
    buffer: &mut Buffer,
) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
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
    (content.width > 0).then_some(content)
}

fn render_help(area: Rect, ui: &UiConfig, buffer: &mut Buffer) {
    let normal = ui.styles.apply(SemanticStyle::Normal, Style::default());
    let muted = ui.styles.apply(SemanticStyle::Muted, normal);
    for (line, (keys, label)) in HELP_KEYS.iter().enumerate() {
        let Ok(offset) = u16::try_from(line) else {
            return;
        };
        if offset >= area.height {
            return;
        }
        let y = area.y.saturating_add(offset);
        buffer.set_stringn(
            area.x,
            y,
            format!("  {keys}"),
            usize::from(area.width),
            normal,
        );
        let indent = 9;
        if area.width > indent {
            buffer.set_stringn(
                area.x + indent,
                y,
                *label,
                usize::from(area.width - indent),
                muted,
            );
        }
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
    let display = ui.workspace_sidebar.display.label();
    let visibility = ui.workspace_sidebar.visibility.label();
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
            "sidebar.display" => TokenValue::plain(display),
            "sidebar.visibility" => TokenValue::plain(visibility),
            "sidebar.status" => match status {
                Some(WorkspaceStatus::Ready) => {
                    TokenValue::plain(format!(" h/m/? · {visibility} · {display}"))
                }
                Some(WorkspaceStatus::Switching) => {
                    TokenValue::plain(format!(" switching… · {display} · {visibility}"))
                }
                Some(WorkspaceStatus::Error(message)) => TokenValue::styled(
                    format!(" {message} · retry · {display} · {visibility}"),
                    SemanticStyle::Error,
                ),
                None => TokenValue::plain(format!(" {display} · {visibility}")),
            },
            _ => TokenValue::plain(model.extension_value(token)),
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
        "workspace.marker" if item.current => TokenValue::plain(CURRENT_MARKER),
        "workspace.marker" => TokenValue::plain(" "),
        "workspace.index" => TokenValue::plain((item.index + 1).to_string()),
        "workspace.name" => TokenValue::plain(sanitize(&item.name)),
        "workspace.id" => TokenValue::plain(item.id.to_string()),
        "workspace.root" => match item.location.as_ref() {
            Some(location) => TokenValue::plain(sanitize(&location.to_string_lossy())),
            None => TokenValue::plain(crate::resources::MULTIPLE_LOCATIONS),
        },
        "workspace.root_name" => match item.location.as_ref() {
            Some(location) => TokenValue::plain(sanitize(
                &location
                    .file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            )),
            None => TokenValue::plain(crate::resources::MULTIPLE_LOCATIONS),
        },
        "workspace.closing" if item.closing => {
            TokenValue::styled(icons.closing.clone(), SemanticStyle::Closing)
        }
        "workspace.tab_count" => TokenValue::plain(item.tab_count.to_string()),
        "workspace.icon" => TokenValue::plain(icons.workspace.clone()),
        "workspace.git_branch" => TokenValue::plain(sanitize(item.token_value(token))),
        "workspace.git_added" if !item.token_value(token).is_empty() => {
            TokenValue::styled(item.token_value(token).to_owned(), SemanticStyle::Added)
        }
        "workspace.git_deleted" if !item.token_value(token).is_empty() => {
            TokenValue::styled(item.token_value(token).to_owned(), SemanticStyle::Deleted)
        }
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
        _ => TokenValue::plain(item.token_value(token)),
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

fn render_sidebar_header(
    header: &Line<'static>,
    content: Rect,
    max_width: u16,
    buffer: &mut Buffer,
) -> u16 {
    if header.spans.is_empty() || content.height <= SIDEBAR_HEADER_HEIGHT {
        return 0;
    }

    let width = content.width.min(max_width);
    buffer.set_line(
        content.x,
        content.y,
        &truncate_line(header, usize::from(width)),
        width,
    );
    for column in content.x..content.x.saturating_add(width) {
        if let Some(cell) = buffer.cell_mut((column, content.y)) {
            cell.modifier.insert(ratatui::style::Modifier::BOLD);
        }
    }
    SIDEBAR_HEADER_HEIGHT
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
                    tokens: Default::default(),
                    id: WorkspaceId::new(),
                    name: (*name).into(),
                    root: PathBuf::from(format!("/project/{index}")),
                    closing: false,
                    tabs: vec![TabSnapshot {
                        tokens: Default::default(),
                        id: TabId::new(),
                        name: "shell".into(),
                        closing: false,
                        layout: crate::splits::SplitTree::leaf(pane_id),
                        panes: vec![PaneSnapshot {
                            tokens: Default::default(),
                            id: pane_id,
                            terminal_id: TerminalId::new(),
                            closing: false,
                            activity: Default::default(),
                            cwd: None,
                            worktree: None,
                        }],
                    }],
                }
            })
            .collect::<Vec<_>>();
        let workspace = &workspaces[current];
        let tab = &workspace.tabs[0];
        let pane = tab.panes[0].clone();
        let workspace_id = workspace.id;
        let tab_id = tab.id;
        let pane_id = pane.id;
        let terminal_id = pane.terminal_id;
        (
            ResourceSnapshot {
                revision: 1,
                sessions: vec![SessionSnapshot {
                    tokens: Default::default(),
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

    #[test]
    fn extension_tokens_resolve_from_sidebar_ancestry_and_workspace_rows() {
        let (mut snapshot, focused) = fixture(&["main", "peer"], 0);
        let session = &mut snapshot.sessions[0];
        session
            .tokens
            .insert("session.extension.demo.value".into(), "session".into());
        let workspace = &mut session.workspaces[0];
        workspace
            .tokens
            .insert("workspace.extension.demo.value".into(), "workspace".into());
        let tab = &mut workspace.tabs[0];
        tab.tokens
            .insert("tab.extension.demo.value".into(), "tab".into());
        tab.panes[0]
            .tokens
            .insert("pane.extension.demo.value".into(), "pane".into());

        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        assert_eq!(
            model.extension_value("session.extension.demo.value"),
            "session"
        );
        assert_eq!(
            model.extension_value("workspace.extension.demo.value"),
            "workspace"
        );
        assert_eq!(model.extension_value("tab.extension.demo.value"), "tab");
        assert_eq!(model.extension_value("pane.extension.demo.value"), "pane");
        assert_eq!(
            model.items[0].token_value("workspace.extension.demo.value"),
            "workspace"
        );
        assert_eq!(
            model.items[1].token_value("workspace.extension.demo.value"),
            ""
        );
    }

    #[test]
    fn workspace_git_tokens_render_from_snapshot_with_builtin_semantic_styles() {
        let (mut snapshot, focused) = fixture(&["main"], 0);
        let tokens = &mut snapshot.sessions[0].workspaces[0].tokens;
        tokens.insert("workspace.git_branch".into(), "feature".into());
        tokens.insert("workspace.git_added".into(), "+3".into());
        tokens.insert("workspace.git_deleted".into(), "-2".into());
        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );

        let (text, buffer) = rendered(&model, None, None, 28, 6, WorkspaceSidebarPosition::Left);
        assert!(text.contains("feature +3 -2"), "{text:?}");
        assert_eq!(
            buffer
                .content()
                .iter()
                .find(|cell| cell.symbol() == "+")
                .unwrap()
                .fg,
            ratatui::style::Color::Green
        );
        assert_eq!(
            buffer
                .content()
                .iter()
                .find(|cell| cell.symbol() == "-")
                .unwrap()
                .fg,
            ratatui::style::Color::Red
        );
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
            false,
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
    fn rows_show_the_shared_live_location_or_multiple() {
        let (mut snapshot, focused) = fixture(&["main", "feature"], 0);
        let worktree = PathBuf::from("/project/worktrees/feature");
        {
            let feature = &mut snapshot.sessions[0].workspaces[1];
            feature.tabs[0].panes[0].cwd = Some(worktree.join("src"));
            feature.tabs[0].panes[0].worktree = Some(worktree.clone());
            let mut editor = feature.tabs[0].panes[0].clone();
            editor.id = PaneId::new();
            editor.terminal_id = TerminalId::new();
            editor.cwd = Some(worktree.clone());
            feature.tabs.push(TabSnapshot {
                tokens: Default::default(),
                id: TabId::new(),
                name: "editor".into(),
                closing: false,
                layout: crate::splits::SplitTree::leaf(editor.id),
                panes: vec![editor],
            });
        }
        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        assert_eq!(
            model.items[0].location,
            Some(PathBuf::from("/project/0")),
            "unobserved panes fall back to the workspace root"
        );
        assert_eq!(
            model.items[1].location,
            Some(worktree.clone()),
            "subdirectories of one work tree are one location"
        );

        snapshot.sessions[0].workspaces[1].tabs[1].panes[0].cwd = Some(PathBuf::from("/elsewhere"));
        snapshot.sessions[0].workspaces[1].tabs[1].panes[0].worktree = None;
        let diverged = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        assert_eq!(diverged.items[1].location, None, "disagreement is multiple");
    }

    #[test]
    fn model_uses_fresh_terminal_ancestry_and_remembered_destinations() {
        let (mut snapshot, mut focused) = fixture(&["main", "feature"], 1);
        focused.workspace_id = snapshot.sessions[0].workspaces[0].id;
        let feature = &mut snapshot.sessions[0].workspaces[1];
        let remembered = PaneSnapshot {
            tokens: Default::default(),
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing: false,
            activity: Default::default(),
            cwd: None,
            worktree: None,
        };
        feature.tabs.push(TabSnapshot {
            tokens: Default::default(),
            id: TabId::new(),
            name: "remembered".into(),
            closing: false,
            layout: crate::splits::SplitTree::leaf(remembered.id),
            panes: vec![remembered.clone()],
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
    fn clicks_activate_expanded_and_minimized_workspaces() {
        let (snapshot, focused) = fixture(&["main", "feature"], 0);
        let mut state = WorkspaceSidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        )
        .unwrap();
        let feature_pane = snapshot.sessions[0].workspaces[1].tabs[0].panes[0].id;
        let area = Rect::new(0, 0, 28, 24);
        assert_eq!(
            state.passive_click(
                area,
                WorkspaceSidebarPosition::Left,
                &UiConfig::default(),
                5,
                5,
            ),
            WorkspaceSidebarAction::Select(feature_pane)
        );

        assert_eq!(
            state.click(
                area,
                WorkspaceSidebarPosition::Left,
                &UiConfig::default(),
                5,
                21,
            ),
            WorkspaceSidebarAction::CycleVisibility
        );
        assert_eq!(
            state.click(
                area,
                WorkspaceSidebarPosition::Left,
                &UiConfig::default(),
                5,
                22,
            ),
            WorkspaceSidebarAction::ToggleDisplay
        );
        assert_eq!(
            state.click(
                area,
                WorkspaceSidebarPosition::Left,
                &UiConfig::default(),
                5,
                23,
            ),
            WorkspaceSidebarAction::Stay
        );
        assert!(state.help);
        assert_eq!(
            state.click(
                area,
                WorkspaceSidebarPosition::Left,
                &UiConfig::default(),
                2,
                23,
            ),
            WorkspaceSidebarAction::Stay
        );
        assert!(!state.help);

        let mut minimized = UiConfig::default();
        minimized.workspace_sidebar.display = WorkspaceSidebarDisplay::Minimized;
        assert_eq!(
            state.passive_click(
                Rect::new(0, 0, 5, 24),
                WorkspaceSidebarPosition::Left,
                &minimized,
                2,
                4,
            ),
            WorkspaceSidebarAction::Select(feature_pane)
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
            WorkspaceSidebarAction::CycleVisibility
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::ToggleDisplay
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::Stay
        );
        assert!(state.help, "? opens the hotkey help");
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            WorkspaceSidebarAction::Stay,
            "any key leaves help without acting"
        );
        assert!(!state.help);
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
            rendered(&model, None, None, 24, 10, WorkspaceSidebarPosition::Left);
        assert!(left.lines().next().unwrap().contains("project"));
        assert!(left.contains("main"));
        assert!(left.contains("bad�name"));
        assert!(left.contains("closing"));
        assert!(left.contains('×'));
        assert!(left_buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(left.lines().nth(1).unwrap().trim(), "│");
        assert_eq!(left.lines().nth(2).unwrap().trim(), "│");
        assert_eq!(left_buffer[(0, 3)].symbol(), CURRENT_MARKER);
        assert_eq!(left_buffer[(1, 3)].symbol(), " ");
        assert_eq!(left_buffer[(2, 3)].symbol(), "1");
        assert_eq!(left_buffer[(3, 3)].symbol(), " ");
        assert_eq!(left_buffer[(22, 7)].symbol(), " ");
        assert!(!left_buffer[(0, 3)].modifier.contains(Modifier::REVERSED));
        assert!(left_buffer[(0, 3)].modifier.contains(Modifier::BOLD));
        assert_eq!(left_buffer[(23, 0)].symbol(), "│");
        assert_eq!(left_buffer[(23, 0)].fg, ratatui::style::Color::DarkGray);
        assert!(
            left.lines().nth(5).unwrap().contains("bad�name"),
            "entries follow each other without spacing"
        );

        let (right, right_buffer) =
            rendered(&model, None, None, 24, 10, WorkspaceSidebarPosition::Right);
        assert!(right.contains("main"));
        assert_eq!(right_buffer[(0, 0)].symbol(), "│");
        assert_eq!(right_buffer[(1, 3)].symbol(), CURRENT_MARKER);
    }

    #[test]
    fn minimized_render_keeps_marker_number_status_padding_and_mirrored_divider() {
        let (snapshot, focused) = fixture(&["main", "feature"], 0);
        let mut model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        model.items[1].activity = Some(ActivityIndicator::Blocked);
        let ui = UiConfig::default();

        let area = Rect::new(0, 0, 6, 2);
        let mut left = Buffer::empty(area);
        render_minimized_model(
            &model,
            0,
            area,
            WorkspaceSidebarPosition::Left,
            &ui,
            &mut left,
        );
        assert_eq!(left[(0, 0)].symbol(), CURRENT_MARKER);
        assert_eq!(left[(1, 0)].symbol(), " ");
        assert_eq!(left[(2, 0)].symbol(), "1");
        assert_eq!(left[(3, 1)].symbol(), "!");
        assert_eq!(left[(4, 1)].symbol(), " ");
        assert_eq!(left[(5, 1)].symbol(), "│");

        let mut right = Buffer::empty(area);
        render_minimized_model(
            &model,
            0,
            area,
            WorkspaceSidebarPosition::Right,
            &ui,
            &mut right,
        );
        assert_eq!(right[(0, 1)].symbol(), "│");
        assert_eq!(right[(1, 0)].symbol(), CURRENT_MARKER);
        assert_eq!(right[(2, 0)].symbol(), " ");
        assert_eq!(right[(3, 0)].symbol(), "1");
        assert_eq!(right[(4, 1)].symbol(), "!");
        assert_eq!(right[(5, 1)].symbol(), " ");

        let area = Rect::new(0, 0, 6, 5);
        let mut headed = Buffer::empty(area);
        render_minimized_model(
            &model,
            0,
            area,
            WorkspaceSidebarPosition::Left,
            &ui,
            &mut headed,
        );
        assert_eq!(headed[(0, 0)].symbol(), "p");
        assert_eq!(headed[(1, 0)].symbol(), "…");
        assert!(headed[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(headed[(0, 1)].symbol(), " ");
        assert_eq!(headed[(0, 2)].symbol(), " ");
        assert_eq!(headed[(0, 3)].symbol(), CURRENT_MARKER);
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
            assert!(text.contains(CURRENT_MARKER), "width {width}: {text:?}");
        }
    }

    #[test]
    fn active_render_marks_selection_and_lists_footer_hotkeys_on_separate_lines() {
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
            9,
            WorkspaceSidebarPosition::Left,
        );
        assert!(ready.contains("hide with one"));
        let lines = ready.lines().collect::<Vec<_>>();
        assert!(lines[6].contains("h  hide with one"));
        assert!(lines[7].contains("m  expanded"));
        assert!(lines[8].contains("?  hotkeys"));
        assert_eq!(buffer[(0, 4)].bg, ratatui::style::Color::DarkGray);
        assert!(
            !buffer[(0, 4)].modifier.contains(Modifier::UNDERLINED),
            "selection reads as a background, never an underline"
        );
        assert_eq!(
            buffer[(0, 2)].bg,
            ratatui::style::Color::Reset,
            "header padding keeps the plain bar background"
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

    #[test]
    fn nerd_font_footer_uses_stateful_visibility_and_display_icons() {
        let mut ui: UiConfig = toml::from_str("[icons]\npreset = 'nerd_font'\n").unwrap();
        let lines = workspace_hotkey_lines(false, &ui);
        assert_eq!(lines[0].line.spans[1].content, "󱥼  ");
        assert_eq!(lines[1].line.spans[1].content, "󰡎  ");
        assert_eq!(lines[2].line.spans[1].content, "󰘥  ");

        ui.workspace_sidebar.visibility = WorkspaceSidebarVisibility::Hidden;
        ui.workspace_sidebar.display = WorkspaceSidebarDisplay::Minimized;
        let lines = workspace_hotkey_lines(false, &ui);
        assert_eq!(lines[0].line.spans[1].content, "󰈉  ");
        assert_eq!(lines[1].line.spans[1].content, "󰡌  ");
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
            false,
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
