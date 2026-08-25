use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
};

use crate::{
    domain::{PaneId, SessionId, TerminalId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{MaterializedTokenMap, PanePathRef, ResourceSnapshot},
};

use super::{
    actions::NavigationScope,
    agents::{self, AgentItem},
    chrome::{sanitize, truncate},
    config::{
        AgentScope, IconPreset, MINIMIZED_SIDEBAR_WIDTH, SemanticStyle, SidebarComponentConfig,
        SidebarComponentSize, SidebarDisplay, SidebarSlotConfig, SidebarVisibility, UiConfig,
        WorkspaceComponentConfigRef,
    },
    hotkey::{HotkeyButton, HotkeyLine},
    navigation::NavigationHistory,
    notifications::{ActivityIndicator, NotificationState},
    presentation::{
        ItemState, TokenValue, apply_item_state, extension_token_value, render_token_segments,
        truncate_line,
    },
};

/// Compact workspace and agent rails use the same preset-independent marker.
const CURRENT_MARKER: &str = "•";
const SIDEBAR_HEADER_HEIGHT: u16 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarSide {
    Left,
    Right,
}

impl SidebarSide {
    pub(super) const ALL: [Self; 2] = [Self::Left, Self::Right];

    pub(super) fn config(self, ui: &UiConfig) -> &SidebarSlotConfig {
        match self {
            Self::Left => &ui.sidebar.left,
            Self::Right => &ui.sidebar.right,
        }
    }

    pub(super) fn config_mut(self, ui: &mut UiConfig) -> &mut SidebarSlotConfig {
        match self {
            Self::Left => &mut ui.sidebar.left,
            Self::Right => &mut ui.sidebar.right,
        }
    }
}

impl AgentScope {
    fn navigation_scope(self) -> NavigationScope {
        match self {
            Self::Tab => NavigationScope::Tab,
            Self::Workspace => NavigationScope::Workspace,
            Self::Session => NavigationScope::Session,
            Self::Global => NavigationScope::Global,
        }
    }
}

fn path_is_live(path: PanePathRef<'_>) -> bool {
    !path.session.closing && !path.workspace.closing && !path.tab.closing && !path.pane.closing
}

fn fresh_focused_path<'a>(
    snapshot: &'a ResourceSnapshot,
    focused: &SelectedTarget,
) -> Option<PanePathRef<'a>> {
    snapshot
        .pane_paths()
        .find(|path| path.pane.id == focused.pane_id && path_is_live(*path))
}

#[derive(Clone, Copy)]
struct FocusedAncestry {
    session_id: SessionId,
    workspace_id: WorkspaceId,
}

fn focused_ancestry(snapshot: &ResourceSnapshot, focused: &SelectedTarget) -> FocusedAncestry {
    fresh_focused_path(snapshot, focused).map_or(
        FocusedAncestry {
            session_id: focused.session_id,
            workspace_id: focused.workspace_id,
        },
        |path| FocusedAncestry {
            session_id: path.session.id,
            workspace_id: path.workspace.id,
        },
    )
}

pub(super) fn slot_relevant(
    snapshot: &ResourceSnapshot,
    focused: &SelectedTarget,
    side: SidebarSide,
    ui: &UiConfig,
) -> bool {
    let focused_ancestry = focused_ancestry(snapshot, focused);
    side.config(ui)
        .components
        .iter()
        .any(|component| match component {
            SidebarComponentConfig::Workspaces { .. } => {
                snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id == focused_ancestry.session_id)
                    .into_iter()
                    .flat_map(|session| &session.workspaces)
                    .filter(|workspace| !workspace.closing)
                    .count()
                    > 1
            }
            SidebarComponentConfig::Agents { scope, .. } => {
                agents::has_items(snapshot, focused, *scope)
            }
        })
}

fn workspace_config(ui: &UiConfig, side: SidebarSide) -> WorkspaceComponentConfigRef<'_> {
    side.config(ui)
        .workspaces()
        .expect("workspace component renderer requires workspace configuration")
}

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
        let focused_ancestry = focused_ancestry(snapshot, focused);
        let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.id == focused_ancestry.session_id)
        else {
            return Self::default();
        };

        let focused_workspace = session
            .workspaces
            .iter()
            .find(|workspace| workspace.id == focused_ancestry.workspace_id);
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
                        current: workspace.id == focused_ancestry.workspace_id,
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

fn switch_to(item: &WorkspaceItem) -> ComponentEffect {
    if item.current {
        ComponentEffect::CloseSidebar
    } else if let Some(destination) = item.destination {
        ComponentEffect::Navigate(
            destination,
            NavigationScope::Workspace,
            SidebarComponentKind::Workspaces,
        )
    } else {
        ComponentEffect::Stay
    }
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

pub(super) struct WorkspacesComponent {
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
pub(super) enum ComponentEffect {
    Stay,
    CloseSidebar,
    CreateWorkspace,
    CycleVisibility,
    ToggleDisplay,
    RenameWorkspace(WorkspaceId, String),
    Navigate(PaneId, NavigationScope, SidebarComponentKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarComponentKind {
    Workspaces,
    Agents,
}

impl WorkspacesComponent {
    pub fn open(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
        notifications: &NotificationState,
    ) -> Self {
        let model = WorkspaceModel::from_snapshot(snapshot, focused, history, notifications);
        let selected = model
            .items
            .iter()
            .find(|item| item.current && item.destination.is_some())
            .or_else(|| model.items.iter().find(|item| item.destination.is_some()))
            .map(|item| item.id);
        Self {
            model,
            selected,
            status: WorkspaceStatus::Ready,
            help: false,
        }
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

    pub fn key(&mut self, key: KeyEvent) -> ComponentEffect {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || matches!(self.status, WorkspaceStatus::Switching)
        {
            return ComponentEffect::Stay;
        }
        if self.help {
            self.help = false;
            return ComponentEffect::Stay;
        }
        match key.code {
            KeyCode::Char('?') => {
                self.help = true;
                ComponentEffect::Stay
            }
            KeyCode::Esc | KeyCode::Char('q') => ComponentEffect::CloseSidebar,
            KeyCode::Char('c') if key.modifiers == KeyModifiers::NONE => {
                ComponentEffect::CreateWorkspace
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::NONE => self
                .selected_item()
                .map(|item| ComponentEffect::RenameWorkspace(item.id, item.name.clone()))
                .unwrap_or(ComponentEffect::Stay),
            KeyCode::Char('h') if key.modifiers == KeyModifiers::NONE => {
                ComponentEffect::CycleVisibility
            }
            KeyCode::Char('m') if key.modifiers == KeyModifiers::NONE => {
                ComponentEffect::ToggleDisplay
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
                    .unwrap_or(ComponentEffect::Stay)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(false);
                ComponentEffect::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(true);
                ComponentEffect::Stay
            }
            KeyCode::Home => {
                self.selected = self
                    .model
                    .items
                    .iter()
                    .find(|item| item.destination.is_some())
                    .map(|item| item.id);
                self.status = WorkspaceStatus::Ready;
                ComponentEffect::Stay
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
                ComponentEffect::Stay
            }
            KeyCode::Enter => self
                .selected
                .and_then(|id| self.model.items.iter().find(|item| item.id == id))
                .map(switch_to)
                .unwrap_or(ComponentEffect::Stay),
            _ => ComponentEffect::Stay,
        }
    }

    pub fn click(
        &mut self,
        area: Rect,
        position: SidebarSide,
        ui: &UiConfig,
        column: u16,
        row: u16,
    ) -> ComponentEffect {
        if matches!(self.status, WorkspaceStatus::Switching) {
            return ComponentEffect::Stay;
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
                WorkspaceHotkey::CycleVisibility => ComponentEffect::CycleVisibility,
                WorkspaceHotkey::ToggleDisplay => ComponentEffect::ToggleDisplay,
                WorkspaceHotkey::OpenHelp => {
                    self.help = true;
                    ComponentEffect::Stay
                }
                WorkspaceHotkey::Back => {
                    self.help = false;
                    ComponentEffect::Stay
                }
            };
        }
        if self.help {
            return ComponentEffect::Stay;
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
        .unwrap_or(ComponentEffect::Stay)
    }

    pub fn passive_click(
        &self,
        area: Rect,
        position: SidebarSide,
        ui: &UiConfig,
        column: u16,
        row: u16,
    ) -> ComponentEffect {
        self.item_at(area, position, ui, column, row)
            .map(switch_to)
            .unwrap_or(ComponentEffect::Stay)
    }

    pub fn item_id_at(
        &self,
        area: Rect,
        position: SidebarSide,
        ui: &UiConfig,
        column: u16,
        row: u16,
    ) -> Option<WorkspaceId> {
        self.item_at(area, position, ui, column, row)
            .map(|item| item.id)
    }

    fn item_at(
        &self,
        area: Rect,
        position: SidebarSide,
        ui: &UiConfig,
        column: u16,
        row: u16,
    ) -> Option<&WorkspaceItem> {
        if !sidebar_is_minimized(area, position, ui) {
            return workspace_item_at(&self.model, None, None, area, position, ui, column, row);
        }
        let content = sidebar_content(area, position)?;
        if column < content.x || column >= content.right() {
            return None;
        }
        let header = render_sidebar_chrome(
            workspace_config(ui, position).header,
            &self.model,
            None,
            0,
            position,
            ui,
        );
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
                        return self.model.items.get(index).filter(|item| !item.closing);
                    }
                    y = y.saturating_add(1);
                }
            }
        }
        None
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
        position: SidebarSide,
        focused: bool,
        ui: &UiConfig,
        spinner_frame: usize,
        buffer: &mut Buffer,
    ) {
        render_model(
            &self.model,
            focused.then_some(self.selected).flatten(),
            focused.then_some(&self.status),
            focused && self.help,
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

pub(super) struct AgentsComponent {
    items: Vec<AgentItem>,
    selected: Option<TerminalId>,
    scope: AgentScope,
}

impl Default for AgentsComponent {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected: None,
            scope: AgentScope::Session,
        }
    }
}

impl AgentsComponent {
    fn open(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        notifications: &NotificationState,
        scope: AgentScope,
    ) -> Self {
        let mut component = Self {
            scope,
            ..Self::default()
        };
        component.accept_resources(snapshot, focused, notifications);
        component
    }

    fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        notifications: &NotificationState,
    ) {
        let selected = self.selected;
        self.items = agents::items(snapshot, focused, notifications, self.scope);
        self.selected = selected
            .filter(|terminal_id| {
                self.items
                    .iter()
                    .any(|item| item.terminal_id == *terminal_id)
            })
            .or_else(|| {
                self.items
                    .iter()
                    .find(|item| item.current)
                    .map(|item| item.terminal_id)
            })
            .or_else(|| self.items.first().map(|item| item.terminal_id));
    }

    fn key(&mut self, key: KeyEvent) -> ComponentEffect {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentEffect::Stay;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => ComponentEffect::CloseSidebar,
            KeyCode::Char('h') if key.modifiers == KeyModifiers::NONE => {
                ComponentEffect::CycleVisibility
            }
            KeyCode::Char('m') if key.modifiers == KeyModifiers::NONE => {
                ComponentEffect::ToggleDisplay
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(false);
                ComponentEffect::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(true);
                ComponentEffect::Stay
            }
            KeyCode::Home => {
                self.selected = self.items.first().map(|item| item.terminal_id);
                ComponentEffect::Stay
            }
            KeyCode::End => {
                self.selected = self.items.last().map(|item| item.terminal_id);
                ComponentEffect::Stay
            }
            KeyCode::Enter => self.selected_item().map_or(ComponentEffect::Stay, |item| {
                ComponentEffect::Navigate(
                    item.pane_id,
                    self.scope.navigation_scope(),
                    SidebarComponentKind::Agents,
                )
            }),
            _ => ComponentEffect::Stay,
        }
    }

    fn click(&mut self, area: Rect, side: SidebarSide, column: u16, row: u16) -> ComponentEffect {
        let Some(index) = agent_item_at(area, side, self, column, row) else {
            return ComponentEffect::Stay;
        };
        let item = &self.items[index];
        self.selected = Some(item.terminal_id);
        ComponentEffect::Navigate(
            item.pane_id,
            self.scope.navigation_scope(),
            SidebarComponentKind::Agents,
        )
    }

    fn passive_click(
        &self,
        area: Rect,
        side: SidebarSide,
        column: u16,
        row: u16,
    ) -> ComponentEffect {
        agent_item_at(area, side, self, column, row).map_or(ComponentEffect::Stay, |index| {
            ComponentEffect::Navigate(
                self.items[index].pane_id,
                self.scope.navigation_scope(),
                SidebarComponentKind::Agents,
            )
        })
    }

    fn move_selection(&mut self, forward: bool) {
        if self.items.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|selected| {
                self.items
                    .iter()
                    .position(|item| item.terminal_id == selected)
            })
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % self.items.len()
        } else if current == 0 {
            self.items.len() - 1
        } else {
            current - 1
        };
        self.selected = Some(self.items[next].terminal_id);
    }

    fn selected_item(&self) -> Option<&AgentItem> {
        self.selected.and_then(|terminal_id| {
            self.items
                .iter()
                .find(|item| item.terminal_id == terminal_id)
        })
    }

    fn render(
        &self,
        area: Rect,
        side: SidebarSide,
        focused: bool,
        spinner_frame: usize,
        ui: &UiConfig,
        buffer: &mut Buffer,
    ) {
        let Some(content) = render_sidebar_frame(area, side, ui, buffer) else {
            return;
        };
        if content.height == 0 {
            return;
        }
        let normal = ui.styles.apply(SemanticStyle::Normal, Style::default());
        let title_style = if focused {
            ui.styles.apply(SemanticStyle::Current, normal)
        } else {
            ui.styles.apply(SemanticStyle::Muted, normal)
        };
        let minimized = sidebar_is_minimized(area, side, ui);
        buffer.set_stringn(
            content.x,
            content.y,
            if minimized { " Agts" } else { " Agents" },
            usize::from(content.width),
            title_style,
        );
        for (index, row) in agent_rows(content, self) {
            let selected = self.selected == Some(self.items[index].terminal_id) && focused;
            if minimized {
                render_minimized_agent_row(
                    &self.items[index],
                    index,
                    selected,
                    spinner_frame,
                    row,
                    ui,
                    buffer,
                );
            } else {
                render_agent_row(&self.items[index], selected, spinner_frame, row, ui, buffer);
            }
        }
    }
}

