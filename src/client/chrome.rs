use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    domain::{AgentReport, TabId},
    protocol::SelectedTarget,
    resources::{MaterializedTokenMap, ResourceSnapshot},
};

use super::{
    config::{
        GroupConfig, MINIMIZED_SIDEBAR_WIDTH, SemanticStyle, TabBarPosition, UiConfig,
        WorkspaceSidebarDisplay, WorkspaceSidebarPosition, WorkspaceSidebarVisibility,
    },
    notifications::{ActivityIndicator, NotificationState},
    presentation::{ItemState, TokenValue, apply_item_state, render_token_segments, truncate_line},
};

pub(super) const MIN_DOCKED_TERMINAL_WIDTH: u16 = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ClientLayout {
    pub terminal: Rect,
    pub tab_bar: Option<Rect>,
    pub workspace_sidebar: Option<WorkspaceSidebarLayout>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceSidebarLayout {
    Docked(Rect),
    Drawer(Rect),
}

impl WorkspaceSidebarLayout {
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
    workspace_count: Option<usize>,
) -> ClientLayout {
    if host.width == 0 || host.height < 2 {
        return ClientLayout {
            terminal: host,
            tab_bar: None,
            workspace_sidebar: sidebar_rect(
                host,
                ui.workspace_sidebar.position,
                ui.workspace_sidebar.width,
            )
            .map(WorkspaceSidebarLayout::Drawer),
        };
    }

    let sidebar_width = match ui.workspace_sidebar.display {
        WorkspaceSidebarDisplay::Expanded => ui.workspace_sidebar.width,
        WorkspaceSidebarDisplay::Minimized => MINIMIZED_SIDEBAR_WIDTH,
    };
    let sidebar_needed = match ui.workspace_sidebar.visibility {
        WorkspaceSidebarVisibility::Visible => true,
        WorkspaceSidebarVisibility::AutoHideWhenSingle => {
            workspace_count.is_none_or(|count| count > 1)
        }
        WorkspaceSidebarVisibility::Hidden => false,
    };
    let docked =
        sidebar_needed && host.width >= sidebar_width.saturating_add(MIN_DOCKED_TERMINAL_WIDTH);
    let (workspace, docked_sidebar) = if docked {
        let sidebar = sidebar_rect(host, ui.workspace_sidebar.position, sidebar_width)
            .expect("nonempty host has a sidebar rectangle");
        let workspace = match ui.workspace_sidebar.position {
            WorkspaceSidebarPosition::Left => Rect::new(
                host.x.saturating_add(sidebar_width),
                host.y,
                host.width - sidebar_width,
                host.height,
            ),
            WorkspaceSidebarPosition::Right => {
                Rect::new(host.x, host.y, host.width - sidebar_width, host.height)
            }
        };
        (workspace, Some(sidebar))
    } else {
        (host, None)
    };

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
        workspace_sidebar: docked_sidebar
            .map(WorkspaceSidebarLayout::Docked)
            .or_else(|| workspace_sidebar_drawer(host, ui).map(WorkspaceSidebarLayout::Drawer)),
    }
}

pub(super) fn workspace_sidebar_drawer(host: Rect, ui: &UiConfig) -> Option<Rect> {
    sidebar_rect(
        host,
        ui.workspace_sidebar.position,
        ui.workspace_sidebar.width,
    )
}

fn sidebar_rect(
    body: Rect,
    position: WorkspaceSidebarPosition,
    configured_width: u16,
) -> Option<Rect> {
    if body.width == 0 || body.height == 0 {
        return None;
    }
    let width = body.width.min(configured_width);
    let x = match position {
        WorkspaceSidebarPosition::Left => body.x,
        WorkspaceSidebarPosition::Right => body.x.saturating_add(body.width - width),
    };
    Some(Rect::new(x, body.y, width, body.height))
}

#[derive(Default)]
pub(super) struct ResourceState {
    snapshot: Option<ResourceSnapshot>,
    notifications: NotificationState,
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

    pub fn notifications(&self) -> &NotificationState {
        &self.notifications
    }

