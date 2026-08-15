//! Which-key style cheatsheet shown after hesitating on the prefix: every
//! bound suffix and what it does, laid out in columns. Purely display — keys
//! keep flowing through the prefix state machine unchanged.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use super::{
    actions::{ALL_ACTIONS, ClientAction, command_name},
    chrome::truncate,
    config::BindingsConfig,
    dialog::{dialog_area, render_frame, render_title},
};

const MAX_WIDTH: u16 = 80;

pub(super) fn render(bindings: &BindingsConfig, host: Rect, buffer: &mut Buffer) {
    let entries = entries(bindings);
    let rows = entries.len().div_ceil(2);
    let height = u16::try_from(rows + 3).unwrap_or(u16::MAX);
    let area = render_frame(dialog_area(host, MAX_WIDTH, height), buffer);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let header = usize::from(area.height >= 2);
    if header == 1 {
        render_title(area, " Ctrl-b …", buffer);
    }
    let body_rows = usize::from(area.height).saturating_sub(header).max(1);
    let columns = entries.len().div_ceil(body_rows).max(1);
    let column_width = usize::from(area.width) / columns;
    let key_width = entries
        .iter()
        .map(|(key, _)| UnicodeWidthStr::width(key.as_str()))
        .max()
        .unwrap_or(1);
    for (index, (key, title)) in entries.iter().enumerate() {
        let column = index / body_rows;
        let row = index % body_rows;
        let x = area.x + u16::try_from(column * column_width).expect("column offset fits u16");
        let y = area.y + header as u16 + u16::try_from(row).expect("row fits u16");
        let key = format!(" {key:>key_width$}");
        let key_span = UnicodeWidthStr::width(key.as_str());
        buffer.set_stringn(
            x,
            y,
            &key,
            column_width,
            Style::default().add_modifier(Modifier::BOLD),
        );
        let title_width = column_width.saturating_sub(key_span + 2);
        buffer.set_stringn(
            x + u16::try_from(key_span + 2).expect("key span fits u16"),
            y,
            truncate(title, title_width),
            title_width,
            Style::default(),
        );
    }
}

fn entries(bindings: &BindingsConfig) -> Vec<(String, String)> {
    let mut entries = ALL_ACTIONS
        .into_iter()
        .filter(|action| bindings.label(*action) != "Unbound")
        .map(|action| (bindings.suffix_label(action), title(action).to_owned()))
        .collect::<Vec<_>>();
    entries.extend(bindings.commands().filter_map(|(_, command)| {
        command.binding.as_ref().map(|binding| {
            (
                super::actions::parse_suffix(binding)
                    .expect("validated command binding")
                    .1,
                command.title.clone(),
            )
        })
    }));
    entries
}

fn title(action: ClientAction) -> &'static str {
    command_name(action)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn lists_every_binding_including_the_command_palette() {
        let host = Rect::new(0, 0, 100, 40);
        let mut buffer = Buffer::empty(host);
        render(&BindingsConfig::default(), host, &mut buffer);
        let rendered = text(&buffer);
        assert!(rendered.contains("Ctrl-b …"));
        assert!(rendered.contains("command-palette"));
        assert!(rendered.contains("choose-tree"));
        assert!(rendered.contains("detach-client"));
    }

    #[test]
    fn short_hosts_overflow_into_more_columns_without_panicking() {
        for height in 0..30 {
            let host = Rect::new(0, 0, 90, height);
            let mut buffer = Buffer::empty(host);
            render(&BindingsConfig::default(), host, &mut buffer);
        }
        // Several columns on a 12-row host: titles truncate but stay present.
        let host = Rect::new(0, 0, 90, 12);
        let mut buffer = Buffer::empty(host);
        render(&BindingsConfig::default(), host, &mut buffer);
        assert!(text(&buffer).contains(':'));
        assert!(text(&buffer).contains("relo"));
    }
}
