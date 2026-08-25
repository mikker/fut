use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    chrome::{sanitize, truncate},
    config::ProjectCatalog,
    dialog::{
        dialog_area, fill_row, muted_style, render_frame, render_list_scrollbar, row_style,
        title_style,
    },
    fuzzy,
};

const MAX_QUERY_BYTES: usize = 4096;
const MAX_WIDTH: u16 = 88;
const MAX_HEIGHT: u16 = 22;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ProjectOpenChoice {
    Configured { name: String, path: PathBuf },
    Path(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ProjectOpenerAction {
    Stay,
    Close,
    Submit(ProjectOpenChoice),
    Approve {
        project: String,
        cwd: PathBuf,
        digest: String,
    },
}

#[derive(Clone, Debug)]
struct ProjectEntry {
    name: String,
    path: PathBuf,
}

#[derive(Clone, Debug)]
enum Phase {
    Browsing,
    Approval {
        project: String,
        cwd: PathBuf,
        path: PathBuf,
        digest: String,
        recipe: Vec<String>,
        scroll: usize,
    },
    Opening {
        request_id: Uuid,
    },
    AwaitingSelection {
        request_id: Uuid,
    },
    AwaitingSnapshot,
}

pub(super) struct ProjectOpenerState {
    base: PathBuf,
    entries: Vec<ProjectEntry>,
    query: String,
    filtered: Vec<usize>,
    selected: usize,
    scroll: usize,
    error: Option<String>,
    phase: Phase,
}

impl ProjectOpenerState {
    pub(super) fn open(catalog: &ProjectCatalog, base: PathBuf) -> Self {
        let entries = catalog
            .iter()
            .map(|(name, project)| ProjectEntry {
                name: name.to_owned(),
                path: project.path().to_owned(),
            })
            .collect::<Vec<_>>();
        let filtered = (0..entries.len()).collect();
        Self {
            base,
            entries,
            query: String::new(),
            filtered,
            selected: 0,
            scroll: 0,
            error: None,
            phase: Phase::Browsing,
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent, visible_rows: usize) -> ProjectOpenerAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ProjectOpenerAction::Stay;
        }
        match &mut self.phase {
            Phase::Opening { .. } | Phase::AwaitingSelection { .. } | Phase::AwaitingSnapshot => {
                ProjectOpenerAction::Stay
            }
            Phase::Approval {
                project,
                cwd,
                digest,
                recipe,
                scroll,
                ..
            } => match key.code {
                KeyCode::Char('y' | 'Y') => ProjectOpenerAction::Approve {
                    project: project.clone(),
                    cwd: cwd.clone(),
                    digest: digest.clone(),
                },
                KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                    self.phase = Phase::Browsing;
                    ProjectOpenerAction::Stay
                }
                KeyCode::Up => {
                    *scroll = scroll.saturating_sub(1);
                    ProjectOpenerAction::Stay
                }
                KeyCode::Down => {
                    *scroll = (*scroll + 1).min(recipe.len().saturating_sub(1));
                    ProjectOpenerAction::Stay
                }
                KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(visible_rows.max(1));
                    ProjectOpenerAction::Stay
                }
                KeyCode::PageDown => {
                    *scroll = (*scroll + visible_rows.max(1)).min(recipe.len().saturating_sub(1));
                    ProjectOpenerAction::Stay
                }
                _ => ProjectOpenerAction::Stay,
            },
            Phase::Browsing => self.browsing_key(key, visible_rows),
        }
    }

    fn browsing_key(&mut self, key: KeyEvent, visible_rows: usize) -> ProjectOpenerAction {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => ProjectOpenerAction::Close,
            (KeyCode::Char('c'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                ProjectOpenerAction::Close
            }
            (KeyCode::Enter, _) => self
                .selected_choice()
                .map(ProjectOpenerAction::Submit)
                .unwrap_or(ProjectOpenerAction::Stay),
            (KeyCode::Up | KeyCode::BackTab, _) => {
                self.move_selection(-1, visible_rows);
                ProjectOpenerAction::Stay
            }
            (KeyCode::Down | KeyCode::Tab, _) => {
                self.move_selection(1, visible_rows);
                ProjectOpenerAction::Stay
            }
            (KeyCode::Char('p'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-1, visible_rows);
                ProjectOpenerAction::Stay
            }
            (KeyCode::Char('n'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(1, visible_rows);
                ProjectOpenerAction::Stay
            }
            (KeyCode::Home, _) => {
                self.selected = 0;
                self.keep_visible(visible_rows);
                ProjectOpenerAction::Stay
            }
            (KeyCode::End, _) => {
                self.selected = self.choice_count().saturating_sub(1);
                self.keep_visible(visible_rows);
                ProjectOpenerAction::Stay
            }
            (KeyCode::Backspace | KeyCode::Delete, _) => {
                self.remove_last_grapheme();
                ProjectOpenerAction::Stay
            }
            (KeyCode::Char('u'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.refilter();
                ProjectOpenerAction::Stay
            }
            (KeyCode::Char(character), modifiers)
                if !character.is_control()
                    && !modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
            {
                self.append(character);
                ProjectOpenerAction::Stay
            }
            _ => ProjectOpenerAction::Stay,
        }
    }

    pub(super) fn paste(&mut self, value: &str) {
        if !matches!(self.phase, Phase::Browsing) {
            return;
        }
        for character in value.chars().filter(|character| !character.is_control()) {
            if self.query.len() + character.len_utf8() > MAX_QUERY_BYTES {
                break;
            }
            self.query.push(character);
        }
        self.refilter();
    }

    pub(super) fn confirm_approval(
        &mut self,
        project: String,
        cwd: PathBuf,
        path: PathBuf,
        digest: String,
        source: &str,
    ) {
        self.error = None;
        self.phase = Phase::Approval {
            project,
            cwd,
            path,
            digest,
            recipe: source.lines().map(str::to_owned).collect(),
            scroll: 0,
        };
    }

    pub(super) fn begin_open(&mut self, request_id: Uuid) {
        self.error = None;
        self.phase = Phase::Opening { request_id };
    }

    pub(super) fn show_error(&mut self, message: String) {
        self.phase = Phase::Browsing;
        self.error = Some(message);
    }

    pub(super) fn opened(&mut self, request_id: Option<Uuid>) -> bool {
        let Phase::Opening {
            request_id: expected,
        } = self.phase
        else {
            return false;
        };
        if request_id != Some(expected) {
            return false;
        }
        self.phase = Phase::AwaitingSelection {
            request_id: expected,
        };
        true
    }

    pub(super) fn selection_received(&mut self, request_id: Option<Uuid>) -> bool {
        let Phase::AwaitingSelection {
            request_id: expected,
        } = self.phase
        else {
            return false;
        };
        if request_id != Some(expected) {
            return false;
        }
        self.phase = Phase::AwaitingSnapshot;
        true
    }

    pub(super) fn accept_snapshot(&mut self) -> bool {
        matches!(self.phase, Phase::AwaitingSnapshot)
    }

    pub(super) fn fail(&mut self, request_id: Option<Uuid>, message: String) -> bool {
        let expected = match self.phase {
            Phase::Opening { request_id } | Phase::AwaitingSelection { request_id } => request_id,
            _ => return false,
        };
        if request_id != Some(expected) {
            return false;
        }
        self.phase = Phase::Browsing;
        self.error = Some(message);
        true
    }

    pub(super) fn render(&mut self, host: Rect, buffer: &mut Buffer) {
        let area = render_frame(dialog_area(host, MAX_WIDTH, MAX_HEIGHT), buffer);
        if area.width == 0 || area.height == 0 {
            return;
        }
        match &mut self.phase {
            Phase::Approval {
                project,
                path,
                recipe,
                scroll,
                ..
            } => render_approval(area, project, path, recipe, scroll, buffer),
            Phase::Opening { .. } | Phase::AwaitingSelection { .. } | Phase::AwaitingSnapshot => {
                fill_row(
                    Rect::new(area.x, area.y, area.width, 1),
                    title_style(),
                    buffer,
                );
                buffer.set_stringn(
                    area.x,
                    area.y,
                    " Open project · opening…",
                    usize::from(area.width),
                    title_style(),
                );
            }
            Phase::Browsing => self.render_browser(area, buffer),
        }
    }

    fn render_browser(&mut self, area: Rect, buffer: &mut Buffer) {
        fill_row(
            Rect::new(area.x, area.y, area.width, 1),
            title_style(),
            buffer,
        );
        let prompt = if self.query.is_empty() {
            "› Project name or path…".to_owned()
        } else {
            format!("› {}", sanitize(&self.query))
        };
        buffer.set_stringn(
            area.x,
            area.y,
            truncate(&prompt, usize::from(area.width)),
            usize::from(area.width),
            title_style(),
        );
        if area.height == 1 {
            return;
        }
        let footer_rows = usize::from(area.height >= 3);
        let error_rows = usize::from(self.error.is_some() && area.height >= 4);
        let body_height = usize::from(area.height - 1).saturating_sub(footer_rows + error_rows);
        self.keep_visible(body_height);
        for offset in 0..body_height {
            let choice = self.scroll + offset;
            if choice >= self.choice_count() {
                break;
            }
            self.render_choice(
                choice,
                Rect::new(area.x, area.y + 1 + offset as u16, area.width, 1),
                buffer,
            );
        }
        render_list_scrollbar(
            self.scroll,
            self.choice_count(),
            Rect::new(area.x, area.y + 1, area.width, body_height as u16),
            buffer,
        );
        if let Some(error) = &self.error {
            let row = area.y + area.height - 1 - footer_rows as u16;
            buffer.set_stringn(
                area.x,
                row,
                format!(" {}", sanitize(error)),
                usize::from(area.width),
                muted_style().add_modifier(Modifier::BOLD),
            );
        }
        if footer_rows == 1 {
            buffer.set_stringn(
                area.x,
                area.y + area.height - 1,
                " type a path or fuzzy project name · ↑↓ choose · enter open · esc close",
                usize::from(area.width),
                muted_style(),
            );
        }
    }

    fn render_choice(&self, choice: usize, area: Rect, buffer: &mut Buffer) {
        let style = row_style(choice == self.selected);
        fill_row(area, style, buffer);
        let (title, detail) = if !self.query.is_empty() && choice == self.filtered.len() {
            (
                "Open path".to_owned(),
                display_path(&self.resolved_query_path()),
            )
        } else {
            let Some(entry) = self
                .filtered
                .get(choice)
                .and_then(|index| self.entries.get(*index))
            else {
                return;
            };
            (entry.name.clone(), display_path(&entry.path))
        };
        let detail_width = UnicodeWidthStr::width(detail.as_str()).min(usize::from(area.width) / 2);
        let title_width = usize::from(area.width).saturating_sub(detail_width + 3);
        buffer.set_stringn(
            area.x,
            area.y,
            format!(" {}", truncate(&title, title_width.saturating_sub(1))),
            title_width,
            style,
        );
        if detail_width > 0 {
            buffer.set_stringn(
                area.x + area.width - detail_width as u16 - 1,
                area.y,
                truncate(&detail, detail_width),
                detail_width,
                style.add_modifier(Modifier::DIM),
            );
        }
    }

    fn selected_choice(&self) -> Option<ProjectOpenChoice> {
        if !self.query.is_empty() && self.selected == self.filtered.len() {
            return Some(ProjectOpenChoice::Path(self.resolved_query_path()));
        }
        let entry = self
            .filtered
            .get(self.selected)
            .and_then(|index| self.entries.get(*index))?;
        Some(ProjectOpenChoice::Configured {
            name: entry.name.clone(),
            path: entry.path.clone(),
        })
    }

    fn resolved_query_path(&self) -> PathBuf {
        let path = if let Some(rest) = self.query.strip_prefix("~/") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| self.base.clone())
                .join(rest)
        } else {
            PathBuf::from(&self.query)
        };
        if path.is_absolute() {
            path
        } else {
            self.base.join(path)
        }
    }

    fn append(&mut self, character: char) {
        if self.query.len() + character.len_utf8() <= MAX_QUERY_BYTES {
            self.query.push(character);
            self.refilter();
        }
    }

    fn remove_last_grapheme(&mut self) {
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(index);
            self.refilter();
        }
    }

    fn refilter(&mut self) {
        self.filtered = fuzzy::ranked(
            &self.query,
            self.entries
                .iter()
                .map(|entry| format!("{} {}", entry.name, entry.path.display())),
        );
        self.selected = if self.query.contains('/') || self.filtered.is_empty() {
            self.filtered.len()
        } else {
            0
        };
        self.scroll = 0;
        self.error = None;
    }

    fn choice_count(&self) -> usize {
        self.filtered.len() + usize::from(!self.query.is_empty())
    }

    fn move_selection(&mut self, delta: isize, visible_rows: usize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.choice_count().saturating_sub(1));
        self.keep_visible(visible_rows);
    }

    fn keep_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
    }
}

