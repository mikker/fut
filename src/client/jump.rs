//! Type-to-filter quick switcher: one flat, hierarchically labelled row per
//! session, workspace, tab, and pane, filtered by a typed query. The navigator
//! keeps the structural letter keys; this dialog spends them on the query.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    domain::PaneId,
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, TabSnapshot},
};

use super::{
    chrome::{sanitize, truncate},
    dialog::{
        dialog_area, fill_row, muted_style, render_frame, render_list_scrollbar, row_style,
        title_style,
    },
};

const MAX_QUERY_BYTES: usize = 512;
const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct JumpRow {
    pub pane_id: PaneId,
    pub label: String,
    pub current: bool,
}

pub(super) struct JumpState {
    rows: Vec<JumpRow>,
    query: String,
    filtered: Vec<usize>,
    selected: Option<usize>,
    scroll: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JumpAction {
    Stay,
    Close,
    Select(PaneId),
}

impl JumpState {
    pub(super) fn open(snapshot: &ResourceSnapshot, current: &SelectedTarget) -> Self {
        let mut state = Self {
            rows: rows(snapshot, current),
            query: String::new(),
            filtered: Vec::new(),
            selected: None,
            scroll: 0,
        };
        state.refilter(None);
        state
    }

    pub(super) fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        current: &SelectedTarget,
    ) {
        let label = self.selected_row().map(|row| row.label.clone());
        self.rows = rows(snapshot, current);
        self.refilter(None);
        if let Some(label) = label
            && let Some(position) = self
                .filtered
                .iter()
                .position(|&index| self.rows[index].label == label)
        {
            self.selected = Some(position);
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> JumpAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return JumpAction::Stay;
        }
        match key.code {
            KeyCode::Esc => JumpAction::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                JumpAction::Close
            }
            KeyCode::Enter => self
                .selected_row()
                .map(|row| JumpAction::Select(row.pane_id))
                .unwrap_or(JumpAction::Stay),
            KeyCode::Up | KeyCode::BackTab => {
                self.move_selection(-1);
                JumpAction::Stay
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_selection(1);
                JumpAction::Stay
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-1);
                JumpAction::Stay
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(1);
                JumpAction::Stay
            }
            KeyCode::Home => {
                self.selected = (!self.filtered.is_empty()).then_some(0);
                JumpAction::Stay
            }
            KeyCode::End => {
                self.selected = self.filtered.len().checked_sub(1);
                JumpAction::Stay
            }
            KeyCode::PageUp => {
                self.move_selection(-5);
                JumpAction::Stay
            }
            KeyCode::PageDown => {
                self.move_selection(5);
                JumpAction::Stay
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.remove_last_grapheme();
                JumpAction::Stay
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.refilter(None);
                JumpAction::Stay
            }
            KeyCode::Char(character)
                if !character.is_control()
                    && !key.modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
            {
                self.append(character);
                JumpAction::Stay
            }
            _ => JumpAction::Stay,
        }
    }

    pub(super) fn paste(&mut self, value: &str) {
        for character in value.chars() {
            if character.is_control() {
                continue;
            }
            self.append_raw(character);
            if self.query.len() >= MAX_QUERY_BYTES {
                break;
            }
        }
        self.refilter(None);
    }

    pub(super) fn render(&mut self, host: Rect, buffer: &mut Buffer) {
        let area = render_frame(dialog_area(host, MAX_WIDTH, MAX_HEIGHT), buffer);
        if area.width == 0 || area.height == 0 {
            return;
        }
        render_prompt(area, &self.query, self.selected_position(), buffer);
        if area.height == 1 {
            return;
        }
        let footer = (area.height >= 4).then_some(area.y + area.height - 1);
        let body_height = usize::from(area.height - 1 - u16::from(footer.is_some()));
        let body = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            u16::try_from(body_height).expect("body height fits u16"),
        );
        self.keep_selected_visible(body_height);
        if self.filtered.is_empty() {
            buffer.set_stringn(
                body.x,
                body.y,
                " No matching resources",
                usize::from(body.width),
                muted_style(),
            );
        } else {
            for (line, index) in self
                .filtered
                .iter()
                .skip(self.scroll)
                .take(body_height)
                .copied()
                .enumerate()
            {
                let row = &self.rows[index];
                let style = row_style(self.selected == Some(self.scroll + line));
                let text = format!(" {} {}", if row.current { "•" } else { " " }, row.label);
                let y = body.y + u16::try_from(line).expect("visible line fits u16");
                let row_area = Rect::new(body.x, y, body.width, 1);
                fill_row(row_area, style, buffer);
                buffer.set_stringn(
                    body.x,
                    y,
                    truncate(&text, usize::from(body.width)),
                    usize::from(body.width),
                    style,
                );
            }
            render_list_scrollbar(self.scroll, self.filtered.len(), body, buffer);
        }
        if let Some(row) = footer {
            buffer.set_stringn(
                area.x,
                row,
                " type to filter · ↑↓ choose · enter switch · esc close",
                usize::from(area.width),
                muted_style(),
            );
        }
    }

    fn append(&mut self, character: char) {
        let selected = self.selected_index();
        self.append_raw(character);
        self.refilter(selected);
    }

    fn append_raw(&mut self, character: char) {
        if self.query.len() + character.len_utf8() <= MAX_QUERY_BYTES {
            self.query.push(character);
        }
    }

    fn remove_last_grapheme(&mut self) {
        let selected = self.selected_index();
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(index);
            self.refilter(selected);
        }
    }

    fn refilter(&mut self, preserve: Option<usize>) {
        let tokens = self
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        self.filtered = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                let haystack = row.label.to_lowercase();
                tokens.iter().all(|token| haystack.contains(token))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = preserve
            .and_then(|row| self.filtered.iter().position(|&index| index == row))
            .or_else(|| (!self.filtered.is_empty()).then_some(0));
        self.scroll = 0;
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected
            .and_then(|index| self.filtered.get(index))
            .copied()
    }

    fn selected_row(&self) -> Option<&JumpRow> {
        self.selected_index().map(|index| &self.rows[index])
    }

    fn selected_position(&self) -> Option<(usize, usize)> {
        self.selected.map(|index| (index + 1, self.filtered.len()))
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(selected) = self.selected else {
            return;
        };
        self.selected = Some(
            selected
                .saturating_add_signed(delta)
                .min(self.filtered.len().saturating_sub(1)),
        );
    }

    fn keep_selected_visible(&mut self, height: usize) {
        let Some(selected) = self.selected else {
            self.scroll = 0;
            return;
        };
        if height == 0 {
            return;
        }
        if selected < self.scroll {
            self.scroll = selected;
        } else if selected >= self.scroll + height {
            self.scroll = selected + 1 - height;
        }
    }
}

