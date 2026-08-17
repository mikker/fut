use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    config::{IconSet, SegmentConfig, SemanticStyle, StylesConfig, UiConfig},
    notifications::spinner_marker,
};
use crate::extensions::TokenPresentation;

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

pub(super) fn extension_token_value(
    ui: &UiConfig,
    token: &str,
    value: &str,
    spinner_frame: usize,
) -> TokenValue {
    if value.is_empty() {
        return TokenValue::plain("");
    }
    let presentation = ui
        .extensions
        .iter()
        .flat_map(|extension| extension.presentation_tokens())
        .find(|declaration| declaration.qualified_name() == token)
        .map(|declaration| declaration.presentation())
        .unwrap_or_default();
    match presentation {
        TokenPresentation::Plain => TokenValue::plain(value),
        TokenPresentation::Spinner => TokenValue::plain(spinner_marker(spinner_frame)),
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ItemState {
    pub current: bool,
    pub selected: bool,
    pub closing: bool,
    pub attention: bool,
}

pub(super) fn apply_item_state(styles: &StylesConfig, state: ItemState, mut style: Style) -> Style {
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
    style
}

/// Draw a powerline pill cap using the item's fill on its surrounding surface.
pub(super) fn pill_cap_style(item: Style, surface: Style) -> Style {
    let fill = if item.add_modifier.contains(Modifier::REVERSED) {
        item.fg
    } else {
        item.bg
    };
    Style {
        fg: fill,
        bg: surface.bg,
        ..Style::default()
    }
}

pub(super) fn render_token_segments(
    segments: &[SegmentConfig],
    group_style: Option<SemanticStyle>,
    state: ItemState,
    styles: &StylesConfig,
    icons: &IconSet,
    mut resolve: impl FnMut(&str) -> TokenValue,
) -> Line<'static> {
    let mut spans = Vec::new();
    for segment in segments {
        let (text, token_style, is_token) = if let Some(text) = segment.text.as_deref() {
            (text.to_owned(), None, false)
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
                true,
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
        let surface = apply_item_state(styles, state, style);
        if let Some(role) = token_style {
            style = styles.apply(role, style);
        }
        if let Some(role) = segment.style {
            style = styles.apply(role, style);
        }
        style = apply_item_state(styles, state, style);
        if is_token && segment.inverted {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if is_token && segment.pill && !icons.pill_left.is_empty() && !icons.pill_right.is_empty() {
            let cap = pill_cap_style(style, surface);
            spans.push(Span::styled(icons.pill_left.clone(), cap));
            spans.push(Span::styled(text, style));
            spans.push(Span::styled(icons.pill_right.clone(), cap));
            continue;
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
    use std::path::Path;

    use super::*;
    use crate::{
        client::config::{IconPreset, SegmentConfig, StylesConfig},
        extensions,
    };

    fn icons(preset: IconPreset) -> IconSet {
        let mut ui = UiConfig::default();
        ui.icons.preset = preset;
        ui.icons.resolve()
    }

    fn run_extension_ui() -> UiConfig {
        let mut ui = UiConfig::default();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/extensions/run");
        ui.extensions = extensions::load(&[root]).unwrap();
        ui
    }

    #[test]
    fn only_manifest_animated_tokens_replace_populated_plain_text() {
        let ui = run_extension_ui();
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.launching", "1", 0).text,
            "⠋"
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.launching", "1", 1).text,
            "⠙"
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.launching", "", 1).text,
            ""
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.play", "spinner", 1).text,
            "spinner"
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.unknown.state", "spinner", 1).text,
            "spinner"
        );
    }

    #[test]
    fn empty_tokens_suppress_affixes_and_unicode_truncates_by_cells() {
        let icons = icons(IconPreset::NerdFont);
        let segments = vec![SegmentConfig {
            token: Some("test".into()),
            prefix: "[".into(),
            suffix: "]".into(),
            max_width: Some(3),
            inverted: true,
            pill: true,
            ..SegmentConfig::default()
        }];
        let empty = render_token_segments(
            &segments,
            None,
            ItemState::default(),
            &StylesConfig::default(),
            &icons,
            |_| TokenValue::plain(""),
        );
        assert_eq!(empty.width(), 0);
        let value = render_token_segments(
            &segments,
            None,
            ItemState::default(),
            &StylesConfig::default(),
            &icons,
            |_| TokenValue::plain("開発中"),
        );
        assert_eq!(value.to_string(), "\u{e0b6}[開…]\u{e0b4}");
        assert_eq!(value.width(), 7);
    }

    #[test]
    fn inverted_pill_uses_semantic_fill_and_keeps_spinner_and_modifiers_inside_caps() {
        let styles: StylesConfig = toml::from_str(
            "[normal]\nbackground = 'blue'\nadd_modifiers = ['italic']\n\n[attention]\nforeground = 'yellow'\nbackground = 'red'\nadd_modifiers = ['bold']\n",
        )
        .unwrap();
        let icons = icons(IconPreset::NerdFont);
        let line = render_token_segments(
            &[SegmentConfig {
                token: Some("status".into()),
                prefix: " ".into(),
                suffix: " ".into(),
                style: Some(SemanticStyle::Attention),
                inverted: true,
                pill: true,
                ..SegmentConfig::default()
            }],
            None,
            ItemState::default(),
            &styles,
            &icons,
            |_| TokenValue::plain("⠙"),
        );

        assert_eq!(line.to_string(), "\u{e0b6} ⠙ \u{e0b4}");
        assert_eq!(line.width(), 5);
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].style.fg, Some(ratatui::style::Color::Yellow));
        assert_eq!(line.spans[0].style.bg, Some(ratatui::style::Color::Blue));
        assert_eq!(line.spans[0].style.add_modifier, Modifier::empty());
        assert_eq!(line.spans[1].style.fg, Some(ratatui::style::Color::Yellow));
        assert_eq!(line.spans[1].style.bg, Some(ratatui::style::Color::Red));
        assert!(
            line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED | Modifier::BOLD | Modifier::ITALIC)
        );
        assert_eq!(line.spans[2].style, line.spans[0].style);
    }

    #[test]
    fn pill_without_configured_caps_falls_back_to_inverted_content_only() {
        for preset in [IconPreset::Unicode, IconPreset::Ascii] {
            let line = render_token_segments(
                &[SegmentConfig {
                    token: Some("status".into()),
                    style: Some(SemanticStyle::Added),
                    inverted: true,
                    pill: true,
                    ..SegmentConfig::default()
                }],
                None,
                ItemState::default(),
                &StylesConfig::default(),
                &icons(preset),
                |_| TokenValue::plain("▶"),
            );

            assert_eq!(line.to_string(), "▶");
            assert_eq!(line.width(), 1);
            assert_eq!(line.spans.len(), 1);
            assert_eq!(line.spans[0].style.fg, Some(ratatui::style::Color::Green));
            assert!(
                line.spans[0]
                    .style
                    .add_modifier
                    .contains(Modifier::REVERSED)
            );
        }
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
            &icons(IconPreset::Unicode),
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
