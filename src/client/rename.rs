use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use crate::{protocol::RenameSelector, resources::ResourceSnapshot};

use super::chrome::{sanitize, truncate};

const MAX_NAME_BYTES: usize = 512;
const MAX_WIDTH: u16 = 52;

pub(super) struct RenameState {
    selector: RenameSelector,
    kind: &'static str,
    name: String,
    request_id: Option<Uuid>,
    acknowledged_revision: Option<u64>,
    observed_revision: u64,
    error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RenameAction {
    Stay,
    Close,
    Submit {
        request_id: Uuid,
        selector: RenameSelector,
        name: String,
    },
}

impl RenameState {
    pub fn open(selector: RenameSelector, kind: &'static str, name: String) -> Self {
        Self {
            selector,
            kind,
            name,
            request_id: None,
            acknowledged_revision: None,
            observed_revision: 0,
            error: None,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> RenameAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || self.request_id.is_some()
        {
            return RenameAction::Stay;
        }
        match key.code {
            KeyCode::Esc => RenameAction::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                RenameAction::Close
            }
            KeyCode::Enter => {
                let cleared = self.name.trim().is_empty();
                // Sessions require a name; tabs and workspaces submit an empty
                // name to return to automatic naming.
                if cleared && matches!(self.selector, RenameSelector::Session(_)) {
                    self.error = Some("name cannot be empty".into());
                    return RenameAction::Stay;
                }
                let request_id = Uuid::new_v4();
                self.request_id = Some(request_id);
                self.acknowledged_revision = None;
                self.error = None;
                RenameAction::Submit {
                    request_id,
                    selector: self.selector.clone(),
                    name: if cleared {
                        String::new()
                    } else {
                        self.name.clone()
                    },
                }
            }
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::SUPER) => {
                self.name.clear();
                self.error = None;
                RenameAction::Stay
            }
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                self.remove_last_word();
                RenameAction::Stay
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.remove_last_word();
                RenameAction::Stay
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.remove_last_grapheme();
                RenameAction::Stay
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.name.clear();
                self.error = None;
                RenameAction::Stay
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
                RenameAction::Stay
            }
            _ => RenameAction::Stay,
        }
    }

    pub fn paste(&mut self, value: &str) {
        if self.request_id.is_some() {
            return;
        }
        for character in value.chars().filter(|character| !character.is_control()) {
            self.append(character);
            if self.name.len() >= MAX_NAME_BYTES {
                break;
            }
        }
    }

    pub fn complete(&mut self, request_id: Option<Uuid>, resource_revision: u64) -> bool {
        if request_id != self.request_id || self.request_id.is_none() {
            return false;
        }
        self.acknowledged_revision = Some(resource_revision);
        self.observed_revision >= resource_revision
    }

    pub fn accept_resources(&mut self, snapshot: &ResourceSnapshot) -> bool {
        if self.request_id.is_none() {
            return false;
        }
        self.observed_revision = self.observed_revision.max(snapshot.revision);
        self.acknowledged_revision
            .is_some_and(|revision| self.observed_revision >= revision)
    }

    pub fn fail(&mut self, request_id: Option<Uuid>, message: String) -> bool {
        if request_id != self.request_id || self.request_id.is_none() {
            return false;
        }
        self.request_id = None;
        self.acknowledged_revision = None;
        self.error = Some(sanitize(&message));
        true
    }

    pub fn render(&self, host: Rect, buffer: &mut Buffer) {
        let area = rename_area(host);
        if area.width == 0 || area.height == 0 {
            return;
        }
        clear(area, buffer);
        let width = usize::from(area.width);
        let title = truncate(&format!(" Rename {}", self.kind), width);
        buffer.set_stringn(area.x, area.y, title, width, title_style());
        if area.height >= 2 {
            let available = width.saturating_sub(4);
            let name = trailing_view(&sanitize(&self.name), available);
            buffer.set_stringn(
                area.x,
                area.y + 1,
                format!(" > {name}"),
                width,
                Style::default(),
            );
            let cursor_x = area
                .x
                .saturating_add(3)
                .saturating_add(
                    u16::try_from(UnicodeWidthStr::width(name.as_str())).unwrap_or(u16::MAX),
                )
                .min(area.x.saturating_add(area.width - 1));
            if let Some(cell) = buffer.cell_mut((cursor_x, area.y + 1)) {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
        if area.height >= 3 {
            let status = if self.request_id.is_some() {
                " renaming…"
            } else if let Some(error) = self.error.as_deref() {
                error
            } else if self.name.trim().is_empty()
                && !matches!(self.selector, RenameSelector::Session(_))
            {
                " enter clear name · esc cancel"
            } else {
                " enter rename · esc cancel"
            };
            buffer.set_stringn(
                area.x,
                area.y + area.height - 1,
                truncate(status, width),
                width,
                muted_style(),
            );
        }
    }

    fn append(&mut self, character: char) {
        if self.name.len().saturating_add(character.len_utf8()) <= MAX_NAME_BYTES {
            self.name.push(character);
            self.error = None;
        }
    }

    fn remove_last_grapheme(&mut self) {
        if let Some((index, _)) = self.name.grapheme_indices(true).next_back() {
            self.name.truncate(index);
            self.error = None;
        }
    }

    /// Delete the last word and any whitespace after it, as Option-Backspace
    /// (ESC DEL) and Ctrl-W do in a shell.
    fn remove_last_word(&mut self) {
        let end = self.name.trim_end().len();
        let start = self.name[..end]
            .char_indices()
            .rev()
            .take_while(|(_, character)| !character.is_whitespace())
            .last()
            .map_or(end, |(index, _)| index);
        self.name.truncate(start);
        self.error = None;
    }
}

fn trailing_view(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut suffix = Vec::new();
    let mut used = 0;
    for grapheme in value.graphemes(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > width - 1 {
            break;
        }
        suffix.push(grapheme);
        used += grapheme_width;
    }
    suffix.reverse();
    format!("…{}", suffix.concat())
}

fn rename_area(host: Rect) -> Rect {
    let width = host.width.min(MAX_WIDTH);
    let height = host.height.min(3);
    Rect::new(
        host.x.saturating_add(host.width.saturating_sub(width) / 2),
        host.y
            .saturating_add(host.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

fn clear(area: Rect, buffer: &mut Buffer) {
    for row in area.y..area.y.saturating_add(area.height) {
        for column in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buffer.cell_mut((column, row)) {
                cell.reset();
            }
        }
    }
}

fn title_style() -> Style {
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
        domain::{SessionId, WorkspaceId},
        resources::{Project, ProjectIdentity, SessionSnapshot, WorkspaceSnapshot},
    };

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn editing_submit_error_and_retry_stay_correlated() {
        let mut rename = RenameState::open(
            RenameSelector::Workspace(WorkspaceId::new()),
            "workspace",
            "main".into(),
        );
        rename.key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        rename.paste("feature\nλ");
        let RenameAction::Submit { request_id, .. } =
            rename.key(key(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("rename did not submit")
        };
        assert_eq!(
            rename.key(key(KeyCode::Esc, KeyModifiers::NONE)),
            RenameAction::Stay
        );
        assert!(!rename.fail(Some(Uuid::new_v4()), "wrong".into()));
        assert!(rename.fail(Some(request_id), "duplicate name".into()));
        assert!(matches!(
            rename.key(key(KeyCode::Enter, KeyModifiers::NONE)),
            RenameAction::Submit { .. }
        ));
    }

    #[test]
    fn success_waits_for_acknowledgement_and_authoritative_name() {
        let workspace_id = WorkspaceId::new();
        let mut snapshot = ResourceSnapshot {
            revision: 1,
            sessions: vec![SessionSnapshot {
                tokens: Default::default(),
                id: SessionId::new(),
                name: "project".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/project")),
                },
                trusted_project_config: None,
                closing: false,
                workspaces: vec![WorkspaceSnapshot {
                    tokens: Default::default(),
                    id: workspace_id,
                    name: "main".into(),
                    root: PathBuf::from("/project"),
                    closing: false,
                    tabs: Vec::new(),
                }],
            }],
        };
        let mut rename = RenameState::open(
            RenameSelector::Workspace(workspace_id),
            "workspace",
            "main".into(),
        );
        rename.key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        rename.paste("feature");
        let RenameAction::Submit { request_id, .. } =
            rename.key(key(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("rename did not submit")
        };

        assert!(!rename.accept_resources(&snapshot));
        assert!(!rename.complete(Some(request_id), 2));
        snapshot.revision = 2;
        snapshot.sessions[0].workspaces[0].name = "feature".into();
        assert!(rename.accept_resources(&snapshot));
    }

    #[test]
    fn empty_submissions_clear_tabs_and_workspaces_but_not_sessions() {
        let mut workspace = RenameState::open(
            RenameSelector::Workspace(WorkspaceId::new()),
            "workspace",
            "  ".into(),
        );
        let RenameAction::Submit { name, .. } =
            workspace.key(key(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("blank workspace rename did not submit")
        };
        assert_eq!(name, "", "whitespace normalizes to a cleared name");

        let mut session = RenameState::open(
            RenameSelector::Session(crate::resources::SessionSelector::Id(SessionId::new())),
            "session",
            String::new(),
        );
        assert_eq!(
            session.key(key(KeyCode::Enter, KeyModifiers::NONE)),
            RenameAction::Stay,
            "sessions still require a name"
        );
    }

    #[test]
    fn word_and_line_deletion_edit_like_a_shell() {
        let mut rename = RenameState::open(
            RenameSelector::Tab(crate::domain::TabId::new()),
            "tab",
            "several words  here   ".into(),
        );
        rename.key(key(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(rename.name, "several words  ");
        rename.key(key(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(rename.name, "several ");
        rename.key(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(rename.name, "");

        rename.paste("whole line");
        rename.key(key(KeyCode::Backspace, KeyModifiers::SUPER));
        assert_eq!(rename.name, "");
    }

    #[test]
    fn long_names_keep_the_edited_suffix_visible() {
        assert_eq!(trailing_view("abcdefghijkl", 6), "…hijkl");
        assert_eq!(trailing_view("👩🏽‍💻abcdef", 5), "…cdef");
    }
}