fn render_approval(
    area: Rect,
    project: &str,
    path: &Path,
    recipe: &[String],
    scroll: &mut usize,
    buffer: &mut Buffer,
) {
    fill_row(
        Rect::new(area.x, area.y, area.width, 1),
        title_style(),
        buffer,
    );
    buffer.set_stringn(
        area.x,
        area.y,
        format!(" Trust project recipe · {}", sanitize(project)),
        usize::from(area.width),
        title_style(),
    );
    if area.height <= 1 {
        return;
    }
    let header = format!(" {}", display_path(path));
    buffer.set_stringn(
        area.x,
        area.y + 1,
        truncate(&header, usize::from(area.width)),
        usize::from(area.width),
        muted_style(),
    );
    let footer = usize::from(area.height >= 3);
    let body_height = usize::from(area.height - 2).saturating_sub(footer);
    *scroll = (*scroll).min(recipe.len().saturating_sub(body_height.max(1)));
    for (offset, line) in recipe.iter().skip(*scroll).take(body_height).enumerate() {
        buffer.set_stringn(
            area.x,
            area.y + 2 + offset as u16,
            format!(" {}", sanitize(line)),
            usize::from(area.width),
            ratatui::style::Style::default(),
        );
    }
    render_list_scrollbar(
        *scroll,
        recipe.len(),
        Rect::new(area.x, area.y + 2, area.width, body_height as u16),
        buffer,
    );
    if footer == 1 {
        buffer.set_stringn(
            area.x,
            area.y + area.height - 1,
            " review exact recipe · y trust and open · n/esc cancel · ↑↓ scroll",
            usize::from(area.width),
            muted_style(),
        );
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

pub(super) fn dialog_body_rows(host: Rect) -> usize {
    usize::from(
        dialog_area(host, MAX_WIDTH, MAX_HEIGHT)
            .height
            .saturating_sub(4),
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::*;
    use crate::client::config::{ProjectCatalog, ProjectConfig};

    fn catalog(root: &Path) -> ProjectCatalog {
        ProjectCatalog::from_projects(BTreeMap::from([
            (
                "fut".into(),
                ProjectConfig {
                    path: root.join("fut"),
                    recipe: None,
                },
            ),
            (
                "website".into(),
                ProjectConfig {
                    path: root.join("website"),
                    recipe: None,
                },
            ),
        ]))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn configured_projects_are_fuzzy_suggestions_and_input_can_remain_a_path() {
        let temporary = tempfile::tempdir().unwrap();
        let mut opener =
            ProjectOpenerState::open(&catalog(temporary.path()), temporary.path().into());
        opener.paste("wbst");
        assert_eq!(opener.filtered, [1]);
        assert_eq!(
            opener.key(key(KeyCode::Enter), 10),
            ProjectOpenerAction::Submit(ProjectOpenChoice::Configured {
                name: "website".into(),
                path: temporary.path().join("website"),
            })
        );
        opener.key(key(KeyCode::End), 10);
        assert_eq!(
            opener.key(key(KeyCode::Enter), 10),
            ProjectOpenerAction::Submit(ProjectOpenChoice::Path(temporary.path().join("wbst")))
        );

        opener.paste("/nested");
        assert_eq!(
            opener.key(key(KeyCode::Enter), 10),
            ProjectOpenerAction::Submit(ProjectOpenChoice::Path(
                temporary.path().join("wbst/nested")
            ))
        );
    }

    #[test]
    fn approval_shows_recipe_without_exposing_digest_and_is_explicit() {
        let temporary = tempfile::tempdir().unwrap();
        let recipe = temporary.path().join(".fut/project.toml");
        fs::create_dir_all(recipe.parent().unwrap()).unwrap();
        let mut opener =
            ProjectOpenerState::open(&catalog(temporary.path()), temporary.path().into());
        opener.confirm_approval(
            "fut".into(),
            temporary.path().join("fut"),
            recipe,
            "secret-digest".into(),
            "[[workspaces]]\ncommand = ['pi']\n",
        );
        let host = Rect::new(0, 0, 100, 30);
        let mut buffer = Buffer::empty(host);
        opener.render(host, &mut buffer);
        let rendered = (0..host.height)
            .flat_map(|row| (0..host.width).map(move |column| (column, row)))
            .map(|position| buffer[position].symbol())
            .collect::<String>();
        assert!(rendered.contains("command = ['pi']"));
        assert!(!rendered.contains("secret-digest"));
        assert_eq!(
            opener.key(key(KeyCode::Char('y')), 10),
            ProjectOpenerAction::Approve {
                project: "fut".into(),
                cwd: temporary.path().join("fut"),
                digest: "secret-digest".into(),
            }
        );
    }
}