pub(super) enum SidebarComponent {
    Workspaces(WorkspacesComponent),
    Agents(AgentsComponent),
}

impl SidebarComponent {
    fn key(&mut self, key: KeyEvent) -> ComponentEffect {
        match self {
            Self::Workspaces(component) => component.key(key),
            Self::Agents(component) => component.key(key),
        }
    }

    fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
        notifications: &NotificationState,
    ) {
        match self {
            Self::Workspaces(component) => {
                component.accept_resources(snapshot, focused, history, notifications);
            }
            Self::Agents(component) => {
                component.accept_resources(snapshot, focused, notifications);
            }
        }
    }
}

pub(super) struct SidebarState {
    side: SidebarSide,
    components: Vec<SidebarComponent>,
    focused_component: usize,
}

impl SidebarState {
    pub(super) fn open(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
        notifications: &NotificationState,
        side: SidebarSide,
        ui: &UiConfig,
    ) -> Option<Self> {
        let components = side
            .config(ui)
            .components
            .iter()
            .map(|config| match config {
                SidebarComponentConfig::Workspaces { .. } => SidebarComponent::Workspaces(
                    WorkspacesComponent::open(snapshot, focused, history, notifications),
                ),
                SidebarComponentConfig::Agents { scope, .. } => SidebarComponent::Agents(
                    AgentsComponent::open(snapshot, focused, notifications, *scope),
                ),
            })
            .collect::<Vec<_>>();
        (!components.is_empty()).then_some(Self {
            side,
            components,
            focused_component: 0,
        })
    }

