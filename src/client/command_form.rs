use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    chrome::truncate,
    config::PaletteCommand,
    dialog::{dialog_area, render_footer, render_frame, render_title},
    temporary_command::ExtensionCommandContext,
};
use crate::extensions::contains_unsafe_text;

const MAX_WIDTH: u16 = 72;
const MAX_VALUE_BYTES: usize = 4 * 1024;

pub(super) struct CommandFormState {
    command: PaletteCommand,
    context: ExtensionCommandContext,
    fields: Vec<FieldState>,
    selected: usize,
}

struct FieldState {
    name: String,
    label: String,
    prefix: String,
    placeholder: String,
    value: String,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommandFormAction {
    Stay,
    Cancel,
    Submit,
}

pub(super) struct CommandFormSubmission {
    pub command: PaletteCommand,
    pub context: ExtensionCommandContext,
    pub values: BTreeMap<String, String>,
}

impl CommandFormState {
    pub(super) fn open(
        command: PaletteCommand,
        context: ExtensionCommandContext,
    ) -> anyhow::Result<Self> {
        let fields = command
            .fields
            .iter()
            .map(|field| {
                let value = match &field.default_config {
                    Some(key) => context.configured_field_default(key)?.unwrap_or_default(),
                    None => field.default.clone().unwrap_or_default(),
                };
                if value.len() > MAX_VALUE_BYTES {
                    anyhow::bail!(
                        "field {:?} default exceeds {MAX_VALUE_BYTES} bytes",
                        field.name
                    );
                }
                if contains_unsafe_text(&value) {
                    anyhow::bail!(
                        "field {:?} default contains unsafe terminal text",
                        field.name
                    );
                }
                let cursor = value.len();
                Ok(FieldState {
                    name: field.name.clone(),
                    label: field.label.clone(),
                    prefix: field.prefix.clone(),
                    placeholder: field.placeholder.clone(),
                    value,
                    cursor,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            command,
            context,
            fields,
            selected: 0,
        })
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> CommandFormAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return CommandFormAction::Stay;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                CommandFormAction::Cancel
            }
            (KeyCode::Tab, _) | (KeyCode::Down, _) => {
                self.select_next();
                CommandFormAction::Stay
            }
            (KeyCode::BackTab, _) | (KeyCode::Up, _) => {
                self.select_previous();
                CommandFormAction::Stay
            }
            (KeyCode::Enter, _) if self.selected + 1 < self.fields.len() => {
                self.select_next();
                CommandFormAction::Stay
            }
            (KeyCode::Enter, _) => CommandFormAction::Submit,
            (KeyCode::Left, _) => {
                self.field_mut().move_left();
                CommandFormAction::Stay
            }
            (KeyCode::Right, _) => {
                self.field_mut().move_right();
                CommandFormAction::Stay
            }
            (KeyCode::Home, _) => {
                self.field_mut().cursor = 0;
                CommandFormAction::Stay
            }
            (KeyCode::End, _) => {
                let field = self.field_mut();
                field.cursor = field.value.len();
                CommandFormAction::Stay
            }
            (KeyCode::Backspace, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
                self.field_mut().remove_previous_word();
                CommandFormAction::Stay
            }
            (KeyCode::Char('w'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.field_mut().remove_previous_word();
                CommandFormAction::Stay
            }
            (KeyCode::Char('u'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.field_mut().clear();
                CommandFormAction::Stay
            }
            (KeyCode::Backspace, _) => {
                self.field_mut().backspace();
                CommandFormAction::Stay
            }
            (KeyCode::Delete, _) => {
                self.field_mut().delete();
                CommandFormAction::Stay
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
                self.field_mut().insert(character);
                CommandFormAction::Stay
            }
            _ => CommandFormAction::Stay,
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            self.field_mut().insert(character);
        }
    }

    pub(super) fn submit(self) -> CommandFormSubmission {
        CommandFormSubmission {
            command: self.command,
            context: self.context,
            values: self
                .fields
                .into_iter()
                .map(|field| (field.name, field.value))
                .collect(),
        }
    }

    pub(super) fn render(&self, host: Rect, buffer: &mut Buffer) {
        let height = u16::try_from(self.fields.len())
            .unwrap_or(u16::MAX)
            .saturating_add(4);
        let area = render_frame(dialog_area(host, MAX_WIDTH, height), buffer);
        if area.width == 0 || area.height == 0 {
            return;
        }
        render_title(area, &format!(" {}", self.command.title), buffer);
        let label_width = self
            .fields
            .iter()
            .map(|field| UnicodeWidthStr::width(field.label.as_str()))
            .max()
            .unwrap_or(0)
            .min(usize::from(area.width.saturating_sub(4) / 2));
        let visible_rows = usize::from(area.height.saturating_sub(2));
        let first = (self.selected + 1).saturating_sub(visible_rows);
        for (index, field) in self
            .fields
            .iter()
            .enumerate()
            .skip(first)
            .take(visible_rows)
        {
            let y = area.y + 1 + (index - first) as u16;
            let field_label = truncate(&field.label, label_width);
            let padding = label_width.saturating_sub(UnicodeWidthStr::width(field_label.as_str()));
            let label = truncate(
                &format!(" {}{}: ", " ".repeat(padding), field_label),
                usize::from(area.width),
            );
            let label_cells = UnicodeWidthStr::width(label.as_str());
            buffer.set_stringn(
                area.x,
                y,
                &label,
                usize::from(area.width),
                Style::default().add_modifier(if index == self.selected {
                    Modifier::BOLD
                } else {
                    Modifier::default()
                }),
            );
            let available = usize::from(area.width).saturating_sub(label_cells);
            let x = area
                .x
                .saturating_add(u16::try_from(label_cells).unwrap_or(u16::MAX));
            if field.value.is_empty() && index != self.selected && !field.placeholder.is_empty() {
                let text = format!("{}{}", field.prefix, field.placeholder);
                buffer.set_stringn(
                    x,
                    y,
                    text,
                    available,
                    Style::default().add_modifier(Modifier::DIM),
                );
                continue;
            }
            let before = format!("{}{}", field.prefix, &field.value[..field.cursor]);
            let after = &field.value[field.cursor..];
            let visible_before = trailing_view(&before, available.saturating_sub(1));
            buffer.set_stringn(x, y, &visible_before, available, Style::default());
            let used = UnicodeWidthStr::width(visible_before.as_str());
            if index == self.selected && used < available {
                let cursor_x = x + u16::try_from(used).unwrap_or(u16::MAX);
                let next = after.graphemes(true).next();
                let symbol = next.unwrap_or(" ");
                buffer.set_stringn(
                    cursor_x,
                    y,
                    symbol,
                    available - used,
                    Style::default().add_modifier(Modifier::REVERSED),
                );
                let symbol_width = UnicodeWidthStr::width(symbol);
                if next.is_some() && symbol_width < available - used {
                    let rest = &after[symbol.len()..];
                    buffer.set_stringn(
                        cursor_x + symbol_width as u16,
                        y,
                        rest,
                        available - used - symbol_width,
                        Style::default(),
                    );
                }
            } else if used < available {
                buffer.set_stringn(
                    x + used as u16,
                    y,
                    after,
                    available - used,
                    Style::default(),
                );
            }
        }
        render_footer(
            area,
            " tab/↑↓ fields  enter next/create  esc cancel",
            buffer,
        );
    }

    fn field_mut(&mut self) -> &mut FieldState {
        &mut self.fields[self.selected]
    }

    fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.fields.len().saturating_sub(1));
    }

    fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

impl FieldState {
    fn insert(&mut self, character: char) {
        if self.value.len() + character.len_utf8() <= MAX_VALUE_BYTES {
            self.value.insert(self.cursor, character);
            self.cursor += character.len_utf8();
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.value[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    fn move_right(&mut self) {
        self.cursor += self.value[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
    }

    fn backspace(&mut self) {
        let end = self.cursor;
        self.move_left();
        self.value.replace_range(self.cursor..end, "");
    }

    fn delete(&mut self) {
        let length = self.value[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
        self.value
            .replace_range(self.cursor..self.cursor + length, "");
    }

    fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    fn remove_previous_word(&mut self) {
        let before = &self.value[..self.cursor];
        let trimmed = before.trim_end();
        let start = trimmed
            .char_indices()
            .rev()
            .take_while(|(_, character)| !character.is_whitespace())
            .last()
            .map_or(trimmed.len(), |(index, _)| index);
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
}

fn trailing_view(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut suffix = Vec::new();
    let mut used = 1;
    for grapheme in value.graphemes(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > width {
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
    use super::*;
    use crate::{
        command::PopupSize,
        extensions::{ExtensionCommandExecution, ExtensionCommandField},
    };

    fn form() -> CommandFormState {
        CommandFormState::open(
            PaletteCommand {
                title: "Create something".into(),
                binding: None,
                program: "/bin/true".into(),
                args: Vec::new(),
                execution: ExtensionCommandExecution::Interactive {
                    size: PopupSize::default(),
                    activate_opened: false,
                },
                extension: None,
                fields: vec![
                    ExtensionCommandField {
                        name: "worktree".into(),
                        label: "Very long Worktree".into(),
                        prefix: String::new(),
                        placeholder: "random".into(),
                        default: None,
                        default_config: None,
                    },
                    ExtensionCommandField {
                        name: "command".into(),
                        label: "Command".into(),
                        prefix: "$ ".into(),
                        placeholder: String::new(),
                        default: Some("pi".into()),
                        default_config: None,
                    },
                    ExtensionCommandField {
                        name: "prompt".into(),
                        label: "Prompt".into(),
                        prefix: String::new(),
                        placeholder: String::new(),
                        default: None,
                        default_config: None,
                    },
                ],
            },
            ExtensionCommandContext::test(),
        )
        .unwrap()
    }

    #[test]
    fn field_editing_handles_cursor_words_and_unicode_graphemes() {
        let mut field = FieldState {
            name: "value".into(),
            label: "Value".into(),
            prefix: String::new(),
            placeholder: String::new(),
            value: "one 👩🏽‍💻 three".into(),
            cursor: "one 👩🏽‍💻".len(),
        };
        field.backspace();
        assert_eq!(field.value, "one  three");
        field.remove_previous_word();
        assert_eq!(field.value, " three");
        field.delete();
        assert_eq!(field.value, "three");
    }

    #[test]
    fn trailing_view_keeps_the_edited_end_visible() {
        assert_eq!(trailing_view("abcdefghijkl", 6), "…hijkl");
    }

    #[test]
    fn navigation_collects_all_fields_and_submits_from_the_last() {
        let mut form = form();
        form.paste("feature");
        assert_eq!(
            form.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            CommandFormAction::Stay
        );
        assert_eq!(form.selected, 1);
        assert_eq!(
            form.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            CommandFormAction::Stay
        );
        form.paste("start here");
        assert_eq!(
            form.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            CommandFormAction::Submit
        );
        assert_eq!(
            form.submit().values,
            BTreeMap::from([
                ("command".into(), "pi".into()),
                ("prompt".into(), "start here".into()),
                ("worktree".into(), "feature".into()),
            ])
        );
    }

    #[test]
    fn rendering_empty_fields_is_safe_at_tiny_sizes() {
        let form = form();
        for width in 1..10 {
            for height in 1..8 {
                let area = Rect::new(0, 0, width, height);
                form.render(area, &mut Buffer::empty(area));
            }
        }
    }
}