fn render_prompt(area: Rect, query: &str, position: Option<(usize, usize)>, buffer: &mut Buffer) {
    let row = Rect::new(area.x, area.y, area.width, 1);
    fill_row(row, title_style(), buffer);
    let text = if query.is_empty() {
        "› Jump to…".to_owned()
    } else {
        format!("› {}", sanitize(query))
    };
    let count = position.and_then(|(selected, total)| {
        let text = format!(" {selected}/{total} ");
        let width = UnicodeWidthStr::width(text.as_str());
        (width + 4 <= usize::from(area.width)).then_some((text, width))
    });
    let prompt_width = usize::from(area.width)
        .saturating_sub(count.as_ref().map(|(_, width)| *width).unwrap_or(0));
    buffer.set_stringn(
        area.x,
        area.y,
        truncate(&text, prompt_width),
        prompt_width,
        title_style(),
    );
    if let Some((count, count_width)) = count {
        buffer.set_stringn(
            area.x + area.width - u16::try_from(count_width).expect("count width fits u16"),
            area.y,
            count,
            count_width,
            title_style().add_modifier(Modifier::DIM),
        );
    }
}

/// Every switchable resource as its own row, labelled by its path through the
/// tree. Closing resources are left out; a quick switcher only offers places
/// worth landing in.
fn rows(snapshot: &ResourceSnapshot, current: &SelectedTarget) -> Vec<JumpRow> {
    let mut rows = Vec::new();
    let mut push = |label: String, pane_id: Option<PaneId>, current: bool| {
        if let Some(pane_id) = pane_id {
            rows.push(JumpRow {
                pane_id,
                label,
                current,
            });
        }
    };
    for session in snapshot.sessions.iter().filter(|session| !session.closing) {
        let workspaces = session
            .workspaces
            .iter()
            .filter(|workspace| !workspace.closing);
        push(
            session.name.clone(),
            workspaces
                .clone()
                .find_map(|workspace| first_pane(&workspace.tabs)),
            false,
        );
        for workspace in workspaces {
            let path = format!("{} › {}", session.name, workspace.name);
            push(path.clone(), first_pane(&workspace.tabs), false);
            for tab in workspace.tabs.iter().filter(|tab| !tab.closing) {
                let path = format!("{path} › {}", tab.name);
                push(path.clone(), first_pane(std::slice::from_ref(tab)), false);
                for (index, pane) in tab
                    .panes
                    .iter()
                    .enumerate()
                    .filter(|(_, pane)| !pane.closing)
                {
                    push(
                        format!("{path} › pane {}", index + 1),
                        Some(pane.id),
                        pane.id == current.pane_id,
                    );
                }
            }
        }
    }
    rows
}

