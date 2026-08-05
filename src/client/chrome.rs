use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    domain::TabId,
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, TabSnapshot},
};

use super::config::{TabBarPosition, UiConfig, WorkspaceSidebarPosition};

pub(super) const WORKSPACE_SIDEBAR_WIDTH: u16 = 24;
const MIN_DOCKED_TERMINAL_WIDTH: u16 = 96;
const ZOOM_STATUS: &str = "zoom ";

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

pub(super) fn client_layout(host: Rect, ui: UiConfig) -> ClientLayout {
    if host.width == 0 || host.height < 2 {
        return ClientLayout {
            terminal: host,
            tab_bar: None,
            workspace_sidebar: sidebar_rect(host, ui.workspace_sidebar_position)
                .map(WorkspaceSidebarLayout::Drawer),
        };
    }

    let docked = host.width >= WORKSPACE_SIDEBAR_WIDTH.saturating_add(MIN_DOCKED_TERMINAL_WIDTH);
    let (workspace, docked_sidebar) = if docked {
        let sidebar = sidebar_rect(host, ui.workspace_sidebar_position)
            .expect("nonempty host has a sidebar rectangle");
        let workspace = match ui.workspace_sidebar_position {
            WorkspaceSidebarPosition::Left => Rect::new(
                host.x.saturating_add(WORKSPACE_SIDEBAR_WIDTH),
                host.y,
                host.width - WORKSPACE_SIDEBAR_WIDTH,
                host.height,
            ),
            WorkspaceSidebarPosition::Right => Rect::new(
                host.x,
                host.y,
                host.width - WORKSPACE_SIDEBAR_WIDTH,
                host.height,
            ),
        };
        (workspace, Some(sidebar))
    } else {
        (host, None)
    };

    let (terminal, tab_bar) = match ui.tab_bar_position {
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
                sidebar_rect(host, ui.workspace_sidebar_position)
                    .map(WorkspaceSidebarLayout::Drawer)
            }),
    }
}