    pub(super) fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
        notifications: &NotificationState,
    ) {
        for component in &mut self.components {
            component.accept_resources(snapshot, focused, history, notifications);
        }
        self.focused_component = self
            .focused_component
            .min(self.components.len().saturating_sub(1));
    }

    pub(super) fn reconfigure(
        &mut self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        history: &NavigationHistory,
        notifications: &NotificationState,
        ui: &UiConfig,
    ) -> bool {
        if let Some(reconfigured) =
            Self::open(snapshot, focused, history, notifications, self.side, ui)
        {
            *self = reconfigured;
            true
        } else {
            false
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent, area: Rect, ui: &UiConfig) -> ComponentEffect {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ComponentEffect::Stay;
        }
        match key.code {
            KeyCode::Tab => {
                self.cycle_focus(true, area, ui);
                ComponentEffect::Stay
            }
            KeyCode::BackTab => {
                self.cycle_focus(false, area, ui);
                ComponentEffect::Stay
            }
            _ => self
                .components
                .get_mut(self.focused_component)
                .map_or(ComponentEffect::Stay, |component| component.key(key)),
        }
    }

    pub(super) fn click(
        &mut self,
        area: Rect,
        ui: &UiConfig,
        column: u16,
        row: u16,
    ) -> ComponentEffect {
        let geometry = SidebarGeometry::new(area, &self.side.config(ui).components);
        let Some(component_geometry) = geometry.component_at(column, row) else {
            return ComponentEffect::Stay;
        };
        let was_focused = self.focused_component == component_geometry.index;
        let effect = match self.components.get_mut(component_geometry.index) {
            Some(SidebarComponent::Workspaces(component)) if was_focused => {
                component.click(component_geometry.area, self.side, ui, column, row)
            }
            Some(SidebarComponent::Workspaces(component)) => {
                component.passive_click(component_geometry.area, self.side, ui, column, row)
            }
            Some(SidebarComponent::Agents(component)) => {
                component.click(component_geometry.area, self.side, column, row)
            }
            None => ComponentEffect::Stay,
        };
        self.focused_component = component_geometry.index;
        effect
    }

    pub(super) fn passive_click(
        &self,
        area: Rect,
        ui: &UiConfig,
        column: u16,
        row: u16,
    ) -> ComponentEffect {
        let geometry = SidebarGeometry::new(area, &self.side.config(ui).components);
        let Some(component_geometry) = geometry.component_at(column, row) else {
            return ComponentEffect::Stay;
        };
        match self.components.get(component_geometry.index) {
            Some(SidebarComponent::Workspaces(component)) => {
                component.passive_click(component_geometry.area, self.side, ui, column, row)
            }
            Some(SidebarComponent::Agents(component)) => {
                component.passive_click(component_geometry.area, self.side, column, row)
            }
            None => ComponentEffect::Stay,
        }
    }

    pub(super) fn workspace_item_id_at(
        &self,
        area: Rect,
        ui: &UiConfig,
        column: u16,
        row: u16,
    ) -> Option<WorkspaceId> {
        let geometry = SidebarGeometry::new(area, &self.side.config(ui).components);
        let component_geometry = geometry.component_at(column, row)?;
        match self.components.get(component_geometry.index)? {
            SidebarComponent::Workspaces(component) => {
                component.item_id_at(component_geometry.area, self.side, ui, column, row)
            }
            SidebarComponent::Agents(_) => None,
        }
    }

    pub(super) fn begin_switch(&mut self) {
        if let Some(SidebarComponent::Workspaces(component)) =
            self.components.get_mut(self.focused_component)
        {
            component.begin_switch();
        }
    }

    pub(super) fn switch_error(&mut self, message: String) -> bool {
        if let Some(SidebarComponent::Workspaces(component)) =
            self.components.get_mut(self.focused_component)
        {
            component.switch_error(message);
            true
        } else {
            false
        }
    }

    pub(super) fn render(
        &self,
        area: Rect,
        ui: &UiConfig,
        spinner_frame: usize,
        buffer: &mut Buffer,
    ) {
        render_sidebar_frame(area, self.side, ui, buffer);
        let geometry = SidebarGeometry::new(area, &self.side.config(ui).components);
        for component_geometry in &geometry.components {
            let Some(component) = self.components.get(component_geometry.index) else {
                continue;
            };
            match component {
                SidebarComponent::Workspaces(component) => component.render(
                    component_geometry.area,
                    self.side,
                    component_geometry.index == self.focused_component,
                    ui,
                    spinner_frame,
                    buffer,
                ),
                SidebarComponent::Agents(component) => component.render(
                    component_geometry.area,
                    self.side,
                    component_geometry.index == self.focused_component,
                    spinner_frame,
                    ui,
                    buffer,
                ),
            }
        }
        render_component_dividers(&geometry, self.side, ui, buffer);
    }

    pub(super) const fn side(&self) -> SidebarSide {
        self.side
    }

    fn cycle_focus(&mut self, forward: bool, area: Rect, ui: &UiConfig) {
        let focusable = SidebarGeometry::new(area, &self.side.config(ui).components)
            .components
            .into_iter()
            .filter(|component| component.area.height > 0)
            .map(|component| component.index)
            .collect::<Vec<_>>();
        if focusable.len() < 2 {
            return;
        }
        let current = focusable
            .iter()
            .position(|index| *index == self.focused_component)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % focusable.len()
        } else if current == 0 {
            focusable.len() - 1
        } else {
            current - 1
        };
        self.focused_component = focusable[next];
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ComponentGeometry {
    index: usize,
    area: Rect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SidebarGeometry {
    components: Vec<ComponentGeometry>,
    dividers: Vec<Rect>,
}

impl SidebarGeometry {
    fn new(area: Rect, components: &[SidebarComponentConfig]) -> Self {
        let divider_count = components
            .len()
            .saturating_sub(1)
            .min(usize::from(area.height.saturating_sub(
                u16::try_from(components.len()).unwrap_or(u16::MAX),
            )));
        let component_height = area
            .height
            .saturating_sub(u16::try_from(divider_count).unwrap_or(u16::MAX));
        let has_fill = components
            .iter()
            .any(|component| component.size() == SidebarComponentSize::Fill);
        let mut fixed_budget =
            component_height.saturating_sub(u16::from(has_fill && component_height > 0));
        let fixed_heights = components
            .iter()
            .map(|component| match component.size() {
                SidebarComponentSize::Fixed(rows) => {
                    let height = rows.min(fixed_budget);
                    fixed_budget = fixed_budget.saturating_sub(height);
                    height
                }
                SidebarComponentSize::Fill => 0,
            })
            .collect::<Vec<_>>();
        let fill_rows = area
            .height
            .saturating_sub(u16::try_from(divider_count).unwrap_or(u16::MAX))
            .saturating_sub(fixed_heights.iter().copied().sum::<u16>());
        let mut y = area.y;
        let mut remaining = component_height;
        let mut dividers = Vec::with_capacity(divider_count);
        let components = components
            .iter()
            .enumerate()
            .map(|(index, component)| {
                let wanted = match component.size() {
                    SidebarComponentSize::Fixed(_) => fixed_heights[index],
                    SidebarComponentSize::Fill => fill_rows,
                };
                let height = wanted.min(remaining);
                let geometry = ComponentGeometry {
                    index,
                    area: Rect::new(area.x, y, area.width, height),
                };
                y = y.saturating_add(height);
                remaining = remaining.saturating_sub(height);
                if index < divider_count {
                    dividers.push(Rect::new(area.x, y, area.width, 1));
                    y = y.saturating_add(1);
                }
                geometry
            })
            .collect();
        Self {
            components,
            dividers,
        }
    }

    fn component_at(&self, column: u16, row: u16) -> Option<ComponentGeometry> {
        self.components
            .iter()
            .copied()
            .find(|component| rect_contains(component.area, column, row))
    }
}

fn render_component_dividers(
    geometry: &SidebarGeometry,
    side: SidebarSide,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    let normal = ui.styles.apply(SemanticStyle::Normal, Style::default());
    let style = ui.styles.apply(SemanticStyle::Divider, normal);
    let (horizontal, junction) = match ui.icons.preset {
        IconPreset::Ascii => ("-", "+"),
        IconPreset::Unicode | IconPreset::NerdFont => (
            "─",
            match side {
                SidebarSide::Left => "┤",
                SidebarSide::Right => "├",
            },
        ),
    };
    for divider in &geometry.dividers {
        let Some(content) = sidebar_content(*divider, side) else {
            continue;
        };
        for column in content.x..content.right() {
            if let Some(cell) = buffer.cell_mut((column, divider.y)) {
                cell.set_symbol(horizontal).set_style(style);
            }
        }
        if divider.width > 1 {
            let column = match side {
                SidebarSide::Left => divider.right() - 1,
                SidebarSide::Right => divider.x,
            };
            if let Some(cell) = buffer.cell_mut((column, divider.y)) {
                cell.set_symbol(junction).set_style(style);
            }
        }
    }
}

fn agent_rows(content: Rect, component: &AgentsComponent) -> Vec<(usize, Rect)> {
    let rows = Rect::new(
        content.x,
        content.y.saturating_add(1),
        content.width,
        content.height.saturating_sub(1),
    );
    let anchor = component
        .selected
        .and_then(|terminal_id| {
            component
                .items
                .iter()
                .position(|item| item.terminal_id == terminal_id)
        })
        .or_else(|| component.items.iter().position(|item| item.current))
        .unwrap_or(0);
    let mut y = rows.y;
    let mut geometry = Vec::new();
    for visible in
        visible_rows_with_item_height(component.items.len(), anchor, usize::from(rows.height), 1)
    {
        match visible {
            VisibleRow::Ellipsis => y = y.saturating_add(1),
            VisibleRow::Item(index) => {
                geometry.push((index, Rect::new(rows.x, y, rows.width, 1)));
                y = y.saturating_add(1);
            }
        }
    }
    geometry
}

fn agent_item_at(
    area: Rect,
    side: SidebarSide,
    component: &AgentsComponent,
    column: u16,
    row: u16,
) -> Option<usize> {
    let content = sidebar_content(area, side)?;
    agent_rows(content, component)
        .into_iter()
        .find_map(|(index, area)| rect_contains(area, column, row).then_some(index))
}

fn render_agent_row(
    item: &AgentItem,
    selected: bool,
    spinner_frame: usize,
    area: Rect,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    let row_state = ItemState {
        current: false,
        selected,
        closing: false,
        attention: false,
    };
    let normal = ui.styles.apply(SemanticStyle::Normal, Style::default());
    let detail_style = apply_item_state(&ui.styles, row_state, normal);
    clear(area, detail_style, buffer);
    let title_style = apply_item_state(
        &ui.styles,
        ItemState {
            current: item.current,
            ..row_state
        },
        normal,
    );
    let status_style = ui.styles.apply(item.status_style(), detail_style);
    buffer.set_line(
        area.x,
        area.y,
        &item.line(spinner_frame, "/", title_style, detail_style, status_style),
        area.width,
    );
}

fn render_minimized_agent_row(
    item: &AgentItem,
    index: usize,
    selected: bool,
    spinner_frame: usize,
    area: Rect,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    let state = ItemState {
        current: item.current,
        selected,
        closing: false,
        attention: false,
    };
    let base = apply_item_state(
        &ui.styles,
        state,
        ui.styles.apply(SemanticStyle::Normal, Style::default()),
    );
    clear(area, base, buffer);
    let marker = if item.current { CURRENT_MARKER } else { " " };
    let number = match index {
        0..=8 => (index + 1).to_string(),
        9 => "0".into(),
        _ => "…".into(),
    };
    let activity = item
        .indicator
        .map_or(" ", |indicator| indicator.marker(spinner_frame));
    let activity_style = ui.styles.apply(item.status_style(), base);
    buffer.set_line(
        area.x,
        area.y,
        &Line::from(vec![
            Span::styled(marker, base),
            Span::styled(" ", base),
            Span::styled(number, base),
            Span::styled(activity, activity_style),
            Span::styled(" ", base),
        ]),
        area.width,
    );
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkspaceRowGeometry {
    row: VisibleRow,
    area: Rect,
}

struct WorkspaceGeometry {
    content: Rect,
    header: Line<'static>,
    rows: Rect,
    row_geometries: Vec<WorkspaceRowGeometry>,
    footer: Vec<Line<'static>>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "shared render and hit geometry keeps its resource and animation inputs explicit"
)]
fn workspace_geometry(
    model: &WorkspaceModel,
    selected: Option<WorkspaceId>,
    status: Option<&WorkspaceStatus>,
    help: bool,
    spinner_frame: usize,
    area: Rect,
    position: SidebarSide,
    ui: &UiConfig,
) -> Option<WorkspaceGeometry> {
    let content = sidebar_content(area, position)?;
    let header = render_sidebar_chrome(
        workspace_config(ui, position).header,
        model,
        status,
        spinner_frame,
        position,
        ui,
    );
    let header_height = if header.spans.is_empty() || content.height <= SIDEBAR_HEADER_HEIGHT {
        0
    } else {
        SIDEBAR_HEADER_HEIGHT
    };
    let footer_lines = render_sidebar_footer(model, status, help, spinner_frame, position, ui);
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
    let rows = Rect::new(
        content.x,
        content.y.saturating_add(header_height),
        content.width,
        content.height.saturating_sub(header_height + footer_height),
    );
    let item_height = if workspace_config(ui, position).row.detail.is_empty() {
        1
    } else {
        2
    };
    let anchor = selected
        .and_then(|id| model.items.iter().position(|item| item.id == id))
        .or_else(|| model.items.iter().position(|item| item.current))
        .unwrap_or(0);
    let mut y = rows.y;
    let row_geometries = visible_rows_with_item_height(
        model.items.len(),
        anchor,
        usize::from(rows.height),
        item_height,
    )
    .into_iter()
    .map(|row| {
        let height = match row {
            VisibleRow::Ellipsis => 1,
            VisibleRow::Item(_) => u16::try_from(item_height).unwrap_or(u16::MAX),
        }
        .min(rows.bottom().saturating_sub(y));
        let geometry = WorkspaceRowGeometry {
            row,
            area: Rect::new(rows.x, y, rows.width, height),
        };
        y = y.saturating_add(height);
        geometry
    })
    .collect();
    Some(WorkspaceGeometry {
        content,
        header,
        rows,
        row_geometries,
        footer,
    })
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
    position: SidebarSide,
    ui: &UiConfig,
    column: u16,
    row: u16,
) -> Option<WorkspaceHotkey> {
    if !matches!(status, WorkspaceStatus::Ready) {
        return None;
    }
    let lines = workspace_hotkey_lines(help, position, ui);
    if lines.is_empty() {
        return None;
    }
    let geometry = workspace_geometry(model, None, Some(status), help, 0, area, position, ui)?;
    if !rect_contains(geometry.content, column, row) || geometry.content.height < 5 {
        return None;
    }
    let footer_y = geometry
        .content
        .bottom()
        .saturating_sub(u16::try_from(geometry.footer.len()).unwrap_or(u16::MAX));
    lines
        .get(usize::from(row.checked_sub(footer_y)?))?
        .action_at(usize::from(column - geometry.content.x))
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
    position: SidebarSide,
    ui: &UiConfig,
    column: u16,
    row: u16,
) -> Option<&'a WorkspaceItem> {
    let geometry = workspace_geometry(model, selected, status, false, 0, area, position, ui)?;
    geometry.row_geometries.into_iter().find_map(|geometry| {
        let VisibleRow::Item(index) = geometry.row else {
            return None;
        };
        rect_contains(geometry.area, column, row)
            .then(|| model.items.get(index))
            .flatten()
            .filter(|item| !item.closing)
    })
}

