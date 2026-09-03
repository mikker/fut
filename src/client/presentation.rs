use std::{fmt, ops::Deref};

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    config::{
        IconPreset, IconSet, SegmentConfig, SemanticStyle, StylesConfig, TokenVisual, UiConfig,
    },
    notifications::spinner_marker,
};
use crate::{
    extensions::TokenPresentation,
    protocol::ExtensionTokenStyle,
    resources::{MaterializedTokenValue, PresentationTokenAction, PresentationTokenTarget},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PresentationTokenInvocation {
    pub action: PresentationTokenAction,
    pub target: PresentationTokenTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TokenActionRegion {
    pub start: usize,
    pub width: usize,
    pub invocation: PresentationTokenInvocation,
}

#[derive(Clone, Debug, Default)]
pub(super) struct RenderedTokenLine {
    pub line: Line<'static>,
    pub actions: Vec<TokenActionRegion>,
}

impl RenderedTokenLine {
    pub fn action_at(&self, column: usize) -> Option<&PresentationTokenInvocation> {
        self.actions
            .iter()
            .find(|region| column >= region.start && column < region.start + region.width)
            .map(|region| &region.invocation)
    }

    pub fn truncated(&self, width: usize) -> Self {
        let line = truncate_line(&self.line, width);
        let visible_width = line.width();
        let actions = self
            .actions
            .iter()
            .filter_map(|region| {
                let clipped_width = visible_width.saturating_sub(region.start).min(region.width);
                (clipped_width > 0).then(|| TokenActionRegion {
                    start: region.start,
                    width: clipped_width,
                    invocation: region.invocation.clone(),
                })
            })
            .collect();
        Self { line, actions }
    }

    pub fn append_to(&self, line: &mut Line<'static>, actions: &mut Vec<TokenActionRegion>) {
        let offset = line.width();
        line.spans.extend(self.line.spans.iter().cloned());
        actions.extend(self.actions.iter().cloned().map(|mut region| {
            region.start += offset;
            region
        }));
    }
}

impl Deref for RenderedTokenLine {
    type Target = Line<'static>;

    fn deref(&self) -> &Self::Target {
        &self.line
    }
}

impl fmt::Display for RenderedTokenLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.line.fmt(formatter)
    }
}

impl From<Line<'static>> for RenderedTokenLine {
    fn from(line: Line<'static>) -> Self {
        Self {
            line,
            actions: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum TokenEffect {
    #[default]
    Plain,
    Pulse,
    Wave,
}

pub(super) struct TokenValue {
    pub text: String,
    pub style: Option<SemanticStyle>,
    effect: TokenEffect,
    elapsed_ms: usize,
    pub invocation: Option<PresentationTokenInvocation>,
}

impl TokenValue {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: None,
            effect: TokenEffect::Plain,
            elapsed_ms: 0,
            invocation: None,
        }
    }

    pub fn styled(text: impl Into<String>, style: SemanticStyle) -> Self {
        Self {
            text: text.into(),
            style: Some(style),
            effect: TokenEffect::Plain,
            elapsed_ms: 0,
            invocation: None,
        }
    }

    pub(super) fn with_effect(mut self, effect: TokenEffect, elapsed_ms: usize) -> Self {
        self.effect = effect;
        self.elapsed_ms = elapsed_ms;
        self
    }

    fn with_invocation(mut self, invocation: Option<PresentationTokenInvocation>) -> Self {
        self.invocation = invocation;
        self
    }
}

pub(super) fn extension_token_value(
    ui: &UiConfig,
    token: &str,
    value: &str,
    elapsed_ms: usize,
) -> TokenValue {
    if value.is_empty() {
        return TokenValue::plain("");
    }
    let declaration = ui
        .extensions
        .iter()
        .flat_map(|extension| extension.presentation_tokens())
        .find(|declaration| declaration.qualified_name() == token);
    if let Some(variant) = declaration.and_then(|declaration| declaration.variant(value)) {
        let nerd_font_text = (ui.icons.preset == IconPreset::NerdFont)
            .then_some(variant.nerd_font_text.as_ref())
            .flatten();
        let text = match (nerd_font_text, variant.presentation) {
            (Some(text), _) => text.clone(),
            (None, TokenPresentation::Plain) => variant.text.clone(),
            (None, TokenPresentation::Spinner) => spinner_marker(&ui.spinner, elapsed_ms).into(),
            (None, TokenPresentation::Pulse | TokenPresentation::Wave) => variant.text.clone(),
        };
        return TokenValue::styled(text, semantic_style(variant.style))
            .with_effect(token_effect(variant.presentation), elapsed_ms);
    }
    let presentation = declaration
        .map(|declaration| declaration.presentation())
        .unwrap_or_default();
    match presentation {
        TokenPresentation::Plain => TokenValue::plain(value),
        TokenPresentation::Spinner => TokenValue::plain(spinner_marker(&ui.spinner, elapsed_ms)),
        TokenPresentation::Pulse | TokenPresentation::Wave => {
            TokenValue::plain(value).with_effect(token_effect(presentation), elapsed_ms)
        }
    }
}