fn sidebar_rect(body: Rect, position: WorkspaceSidebarPosition) -> Option<Rect> {
    if body.width == 0 || body.height == 0 {
        return None;
    }
    let width = body.width.min(WORKSPACE_SIDEBAR_WIDTH);
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TabBarModel {
    tabs: Vec<TabItem>,
    active: usize,
}

impl TabBarModel {
    fn from_snapshot(snapshot: &ResourceSnapshot, focused: &SelectedTarget) -> Option<Self> {
        let workspace = snapshot
            .sessions
            .iter()
            .find(|session| session.id == focused.session_id)?
            .workspaces
            .iter()
            .find(|workspace| workspace.id == focused.workspace_id)?;
        let active = workspace
            .tabs
            .iter()
            .position(|tab| tab.id == focused.tab_id)?;
        Some(Self {
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
        }
    }
}

pub(super) fn render_tab_bar(
    snapshot: Option<&ResourceSnapshot>,
    focused: &SelectedTarget,
    zoomed: bool,
    selected: Option<TabId>,
    area: Rect,
    buffer: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    clear_row(area, buffer);
    let width = usize::from(area.width);
    let zoom_width = if zoomed {
        width.min(UnicodeWidthStr::width(ZOOM_STATUS))
    } else {
        0
    };
    let tab_width = width.saturating_sub(zoom_width);
    let Some(model) = snapshot.and_then(|snapshot| TabBarModel::from_snapshot(snapshot, focused))
    else {
        let fallback = single_tab("●", "tab", false, tab_width);
        buffer.set_line(
            area.x,
            area.y,
            &Line::styled(fallback, active_style()),
            u16::try_from(tab_width).expect("tab width fits u16"),
        );
        render_zoom_status(zoom_width, area, buffer);
        return;
    };

    let (line, complete) = visible_tabs(&model, selected, tab_width);
    buffer.set_line(
        area.x,
        area.y,
        &line,
        u16::try_from(tab_width).expect("tab width fits u16"),
    );

    if zoom_width > 0 {
        render_zoom_status(zoom_width, area, buffer);
        return;
    }

    let suffix = if selected.is_some() {
        "c new · r rename · esc "
    } else {
        "fut "
    };
    let suffix_width = UnicodeWidthStr::width(suffix);
    if complete && line.width().saturating_add(suffix_width) <= width {
        buffer.set_string(
            area.x + area.width - u16::try_from(suffix_width).expect("suffix width fits u16"),
            area.y,
            suffix,
            muted_style(),
        );
    }
}

fn render_zoom_status(width: usize, area: Rect, buffer: &mut Buffer) {
    if width == 0 {
        return;
    }
    buffer.set_string(
        area.x + area.width - u16::try_from(width).expect("zoom width fits u16"),
        area.y,
        &ZOOM_STATUS[..width],
        active_style(),
    );
}

fn visible_tabs(
    model: &TabBarModel,
    selected: Option<TabId>,
    width: usize,
) -> (Line<'static>, bool) {
    if width == 0 {
        return (Line::default(), false);
    }
    let anchor = selected
        .and_then(|id| model.tabs.iter().position(|tab| tab.id == id))
        .unwrap_or(model.active);
    let mut first = anchor;
    let mut last = anchor;
    let mut line = selected_line(model, selected, first, last);
    if line.width() > width {
        let tab = &model.tabs[anchor];
        let active = anchor == model.active;
        let marker = if active {
            "●".to_owned()
        } else {
            (anchor + 1).to_string()
        };
        let mut style = if active {
            active_style()
        } else if tab.closing {
            muted_style()
        } else {
            Style::default()
        };
        if selected == Some(tab.id) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        return (
            Line::styled(single_tab(&marker, &tab.name, tab.closing, width), style),
            false,
        );
    }

    loop {
        let mut changed = false;
        if first > 0 {
            let candidate = selected_line(model, selected, first - 1, last);
            if candidate.width() <= width {
                first -= 1;
                line = candidate;
                changed = true;
            }
        }
        if last + 1 < model.tabs.len() {
            let candidate = selected_line(model, selected, first, last + 1);
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
) -> Line<'static> {
    let mut spans = Vec::new();
    if first > 0 {
        spans.push(Span::styled(" … ", muted_style()));
    }
    for index in first..=last {
        let tab = &model.tabs[index];
        let active = index == model.active;
        let marker = if active {
            "●".to_owned()
        } else {
            (index + 1).to_string()
        };
        let closing = if tab.closing { " ×" } else { "" };
        let text = format!(" {marker} {}{closing} ", tab.name);
        let mut style = if active {
            active_style()
        } else if tab.closing {
            muted_style()
        } else {
            Style::default()
        };
        if selected == Some(tab.id) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled(text, style));
    }
    if last + 1 < model.tabs.len() {
        spans.push(Span::styled(" … ", muted_style()));
    }
    Line::from(spans)
}

fn single_tab(marker: &str, name: &str, closing: bool, width: usize) -> String {
    match width {
        0 => String::new(),
        1 => truncate(marker, 1),
        2 => format!("{} ", truncate(marker, 1)),
        _ => {
            let suffix = if closing && width >= 7 { " × " } else { " " };
            let available = width.saturating_sub(
                2 + UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(suffix),
            );
            format!(" {marker} {}{suffix}", truncate(name, available))
        }
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
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn clear_row(area: Rect, buffer: &mut Buffer) {
    for column in area.x..area.x.saturating_add(area.width) {
        if let Some(cell) = buffer.cell_mut((column, area.y)) {
            cell.reset();
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

    use super::*;
    use crate::{
        domain::{PaneId, SessionId, TabId, TerminalId, WorkspaceId},
        resources::{PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, WorkspaceSnapshot},
    };

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
        render_tab_bar(Some(&snapshot), &focused, false, None, area, &mut buffer);
        let text = (0..width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        (text, buffer)
    }

    #[test]
    fn chrome_layout_composes_tab_and_sidebar_positions_at_the_exact_breakpoint() {
        let top_left = UiConfig::default();
        assert_eq!(
            client_layout(Rect::new(3, 4, 119, 24), top_left),
            ClientLayout {
                tab_bar: Some(Rect::new(3, 4, 119, 1)),
                terminal: Rect::new(3, 5, 119, 23),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Drawer(Rect::new(3, 4, 24, 24,))),
            }
        );
        assert_eq!(
            client_layout(Rect::new(3, 4, 120, 24), top_left),
            ClientLayout {
                tab_bar: Some(Rect::new(27, 4, 96, 1)),
                terminal: Rect::new(27, 5, 96, 23),
                workspace_sidebar: Some(WorkspaceSidebarLayout::Docked(Rect::new(3, 4, 24, 24,))),
            }
        );
        let bottom_right = UiConfig {
            tab_bar_position: TabBarPosition::Bottom,
            workspace_sidebar_position: WorkspaceSidebarPosition::Right,
            pane_layout: crate::client::config::PaneLayoutPolicy::Splits,
        };
        assert_eq!(
            client_layout(Rect::new(3, 4, 120, 24), bottom_right),
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
            client_layout(Rect::new(3, 4, 80, 0), UiConfig::default()),
            ClientLayout {
                tab_bar: None,
                terminal: Rect::new(3, 4, 80, 0),
                workspace_sidebar: None,
            }
        );
        assert_eq!(
            client_layout(Rect::new(3, 4, 120, 1), UiConfig::default()),
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
        assert_eq!(sanitize("bad\nname"), "bad�name");
    }

    #[test]
    fn closing_and_missing_metadata_are_visible_without_panicking() {
        let (mut snapshot, focused) = fixture(&["shell", "closing"], 0);
        snapshot.sessions[0].workspaces[0].tabs[1].closing = true;
        let area = Rect::new(0, 0, 40, 1);
        let mut buffer = Buffer::empty(area);
        render_tab_bar(Some(&snapshot), &focused, false, None, area, &mut buffer);
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(text.contains("closing ×"));

        let mut missing = focused.clone();
        missing.workspace_id = WorkspaceId::new();
        let mut fallback = Buffer::empty(area);
        render_tab_bar(Some(&snapshot), &missing, false, None, area, &mut fallback);
        assert_eq!(fallback[(1, 0)].symbol(), "●");
    }

    #[test]
    fn zoom_status_is_persistent_bold_and_preserves_the_active_tab() {
        let (snapshot, focused) = fixture(&["shell", "editor", "tests"], 1);
        let area = Rect::new(3, 4, 24, 1);
        let mut buffer = Buffer::empty(area);
        render_tab_bar(Some(&snapshot), &focused, true, None, area, &mut buffer);
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
            render_tab_bar(Some(&snapshot), &focused, true, None, area, &mut buffer);
            let text = (0..width)
                .map(|column| buffer[(column, 0)].symbol())
                .collect::<String>();
            let status_width = usize::from(width).min(ZOOM_STATUS.len());
            assert!(text.ends_with(&ZOOM_STATUS[..status_width]));
            assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
            if width == 6 {
                assert!(text.contains('●'));
            }
        }
    }
}
