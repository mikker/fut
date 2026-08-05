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
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, TabSnapshot},
};

use super::{
    config::{GroupConfig, SemanticStyle, TabBarPosition, UiConfig, WorkspaceSidebarPosition},
    presentation::{ItemState, TokenValue, render_token_segments, truncate_line},
};

const MIN_DOCKED_TERMINAL_WIDTH: u16 = 96;

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
    pub fn area(self) -> Rect {
        match self {
            Self::Docked(area) | Self::Drawer(area) => area,
        }
    }

    pub fn docked(self) -> Option<Rect> {
        match self {
            Self::Docked(area) => Some(area),
            Self::Drawer(_) => None,
        }
    }
}

pub(super) fn client_layout(host: Rect, ui: &UiConfig) -> ClientLayout {
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

    let sidebar_width = ui.workspace_sidebar.width;
    let docked = host.width >= sidebar_width.saturating_add(MIN_DOCKED_TERMINAL_WIDTH);
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
            .or_else(|| {
                sidebar_rect(host, ui.workspace_sidebar.position, sidebar_width)
                    .map(WorkspaceSidebarLayout::Drawer)
            }),
    }
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TabItem {
    id: TabId,
    name: String,
    closing: bool,
    pane_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TabBarModel {
    session_name: String,
    workspace_name: String,
    tabs: Vec<TabItem>,
    active: usize,
}

impl TabBarModel {
    fn from_snapshot(snapshot: &ResourceSnapshot, focused: &SelectedTarget) -> Option<Self> {
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
            workspace_name: sanitize(&workspace.name),
            tabs: workspace.tabs.iter().map(TabItem::from).collect(),
            active,
        })
    }
}

