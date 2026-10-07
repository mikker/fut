use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use crate::{protocol::RenameSelector, resources::ResourceSnapshot};

use super::{
    chrome::sanitize,
    config::{SemanticStyle, StylesConfig},
    dialog::{dialog_area, fill_row, render_frame, style_frame},
};

const MAX_NAME_BYTES: usize = 512;
const MAX_WIDTH: u16 = 52;
const HEIGHT: u16 = 6;

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

    pub fn render(&self, host: Rect, styles: &StylesConfig, buffer: &mut Buffer) {
        let outer = dialog_area(host, MAX_WIDTH, HEIGHT);
        let area = render_frame(outer, buffer);
        if area.width < 3 || area.height < 4 {
            return;
        }
        let accent = styles.apply(SemanticStyle::Current, Style::default()).fg;
        let title_style = Style {
            fg: accent,
            ..Style::default()
        }
        .add_modifier(Modifier::BOLD);
        style_frame(
            outer,
            &format!("Rename {}", self.kind),
            styles.apply(SemanticStyle::Divider, Style::default()),
            title_style,
            buffer,
        );

        let content = Rect::new(area.x + 1, area.y, area.width - 2, area.height);
        let field = Rect::new(content.x, content.y + 1, content.width, 1);
        let field_style = styles.apply(SemanticStyle::Selected, Style::default());
        fill_row(field, field_style, buffer);
        let width = usize::from(field.width);
        let name = trailing_view(&sanitize(&self.name), width.saturating_sub(2));
        buffer.set_stringn(field.x, field.y, format!(" {name}"), width, field_style);
        let cursor_x = field
            .x
            .saturating_add(1)
            .saturating_add(
                u16::try_from(UnicodeWidthStr::width(name.as_str())).unwrap_or(u16::MAX),
            )
            .min(field.x.saturating_add(field.width - 1));
        if let Some(cell) = buffer.cell_mut((cursor_x, field.y)) {
            cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
        }

        let muted = styles.apply(SemanticStyle::Muted, Style::default());
        let footer = if self.request_id.is_some() {
            Line::from(Span::styled("renaming…", muted))
        } else if let Some(error) = self.error.as_deref() {
            Line::from(Span::styled(
                error.trim().to_owned(),
                styles.apply(SemanticStyle::Error, Style::default()),
            ))
        } else {
            let action = if self.name.trim().is_empty()
                && !matches!(self.selector, RenameSelector::Session(_))
            {
                "clear name"
            } else {
                "rename"
            };
            let key = styles
                .apply(SemanticStyle::Normal, Style::default())
                .add_modifier(Modifier::BOLD);
            Line::from(vec![
                Span::styled("⏎", key),
                Span::styled(format!(" {action}   "), muted),
                Span::styled("esc", key),
                Span::styled(" cancel", muted),
            ])
        };
        buffer.set_line(content.x + 1, content.y + 3, &footer, content.width - 1);
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
    fn render_titles_the_border_and_themes_the_field_and_hints() {
        let rename = RenameState::open(
            RenameSelector::Workspace(WorkspaceId::new()),
            "workspace",
            "main".into(),
        );
        let styles = StylesConfig::default();
        let host = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(host);
        rename.render(host, &styles, &mut buffer);

        let row = |y: u16| -> String {
            (4..56)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect()
        };
        assert!(row(4).starts_with("╭─ Rename workspace ─") && row(4).ends_with("─╮"));
        assert_eq!(
            buffer[(4, 4)].fg,
            styles
                .apply(SemanticStyle::Divider, Style::default())
                .fg
                .unwrap()
        );
        assert!(buffer[(7, 4)].modifier.contains(Modifier::BOLD));
        assert_eq!(row(5).trim_matches(['│', ' ']), "");
        assert!(row(6).starts_with("│  main"));
        let field = styles.apply(SemanticStyle::Selected, Style::default());
        assert_eq!(buffer[(6, 6)].bg, field.bg.unwrap());
        assert_eq!(buffer[(53, 6)].bg, field.bg.unwrap());
        assert!(
            buffer[(11, 6)].modifier.contains(Modifier::REVERSED),
            "cursor"
        );
        assert!(row(8).starts_with("│  ⏎ rename   esc cancel"));
        assert!(buffer[(7, 8)].modifier.contains(Modifier::BOLD));
        assert!(row(9).starts_with("╰─") && row(9).ends_with("─╯"));
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
                    parent_workspace_id: None,
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
