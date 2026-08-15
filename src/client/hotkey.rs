use std::ops::Range;

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

#[derive(Clone)]
pub(super) struct HotkeyButton<A> {
    key: &'static str,
    label: &'static str,
    action: A,
}

impl<A> HotkeyButton<A> {
    pub(super) const fn new(key: &'static str, label: &'static str, action: A) -> Self {
        Self { key, label, action }
    }
}

pub(super) struct HotkeyLine<A> {
    pub(super) line: Line<'static>,
    hits: Vec<(Range<usize>, A)>,
}

impl<A: Clone> HotkeyLine<A> {
    pub(super) fn inline(
        buttons: &[HotkeyButton<A>],
        prefix: &'static str,
        separator: &'static str,
        suffix: &'static str,
        key_style: Style,
        label_style: Style,
    ) -> Self {
        let mut spans = Vec::new();
        let mut hits = Vec::new();
        let mut column = UnicodeWidthStr::width(prefix);
        if !prefix.is_empty() {
            spans.push(Span::styled(prefix, label_style));
        }
        for (index, button) in buttons.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(separator, label_style));
                column += UnicodeWidthStr::width(separator);
            }
            let start = column;
            spans.push(Span::styled(button.key, key_style));
            column += UnicodeWidthStr::width(button.key);
            if !button.label.is_empty() {
                spans.push(Span::styled(" ", label_style));
                spans.push(Span::styled(button.label, label_style));
                column += 1 + UnicodeWidthStr::width(button.label);
            }
            hits.push((start..column, button.action.clone()));
        }
        if !suffix.is_empty() {
            spans.push(Span::styled(suffix, label_style));
        }
        Self {
            line: Line::from(spans),
            hits,
        }
    }

    pub(super) fn row(
        button: &HotkeyButton<A>,
        icon: &str,
        key_style: Style,
        label_style: Style,
    ) -> Self {
        let icon = (!icon.is_empty()).then(|| Span::styled(format!("{icon}  "), label_style));
        let line = Line::from(
            [
                Some(Span::styled(format!(" {}  ", button.key), key_style)),
                icon,
                Some(Span::styled(button.label, label_style)),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
        );
        let width = line.width();
        Self {
            line,
            hits: vec![(0..width, button.action.clone())],
        }
    }

    pub(super) fn action_at(&self, column: usize) -> Option<A> {
        self.hits
            .iter()
            .find(|(range, _)| range.contains(&column))
            .map(|(_, action)| action.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_buttons_share_rendering_and_exact_hit_regions() {
        let line = HotkeyLine::inline(
            &[
                HotkeyButton::new("c", "new", 1),
                HotkeyButton::new("esc", "", 2),
            ],
            " ",
            " · ",
            " ",
            Style::default(),
            Style::default(),
        );
        assert_eq!(line.line.to_string(), " c new · esc ");
        assert_eq!(line.action_at(1), Some(1));
        assert_eq!(line.action_at(4), Some(1));
        assert_eq!(line.action_at(7), None);
        assert_eq!(line.action_at(10), Some(2));
    }
}