fn sidebar_content(area: Rect, position: SidebarSide) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if area.width == 1 {
        return Some(area);
    }
    Some(match position {
        SidebarSide::Left => Rect::new(area.x, area.y, area.width - 1, area.height),
        SidebarSide::Right => Rect::new(area.x + 1, area.y, area.width - 1, area.height),
    })
}

fn sidebar_is_minimized(area: Rect, side: SidebarSide, ui: &UiConfig) -> bool {
    side.config(ui).display == SidebarDisplay::Minimized && area.width <= MINIMIZED_SIDEBAR_WIDTH
}

#[allow(
    clippy::too_many_arguments,
    reason = "the renderer keeps resource, client, configuration, and target inputs explicit"
)]
pub(super) fn render_sidebar(
    snapshot: Option<&ResourceSnapshot>,
    focused: &SelectedTarget,
    history: &NavigationHistory,
    notifications: &NotificationState,
    spinner_frame: usize,
    area: Rect,
    side: SidebarSide,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    render_sidebar_frame(area, side, ui, buffer);
    let geometry = SidebarGeometry::new(area, &side.config(ui).components);
    for component in &geometry.components {
        match &side.config(ui).components[component.index] {
            SidebarComponentConfig::Workspaces { .. } => render_workspaces_component(
                snapshot,
                focused,
                history,
                notifications,
                spinner_frame,
                component.area,
                side,
                ui,
                buffer,
            ),
            SidebarComponentConfig::Agents { scope, .. } => {
                let agents = snapshot.map_or_else(AgentsComponent::default, |snapshot| {
                    AgentsComponent::open(snapshot, focused, notifications, *scope)
                });
                agents.render(component.area, side, false, spinner_frame, ui, buffer);
            }
        }
    }
    render_component_dividers(&geometry, side, ui, buffer);
}

#[allow(
    clippy::too_many_arguments,
    reason = "the renderer keeps resource, client, configuration, and target inputs explicit"
)]
pub(super) fn render_workspaces_component(
    snapshot: Option<&ResourceSnapshot>,
    focused: &SelectedTarget,
    history: &NavigationHistory,
    notifications: &NotificationState,
    spinner_frame: usize,
    area: Rect,
    position: SidebarSide,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    let model = snapshot
        .map(|snapshot| WorkspaceModel::from_snapshot(snapshot, focused, history, notifications))
        .unwrap_or_default();
    if sidebar_is_minimized(area, position, ui) {
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
    position: SidebarSide,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    let Some(content) = render_sidebar_frame(area, position, ui, buffer) else {
        return;
    };
    let header = render_sidebar_chrome(
        workspace_config(ui, position).header,
        model,
        None,
        spinner_frame,
        position,
        ui,
    );
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
    position: SidebarSide,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    if render_sidebar_frame(area, position, ui, buffer).is_none() {
        return;
    }
    let Some(geometry) = workspace_geometry(
        model,
        selected,
        status,
        help,
        spinner_frame,
        area,
        position,
        ui,
    ) else {
        return;
    };
    render_sidebar_header(
        &geometry.header,
        geometry.content,
        geometry.content.width,
        buffer,
    );
    if help {
        render_help(geometry.rows, ui, buffer);
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
            Rect::new(
                geometry.rows.x,
                geometry.rows.y,
                geometry.rows.width,
                geometry.rows.height.min(2),
            ),
            position,
            ui,
            buffer,
        );
    } else {
        for row in &geometry.row_geometries {
            match row.row {
                VisibleRow::Ellipsis => {
                    buffer.set_stringn(
                        row.area.x,
                        row.area.y,
                        format!(" {}", ui.icons.resolve().overflow),
                        usize::from(row.area.width),
                        ui.styles.apply(
                            SemanticStyle::Muted,
                            ui.styles.apply(SemanticStyle::Normal, Style::default()),
                        ),
                    );
                }
                VisibleRow::Item(index) => {
                    let item = &model.items[index];
                    render_workspace_row(
                        item,
                        selected == Some(item.id),
                        spinner_frame,
                        row.area,
                        position,
                        ui,
                        buffer,
                    );
                }
            }
        }
    }

    let footer_y = geometry
        .content
        .bottom()
        .saturating_sub(u16::try_from(geometry.footer.len()).unwrap_or(u16::MAX));
    for (offset, footer) in geometry.footer.iter().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        buffer.set_line(
            geometry.content.x,
            footer_y.saturating_add(offset),
            &truncate_line(footer, usize::from(geometry.content.width)),
            geometry.content.width,
        );
    }
}

fn render_sidebar_footer(
    model: &WorkspaceModel,
    status: Option<&WorkspaceStatus>,
    help: bool,
    spinner_frame: usize,
    side: SidebarSide,
    ui: &UiConfig,
) -> Vec<Line<'static>> {
    if help
        || matches!(status, Some(WorkspaceStatus::Ready))
            && workspace_config(ui, side).uses_default_footer
    {
        return workspace_hotkey_lines(help, side, ui)
            .into_iter()
            .map(|hotkey| hotkey.line)
            .collect();
    }
    let footer = render_sidebar_chrome(
        workspace_config(ui, side).footer,
        model,
        status,
        spinner_frame,
        side,
        ui,
    );
    (!footer.spans.is_empty())
        .then_some(footer)
        .into_iter()
        .collect()
}

