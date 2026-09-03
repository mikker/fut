use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    config::{SegmentConfig, SemanticStyle, TokenVisual, UiConfig},
    dialog::{
        dialog_area, fill_row, render_footer, render_frame, render_list_scrollbar, render_title,
        row_style,
    },
    fuzzy,
    presentation::{ItemState, TokenEffect, TokenValue, render_token_segments},
    spinners::BUILTIN_SPINNERS,
};

const MAX_WIDTH: u16 = 100;
const MAX_HEIGHT: u16 = 28;
const LIST_WIDTH: u16 = 34;
const MAX_QUERY_BYTES: usize = 256;

pub(super) struct UiCatalog {
    filtered: Vec<usize>,
    query: String,
    selected: usize,
    scroll: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UiCatalogAction {
    Stay,
    Close,
}

impl UiCatalog {
    pub fn open(ui: &UiConfig) -> Self {
        let selected = BUILTIN_SPINNERS
            .iter()
            .position(|spinner| ui.spinner.frames.is_none() && spinner.name == ui.spinner.style)
            .unwrap_or(0);
        Self {
            filtered: (0..BUILTIN_SPINNERS.len()).collect(),
            query: String::new(),
            selected,
            scroll: 0,
        }
    }

    pub fn key(&mut self, key: KeyEvent, visible_rows: usize) -> UiCatalogAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return UiCatalogAction::Stay;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => return UiCatalogAction::Close,
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                self.move_selection(-1)
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                self.move_selection(1)
            }
            (KeyCode::Home, _) => self.select_filtered(0),
            (KeyCode::End, _) => self.select_filtered(self.filtered.len().saturating_sub(1)),
            (KeyCode::PageUp, _) => self.move_selection(-(visible_rows.max(1) as isize)),
            (KeyCode::PageDown, _) => self.move_selection(visible_rows.max(1) as isize),
            (KeyCode::Backspace | KeyCode::Delete, _) => self.remove_last_grapheme(),
            (KeyCode::Char(character), modifiers)
                if !character.is_control()
                    && !modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    )
                    && self.query.len() + character.len_utf8() <= MAX_QUERY_BYTES =>
            {
                self.query.push(character);
                self.refilter();
            }
            _ => {}
        }
        self.keep_visible(visible_rows);
        UiCatalogAction::Stay
    }

    pub fn paste(&mut self, value: &str) {
        for character in value.chars().filter(|character| !character.is_control()) {
            if self.query.len() + character.len_utf8() > MAX_QUERY_BYTES {
                break;
            }
            self.query.push(character);
        }
        self.refilter();
    }

    pub fn render(&mut self, host: Rect, elapsed_ms: usize, ui: &UiConfig, buffer: &mut Buffer) {
        let area = render_frame(dialog_area(host, MAX_WIDTH, MAX_HEIGHT), buffer);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let title = if self.query.is_empty() {
            " UI playground".to_owned()
        } else {
            format!(" UI playground › {}", self.query)
        };
        render_title(area, &title, buffer);
        if area.height > 1 {
            render_footer(area, " type search  ↑↓/C-jk browse  esc close", buffer);
        }
        let body = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(2),
        );
        if body.width < 48 {
            self.render_list(body, elapsed_ms, buffer);
            return;
        }
        let list_width = LIST_WIDTH.min(body.width / 2);
        let list = Rect::new(body.x, body.y, list_width, body.height);
        let preview = Rect::new(
            body.x + list_width + 1,
            body.y,
            body.width.saturating_sub(list_width + 1),
            body.height,
        );
        self.render_list(list, elapsed_ms, buffer);
        for y in body.y..body.y.saturating_add(body.height) {
            if let Some(cell) = buffer.cell_mut((body.x + list_width, y)) {
                cell.set_symbol("│")
                    .set_style(Style::default().add_modifier(Modifier::DIM));
            }
        }
        self.render_preview(preview, elapsed_ms, ui, buffer);
    }

    fn render_list(&mut self, area: Rect, elapsed_ms: usize, buffer: &mut Buffer) {
        self.keep_visible(usize::from(area.height));
        if self.filtered.is_empty() {
            buffer.set_stringn(
                area.x,
                area.y,
                " No matching spinners",
                area.width.into(),
                Style::default(),
            );
            return;
        }
        for (row, index) in self
            .filtered
            .iter()
            .skip(self.scroll)
            .take(usize::from(area.height))
            .enumerate()
        {
            let spinner = BUILTIN_SPINNERS[*index];
            let style = row_style(*index == self.selected);
            let y = area.y + row as u16;
            fill_row(Rect::new(area.x, y, area.width, 1), style, buffer);
            let frame = spinner.frame(elapsed_ms);
            buffer.set_stringn(area.x + 1, y, frame, 8, style);
            let label = format!("{:<14} {:>4}ms", spinner.name, spinner.interval_ms);
            buffer.set_stringn(
                area.x + 10,
                y,
                label,
                area.width.saturating_sub(11).into(),
                style,
            );
        }
        render_list_scrollbar(self.scroll, self.filtered.len(), area, buffer);
    }

    fn render_preview(&self, area: Rect, elapsed_ms: usize, ui: &UiConfig, buffer: &mut Buffer) {
        let Some(spinner) = BUILTIN_SPINNERS.get(self.selected).copied() else {
            return;
        };
        let normal = ui.styles.apply(SemanticStyle::Normal, Style::default());
        let activity = ui.styles.apply(SemanticStyle::Activity, normal);
        let muted = ui.styles.apply(SemanticStyle::Muted, normal);
        let icons = ui.icons.resolve();
        let rows = [
            (0, " Spinner".to_owned(), normal),
            (
                1,
                format!(" {}  working", spinner.frame(elapsed_ms)),
                activity,
            ),
            (
                3,
                " Effects".to_owned(),
                normal.add_modifier(Modifier::BOLD),
            ),
            (
                14,
                " Config".to_owned(),
                normal.add_modifier(Modifier::BOLD),
            ),
            (15, " [ui.spinner]".to_owned(), muted),
            (16, format!(" style = {:?}", spinner.name), muted),
            (17, format!(" # interval = {}", spinner.interval_ms), muted),
            (
                19,
                " Custom".to_owned(),
                normal.add_modifier(Modifier::BOLD),
            ),
            (
                20,
                " frames = [\"-\", \"\\\\\", \"|\", \"/\"]".to_owned(),
                muted,
            ),
            (21, " interval = 100".to_owned(), muted),
        ];
        for (offset, text, style) in rows {
            if offset >= area.height {
                continue;
            }
            buffer.set_stringn(
                area.x + 1,
                area.y + offset,
                text,
                area.width.saturating_sub(2).into(),
                style,
            );
        }

        let effects = [
            ("plain", TokenVisual::Plain, TokenEffect::Plain),
            ("pulse", TokenVisual::Plain, TokenEffect::Pulse),
            ("wave", TokenVisual::Plain, TokenEffect::Wave),
            ("inverted", TokenVisual::Inverted, TokenEffect::Plain),
            ("inv pulse", TokenVisual::Inverted, TokenEffect::Pulse),
            ("inv wave", TokenVisual::Inverted, TokenEffect::Wave),
            ("pill", TokenVisual::Pill, TokenEffect::Plain),
            ("pill pulse", TokenVisual::Pill, TokenEffect::Pulse),
            ("pill wave", TokenVisual::Pill, TokenEffect::Wave),
        ];
        for (index, (label, visual, effect)) in effects.into_iter().enumerate() {
            let y = 4 + index as u16;
            if y >= area.height {
                break;
            }
            let line = render_token_segments(
                &[
                    SegmentConfig::Text {
                        text: format!(" {label:<10}"),
                        style: None,
                    },
                    SegmentConfig::Token {
                        token: "preview".into(),
                        style: None,
                        prefix: " ".into(),
                        suffix: " ".into(),
                        max_width: None,
                        visual,
                    },
                ],
                None,
                ItemState::default(),
                &ui.styles,
                &icons,
                |_| {
                    TokenValue::styled("launching", SemanticStyle::Activity)
                        .with_effect(effect, elapsed_ms)
                },
            );
            buffer.set_line(
                area.x + 1,
                area.y + y,
                &line.line,
                area.width.saturating_sub(2),
            );
        }
    }

    fn refilter(&mut self) {
        self.filtered = fuzzy::ranked(
            &self.query,
            BUILTIN_SPINNERS
                .iter()
                .map(|spinner| spinner.name.to_owned()),
        );
        self.scroll = 0;
        if !self.filtered.contains(&self.selected) {
            self.selected = self.filtered.first().copied().unwrap_or(0);
        }
    }

    fn remove_last_grapheme(&mut self) {
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(index);
            self.refilter();
        }
    }

    fn selected_filtered_position(&self) -> usize {
        self.filtered
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0)
    }

    fn select_filtered(&mut self, position: usize) {
        if let Some(index) = self.filtered.get(position) {
            self.selected = *index;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let position = self
            .selected_filtered_position()
            .saturating_add_signed(delta)
            .min(self.filtered.len().saturating_sub(1));
        self.select_filtered(position);
    }

    fn keep_visible(&mut self, visible_rows: usize) {
        let position = self.selected_filtered_position();
        if position < self.scroll {
            self.scroll = position;
        } else if position >= self.scroll.saturating_add(visible_rows.max(1)) {
            self.scroll = position + 1 - visible_rows.max(1);
        }
    }
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
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn catalog_opens_on_the_configured_spinner_and_filters() {
        let mut ui = UiConfig::default();
        ui.spinner.style = "circle_halves".into();
        let mut catalog = UiCatalog::open(&ui);
        assert_eq!(BUILTIN_SPINNERS[catalog.selected].name, "circle_halves");

        catalog.paste("shark");
        assert_eq!(catalog.filtered.len(), 1);
        assert_eq!(BUILTIN_SPINNERS[catalog.selected].name, "shark");
        assert_eq!(catalog.key(key(KeyCode::Esc), 10), UiCatalogAction::Close);
    }

    #[test]
    fn catalog_renders_live_preview_at_normal_and_tiny_sizes() {
        let ui = UiConfig::default();
        let mut catalog = UiCatalog::open(&ui);
        for area in [Rect::new(0, 0, 100, 28), Rect::new(0, 0, 20, 3)] {
            let mut buffer = Buffer::empty(area);
            catalog.render(area, 1_300, &ui, &mut buffer);
        }
    }

    #[test]
    fn inverted_preview_keeps_its_label_outside_symmetric_padding() {
        let ui = UiConfig::default();
        let catalog = UiCatalog::open(&ui);
        let area = Rect::new(0, 0, 50, 24);
        let mut buffer = Buffer::empty(area);
        catalog.render_preview(area, 0, &ui, &mut buffer);

        let reversed = |x| buffer[(x, 7)].modifier.contains(Modifier::REVERSED);
        assert!(!reversed(1));
        assert!(reversed(12));
        assert!(reversed(22));
        assert!(!reversed(23));
        assert_eq!(buffer[(13, 7)].symbol(), "l");
        assert_eq!(buffer[(21, 7)].symbol(), "g");
    }
}