    pub fn attention_revision(&self, terminal_id: crate::domain::TerminalId) -> Option<u64> {
        self.snapshot
            .as_ref()?
            .sessions
            .iter()
            .flat_map(|session| &session.workspaces)
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| &tab.panes)
            .find(|pane| pane.terminal_id == terminal_id)?
            .activity
            .last_event
            .as_ref()
            .filter(|event| matches!(event.kind, AgentReport::Blocked | AgentReport::Completed))
            .map(|event| event.revision)
    }

    pub fn observe(&mut self, terminal_id: crate::domain::TerminalId, revision: u64) -> bool {
        self.notifications.observe(terminal_id, revision)
    }

    pub fn has_working(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| self.notifications.has_working(snapshot))
    }

    pub fn workspace_count(&self, focused: &SelectedTarget) -> Option<usize> {
        self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .sessions
                .iter()
                .find(|session| session.id == focused.session_id)
                .map(|session| session.workspaces.len())
        })
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
    style: Option<SemanticStyle>,
    priority: u8,
    allocation: usize,
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
    let width = usize::from(area.width);
    let model = snapshot
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
        });
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
                .any(|segment| segment.component.as_deref() == Some("tabs"));
            let line = if tabs {
                selected_line(
                    &model,
                    selected,
                    0,
                    model.tabs.len() - 1,
                    group.style,
                    spinner_frame,
                    ui,
                )
            } else {
                render_bar_group(group, &model, zoomed, selected, spinner_frame, ui)
            };
            if tabs || line.width() > 0 {
                groups.push(ResolvedGroup {
                    lane,
                    line,
                    tabs,
                    style: group.style,
                    priority: group.priority,
                    allocation: 0,
                });
            }
        }
    }
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
    for (lane, mut x) in [
        (Lane::Left, 0usize),
        (Lane::Center, center_x),
        (Lane::Right, width.saturating_sub(right_width)),
    ] {
        for group in groups.iter().filter(|group| group.lane == lane) {
            if group.allocation == 0 {
                continue;
            }
            let line = if group.tabs {
                visible_tabs(
                    &model,
                    selected,
                    group.allocation,
                    group.style,
                    spinner_frame,
                    ui,
                )
                .0
            } else {
                truncate_line(&group.line, group.allocation)
            };
            buffer.set_line(
                area.x.saturating_add(u16::try_from(x).unwrap_or(u16::MAX)),
                area.y,
                &line,
                u16::try_from(group.allocation).unwrap_or(u16::MAX),
            );
            x += group.allocation;
        }
    }
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
        |token| match token {
            "fut" if selected.is_none() => TokenValue::plain("fut "),
            "session.name" => TokenValue::plain(model.session_name.clone()),
            "workspace.name" => TokenValue::plain(model.workspace_name.clone()),
            "workspace.icon" => TokenValue::plain(icons.workspace.clone()),
            "tab.name" => TokenValue::plain(active.name.clone()),
            "tab.index" => TokenValue::plain((model.active + 1).to_string()),
            "tab.pane_count" => TokenValue::plain(active.pane_count.to_string()),
            "client.zoom" if zoomed => TokenValue::plain(icons.zoom.clone()),
            "client.help" if selected.is_some() => TokenValue::plain("c new · r rename · esc "),
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
            _ => TokenValue::plain(model.extension_value(token)),
        },
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

