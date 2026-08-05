use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::config::{SegmentConfig, SemanticStyle, StylesConfig};

pub(super) struct TokenValue {
    pub text: String,
    pub style: Option<SemanticStyle>,
}

impl TokenValue {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: None,
        }
    }

    pub fn styled(text: impl Into<String>, style: SemanticStyle) -> Self {
        Self {
            text: text.into(),
            style: Some(style),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ItemState {
    pub current: bool,
    pub selected: bool,
    pub closing: bool,
    pub attention: bool,
}

pub(super) fn render_token_segments(
    segments: &[SegmentConfig],
    group_style: Option<SemanticStyle>,
    state: ItemState,
    styles: &StylesConfig,
    mut resolve: impl FnMut(&str) -> TokenValue,
) -> Line<'static> {
    let mut spans = Vec::new();
    for segment in segments {
        let (text, token_style) = if let Some(text) = segment.text.as_deref() {
            (text.to_owned(), None)
        } else if let Some(token) = segment.token.as_deref() {
            let value = resolve(token);
            if value.text.is_empty() {
                continue;
            }
            let value_text = match segment.max_width {
                Some(width) => truncate(&value.text, usize::from(width)),
                None => value.text,
            };
            (
                format!("{}{}{}", segment.prefix, value_text, segment.suffix),
                value.style,
            )
        } else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let mut style = styles.apply(SemanticStyle::Normal, Style::default());
        if let Some(role) = group_style {
            style = styles.apply(role, style);
        }
        if let Some(role) = token_style {
            style = styles.apply(role, style);
        }
        if let Some(role) = segment.style {
            style = styles.apply(role, style);
        }
        for (enabled, role) in [
            (state.current, SemanticStyle::Current),
            (state.attention, SemanticStyle::Attention),
            (state.closing, SemanticStyle::Closing),
            (state.selected, SemanticStyle::Selected),
        ] {
            if enabled {
                style = styles.apply(role, style);
            }
        }
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

pub(super) fn truncate(value: &str, width: usize) -> String {
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
    let mut output = String::new();
    for grapheme in value.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > width - 1 {
            break;
        }
        output.push_str(grapheme);
        used += grapheme_width;
    }
    output.push('…');
    output
}

pub(super) fn truncate_line(line: &Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line.clone();
    }
    if width == 0 {
        return Line::default();
    }
    let mut spans = Vec::new();
    let mut remaining = width;
    for span in &line.spans {
        if remaining == 0 {
            break;
        }
        let truncated = UnicodeWidthStr::width(span.content.as_ref()) > remaining;
        let text = truncate(span.content.as_ref(), remaining);
        let used = UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(text, span.style));
        remaining = remaining.saturating_sub(used);
        if truncated || used > 0 && remaining == 0 {
            break;
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::config::{SegmentConfig, StylesConfig};

    #[test]
    fn empty_tokens_suppress_affixes_and_unicode_truncates_by_cells() {
        let segments = vec![SegmentConfig {
            token: Some("test".into()),
            prefix: "[".into(),
            suffix: "]".into(),
            max_width: Some(3),
            ..SegmentConfig::default()
        }];
        let empty = render_token_segments(
            &segments,
            None,
            ItemState::default(),
            &StylesConfig::default(),
            |_| TokenValue::plain(""),
        );
        assert_eq!(empty.width(), 0);
        let value = render_token_segments(
            &segments,
            None,
            ItemState::default(),
            &StylesConfig::default(),
            |_| TokenValue::plain("開発中"),
        );
        assert_eq!(value.to_string(), "[開…]");
    }

    #[test]
    fn normal_style_is_the_baseline_before_segment_and_state_overlays() {
        let styles: StylesConfig = toml::from_str(
            "[normal]\nforeground = 'blue'\n\n[current]\nadd_modifiers = ['bold']\n",
        )
        .unwrap();
        let line = render_token_segments(
            &[SegmentConfig {
                text: Some("value".into()),
                ..SegmentConfig::default()
            }],
            None,
            ItemState {
                current: true,
                ..ItemState::default()
            },
            &styles,
            |_| TokenValue::plain(""),
        );
        assert_eq!(line.spans[0].style.fg, Some(ratatui::style::Color::Blue));
        assert!(
            line.spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn line_truncation_never_renders_later_spans_after_the_ellipsis() {
        let line = Line::from(vec![Span::raw("界a"), Span::raw("!")]);
        assert_eq!(truncate_line(&line, 2).to_string(), "…");
    }
}