fn workspace_hotkey_lines(
    help: bool,
    side: SidebarSide,
    ui: &UiConfig,
) -> Vec<HotkeyLine<WorkspaceHotkey>> {
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
    if !workspace_config(ui, side).uses_default_footer {
        return Vec::new();
    }
    let nerd_font = ui.icons.preset == IconPreset::NerdFont;
    let visibility_icon = if nerd_font {
        match side.config(ui).visibility {
            SidebarVisibility::Visible => "󰈈",
            SidebarVisibility::Automatic => "󱥼",
            SidebarVisibility::Hidden => "󰈉",
        }
    } else {
        ""
    };
    let display_icon = if nerd_font {
        match side.config(ui).display {
            SidebarDisplay::Expanded => "󰡎",
            SidebarDisplay::Minimized => "󰡌",
        }
    } else {
        ""
    };
    [
        (
            HotkeyButton::new(
                "h",
                side.config(ui).visibility.label(),
                WorkspaceHotkey::CycleVisibility,
            ),
            visibility_icon,
        ),
        (
            HotkeyButton::new(
                "m",
                side.config(ui).display.label(),
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
    position: SidebarSide,
    ui: &UiConfig,
    buffer: &mut Buffer,
) -> Option<Rect> {
    let content = sidebar_content(area, position)?;
    clear(
        area,
        ui.styles.apply(SemanticStyle::Normal, Style::default()),
        buffer,
    );
    let divider_x = match (position, area.width) {
        (_, 1) => None,
        (SidebarSide::Left, _) => Some(area.right() - 1),
        (SidebarSide::Right, _) => Some(area.x),
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
    spinner_frame: usize,
    side: SidebarSide,
    ui: &UiConfig,
) -> ratatui::text::Line<'static> {
    let current = model.items.iter().find(|item| item.current);
    let icons = ui.icons.resolve();
    let display = side.config(ui).display.label();
    let visibility = side.config(ui).visibility.label();
    render_token_segments(
        segments,
        None,
        ItemState::default(),
        &ui.styles,
        &icons,
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
            _ => extension_token_value(ui, token, model.extension_value(token), spinner_frame),
        },
    )
}

fn render_workspace_row(
    item: &WorkspaceItem,
    selected: bool,
    spinner_frame: usize,
    area: Rect,
    side: SidebarSide,
    ui: &UiConfig,
    buffer: &mut Buffer,
) {
    if area.width == 0 {
        return;
    }
    let icons = ui.icons.resolve();
    let state = ItemState {
        // Keyboard selection and closing own the whole row. Current styling is
        // limited to the workspace index and name so status colors stay visible.
        current: false,
        selected,
        closing: item.closing,
        attention: false,
    };
    let surface = ui.styles.apply(SemanticStyle::Normal, Style::default());
    let row_style = apply_item_state(&ui.styles, state, surface);
    clear(area, row_style, buffer);
    let resolve = |token: &str| match token {
        "workspace.index" if item.current => {
            TokenValue::styled((item.index + 1).to_string(), SemanticStyle::Current)
        }
        "workspace.index" => TokenValue::plain((item.index + 1).to_string()),
        "workspace.name" if item.current => {
            TokenValue::styled(sanitize(&item.name), SemanticStyle::Current)
        }
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
        _ => extension_token_value(ui, token, item.token_value(token), spinner_frame),
    };
    let left = render_token_segments(
        &workspace_config(ui, side).row.left,
        None,
        state,
        &ui.styles,
        &icons,
        resolve,
    );
    let body = render_token_segments(
        &workspace_config(ui, side).row.body,
        None,
        state,
        &ui.styles,
        &icons,
        resolve,
    );
    let right = render_token_segments(
        &workspace_config(ui, side).row.right,
        None,
        state,
        &ui.styles,
        &icons,
        resolve,
    );
    let title_width = usize::from(area.width);
    let left_width = left.width().min(title_width);
    let right_width = right.width().min(title_width.saturating_sub(left_width));
    let body_width = title_width.saturating_sub(left_width + right_width);
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
        area.x.saturating_add((title_width - right_width) as u16),
        area.y,
        &truncate_line(&right, right_width),
        right_width as u16,
    );
    if area.height > 1 && !workspace_config(ui, side).row.detail.is_empty() {
        let detail = render_token_segments(
            &workspace_config(ui, side).row.detail,
            None,
            state,
            &ui.styles,
            &icons,
            resolve,
        );
        buffer.set_line(
            area.x,
            area.y.saturating_add(1),
            &truncate_line(&detail, usize::from(area.width)),
            area.width,
        );
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
    use std::path::{Path, PathBuf};

    use crossterm::event::KeyModifiers;
    use ratatui::style::Modifier;

    use super::*;
    use crate::{
        client::config::SidebarRowConfig,
        domain::{
            AgentActivity, AgentDetection, AgentEvent, AgentIntegration, AgentReport, AgentState,
            PaneId, SessionId, TabId, TerminalId,
        },
        extensions,
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
                    trusted_project_config: None,
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

    fn integrate(pane: &mut PaneSnapshot, source: &str, state: AgentState) {
        pane.activity = AgentActivity {
            integration: Some(AgentIntegration {
                source: Some(source.into()),
                ..AgentIntegration::default()
            }),
            detection: None,
            state,
            revision: 1,
            updated_at_ms: 1,
            last_event: None,
            read_revision: 0,
        };
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
    fn workspace_row_renders_manifest_spinner_frames() {
        let (mut snapshot, focused) = fixture(&["main"], 0);
        snapshot.sessions[0].workspaces[0].tokens.insert(
            "workspace.extension.run.launching".into(),
            "populated".into(),
        );
        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        let mut ui = UiConfig::default();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions/run");
        ui.extensions = extensions::load(&[root]).unwrap();
        let SidebarComponentConfig::Workspaces { row, .. } = &mut ui.sidebar.left.components[0]
        else {
            panic!("default left sidebar should contain workspaces");
        };
        row.left.clear();
        row.body = vec![super::super::config::SegmentConfig::Token {
            token: "workspace.extension.run.launching".into(),
            style: None,
            prefix: String::new(),
            suffix: String::new(),
            max_width: None,
            visual: super::super::config::TokenVisual::Plain,
        }];
        row.right.clear();
        row.detail.clear();

        let area = Rect::new(0, 0, 2, 1);
        let mut first = Buffer::empty(area);
        render_workspace_row(
            &model.items[0],
            false,
            0,
            area,
            SidebarSide::Left,
            &ui,
            &mut first,
        );
        let mut second = Buffer::empty(area);
        render_workspace_row(
            &model.items[0],
            false,
            1,
            area,
            SidebarSide::Left,
            &ui,
            &mut second,
        );
        assert_eq!(first[(0, 0)].symbol(), "⠋");
        assert_eq!(second[(0, 0)].symbol(), "⠙");
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

        let (text, buffer) = rendered(&model, None, None, 28, 6, SidebarSide::Left);
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
        position: SidebarSide,
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
        let mut state = WorkspacesComponent::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        let feature_pane = snapshot.sessions[0].workspaces[1].tabs[0].panes[0].id;
        let area = Rect::new(0, 0, 28, 24);
        assert_eq!(
            state.passive_click(area, SidebarSide::Left, &UiConfig::default(), 5, 5,),
            ComponentEffect::Navigate(
                feature_pane,
                NavigationScope::Workspace,
                SidebarComponentKind::Workspaces,
            )
        );

        assert_eq!(
            state.click(area, SidebarSide::Left, &UiConfig::default(), 5, 21,),
            ComponentEffect::CycleVisibility
        );
        assert_eq!(
            state.click(area, SidebarSide::Left, &UiConfig::default(), 5, 22,),
            ComponentEffect::ToggleDisplay
        );
        assert_eq!(
            state.click(area, SidebarSide::Left, &UiConfig::default(), 5, 23,),
            ComponentEffect::Stay
        );
        assert!(state.help);
        assert_eq!(
            state.click(area, SidebarSide::Left, &UiConfig::default(), 2, 23,),
            ComponentEffect::Stay
        );
        assert!(!state.help);

        let mut minimized = UiConfig::default();
        minimized.sidebar.left.display = SidebarDisplay::Minimized;
        assert_eq!(
            state.passive_click(Rect::new(0, 0, 5, 24), SidebarSide::Left, &minimized, 2, 4,),
            ComponentEffect::Navigate(
                feature_pane,
                NavigationScope::Workspace,
                SidebarComponentKind::Workspaces,
            )
        );
    }

    #[test]
    fn selection_wraps_skips_closing_rows_and_switching_blocks_input_until_error() {
        let (mut snapshot, focused) = fixture(&["main", "retiring", "feature"], 0);
        snapshot.sessions[0].workspaces[1].closing = true;
        let history = NavigationHistory::default();
        let mut state =
            WorkspacesComponent::open(&snapshot, &focused, &history, &NotificationState::default());
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            ComponentEffect::CreateWorkspace
        );
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            ComponentEffect::RenameWorkspace(_, ref name) if name == "main"
        ));
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            ComponentEffect::CycleVisibility
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            ComponentEffect::ToggleDisplay
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
            ComponentEffect::Stay
        );
        assert!(state.help, "? opens the hotkey help");
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            ComponentEffect::Stay,
            "any key leaves help without acting"
        );
        assert!(!state.help);
        let feature_pane = snapshot.sessions[0].workspaces[2].tabs[0].panes[0].id;
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)),
            ComponentEffect::Navigate(
                feature_pane,
                NavigationScope::Workspace,
                SidebarComponentKind::Workspaces,
            )
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
            ComponentEffect::Stay,
            "a closing workspace has no destination"
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
            ComponentEffect::CloseSidebar,
            "the current workspace closes the sidebar"
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)),
            ComponentEffect::Stay
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            ComponentEffect::Stay
        );
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ComponentEffect::Navigate(
                feature_pane,
                NavigationScope::Workspace,
                SidebarComponentKind::Workspaces,
            )
        );

        state.begin_switch();
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ComponentEffect::Stay
        );
        state.switch_error("busy".into());
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ComponentEffect::Navigate(
                feature_pane,
                NavigationScope::Workspace,
                SidebarComponentKind::Workspaces,
            )
        );

        snapshot.sessions[0].workspaces.pop();
        state.accept_resources(&snapshot, &focused, &history, &NotificationState::default());
        assert_eq!(
            state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ComponentEffect::CloseSidebar
        );
    }

    #[test]
    fn passive_render_is_borderless_ordered_and_draws_its_left_slot_divider() {
        let (mut snapshot, focused) = fixture(&["main", "bad\nname", "closing"], 0);
        snapshot.sessions[0].workspaces[2].closing = true;
        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        let (left, left_buffer) = rendered(&model, None, None, 24, 10, SidebarSide::Left);
        assert!(left.lines().next().unwrap().contains("project"));
        assert!(left.contains("main"));
        assert!(left.contains("bad�name"));
        assert!(left.contains("closing"));
        assert!(left.contains('×'));
        assert!(left_buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(left.lines().nth(1).unwrap().trim(), "│");
        assert_eq!(left.lines().nth(2).unwrap().trim(), "│");
        assert_eq!(left_buffer[(0, 3)].symbol(), " ");
        assert_eq!(left_buffer[(1, 3)].symbol(), "1");
        assert_eq!(left_buffer[(2, 3)].symbol(), " ");
        assert_eq!(left_buffer[(22, 7)].symbol(), " ");
        assert!(!left_buffer[(0, 3)].modifier.contains(Modifier::REVERSED));
        assert!((2..7).all(|column| {
            left_buffer[(column, 3)]
                .modifier
                .contains(Modifier::REVERSED)
        }));
        assert!(!left_buffer[(0, 4)].modifier.contains(Modifier::REVERSED));
        assert_eq!(left_buffer[(23, 0)].symbol(), "│");
        assert_eq!(left_buffer[(23, 0)].fg, ratatui::style::Color::DarkGray);
        assert!(
            left.lines().nth(5).unwrap().contains("bad�name"),
            "entries follow each other without spacing"
        );
    }

    #[test]
    fn current_workspace_styles_its_index_and_name() {
        let (snapshot, focused) = fixture(&["main"], 0);
        let model = WorkspaceModel::from_snapshot(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
        );
        let ui = UiConfig::default();
        let area = Rect::new(0, 0, 24, 6);
        let mut buffer = Buffer::empty(area);
        render_model(
            &model,
            None,
            None,
            false,
            0,
            area,
            SidebarSide::Left,
            &ui,
            &mut buffer,
        );

        assert!((1..7).all(|column| buffer[(column, 3)].modifier.contains(Modifier::REVERSED)));
        assert!(!buffer[(0, 3)].modifier.contains(Modifier::REVERSED));
        assert!((7..23).all(|column| !buffer[(column, 3)].modifier.contains(Modifier::REVERSED)));
        assert!((0..23).all(|column| !buffer[(column, 4)].modifier.contains(Modifier::REVERSED)));
    }

    #[test]
    fn current_agent_styles_only_its_source_and_keeps_status_color() {
        let item = AgentItem {
            terminal_id: TerminalId::new(),
            pane_id: PaneId::new(),
            session: "session".into(),
            workspace: "workspace".into(),
            tab: "tab".into(),
            source: "codex".into(),
            current: true,
            indicator: Some(ActivityIndicator::Working),
        };
        let ui = UiConfig::default();
        let area = Rect::new(0, 0, 40, 1);
        let mut buffer = Buffer::empty(area);
        render_agent_row(&item, false, 0, area, &ui, &mut buffer);

        assert!((2..9).all(|column| buffer[(column, 0)].modifier.contains(Modifier::REVERSED)));
        assert!((9..40).all(|column| !buffer[(column, 0)].modifier.contains(Modifier::REVERSED)));
        assert_eq!(
            buffer[(9, 0)].fg,
            ui.styles
                .apply(SemanticStyle::Activity, Style::default())
                .fg
                .expect("activity foreground")
        );
    }

    #[test]
    fn minimized_render_keeps_marker_number_status_padding_and_divider() {
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
        render_minimized_model(&model, 0, area, SidebarSide::Left, &ui, &mut left);
        assert_eq!(left[(0, 0)].symbol(), CURRENT_MARKER);
        assert_eq!(left[(1, 0)].symbol(), " ");
        assert_eq!(left[(2, 0)].symbol(), "1");
        assert_eq!(left[(3, 1)].symbol(), "!");
        assert_eq!(left[(4, 1)].symbol(), " ");
        assert_eq!(left[(5, 1)].symbol(), "│");

        let area = Rect::new(0, 0, 6, 5);
        let mut headed = Buffer::empty(area);
        render_minimized_model(&model, 0, area, SidebarSide::Left, &ui, &mut headed);
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
            let (_, buffer) = rendered(&model, None, None, width, 3, SidebarSide::Left);
            if width >= 7 {
                assert!(
                    buffer
                        .content()
                        .iter()
                        .any(|cell| cell.modifier.contains(Modifier::REVERSED)),
                    "width {width}"
                );
            }
        }
    }

    #[test]
    fn active_render_marks_selection_and_lists_footer_hotkeys_on_separate_lines() {
        let (snapshot, focused) = fixture(&["main", "feature"], 0);
        let history = NavigationHistory::default();
        let mut state =
            WorkspacesComponent::open(&snapshot, &focused, &history, &NotificationState::default());
        state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let (ready, buffer) = rendered(
            &state.model,
            state.selected,
            Some(&state.status),
            24,
            9,
            SidebarSide::Left,
        );
        assert!(ready.contains("automatic"));
        let lines = ready.lines().collect::<Vec<_>>();
        assert!(lines[6].contains("h  automatic"));
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
            SidebarSide::Left,
        );
        assert!(switching.contains("switching…"));

        state.switch_error("destination busy".into());
        let (tiny_error, _) = rendered(
            &state.model,
            state.selected,
            Some(&state.status),
            24,
            1,
            SidebarSide::Left,
        );
        assert!(tiny_error.contains("destination busy"));
    }

    #[test]
    fn nerd_font_footer_uses_stateful_visibility_and_display_icons() {
        let mut ui: UiConfig = toml::from_str("[icons]\npreset = 'nerd_font'\n").unwrap();
        let lines = workspace_hotkey_lines(false, SidebarSide::Left, &ui);
        assert_eq!(lines[0].line.spans[1].content, "󱥼  ");
        assert_eq!(lines[1].line.spans[1].content, "󰡎  ");
        assert_eq!(lines[2].line.spans[1].content, "󰘥  ");

        ui.sidebar.left.visibility = SidebarVisibility::Hidden;
        ui.sidebar.left.display = SidebarDisplay::Minimized;
        let lines = workspace_hotkey_lines(false, SidebarSide::Left, &ui);
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
        let ui: UiConfig = toml::from_str(
            "[sidebar.left]\ncomponents = [{ component = 'workspaces', footer = [{ text = 'FOOTER' }] }]\n",
        )
        .unwrap();
        let area = Rect::new(0, 0, 24, 1);
        let mut buffer = Buffer::empty(area);
        render_model(
            &model,
            None,
            None,
            false,
            0,
            area,
            SidebarSide::Left,
            &ui,
            &mut buffer,
        );
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(text.contains("main"));
        assert!(!text.contains("FOOTER"));
    }

    #[test]
    fn agents_are_session_scoped_and_exclude_every_closing_ancestor() {
        let (mut snapshot, focused) = fixture(&["one", "two"], 0);
        for workspace in &mut snapshot.sessions[0].workspaces {
            integrate(&mut workspace.tabs[0].panes[0], "codex", AgentState::Idle);
        }
        let mut other_session = snapshot.sessions[0].clone();
        other_session.id = SessionId::new();
        other_session.name = "other".into();
        snapshot.sessions.push(other_session);

        let agents = AgentsComponent::open(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Session,
        );
        assert_eq!(agents.items.len(), 2, "other sessions are excluded");
        assert!(agents.items.iter().all(|item| item.session == "project"));
        snapshot.sessions.pop();

        snapshot.sessions[0].workspaces[0].tabs[0].panes[0].closing = true;
        snapshot.sessions[0].workspaces[1].tabs[0].closing = true;
        let agents = AgentsComponent::open(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Session,
        );
        assert!(agents.items.is_empty());

        snapshot.sessions[0].workspaces[0].tabs[0].panes[0].closing = false;
        snapshot.sessions[0].workspaces[1].tabs[0].closing = false;
        snapshot.sessions[0].workspaces[0].closing = true;
        snapshot.sessions[0].closing = true;
        let agents = AgentsComponent::open(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Session,
        );
        assert!(agents.items.is_empty());
    }

    #[test]
    fn screen_detected_codex_is_visible_before_its_first_lifecycle_report() {
        let (mut snapshot, focused) = fixture(&["main"], 0);
        let pane = &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0];
        pane.activity.detection = Some(AgentDetection {
            agent: "codex".into(),
            rule: "idle_fallback".into(),
        });

        let agents = AgentsComponent::open(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Global,
        );

        assert_eq!(agents.items.len(), 1);
        assert_eq!(agents.items[0].source, "codex");
        assert_eq!(agents.items[0].status(), "idle");
    }

    #[test]
    fn agents_support_every_scope_from_fresh_ancestry_and_global_navigation() {
        let (mut snapshot, mut focused) = fixture(&["current", "session-peer"], 0);
        integrate(
            &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0],
            "codex",
            AgentState::Idle,
        );

        let mut workspace_tab = snapshot.sessions[0].workspaces[0].tabs[0].clone();
        workspace_tab.id = TabId::new();
        workspace_tab.panes[0].id = PaneId::new();
        workspace_tab.panes[0].terminal_id = TerminalId::new();
        integrate(&mut workspace_tab.panes[0], "claude", AgentState::Working);
        snapshot.sessions[0].workspaces[0].tabs.push(workspace_tab);
        integrate(
            &mut snapshot.sessions[0].workspaces[1].tabs[0].panes[0],
            "codex",
            AgentState::Blocked,
        );

        let mut other_session = snapshot.sessions[0].clone();
        other_session.id = SessionId::new();
        other_session.name = "global-peer".into();
        other_session.workspaces.truncate(1);
        other_session.workspaces[0].id = WorkspaceId::new();
        other_session.workspaces[0].tabs.truncate(1);
        other_session.workspaces[0].tabs[0].id = TabId::new();
        other_session.workspaces[0].tabs[0].panes[0].id = PaneId::new();
        other_session.workspaces[0].tabs[0].panes[0].terminal_id = TerminalId::new();
        let global_pane = other_session.workspaces[0].tabs[0].panes[0].id;
        snapshot.sessions.push(other_session);

        // SelectedTarget ancestry can lag behind a pane move. The pane ID in the
        // fresh snapshot remains authoritative for every configured scope.
        focused.session_id = SessionId::new();
        focused.workspace_id = WorkspaceId::new();
        focused.tab_id = TabId::new();
        for (scope, expected) in [
            (AgentScope::Tab, 1),
            (AgentScope::Workspace, 2),
            (AgentScope::Session, 3),
            (AgentScope::Global, 4),
        ] {
            let agents =
                AgentsComponent::open(&snapshot, &focused, &NotificationState::default(), scope);
            assert_eq!(agents.items.len(), expected, "{scope:?}");
        }

        let mut global = AgentsComponent::open(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Global,
        );
        global.selected = global
            .items
            .iter()
            .find(|item| item.pane_id == global_pane)
            .map(|item| item.terminal_id);
        assert_eq!(
            global.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ComponentEffect::Navigate(
                global_pane,
                NavigationScope::Global,
                SidebarComponentKind::Agents,
            )
        );
    }

    #[test]
    fn agent_scopes_fall_back_to_selected_ids_and_global_needs_no_focus_anchor() {
        let (mut snapshot, mut focused) = fixture(&["current", "peer", "third"], 0);
        let tab = &mut snapshot.sessions[0].workspaces[0].tabs[0];
        integrate(&mut tab.panes[0], "closing", AgentState::Idle);
        tab.panes[0].closing = true;
        let mut live = tab.panes[0].clone();
        live.id = PaneId::new();
        live.terminal_id = TerminalId::new();
        live.closing = false;
        integrate(&mut live, "live", AgentState::Working);
        tab.panes.push(live);

        for scope in [
            AgentScope::Tab,
            AgentScope::Workspace,
            AgentScope::Session,
            AgentScope::Global,
        ] {
            let agents =
                AgentsComponent::open(&snapshot, &focused, &NotificationState::default(), scope);
            assert_eq!(agents.items.len(), 1, "{scope:?}");
            assert_eq!(agents.items[0].source, "live");
        }

        focused.session_id = SessionId::new();
        focused.workspace_id = WorkspaceId::new();
        focused.tab_id = TabId::new();
        focused.pane_id = PaneId::new();
        let global = AgentsComponent::open(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Global,
        );
        assert_eq!(
            global.items.len(),
            1,
            "global has no focus-anchor requirement"
        );

        let mut ui = UiConfig::default();
        ui.sidebar.right.components = vec![SidebarComponentConfig::Agents {
            size: SidebarComponentSize::Fill,
            scope: AgentScope::Global,
        }];
        assert!(slot_relevant(&snapshot, &focused, SidebarSide::Right, &ui));

        let (mut workspaces, workspace_focus) = fixture(&["one", "two", "three"], 0);
        workspaces.sessions[0].workspaces[0].tabs[0].panes[0].closing = true;
        assert!(slot_relevant(
            &workspaces,
            &workspace_focus,
            SidebarSide::Left,
            &UiConfig::default(),
        ));
    }

    #[test]
    fn right_sidebar_frame_and_agent_hits_use_the_inner_left_divider() {
        let (mut snapshot, focused) = fixture(&["current"], 0);
        let pane = &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0];
        integrate(pane, "codex", AgentState::Idle);
        let pane_id = pane.id;
        let agents = AgentsComponent::open(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Session,
        );
        let area = Rect::new(10, 2, 28, 4);
        let mut buffer = Buffer::empty(area);
        agents.render(
            area,
            SidebarSide::Right,
            false,
            0,
            &UiConfig::default(),
            &mut buffer,
        );
        assert_eq!(buffer[(10, 2)].symbol(), "│");
        assert_eq!(buffer[(11, 2)].symbol(), " ");
        assert_eq!(
            agents.passive_click(area, SidebarSide::Right, 11, 3),
            ComponentEffect::Navigate(
                pane_id,
                NavigationScope::Session,
                SidebarComponentKind::Agents,
            )
        );
        assert_eq!(
            agents.passive_click(area, SidebarSide::Right, 10, 3),
            ComponentEffect::Stay,
            "the divider is not component content"
        );
    }

    #[test]
    fn minimized_agents_render_a_stable_compact_rail_and_hit_on_both_sides() {
        let (mut snapshot, focused) = fixture(&["current", "working"], 0);
        integrate(
            &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0],
            "codex",
            AgentState::Idle,
        );
        integrate(
            &mut snapshot.sessions[0].workspaces[1].tabs[0].panes[0],
            "claude",
            AgentState::Working,
        );
        let working_pane = snapshot.sessions[0].workspaces[1].tabs[0].panes[0].id;
        let agents = AgentsComponent::open(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Session,
        );
        let mut ui = UiConfig::default();
        ui.sidebar.left.display = SidebarDisplay::Minimized;
        ui.sidebar.right.display = SidebarDisplay::Minimized;

        for (side, area, divider_x, content_x) in [
            (SidebarSide::Left, Rect::new(0, 0, 6, 4), 5, 0),
            (SidebarSide::Right, Rect::new(10, 0, 6, 4), 10, 11),
        ] {
            let mut buffer = Buffer::empty(area);
            agents.render(area, side, false, 0, &ui, &mut buffer);
            assert_eq!(buffer[(divider_x, 0)].symbol(), "│");
            assert_eq!(buffer[(content_x, 1)].symbol(), CURRENT_MARKER);
            assert_eq!(buffer[(content_x + 2, 1)].symbol(), "1");
            assert_eq!(buffer[(content_x + 2, 2)].symbol(), "2");
            assert_eq!(buffer[(content_x + 3, 2)].symbol(), "⠋");
            assert_eq!(
                agents.passive_click(area, side, content_x + 1, 2),
                ComponentEffect::Navigate(
                    working_pane,
                    NavigationScope::Session,
                    SidebarComponentKind::Agents,
                )
            );
        }

        let drawer = Rect::new(10, 0, 30, 4);
        let mut buffer = Buffer::empty(drawer);
        agents.render(drawer, SidebarSide::Right, true, 0, &ui, &mut buffer);
        assert_eq!(buffer[(11, 0)].symbol(), " ");
        assert_eq!(buffer[(12, 0)].symbol(), "A");
        assert_eq!(buffer[(13, 0)].symbol(), "g");
    }

    #[test]
    fn agents_reuse_activity_and_shared_unread_completion_indicators() {
        let (mut snapshot, focused) = fixture(&["idle", "working", "blocked", "done"], 0);
        let states = [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Idle,
        ];
        for (workspace, state) in snapshot.sessions[0].workspaces.iter_mut().zip(states) {
            integrate(&mut workspace.tabs[0].panes[0], "codex", state);
        }
        let completed = &mut snapshot.sessions[0].workspaces[3].tabs[0].panes[0];
        completed.activity.revision = 7;
        completed.activity.last_event = Some(AgentEvent {
            revision: 7,
            kind: AgentReport::Completed,
            occurred_at_ms: 10,
            turn_id: None,
        });
        let notifications = NotificationState::default();
        let agents =
            AgentsComponent::open(&snapshot, &focused, &notifications, AgentScope::Session);
        assert_eq!(agents.items[0].indicator, None);
        assert_eq!(agents.items[1].indicator, Some(ActivityIndicator::Working));
        assert_eq!(agents.items[2].indicator, Some(ActivityIndicator::Blocked));
        assert_eq!(
            agents.items[3].indicator,
            Some(ActivityIndicator::Completed)
        );
        assert_eq!(agents.items[0].status_style(), SemanticStyle::Muted);
        assert_eq!(agents.items[1].status_style(), SemanticStyle::Activity);
        assert_eq!(agents.items[2].status_style(), SemanticStyle::Error);
        assert_eq!(agents.items[3].status_style(), SemanticStyle::Added);

        snapshot.sessions[0].workspaces[3].tabs[0].panes[0]
            .activity
            .read_revision = 7;
        let agents =
            AgentsComponent::open(&snapshot, &focused, &notifications, AgentScope::Session);
        assert_eq!(agents.items[3].indicator, None);
    }

    #[test]
    fn agent_selection_keeps_terminal_identity_and_navigates_with_fresh_pane_identity() {
        let (mut snapshot, focused) = fixture(&["one", "two"], 0);
        for workspace in &mut snapshot.sessions[0].workspaces {
            integrate(&mut workspace.tabs[0].panes[0], "codex", AgentState::Idle);
        }
        let mut agents = AgentsComponent::open(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Session,
        );
        agents.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let selected_terminal = agents.selected.unwrap();
        let fresh_pane = PaneId::new();
        let pane = &mut snapshot.sessions[0].workspaces[1].tabs[0].panes[0];
        assert_eq!(pane.terminal_id, selected_terminal);
        pane.id = fresh_pane;
        agents.accept_resources(&snapshot, &focused, &NotificationState::default());
        assert_eq!(agents.selected, Some(selected_terminal));
        assert_eq!(
            agents.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ComponentEffect::Navigate(
                fresh_pane,
                NavigationScope::Session,
                SidebarComponentKind::Agents,
            )
        );

        let area = Rect::new(0, 0, 28, 4);
        assert_eq!(
            agents.passive_click(area, SidebarSide::Left, 3, 2),
            ComponentEffect::Navigate(
                fresh_pane,
                NavigationScope::Session,
                SidebarComponentKind::Agents,
            )
        );
    }

    #[test]
    fn sidebar_tab_focus_cycles_components_and_routes_other_keys_only_to_focus() {
        let (mut snapshot, focused) = fixture(&["one", "two"], 0);
        integrate(
            &mut snapshot.sessions[0].workspaces[1].tabs[0].panes[0],
            "codex",
            AgentState::Idle,
        );
        let mut ui = UiConfig::default();
        ui.sidebar
            .left
            .components
            .push(SidebarComponentConfig::Agents {
                size: SidebarComponentSize::Fixed(4),
                scope: AgentScope::Session,
            });
        let mut sidebar = SidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            SidebarSide::Left,
            &ui,
        )
        .unwrap();
        assert_eq!(sidebar.focused_component, 0);
        let area = Rect::new(0, 0, 28, 24);
        assert_eq!(
            sidebar.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), area, &ui,),
            ComponentEffect::Stay
        );
        assert_eq!(sidebar.focused_component, 1);
        let agent_pane = snapshot.sessions[0].workspaces[1].tabs[0].panes[0].id;
        assert_eq!(
            sidebar.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area, &ui,),
            ComponentEffect::Navigate(
                agent_pane,
                NavigationScope::Session,
                SidebarComponentKind::Agents,
            )
        );
        sidebar.key(
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            area,
            &ui,
        );
        assert_eq!(sidebar.focused_component, 0);
        assert_eq!(
            sidebar.key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
                area,
                &ui,
            ),
            ComponentEffect::CreateWorkspace
        );
    }

    #[test]
    fn tab_focus_skips_components_allocated_zero_rows() {
        let (mut snapshot, focused) = fixture(&["one"], 0);
        integrate(
            &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0],
            "codex",
            AgentState::Idle,
        );
        let ui: UiConfig = toml::from_str(
            "[sidebar.left]\ncomponents = [{ component = 'agents', size = 1 }, { component = 'workspaces', size = 1 }]\n",
        )
        .unwrap();
        let mut sidebar = SidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            SidebarSide::Left,
            &ui,
        )
        .unwrap();
        assert_eq!(sidebar.focused_component, 0);
        sidebar.key(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            Rect::new(0, 0, 28, 1),
            &ui,
        );
        assert_eq!(sidebar.focused_component, 0);
    }

    #[test]
    fn only_workspace_navigation_errors_are_kept_inline() {
        let (mut snapshot, focused) = fixture(&["one", "two"], 0);
        integrate(
            &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0],
            "codex",
            AgentState::Idle,
        );
        let mut workspaces = SidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            SidebarSide::Left,
            &UiConfig::default(),
        )
        .unwrap();
        assert!(workspaces.switch_error("busy".into()));
        assert!(matches!(
            workspaces.components[0],
            SidebarComponent::Workspaces(WorkspacesComponent {
                status: WorkspaceStatus::Error(_),
                ..
            })
        ));

        let ui: UiConfig = toml::from_str(
            "[sidebar.right]\ncomponents = [{ component = 'agents', scope = 'workspace' }]\n",
        )
        .unwrap();
        let mut agents = SidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            SidebarSide::Right,
            &ui,
        )
        .unwrap();
        assert!(!agents.switch_error("busy".into()));
    }

    #[test]
    fn unfocused_workspace_click_uses_the_passive_geometry_that_was_rendered() {
        let (mut snapshot, focused) = fixture(&["one", "two", "three"], 0);
        integrate(
            &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0],
            "codex",
            AgentState::Idle,
        );
        let destination = snapshot.sessions[0].workspaces[2].tabs[0].panes[0].id;
        let ui: UiConfig = toml::from_str(
            "[sidebar.left]\ncomponents = [{ component = 'agents', size = 6 }, { component = 'workspaces', size = 'fill' }]\n",
        )
        .unwrap();
        let mut sidebar = SidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            SidebarSide::Left,
            &ui,
        )
        .unwrap();
        assert_eq!(sidebar.focused_component, 0);
        let area = Rect::new(0, 0, 28, 17);
        assert_eq!(
            sidebar.workspace_item_id_at(area, &ui, 2, 2),
            None,
            "agent rows are not workspace context-menu targets"
        );
        assert_eq!(
            sidebar.workspace_item_id_at(area, &ui, 2, 14),
            Some(snapshot.sessions[0].workspaces[2].id)
        );
        assert_eq!(
            sidebar.click(area, &ui, 2, 14),
            ComponentEffect::Navigate(
                destination,
                NavigationScope::Workspace,
                SidebarComponentKind::Workspaces,
            )
        );
        assert_eq!(sidebar.focused_component, 1);
    }

    #[test]
    fn workspace_context_hits_follow_the_configured_right_sidebar_geometry() {
        let (snapshot, focused) = fixture(&["one", "two"], 0);
        let area = Rect::new(20, 0, 28, 16);
        let mut ui = UiConfig::default();
        let agents = SidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            SidebarSide::Right,
            &ui,
        )
        .unwrap();
        assert_eq!(agents.workspace_item_id_at(area, &ui, 22, 3), None);

        ui.sidebar.right.components = ui.sidebar.left.components.clone();
        let workspaces = SidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            SidebarSide::Right,
            &ui,
        )
        .unwrap();
        assert_eq!(
            workspaces.workspace_item_id_at(area, &ui, 22, 3),
            Some(snapshot.sessions[0].workspaces[0].id)
        );

        ui.sidebar.right.display = SidebarDisplay::Minimized;
        assert_eq!(
            workspaces.workspace_item_id_at(area, &ui, 47, 3),
            Some(snapshot.sessions[0].workspaces[0].id)
        );
    }

    #[test]
    fn complete_sidebar_frame_is_painted_when_fixed_components_leave_space() {
        let (mut snapshot, focused) = fixture(&["one"], 0);
        integrate(
            &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0],
            "codex",
            AgentState::Idle,
        );
        let ui: UiConfig = toml::from_str(
            "[styles.normal]\nbackground = 'blue'\n[sidebar.left]\ncomponents = [{ component = 'agents', size = 2 }]\n",
        )
        .unwrap();
        let area = Rect::new(0, 0, 6, 6);

        let mut passive = Buffer::empty(area);
        render_sidebar(
            Some(&snapshot),
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            0,
            area,
            SidebarSide::Left,
            &ui,
            &mut passive,
        );
        let sidebar = SidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            SidebarSide::Left,
            &ui,
        )
        .unwrap();
        let mut active = Buffer::empty(area);
        sidebar.render(area, &ui, 0, &mut active);

        for buffer in [&passive, &active] {
            for row in 0..area.height {
                assert_eq!(buffer[(5, row)].symbol(), "│");
            }
            assert_eq!(buffer[(2, 5)].bg, ratatui::style::Color::Blue);
        }
    }

    #[test]
    fn sidebar_geometry_allocates_fixed_and_fill_once_for_render_and_hits() {
        let components = vec![
            SidebarComponentConfig::Agents {
                size: SidebarComponentSize::Fixed(3),
                scope: AgentScope::Session,
            },
            SidebarComponentConfig::Workspaces {
                size: SidebarComponentSize::Fill,
                header: Vec::new(),
                footer: Vec::new(),
                row: SidebarRowConfig::default(),
            },
        ];
        let geometry = SidebarGeometry::new(Rect::new(4, 5, 20, 8), &components);
        assert_eq!(geometry.components[0].area, Rect::new(4, 5, 20, 3));
        assert_eq!(geometry.dividers, vec![Rect::new(4, 8, 20, 1)]);
        assert_eq!(geometry.components[1].area, Rect::new(4, 9, 20, 4));
        assert_eq!(geometry.component_at(7, 7).unwrap().index, 0);
        assert!(geometry.component_at(7, 8).is_none());
        assert_eq!(geometry.component_at(7, 9).unwrap().index, 1);

        let tiny = SidebarGeometry::new(Rect::new(0, 0, 20, 2), &components);
        assert_eq!(tiny.components[0].area.height, 1);
        assert_eq!(tiny.components[1].area.height, 1);
        assert!(tiny.dividers.is_empty());
    }

    #[test]
    fn passive_and_active_sidebars_draw_dividers_between_components() {
        let (mut snapshot, focused) = fixture(&["one", "two"], 0);
        integrate(
            &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0],
            "codex",
            AgentState::Idle,
        );
        let ui: UiConfig = toml::from_str(
            "[sidebar.left]\ncomponents = [{ component = 'agents', size = 3 }, { component = 'workspaces', size = 'fill' }]\n",
        )
        .unwrap();
        let area = Rect::new(0, 0, 10, 8);
        let sidebar = SidebarState::open(
            &snapshot,
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            SidebarSide::Left,
            &ui,
        )
        .unwrap();
        let mut passive = Buffer::empty(area);
        render_sidebar(
            Some(&snapshot),
            &focused,
            &NavigationHistory::default(),
            &NotificationState::default(),
            0,
            area,
            SidebarSide::Left,
            &ui,
            &mut passive,
        );
        let mut active = Buffer::empty(area);
        sidebar.render(area, &ui, 0, &mut active);

        for buffer in [&passive, &active] {
            for column in 0..9 {
                assert_eq!(buffer[(column, 3)].symbol(), "─");
            }
            assert_eq!(buffer[(9, 3)].symbol(), "┤");
        }
    }
}