fn visible_tabs(
    model: &TabBarModel,
    selected: Option<TabId>,
    width: usize,
    component_style: Option<SemanticStyle>,
    spinner_frame: usize,
    ui: &UiConfig,
) -> (Line<'static>, bool) {
    if width == 0 {
        return (Line::default(), false);
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
        let marker = tab_token(
            model,
            anchor,
            "tab.index",
            spinner_frame,
            &ui.icons.resolve(),
        );
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
            return (
                Line::styled(format!("{:^width$}", marker.text), style),
                false,
            );
        }
        return (truncate_line(&fallback, width), false);
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
    (line, first == 0 && last + 1 == model.tabs.len())
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
    let cap = pill_cap_style(style, component_style, ui);
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

/// Caps are drawn as the pill's own background on the bar background so they read as round ends.
fn pill_cap_style(item: Style, component_style: Option<SemanticStyle>, ui: &UiConfig) -> Style {
    let mut bar = ui.styles.apply(SemanticStyle::Normal, Style::default());
    if let Some(role) = component_style {
        bar = ui.styles.apply(role, bar);
    }
    let fill = if item.add_modifier.contains(Modifier::REVERSED) {
        item.fg
    } else {
        item.bg
    };
    Style {
        fg: fill,
        bg: bar.bg,
        ..Style::default()
    }
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
        |token| tab_token(model, index, token, spinner_frame, &icons),
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
        _ => TokenValue::plain(tab.tokens.get(token).map_or("", String::as_str)),
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
    use std::path::PathBuf;

    use super::*;
    use crate::{
        domain::{PaneId, SessionId, TabId, TerminalId, WorkspaceId},
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
            .map(|token| super::super::config::SegmentConfig {
                token: Some(token.into()),
                ..Default::default()
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
    fn chrome_layout_composes_tab_and_sidebar_positions_at_the_exact_breakpoint() {
        let top_left = UiConfig::default();
        assert_eq!(
            client_layout(Rect::new(3, 4, 67, 24), &top_left, Some(2)),
            ClientLayout {
                tab_bar: Some(Rect::new(3, 4, 67, 1)),
                terminal: Rect::new(3, 5, 67, 23),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Drawer(Rect::new(3, 4, 28, 24,))),
            }
        );
        assert_eq!(
            client_layout(Rect::new(3, 4, 68, 24), &top_left, Some(2)),
            ClientLayout {
                tab_bar: Some(Rect::new(31, 4, 40, 1)),
                terminal: Rect::new(31, 5, 40, 23),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Docked(Rect::new(3, 4, 28, 24,))),
            }
        );
        let mut bottom_right = UiConfig::default();
        bottom_right.tab_bar.position = TabBarPosition::Bottom;
        bottom_right.workspace_sidebar.position = WorkspaceSidebarPosition::Right;
        assert_eq!(
            client_layout(Rect::new(3, 4, 68, 24), &bottom_right, Some(2)),
            ClientLayout {
                tab_bar: Some(Rect::new(3, 27, 40, 1)),
                terminal: Rect::new(3, 4, 40, 23),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Docked(Rect::new(43, 4, 28, 24,))),
            }
        );
    }

    #[test]
    fn workspace_sidebar_visibility_controls_docking() {
        let ui = UiConfig::default();
        let layout = client_layout(Rect::new(0, 0, 124, 24), &ui, Some(1));
        assert_eq!(layout.terminal, Rect::new(0, 1, 124, 23));
        assert_eq!(
            layout.workspace_sidebar,
            Some(WorkspaceSidebarLayout::Drawer(Rect::new(0, 0, 28, 24)))
        );

        let mut always_visible = UiConfig::default();
        always_visible.workspace_sidebar.visibility = WorkspaceSidebarVisibility::Visible;
        assert!(matches!(
            client_layout(Rect::new(0, 0, 124, 24), &always_visible, Some(1)).workspace_sidebar,
            Some(WorkspaceSidebarLayout::Docked(_))
        ));

        let mut minimized = UiConfig::default();
        minimized.workspace_sidebar.display = WorkspaceSidebarDisplay::Minimized;
        let single = client_layout(Rect::new(0, 0, 46, 24), &minimized, Some(1));
        assert_eq!(single.terminal, Rect::new(0, 1, 46, 23));
        assert_eq!(
            single.workspace_sidebar,
            Some(WorkspaceSidebarLayout::Drawer(Rect::new(0, 0, 28, 24)))
        );
        let layout = client_layout(Rect::new(0, 0, 46, 24), &minimized, Some(2));
        assert_eq!(layout.terminal, Rect::new(6, 1, 40, 23));
        assert_eq!(
            layout.workspace_sidebar,
            Some(WorkspaceSidebarLayout::Docked(Rect::new(0, 0, 6, 24)))
        );
        assert_eq!(
            workspace_sidebar_drawer(Rect::new(0, 0, 46, 24), &minimized),
            Some(Rect::new(0, 0, 28, 24))
        );
    }

    #[test]
    fn hidden_workspace_sidebar_is_available_as_a_drawer() {
        let mut ui = UiConfig::default();
        ui.workspace_sidebar.visibility = WorkspaceSidebarVisibility::Hidden;
        let layout = client_layout(Rect::new(0, 0, 124, 24), &ui, Some(3));
        assert_eq!(layout.terminal, Rect::new(0, 1, 124, 23));
        assert_eq!(
            layout.workspace_sidebar,
            Some(WorkspaceSidebarLayout::Drawer(Rect::new(0, 0, 28, 24)))
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
            client_layout(Rect::new(3, 4, 80, 0), &UiConfig::default(), Some(2)),
            ClientLayout {
                tab_bar: None,
                terminal: Rect::new(3, 4, 80, 0),
                workspace_sidebar: None,
            }
        );
        assert_eq!(
            client_layout(Rect::new(3, 4, 124, 1), &UiConfig::default(), Some(2)),
            ClientLayout {
                tab_bar: None,
                terminal: Rect::new(3, 4, 124, 1),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Drawer(Rect::new(3, 4, 28, 1,))),
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
    fn configurable_group_allocation_never_overlaps_and_honors_priority() {
        for width in 0..=200 {
            let mut groups = vec![
                ResolvedGroup {
                    lane: Lane::Left,
                    line: Line::raw("tabs preferred content"),
                    tabs: true,
                    style: None,
                    priority: 100,
                    allocation: 0,
                },
                ResolvedGroup {
                    lane: Lane::Center,
                    line: Line::raw("CENTER"),
                    tabs: false,
                    style: None,
                    priority: 20,
                    allocation: 0,
                },
                ResolvedGroup {
                    lane: Lane::Right,
                    line: Line::raw("ZOOM"),
                    tabs: false,
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
