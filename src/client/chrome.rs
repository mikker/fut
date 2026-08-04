use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, TabSnapshot},
};

use super::config::TabBarPosition;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ClientLayout {
    pub terminal: Rect,
    pub tab_bar: Option<Rect>,
}

pub(super) fn client_layout(host: Rect, position: TabBarPosition) -> ClientLayout {
    if host.width == 0 || host.height < 2 {
        return ClientLayout {
            terminal: host,
            tab_bar: None,
        };
    }

    match position {
        TabBarPosition::Top => ClientLayout {
            terminal: Rect::new(
                host.x,
                host.y.saturating_add(1),
                host.width,
                host.height - 1,
            ),
            tab_bar: Some(Rect::new(host.x, host.y, host.width, 1)),
        },
        TabBarPosition::Bottom => ClientLayout {
            terminal: Rect::new(host.x, host.y, host.width, host.height - 1),
            tab_bar: Some(Rect::new(
                host.x,
                host.y.saturating_add(host.height - 1),
                host.width,
                1,
            )),
        },
    }
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
            name: sanitize(&tab.name),
            closing: tab.closing,
        }
    }
}

pub(super) fn render_tab_bar(
    snapshot: Option<&ResourceSnapshot>,
    focused: &SelectedTarget,
    area: Rect,
    buffer: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    clear_row(area, buffer);
    let Some(model) = snapshot.and_then(|snapshot| TabBarModel::from_snapshot(snapshot, focused))
    else {
        let fallback = active_only("tab", false, usize::from(area.width));
        buffer.set_line(
            area.x,
            area.y,
            &Line::styled(fallback, active_style()),
            area.width,
        );
        return;
    };

    let width = usize::from(area.width);
    let (line, complete) = visible_tabs(&model, width);
    buffer.set_line(area.x, area.y, &line, area.width);

    const BRAND: &str = "fut ";
    let brand_width = UnicodeWidthStr::width(BRAND);
    if complete && line.width().saturating_add(brand_width) <= width {
        buffer.set_string(
            area.x + area.width - u16::try_from(brand_width).expect("brand width fits u16"),
            area.y,
            BRAND,
            muted_style(),
        );
    }
}

fn visible_tabs(model: &TabBarModel, width: usize) -> (Line<'static>, bool) {
    if width == 0 {
        return (Line::default(), false);
    }
    let mut first = model.active;
    let mut last = model.active;
    let mut line = selected_line(model, first, last);
    if line.width() > width {
        return (
            Line::styled(
                active_only(
                    &model.tabs[model.active].name,
                    model.tabs[model.active].closing,
                    width,
                ),
                active_style(),
            ),
            false,
        );
    }

    loop {
        let mut changed = false;
        if first > 0 {
            let candidate = selected_line(model, first - 1, last);
            if candidate.width() <= width {
                first -= 1;
                line = candidate;
                changed = true;
            }
        }
        if last + 1 < model.tabs.len() {
            let candidate = selected_line(model, first, last + 1);
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

fn selected_line(model: &TabBarModel, first: usize, last: usize) -> Line<'static> {
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
        let style = if active {
            active_style()
        } else if tab.closing {
            muted_style()
        } else {
            Style::default()
        };
        spans.push(Span::styled(text, style));
    }
    if last + 1 < model.tabs.len() {
        spans.push(Span::styled(" … ", muted_style()));
    }
    Line::from(spans)
}

fn active_only(name: &str, closing: bool, width: usize) -> String {
    match width {
        0 => String::new(),
        1 => "●".into(),
        2 => "● ".into(),
        _ => {
            let suffix = if closing && width >= 7 { " × " } else { " " };
            let available = width.saturating_sub(3 + UnicodeWidthStr::width(suffix));
            format!(" ● {}{suffix}", truncate(name, available))
        }
    }
}

fn truncate(value: &str, width: usize) -> String {
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

fn sanitize(value: &str) -> String {
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
            .map(|name| TabSnapshot {
                id: TabId::new(),
                name: (*name).into(),
                closing: false,
                panes: vec![PaneSnapshot {
                    id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                    closing: false,
                }],
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
        render_tab_bar(Some(&snapshot), &focused, area, &mut buffer);
        let text = (0..width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        (text, buffer)
    }

    #[test]
    fn chrome_layout_reserves_one_row_at_either_edge_and_suppresses_it_when_tiny() {
        assert_eq!(
            client_layout(Rect::new(3, 4, 80, 24), TabBarPosition::Top),
            ClientLayout {
                tab_bar: Some(Rect::new(3, 4, 80, 1)),
                terminal: Rect::new(3, 5, 80, 23),
            }
        );
        assert_eq!(
            client_layout(Rect::new(3, 4, 80, 24), TabBarPosition::Bottom),
            ClientLayout {
                tab_bar: Some(Rect::new(3, 27, 80, 1)),
                terminal: Rect::new(3, 4, 80, 23),
            }
        );
        for height in [0, 1] {
            let host = Rect::new(3, 4, 80, height);
            assert_eq!(
                client_layout(host, TabBarPosition::Top),
                ClientLayout {
                    tab_bar: None,
                    terminal: host,
                }
            );
        }
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
        render_tab_bar(Some(&snapshot), &focused, area, &mut buffer);
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(text.contains("closing ×"));

        let mut missing = focused.clone();
        missing.workspace_id = WorkspaceId::new();
        let mut fallback = Buffer::empty(area);
        render_tab_bar(Some(&snapshot), &missing, area, &mut fallback);
        assert_eq!(fallback[(1, 0)].symbol(), "●");
    }
}