fn first_pane(tabs: &[TabSnapshot]) -> Option<PaneId> {
    tabs.iter()
        .filter(|tab| !tab.closing)
        .flat_map(|tab| &tab.panes)
        .find(|pane| !pane.closing)
        .map(|pane| pane.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{PaneId, SessionId, TabId, TerminalId, WorkspaceId},
        resources::{PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, WorkspaceSnapshot},
        splits::SplitTree,
    };
    use std::path::PathBuf;

    fn pane() -> PaneSnapshot {
        PaneSnapshot {
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing: false,
            activity: Default::default(),
        }
    }

    fn tab(name: &str, panes: Vec<PaneSnapshot>) -> TabSnapshot {
        TabSnapshot {
            id: TabId::new(),
            name: name.into(),
            closing: false,
            layout: SplitTree::leaf(panes[0].id),
            panes,
        }
    }

    fn workspace(name: &str, tabs: Vec<TabSnapshot>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: WorkspaceId::new(),
            name: name.into(),
            root: PathBuf::from("/tmp"),
            closing: false,
            tabs,
        }
    }

    fn session(name: &str, workspaces: Vec<WorkspaceSnapshot>) -> SessionSnapshot {
        SessionSnapshot {
            id: SessionId::new(),
            name: name.into(),
            project: Project {
                identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/tmp")),
            },
            closing: false,
            workspaces,
        }
    }

    /// fut(main(agent[P1 P2] server[P3])) demo(review(editor[P4]))
    fn fixture() -> (ResourceSnapshot, SelectedTarget) {
        let snapshot = ResourceSnapshot {
            revision: 1,
            sessions: vec![
                session(
                    "fut",
                    vec![workspace(
                        "main",
                        vec![
                            tab("agent", vec![pane(), pane()]),
                            tab("server", vec![pane()]),
                        ],
                    )],
                ),
                session(
                    "demo",
                    vec![workspace("review", vec![tab("editor", vec![pane()])])],
                ),
            ],
        };
        let session = &snapshot.sessions[0];
        let workspace = &session.workspaces[0];
        let tab = &workspace.tabs[0];
        let current = SelectedTarget {
            session_id: session.id,
            workspace_id: workspace.id,
            tab_id: tab.id,
            pane_id: tab.panes[0].id,
            terminal_id: tab.panes[0].terminal_id,
            child_pid: 1,
        };
        (snapshot, current)
    }

    fn labels(jump: &JumpState) -> Vec<String> {
        jump.filtered
            .iter()
            .map(|&index| jump.rows[index].label.clone())
            .collect()
    }

    fn text(buffer: &Buffer) -> String {
        let area = buffer.area;
        (area.y..area.y + area.height)
            .map(|row| {
                (area.x..area.x + area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn rows_are_flat_hierarchical_paths_for_every_resource() {
        let (snapshot, current) = fixture();
        let jump = JumpState::open(&snapshot, &current);
        assert_eq!(
            labels(&jump),
            [
                "fut",
                "fut › main",
                "fut › main › agent",
                "fut › main › agent › pane 1",
                "fut › main › agent › pane 2",
                "fut › main › server",
                "fut › main › server › pane 1",
                "demo",
                "demo › review",
                "demo › review › editor",
                "demo › review › editor › pane 1",
            ]
        );
        // Ancestors switch to the first pane inside them.
        let first_pane = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;
        assert_eq!(jump.rows[0].pane_id, first_pane);
        assert!(jump.rows[3].current, "the focused pane is marked");
        assert!(!jump.rows[4].current);
    }

    #[test]
    fn closing_resources_are_left_out_and_ancestors_skip_them() {
        let (mut snapshot, current) = fixture();
        snapshot.sessions[0].workspaces[0].tabs[0].closing = true;
        snapshot.sessions[1].closing = true;

        let jump = JumpState::open(&snapshot, &current);

        assert_eq!(
            labels(&jump),
            [
                "fut",
                "fut › main",
                "fut › main › server",
                "fut › main › server › pane 1"
            ]
        );
        assert_eq!(
            jump.rows[0].pane_id,
            snapshot.sessions[0].workspaces[0].tabs[1].panes[0].id
        );
    }

    #[test]
    fn filtering_is_case_insensitive_multi_token_and_enter_switches() {
        let (snapshot, current) = fixture();
        let mut jump = JumpState::open(&snapshot, &current);

        jump.paste("MAIN age");
        assert_eq!(
            labels(&jump),
            [
                "fut › main › agent",
                "fut › main › agent › pane 1",
                "fut › main › agent › pane 2",
            ]
        );
        jump.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            jump.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            JumpAction::Select(snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id)
        );

        jump.paste("nothing");
        assert!(jump.filtered.is_empty());
        assert_eq!(
            jump.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            JumpAction::Stay
        );
        assert_eq!(
            jump.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            JumpAction::Close
        );
    }

    #[test]
    fn letters_type_into_the_query_and_ctrl_keys_edit_and_move() {
        let (snapshot, current) = fixture();
        let mut jump = JumpState::open(&snapshot, &current);

        for character in ['d', 'e', 'm', 'o'] {
            jump.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(jump.query, "demo");
        assert_eq!(labels(&jump).len(), 4);
        jump.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(jump.selected, Some(1));
        jump.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(jump.selected, Some(0));
        jump.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(jump.query, "dem");
        jump.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(jump.query.is_empty());
        assert_eq!(labels(&jump).len(), 11);
    }

    #[test]
    fn refreshed_resources_keep_the_selected_row() {
        let (mut snapshot, current) = fixture();
        let mut jump = JumpState::open(&snapshot, &current);
        jump.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let selected = jump
            .selected_row()
            .expect("a row is selected")
            .label
            .clone();

        snapshot.sessions.insert(
            0,
            session(
                "extra",
                vec![workspace("root", vec![tab("shell", vec![pane()])])],
            ),
        );
        jump.accept_resources(&snapshot, &current);

        assert_eq!(
            jump.selected_row().map(|row| row.label.clone()),
            Some(selected)
        );
    }

    #[test]
    fn rendering_shows_the_prompt_rows_and_help_without_panicking() {
        let (snapshot, current) = fixture();
        let host = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(host);
        let mut jump = JumpState::open(&snapshot, &current);
        jump.render(host, &mut buffer);
        let rendered = text(&buffer);
        assert!(rendered.contains("Jump to…"));
        assert!(rendered.contains("fut › main › agent"));
        assert!(rendered.contains("type to filter"));
        assert!(rendered.contains("1/11"));

        for width in 0..40 {
            for height in 0..8 {
                let host = Rect::new(0, 0, width, height);
                let mut buffer = Buffer::empty(host);
                let mut jump = JumpState::open(&snapshot, &current);
                jump.paste("no such resource");
                jump.render(host, &mut buffer);
            }
        }
    }
}