fn token_effect(presentation: TokenPresentation) -> TokenEffect {
    match presentation {
        TokenPresentation::Pulse => TokenEffect::Pulse,
        TokenPresentation::Wave => TokenEffect::Wave,
        TokenPresentation::Plain | TokenPresentation::Spinner => TokenEffect::Plain,
    }
}

fn effect_style(
    effect: TokenEffect,
    style: Style,
    index: usize,
    count: usize,
    elapsed_ms: usize,
) -> Style {
    let dim = match effect {
        TokenEffect::Plain => false,
        TokenEffect::Pulse => elapsed_ms % 2_000 >= 1_000,
        TokenEffect::Wave => {
            let padding = 3;
            let travel = count + padding * 2;
            let center = (elapsed_ms / 90) % travel;
            let position = index + padding;
            center.abs_diff(position) <= 1
        }
    };
    let style = style.remove_modifier(Modifier::DIM);
    if dim {
        style.add_modifier(Modifier::DIM)
    } else {
        style
    }
}

fn semantic_style(style: ExtensionTokenStyle) -> SemanticStyle {
    match style {
        ExtensionTokenStyle::Normal => SemanticStyle::Normal,
        ExtensionTokenStyle::Muted => SemanticStyle::Muted,
        ExtensionTokenStyle::Session => SemanticStyle::Session,
        ExtensionTokenStyle::Workspace => SemanticStyle::Workspace,
        ExtensionTokenStyle::Tab => SemanticStyle::Tab,
        ExtensionTokenStyle::Pane => SemanticStyle::Pane,
        ExtensionTokenStyle::Current => SemanticStyle::Current,
        ExtensionTokenStyle::Selected => SemanticStyle::Selected,
        ExtensionTokenStyle::Closing => SemanticStyle::Closing,
        ExtensionTokenStyle::Activity => SemanticStyle::Activity,
        ExtensionTokenStyle::Attention => SemanticStyle::Attention,
        ExtensionTokenStyle::Error => SemanticStyle::Error,
        ExtensionTokenStyle::Divider => SemanticStyle::Divider,
        ExtensionTokenStyle::Added => SemanticStyle::Added,
        ExtensionTokenStyle::Deleted => SemanticStyle::Deleted,
    }
}