impl From<&TabSnapshot> for TabItem {
    fn from(tab: &TabSnapshot) -> Self {
        Self {
            id: tab.id,
            name: sanitize(&tab.name),
            closing: tab.closing,
            pane_count: tab.panes.len(),
        }
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

pub(super) fn render_tab_bar(
    snapshot: Option<&ResourceSnapshot>,
    focused: &SelectedTarget,
    zoomed: bool,
    selected: Option<TabId>,
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
        .and_then(|snapshot| TabBarModel::from_snapshot(snapshot, focused))
        .unwrap_or_else(|| TabBarModel {
            session_name: "session".into(),
            workspace_name: "workspace".into(),
            tabs: vec![TabItem {
                id: focused.tab_id,
                name: "tab".into(),
                closing: false,
                pane_count: 1,
            }],
            active: 0,
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
                selected_line(&model, selected, 0, model.tabs.len() - 1, group.style, ui)
            } else {
                render_bar_group(group, &model, zoomed, selected, ui)
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
                visible_tabs(&model, selected, group.allocation, group.style, ui).0
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
            _ => TokenValue::plain(""),
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
    let mut line = selected_line(model, selected, first, last, component_style, ui);
    if line.width() > width {
        let fallback = render_tab_item(model, anchor, selected, component_style, ui);
        if width == 1 {
            let marker = tab_token(model, anchor, "tab.marker", &ui.icons.resolve());
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
            return (Line::styled(truncate(&marker.text, 1), style), false);
        }
        return (truncate_line(&fallback, width), false);
    }

    loop {
        let mut changed = false;
        if first > 0 {
            let candidate = selected_line(model, selected, first - 1, last, component_style, ui);
            if candidate.width() <= width {
                first -= 1;
                line = candidate;
                changed = true;
            }
        }
        if last + 1 < model.tabs.len() {
            let candidate = selected_line(model, selected, first, last + 1, component_style, ui);
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
        spans.extend(render_tab_item(model, index, selected, component_style, ui).spans);
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
            attention: false,
        },
        &ui.styles,
        |token| tab_token(model, index, token, &icons),
    )
}

fn tab_token(
    model: &TabBarModel,
    index: usize,
    token: &str,
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
        _ => TokenValue::plain(""),
    }
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
        resources::{PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, WorkspaceSnapshot},
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
                    id: TabId::new(),
                    name: (*name).into(),
                    closing: false,
                    layout: crate::splits::SplitTree::leaf(pane_id),
                    panes: vec![PaneSnapshot {
                        id: pane_id,
                        terminal_id: TerminalId::new(),
                        closing: false,
                    }],
                }
            })
            .collect::<Vec<_>>();
        let selected_tab = &tabs[active];
        let selected_pane = selected_tab.panes[0];
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
                    id: session_id,
                    name: "project".into(),
                    project: Project {
                        identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/project")),
                    },
                    closing: false,
                    workspaces: vec![WorkspaceSnapshot {
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

    fn render(names: &[&str], active: usize, width: u16) -> (String, Buffer) {
        let (snapshot, focused) = fixture(names, active);
        let area = Rect::new(0, 0, width, 1);
        let mut buffer = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &focused,
            false,
            None,
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
            client_layout(Rect::new(3, 4, 119, 24), &top_left),
            ClientLayout {
                tab_bar: Some(Rect::new(3, 4, 119, 1)),
                terminal: Rect::new(3, 5, 119, 23),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Drawer(Rect::new(3, 4, 24, 24,))),
            }
        );
        assert_eq!(
            client_layout(Rect::new(3, 4, 120, 24), &top_left),
            ClientLayout {
                tab_bar: Some(Rect::new(27, 4, 96, 1)),
                terminal: Rect::new(27, 5, 96, 23),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Docked(Rect::new(3, 4, 24, 24,))),
            }
        );
        let mut bottom_right = UiConfig::default();
        bottom_right.tab_bar.position = TabBarPosition::Bottom;
        bottom_right.workspace_sidebar.position = WorkspaceSidebarPosition::Right;
        assert_eq!(
            client_layout(Rect::new(3, 4, 120, 24), &bottom_right),
            ClientLayout {
                tab_bar: Some(Rect::new(3, 27, 96, 1)),
                terminal: Rect::new(3, 4, 96, 23),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Docked(Rect::new(99, 4, 24, 24,))),
            }
        );
    }

    #[test]
    fn chrome_layout_returns_tiny_hosts_to_the_terminal_but_keeps_a_drawer_overlay() {
        assert_eq!(
            client_layout(Rect::new(3, 4, 80, 0), &UiConfig::default()),
            ClientLayout {
                tab_bar: None,
                terminal: Rect::new(3, 4, 80, 0),
                workspace_sidebar: None,
            }
        );
        assert_eq!(
            client_layout(Rect::new(3, 4, 120, 1), &UiConfig::default()),
            ClientLayout {
                tab_bar: None,
                terminal: Rect::new(3, 4, 120, 1),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Drawer(Rect::new(3, 4, 24, 1,))),
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
            &ui,
            area,
            &mut buffer,
        );
        assert!(
            (0..area.width).all(|column| { buffer[(column, 0)].bg == ratatui::style::Color::Blue })
        );
    }

    #[test]
    fn tab_bar_preserves_order_marks_focus_and_uses_terminal_native_styles() {
        let (text, buffer) = render(&["shell", "editor", "tests"], 1, 60);
        assert!(text.contains(" 1 shell "));
        assert!(text.contains(" ● editor "));
        assert!(text.contains(" 3 tests "));
        assert!(text.ends_with("fut "));
        let marker = text.find('●').unwrap() as u16;
        assert!(buffer[(marker, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(marker, 0)].fg, ratatui::style::Color::Reset);
        assert_eq!(buffer[(marker, 0)].bg, ratatui::style::Color::Reset);
    }

    #[test]
    fn narrow_rows_keep_focus_and_choose_nearby_tabs_in_resource_order() {
        let (text, _) = render(&["one", "two", "three", "four", "five"], 2, 24);
        assert!(text.contains('●'));
        assert!(text.contains("three"));
        let visible = ["one", "two", "three", "four", "five"]
            .into_iter()
            .filter_map(|name| text.find(name).map(|index| (index, name)))
            .collect::<Vec<_>>();
        assert!(visible.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(text.matches('…').count() <= 2);
    }

    #[test]
    fn tiny_and_unicode_rows_clip_by_cells_without_losing_the_active_marker() {
        for width in 0..20 {
            let (text, _) = render(&["前の", "agent 👩🏽‍💻 long", "後ろ"], 1, width);
            if width > 0 {
                assert!(text.contains('●'), "width {width}: {text:?}");
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
            &UiConfig::default(),
            area,
            &mut buffer,
        );
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(text.contains("closing ×"));

        let mut missing = focused.clone();
        missing.workspace_id = WorkspaceId::new();
        let mut fallback = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &missing,
            false,
            None,
            &UiConfig::default(),
            area,
            &mut fallback,
        );
        assert_eq!(fallback[(1, 0)].symbol(), "●");
    }

    #[test]
    fn zoom_status_is_persistent_bold_and_preserves_the_active_tab() {
        let (snapshot, focused) = fixture(&["shell", "editor", "tests"], 1);
        let area = Rect::new(3, 4, 24, 1);
        let mut buffer = Buffer::empty(area);
        render_tab_bar(
            Some(&snapshot),
            &focused,
            true,
            None,
            &UiConfig::default(),
            area,
            &mut buffer,
        );
        let text = (area.x..area.x + area.width)
            .map(|column| buffer[(column, area.y)].symbol())
            .collect::<String>();

        assert!(text.contains("●"));
        assert!(text.contains("editor"));
        assert!(text.ends_with("zoom "));
        assert!(
            buffer[(area.x + area.width - 5, area.y)]
                .modifier
                .contains(Modifier::BOLD)
        );

        for width in 1..=6 {
            let area = Rect::new(0, 0, width, 1);
            let mut buffer = Buffer::empty(area);
            render_tab_bar(
                Some(&snapshot),
                &focused,
                true,
                None,
                &UiConfig::default(),
                area,
                &mut buffer,
            );
            let text = (0..width)
                .map(|column| buffer[(column, 0)].symbol())
                .collect::<String>();
            if width == 6 {
                assert!(text.contains('●'));
                assert!(text.ends_with("zoom "));
            } else {
                assert!(text.contains('●'));
            }
        }
    }
}
