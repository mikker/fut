use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    domain::TabId,
    protocol::{ClientPresenceSnapshot, SelectedTarget},
    resources::{MaterializedTokenMap, ResourceSnapshot},
};

use super::{
    config::{
        GroupConfig, MINIMIZED_SIDEBAR_WIDTH, SegmentConfig, SemanticStyle, SidebarDisplay,
        SidebarVisibility, TabBarPosition, UiConfig,
    },
    hotkey::{HotkeyButton, HotkeyLine},
    notifications::{ActivityIndicator, NotificationState},
    presentation::{
        ItemState, TokenValue, apply_item_state, extension_token_value, pill_cap_style,
        render_token_segments, truncate_line,
    },
    sidebar::{SidebarSide, slot_relevant},
};

pub(super) const MIN_DOCKED_TERMINAL_WIDTH: u16 = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ClientLayout {
    pub terminal: Rect,
    pub tab_bar: Option<Rect>,
    pub left_sidebar: Option<SidebarLayout>,
    pub right_sidebar: Option<SidebarLayout>,
}

impl ClientLayout {
    pub(super) const fn sidebar(self, side: SidebarSide) -> Option<SidebarLayout> {
        match side {
            SidebarSide::Left => self.left_sidebar,
            SidebarSide::Right => self.right_sidebar,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarLayout {
    Docked(Rect),
    Drawer(Rect),
}

impl SidebarLayout {
    pub fn docked(self) -> Option<Rect> {
        match self {
            Self::Docked(area) => Some(area),
            Self::Drawer(_) => None,
        }
    }
}

pub(super) fn client_layout(
    host: Rect,
    ui: &UiConfig,
    sidebar_relevance: SidebarRelevance,
) -> ClientLayout {
    if host.width == 0 || host.height < 2 {
        return ClientLayout {
            terminal: host,
            tab_bar: None,
            left_sidebar: sidebar_drawer(host, ui, SidebarSide::Left).map(SidebarLayout::Drawer),
            right_sidebar: sidebar_drawer(host, ui, SidebarSide::Right).map(SidebarLayout::Drawer),
        };
    }

    let mut available = host.width.saturating_sub(MIN_DOCKED_TERMINAL_WIDTH);
    let mut docked = [None, None];
    for (index, side) in SidebarSide::ALL.into_iter().enumerate() {
        let slot = side.config(ui);
        let width = match slot.display {
            SidebarDisplay::Expanded => slot.width,
            SidebarDisplay::Minimized => MINIMIZED_SIDEBAR_WIDTH,
        };
        let relevant = match side {
            SidebarSide::Left => sidebar_relevance.left,
            SidebarSide::Right => sidebar_relevance.right,
        };
        let needed = match slot.visibility {
            SidebarVisibility::Visible => true,
            SidebarVisibility::Automatic => relevant.unwrap_or(true),
            SidebarVisibility::Hidden => false,
        };
        if needed && width <= available {
            docked[index] = Some(width);
            available -= width;
        }
    }

    let left_width = docked[0].unwrap_or(0);
    let right_width = docked[1].unwrap_or(0);
    let workspace = Rect::new(
        host.x.saturating_add(left_width),
        host.y,
        host.width.saturating_sub(left_width + right_width),
        host.height,
    );
    let docked_left = docked[0].map(|width| Rect::new(host.x, host.y, width, host.height));
    let docked_right = docked[1].map(|width| {
        Rect::new(
            host.right().saturating_sub(width),
            host.y,
            width,
            host.height,
        )
    });

    let (terminal, tab_bar) = match ui.tab_bar.position {
        TabBarPosition::Top => (
            Rect::new(
                workspace.x,
                workspace.y.saturating_add(1),
                workspace.width,
                workspace.height - 1,
            ),
            Some(Rect::new(workspace.x, workspace.y, workspace.width, 1)),
        ),
        TabBarPosition::Bottom => (
            Rect::new(
                workspace.x,
                workspace.y,
                workspace.width,
                workspace.height - 1,
            ),
            Some(Rect::new(
                workspace.x,
                workspace.y.saturating_add(workspace.height - 1),
                workspace.width,
                1,
            )),
        ),
    };

    ClientLayout {
        terminal,
        tab_bar,
        left_sidebar: docked_left
            .map(SidebarLayout::Docked)
            .or_else(|| sidebar_drawer(host, ui, SidebarSide::Left).map(SidebarLayout::Drawer)),
        right_sidebar: docked_right
            .map(SidebarLayout::Docked)
            .or_else(|| sidebar_drawer(host, ui, SidebarSide::Right).map(SidebarLayout::Drawer)),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SidebarRelevance {
    pub left: Option<bool>,
    pub right: Option<bool>,
}

pub(super) fn sidebar_drawer(host: Rect, ui: &UiConfig, side: SidebarSide) -> Option<Rect> {
    sidebar_rect(host, side.config(ui).width, side)
}

fn sidebar_rect(body: Rect, configured_width: u16, side: SidebarSide) -> Option<Rect> {
    if body.width == 0 || body.height == 0 {
        return None;
    }
    let width = body.width.min(configured_width);
    let x = match side {
        SidebarSide::Left => body.x,
        SidebarSide::Right => body.right().saturating_sub(width),
    };
    Some(Rect::new(x, body.y, width, body.height))
}

static NOTIFICATIONS: NotificationState = NotificationState::new();

#[derive(Default)]
pub(super) struct ResourceState {
    snapshot: Option<ResourceSnapshot>,
    presence: ClientPresenceSnapshot,
}

impl ResourceState {
    pub fn accept(&mut self, snapshot: ResourceSnapshot) -> bool {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| snapshot.revision <= current.revision)
        {
            return false;
        }
        self.snapshot = Some(snapshot);
        true
    }

    pub fn snapshot(&self) -> Option<&ResourceSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn accept_presence(&mut self, presence: ClientPresenceSnapshot) -> bool {
        if presence.revision <= self.presence.revision {
            return false;
        }
        self.presence = presence;
        true
    }

    pub fn presence(&self) -> &ClientPresenceSnapshot {
        &self.presence
    }

    pub fn notifications(&self) -> &NotificationState {
        &NOTIFICATIONS
    }

    pub fn attention_revision(&self, terminal_id: crate::domain::TerminalId) -> Option<u64> {
        let pane = self
            .snapshot
            .as_ref()?
            .pane_paths()
            .find(|path| path.pane.terminal_id == terminal_id)?
            .pane;
        if !pane.activity.has_unread_attention() {
            return None;
        }
        pane.activity
            .attention()
            .map(|attention| attention.revision)
    }

    pub fn has_working(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| self.notifications().has_working(snapshot))
    }

    pub fn has_animated_extension_token(&self, ui: &UiConfig) -> bool {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        ui.extensions
            .iter()
            .flat_map(|extension| extension.presentation_tokens())
            .filter(|token| token.presentation() == crate::extensions::TokenPresentation::Spinner)
            .any(|token| {
                let populated = |values: &MaterializedTokenMap| {
                    values
                        .get(token.qualified_name())
                        .is_some_and(|value| !value.is_empty())
                };
                match token.scope() {
                    crate::extensions::PresentationScope::Session => snapshot
                        .sessions
                        .iter()
                        .any(|session| populated(&session.tokens)),
                    crate::extensions::PresentationScope::Workspace => snapshot
                        .sessions
                        .iter()
                        .flat_map(|session| &session.workspaces)
                        .any(|workspace| populated(&workspace.tokens)),
                    crate::extensions::PresentationScope::Tab => snapshot
                        .sessions
                        .iter()
                        .flat_map(|session| &session.workspaces)
                        .flat_map(|workspace| &workspace.tabs)
                        .any(|tab| populated(&tab.tokens)),
                    crate::extensions::PresentationScope::Pane => snapshot
                        .pane_paths()
                        .any(|path| populated(&path.pane.tokens)),
                }
            })
    }

    pub fn sidebar_relevance(&self, focused: &SelectedTarget, ui: &UiConfig) -> SidebarRelevance {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return SidebarRelevance::default();
        };
        SidebarRelevance {
            left: Some(slot_relevant(snapshot, focused, SidebarSide::Left, ui)),
            right: Some(slot_relevant(snapshot, focused, SidebarSide::Right, ui)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TabItem {
    id: TabId,
    name: String,
    closing: bool,
    pane_count: usize,
    tokens: MaterializedTokenMap,
    activity: Option<ActivityIndicator>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TabBarModel {
    session_name: String,
    session_tokens: MaterializedTokenMap,
    workspace_name: String,
    workspace_tokens: MaterializedTokenMap,
    pane_tokens: MaterializedTokenMap,
    tabs: Vec<TabItem>,
    active: usize,
    client_waiting: usize,
    session_waiting: usize,
}

impl TabBarModel {
    fn from_snapshot(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        notifications: &NotificationState,
    ) -> Option<Self> {
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.id == focused.session_id)?;
        let workspace = session
            .workspaces
            .iter()
            .find(|workspace| workspace.id == focused.workspace_id)?;
        let active = workspace
            .tabs
            .iter()
            .position(|tab| tab.id == focused.tab_id)?;
        Some(Self {
            session_name: sanitize(&session.name),
            session_tokens: session.tokens.clone(),
            workspace_name: sanitize(&workspace.name),
            workspace_tokens: workspace.tokens.clone(),
            pane_tokens: workspace.tabs[active]
                .panes
                .iter()
                .find(|pane| pane.id == focused.pane_id)
                .map_or_else(MaterializedTokenMap::new, |pane| pane.tokens.clone()),
            tabs: workspace
                .tabs
                .iter()
                .map(|tab| TabItem {
                    id: tab.id,
                    name: sanitize(&tab.name),
                    closing: tab.closing,
                    pane_count: tab.panes.len(),
                    tokens: tab.tokens.clone(),
                    activity: notifications.indicator(&tab.panes),
                })
                .collect(),
            active,
            client_waiting: notifications.waiting_count(snapshot),
            session_waiting: notifications.session_waiting_count(snapshot, focused.session_id),
        })
    }

    fn extension_value(&self, token: &str) -> &str {
        self.session_tokens
            .get(token)
            .or_else(|| self.workspace_tokens.get(token))
            .or_else(|| self.tabs[self.active].tokens.get(token))
            .or_else(|| self.pane_tokens.get(token))
            .map_or("", String::as_str)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Lane {
    Left,
    Center,
    Right,
}

struct ResolvedGroup {
    lane: Lane,
    line: Line<'static>,
    tabs: bool,
    hotkeys: bool,
    style: Option<SemanticStyle>,
    priority: u8,
    allocation: usize,
}

/// The fully resolved tab-bar layout for one terminal row. Rendering and hit
/// testing both consume this scene so configured lane allocation cannot drift
/// from clickable geometry.
struct TabBarScene {
    groups: Vec<PlacedTabBarGroup>,
}

struct PlacedTabBarGroup {
    x: usize,
    width: usize,
    line: Line<'static>,
    tabs: Vec<VisibleTab>,
    hotkeys: Option<PlacedHotkeys>,
}

#[derive(Clone, Copy)]
struct VisibleTab {
    id: TabId,
    x: usize,
    width: usize,
}

struct PlacedHotkeys {
    x: usize,
    line: HotkeyLine<TabBarHotkey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TabBarHotkey {
    Create,
    Rename,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TabBarHit {
    Item(TabId),
    Hotkey(TabBarHotkey),
}

impl TabBarScene {
    fn build(
        model: &TabBarModel,
        zoomed: bool,
        selected: Option<TabId>,
        spinner_frame: usize,
        ui: &UiConfig,
        width: usize,
    ) -> Self {
        let mut groups = resolved_groups(model, zoomed, selected, spinner_frame, ui);
        allocate_groups(&mut groups, width);
        let lane_width = |lane| {
            groups
                .iter()
                .filter(|group| group.lane == lane)
                .map(|group| group.allocation)
                .sum::<usize>()
        };
        let left_width = lane_width(Lane::Left);
        let center_width = lane_width(Lane::Center);
        let right_width = lane_width(Lane::Right);
        let center_x = width
            .saturating_sub(center_width)
            .checked_div(2)
            .unwrap_or(0)
            .clamp(left_width, width.saturating_sub(right_width + center_width));

        let mut placed = Vec::new();
        for (lane, mut x) in [
            (Lane::Left, 0usize),
            (Lane::Center, center_x),
            (Lane::Right, width.saturating_sub(right_width)),
        ] {
            for group in groups.iter().filter(|group| group.lane == lane) {
                if group.allocation == 0 {
                    continue;
                }
                let (line, tabs) = if group.tabs {
                    let visible = visible_tabs(
                        model,
                        selected,
                        group.allocation,
                        group.style,
                        spinner_frame,
                        ui,
                    );
                    (visible.line, visible.tabs)
                } else {
                    (truncate_line(&group.line, group.allocation), Vec::new())
                };
                let hotkeys = group
                    .hotkeys
                    .then(|| {
                        let hotkeys = tab_bar_hotkeys(group.style, ui);
                        let rendered = group.line.to_string();
                        let needle = hotkeys.line.to_string();
                        rendered.find(&needle).map(|byte_offset| PlacedHotkeys {
                            x: UnicodeWidthStr::width(&rendered[..byte_offset]),
                            line: hotkeys,
                        })
                    })
                    .flatten();
                placed.push(PlacedTabBarGroup {
                    x,
                    width: group.allocation,
                    line,
                    tabs,
                    hotkeys,
                });
                x += group.allocation;
            }
        }
        Self { groups: placed }
    }

    fn hit_at(&self, column: usize) -> Option<TabBarHit> {
        let group = self
            .groups
            .iter()
            .find(|group| column >= group.x && column < group.x + group.width)?;
        let column = column - group.x;
        if let Some(tab) = group
            .tabs
            .iter()
            .find(|tab| column >= tab.x && column < tab.x + tab.width)
        {
            return Some(TabBarHit::Item(tab.id));
        }
        let hotkeys = group.hotkeys.as_ref()?;
        hotkeys
            .line
            .action_at(column.saturating_sub(hotkeys.x))
            .map(TabBarHit::Hotkey)
    }
}

fn tab_bar_model(
    snapshot: Option<&ResourceSnapshot>,
    focused: &SelectedTarget,
    notifications: &NotificationState,
) -> TabBarModel {
    snapshot
        .and_then(|snapshot| TabBarModel::from_snapshot(snapshot, focused, notifications))
        .unwrap_or_else(|| TabBarModel {
            session_name: "session".into(),
            session_tokens: MaterializedTokenMap::new(),
            workspace_name: "workspace".into(),
            workspace_tokens: MaterializedTokenMap::new(),
            pane_tokens: MaterializedTokenMap::new(),
            tabs: vec![TabItem {
                id: focused.tab_id,
                name: "tab".into(),
                closing: false,
                pane_count: 1,
                tokens: MaterializedTokenMap::new(),
                activity: None,
            }],
            active: 0,
            client_waiting: 0,
            session_waiting: 0,
        })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the renderer keeps resource, client, configuration, and target inputs explicit"
)]
pub(super) fn render_tab_bar(
    snapshot: Option<&ResourceSnapshot>,
    focused: &SelectedTarget,
    zoomed: bool,
    selected: Option<TabId>,
    notifications: &NotificationState,
    spinner_frame: usize,
    ui: &UiConfig,
    area: Rect,
    buffer: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    clear_row(
        area,
        ui.styles.apply(SemanticStyle::Normal, Style::default()),
        buffer,
    );
    let model = tab_bar_model(snapshot, focused, notifications);
    let scene = TabBarScene::build(
        &model,
        zoomed,
        selected,
        spinner_frame,
        ui,
        usize::from(area.width),
    );
    for group in scene.groups {
        buffer.set_line(
            area.x
                .saturating_add(u16::try_from(group.x).unwrap_or(u16::MAX)),
            area.y,
            &group.line,
            u16::try_from(group.width).unwrap_or(u16::MAX),
        );
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "hit testing must use the same configurable tab geometry as rendering"
)]
pub(super) fn tab_bar_hit_at(
    snapshot: &ResourceSnapshot,
    focused: &SelectedTarget,
    zoomed: bool,
    selected: Option<TabId>,
    notifications: &NotificationState,
    spinner_frame: usize,
    ui: &UiConfig,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<TabBarHit> {
    if row != area.y || column < area.x || column >= area.right() {
        return None;
    }
    let model = TabBarModel::from_snapshot(snapshot, focused, notifications)?;
    TabBarScene::build(
        &model,
        zoomed,
        selected,
        spinner_frame,
        ui,
        usize::from(area.width),
    )
    .hit_at(usize::from(column - area.x))
}

fn resolved_groups(
    model: &TabBarModel,
    zoomed: bool,
    selected: Option<TabId>,
    spinner_frame: usize,
    ui: &UiConfig,
) -> Vec<ResolvedGroup> {
    let mut groups = Vec::new();
    for (lane, configured) in [
        (Lane::Left, &ui.tab_bar.left),
        (Lane::Center, &ui.tab_bar.center),
        (Lane::Right, &ui.tab_bar.right),
    ] {
        for group in configured {
            let tabs = group
                .segments
                .iter()
                .any(|segment| matches!(segment, SegmentConfig::Tabs));
            let hotkeys = group
                .segments
                .iter()
                .any(|segment| matches!(segment, SegmentConfig::Token { token, .. } if token == "client.help"));
            let line = if tabs {
                selected_line(
                    model,
                    selected,
                    0,
                    model.tabs.len() - 1,
                    group.style,
                    spinner_frame,
                    ui,
                )
            } else {
                render_bar_group(group, model, zoomed, selected, spinner_frame, ui)
            };
            if tabs || line.width() > 0 {
                groups.push(ResolvedGroup {
                    lane,
                    line,
                    tabs,
                    hotkeys,
                    style: group.style,
                    priority: group.priority,
                    allocation: 0,
                });
            }
        }
    }
    groups
}

fn render_bar_group(
    group: &GroupConfig,
    model: &TabBarModel,
    zoomed: bool,
    selected: Option<TabId>,
    spinner_frame: usize,
    ui: &UiConfig,
) -> Line<'static> {
    let icons = ui.icons.resolve();
    let active = &model.tabs[model.active];
    render_token_segments(
        &group.segments,
        group.style,
        ItemState::default(),
        &ui.styles,
        &icons,
        |token| match token {
            "fut" if selected.is_none() => TokenValue::plain("fut "),
            "session.name" => TokenValue::plain(model.session_name.clone()),
            "workspace.name" => TokenValue::plain(model.workspace_name.clone()),
            "workspace.icon" => TokenValue::plain(icons.workspace.clone()),
            "tab.name" => TokenValue::plain(active.name.clone()),
            "tab.index" => TokenValue::plain((model.active + 1).to_string()),
            "tab.pane_count" => TokenValue::plain(active.pane_count.to_string()),
            "client.zoom" if zoomed => TokenValue::plain(icons.zoom.clone()),
            "client.help" if selected.is_some() => {
                TokenValue::plain(tab_bar_hotkeys(group.style, ui).line.to_string())
            }
            "client.waiting" if model.client_waiting > 0 => TokenValue::styled(
                format!("• {}", model.client_waiting),
                SemanticStyle::Attention,
            ),
            "session.waiting" if model.session_waiting > 0 => TokenValue::styled(
                format!("• {}", model.session_waiting),
                SemanticStyle::Attention,
            ),
            "tab.activity" => active.activity.map_or_else(
                || TokenValue::plain(""),
                |activity| activity_token(activity, spinner_frame),
            ),
            _ => extension_token_value(ui, token, model.extension_value(token), spinner_frame),
        },
    )
}

fn tab_bar_hotkeys(group_style: Option<SemanticStyle>, ui: &UiConfig) -> HotkeyLine<TabBarHotkey> {
    let mut style = ui.styles.apply(SemanticStyle::Normal, Style::default());
    if let Some(role) = group_style {
        style = ui.styles.apply(role, style);
    }
    HotkeyLine::inline(
        &[
            HotkeyButton::new("c", "new", TabBarHotkey::Create),
            HotkeyButton::new("r", "rename", TabBarHotkey::Rename),
            HotkeyButton::new("esc", "", TabBarHotkey::Close),
        ],
        "",
        " · ",
        " ",
        style,
        style,
    )
}

fn allocate_groups(groups: &mut [ResolvedGroup], width: usize) {
    let tabs = groups.iter().position(|group| group.tabs);
    let mut remaining = width;
    if let Some(index) = tabs
        && remaining > 0
    {
        groups[index].allocation = 1;
        remaining -= 1;
    }
    let tab_priority = tabs.map_or(0, |index| groups[index].priority);
    let mut indices = (0..groups.len())
        .filter(|index| Some(*index) != tabs)
        .collect::<Vec<_>>();
    indices.sort_by_key(|index| std::cmp::Reverse(groups[*index].priority));
    for index in indices.iter().copied() {
        if groups[index].priority <= tab_priority {
            continue;
        }
        let demand = groups[index].line.width();
        if demand <= remaining {
            groups[index].allocation = demand;
            remaining -= demand;
        }
    }
    if let Some(index) = tabs {
        let growth = groups[index]
            .line
            .width()
            .saturating_sub(groups[index].allocation)
            .min(remaining);
        groups[index].allocation += growth;
        remaining -= growth;
    }
    for index in indices {
        if groups[index].priority > tab_priority {
            continue;
        }
        let demand = groups[index].line.width();
        if demand <= remaining {
            groups[index].allocation = demand;
            remaining -= demand;
        }
    }
}

struct VisibleTabs {
    line: Line<'static>,
    tabs: Vec<VisibleTab>,
}

fn visible_tabs(
    model: &TabBarModel,
    selected: Option<TabId>,
    width: usize,
    component_style: Option<SemanticStyle>,
    spinner_frame: usize,
    ui: &UiConfig,
) -> VisibleTabs {
    if width == 0 {
        return VisibleTabs {
            line: Line::default(),
            tabs: Vec::new(),
        };
    }
    let anchor = selected
        .and_then(|id| model.tabs.iter().position(|tab| tab.id == id))
        .unwrap_or(model.active);
    let mut first = anchor;
    let mut last = anchor;
    let mut line = selected_line(
        model,
        selected,
        first,
        last,
        component_style,
        spinner_frame,
        ui,
    );
    if line.width() > width {
        let fallback =
            render_tab_item_content(model, anchor, selected, component_style, spinner_frame, ui);
        let icons = ui.icons.resolve();
        let marker = tab_token(model, anchor, "tab.index", spinner_frame, ui, &icons);
        if width <= UnicodeWidthStr::width(marker.text.as_str()).saturating_add(2) {
            let mut style = ui.styles.apply(SemanticStyle::Normal, Style::default());
            if let Some(role) = component_style {
                style = ui.styles.apply(role, style);
            }
            let tab = &model.tabs[anchor];
            if anchor == model.active {
                style = ui.styles.apply(SemanticStyle::Current, style);
            }
            if tab.closing {
                style = ui.styles.apply(SemanticStyle::Closing, style);
            }
            if selected == Some(tab.id) {
                style = ui.styles.apply(SemanticStyle::Selected, style);
            }
            return VisibleTabs {
                line: Line::styled(format!("{:^width$}", marker.text), style),
                tabs: vec![VisibleTab {
                    id: tab.id,
                    x: 0,
                    width,
                }],
            };
        }
        return VisibleTabs {
            line: truncate_line(&fallback, width),
            tabs: vec![VisibleTab {
                id: model.tabs[anchor].id,
                x: 0,
                width,
            }],
        };
    }

    loop {
        let mut changed = false;
        if first > 0 {
            let candidate = selected_line(
                model,
                selected,
                first - 1,
                last,
                component_style,
                spinner_frame,
                ui,
            );
            if candidate.width() <= width {
                first -= 1;
                line = candidate;
                changed = true;
            }
        }
        if last + 1 < model.tabs.len() {
            let candidate = selected_line(
                model,
                selected,
                first,
                last + 1,
                component_style,
                spinner_frame,
                ui,
            );
            if candidate.width() <= width {
                last += 1;
                line = candidate;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let overflow_width = format!(" {} ", ui.icons.resolve().overflow).width();
    let mut x = usize::from(first > 0) * overflow_width;
    let mut tabs = Vec::with_capacity(last - first + 1);
    for index in first..=last {
        let item_width =
            render_tab_item(model, index, selected, component_style, spinner_frame, ui).width();
        tabs.push(VisibleTab {
            id: model.tabs[index].id,
            x,
            width: item_width,
        });
        x += item_width;
    }
    VisibleTabs { line, tabs }
}

fn selected_line(
    model: &TabBarModel,
    selected: Option<TabId>,
    first: usize,
    last: usize,
    component_style: Option<SemanticStyle>,
    spinner_frame: usize,
    ui: &UiConfig,
) -> Line<'static> {
    let mut spans = Vec::new();
    let icons = ui.icons.resolve();
    if first > 0 {
        let mut style = ui.styles.apply(SemanticStyle::Normal, Style::default());
        if let Some(role) = component_style {
            style = ui.styles.apply(role, style);
        }
        spans.push(Span::styled(
            format!(" {} ", icons.overflow),
            ui.styles.apply(SemanticStyle::Muted, style),
        ));
    }
    for index in first..=last {
        spans.extend(
            render_tab_item(model, index, selected, component_style, spinner_frame, ui).spans,
        );
    }
    if last + 1 < model.tabs.len() {
        let mut style = ui.styles.apply(SemanticStyle::Normal, Style::default());
        if let Some(role) = component_style {
            style = ui.styles.apply(role, style);
        }
        spans.push(Span::styled(
            format!(" {} ", icons.overflow),
            ui.styles.apply(SemanticStyle::Muted, style),
        ));
    }
    Line::from(spans)
}

fn render_tab_item(
    model: &TabBarModel,
    index: usize,
    selected: Option<TabId>,
    component_style: Option<SemanticStyle>,
    spinner_frame: usize,
    ui: &UiConfig,
) -> Line<'static> {
    let line = render_tab_item_content(model, index, selected, component_style, spinner_frame, ui);
    let icons = ui.icons.resolve();
    if icons.pill_left.is_empty() || icons.pill_right.is_empty() {
        return line;
    }
    let style = tab_item_style(model, index, selected, component_style, ui);
    let mut bar = ui.styles.apply(SemanticStyle::Normal, Style::default());
    if let Some(role) = component_style {
        bar = ui.styles.apply(role, bar);
    }
    let cap = pill_cap_style(style, bar);
    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    spans.extend(line.spans);
    if index == model.active {
        spans.insert(0, Span::styled(icons.pill_left, cap));
        spans.push(Span::styled(icons.pill_right, cap));
    } else {
        // Inactive tabs pad by the cap width so items keep a stable width.
        spans.insert(0, Span::styled(" ", cap));
        spans.push(Span::styled(" ", cap));
    }
    Line::from(spans)
}

fn render_tab_item_content(
    model: &TabBarModel,
    index: usize,
    selected: Option<TabId>,
    component_style: Option<SemanticStyle>,
    spinner_frame: usize,
    ui: &UiConfig,
) -> Line<'static> {
    let tab = &model.tabs[index];
    let icons = ui.icons.resolve();
    render_token_segments(
        &ui.tab_bar.item.segments,
        component_style,
        ItemState {
            current: index == model.active,
            selected: selected == Some(tab.id),
            closing: tab.closing,
            attention: matches!(
                tab.activity,
                Some(ActivityIndicator::Blocked | ActivityIndicator::Completed)
            ),
        },
        &ui.styles,
        &icons,
        |token| tab_token(model, index, token, spinner_frame, ui, &icons),
    )
}

fn tab_item_style(
    model: &TabBarModel,
    index: usize,
    selected: Option<TabId>,
    component_style: Option<SemanticStyle>,
    ui: &UiConfig,
) -> Style {
    let tab = &model.tabs[index];
    let mut style = ui.styles.apply(SemanticStyle::Normal, Style::default());
    if let Some(role) = component_style {
        style = ui.styles.apply(role, style);
    }
    apply_item_state(
        &ui.styles,
        ItemState {
            current: index == model.active,
            selected: selected == Some(tab.id),
            closing: tab.closing,
            attention: matches!(
                tab.activity,
                Some(ActivityIndicator::Blocked | ActivityIndicator::Completed)
            ),
        },
        style,
    )
}

fn tab_token(
    model: &TabBarModel,
    index: usize,
    token: &str,
    spinner_frame: usize,
    ui: &UiConfig,
    icons: &super::config::IconSet,
) -> TokenValue {
    let tab = &model.tabs[index];
    match token {
        "tab.marker" if index == model.active => TokenValue::plain(icons.current.clone()),
        "tab.marker" => TokenValue::plain((index + 1).to_string()),
        "tab.index" => TokenValue::plain((index + 1).to_string()),
        "tab.name" => TokenValue::plain(tab.name.clone()),
        "tab.id" => TokenValue::plain(tab.id.to_string()),
        "tab.closing" if tab.closing => {
            TokenValue::styled(icons.closing.clone(), SemanticStyle::Closing)
        }
        "tab.pane_count" => TokenValue::plain(tab.pane_count.to_string()),
        "tab.icon" => TokenValue::plain(icons.tab.clone()),
        "tab.activity" => tab.activity.map_or_else(
            || TokenValue::plain(""),
            |activity| activity_token(activity, spinner_frame),
        ),
        _ => extension_token_value(
            ui,
            token,
            tab.tokens.get(token).map_or("", String::as_str),
            spinner_frame,
        ),
    }
}

fn activity_token(activity: ActivityIndicator, frame: usize) -> TokenValue {
    let style = match activity {
        ActivityIndicator::Working => SemanticStyle::Activity,
        ActivityIndicator::Blocked | ActivityIndicator::Completed => SemanticStyle::Attention,
    };
    TokenValue::styled(activity.marker(frame), style)
}

pub(super) fn truncate(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }

    let mut used = 0;
    let mut truncated = String::new();
    for grapheme in value.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > width - 1 {
            break;
        }
        truncated.push_str(grapheme);
        used += grapheme_width;
    }
    truncated.push('…');
    truncated
}

pub(super) fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
            {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn clear_row(area: Rect, style: Style, buffer: &mut Buffer) {
    for column in area.x..area.x.saturating_add(area.width) {
        if let Some(cell) = buffer.cell_mut((column, area.y)) {
            cell.reset();
            cell.set_style(style);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::{
        domain::{AgentIntegration, PaneId, SessionId, TabId, TerminalId, WorkspaceId},
        extensions,
        resources::{
            PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
        },
    };
    use ratatui::style::Modifier;

    fn fixture(names: &[&str], active: usize) -> (ResourceSnapshot, SelectedTarget) {
        let session_id = SessionId::new();
        let workspace_id = WorkspaceId::new();
        let tabs = names
            .iter()
            .map(|name| {
                let pane_id = PaneId::new();
                TabSnapshot {
                    tokens: Default::default(),
                    id: TabId::new(),
                    name: (*name).into(),
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
                }
            })
            .collect::<Vec<_>>();
        let selected_tab = &tabs[active];
        let selected_pane = selected_tab.panes[0].clone();
        let target = SelectedTarget {
            session_id,
            workspace_id,
            tab_id: selected_tab.id,
            pane_id: selected_pane.id,
            terminal_id: selected_pane.terminal_id,
            child_pid: 1,
        };
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
                    workspaces: vec![WorkspaceSnapshot {
                        tokens: Default::default(),
                        id: workspace_id,
                        name: "main".into(),
                        root: PathBuf::from("/project"),
                        closing: false,
                        tabs,
                    }],
                }],
            },
            target,
        )
    }

    fn run_extension_ui() -> UiConfig {
        let mut ui = UiConfig::default();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/extensions/run");
        ui.extensions = extensions::load(&[root]).unwrap();
        ui
    }

    #[test]
    fn populated_spinner_token_animates_in_bar_and_keeps_redraw_clock_alive() {
        let (mut snapshot, focused) = fixture(&["shell"], 0);
        snapshot.sessions[0].workspaces[0].tokens.insert(
            "workspace.extension.run.launching".into(),
            "populated".into(),
        );
        let mut ui = run_extension_ui();
        ui.icons.preset = super::super::config::IconPreset::NerdFont;
        let model =
            TabBarModel::from_snapshot(&snapshot, &focused, &NotificationState::default()).unwrap();
        let group = GroupConfig {
            segments: vec![super::super::config::SegmentConfig::Token {
                token: "workspace.extension.run.launching".into(),
                style: Some(SemanticStyle::Attention),
                prefix: String::new(),
                suffix: String::new(),
                max_width: None,
                visual: super::super::config::TokenVisual::Pill,
            }],
            ..Default::default()
        };
        assert_eq!(
            render_bar_group(&group, &model, false, None, 0, &ui).to_string(),
            "\u{e0b6}⠋\u{e0b4}"
        );
        assert_eq!(
            render_bar_group(&group, &model, false, None, 1, &ui).to_string(),
            "\u{e0b6}⠙\u{e0b4}"
        );

        let mut resources = ResourceState::default();
        assert!(resources.accept(snapshot.clone()));
        assert!(resources.has_animated_extension_token(&ui));
        snapshot.revision += 1;
        snapshot.sessions[0].workspaces[0]
            .tokens
            .insert("workspace.extension.run.launching".into(), String::new());
        snapshot.sessions[0].workspaces[0]
            .tokens
            .insert("workspace.extension.run.play".into(), "spinner".into());
        assert!(resources.accept(snapshot));
        assert!(!resources.has_animated_extension_token(&ui));
    }

    #[test]
    fn extension_tokens_resolve_from_current_bar_ancestry_and_tab_items() {
        let (mut snapshot, focused) = fixture(&["shell", "peer"], 0);
        let session = &mut snapshot.sessions[0];
        session
            .tokens
            .insert("session.extension.demo.value".into(), "S".into());
        let workspace = &mut session.workspaces[0];
        workspace
            .tokens
            .insert("workspace.extension.demo.value".into(), "W".into());
        let tab = &mut workspace.tabs[0];
        tab.tokens
            .insert("tab.extension.demo.value".into(), "T".into());
        tab.panes[0]
            .tokens
            .insert("pane.extension.demo.value".into(), "P".into());
        let model =
            TabBarModel::from_snapshot(&snapshot, &focused, &NotificationState::default()).unwrap();
        let group = GroupConfig {
            segments: [
                "session.extension.demo.value",
                "workspace.extension.demo.value",
                "tab.extension.demo.value",
                "pane.extension.demo.value",
            ]
            .into_iter()
            .map(|token| super::super::config::SegmentConfig::Token {
                token: token.into(),
                style: None,
                prefix: String::new(),
                suffix: String::new(),
                max_width: None,
                visual: super::super::config::TokenVisual::Plain,
            })
            .collect(),
            ..Default::default()
        };
        assert_eq!(
            render_bar_group(&group, &model, false, None, 0, &UiConfig::default()).to_string(),
            "SWTP"
        );
        assert_eq!(
            tab_token(
                &model,
                0,
                "tab.extension.demo.value",
                0,
                &UiConfig::default(),
                &UiConfig::default().icons.resolve(),
            )
            .text,
            "T"
        );
        assert_eq!(
            tab_token(
                &model,
                1,
                "tab.extension.demo.value",
                0,
                &UiConfig::default(),
                &UiConfig::default().icons.resolve(),
            )
            .text,
            ""
        );
    }

    fn render(names: &[&str], active: usize, width: u16) -> (String, Buffer) {
        let (snapshot, focused) = fixture(names, active);
        let area = Rect::new(0, 0, width, 1);
        let mut buffer = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &focused,
            false,
            None,
            &NotificationState::default(),
            0,
            &UiConfig::default(),
            area,
            &mut buffer,
        );
        let text = (0..width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        (text, buffer)
    }

    #[test]
    fn chrome_layout_docks_both_sides_at_exact_deterministic_breakpoints() {
        let ui = UiConfig::default();
        let both = SidebarRelevance {
            left: Some(true),
            right: Some(true),
        };
        assert_eq!(
            client_layout(Rect::new(3, 4, 67, 24), &ui, both),
            ClientLayout {
                tab_bar: Some(Rect::new(3, 4, 67, 1)),
                terminal: Rect::new(3, 5, 67, 23),
                left_sidebar: Some(SidebarLayout::Drawer(Rect::new(3, 4, 28, 24))),
                right_sidebar: Some(SidebarLayout::Drawer(Rect::new(42, 4, 28, 24))),
            }
        );
        assert_eq!(
            client_layout(Rect::new(3, 4, 95, 24), &ui, both),
            ClientLayout {
                tab_bar: Some(Rect::new(31, 4, 67, 1)),
                terminal: Rect::new(31, 5, 67, 23),
                left_sidebar: Some(SidebarLayout::Docked(Rect::new(3, 4, 28, 24))),
                right_sidebar: Some(SidebarLayout::Drawer(Rect::new(70, 4, 28, 24))),
            }
        );
        assert_eq!(
            client_layout(Rect::new(3, 4, 96, 24), &ui, both),
            ClientLayout {
                tab_bar: Some(Rect::new(31, 4, 40, 1)),
                terminal: Rect::new(31, 5, 40, 23),
                left_sidebar: Some(SidebarLayout::Docked(Rect::new(3, 4, 28, 24))),
                right_sidebar: Some(SidebarLayout::Docked(Rect::new(71, 4, 28, 24))),
            }
        );
        let mut bottom = UiConfig::default();
        bottom.tab_bar.position = TabBarPosition::Bottom;
        assert_eq!(
            client_layout(Rect::new(3, 4, 96, 24), &bottom, both),
            ClientLayout {
                tab_bar: Some(Rect::new(31, 27, 40, 1)),
                terminal: Rect::new(31, 4, 40, 23),
                left_sidebar: Some(SidebarLayout::Docked(Rect::new(3, 4, 28, 24))),
                right_sidebar: Some(SidebarLayout::Docked(Rect::new(71, 4, 28, 24))),
            }
        );
    }

    #[test]
    fn sidebar_visibility_uses_component_relevance() {
        let ui = UiConfig::default();
        let irrelevant = SidebarRelevance {
            left: Some(false),
            right: Some(false),
        };
        let layout = client_layout(Rect::new(0, 0, 124, 24), &ui, irrelevant);
        assert_eq!(layout.terminal, Rect::new(0, 1, 124, 23));
        assert_eq!(
            layout.left_sidebar,
            Some(SidebarLayout::Drawer(Rect::new(0, 0, 28, 24)))
        );

        let mut always_visible = UiConfig::default();
        always_visible.sidebar.left.visibility = SidebarVisibility::Visible;
        assert!(matches!(
            client_layout(Rect::new(0, 0, 124, 24), &always_visible, irrelevant).left_sidebar,
            Some(SidebarLayout::Docked(_))
        ));

        let mut minimized = UiConfig::default();
        minimized.sidebar.left.display = SidebarDisplay::Minimized;
        let narrow = client_layout(Rect::new(0, 0, 46, 24), &minimized, irrelevant);
        assert_eq!(narrow.terminal, Rect::new(0, 1, 46, 23));
        assert_eq!(
            narrow.left_sidebar,
            Some(SidebarLayout::Drawer(Rect::new(0, 0, 28, 24)))
        );
        let layout = client_layout(
            Rect::new(0, 0, 46, 24),
            &minimized,
            SidebarRelevance {
                left: Some(true),
                right: Some(false),
            },
        );
        assert_eq!(layout.terminal, Rect::new(6, 1, 40, 23));
        assert_eq!(
            layout.left_sidebar,
            Some(SidebarLayout::Docked(Rect::new(0, 0, 6, 24)))
        );
        assert_eq!(
            sidebar_drawer(Rect::new(0, 0, 46, 24), &minimized, SidebarSide::Left),
            Some(Rect::new(0, 0, 28, 24))
        );
    }

    #[test]
    fn hidden_sidebar_is_available_as_a_drawer() {
        let mut ui = UiConfig::default();
        ui.sidebar.left.visibility = SidebarVisibility::Hidden;
        let layout = client_layout(
            Rect::new(0, 0, 124, 24),
            &ui,
            SidebarRelevance {
                left: Some(true),
                right: Some(false),
            },
        );
        assert_eq!(layout.terminal, Rect::new(0, 1, 124, 23));
        assert_eq!(
            layout.left_sidebar,
            Some(SidebarLayout::Drawer(Rect::new(0, 0, 28, 24)))
        );
    }

    #[test]
    fn the_right_lane_names_the_current_workspace_before_optional_chrome() {
        let (wide, _) = render(&["shell", "editor"], 0, 60);
        assert!(wide.ends_with("fut "));
        assert!(wide.find("main").unwrap() > wide.find('2').unwrap());

        let (narrow, _) = render(&["shell", "editor"], 0, 22);
        assert!(narrow.contains("main"));
        assert!(!narrow.contains("fut"));
    }

    #[test]
    fn chrome_layout_returns_tiny_hosts_to_the_terminal_but_keeps_a_drawer_overlay() {
        assert_eq!(
            client_layout(
                Rect::new(3, 4, 80, 0),
                &UiConfig::default(),
                SidebarRelevance::default(),
            ),
            ClientLayout {
                tab_bar: None,
                terminal: Rect::new(3, 4, 80, 0),
                left_sidebar: None,
                right_sidebar: None,
            }
        );
        assert_eq!(
            client_layout(
                Rect::new(3, 4, 124, 1),
                &UiConfig::default(),
                SidebarRelevance::default(),
            ),
            ClientLayout {
                tab_bar: None,
                terminal: Rect::new(3, 4, 124, 1),
                left_sidebar: Some(SidebarLayout::Drawer(Rect::new(3, 4, 28, 1))),
                right_sidebar: Some(SidebarLayout::Drawer(Rect::new(99, 4, 28, 1))),
            }
        );
    }

    #[test]
    fn resource_state_only_accepts_newer_complete_snapshots() {
        let (mut first, _) = fixture(&["one"], 0);
        let mut state = ResourceState::default();
        assert!(state.accept(first.clone()));
        assert!(!state.accept(first.clone()));
        first.revision = 0;
        assert!(!state.accept(first.clone()));
        first.revision = 2;
        assert!(state.accept(first));
        assert_eq!(state.snapshot().unwrap().revision, 2);
    }

    #[test]
    fn resource_state_combines_workspace_and_agent_component_relevance() {
        let (mut snapshot, focused) = fixture(&["one"], 0);
        let mut state = ResourceState::default();
        assert!(state.accept(snapshot.clone()));
        assert_eq!(
            state.sidebar_relevance(&focused, &UiConfig::default()),
            SidebarRelevance {
                left: Some(false),
                right: Some(false),
            }
        );

        snapshot.revision += 1;
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .integration = Some(AgentIntegration::default());
        assert!(state.accept(snapshot.clone()));
        assert_eq!(
            state.sidebar_relevance(&focused, &UiConfig::default()),
            SidebarRelevance {
                left: Some(false),
                right: Some(true),
            }
        );

        snapshot.revision += 1;
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0].closing = true;
        assert!(state.accept(snapshot.clone()));
        assert_eq!(
            state.sidebar_relevance(&focused, &UiConfig::default()),
            SidebarRelevance {
                left: Some(false),
                right: Some(false),
            }
        );

        snapshot.revision += 1;
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0].closing = false;
        let mut second = snapshot.sessions[0].workspaces[0].clone();
        second.id = WorkspaceId::new();
        second.closing = false;
        snapshot.sessions[0].workspaces.push(second);
        assert!(state.accept(snapshot));
        assert_eq!(
            state.sidebar_relevance(&focused, &UiConfig::default()),
            SidebarRelevance {
                left: Some(true),
                right: Some(true),
            }
        );
    }

    #[test]
    fn configurable_group_allocation_never_overlaps_and_honors_priority() {
        for width in 0..=200 {
            let mut groups = vec![
                ResolvedGroup {
                    lane: Lane::Left,
                    line: Line::raw("tabs preferred content"),
                    tabs: true,
                    hotkeys: false,
                    style: None,
                    priority: 100,
                    allocation: 0,
                },
                ResolvedGroup {
                    lane: Lane::Center,
                    line: Line::raw("CENTER"),
                    tabs: false,
                    hotkeys: false,
                    style: None,
                    priority: 20,
                    allocation: 0,
                },
                ResolvedGroup {
                    lane: Lane::Right,
                    line: Line::raw("ZOOM"),
                    tabs: false,
                    hotkeys: false,
                    style: None,
                    priority: 255,
                    allocation: 0,
                },
            ];
            allocate_groups(&mut groups, width);
            assert!(
                groups.iter().map(|group| group.allocation).sum::<usize>() <= width,
                "width {width}"
            );
            if width >= 5 {
                assert_eq!(groups[2].allocation, 4, "width {width}");
                assert!(groups[0].allocation >= 1, "width {width}");
            }
        }
    }

    #[test]
    fn configured_normal_background_covers_the_complete_bar() {
        let ui: UiConfig = toml::from_str(
            "[styles.normal]\nbackground = 'blue'\n\n[tab_bar]\nleft = []\ncenter = []\nright = []\n",
        )
        .unwrap();
        let (snapshot, focused) = fixture(&["shell"], 0);
        let area = Rect::new(0, 0, 20, 1);
        let mut buffer = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &focused,
            false,
            None,
            &NotificationState::default(),
            0,
            &ui,
            area,
            &mut buffer,
        );
        assert!(
            (0..area.width).all(|column| { buffer[(column, 0)].bg == ratatui::style::Color::Blue })
        );
    }

    #[test]
    fn tab_bar_defaults_to_stable_numbered_items_and_theme_native_selection() {
        let (text, buffer) = render(&["shell", "editor", "tests"], 1, 60);
        assert_eq!(text.chars().nth(1), Some('1'));
        assert_eq!(text.chars().nth(10), Some('2'));
        assert_eq!(text.chars().nth(20), Some('3'));
        assert!(text.contains("1 shell"));
        assert!(text.contains("2 editor"));
        assert!(text.ends_with("fut "));
        let active = 13;
        assert_eq!(buffer[(active, 0)].fg, ratatui::style::Color::Blue);
        assert!(buffer[(active, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(active, 0)].modifier.contains(Modifier::UNDERLINED));
        assert!(!buffer[(active, 0)].modifier.contains(Modifier::BOLD));

        let (snapshot, focused) = fixture(&["shell", "editor", "tests"], 1);
        let area = Rect::new(0, 0, 60, 1);
        let mut selected = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &focused,
            false,
            Some(snapshot.sessions[0].workspaces[0].tabs[2].id),
            &NotificationState::default(),
            0,
            &UiConfig::default(),
            area,
            &mut selected,
        );
        let selected_text = (0..area.width)
            .map(|column| selected[(column, 0)].symbol())
            .collect::<String>();
        for number in ['1', '2', '3'] {
            assert_eq!(
                text.find(number),
                selected_text.find(number),
                "selection styling must not move tab {number:?}"
            );
        }
        assert!((19..28).all(|column| {
            selected[(column, 0)].bg == ratatui::style::Color::DarkGray
                && !selected[(column, 0)]
                    .modifier
                    .contains(Modifier::UNDERLINED)
                && !selected[(column, 0)].modifier.contains(Modifier::REVERSED)
        }));
    }

    #[test]
    fn short_tab_items_use_their_intrinsic_width() {
        let (text, _) = render(&["a", "bb", "c"], 1, 30);
        assert!(text.starts_with(" 1 a  2 bb  3 c "), "{text:?}");
    }

    #[test]
    fn nerd_font_preset_draws_the_active_tab_as_a_pill() {
        let ui: UiConfig = toml::from_str("[icons]\npreset = 'nerd_font'\n").unwrap();
        let (snapshot, focused) = fixture(&["shell", "editor", "tests"], 1);
        let area = Rect::new(0, 0, 60, 1);
        let mut buffer = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &focused,
            false,
            None,
            &NotificationState::default(),
            0,
            &ui,
            area,
            &mut buffer,
        );
        let column_of = |symbol: &str| {
            (0..area.width)
                .find(|column| buffer[(*column, 0)].symbol() == symbol)
                .unwrap()
        };
        let left = column_of("\u{e0b6}");
        let right = column_of("\u{e0b4}");
        assert_eq!(right - left, " 2 editor ".len() as u16 + 1);
        for cap in [left, right] {
            assert_eq!(buffer[(cap, 0)].fg, ratatui::style::Color::Blue);
            assert_eq!(buffer[(cap, 0)].bg, ratatui::style::Color::Reset);
        }
        assert!((left + 1..right).all(|column| {
            buffer[(column, 0)].fg == ratatui::style::Color::Blue
                && buffer[(column, 0)].modifier.contains(Modifier::REVERSED)
        }));

        let (plain, _) = render(&["shell", "editor", "tests"], 1, 60);
        assert!(!plain.contains('\u{e0b6}') && !plain.contains('\u{e0b4}'));
    }

    #[test]
    fn nerd_font_placeholders_keep_tab_positions_stable_as_focus_changes() {
        let ui: UiConfig = toml::from_str("[icons]\npreset = 'nerd_font'\n").unwrap();
        let rendered = [0, 1].map(|active| {
            let (snapshot, focused) = fixture(&["a", "bb", "c"], active);
            let area = Rect::new(0, 0, 30, 1);
            let mut buffer = Buffer::empty(area);
            render_tab_bar(
                Some(&snapshot),
                &focused,
                false,
                None,
                &NotificationState::default(),
                0,
                &ui,
                area,
                &mut buffer,
            );
            ['1', '2', '3'].map(|number| {
                (0..area.width).find(|column| buffer[(*column, 0)].symbol() == number.to_string())
            })
        });

        assert_eq!(rendered[0], rendered[1]);
    }

    #[test]
    fn narrow_rows_keep_focus_and_choose_nearby_tabs_in_resource_order() {
        let (text, _) = render(&["one", "two", "three", "four", "five"], 2, 24);
        assert!(text.contains('3'));
        let visible = ['1', '2', '3', '4', '5']
            .into_iter()
            .filter_map(|index| text.find(index).map(|position| (position, index)))
            .collect::<Vec<_>>();
        assert!(visible.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(text.matches('…').count() <= 2);
    }

    #[test]
    fn tab_bar_scene_uses_the_rendered_tab_geometry_for_hits() {
        let (snapshot, focused) = fixture(&["one", "two", "three", "four"], 1);
        let model =
            TabBarModel::from_snapshot(&snapshot, &focused, &NotificationState::default()).unwrap();
        let scene = TabBarScene::build(&model, false, None, 0, &UiConfig::default(), 18);

        let rendered_tabs = scene
            .groups
            .iter()
            .flat_map(|group| {
                group
                    .tabs
                    .iter()
                    .map(move |tab| (group.x + tab.x, tab.width, TabBarHit::Item(tab.id)))
            })
            .collect::<Vec<_>>();
        assert!(!rendered_tabs.is_empty());
        for (x, width, hit) in rendered_tabs {
            assert!((x..x + width).all(|column| scene.hit_at(column) == Some(hit)));
        }
    }

    #[test]
    fn tiny_and_unicode_rows_clip_by_cells_without_losing_the_active_number() {
        for width in 0..20 {
            let (text, _) = render(&["前の", "agent 👩🏽‍💻 long", "後ろ"], 1, width);
            if width > 0 {
                assert!(text.contains('2'), "width {width}: {text:?}");
            }
        }
        assert_eq!(truncate("👩🏽‍💻abc", 3), "👩🏽‍💻…");
        assert_eq!(sanitize("bad\nname\u{202e}"), "bad�name�");
    }

    #[test]
    fn closing_and_missing_metadata_are_visible_without_panicking() {
        let (mut snapshot, focused) = fixture(&["shell", "closing"], 0);
        snapshot.sessions[0].workspaces[0].tabs[1].closing = true;
        let area = Rect::new(0, 0, 40, 1);
        let mut buffer = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &focused,
            false,
            None,
            &NotificationState::default(),
            0,
            &UiConfig::default(),
            area,
            &mut buffer,
        );
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(text.contains("2 closing ×"));

        let mut missing = focused.clone();
        missing.workspace_id = WorkspaceId::new();
        let mut fallback = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &missing,
            false,
            None,
            &NotificationState::default(),
            0,
            &UiConfig::default(),
            area,
            &mut fallback,
        );
        assert_eq!(fallback[(1, 0)].symbol(), "1");
    }

    #[test]
    fn zoom_status_is_persistent_and_preserves_the_active_tab() {
        let (snapshot, focused) = fixture(&["shell", "editor", "tests"], 1);
        let area = Rect::new(3, 4, 24, 1);
        let mut buffer = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &focused,
            true,
            None,
            &NotificationState::default(),
            0,
            &UiConfig::default(),
            area,
            &mut buffer,
        );
        let text = (area.x..area.x + area.width)
            .map(|column| buffer[(column, area.y)].symbol())
            .collect::<String>();

        assert!(text.contains('2'));
        assert!(text.contains("zoom"));

        for width in 1..=6 {
            let area = Rect::new(0, 0, width, 1);
            let mut buffer = Buffer::empty(area);
            render_tab_bar(
                Some(&snapshot),
                &focused,
                true,
                None,
                &NotificationState::default(),
                0,
                &UiConfig::default(),
                area,
                &mut buffer,
            );
            let text = (0..width)
                .map(|column| buffer[(column, 0)].symbol())
                .collect::<String>();
            if width == 6 {
                assert!(text.contains('2'));
                assert!(text.ends_with("zoom "));
            } else {
                assert!(text.contains('2'));
            }
        }
    }
}
