use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Deserializer, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::actions::{
    ALL_ACTIONS, ClientAction, config_key, default_suffix, parse_key, suffix_name,
};
use super::spinners::{SpinnerStyle, builtin_spinner};
use crate::{
    command::PopupSize,
    extension_store,
    extensions::{self, Extension, ExtensionCommandExecution, ExtensionCommandMode},
    resources::{ExtensionConfigTable, TrustedProjectConfig},
};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_EXTENSION_CONFIG_KEYS: usize = 128;
const MAX_EXTENSION_CONFIG_DEPTH: usize = 8;
const MAX_EXTENSION_CONFIG_KEY_BYTES: usize = 128;
const MAX_EXTENSION_CONFIG_VALUE_BYTES: usize = 4 * 1024;
const MAX_EXTENSION_CONFIG_ARRAY_VALUES: usize = 128;
const MAX_EXTENSION_CONFIG_SERIALIZED_BYTES: usize = 16 * 1024;
const MAX_SEGMENTS: usize = 64;
const MAX_TEXT_BYTES: usize = 1024;
const MAX_SPINNER_FRAMES: usize = 256;
const MAX_SPINNER_WIDTH: usize = 32;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TabBarPosition {
    #[default]
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SidebarVisibility {
    Visible,
    #[default]
    Automatic,
    Hidden,
}

impl SidebarVisibility {
    pub fn set(&mut self, visibility: Self) {
        *self = visibility;
    }

    pub fn cycle(&mut self) {
        *self = match self {
            Self::Visible => Self::Automatic,
            Self::Automatic => Self::Hidden,
            Self::Hidden => Self::Visible,
        };
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Automatic => "automatic",
            Self::Hidden => "hidden",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SidebarDisplay {
    #[default]
    Expanded,
    Minimized,
}

impl SidebarDisplay {
    pub fn set(&mut self, display: Self) {
        *self = display;
    }

    pub fn toggle(&mut self) {
        *self = match self {
            Self::Expanded => Self::Minimized,
            Self::Minimized => Self::Expanded,
        };
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Expanded => "expanded",
            Self::Minimized => "minimized",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PaneLayoutPolicy {
    #[default]
    Splits,
    Accordion,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum IconPreset {
    Ascii,
    #[default]
    Unicode,
    NerdFont,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SemanticStyle {
    #[default]
    Normal,
    Muted,
    Session,
    Workspace,
    Tab,
    Pane,
    Current,
    Selected,
    Closing,
    Activity,
    Attention,
    Error,
    Divider,
    Added,
    Deleted,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(transparent)]
pub(super) struct BindingsConfig {
    values: BTreeMap<String, String>,
    #[serde(skip)]
    commands: Vec<PaletteCommand>,
    #[serde(skip, default = "default_prefix")]
    prefix: Vec<u8>,
}

fn default_prefix() -> Vec<u8> {
    vec![2]
}

impl Default for BindingsConfig {
    fn default() -> Self {
        Self {
            values: BTreeMap::new(),
            commands: Vec::new(),
            prefix: default_prefix(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PaletteCommand {
    pub title: String,
    pub binding: Option<String>,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub execution: ExtensionCommandExecution,
    pub extension: Option<ExtensionCommandIdentity>,
    pub fields: Vec<crate::extensions::ExtensionCommandField>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaletteCommandDto {
    title: String,
    #[serde(default)]
    binding: Option<String>,
    program: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    size: PopupSize,
    #[serde(default)]
    activate_opened: bool,
    #[serde(default)]
    mode: ExtensionCommandMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExtensionCommandIdentity {
    pub id: String,
    pub root: PathBuf,
    pub command: String,
}

impl ExtensionCommandIdentity {
    fn slug(&self) -> String {
        format!("{}:{}", self.id, self.command)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtensionCommandConfig {
    args: Vec<String>,
}

impl PaletteCommand {
    pub(super) fn slug(&self) -> Option<String> {
        self.extension.as_ref().map(ExtensionCommandIdentity::slug)
    }

    pub(super) const fn mode(&self) -> ExtensionCommandMode {
        self.execution.mode()
    }
}

impl<'de> Deserialize<'de> for PaletteCommand {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let dto = PaletteCommandDto::deserialize(deserializer)?;
        let execution = match dto.mode {
            ExtensionCommandMode::Interactive => ExtensionCommandExecution::Interactive {
                size: dto.size,
                activate_opened: dto.activate_opened,
            },
            ExtensionCommandMode::Background => {
                if dto.activate_opened || dto.size.width.is_some() || dto.size.height.is_some() {
                    return Err(serde::de::Error::custom(
                        "background command cannot declare size or activate_opened",
                    ));
                }
                ExtensionCommandExecution::Background
            }
        };
        Ok(Self {
            title: dto.title,
            binding: dto.binding,
            program: dto.program,
            args: dto.args,
            execution,
            extension: None,
            fields: Vec::new(),
        })
    }
}

impl BindingsConfig {
    pub(super) fn parse_suffix(&self, value: &str) -> Option<(Vec<u8>, String)> {
        if value == "prefix" {
            return Some((self.prefix.clone(), self.prefix_label()));
        }
        parse_key(value)
    }

    fn set_prefix(&mut self, prefix: Vec<u8>) {
        self.prefix = prefix;
    }

    pub(super) fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    pub(super) fn prefix_label(&self) -> String {
        suffix_name(&self.prefix)
    }

    fn default_suffix(&self, action: ClientAction) -> Option<&[u8]> {
        if action == ClientAction::FocusNextNotification {
            Some(&self.prefix)
        } else {
            default_suffix(action)
        }
    }

    pub(super) fn command(&self, index: usize) -> Option<&PaletteCommand> {
        self.commands.get(index)
    }

    pub(super) fn extension_command(
        &self,
        extension_id: &str,
        command: &str,
    ) -> Option<&PaletteCommand> {
        self.commands.iter().find(|candidate| {
            candidate
                .extension
                .as_ref()
                .is_some_and(|identity| identity.id == extension_id && identity.command == command)
        })
    }

    pub(super) fn suffix(&self, action: ClientAction) -> Option<Vec<u8>> {
        match self.values.get(config_key(action)) {
            Some(value) => self.parse_suffix(value).map(|(bytes, _)| bytes),
            None => {
                let default = self.default_suffix(action)?;
                // Shift-S was historically available for user-bound actions.
                // Let an existing explicit binding keep it instead of making
                // the newly added project opener invalidate that config.
                let displaced = action == ClientAction::OpenProject
                    && self.values.iter().any(|(key, value)| {
                        key != config_key(action)
                            && self
                                .parse_suffix(value)
                                .is_some_and(|(suffix, _)| suffix == default)
                    });
                (!displaced).then(|| default.to_vec())
            }
        }
    }

    pub(super) fn suffix_label(&self, action: ClientAction) -> String {
        self.values.get(config_key(action)).map_or_else(
            || {
                self.default_suffix(action)
                    .map_or_else(|| "Unbound".into(), suffix_name)
            },
            |value| self.parse_suffix(value).expect("bindings are validated").1,
        )
    }

    pub(super) fn label(&self, action: ClientAction) -> String {
        if let ClientAction::RunCommand(index) = action {
            return self.commands.get(index).map_or_else(
                || "Unbound".into(),
                |command| {
                    command
                        .binding
                        .as_ref()
                        .map_or_else(String::new, |binding| {
                            format!(
                                "{} {}",
                                self.prefix_label(),
                                self.parse_suffix(binding)
                                    .expect("commands are validated")
                                    .1
                            )
                        })
                },
            );
        }
        if self.commands.iter().any(|command| {
            command.binding.as_ref().is_some_and(|binding| {
                self.parse_suffix(binding)
                    .is_some_and(|(suffix, _)| self.suffix(action).as_deref() == Some(&suffix))
            })
        }) {
            return "Unbound".into();
        }
        if self.suffix(action).is_none() {
            return "Unbound".into();
        }
        format!("{} {}", self.prefix_label(), self.suffix_label(action))
    }

    pub(super) fn action_for_suffix(&self, suffix: &[u8]) -> Option<ClientAction> {
        if let Some(index) = self.commands.iter().position(|command| {
            command.binding.as_ref().is_some_and(|binding| {
                self.parse_suffix(binding)
                    .is_some_and(|(bytes, _)| bytes == suffix)
            })
        }) {
            return Some(ClientAction::RunCommand(index));
        }
        ALL_ACTIONS.into_iter().find(|action| {
            self.suffix(*action).as_deref() == Some(suffix) && self.label(*action) != "Unbound"
        })
    }

    pub(super) fn commands(&self) -> impl Iterator<Item = (usize, &PaletteCommand)> {
        self.commands.iter().enumerate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UiColor {
    Default,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl<'de> Deserialize<'de> for UiColor {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_color(&value).map_err(serde::de::Error::custom)
    }
}

fn parse_color(value: &str) -> std::result::Result<UiColor, String> {
    let color = match value {
        "default" => UiColor::Default,
        "black" => UiColor::Black,
        "red" => UiColor::Red,
        "green" => UiColor::Green,
        "yellow" => UiColor::Yellow,
        "blue" => UiColor::Blue,
        "magenta" => UiColor::Magenta,
        "cyan" => UiColor::Cyan,
        "gray" => UiColor::Gray,
        "dark_gray" => UiColor::DarkGray,
        "light_red" => UiColor::LightRed,
        "light_green" => UiColor::LightGreen,
        "light_yellow" => UiColor::LightYellow,
        "light_blue" => UiColor::LightBlue,
        "light_magenta" => UiColor::LightMagenta,
        "light_cyan" => UiColor::LightCyan,
        "white" => UiColor::White,
        _ if value.starts_with("index:") => UiColor::Indexed(
            value[6..]
                .parse::<u8>()
                .map_err(|_| format!("invalid indexed color {value:?}"))?,
        ),
        _ if value.len() == 7
            && value.starts_with('#')
            && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            let component = |range| {
                u8::from_str_radix(&value[range], 16)
                    .map_err(|_| format!("invalid RGB color {value:?}"))
            };
            UiColor::Rgb(component(1..3)?, component(3..5)?, component(5..7)?)
        }
        _ => return Err(format!("unknown UI color {value:?}")),
    };
    Ok(color)
}

impl From<UiColor> for Color {
    fn from(value: UiColor) -> Self {
        match value {
            UiColor::Default => Self::Reset,
            UiColor::Black => Self::Black,
            UiColor::Red => Self::Red,
            UiColor::Green => Self::Green,
            UiColor::Yellow => Self::Yellow,
            UiColor::Blue => Self::Blue,
            UiColor::Magenta => Self::Magenta,
            UiColor::Cyan => Self::Cyan,
            UiColor::Gray => Self::Gray,
            UiColor::DarkGray => Self::DarkGray,
            UiColor::LightRed => Self::LightRed,
            UiColor::LightGreen => Self::LightGreen,
            UiColor::LightYellow => Self::LightYellow,
            UiColor::LightBlue => Self::LightBlue,
            UiColor::LightMagenta => Self::LightMagenta,
            UiColor::LightCyan => Self::LightCyan,
            UiColor::White => Self::White,
            UiColor::Indexed(index) => Self::Indexed(index),
            UiColor::Rgb(red, green, blue) => Self::Rgb(red, green, blue),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ModifierName {
    Bold,
    Dim,
    Italic,
    Underlined,
    Reversed,
    CrossedOut,
}

impl From<ModifierName> for Modifier {
    fn from(value: ModifierName) -> Self {
        match value {
            ModifierName::Bold => Self::BOLD,
            ModifierName::Dim => Self::DIM,
            ModifierName::Italic => Self::ITALIC,
            ModifierName::Underlined => Self::UNDERLINED,
            ModifierName::Reversed => Self::REVERSED,
            ModifierName::CrossedOut => Self::CROSSED_OUT,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct StyleConfig {
    foreground: Option<UiColor>,
    background: Option<UiColor>,
    add_modifiers: Vec<ModifierName>,
    remove_modifiers: Vec<ModifierName>,
}

impl StyleConfig {
    pub fn apply(&self, mut style: Style) -> Style {
        if let Some(foreground) = self.foreground {
            style = style.fg(foreground.into());
        }
        if let Some(background) = self.background {
            style = style.bg(background.into());
        }
        for modifier in &self.add_modifiers {
            style = style.add_modifier((*modifier).into());
        }
        for modifier in &self.remove_modifiers {
            style = style.remove_modifier((*modifier).into());
        }
        style
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StylesConfig {
    normal: StyleConfig,
    muted: StyleConfig,
    session: StyleConfig,
    workspace: StyleConfig,
    tab: StyleConfig,
    pane: StyleConfig,
    current: StyleConfig,
    selected: StyleConfig,
    closing: StyleConfig,
    activity: StyleConfig,
    attention: StyleConfig,
    error: StyleConfig,
    divider: StyleConfig,
    added: StyleConfig,
    deleted: StyleConfig,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StylePatch {
    foreground: Option<UiColor>,
    background: Option<UiColor>,
    add_modifiers: Option<Vec<ModifierName>>,
    remove_modifiers: Option<Vec<ModifierName>>,
}

impl StylePatch {
    fn apply(self, style: &mut StyleConfig) {
        if let Some(foreground) = self.foreground {
            style.foreground = Some(foreground);
        }
        if let Some(background) = self.background {
            style.background = Some(background);
        }
        if let Some(add_modifiers) = self.add_modifiers {
            style.add_modifiers = add_modifiers;
        }
        if let Some(remove_modifiers) = self.remove_modifiers {
            style.remove_modifiers = remove_modifiers;
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StylesPatch {
    normal: Option<StylePatch>,
    muted: Option<StylePatch>,
    session: Option<StylePatch>,
    workspace: Option<StylePatch>,
    tab: Option<StylePatch>,
    pane: Option<StylePatch>,
    current: Option<StylePatch>,
    selected: Option<StylePatch>,
    closing: Option<StylePatch>,
    activity: Option<StylePatch>,
    attention: Option<StylePatch>,
    error: Option<StylePatch>,
    divider: Option<StylePatch>,
    added: Option<StylePatch>,
    deleted: Option<StylePatch>,
}

impl<'de> Deserialize<'de> for StylesConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let patch = StylesPatch::deserialize(deserializer)?;
        let mut styles = Self::default();
        for (patch, style) in [
            (patch.normal, &mut styles.normal),
            (patch.muted, &mut styles.muted),
            (patch.session, &mut styles.session),
            (patch.workspace, &mut styles.workspace),
            (patch.tab, &mut styles.tab),
            (patch.pane, &mut styles.pane),
            (patch.current, &mut styles.current),
            (patch.selected, &mut styles.selected),
            (patch.closing, &mut styles.closing),
            (patch.activity, &mut styles.activity),
            (patch.attention, &mut styles.attention),
            (patch.error, &mut styles.error),
            (patch.divider, &mut styles.divider),
            (patch.added, &mut styles.added),
            (patch.deleted, &mut styles.deleted),
        ] {
            if let Some(patch) = patch {
                patch.apply(style);
            }
        }
        Ok(styles)
    }
}

impl Default for StylesConfig {
    fn default() -> Self {
        let with = |modifier| StyleConfig {
            add_modifiers: vec![modifier],
            ..StyleConfig::default()
        };
        Self {
            normal: StyleConfig::default(),
            muted: with(ModifierName::Dim),
            session: StyleConfig {
                foreground: Some(UiColor::Red),
                ..StyleConfig::default()
            },
            workspace: StyleConfig {
                foreground: Some(UiColor::Blue),
                ..StyleConfig::default()
            },
            tab: StyleConfig {
                foreground: Some(UiColor::Green),
                ..StyleConfig::default()
            },
            pane: StyleConfig {
                foreground: Some(UiColor::Magenta),
                ..StyleConfig::default()
            },
            // Reversed blue: the background renders blue and the text renders in
            // the terminal's own background color, whatever the theme.
            current: StyleConfig {
                foreground: Some(UiColor::Blue),
                add_modifiers: vec![ModifierName::Reversed],
                remove_modifiers: vec![ModifierName::Underlined],
                ..StyleConfig::default()
            },
            selected: StyleConfig {
                background: Some(UiColor::DarkGray),
                remove_modifiers: vec![ModifierName::Reversed],
                ..StyleConfig::default()
            },
            closing: with(ModifierName::Dim),
            activity: StyleConfig {
                foreground: Some(UiColor::LightCyan),
                ..StyleConfig::default()
            },
            attention: StyleConfig {
                foreground: Some(UiColor::Yellow),
                add_modifiers: vec![ModifierName::Bold],
                ..StyleConfig::default()
            },
            error: StyleConfig {
                foreground: Some(UiColor::Red),
                add_modifiers: vec![ModifierName::Bold],
                ..StyleConfig::default()
            },
            divider: StyleConfig {
                foreground: Some(UiColor::DarkGray),
                ..StyleConfig::default()
            },
            added: StyleConfig {
                foreground: Some(UiColor::Green),
                ..StyleConfig::default()
            },
            deleted: StyleConfig {
                foreground: Some(UiColor::Red),
                ..StyleConfig::default()
            },
        }
    }
}

impl StylesConfig {
    pub fn apply(&self, role: SemanticStyle, style: Style) -> Style {
        match role {
            SemanticStyle::Normal => &self.normal,
            SemanticStyle::Muted => &self.muted,
            SemanticStyle::Session => &self.session,
            SemanticStyle::Workspace => &self.workspace,
            SemanticStyle::Tab => &self.tab,
            SemanticStyle::Pane => &self.pane,
            SemanticStyle::Current => &self.current,
            SemanticStyle::Selected => &self.selected,
            SemanticStyle::Closing => &self.closing,
            SemanticStyle::Activity => &self.activity,
            SemanticStyle::Attention => &self.attention,
            SemanticStyle::Error => &self.error,
            SemanticStyle::Divider => &self.divider,
            SemanticStyle::Added => &self.added,
            SemanticStyle::Deleted => &self.deleted,
        }
        .apply(style)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct IconsConfig {
    pub preset: IconPreset,
    pub current: Option<String>,
    pub closing: Option<String>,
    pub overflow: Option<String>,
    pub workspace: Option<String>,
    pub tab: Option<String>,
    pub zoom: Option<String>,
    pub notification: Option<String>,
    pub vertical_divider: Option<String>,
    pub pill_left: Option<String>,
    pub pill_right: Option<String>,
}

impl Default for IconsConfig {
    fn default() -> Self {
        Self {
            preset: IconPreset::Unicode,
            current: None,
            closing: None,
            overflow: None,
            workspace: None,
            tab: None,
            zoom: None,
            notification: None,
            vertical_divider: None,
            pill_left: None,
            pill_right: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IconSet {
    pub current: String,
    pub closing: String,
    pub overflow: String,
    pub workspace: String,
    pub tab: String,
    pub zoom: String,
    pub notification: String,
    pub vertical_divider: String,
    pub pill_left: String,
    pub pill_right: String,
}

impl IconsConfig {
    pub fn resolve(&self) -> IconSet {
        let defaults = match self.preset {
            IconPreset::Ascii => ["*", "x", "...", "", "", "zoom", "• ", "|", "", ""],
            IconPreset::Unicode => ["•", "×", "…", "", "", "zoom", "• ", "│", "", ""],
            IconPreset::NerdFont => [
                "󰄬", "󰅖", "…", "󰉋", "󰓩", "󰍉", "• ", "│", "\u{e0b6}", "\u{e0b4}",
            ],
        };
        IconSet {
            current: self.current.clone().unwrap_or_else(|| defaults[0].into()),
            closing: self.closing.clone().unwrap_or_else(|| defaults[1].into()),
            overflow: self.overflow.clone().unwrap_or_else(|| defaults[2].into()),
            workspace: self.workspace.clone().unwrap_or_else(|| defaults[3].into()),
            tab: self.tab.clone().unwrap_or_else(|| defaults[4].into()),
            zoom: self.zoom.clone().unwrap_or_else(|| defaults[5].into()),
            notification: self
                .notification
                .clone()
                .unwrap_or_else(|| defaults[6].into()),
            vertical_divider: self
                .vertical_divider
                .clone()
                .unwrap_or_else(|| defaults[7].into()),
            pill_left: self.pill_left.clone().unwrap_or_else(|| defaults[8].into()),
            pill_right: self
                .pill_right
                .clone()
                .unwrap_or_else(|| defaults[9].into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SpinnerConfig {
    pub style: String,
    pub frames: Option<Vec<String>>,
    pub interval: Option<u64>,
}

impl Default for SpinnerConfig {
    fn default() -> Self {
        Self {
            style: "dots".into(),
            frames: None,
            interval: None,
        }
    }
}

impl SpinnerConfig {
    pub fn frame(&self, elapsed_ms: usize) -> &str {
        if let Some(frames) = &self.frames {
            let interval = self.interval.unwrap_or(80) as usize;
            return &frames[(elapsed_ms / interval) % frames.len()];
        }
        let spinner = self.builtin().expect("validated spinner style");
        let interval = self.interval.unwrap_or(spinner.interval_ms) as usize;
        spinner.frames[(elapsed_ms / interval) % spinner.frames.len()]
    }

    pub fn builtin(&self) -> Option<SpinnerStyle> {
        builtin_spinner(&self.style)
    }
}

/// Raw TOML form for a presentation segment. This deliberately mirrors the
/// user-facing syntax; [`SegmentConfig`] is the validated form the renderer
/// receives.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SegmentConfigDto {
    text: Option<String>,
    token: Option<String>,
    component: Option<String>,
    style: Option<SemanticStyle>,
    inverted: bool,
    pill: bool,
    prefix: String,
    suffix: String,
    max_width: Option<u16>,
}

/// Renderer-ready presentation segment. Its shape makes selectors mutually
/// exclusive and makes a pill necessarily inverted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SegmentConfig {
    Text {
        text: String,
        style: Option<SemanticStyle>,
    },
    Token {
        token: String,
        style: Option<SemanticStyle>,
        prefix: String,
        suffix: String,
        max_width: Option<u16>,
        visual: TokenVisual,
    },
    Tabs,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum TokenVisual {
    #[default]
    Plain,
    Inverted,
    Pill,
}

impl SegmentConfig {
    fn text(value: &str) -> Self {
        Self::Text {
            text: value.into(),
            style: None,
        }
    }

    fn token(value: &str) -> Self {
        Self::token_with(value, None, "", "", None, TokenVisual::Plain)
    }

    fn token_with(
        value: &str,
        style: Option<SemanticStyle>,
        prefix: &str,
        suffix: &str,
        max_width: Option<u16>,
        visual: TokenVisual,
    ) -> Self {
        Self::Token {
            token: value.into(),
            style,
            prefix: prefix.into(),
            suffix: suffix.into(),
            max_width,
            visual,
        }
    }

    fn component(value: &str) -> Self {
        match value {
            "tabs" => Self::Tabs,
            _ => unreachable!("default config only uses supported components"),
        }
    }
}

impl SegmentConfigDto {
    fn compile(self) -> std::result::Result<SegmentConfig, String> {
        let selectors = usize::from(self.text.is_some())
            + usize::from(self.token.is_some())
            + usize::from(self.component.is_some());
        if selectors != 1 {
            return Err("must set exactly one of text, token, or component".into());
        }
        for (field, value) in [
            ("text", self.text.as_deref()),
            ("prefix", Some(self.prefix.as_str())),
            ("suffix", Some(self.suffix.as_str())),
        ] {
            if let Some(value) = value {
                validate_text(field, value).map_err(|error| error.to_string())?;
            }
        }
        if let Some(text) = self.text {
            if !self.prefix.is_empty()
                || !self.suffix.is_empty()
                || self.max_width.is_some()
                || self.inverted
                || self.pill
            {
                return Err(
                    "text segments do not accept prefix, suffix, max_width, inverted, or pill"
                        .into(),
                );
            }
            return Ok(SegmentConfig::Text {
                text,
                style: self.style,
            });
        }
        if let Some(token) = self.token {
            let visual = match (self.inverted, self.pill) {
                (false, true) => return Err("pill requires inverted = true".into()),
                (false, false) => TokenVisual::Plain,
                (true, false) => TokenVisual::Inverted,
                (true, true) => TokenVisual::Pill,
            };
            return Ok(SegmentConfig::Token {
                token,
                style: self.style,
                prefix: self.prefix,
                suffix: self.suffix,
                max_width: self.max_width,
                visual,
            });
        }
        if self.component.as_deref() != Some("tabs") {
            return Err("contains unknown or out-of-scope component".into());
        }
        if self.style.is_some()
            || !self.prefix.is_empty()
            || !self.suffix.is_empty()
            || self.max_width.is_some()
            || self.inverted
            || self.pill
        {
            return Err(
                "component does not accept style, prefix, suffix, max_width, inverted, or pill"
                    .into(),
            );
        }
        Ok(SegmentConfig::Tabs)
    }
}

impl<'de> Deserialize<'de> for SegmentConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        SegmentConfigDto::deserialize(deserializer)?
            .compile()
            .map_err(serde::de::Error::custom)
    }
}

fn default_priority() -> u8 {
    100
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct GroupConfig {
    pub segments: Vec<SegmentConfig>,
    pub style: Option<SemanticStyle>,
    #[serde(default = "default_priority")]
    pub priority: u8,
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            segments: Vec::new(),
            style: None,
            priority: default_priority(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ItemFormat {
    pub segments: Vec<SegmentConfig>,
}

impl Default for ItemFormat {
    fn default() -> Self {
        Self {
            segments: vec![
                SegmentConfig::text(" "),
                SegmentConfig::token("tab.index"),
                SegmentConfig::token_with("tab.name", None, " ", "", None, TokenVisual::Plain),
                SegmentConfig::token_with("tab.closing", None, " ", "", None, TokenVisual::Plain),
                SegmentConfig::token_with("tab.activity", None, " ", "", None, TokenVisual::Plain),
                SegmentConfig::text(" "),
            ],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TabBarConfig {
    pub position: TabBarPosition,
    pub left: Vec<GroupConfig>,
    pub center: Vec<GroupConfig>,
    pub right: Vec<GroupConfig>,
    pub item: ItemFormat,
}

impl Default for TabBarConfig {
    fn default() -> Self {
        Self {
            position: TabBarPosition::Top,
            left: vec![GroupConfig {
                segments: vec![SegmentConfig::component("tabs")],
                priority: 100,
                ..GroupConfig::default()
            }],
            center: Vec::new(),
            right: vec![
                GroupConfig {
                    segments: vec![SegmentConfig::token_with(
                        "workspace.name",
                        None,
                        " ",
                        " ",
                        Some(20),
                        TokenVisual::Plain,
                    )],
                    style: Some(SemanticStyle::Muted),
                    priority: 200,
                },
                GroupConfig {
                    segments: vec![SegmentConfig::token_with(
                        "client.zoom",
                        None,
                        "",
                        " ",
                        None,
                        TokenVisual::Plain,
                    )],
                    style: None,
                    priority: 255,
                },
                GroupConfig {
                    segments: vec![SegmentConfig::token("client.help")],
                    style: Some(SemanticStyle::Muted),
                    priority: 0,
                },
                GroupConfig {
                    segments: vec![SegmentConfig::token("fut")],
                    style: Some(SemanticStyle::Muted),
                    priority: 0,
                },
                GroupConfig {
                    segments: vec![SegmentConfig::token_with(
                        "client.waiting",
                        None,
                        " ",
                        " ",
                        None,
                        TokenVisual::Plain,
                    )],
                    style: Some(SemanticStyle::Attention),
                    priority: 255,
                },
            ],
            item: ItemFormat::default(),
        }
    }
}

fn default_sidebar_width() -> u16 {
    28
}

pub(super) const MIN_SIDEBAR_WIDTH: u16 = 4;
pub(super) const MAX_SIDEBAR_WIDTH: u16 = 80;
pub(super) const MINIMIZED_SIDEBAR_WIDTH: u16 = 6;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SidebarRowConfig {
    pub left: Vec<SegmentConfig>,
    pub body: Vec<SegmentConfig>,
    pub right: Vec<SegmentConfig>,
    pub detail: Vec<SegmentConfig>,
}

impl Default for SidebarRowConfig {
    fn default() -> Self {
        Self {
            left: vec![SegmentConfig::text(" ")],
            body: vec![
                SegmentConfig::token("workspace.index"),
                SegmentConfig::token_with(
                    "workspace.name",
                    None,
                    " ",
                    "",
                    None,
                    TokenVisual::Plain,
                ),
            ],
            right: vec![
                SegmentConfig::token_with(
                    "workspace.activity",
                    None,
                    " ",
                    "",
                    None,
                    TokenVisual::Plain,
                ),
                SegmentConfig::token_with(
                    "workspace.closing",
                    None,
                    " ",
                    "",
                    None,
                    TokenVisual::Plain,
                ),
                SegmentConfig::text(" "),
            ],
            detail: vec![
                SegmentConfig::text("    "),
                SegmentConfig::token_with(
                    "workspace.git_branch",
                    Some(SemanticStyle::Muted),
                    "",
                    "",
                    None,
                    TokenVisual::Plain,
                ),
                SegmentConfig::token_with(
                    "workspace.git_added",
                    None,
                    " ",
                    "",
                    None,
                    TokenVisual::Plain,
                ),
                SegmentConfig::token_with(
                    "workspace.git_deleted",
                    None,
                    " ",
                    "",
                    None,
                    TokenVisual::Plain,
                ),
            ],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SidebarSlotConfig {
    pub width: u16,
    pub display: SidebarDisplay,
    pub visibility: SidebarVisibility,
    pub components: Vec<SidebarComponentConfig>,
}

fn default_sidebar_footer() -> Vec<SegmentConfig> {
    vec![SegmentConfig::token_with(
        "sidebar.status",
        Some(SemanticStyle::Muted),
        "",
        "",
        None,
        TokenVisual::Plain,
    )]
}

fn default_sidebar_header() -> Vec<SegmentConfig> {
    vec![SegmentConfig::token("session.name")]
}

impl Default for SidebarSlotConfig {
    fn default() -> Self {
        default_left_sidebar()
    }
}

fn default_left_sidebar() -> SidebarSlotConfig {
    SidebarSlotConfig {
        width: default_sidebar_width(),
        display: SidebarDisplay::Expanded,
        visibility: SidebarVisibility::Automatic,
        components: vec![SidebarComponentConfig::Workspaces {
            size: SidebarComponentSize::Fill,
            header: default_sidebar_header(),
            footer: default_sidebar_footer(),
            row: SidebarRowConfig::default(),
        }],
    }
}

fn default_right_sidebar() -> SidebarSlotConfig {
    SidebarSlotConfig {
        width: default_sidebar_width(),
        display: SidebarDisplay::Expanded,
        visibility: SidebarVisibility::Automatic,
        components: vec![SidebarComponentConfig::Agents {
            size: SidebarComponentSize::Fill,
            scope: AgentScope::Session,
        }],
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SidebarSlotOverrides {
    width: Option<u16>,
    display: Option<SidebarDisplay>,
    visibility: Option<SidebarVisibility>,
    components: Option<Vec<SidebarComponentConfig>>,
}

impl SidebarSlotOverrides {
    fn apply(self, slot: &mut SidebarSlotConfig) {
        if let Some(width) = self.width {
            slot.width = width;
        }
        if let Some(display) = self.display {
            slot.display = display;
        }
        if let Some(visibility) = self.visibility {
            slot.visibility = visibility;
        }
        if let Some(components) = self.components {
            slot.components = components;
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SidebarConfigOverrides {
    left: Option<SidebarSlotOverrides>,
    right: Option<SidebarSlotOverrides>,
}

impl<'de> Deserialize<'de> for SidebarConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let overrides = SidebarConfigOverrides::deserialize(deserializer)?;
        let mut config = Self::default();
        if let Some(left) = overrides.left {
            left.apply(&mut config.left);
        }
        if let Some(right) = overrides.right {
            right.apply(&mut config.right);
        }
        Ok(config)
    }
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            left: default_left_sidebar(),
            right: default_right_sidebar(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarComponentSize {
    Fixed(u16),
    Fill,
}

impl<'de> Deserialize<'de> for SidebarComponentSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Value {
            Fixed(u16),
            Fill(String),
        }

        match Value::deserialize(deserializer)? {
            Value::Fixed(rows) => Ok(Self::Fixed(rows)),
            Value::Fill(value) if value == "fill" => Ok(Self::Fill),
            Value::Fill(_) => Err(serde::de::Error::custom(
                "component size must be a row count or 'fill'",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AgentScope {
    Tab,
    Workspace,
    #[default]
    Session,
    Global,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(tag = "component", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum SidebarComponentConfig {
    Workspaces {
        #[serde(default = "default_workspace_component_size")]
        size: SidebarComponentSize,
        #[serde(default = "default_sidebar_header")]
        header: Vec<SegmentConfig>,
        #[serde(default = "default_sidebar_footer")]
        footer: Vec<SegmentConfig>,
        #[serde(default)]
        row: SidebarRowConfig,
    },
    Agents {
        #[serde(default = "default_agents_component_size")]
        size: SidebarComponentSize,
        #[serde(default)]
        scope: AgentScope,
    },
}

impl SidebarComponentConfig {
    pub(super) fn size(&self) -> SidebarComponentSize {
        match self {
            Self::Workspaces { size, .. } | Self::Agents { size, .. } => *size,
        }
    }

    pub(super) fn uses_default_workspace_footer(&self) -> bool {
        matches!(self, Self::Workspaces { footer, .. } if *footer == default_sidebar_footer())
    }
}

#[derive(Clone, Copy)]
pub(super) struct WorkspaceComponentConfigRef<'a> {
    pub header: &'a [SegmentConfig],
    pub footer: &'a [SegmentConfig],
    pub row: &'a SidebarRowConfig,
    pub uses_default_footer: bool,
}

impl SidebarSlotConfig {
    pub(super) fn workspaces(&self) -> Option<WorkspaceComponentConfigRef<'_>> {
        self.components.iter().find_map(|component| {
            let SidebarComponentConfig::Workspaces {
                header,
                footer,
                row,
                ..
            } = component
            else {
                return None;
            };
            Some(WorkspaceComponentConfigRef {
                header,
                footer,
                row,
                uses_default_footer: component.uses_default_workspace_footer(),
            })
        })
    }
}

fn default_workspace_component_size() -> SidebarComponentSize {
    SidebarComponentSize::Fill
}

fn default_agents_component_size() -> SidebarComponentSize {
    SidebarComponentSize::Fill
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SidebarConfig {
    pub left: SidebarSlotConfig,
    pub right: SidebarSlotConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct UiConfig {
    pub(super) pane_layout: PaneLayoutPolicy,
    pub(super) confirm_close: bool,
    prefix: String,
    pub(super) bindings: BindingsConfig,
    pub(super) icons: IconsConfig,
    pub(super) spinner: SpinnerConfig,
    pub(super) styles: StylesConfig,
    pub(super) tab_bar: TabBarConfig,
    pub(super) sidebar: SidebarConfig,
    #[serde(skip)]
    pub(super) alerts: AlertsConfig,
    #[serde(skip)]
    pub(super) extensions: Vec<Extension>,
    #[serde(skip)]
    extension_config: ExtensionConfigCatalog,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            pane_layout: PaneLayoutPolicy::Splits,
            confirm_close: true,
            prefix: "ctrl-b".into(),
            bindings: BindingsConfig::default(),
            icons: IconsConfig::default(),
            spinner: SpinnerConfig::default(),
            styles: StylesConfig::default(),
            tab_bar: TabBarConfig::default(),
            sidebar: SidebarConfig::default(),
            alerts: AlertsConfig::default(),
            extensions: Vec::new(),
            extension_config: ExtensionConfigCatalog::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AlertsConfig {
    pub(super) signal_outer_terminal: bool,
}

impl UiConfig {
    pub(crate) fn icon_preset_name(&self) -> &'static str {
        match self.icons.preset {
            IconPreset::Ascii => "ascii",
            IconPreset::Unicode => "unicode",
            IconPreset::NerdFont => "nerd_font",
        }
    }

    pub(crate) fn icon_probe(&self) -> Vec<String> {
        let icons = self.icons.resolve();
        vec![
            icons.current,
            icons.closing,
            icons.overflow,
            icons.workspace,
            icons.tab,
            icons.zoom,
            icons.notification,
            icons.vertical_divider,
            icons.pill_left,
            icons.pill_right,
        ]
        .into_iter()
        .filter(|icon| !icon.is_empty())
        .collect()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    ui: UiConfig,
    alerts: AlertsConfig,
    trusted_commands: BTreeMap<String, PaletteCommand>,
    extension_commands: BTreeMap<String, ExtensionCommandConfig>,
    extensions: Vec<PathBuf>,
    projects: BTreeMap<String, ProjectConfig>,
    #[serde(deserialize_with = "deserialize_extension_config_catalog")]
    extension: BTreeMap<String, ExtensionConfigTable>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectConfig {
    pub(crate) path: PathBuf,
    pub(crate) recipe: Option<PathBuf>,
}

impl ProjectConfig {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn recipe(&self) -> Option<&Path> {
        self.recipe.as_deref()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectCatalog {
    projects: BTreeMap<String, ProjectConfig>,
}

impl ProjectCatalog {
    #[cfg(test)]
    pub(crate) fn from_projects(projects: BTreeMap<String, ProjectConfig>) -> Self {
        Self { projects }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&ProjectConfig> {
        self.projects.get(name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &ProjectConfig)> {
        self.projects
            .iter()
            .map(|(name, project)| (name.as_str(), project))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExtensionConfigCatalog {
    defaults: BTreeMap<String, ExtensionConfigTable>,
    source: Option<PathBuf>,
}

pub(crate) struct ExtensionConfigFingerprintData<'a> {
    pub(crate) normalized_defaults: String,
    pub(crate) source: Option<&'a Path>,
}

impl ExtensionConfigCatalog {
    /// The behavior-affecting global config data used to identify an extension
    /// registry. Object keys are sorted recursively so equivalent TOML input
    /// has the same representation regardless of declaration order.
    pub(crate) fn fingerprint_data(&self) -> ExtensionConfigFingerprintData<'_> {
        ExtensionConfigFingerprintData {
            normalized_defaults: serde_json::to_string(&self.defaults)
                .expect("extension config defaults always serialize as JSON"),
            source: self.source.as_deref(),
        }
    }

    pub(crate) fn to_protocol(&self) -> crate::protocol::ExtensionCatalogConfig {
        crate::protocol::ExtensionCatalogConfig {
            defaults: self.defaults.clone(),
            source: self.source.clone(),
        }
    }

    pub(crate) fn from_protocol(
        config: crate::protocol::ExtensionCatalogConfig,
        extensions: &[Extension],
    ) -> Result<Self> {
        let source = config
            .source
            .as_deref()
            .unwrap_or_else(|| Path::new("daemon extension catalog"));
        validate_extension_config_catalog(&config.defaults, extensions, source)?;
        Ok(Self {
            defaults: config.defaults,
            source: config.source,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedExtensionConfig {
    pub json: String,
    pub trusted_json: String,
    pub global_source: Option<PathBuf>,
    pub project_source: Option<PathBuf>,
    pub workspace_source: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct ResolvedHookExtensionConfig {
    pub config: ResolvedExtensionConfig,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigLocation {
    pub path: Option<PathBuf>,
    pub explicit: bool,
    pub source: &'static str,
}

impl ConfigLocation {
    pub fn disabled() -> Self {
        Self {
            path: None,
            explicit: false,
            source: "--no-config",
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.source == "--no-config"
    }
}

#[derive(Debug)]
pub(crate) struct LoadedConfig {
    pub ui: UiConfig,
    pub extensions: Vec<Extension>,
    pub present: bool,
}

#[derive(Debug)]
pub(crate) struct LoadedExtensionConfig {
    pub extensions: Vec<Extension>,
    pub config: ExtensionConfigCatalog,
}

/// Parsed local UI input waiting to be materialized against one complete
/// daemon catalog. It deliberately contains no manifest-derived state.
#[derive(Clone, Debug)]
pub(crate) struct StagedUiConfig {
    config: Config,
    source: Option<PathBuf>,
}

impl StagedUiConfig {
    pub(crate) fn materialize(
        &self,
        catalog: &crate::protocol::ExtensionCatalog,
    ) -> Result<UiConfig> {
        let registry = extensions::ExtensionRegistry::from_catalog(catalog.clone())?;
        materialize_config(
            self.config.clone(),
            registry.extensions().to_vec(),
            registry.config().clone(),
            self.source.as_deref(),
        )
    }
}

pub fn resolve_location(config_dir: Option<&std::path::Path>) -> Result<ConfigLocation> {
    if let Some(directory) = config_dir {
        if !directory.is_absolute() {
            bail!("--config-dir must be an absolute path");
        }
        return Ok(ConfigLocation {
            path: Some(directory.join("config.toml")),
            explicit: false,
            source: "--config-dir",
        });
    }
    if let Some(path) = env::var_os("FUT_CONFIG") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            bail!("FUT_CONFIG must be an absolute path");
        }
        return Ok(ConfigLocation {
            path: Some(path),
            explicit: true,
            source: "FUT_CONFIG",
        });
    }
    if let Some(directory) = env::var_os("XDG_CONFIG_HOME") {
        let directory = PathBuf::from(directory);
        if directory.is_absolute() {
            return Ok(ConfigLocation {
                path: Some(directory.join("fut/config.toml")),
                explicit: false,
                source: "XDG_CONFIG_HOME",
            });
        }
    }
    let Some(home) = env::var_os("HOME") else {
        return Ok(ConfigLocation {
            path: None,
            explicit: false,
            source: "defaults",
        });
    };
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        bail!("HOME must be an absolute path when resolving Fut config");
    }
    Ok(ConfigLocation {
        path: Some(home.join(".config/fut/config.toml")),
        explicit: false,
        source: "HOME",
    })
}

pub(crate) fn stage_location(location: &ConfigLocation) -> Result<StagedUiConfig> {
    let Some(path) = &location.path else {
        return Ok(StagedUiConfig {
            config: Config::default(),
            source: None,
        });
    };
    let Some(source) = read_config_source(path, location.explicit)? else {
        return Ok(StagedUiConfig {
            config: Config::default(),
            source: None,
        });
    };
    let config = toml::from_str::<Config>(&source)
        .with_context(|| format!("parse Fut config {}", path.display()))?;
    Ok(StagedUiConfig {
        config,
        source: Some(path.clone()),
    })
}

pub(crate) fn load_location(location: &ConfigLocation) -> Result<LoadedConfig> {
    location
        .path
        .as_ref()
        .map_or_else(load_default_outcome, |path| {
            load_path_outcome(path, location.explicit)
        })
}

/// Load the durable project catalog without making control commands depend on
/// unrelated presentation configuration. Project entries are still strict and
/// bounded by the same global configuration file reader.
pub(crate) fn load_projects_location(location: &ConfigLocation) -> Result<ProjectCatalog> {
    let Some(path) = &location.path else {
        return Ok(ProjectCatalog::default());
    };
    let Some(source) = read_config_source(path, location.explicit)? else {
        return Ok(ProjectCatalog::default());
    };

    #[derive(Default, Deserialize)]
    struct ProjectsConfig {
        #[serde(default)]
        projects: BTreeMap<String, ProjectConfig>,
    }

    let mut config = toml::from_str::<ProjectsConfig>(&source)
        .with_context(|| format!("parse project catalog from {}", path.display()))?;
    validate_projects(&mut config.projects, path)?;
    Ok(ProjectCatalog {
        projects: config.projects,
    })
}

/// Load only the daemon-owned extension declarations and namespaced config.
/// Client presentation mistakes must not prevent the daemon from starting; the
/// interactive client validates the complete configuration independently before
/// changing terminal state.
pub(crate) fn load_extensions_location(location: &ConfigLocation) -> Result<LoadedExtensionConfig> {
    #[derive(Default, Deserialize)]
    struct ExtensionConfig {
        #[serde(default)]
        extensions: Vec<PathBuf>,
        #[serde(default, deserialize_with = "deserialize_extension_config_catalog")]
        extension: BTreeMap<String, ExtensionConfigTable>,
    }

    let (config, source_path) = match &location.path {
        Some(path) => match read_config_source(path, location.explicit)? {
            Some(source) => (
                toml::from_str::<ExtensionConfig>(&source).with_context(|| {
                    format!("parse daemon extension config from {}", path.display())
                })?,
                Some(path.as_path()),
            ),
            None => (ExtensionConfig::default(), None),
        },
        None => (ExtensionConfig::default(), None),
    };
    let roots = merged_extension_roots(&config.extensions)?;
    let loaded_extensions = extensions::load(&roots).with_context(|| {
        source_path.map_or_else(
            || "load managed extensions".to_owned(),
            |path| {
                format!(
                    "load extensions from Fut config {} and managed store",
                    path.display()
                )
            },
        )
    })?;
    let validation_source = source_path.unwrap_or_else(|| Path::new("default Fut config"));
    validate_extension_config_catalog(&config.extension, &loaded_extensions, validation_source)?;
    Ok(LoadedExtensionConfig {
        extensions: loaded_extensions,
        config: ExtensionConfigCatalog {
            source: (!config.extension.is_empty()).then(|| validation_source.to_owned()),
            defaults: config.extension,
        },
    })
}

#[cfg(test)]
fn load_path(path: &std::path::Path, explicit: bool) -> Result<UiConfig> {
    Ok(load_path_outcome(path, explicit)?.ui)
}

fn load_path_outcome(path: &std::path::Path, explicit: bool) -> Result<LoadedConfig> {
    let source = read_config_source(path, explicit)?;
    let present = source.is_some();
    let config = source.map_or_else(
        || Ok(Config::default()),
        |source| {
            toml::from_str::<Config>(&source)
                .with_context(|| format!("parse Fut config {}", path.display()))
        },
    )?;
    let roots = merged_extension_roots(&config.extensions)?;
    let loaded_extensions = extensions::load(&roots)
        .with_context(|| format!("load extensions from Fut config {}", path.display()))?;
    validate_extension_config_catalog(&config.extension, &loaded_extensions, path)?;
    let extension_config = ExtensionConfigCatalog {
        source: (!config.extension.is_empty()).then(|| path.to_owned()),
        defaults: config.extension.clone(),
    };
    let ui = materialize_config(
        config,
        loaded_extensions.clone(),
        extension_config,
        present.then_some(path),
    )?;
    Ok(LoadedConfig {
        ui,
        extensions: loaded_extensions,
        present,
    })
}

fn load_default_outcome() -> Result<LoadedConfig> {
    let roots = merged_extension_roots(&[])?;
    let loaded_extensions = extensions::load(&roots).context("load managed extensions")?;
    let ui = materialize_config(
        Config::default(),
        loaded_extensions.clone(),
        ExtensionConfigCatalog::default(),
        None,
    )?;
    Ok(LoadedConfig {
        ui,
        extensions: loaded_extensions,
        present: false,
    })
}

fn merged_extension_roots(explicit: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let managed = extension_store::enabled_roots().context("load managed extension store")?;
    let mut roots = Vec::with_capacity(explicit.len() + managed.len());
    roots.extend_from_slice(explicit);
    roots.extend(managed);
    Ok(roots)
}

fn materialize_config(
    mut config: Config,
    extensions: Vec<Extension>,
    extension_config: ExtensionConfigCatalog,
    source: Option<&Path>,
) -> Result<UiConfig> {
    config.ui.alerts = config.alerts;
    let prefix = parse_key(&config.ui.prefix)
        .map(|(bytes, _)| bytes)
        .context("ui.prefix must be one character or a named key such as ctrl-a")?;
    config.ui.bindings.set_prefix(prefix);
    config.ui.bindings.commands = config.trusted_commands.into_values().collect();
    for extension in &extensions {
        for launcher in extension.commands() {
            let argv = launcher.argv();
            let identity = ExtensionCommandIdentity {
                id: extension.id().to_owned(),
                root: extension.root().to_owned(),
                command: launcher.name().to_owned(),
            };
            let slug = identity.slug();
            let binding = config.ui.bindings.values.remove(&slug);
            let configured_args = config.extension_commands.remove(&slug);
            config.ui.bindings.commands.push(PaletteCommand {
                title: launcher.title().to_owned(),
                binding,
                program: PathBuf::from(&argv[0]),
                args: configured_args.map_or_else(
                    || {
                        argv[1..]
                            .iter()
                            .map(|value| {
                                value
                                    .to_str()
                                    .expect("extension manifest arguments are UTF-8")
                                    .to_owned()
                            })
                            .collect()
                    },
                    |configured| configured.args,
                ),
                execution: launcher.execution().clone(),
                extension: Some(identity),
                fields: launcher.fields().to_vec(),
            });
        }
    }
    if let Some(slug) = config.extension_commands.keys().next() {
        bail!("unknown extension_commands command {slug:?}");
    }
    let source = source.unwrap_or_else(|| Path::new("default Fut config"));
    validate_projects(&mut config.projects, source)?;
    validate(&config.ui, &extensions)
        .with_context(|| format!("validate Fut config {}", source.display()))?;
    config.ui.extensions = extensions;
    config.ui.extension_config = extension_config;
    Ok(config.ui)
}

fn validate_projects(projects: &mut BTreeMap<String, ProjectConfig>, source: &Path) -> Result<()> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut paths = BTreeMap::<PathBuf, String>::new();
    for (name, project) in projects {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!(
                "invalid project name {name:?} in {}; use 1-64 ASCII letters, numbers, '-' or '_'",
                source.display()
            );
        }
        project.path = expand_project_path(&project.path, home.as_deref())?;
        if !project.path.is_absolute() {
            bail!(
                "project {name:?} path in {} must be absolute or start with ~/",
                source.display()
            );
        }
        if let Some(previous) = paths.insert(project.path.clone(), name.clone()) {
            bail!(
                "projects {previous:?} and {name:?} use the same path {}",
                project.path.display()
            );
        }
        if let Some(recipe) = &mut project.recipe {
            *recipe = expand_project_path(recipe, home.as_deref())?;
            if !recipe.is_absolute() {
                bail!(
                    "project {name:?} recipe in {} must be absolute or start with ~/",
                    source.display()
                );
            }
        }
    }
    Ok(())
}

fn expand_project_path(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let value = path.as_os_str().to_string_lossy();
    if value == "~" {
        return home
            .filter(|home| home.is_absolute())
            .map(Path::to_path_buf)
            .context("HOME must be absolute to expand a project path beginning with ~");
    }
    if let Some(relative) = value.strip_prefix("~/") {
        let home = home
            .filter(|home| home.is_absolute())
            .context("HOME must be absolute to expand a project path beginning with ~/")?;
        return Ok(home.join(relative));
    }
    if value.starts_with('~') {
        bail!("project paths support only ~ or ~/ expansion");
    }
    Ok(path.to_owned())
}

fn read_config_source(path: &std::path::Path, explicit: bool) -> Result<Option<String>> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !explicit => {
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read Fut config {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect Fut config {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("Fut config {} is not a regular file", path.display());
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        bail!(
            "Fut config {} is {} bytes; maximum is {MAX_CONFIG_BYTES}",
            path.display(),
            metadata.len()
        );
    }
    let mut source = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut source)
        .with_context(|| format!("read Fut config {}", path.display()))?;
    if source.len() as u64 > MAX_CONFIG_BYTES {
        bail!(
            "Fut config {} exceeds the {MAX_CONFIG_BYTES}-byte maximum",
            path.display()
        );
    }
    Ok(Some(source))
}

pub(super) fn resolve_extension_config(
    ui: &UiConfig,
    extension_id: &str,
    workspace_root: &Path,
    project: Option<&TrustedProjectConfig>,
) -> Result<ResolvedExtensionConfig> {
    resolve_extension_config_for_catalog(
        &ui.extension_config,
        &ui.extensions,
        extension_id,
        workspace_root,
        project,
    )
}

pub(crate) fn resolve_extension_config_for_catalog(
    catalog: &ExtensionConfigCatalog,
    extensions: &[Extension],
    extension_id: &str,
    workspace_root: &Path,
    project: Option<&TrustedProjectConfig>,
) -> Result<ResolvedExtensionConfig> {
    resolve_extension_config_sources(
        catalog,
        extensions,
        extension_id,
        Some(workspace_root),
        project,
    )
}

pub(crate) fn resolve_hook_extension_config_for_catalog(
    catalog: &ExtensionConfigCatalog,
    extensions: &[Extension],
    extension_id: &str,
    workspace_root: &Path,
    project: Option<&TrustedProjectConfig>,
) -> ResolvedHookExtensionConfig {
    match resolve_extension_config_sources(
        catalog,
        extensions,
        extension_id,
        Some(workspace_root),
        project,
    ) {
        Ok(config) => ResolvedHookExtensionConfig {
            config,
            warning: None,
        },
        Err(workspace_error) => {
            match resolve_extension_config_sources(catalog, extensions, extension_id, None, project)
            {
                Ok(config) => ResolvedHookExtensionConfig {
                    config,
                    warning: Some(format!(
                        "workspace extension config rejected: {workspace_error:#}; using global-only config"
                    )),
                },
                Err(global_error) => ResolvedHookExtensionConfig {
                    config: ResolvedExtensionConfig {
                        json: "{}".to_owned(),
                        trusted_json: "{}".to_owned(),
                        global_source: None,
                        project_source: None,
                        workspace_source: None,
                    },
                    warning: Some(format!(
                        "workspace extension config rejected: {workspace_error:#}; global extension config unavailable: {global_error:#}; using empty config"
                    )),
                },
            }
        }
    }
}

fn resolve_extension_config_sources(
    catalog: &ExtensionConfigCatalog,
    extensions: &[Extension],
    extension_id: &str,
    workspace_root: Option<&Path>,
    project: Option<&TrustedProjectConfig>,
) -> Result<ResolvedExtensionConfig> {
    let mut resolved = catalog
        .defaults
        .get(extension_id)
        .cloned()
        .unwrap_or_default();
    let global_source = catalog
        .defaults
        .contains_key(extension_id)
        .then(|| catalog.source.clone())
        .flatten();
    let project_source = project.and_then(|project| {
        project
            .extension
            .contains_key(extension_id)
            .then(|| project.source.clone())
    });
    if let Some(overrides) = project.and_then(|project| project.extension.get(extension_id)) {
        merge_extension_tables(&mut resolved, overrides.clone());
    }
    let trusted_json = serde_json::to_string(&serde_json::Value::Object(resolved.clone()))?;
    let mut local_source = None;

    if let Some(workspace_root) = workspace_root {
        let workspace_path = workspace_root.join(".fut/config.toml");
        if let Some(source) = read_config_source(&workspace_path, false)? {
            #[derive(Default, Deserialize)]
            #[serde(default, deny_unknown_fields)]
            struct WorkspaceConfig {
                #[serde(deserialize_with = "deserialize_extension_config_catalog")]
                extension: BTreeMap<String, ExtensionConfigTable>,
            }

            let workspace = toml::from_str::<WorkspaceConfig>(&source).with_context(|| {
                format!("parse workspace Fut config {}", workspace_path.display())
            })?;
            validate_extension_config_catalog(&workspace.extension, extensions, &workspace_path)?;
            if let Some(overrides) = workspace.extension.get(extension_id) {
                merge_extension_tables(&mut resolved, overrides.clone());
                local_source = Some(workspace_path);
            }
        }
    }

    validate_extension_config_table(extension_id, &resolved, Path::new("resolved config"))?;
    let json = serde_json::to_string(&serde_json::Value::Object(resolved))?;
    if json.len() > MAX_EXTENSION_CONFIG_SERIALIZED_BYTES {
        bail!(
            "resolved extension.{extension_id} config is {} bytes; maximum is {MAX_EXTENSION_CONFIG_SERIALIZED_BYTES}",
            json.len()
        );
    }
    Ok(ResolvedExtensionConfig {
        json,
        trusted_json,
        global_source,
        project_source,
        workspace_source: local_source,
    })
}

pub(crate) fn deserialize_extension_config_catalog<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, ExtensionConfigTable>, D::Error>
where
    D: Deserializer<'de>,
{
    BTreeMap::<String, toml::Table>::deserialize(deserializer)?
        .into_iter()
        .map(|(id, table)| {
            convert_toml_config_value(toml::Value::Table(table))
                .and_then(|value| {
                    value
                        .as_object()
                        .cloned()
                        .context("extension config must be a table")
                })
                .map(|table| (id, table))
                .map_err(serde::de::Error::custom)
        })
        .collect()
}

fn convert_toml_config_value(value: toml::Value) -> Result<serde_json::Value> {
    Ok(match value {
        toml::Value::String(value) => serde_json::Value::String(value),
        toml::Value::Integer(value) => serde_json::Value::Number(value.into()),
        toml::Value::Float(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .context("extension config floats must be finite")?,
        toml::Value::Boolean(value) => serde_json::Value::Bool(value),
        toml::Value::Datetime(value) => serde_json::Value::String(value.to_string()),
        toml::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(convert_toml_config_value)
                .collect::<Result<_>>()?,
        ),
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .into_iter()
                .map(|(key, value)| convert_toml_config_value(value).map(|value| (key, value)))
                .collect::<Result<_>>()?,
        ),
    })
}

pub(crate) fn validate_extension_config_catalog(
    catalog: &BTreeMap<String, ExtensionConfigTable>,
    extensions: &[Extension],
    source: &Path,
) -> Result<()> {
    for (id, table) in catalog {
        if !extensions.iter().any(|extension| extension.id() == id) {
            bail!(
                "unknown extension ID {id:?} in {} (no configured extension declares it)",
                source.display()
            );
        }
        validate_extension_config_table(id, table, source)?;
    }
    Ok(())
}

fn validate_extension_config_table(
    id: &str,
    table: &ExtensionConfigTable,
    source: &Path,
) -> Result<()> {
    let mut keys = 0;
    validate_extension_config_value(
        &serde_json::Value::Object(table.clone()),
        0,
        &mut keys,
        &format!("extension.{id}"),
        source,
    )?;
    let serialized = serde_json::to_vec(&serde_json::Value::Object(table.clone()))?;
    if serialized.len() > MAX_EXTENSION_CONFIG_SERIALIZED_BYTES {
        bail!(
            "extension.{id} in {} serializes to {} bytes; maximum is {MAX_EXTENSION_CONFIG_SERIALIZED_BYTES}",
            source.display(),
            serialized.len()
        );
    }
    Ok(())
}

fn validate_extension_config_value(
    value: &serde_json::Value,
    depth: usize,
    keys: &mut usize,
    key_path: &str,
    source: &Path,
) -> Result<()> {
    if depth > MAX_EXTENSION_CONFIG_DEPTH {
        bail!(
            "{key_path} in {} exceeds the maximum extension config depth of {MAX_EXTENSION_CONFIG_DEPTH}",
            source.display()
        );
    }
    match value {
        serde_json::Value::Object(table) => {
            for (key, child) in table {
                *keys += 1;
                if *keys > MAX_EXTENSION_CONFIG_KEYS {
                    bail!(
                        "extension config in {} has more than {MAX_EXTENSION_CONFIG_KEYS} keys",
                        source.display()
                    );
                }
                if key.is_empty()
                    || key.len() > MAX_EXTENSION_CONFIG_KEY_BYTES
                    || key.chars().any(char::is_control)
                {
                    bail!(
                        "extension config key {key:?} in {} must be non-empty, control-free, and at most {MAX_EXTENSION_CONFIG_KEY_BYTES} bytes",
                        source.display()
                    );
                }
                validate_extension_config_value(
                    child,
                    depth + 1,
                    keys,
                    &format!("{key_path}.{key}"),
                    source,
                )?;
            }
        }
        serde_json::Value::Array(values) => {
            if values.len() > MAX_EXTENSION_CONFIG_ARRAY_VALUES {
                bail!(
                    "{key_path} in {} has {} array values; maximum is {MAX_EXTENSION_CONFIG_ARRAY_VALUES}",
                    source.display(),
                    values.len()
                );
            }
            for value in values {
                validate_extension_config_value(value, depth + 1, keys, key_path, source)?;
            }
        }
        scalar => {
            let serialized = serde_json::to_vec(scalar)?;
            if serialized.len() > MAX_EXTENSION_CONFIG_VALUE_BYTES {
                bail!(
                    "{key_path} in {} is {} serialized bytes; maximum per value is {MAX_EXTENSION_CONFIG_VALUE_BYTES}",
                    source.display(),
                    serialized.len()
                );
            }
        }
    }
    Ok(())
}

fn merge_extension_tables(base: &mut ExtensionConfigTable, overrides: ExtensionConfigTable) {
    for (key, override_value) in overrides {
        match (base.get_mut(&key), override_value) {
            (
                Some(serde_json::Value::Object(base_table)),
                serde_json::Value::Object(override_table),
            ) => {
                merge_extension_tables(base_table, override_table);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn validate(ui: &UiConfig, extensions: &[Extension]) -> Result<()> {
    if ui.spinner.frames.is_none() && ui.spinner.builtin().is_none() {
        bail!("unknown ui.spinner.style {:?}", ui.spinner.style);
    }
    if let Some(interval) = ui.spinner.interval
        && !(16..=2_000).contains(&interval)
    {
        bail!("ui.spinner.interval must be between 16 and 2000 milliseconds");
    }
    if let Some(frames) = &ui.spinner.frames {
        if frames.is_empty() || frames.len() > MAX_SPINNER_FRAMES {
            bail!("ui.spinner.frames must contain between 1 and {MAX_SPINNER_FRAMES} frames");
        }
        let mut width = None;
        for frame in frames {
            validate_text("ui.spinner.frames", frame)?;
            let frame_width = UnicodeWidthStr::width(frame.as_str());
            if frame_width == 0 || frame_width > MAX_SPINNER_WIDTH {
                bail!("ui.spinner frames must be between 1 and {MAX_SPINNER_WIDTH} cells wide");
            }
            if width
                .replace(frame_width)
                .is_some_and(|width| width != frame_width)
            {
                bail!("ui.spinner frames must all have the same display width");
            }
        }
    }
    let valid_binding_keys = ALL_ACTIONS
        .into_iter()
        .map(config_key)
        .collect::<HashSet<_>>();
    let mut bound_suffixes = HashSet::new();
    for (key, value) in &ui.bindings.values {
        if !valid_binding_keys.contains(key.as_str()) {
            bail!("unknown ui.bindings action {key:?}");
        }
        if ui.bindings.parse_suffix(value).is_none() {
            bail!(
                "ui.bindings.{key} must be one character, prefix, ctrl-a through ctrl-z, space, enter, tab, esc, up, or down"
            );
        }
    }
    for action in ALL_ACTIONS {
        let Some(suffix) = ui.bindings.suffix(action) else {
            continue;
        };
        if !bound_suffixes.insert(suffix) {
            bail!("ui.bindings must not assign the same key to multiple actions");
        }
    }
    let explicitly_bound = ui
        .bindings
        .values
        .keys()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut command_suffixes = HashSet::new();
    for command in &ui.bindings.commands {
        validate_text("command title", &command.title)?;
        if command.title.is_empty() {
            bail!("command titles must not be empty");
        }
        if let ExtensionCommandExecution::Interactive { size, .. } = command.execution {
            size.validate().context("validate command popup size")?;
        }
        if let Some(binding) = &command.binding {
            let Some((suffix, _)) = ui.bindings.parse_suffix(binding) else {
                bail!("command bindings must be one character or a named key");
            };
            if !command_suffixes.insert(suffix.clone()) {
                bail!("commands must not assign the same key to multiple commands");
            }
            if ALL_ACTIONS.into_iter().any(|action| {
                explicitly_bound.contains(config_key(action))
                    && ui.bindings.suffix(action).as_deref() == Some(&suffix)
            }) {
                bail!("a command conflicts with an explicitly configured ui binding");
            }
        }
    }
    for (side, sidebar) in [("left", &ui.sidebar.left), ("right", &ui.sidebar.right)] {
        if !(MIN_SIDEBAR_WIDTH..=MAX_SIDEBAR_WIDTH).contains(&sidebar.width) {
            bail!("ui.sidebar.{side}.width must be between 4 and 80");
        }
        let fill_count = sidebar
            .components
            .iter()
            .filter(|component| component.size() == SidebarComponentSize::Fill)
            .count();
        if fill_count > 1 {
            bail!("ui.sidebar.{side}.components may contain at most one fill component");
        }
        let workspace_count = sidebar
            .components
            .iter()
            .filter(|component| matches!(component, SidebarComponentConfig::Workspaces { .. }))
            .count();
        if workspace_count > 1 {
            bail!("ui.sidebar.{side}.components may contain at most one workspaces component");
        }
        if sidebar
            .components
            .iter()
            .any(|component| matches!(component.size(), SidebarComponentSize::Fixed(0)))
        {
            bail!("ui.sidebar.{side} component fixed row counts must be greater than zero");
        }
    }
    for (name, value) in [
        ("current", &ui.icons.current),
        ("closing", &ui.icons.closing),
        ("overflow", &ui.icons.overflow),
        ("workspace", &ui.icons.workspace),
        ("tab", &ui.icons.tab),
        ("zoom", &ui.icons.zoom),
        ("notification", &ui.icons.notification),
        ("vertical_divider", &ui.icons.vertical_divider),
        ("pill_left", &ui.icons.pill_left),
        ("pill_right", &ui.icons.pill_right),
    ] {
        if let Some(value) = value {
            validate_text(&format!("ui.icons.{name}"), value)?;
        }
    }
    let divider = ui.icons.resolve().vertical_divider;
    if divider.graphemes(true).count() != 1 || UnicodeWidthStr::width(divider.as_str()) != 1 {
        bail!("ui.icons.vertical_divider must be exactly one grapheme and one display cell");
    }
    let mut tab_components = 0;
    for (lane, groups) in [
        ("left", &ui.tab_bar.left),
        ("center", &ui.tab_bar.center),
        ("right", &ui.tab_bar.right),
    ] {
        if groups.len() > MAX_SEGMENTS {
            bail!("ui.tab_bar.{lane} contains too many groups");
        }
        for (group_index, group) in groups.iter().enumerate() {
            validate_segments(
                &format!("ui.tab_bar.{lane}[{group_index}].segments"),
                &group.segments,
                TokenScope::Bar,
                true,
                &mut tab_components,
                extensions,
            )?;
            if group
                .segments
                .iter()
                .any(|segment| matches!(segment, SegmentConfig::Tabs))
                && group.segments.len() != 1
            {
                bail!(
                    "ui.tab_bar.{lane}[{group_index}] component groups must contain exactly one segment"
                );
            }
        }
    }
    if tab_components > 1 {
        bail!("ui.tab_bar may contain at most one tabs component");
    }
    validate_segments(
        "ui.tab_bar.item.segments",
        &ui.tab_bar.item.segments,
        TokenScope::Tab,
        false,
        &mut 0,
        extensions,
    )?;
    for (side, sidebar) in [("left", &ui.sidebar.left), ("right", &ui.sidebar.right)] {
        for (index, component) in sidebar.components.iter().enumerate() {
            let SidebarComponentConfig::Workspaces {
                header,
                footer,
                row,
                ..
            } = component
            else {
                continue;
            };
            for (name, segments, scope) in [
                ("header", header, TokenScope::Sidebar),
                ("footer", footer, TokenScope::Sidebar),
                ("row.left", &row.left, TokenScope::Workspace),
                ("row.body", &row.body, TokenScope::Workspace),
                ("row.right", &row.right, TokenScope::Workspace),
                ("row.detail", &row.detail, TokenScope::Workspace),
            ] {
                validate_segments(
                    &format!("ui.sidebar.{side}.components[{index}].{name}"),
                    segments,
                    scope,
                    false,
                    &mut 0,
                    extensions,
                )?;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum TokenScope {
    Bar,
    Tab,
    Workspace,
    Sidebar,
}

fn validate_segments(
    path: &str,
    segments: &[SegmentConfig],
    scope: TokenScope,
    components: bool,
    component_count: &mut usize,
    extensions: &[Extension],
) -> Result<()> {
    if segments.len() > MAX_SEGMENTS {
        bail!("{path} contains too many segments");
    }
    for (index, segment) in segments.iter().enumerate() {
        let path = format!("{path}[{index}]");
        match segment {
            SegmentConfig::Text { .. } => {}
            SegmentConfig::Token { token, .. } if !token_allowed(scope, token, extensions) => {
                bail!("{path} contains unknown or out-of-scope token {token:?}");
            }
            SegmentConfig::Token { .. } => {}
            SegmentConfig::Tabs if !components => {
                bail!("{path} contains unknown or out-of-scope component \"tabs\"");
            }
            SegmentConfig::Tabs => *component_count += 1,
        }
    }
    Ok(())
}

fn token_allowed(scope: TokenScope, token: &str, extensions: &[Extension]) -> bool {
    let builtin = match scope {
        TokenScope::Bar => matches!(
            token,
            "fut"
                | "session.name"
                | "workspace.name"
                | "workspace.icon"
                | "tab.name"
                | "tab.index"
                | "tab.pane_count"
                | "client.zoom"
                | "client.help"
                | "client.waiting"
                | "session.waiting"
        ),
        TokenScope::Tab => matches!(
            token,
            "tab.marker"
                | "tab.index"
                | "tab.name"
                | "tab.id"
                | "tab.closing"
                | "tab.pane_count"
                | "tab.icon"
                | "tab.activity"
        ),
        TokenScope::Workspace => matches!(
            token,
            "workspace.index"
                | "workspace.name"
                | "workspace.id"
                | "workspace.root"
                | "workspace.root_name"
                | "workspace.closing"
                | "workspace.tab_count"
                | "workspace.icon"
                | "workspace.activity"
                | "workspace.git_branch"
                | "workspace.git_added"
                | "workspace.git_deleted"
        ),
        TokenScope::Sidebar => matches!(
            token,
            "fut"
                | "session.name"
                | "workspace.name"
                | "workspace.icon"
                | "sidebar.display"
                | "sidebar.status"
                | "sidebar.visibility"
        ),
    };
    builtin || extension_token_allowed(scope, token, extensions)
}

fn extension_token_allowed(scope: TokenScope, token: &str, extensions: &[Extension]) -> bool {
    extensions
        .iter()
        .flat_map(Extension::presentation_tokens)
        .find(|declaration| declaration.qualified_name() == token)
        .is_some_and(|declaration| match scope {
            TokenScope::Bar | TokenScope::Sidebar => true,
            TokenScope::Tab => declaration.scope() == extensions::PresentationScope::Tab,
            TokenScope::Workspace => {
                declaration.scope() == extensions::PresentationScope::Workspace
            }
        })
}

fn validate_text(path: &str, value: &str) -> Result<()> {
    if value.len() > MAX_TEXT_BYTES {
        bail!("{path} exceeds {MAX_TEXT_BYTES} bytes");
    }
    if value.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    }) {
        bail!("{path} contains a control or bidirectional formatting character");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_catalog_loads_without_validating_ui_and_expands_home() {
        let temporary = tempfile::tempdir().unwrap();
        let config_dir = temporary.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            format!(
                r#"
[projects.fut]
path = {:?}

[ui]
unknown_future_option = true
"#,
                temporary.path().join("dev/fut")
            ),
        )
        .unwrap();
        let location = resolve_location(Some(&config_dir)).unwrap();
        let catalog = load_projects_location(&location).unwrap();

        assert_eq!(
            catalog.get("fut").unwrap().path(),
            temporary.path().join("dev/fut")
        );
        assert_eq!(
            expand_project_path(Path::new("~/dev/fut"), Some(temporary.path())).unwrap(),
            temporary.path().join("dev/fut")
        );
    }

    #[test]
    fn project_catalog_rejects_ambiguous_names_and_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            r#"
[projects."not a slug"]
path = "/one"
"#,
        )
        .unwrap();
        let location = ConfigLocation {
            path: Some(path.clone()),
            explicit: true,
            source: "test",
        };
        assert!(load_location(&location).is_err());

        fs::write(
            &path,
            r#"
[projects.one]
path = "/same"
[projects.two]
path = "/same"
"#,
        )
        .unwrap();
        assert!(load_location(&location).is_err());

        fs::write(
            &path,
            r#"
[projects.one]
path = "/one"
recipe_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
"#,
        )
        .unwrap();
        let error = format!("{:#}", load_location(&location).unwrap_err());
        assert!(error.contains("unknown field `recipe_sha256`"), "{error}");
    }

    #[test]
    fn missing_implicit_config_and_empty_file_use_defaults() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("missing.toml");
        assert_eq!(load_path(&missing, false).unwrap(), UiConfig::default());
        assert!(
            load_path(&missing, true)
                .unwrap_err()
                .to_string()
                .contains("read Fut config")
        );
        let empty = temporary.path().join("empty.toml");
        fs::write(&empty, "").unwrap();
        assert_eq!(load_path(&empty, true).unwrap(), UiConfig::default());
    }

    #[test]
    fn spinner_presets_and_custom_frames_are_strict_and_time_based() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(&path, "[ui.spinner]\nstyle = 'line'\n").unwrap();
        let preset = load_path(&path, true).unwrap();
        assert_eq!(preset.spinner.frame(0), "-");
        assert_eq!(preset.spinner.frame(130), "\\");

        fs::write(&path, "[ui.spinner]\nframes = ['a', 'b']\ninterval = 40\n").unwrap();
        let custom = load_path(&path, true).unwrap();
        assert_eq!(custom.spinner.frame(39), "a");
        assert_eq!(custom.spinner.frame(40), "b");

        for invalid in [
            "[ui.spinner]\nstyle = 'missing'\n",
            "[ui.spinner]\nframes = []\n",
            "[ui.spinner]\nframes = ['a', 'wide']\n",
            "[ui.spinner]\nframes = ['a']\ninterval = 5\n",
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(load_path(&path, true).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn sidebar_defaults_are_independent_and_partial_sides_keep_their_composition() {
        let defaults = UiConfig::default();
        assert_eq!(defaults.sidebar.left.width, 28);
        assert_eq!(defaults.sidebar.right.width, 28);
        assert_eq!(
            defaults.sidebar.left.visibility,
            SidebarVisibility::Automatic
        );
        assert_eq!(
            defaults.sidebar.right.visibility,
            SidebarVisibility::Automatic
        );
        assert!(matches!(
            defaults.sidebar.left.components.as_slice(),
            [SidebarComponentConfig::Workspaces {
                size: SidebarComponentSize::Fill,
                ..
            }]
        ));
        assert!(matches!(
            defaults.sidebar.right.components.as_slice(),
            [SidebarComponentConfig::Agents {
                size: SidebarComponentSize::Fill,
                scope: AgentScope::Session,
            }]
        ));

        let partial: UiConfig =
            toml::from_str("[sidebar.left]\nwidth = 31\n[sidebar.right]\nvisibility = 'hidden'\n")
                .unwrap();
        assert_eq!(partial.sidebar.left.width, 31);
        assert!(matches!(
            partial.sidebar.left.components.as_slice(),
            [SidebarComponentConfig::Workspaces { .. }]
        ));
        assert_eq!(partial.sidebar.right.visibility, SidebarVisibility::Hidden);
        assert!(matches!(
            partial.sidebar.right.components.as_slice(),
            [SidebarComponentConfig::Agents {
                scope: AgentScope::Session,
                ..
            }]
        ));
    }

    #[test]
    fn parses_complete_chrome_configuration_and_exact_colors() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            r##"
[ui]
pane_layout = "accordion"
confirm_close = false

[ui.icons]
preset = "nerd_font"
workspace = "W"
notification = "N "

[ui.styles.attention]
foreground = "#12abEF"
add_modifiers = ["bold", "underlined"]

[ui.tab_bar]
position = "bottom"
left = [{ segments = [{ token = "workspace.name", max_width = 20, style = "workspace", inverted = true, pill = true }], priority = 200 }]
center = [{ segments = [{ component = "tabs" }] }]
right = [{ segments = [{ token = "client.zoom", prefix = " " }], style = "current" }]

[ui.tab_bar.item]
segments = [{ token = "tab.icon" }, { text = " " }, { token = "tab.name" }]

[ui.sidebar.left]
width = 30
display = "minimized"
visibility = "visible"
components = [
  { component = "workspaces", size = "fill", header = [{ token = "session.name" }], footer = [{ token = "sidebar.status" }], row = { left = [{ token = "workspace.index" }], body = [{ token = "workspace.name" }], right = [{ token = "workspace.tab_count" }] } },
  { component = "agents", size = 6, scope = "workspace" },
]
"##,
        )
        .unwrap();
        let config = load_path(&path, true).unwrap();
        assert_eq!(config.pane_layout, PaneLayoutPolicy::Accordion);
        assert!(!config.confirm_close);
        assert_eq!(config.tab_bar.position, TabBarPosition::Bottom);
        assert_eq!(config.sidebar.left.width, 30);
        assert_eq!(config.sidebar.left.display, SidebarDisplay::Minimized);
        assert_eq!(config.sidebar.left.visibility, SidebarVisibility::Visible);
        assert_eq!(
            config.sidebar.left.components[0].size(),
            SidebarComponentSize::Fill
        );
        assert_eq!(
            config.sidebar.left.components[1].size(),
            SidebarComponentSize::Fixed(6)
        );
        assert!(matches!(
            config.sidebar.left.components[1],
            SidebarComponentConfig::Agents {
                scope: AgentScope::Workspace,
                ..
            }
        ));
        assert_eq!(config.icons.resolve().workspace, "W");
        assert_eq!(config.icons.resolve().notification, "N ");
        assert_eq!(config.icons.resolve().pill_left, "\u{e0b6}");
        assert_eq!(config.icons.resolve().pill_right, "\u{e0b4}");
        assert_eq!(UiConfig::default().icons.resolve().pill_left, "");
        let segment = &config.tab_bar.left[0].segments[0];
        assert!(matches!(
            segment,
            SegmentConfig::Token {
                style: Some(SemanticStyle::Workspace),
                visual: TokenVisual::Pill,
                ..
            }
        ));
        assert_eq!(
            config.styles.attention.foreground,
            Some(UiColor::Rgb(0x12, 0xab, 0xef))
        );
    }

    #[test]
    fn partial_nested_tables_inherit_complete_defaults() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            "[ui.bindings]\nopen_command_bar = ':'\n\n[ui.tab_bar]\nposition = 'bottom'\n\n[ui.styles.current]\nforeground = 'red'\n\n[ui.styles.attention]\nforeground = 'blue'\n\n[ui.sidebar.left]\ncomponents = [{ component = 'workspaces', row = { body = [{ text = 'custom' }] } }]\n",
        )
        .unwrap();
        let config = load_path(&path, true).unwrap();
        assert_eq!(
            config
                .bindings
                .action_for_suffix(b":")
                .map(super::config_key),
            Some("open_command_bar")
        );
        assert_eq!(
            config.bindings.label(ClientAction::OpenCommandBar),
            "Ctrl-b :"
        );
        assert_eq!(config.tab_bar.position, TabBarPosition::Bottom);
        assert!(config.tab_bar.left.iter().any(|group| {
            group
                .segments
                .iter()
                .any(|segment| matches!(segment, SegmentConfig::Tabs))
        }));
        assert_eq!(config.styles.current.background, None);
        assert_eq!(
            config.styles.current.add_modifiers,
            vec![ModifierName::Reversed]
        );
        assert_eq!(
            config.styles.current.remove_modifiers,
            vec![ModifierName::Underlined]
        );
        assert_eq!(config.styles.current.foreground, Some(UiColor::Red));
        assert_eq!(config.styles.attention.foreground, Some(UiColor::Blue));
        let workspaces = config.sidebar.left.workspaces().unwrap();
        assert_eq!(workspaces.row.left, SidebarRowConfig::default().left);
        assert_eq!(workspaces.row.right, SidebarRowConfig::default().right);
    }

    #[test]
    fn sidebar_visibility_parses_serializes_and_rejects_obsolete_settings() {
        for (value, visibility) in [
            ("visible", SidebarVisibility::Visible),
            ("automatic", SidebarVisibility::Automatic),
            ("hidden", SidebarVisibility::Hidden),
        ] {
            let config: UiConfig =
                toml::from_str(&format!("[sidebar.left]\nvisibility = {value:?}\n")).unwrap();
            assert_eq!(config.sidebar.left.visibility, visibility);
            assert_eq!(
                serde_json::to_string(&visibility).unwrap(),
                format!("{value:?}")
            );
        }

        let mut visibility = SidebarVisibility::Visible;
        visibility.cycle();
        assert_eq!(visibility, SidebarVisibility::Automatic);
        visibility.cycle();
        assert_eq!(visibility, SidebarVisibility::Hidden);
        visibility.cycle();
        assert_eq!(visibility, SidebarVisibility::Visible);
        visibility.set(SidebarVisibility::Hidden);
        assert_eq!(visibility, SidebarVisibility::Hidden);

        for obsolete in ["hide_when_single = false", "auto_hide_when_single = true"] {
            assert!(toml::from_str::<UiConfig>(&format!("[sidebar.left]\n{obsolete}\n")).is_err());
        }
        assert!(toml::from_str::<UiConfig>("[workspace_sidebar]\nwidth = 20\n").is_err());
    }

    #[test]
    fn sidebar_display_parses_and_toggles_independently() {
        let config: UiConfig =
            toml::from_str("[sidebar.left]\ndisplay = 'minimized'\nvisibility = 'automatic'\n")
                .unwrap();
        assert_eq!(config.sidebar.left.display, SidebarDisplay::Minimized);
        assert_eq!(config.sidebar.left.visibility, SidebarVisibility::Automatic);

        let mut display = SidebarDisplay::Expanded;
        display.toggle();
        assert_eq!(display, SidebarDisplay::Minimized);
        display.toggle();
        assert_eq!(display, SidebarDisplay::Expanded);
        display.set(SidebarDisplay::Minimized);
        assert_eq!(display, SidebarDisplay::Minimized);
    }

    #[test]
    fn prefix_is_a_configurable_binding_suffix() {
        use crate::client::input::{PrefixAction, PrefixState};

        let defaults = UiConfig::default();
        assert_eq!(
            defaults.bindings.action_for_suffix(b"\x02"),
            Some(ClientAction::FocusNextNotification)
        );
        assert_eq!(
            defaults.bindings.label(ClientAction::FocusNextNotification),
            "Ctrl-b Ctrl-b"
        );
        assert_eq!(
            defaults.bindings.label(ClientAction::ReloadConfig),
            "Ctrl-b R"
        );

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            "[ui.bindings]\nfocus_next_notification = '.'\nreload_config = 'prefix'\n",
        )
        .unwrap();
        let config = load_path(&path, true).unwrap();
        let mut prefix = PrefixState::new(config.bindings);
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(vec![2]),
            PrefixAction::Dispatch(ClientAction::ReloadConfig)
        );

        fs::write(&path, "[ui.bindings]\nfocus_next_notification = '.'\n").unwrap();
        let config = load_path(&path, true).unwrap();
        let mut prefix = PrefixState::new(config.bindings);
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Send(vec![2]));
    }

    #[test]
    fn rename_and_upper_level_close_actions_are_unbound_but_configurable() {
        let defaults = UiConfig::default();
        for action in [
            ClientAction::RenameSession,
            ClientAction::RenameWorkspace,
            ClientAction::RenameTab,
            ClientAction::CloseSession,
            ClientAction::CloseWorkspace,
            ClientAction::CloseTab,
        ] {
            assert_eq!(defaults.bindings.label(action), "Unbound");
        }

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            "[ui.bindings]\nrename_session = 'S'\nrename_workspace = 'W'\nrename_tab = 'T'\nclose_session = 'q'\nclose_workspace = 'Q'\nclose_tab = 'X'\n",
        )
        .unwrap();
        let config = load_path(&path, true).unwrap();
        assert_eq!(
            config.bindings.action_for_suffix(b"S"),
            Some(ClientAction::RenameSession)
        );
        assert_eq!(config.bindings.label(ClientAction::OpenProject), "Unbound");
        assert_eq!(
            config.bindings.action_for_suffix(b"W"),
            Some(ClientAction::RenameWorkspace)
        );
        assert_eq!(
            config.bindings.action_for_suffix(b"T"),
            Some(ClientAction::RenameTab)
        );
        assert_eq!(
            config.bindings.action_for_suffix(b"q"),
            Some(ClientAction::CloseSession)
        );
        assert_eq!(
            config.bindings.action_for_suffix(b"Q"),
            Some(ClientAction::CloseWorkspace)
        );
        assert_eq!(
            config.bindings.action_for_suffix(b"X"),
            Some(ClientAction::CloseTab)
        );
    }

    #[test]
    fn command_prefix_is_configurable_and_updates_prefix_bindings() {
        use crate::client::input::{PrefixAction, PrefixState};

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(&path, "[ui]\nprefix = 'ctrl-a'\n").unwrap();
        let config = load_path(&path, true).unwrap();
        assert_eq!(config.bindings.prefix(), b"\x01");
        assert_eq!(
            config.bindings.label(ClientAction::OpenCommandBar),
            "Ctrl-a :"
        );
        assert_eq!(
            config.bindings.label(ClientAction::FocusNextNotification),
            "Ctrl-a Ctrl-a"
        );

        let mut prefix = PrefixState::new(config.bindings);
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Send(vec![2]));
        assert_eq!(prefix.feed(vec![1]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(vec![1]),
            PrefixAction::Dispatch(ClientAction::FocusNextNotification)
        );
    }

    #[test]
    fn strict_validation_rejects_ambiguous_unsafe_and_out_of_scope_segments() {
        let temporary = tempfile::tempdir().unwrap();
        for source in [
            "[ui.tab_bar]\nleft = [{ segments = [{ text = 'x', token = 'fut' }] }]\n",
            "[ui.tab_bar]\nleft = [{ segments = [{ token = 'tab.marker' }] }]\n",
            "[ui.tab_bar.item]\nsegments = [{ component = 'tabs' }]\n",
            "[ui.tab_bar]\nleft = [{ segments = [{ text = \"x\\n\" }] }]\n",
            "[ui.tab_bar]\nleft = [{ segments = [{ text = 'x', inverted = true }] }]\n",
            "[ui.tab_bar]\nleft = [{ segments = [{ component = 'tabs', pill = true }] }]\n",
            "[ui.tab_bar]\nleft = [{ segments = [{ token = 'workspace.name', pill = true }] }]\n",
            "[ui]\nexecute = 'surprise'\n",
            "[ui.sidebar.left]\nwidth = 2\n",
            "[ui.sidebar.left]\ncomponents = [{ component = 'workspaces', size = 'fill' }, { component = 'agents', size = 'fill' }]\n",
            "[ui.sidebar.left]\ncomponents = [{ component = 'agents', size = 0 }]\n",
            "[ui.sidebar.left]\ncomponents = [{ component = 'agents', size = 'half' }]\n",
            "[ui.sidebar.right]\ncomponents = [{ component = 'agents', size = 'fill' }, { component = 'workspaces', size = 'fill' }]\n",
            "[ui.sidebar.left]\ncomponents = [{ component = 'workspaces', size = 2 }, { component = 'agents', size = 2 }, { component = 'workspaces', size = 2 }]\n",
            "[ui.tab_bar.item]\nmin_width = 8\n",
            "[ui.bindings]\nunknown = 'x'\n",
            "[ui.bindings]\nopen_command_bar = 's'\n",
            "[ui.bindings]\nopen_command_bar = 'ctrl-aa'\n",
            "[ui]\nprefix = 'prefix'\n",
            "[ui.icons]\nvertical_divider = '||'\n",
            "[ui.styles.normal]\nforeground = '#aéabc'\n",
        ] {
            let path = temporary.path().join(format!("bad-{}.toml", source.len()));
            fs::write(&path, source).unwrap();
            assert!(load_path(&path, true).is_err(), "accepted {source:?}");
        }
    }

    #[test]
    fn each_sidebar_side_rejects_more_than_one_workspaces_component() {
        let temporary = tempfile::tempdir().unwrap();
        for side in ["left", "right"] {
            let path = temporary.path().join(format!("duplicate-{side}.toml"));
            fs::write(
                &path,
                format!(
                    "[ui.sidebar.{side}]\ncomponents = [{{ component = 'workspaces', size = 2 }}, {{ component = 'agents', size = 2 }}, {{ component = 'workspaces', size = 2 }}]\n"
                ),
            )
            .unwrap();
            let error = format!("{:#}", load_path(&path, true).unwrap_err());
            assert!(
                error.contains(&format!(
                    "ui.sidebar.{side}.components may contain at most one workspaces component"
                )),
                "{error}"
            );
        }
    }

    #[test]
    fn malformed_unknown_and_oversized_configs_are_rejected_with_the_path() {
        let temporary = tempfile::tempdir().unwrap();
        for source in [
            "[ui]\npane_layout = 'sideways'\n",
            "[project]\ncommand = 'nope'\n",
        ] {
            let path = temporary.path().join(format!("bad-{}.toml", source.len()));
            fs::write(&path, source).unwrap();
            let error = load_path(&path, true).unwrap_err().to_string();
            assert!(error.contains(&path.display().to_string()), "{error}");
        }
        let path = temporary.path().join("large.toml");
        fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(
            load_path(&path, true)
                .unwrap_err()
                .to_string()
                .contains("maximum")
        );
    }

    #[test]
    fn daemon_extension_loading_ignores_unrelated_invalid_ui() {
        let temporary = tempfile::tempdir().unwrap();
        let extension_root = temporary.path().join("extension");
        fs::create_dir(&extension_root).unwrap();
        fs::write(
            extension_root.join(extensions::MANIFEST_FILE_NAME),
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = []\nid = 'run'\n",
        )
        .unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "extensions = [{:?}]\n[ui.tab_bar]\nleft = [{{ segments = [{{ token = 'not.a.token' }}] }}]\n[extension.run]\ncommand = ['global']\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();
        let location = ConfigLocation {
            path: Some(path.clone()),
            explicit: true,
            source: "test",
        };

        assert!(load_location(&location).is_err());
        let loaded = load_extensions_location(&location).unwrap();
        assert_eq!(loaded.extensions.len(), 1);
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let resolved = resolve_extension_config_for_catalog(
            &loaded.config,
            &loaded.extensions,
            "run",
            &workspace,
            None,
        )
        .unwrap();
        assert_eq!(resolved.json, r#"{"command":["global"]}"#);
        assert_eq!(resolved.global_source.as_deref(), Some(path.as_path()));

        fs::write(
            &path,
            format!(
                "extensions = [{:?}]\n[extension.run]\nvalue = {:?}\n",
                extension_root.display().to_string(),
                "x".repeat(MAX_EXTENSION_CONFIG_VALUE_BYTES + 1),
            ),
        )
        .unwrap();
        let error = load_extensions_location(&location).unwrap_err().to_string();
        assert!(error.contains("per value"), "{error}");
    }

    #[test]
    fn global_config_loads_explicit_extensions_atomically() {
        let temporary = tempfile::tempdir().unwrap();
        let extension_root = temporary.path().join("extension");
        fs::create_dir(&extension_root).unwrap();
        fs::write(
            extension_root.join(extensions::MANIFEST_FILE_NAME),
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = ['commands']\nid = 'configured'\n[commands.launch]\ntitle = 'Launch configured extension'\nargv = ['./bin/launch', '--ready']\nsize = { width = 90, height = 24 }\nactivate_opened = true\n",
        )
        .unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "extensions = [{:?}]\n[ui.bindings]\n\"configured:launch\" = \"N\"\n[extension_commands.\"configured:launch\"]\nargs = [\"locally\", \"chosen\"]\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();

        let loaded = load_path_outcome(&path, true).unwrap();
        assert_eq!(loaded.extensions.len(), 1);
        assert_eq!(loaded.extensions[0].id(), "configured");
        let command = loaded.ui.bindings.command(0).unwrap();
        assert_eq!(command.title, "Launch configured extension");
        assert_eq!(command.binding.as_deref(), Some("N"));
        assert_eq!(
            loaded.ui.bindings.action_for_suffix(b"N"),
            Some(ClientAction::RunCommand(0))
        );
        assert_eq!(
            command.program,
            extension_root.canonicalize().unwrap().join("bin/launch")
        );
        assert_eq!(command.args, ["locally", "chosen"]);
        assert!(matches!(
            command.execution,
            ExtensionCommandExecution::Interactive {
                size: PopupSize {
                    width: Some(90),
                    height: Some(24)
                },
                activate_opened: true,
            }
        ));
        assert_eq!(command.extension.as_ref().unwrap().id, "configured");
        assert_eq!(command.slug().as_deref(), Some("configured:launch"));

        fs::write(
            &path,
            format!(
                "extensions = [{:?}]\n[ui.bindings]\n\"configured:missing\" = \"N\"\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();
        let error = format!("{:#}", load_path_outcome(&path, true).unwrap_err());
        assert!(
            error.contains("unknown ui.bindings action \"configured:missing\""),
            "{error}"
        );

        fs::write(
            &path,
            format!(
                "extensions = [{:?}]\n[extension_commands.\"configured:missing\"]\nargs = []\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();
        let error = format!("{:#}", load_path_outcome(&path, true).unwrap_err());
        assert!(
            error.contains("unknown extension_commands command \"configured:missing\""),
            "{error}"
        );

        let invalid_root = temporary.path().join("invalid");
        fs::create_dir(&invalid_root).unwrap();
        fs::write(
            invalid_root.join(extensions::MANIFEST_FILE_NAME),
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = []\nid = 'INVALID'\n",
        )
        .unwrap();
        fs::write(
            &path,
            format!(
                "extensions = [{:?}, {:?}]\n",
                extension_root.display().to_string(),
                invalid_root.display().to_string()
            ),
        )
        .unwrap();
        let error = format!("{:#}", load_path_outcome(&path, true).unwrap_err());
        assert!(error.contains("INVALID"), "{error}");
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[test]
    fn staged_ui_materializes_every_extension_surface_from_catalog_without_manifest_reads() {
        let temporary = tempfile::tempdir().unwrap();
        let extension_root = temporary.path().join("extension");
        fs::create_dir(&extension_root).unwrap();
        let manifest = extension_root.join(extensions::MANIFEST_FILE_NAME);
        fs::write(
            &manifest,
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = ['commands', 'hooks', 'presentation_tokens']\nid = 'catalog'\n[hooks]\n'client.attached' = ['./hook']\n[commands.launch]\ntitle = 'Launch catalog'\nargv = ['./bin/launch', '--manifest']\n[[presentation_tokens]]\nname = 'badge'\nscope = 'tab'\npresentation = 'spinner'\n",
        )
        .unwrap();
        fs::write(
            temporary.path().join("config.toml"),
            format!(
                "extensions = [{:?}]\n[ui.bindings]\n\"catalog:launch\" = \"N\"\n[extension_commands.\"catalog:launch\"]\nargs = [\"configured\"]\n[extension.catalog]\nenabled = true\n[ui.tab_bar.item]\nsegments = [{{ token = 'tab.extension.catalog.badge' }}]\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();
        let staged = stage_location(&resolve_location(Some(temporary.path())).unwrap()).unwrap();
        let location = resolve_location(Some(temporary.path())).unwrap();
        let loaded = load_extensions_location(&location).unwrap();
        let catalog = extensions::ExtensionRegistry::new(3, loaded.extensions, loaded.config)
            .unwrap()
            .catalog()
            .unwrap();

        fs::remove_file(manifest).unwrap();
        let ui = staged.materialize(&catalog).unwrap();
        let command = ui.bindings.command(0).unwrap();
        assert_eq!(command.title, "Launch catalog");
        assert_eq!(command.args, ["configured"]);
        assert_eq!(
            ui.bindings.action_for_suffix(b"N"),
            Some(ClientAction::RunCommand(0))
        );
        assert_eq!(ui.extensions.len(), 1);
        assert_eq!(
            ui.extensions[0].presentation_tokens()[0].qualified_name(),
            "tab.extension.catalog.badge"
        );
        assert_eq!(
            resolve_extension_config(&ui, "catalog", temporary.path(), None)
                .unwrap()
                .json,
            r#"{"enabled":true}"#
        );

        let empty_catalog =
            extensions::ExtensionRegistry::new(4, Vec::new(), ExtensionConfigCatalog::default())
                .unwrap()
                .catalog()
                .unwrap();
        let error = staged.materialize(&empty_catalog).unwrap_err().to_string();
        assert!(
            error.contains("unknown extension_commands command")
                || error.contains("unknown or out-of-scope token")
        );
        assert_eq!(ui.bindings.command(0).unwrap().title, "Launch catalog");
    }

    #[test]
    fn extension_config_recursively_merges_workspace_over_global_and_is_inert() {
        let temporary = tempfile::tempdir().unwrap();
        let extension_root = temporary.path().join("extension");
        fs::create_dir(&extension_root).unwrap();
        fs::write(
            extension_root.join(extensions::MANIFEST_FILE_NAME),
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = ['commands']\nid = 'run'\n[commands.restart]\ntitle = 'Restart'\nargv = ['./restart']\nmode = 'background'\n",
        )
        .unwrap();
        let global_path = temporary.path().join("global.toml");
        fs::write(
            &global_path,
            format!(
                "extensions = [{:?}]\n[extension.run]\ncommand = ['global']\nkeep = true\nstarted = 2026-08-17T12:00:00Z\n[extension.run.log]\nsize = 10\ncolor = 'blue'\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();
        let loaded = load_path_outcome(&global_path, true).unwrap();
        assert_eq!(
            loaded.ui.bindings.command(0).unwrap().mode(),
            ExtensionCommandMode::Background
        );
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(workspace.join(".fut")).unwrap();
        let sentinel = workspace.join("must-not-exist");
        let workspace_path = workspace.join(".fut/config.toml");
        let project_path = temporary.path().join("project.toml");
        let project = TrustedProjectConfig {
            source: project_path.clone(),
            extension: BTreeMap::from([(
                "run".to_owned(),
                serde_json::json!({
                    "command": ["project"],
                    "auto_start": true,
                    "log": { "color": "green" }
                })
                .as_object()
                .unwrap()
                .clone(),
            )]),
        };
        fs::write(
            &workspace_path,
            format!(
                "[extension.run]\ncommand = ['touch', {:?}]\n[extension.run.log]\nsize = 20\n",
                sentinel.display().to_string()
            ),
        )
        .unwrap();

        let resolved =
            resolve_extension_config(&loaded.ui, "run", &workspace, Some(&project)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&resolved.json).unwrap();
        let trusted: serde_json::Value = serde_json::from_str(&resolved.trusted_json).unwrap();
        assert_eq!(value["command"], serde_json::json!(["touch", sentinel]));
        assert_eq!(trusted["command"], serde_json::json!(["project"]));
        assert_eq!(trusted["auto_start"], true);
        assert_eq!(value["keep"], true);
        assert_eq!(value["started"], "2026-08-17T12:00:00Z");
        assert_eq!(value["log"]["size"], 20);
        assert_eq!(value["log"]["color"], "green");
        assert_eq!(trusted["log"]["color"], "green");
        assert_eq!(
            resolved.global_source.as_deref(),
            Some(global_path.as_path())
        );
        assert_eq!(
            resolved.project_source.as_deref(),
            Some(project_path.as_path())
        );
        assert_eq!(
            resolved.workspace_source.as_deref(),
            Some(workspace_path.as_path())
        );
        assert!(
            !sentinel.exists(),
            "loading project config must not execute it"
        );
    }

    #[test]
    fn extension_config_rejects_unknown_ids_in_global_and_workspace_sources() {
        let temporary = tempfile::tempdir().unwrap();
        let extension_root = temporary.path().join("extension");
        fs::create_dir(&extension_root).unwrap();
        fs::write(
            extension_root.join(extensions::MANIFEST_FILE_NAME),
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = ['commands']\nid = 'known'\n[commands.open]\ntitle = 'Open'\nargv = ['./open']\n",
        )
        .unwrap();
        let global_path = temporary.path().join("global.toml");
        fs::write(
            &global_path,
            format!(
                "extensions = [{:?}]\n[extension.unknown]\nvalue = true\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();
        let error = format!("{:#}", load_path_outcome(&global_path, true).unwrap_err());
        assert!(
            error.contains("unknown extension ID \"unknown\""),
            "{error}"
        );
        assert!(
            error.contains(&global_path.display().to_string()),
            "{error}"
        );
        let error = format!(
            "{:#}",
            load_extensions_location(&ConfigLocation {
                path: Some(global_path.clone()),
                explicit: true,
                source: "test",
            })
            .unwrap_err()
        );
        assert!(
            error.contains("unknown extension ID \"unknown\""),
            "{error}"
        );

        fs::write(
            &global_path,
            format!(
                "extensions = [{:?}]\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();
        let loaded = load_path_outcome(&global_path, true).unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(workspace.join(".fut")).unwrap();
        let workspace_path = workspace.join(".fut/config.toml");
        fs::write(&workspace_path, "[extension.unknown]\nvalue = true\n").unwrap();
        let error = format!(
            "{:#}",
            resolve_extension_config(&loaded.ui, "known", &workspace, None).unwrap_err()
        );
        assert!(
            error.contains("unknown extension ID \"unknown\""),
            "{error}"
        );
        assert!(
            error.contains(&workspace_path.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn extension_config_bounds_keys_depth_values_and_serialized_size() {
        let source = Path::new("/tmp/extension-config-test.toml");

        let too_many_keys = (0..=MAX_EXTENSION_CONFIG_KEYS)
            .map(|index| (format!("key-{index}"), serde_json::Value::Bool(true)))
            .collect();
        assert!(
            validate_extension_config_table("run", &too_many_keys, source)
                .unwrap_err()
                .to_string()
                .contains("keys")
        );

        let mut nested = serde_json::Value::Bool(true);
        for index in 0..=MAX_EXTENSION_CONFIG_DEPTH {
            nested = serde_json::json!({ format!("level-{index}"): nested });
        }
        let nested = nested.as_object().unwrap().clone();
        assert!(
            validate_extension_config_table("run", &nested, source)
                .unwrap_err()
                .to_string()
                .contains("depth")
        );

        let oversized_value = serde_json::json!({
            "value": "x".repeat(MAX_EXTENSION_CONFIG_VALUE_BYTES + 1)
        });
        assert!(
            validate_extension_config_table("run", oversized_value.as_object().unwrap(), source)
                .unwrap_err()
                .to_string()
                .contains("per value")
        );

        let serialized = serde_json::json!({
            "one": "x".repeat(3_500),
            "two": "x".repeat(3_500),
            "three": "x".repeat(3_500),
            "four": "x".repeat(3_500),
            "five": "x".repeat(3_500),
        });
        assert!(
            validate_extension_config_table("run", serialized.as_object().unwrap(), source)
                .unwrap_err()
                .to_string()
                .contains("serializes")
        );
    }

    #[test]
    fn extension_tokens_are_validated_against_each_newly_loaded_catalog() {
        let temporary = tempfile::tempdir().unwrap();
        let extension_root = temporary.path().join("extension");
        fs::create_dir(&extension_root).unwrap();
        let manifest = extension_root.join(extensions::MANIFEST_FILE_NAME);
        fs::write(
            &manifest,
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = ['presentation_tokens']\nid = 'status'\n[[presentation_tokens]]\nname = 'whole'\nscope = 'session'\n[[presentation_tokens]]\nname = 'state'\nscope = 'workspace'\n[[presentation_tokens]]\nname = 'badge'\nscope = 'tab'\n[[presentation_tokens]]\nname = 'mark'\nscope = 'pane'\n",
        )
        .unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "extensions = [{:?}]\n[ui.tab_bar]\nright = [{{ segments = [{{ token = 'session.extension.status.whole' }}, {{ token = 'pane.extension.status.mark' }}] }}]\n[ui.sidebar.left]\ncomponents = [{{ component = 'workspaces', row = {{ body = [{{ token = 'workspace.extension.status.state' }}] }} }}]\n[ui.tab_bar.item]\nsegments = [{{ token = 'tab.extension.status.badge' }}]\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();
        load_path_outcome(&path, true).unwrap();

        fs::write(
            &manifest,
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = []\nid = 'status'\n",
        )
        .unwrap();
        let error = format!("{:#}", load_path_outcome(&path, true).unwrap_err());
        assert!(error.contains("unknown or out-of-scope token"), "{error}");

        fs::write(
            &manifest,
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = ['presentation_tokens']\nid = 'status'\n[[presentation_tokens]]\nname = 'state'\nscope = 'workspace'\n",
        )
        .unwrap();
        fs::write(
            &path,
            format!(
                "extensions = [{:?}]\n[ui.tab_bar.item]\nsegments = [{{ token = 'workspace.extension.status.state' }}]\n",
                extension_root.display().to_string()
            ),
        )
        .unwrap();
        let error = format!("{:#}", load_path_outcome(&path, true).unwrap_err());
        assert!(error.contains("unknown or out-of-scope token"), "{error}");
    }

    #[test]
    fn trusted_commands_parse_displace_defaults_and_reject_collisions() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            r#"
[trusted_commands.git_diff]
title = "Repository diff"
binding = "s"
program = "/bin/sh"
args = ["-c", "git diff"]
size = { width = 120, height = 40 }
"#,
        )
        .unwrap();
        let config = load_path(&path, true).unwrap();
        assert_eq!(
            config.bindings.action_for_suffix(b"s"),
            Some(ClientAction::RunCommand(0))
        );
        assert_eq!(
            config.bindings.label(ClientAction::OpenNavigator),
            "Unbound"
        );
        assert_eq!(config.bindings.command(0).unwrap().args, ["-c", "git diff"]);
        assert!(matches!(
            config.bindings.command(0).unwrap().execution,
            ExtensionCommandExecution::Interactive {
                size: PopupSize {
                    width: Some(120),
                    height: Some(40)
                },
                ..
            }
        ));

        fs::write(
            &path,
            r#"
[ui.bindings]
open_navigator = "s"
[trusted_commands.git_diff]
title = "Repository diff"
binding = "s"
program = "/bin/true"
"#,
        )
        .unwrap();
        assert!(
            load_path(&path, true)
                .unwrap_err()
                .to_string()
                .contains("validate")
        );

        fs::write(
            &path,
            r#"
[trusted_commands.one]
title = "One"
binding = "x"
program = "/bin/true"
[trusted_commands.two]
title = "Two"
binding = "x"
program = "/bin/true"
"#,
        )
        .unwrap();
        assert!(load_path(&path, true).is_err());
    }
}