pub(super) fn materialized_extension_token_value(
    ui: &UiConfig,
    token: &str,
    materialized: Option<(&MaterializedTokenValue, PresentationTokenTarget)>,
    elapsed_ms: usize,
) -> TokenValue {
    let Some((value, target)) = materialized else {
        return TokenValue::plain("");
    };
    extension_token_value(ui, token, &value.text, elapsed_ms).with_invocation(
        value
            .action
            .clone()
            .map(|action| PresentationTokenInvocation { action, target }),
    )
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
) -> RenderedTokenLine {
    let mut spans = Vec::new();
    let mut actions = Vec::new();
    let mut rendered_width = 0;
    for segment in segments {
        let (text, token_style, segment_style, effect, elapsed_ms, visual, invocation) =
            match segment {
                SegmentConfig::Text { text, style } => (
                    text.clone(),
                    None,
                    *style,
                    TokenEffect::Plain,
                    0,
                    TokenVisual::Plain,
                    None,
                ),
                SegmentConfig::Token {
                    token,
                    style,
                    prefix,
                    suffix,
                    max_width,
                    visual,
                } => {
                    let value = resolve(token);
                    if value.text.is_empty() {
                        continue;
                    }
                    let value_text = max_width.map_or(value.text.clone(), |width| {
                        truncate(&value.text, usize::from(width))
                    });
                    (
                        format!("{prefix}{value_text}{suffix}"),
                        value.style,
                        *style,
                        value.effect,
                        value.elapsed_ms,
                        *visual,
                        value.invocation,
                    )
                }
                SegmentConfig::Tabs => continue,
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
        if let Some(role) = segment_style {
            style = styles.apply(role, style);
        }
        style = apply_item_state(styles, state, style);
        if visual != TokenVisual::Plain {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if visual == TokenVisual::Pill
            && !icons.pill_left.is_empty()
            && !icons.pill_right.is_empty()
        {
            let cap = pill_cap_style(style, surface);
            let width = UnicodeWidthStr::width(icons.pill_left.as_str())
                + UnicodeWidthStr::width(text.as_str())
                + UnicodeWidthStr::width(icons.pill_right.as_str());
            let text_count = text.graphemes(true).count();
            spans.push(Span::styled(icons.pill_left.clone(), cap));
            push_effect_spans(&mut spans, &text, style, effect, elapsed_ms, text_count);
            spans.push(Span::styled(icons.pill_right.clone(), cap));
            if let Some(invocation) = invocation {
                actions.push(TokenActionRegion {
                    start: rendered_width,
                    width,
                    invocation,
                });
            }
            rendered_width += width;
            continue;
        }
        let width = UnicodeWidthStr::width(text.as_str());
        let count = text.graphemes(true).count();
        push_effect_spans(&mut spans, &text, style, effect, elapsed_ms, count);
        if let Some(invocation) = invocation {
            actions.push(TokenActionRegion {
                start: rendered_width,
                width,
                invocation,
            });
        }
        rendered_width += width;
    }
    RenderedTokenLine {
        line: Line::from(spans),
        actions,
    }
}

fn push_effect_spans(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    effect: TokenEffect,
    elapsed_ms: usize,
    total: usize,
) {
    match effect {
        TokenEffect::Plain => {
            spans.push(Span::styled(text.to_owned(), style));
            return;
        }
        TokenEffect::Pulse => {
            spans.push(Span::styled(
                text.to_owned(),
                effect_style(effect, style, 0, total, elapsed_ms),
            ));
            return;
        }
        TokenEffect::Wave => {}
    }
    spans.extend(text.graphemes(true).enumerate().map(|(index, grapheme)| {
        Span::styled(
            grapheme.to_owned(),
            effect_style(effect, style, index, total, elapsed_ms),
        )
    }));
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
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions/run");
        ui.extensions = extensions::load(&[root]).unwrap();
        ui
    }

    #[test]
    fn manifest_variants_supply_glyph_style_and_animation() {
        let mut ui = run_extension_ui();
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "launching", 0).text,
            "⁕"
        );
        let normal = extension_token_value(&ui, "workspace.extension.run.status", "launching", 0);
        assert_eq!(normal.effect, TokenEffect::Pulse);
        assert_eq!(normal.elapsed_ms, 0);
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "launching", 1_300)
                .elapsed_ms,
            1_300
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "play", 1).text,
            "‣"
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "play", 1).style,
            Some(SemanticStyle::Added)
        );
        ui.icons.preset = IconPreset::NerdFont;
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "pause", 1).text,
            "󰒓"
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "pause", 1).style,
            Some(SemanticStyle::Muted)
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "launching", 1).text,
            "󱑠"
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "launching", 1_300).effect,
            TokenEffect::Pulse
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "play", 1).text,
            "󱤵"
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.run.status", "stop", 1).text,
            "󱤷"
        );
        assert_eq!(
            extension_token_value(&ui, "workspace.extension.unknown.state", "spinner", 1).text,
            "spinner"
        );
    }

    #[test]
    fn pulse_changes_the_complete_text_without_fading_pill_caps() {
        let mut ui = run_extension_ui();
        ui.icons.preset = IconPreset::NerdFont;
        let icons = ui.icons.resolve();
        let render = |frame| {
            render_token_segments(
                &[SegmentConfig::Token {
                    token: "workspace.extension.run.status".into(),
                    style: None,
                    prefix: "[".into(),
                    suffix: "]".into(),
                    max_width: None,
                    visual: TokenVisual::Pill,
                }],
                None,
                ItemState::default(),
                &ui.styles,
                &icons,
                |token| extension_token_value(&ui, token, "launching", frame),
            )
        };

        let normal = render(0);
        let faint = render(1_000);
        assert_eq!(normal.to_string(), "\u{e0b6}[󱑠]\u{e0b4}");
        assert!(
            normal
                .spans
                .iter()
                .all(|span| { !span.style.add_modifier.contains(Modifier::DIM) })
        );
        assert!(
            !faint
                .spans
                .first()
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            !faint
                .spans
                .last()
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            faint.spans[1..faint.spans.len() - 1]
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::DIM))
        );
    }

    #[test]
    fn wave_moves_a_faint_band_across_text_without_fading_pill_caps() {
        let icons = icons(IconPreset::NerdFont);
        let render = |elapsed_ms| {
            render_token_segments(
                &[SegmentConfig::Token {
                    token: "status".into(),
                    style: None,
                    prefix: " ".into(),
                    suffix: " ".into(),
                    max_width: None,
                    visual: TokenVisual::Pill,
                }],
                None,
                ItemState::default(),
                &StylesConfig::default(),
                &icons,
                |_| TokenValue::plain("go").with_effect(TokenEffect::Wave, elapsed_ms),
            )
        };

        let first = render(270);
        let next = render(360);
        let dimmed = |line: &RenderedTokenLine| {
            line.spans
                .iter()
                .map(|span| span.style.add_modifier.contains(Modifier::DIM))
                .collect::<Vec<_>>()
        };
        assert_eq!(first.to_string(), "\u{e0b6} go \u{e0b4}");
        assert_ne!(dimmed(&first), dimmed(&next));
        assert!(!dimmed(&first)[0]);
        assert!(!dimmed(&first)[first.spans.len() - 1]);
        assert!(dimmed(&first).into_iter().any(|dim| dim));
        assert!(dimmed(&next).into_iter().any(|dim| !dim));
    }

    #[test]
    fn effects_preserve_configured_glyph_weight() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let pulse = effect_style(TokenEffect::Pulse, bold, 0, 1, 1_000);
        let wave = effect_style(TokenEffect::Wave, bold, 0, 1, 270);

        assert!(pulse.add_modifier.contains(Modifier::BOLD | Modifier::DIM));
        assert!(wave.add_modifier.contains(Modifier::BOLD | Modifier::DIM));
    }

    #[test]
    fn empty_tokens_suppress_affixes_and_unicode_truncates_by_cells() {
        let icons = icons(IconPreset::NerdFont);
        let segments = vec![SegmentConfig::Token {
            token: "test".into(),
            style: None,
            prefix: "[".into(),
            suffix: "]".into(),
            max_width: Some(3),
            visual: TokenVisual::Pill,
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
    fn actionable_token_region_includes_affixes_and_pill_caps_and_clips_to_visible_cells() {
        let icons = icons(IconPreset::NerdFont);
        let pane_id = crate::domain::PaneId::new();
        let target = PresentationTokenTarget::Pane(pane_id);
        let value = MaterializedTokenValue::new(
            "go".into(),
            Some(PresentationTokenAction::Pane { pane_id }),
        );
        let rendered = render_token_segments(
            &[
                SegmentConfig::Text {
                    text: ".".into(),
                    style: None,
                },
                SegmentConfig::Token {
                    token: "pane.extension.demo.action".into(),
                    style: None,
                    prefix: "[".into(),
                    suffix: "]".into(),
                    max_width: None,
                    visual: TokenVisual::Pill,
                },
                SegmentConfig::Text {
                    text: "!".into(),
                    style: None,
                },
            ],
            None,
            ItemState::default(),
            &StylesConfig::default(),
            &icons,
            |token| {
                materialized_extension_token_value(
                    &UiConfig::default(),
                    token,
                    Some((&value, target)),
                    0,
                )
            },
        );
        let invocation = PresentationTokenInvocation {
            action: PresentationTokenAction::Pane { pane_id },
            target,
        };

        assert_eq!(rendered.to_string(), ".\u{e0b6}[go]\u{e0b4}!");
        assert_eq!(rendered.action_at(0), None);
        for column in 1..7 {
            assert_eq!(rendered.action_at(column), Some(&invocation));
        }
        assert_eq!(rendered.action_at(7), None);

        let wide = MaterializedTokenValue::new(
            "界x".into(),
            Some(PresentationTokenAction::Pane { pane_id }),
        );
        let wide = render_token_segments(
            &[SegmentConfig::Token {
                token: "pane.extension.demo.action".into(),
                style: None,
                prefix: String::new(),
                suffix: String::new(),
                max_width: None,
                visual: TokenVisual::Plain,
            }],
            None,
            ItemState::default(),
            &StylesConfig::default(),
            &icons,
            |token| {
                materialized_extension_token_value(
                    &UiConfig::default(),
                    token,
                    Some((&wide, target)),
                    0,
                )
            },
        )
        .truncated(2);
        assert_eq!(wide.to_string(), "…");
        assert_eq!(wide.action_at(0), Some(&invocation));
        assert_eq!(wide.action_at(1), None);
    }

    #[test]
    fn inverted_pill_uses_semantic_fill_and_keeps_spinner_and_modifiers_inside_caps() {
        let styles: StylesConfig = toml::from_str(
            "[normal]\nbackground = 'blue'\nadd_modifiers = ['italic']\n\n[attention]\nforeground = 'yellow'\nbackground = 'red'\nadd_modifiers = ['bold']\n",
        )
        .unwrap();
        let icons = icons(IconPreset::NerdFont);
        let line = render_token_segments(
            &[SegmentConfig::Token {
                token: "status".into(),
                prefix: " ".into(),
                suffix: " ".into(),
                style: Some(SemanticStyle::Attention),
                max_width: None,
                visual: TokenVisual::Pill,
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
                &[SegmentConfig::Token {
                    token: "status".into(),
                    style: Some(SemanticStyle::Added),
                    prefix: String::new(),
                    suffix: String::new(),
                    max_width: None,
                    visual: TokenVisual::Pill,
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
            &[SegmentConfig::Text {
                text: "value".into(),
                style: None,
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
