use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    actions::{COMMANDS, ClientAction, binding_label, definition},
    chrome::{sanitize, truncate},
};

const MAX_QUERY_BYTES: usize = 512;
const MAX_WIDTH: u16 = 72;
const MAX_HEIGHT: u16 = 8;

pub(super) struct CommandBarState {
    query: String,
    filtered: Vec<ClientAction>,
    selected: Option<usize>,
    scroll: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommandBarAction {
    Stay,
    Close,
    Dispatch(ClientAction),
}

impl CommandBarState {
    pub fn open() -> Self {
        let mut state = Self {
            query: String::new(),
            filtered: Vec::new(),
            selected: None,
            scroll: 0,
        };
        state.refilter(None);
        state
    }

    pub fn key(&mut self, key: KeyEvent) -> CommandBarAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return CommandBarAction::Stay;
        }
        match key.code {
            KeyCode::Esc => CommandBarAction::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                CommandBarAction::Close
            }
            KeyCode::Enter => self
                .selected_action()
                .map(CommandBarAction::Dispatch)
                .unwrap_or(CommandBarAction::Stay),
            KeyCode::Up | KeyCode::BackTab => {
                self.move_selection(-1);
                CommandBarAction::Stay
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_selection(1);
                CommandBarAction::Stay
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-1);
                CommandBarAction::Stay
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(1);
                CommandBarAction::Stay
            }
            KeyCode::Home => {
                if !self.filtered.is_empty() {
                    self.selected = Some(0);
                }
                CommandBarAction::Stay
            }
            KeyCode::End => {
                self.selected = self.filtered.len().checked_sub(1);
                CommandBarAction::Stay
            }
            KeyCode::PageUp => {
                self.page_selection(-5);
                CommandBarAction::Stay
            }
            KeyCode::PageDown => {
                self.page_selection(5);
                CommandBarAction::Stay
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.remove_last_grapheme();
                CommandBarAction::Stay
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.refilter(None);
                CommandBarAction::Stay
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
                CommandBarAction::Stay
            }
            _ => CommandBarAction::Stay,
        }
    }

    pub fn paste(&mut self, value: &str) {
        let mut pending_space = false;
        for character in value.chars() {
            if character.is_control() || character.is_whitespace() {
                pending_space = !self.query.is_empty();
                continue;
            }
            if pending_space && !self.query.ends_with(' ') {
                self.append_raw(' ');
            }
            pending_space = false;
            self.append_raw(character);
            if self.query.len() >= MAX_QUERY_BYTES {
                break;
            }
        }
        self.refilter(None);
    }

    pub fn render(&mut self, host: Rect, buffer: &mut Buffer) {
        let area = command_bar_area(host);
        if area.width == 0 || area.height == 0 {
            return;
        }
        clear(area, buffer);
        if area.height == 1 {
            self.render_tiny(area, buffer);
            return;
        }

        render_prompt(area, &self.query, self.selected_position(), buffer);
        let footer = (area.height >= 4).then_some(area.y + area.height - 1);
        let body_height = usize::from(area.height - 1 - u16::from(footer.is_some()));
        self.keep_selected_visible(body_height);
        if self.filtered.is_empty() {
            buffer.set_stringn(
                area.x,
                area.y + 1,
                " No matching commands",
                usize::from(area.width),
                muted_style(),
            );
        } else {
            for (offset, action) in self
                .filtered
                .iter()
                .skip(self.scroll)
                .take(body_height)
                .copied()
                .enumerate()
            {
                let index = self.scroll + offset;
                render_result(
                    action,
                    self.selected == Some(index),
                    Rect::new(
                        area.x,
                        area.y + 1 + u16::try_from(offset).expect("visible offset fits u16"),
                        area.width,
                        1,
                    ),
                    buffer,
                );
            }
        }
        if let Some(row) = footer {
            buffer.set_stringn(
                area.x,
                row,
                " type to filter · ↑↓ choose · enter run · esc close",
                usize::from(area.width),
                muted_style(),
            );
        }
    }

    fn append(&mut self, character: char) {
        let selected = self.selected_action();
        self.append_raw(character);
        self.refilter(selected);
    }

    fn append_raw(&mut self, character: char) {
        if self.query.len() + character.len_utf8() <= MAX_QUERY_BYTES {
            self.query.push(character);
        }
    }

    fn remove_last_grapheme(&mut self) {
        let selected = self.selected_action();
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(index);
            self.refilter(selected);
        }
    }

    fn refilter(&mut self, preserve: Option<ClientAction>) {
        let tokens = self
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        self.filtered = COMMANDS
            .iter()
            .filter(|command| {
                let haystack = format!(
                    "{} {} {}",
                    command.title,
                    command.keywords,
                    binding_label(command.action)
                )
                .to_lowercase();
                tokens.iter().all(|token| haystack.contains(token))
            })
            .map(|command| command.action)
            .collect();
        self.selected = preserve
            .and_then(|action| {
                self.filtered
                    .iter()
                    .position(|candidate| *candidate == action)
            })
            .or_else(|| (!self.filtered.is_empty()).then_some(0));
        self.scroll = 0;
    }

    fn selected_action(&self) -> Option<ClientAction> {
        self.selected
            .and_then(|index| self.filtered.get(index))
            .copied()
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

    fn page_selection(&mut self, delta: isize) {
        self.move_selection(delta);
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

    fn render_tiny(&self, area: Rect, buffer: &mut Buffer) {
        let detail = if let Some(action) = self.selected_action() {
            definition(action)
                .map(|command| command.title)
                .unwrap_or("")
        } else {
            "No matching commands"
        };
        let text = if self.query.is_empty() {
            format!("› {detail}")
        } else {
            format!("› {} · {detail}", self.query)
        };
        fill_row(area, prompt_style(), buffer);
        buffer.set_stringn(
            area.x,
            area.y,
            truncate(&text, usize::from(area.width)),
            usize::from(area.width),
            prompt_style(),
        );
    }
}

pub(super) fn command_bar_area(host: Rect) -> Rect {
    let width = host.width.min(MAX_WIDTH);
    let height = host.height.min(MAX_HEIGHT);
    Rect::new(
        host.x.saturating_add((host.width - width) / 2),
        host.y.saturating_add((host.height - height) / 3),
        width,
        height,
    )
}

fn render_prompt(area: Rect, query: &str, position: Option<(usize, usize)>, buffer: &mut Buffer) {
    let row = Rect::new(area.x, area.y, area.width, 1);
    fill_row(row, prompt_style(), buffer);
    let text = if query.is_empty() {
        "› Search commands…".to_owned()
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
        prompt_style(),
    );
    if let Some((count, count_width)) = count {
        buffer.set_stringn(
            area.x + area.width - u16::try_from(count_width).expect("count width fits u16"),
            area.y,
            count,
            count_width,
            prompt_style().add_modifier(Modifier::DIM),
        );
    }
}

fn render_result(action: ClientAction, selected: bool, area: Rect, buffer: &mut Buffer) {
    let style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    fill_row(area, style, buffer);
    let definition = definition(action).expect("command bar actions have definitions");
    let full_binding = binding_label(action);
    let binding = if area.width >= 56 {
        full_binding.as_str()
    } else if area.width >= 32 {
        full_binding.split(" · ").next().unwrap_or("")
    } else {
        ""
    };
    let binding_width = UnicodeWidthStr::width(binding);
    let title_width = usize::from(area.width)
        .saturating_sub(binding_width)
        .saturating_sub(2);
    let title = format!(
        " {}",
        truncate(definition.title, title_width.saturating_sub(1))
    );
    buffer.set_stringn(area.x, area.y, title, title_width, style);
    if !binding.is_empty() {
        buffer.set_stringn(
            area.x + area.width - u16::try_from(binding_width + 1).expect("binding width fits u16"),
            area.y,
            binding,
            binding_width,
            style.add_modifier(Modifier::DIM),
        );
    }
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

fn fill_row(area: Rect, style: Style, buffer: &mut Buffer) {
    for column in area.x..area.x.saturating_add(area.width) {
        if let Some(cell) = buffer.cell_mut((column, area.y)) {
            cell.set_symbol(" ").set_style(style);
        }
    }
}

fn prompt_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

fn muted_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
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
    fn filtering_is_stable_case_insensitive_multi_token_and_searches_bindings() {
        let mut bar = CommandBarState::open();
        assert_eq!(bar.filtered.len(), COMMANDS.len());
        assert_eq!(bar.selected_action(), Some(ClientAction::OpenNavigator));

        bar.paste("FOCUS   pane");
        assert_eq!(
            bar.filtered,
            [ClientAction::FocusNextPane, ClientAction::FocusPreviousPane]
        );
        bar.key(key(KeyCode::Char(' '), KeyModifiers::NONE));
        bar.paste("previous");
        assert_eq!(bar.filtered, [ClientAction::FocusPreviousPane]);

        bar = CommandBarState::open();
        bar.paste("ctrl-b ;");
        assert_eq!(bar.filtered, [ClientAction::FocusPreviousPane]);
    }

    #[test]
    fn keyboard_navigation_dispatch_and_empty_results_are_typed() {
        let mut bar = CommandBarState::open();
        bar.key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            bar.key(key(KeyCode::Enter, KeyModifiers::NONE)),
            CommandBarAction::Dispatch(ClientAction::OpenWorkspaceSidebar)
        );
        bar.key(key(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(bar.selected_action(), Some(ClientAction::Detach));
        bar.key(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(bar.selected_action(), Some(ClientAction::OpenNavigator));

        bar.paste("frobnicate");
        assert!(bar.filtered.is_empty());
        assert_eq!(
            bar.key(key(KeyCode::Enter, KeyModifiers::NONE)),
            CommandBarAction::Stay
        );
        assert_eq!(
            bar.key(key(KeyCode::Esc, KeyModifiers::NONE)),
            CommandBarAction::Close
        );
    }

    #[test]
    fn letters_are_query_input_and_backspace_removes_a_grapheme() {
        let mut bar = CommandBarState::open();
        for character in ['j', 'k', 'q', 'e', '\u{301}'] {
            bar.key(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(bar.query, "jkqe\u{301}");
        bar.key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(bar.query, "jkq");
        bar.key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(bar.query.is_empty());
    }

    #[test]
    fn paste_collapses_controls_and_is_bounded() {
        let mut bar = CommandBarState::open();
        bar.paste("  focus\n\t next\x1b pane  ");
        assert_eq!(bar.query, "focus next pane");
        bar.paste(&"x".repeat(MAX_QUERY_BYTES * 2));
        assert!(bar.query.len() <= MAX_QUERY_BYTES);
    }

    #[test]
    fn wide_render_shows_selection_bindings_prompt_and_help() {
        let host = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(host);
        let mut bar = CommandBarState::open();
        bar.render(host, &mut buffer);
        let rendered = text(&buffer);
        assert!(rendered.contains("Search commands…"));
        assert!(rendered.contains("Open global navigator"));
        assert!(rendered.contains("Ctrl-b g"));
        assert!(rendered.contains("type to filter"));
        let area = command_bar_area(host);
        assert!(
            buffer[(area.x, area.y)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            buffer[(area.x, area.y + 1)]
                .modifier
                .contains(Modifier::REVERSED)
        );

        let narrow = Rect::new(0, 0, 20, 4);
        let mut narrow_buffer = Buffer::empty(narrow);
        bar.render(narrow, &mut narrow_buffer);
        let prompt = (0..narrow.width)
            .map(|column| narrow_buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(prompt.contains("1/11"));
        assert!(!prompt.contains("command1/11"));
    }

    #[test]
    fn empty_and_tiny_rendering_never_panics_and_keeps_useful_context() {
        for width in 0..75 {
            for height in 0..10 {
                let host = Rect::new(0, 0, width, height);
                let mut buffer = Buffer::empty(host);
                let mut bar = CommandBarState::open();
                bar.render(host, &mut buffer);
                if width > 2 && height == 1 {
                    assert!(text(&buffer).contains('›'));
                }
            }
        }

        let host = Rect::new(0, 0, 30, 3);
        let mut buffer = Buffer::empty(host);
        let mut bar = CommandBarState::open();
        bar.paste("nothing matches this");
        bar.render(host, &mut buffer);
        assert!(text(&buffer).contains("No matching commands"));
    }

    #[test]
    fn geometry_is_centered_bounded_and_zero_safe() {
        assert_eq!(
            command_bar_area(Rect::new(4, 5, 100, 20)),
            Rect::new(18, 9, 72, 8)
        );
        assert_eq!(
            command_bar_area(Rect::new(4, 5, 20, 3)),
            Rect::new(4, 5, 20, 3)
        );
        assert_eq!(
            command_bar_area(Rect::new(4, 5, 0, 0)),
            Rect::new(4, 5, 0, 0)
        );
    }
}
