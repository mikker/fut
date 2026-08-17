use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Deserializer, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::actions::{
    ALL_ACTIONS, ClientAction, config_key, default_suffix, parse_suffix, suffix_name,
};
use crate::{
    command::PopupSize,
    extensions::{self, Extension},
};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_SEGMENTS: usize = 64;
const MAX_TEXT_BYTES: usize = 1024;

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

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(transparent)]
pub(super) struct BindingsConfig {
    values: BTreeMap<String, String>,
    #[serde(skip)]
    commands: Vec<PaletteCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PaletteCommand {
    pub title: String,
    #[serde(default)]
    pub binding: Option<String>,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub size: PopupSize,
    #[serde(default)]
    pub activate_opened: bool,
    #[serde(skip)]
    pub extension: Option<ExtensionCommandIdentity>,
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

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtensionCommandConfig {
    args: Vec<String>,
}

impl PaletteCommand {
    pub(super) fn slug(&self) -> Option<String> {
        self.extension.as_ref().map(ExtensionCommandIdentity::slug)
    }
}

impl BindingsConfig {
    pub(super) fn command(&self, index: usize) -> Option<&PaletteCommand> {
        self.commands.get(index)
    }

    pub(super) fn suffix(&self, action: ClientAction) -> Vec<u8> {
        self.values
            .get(config_key(action))
            .and_then(|value| parse_suffix(value))
            .map_or_else(|| default_suffix(action).to_vec(), |(bytes, _)| bytes)
    }

    pub(super) fn suffix_label(&self, action: ClientAction) -> String {
        self.values.get(config_key(action)).map_or_else(
            || suffix_name(default_suffix(action)),
            |value| parse_suffix(value).expect("bindings are validated").1,
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
                                "Ctrl-b {}",
                                parse_suffix(binding).expect("commands are validated").1
                            )
                        })
                },
            );
        }
        if self.commands.iter().any(|command| {
            command.binding.as_ref().is_some_and(|binding| {
                parse_suffix(binding).is_some_and(|(suffix, _)| suffix == self.suffix(action))
            })
        }) {
            return "Unbound".into();
        }
        format!("Ctrl-b {}", self.suffix_label(action))
    }

    pub(super) fn action_for_suffix(&self, suffix: &[u8]) -> Option<ClientAction> {
        if let Some(index) = self.commands.iter().position(|command| {
            command.binding.as_ref().is_some_and(|binding| {
                parse_suffix(binding).is_some_and(|(bytes, _)| bytes == suffix)
            })
        }) {
            return Some(ClientAction::RunCommand(index));
        }
        ALL_ACTIONS
            .into_iter()
            .find(|action| self.suffix(*action) == suffix && self.label(*action) != "Unbound")
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
    pub vertical_divider: String,
    pub pill_left: String,
    pub pill_right: String,
}

impl IconsConfig {
    pub fn resolve(&self) -> IconSet {
        let defaults = match self.preset {
            IconPreset::Ascii => ["*", "x", "...", "", "", "zoom", "|", "", ""],
            IconPreset::Unicode => ["•", "×", "…", "", "", "zoom", "│", "", ""],
            IconPreset::NerdFont => ["󰄬", "󰅖", "…", "󰉋", "󰓩", "󰍉", "│", "\u{e0b6}", "\u{e0b4}"],
        };
        IconSet {
            current: self.current.clone().unwrap_or_else(|| defaults[0].into()),
            closing: self.closing.clone().unwrap_or_else(|| defaults[1].into()),
            overflow: self.overflow.clone().unwrap_or_else(|| defaults[2].into()),
            workspace: self.workspace.clone().unwrap_or_else(|| defaults[3].into()),
            tab: self.tab.clone().unwrap_or_else(|| defaults[4].into()),
            zoom: self.zoom.clone().unwrap_or_else(|| defaults[5].into()),
            vertical_divider: self
                .vertical_divider
                .clone()
                .unwrap_or_else(|| defaults[6].into()),
            pill_left: self.pill_left.clone().unwrap_or_else(|| defaults[7].into()),
            pill_right: self
                .pill_right
                .clone()
                .unwrap_or_else(|| defaults[8].into()),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SegmentConfig {
    pub text: Option<String>,
    pub token: Option<String>,
    pub component: Option<String>,
    pub style: Option<SemanticStyle>,
    pub prefix: String,
    pub suffix: String,
    pub max_width: Option<u16>,
}

impl SegmentConfig {
    fn text(value: &str) -> Self {
        Self {
            text: Some(value.into()),
            ..Self::default()
        }
    }

    fn token(value: &str) -> Self {
        Self {
            token: Some(value.into()),
            ..Self::default()
        }
    }

    fn component(value: &str) -> Self {
        Self {
            component: Some(value.into()),
            ..Self::default()
        }
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
                SegmentConfig {
                    token: Some("tab.name".into()),
                    prefix: " ".into(),
                    ..SegmentConfig::default()
                },
                SegmentConfig {
                    token: Some("tab.closing".into()),
                    prefix: " ".into(),
                    ..SegmentConfig::default()
                },
                SegmentConfig {
                    token: Some("tab.activity".into()),
                    prefix: " ".into(),
                    ..SegmentConfig::default()
                },
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
                    segments: vec![SegmentConfig {
                        token: Some("workspace.name".into()),
                        prefix: " ".into(),
                        suffix: " ".into(),
                        max_width: Some(20),
                        ..SegmentConfig::default()
                    }],
                    style: Some(SemanticStyle::Muted),
                    priority: 200,
                },
                GroupConfig {
                    segments: vec![SegmentConfig {
                        token: Some("client.zoom".into()),
                        suffix: " ".into(),
                        ..SegmentConfig::default()
                    }],
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
                    segments: vec![SegmentConfig {
                        token: Some("client.waiting".into()),
                        prefix: " ".into(),
                        suffix: " ".into(),
                        ..SegmentConfig::default()
                    }],
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
                SegmentConfig {
                    token: Some("workspace.name".into()),
                    prefix: " ".into(),
                    ..SegmentConfig::default()
                },
            ],
            right: vec![
                SegmentConfig {
                    token: Some("workspace.activity".into()),
                    prefix: " ".into(),
                    ..SegmentConfig::default()
                },
                SegmentConfig {
                    token: Some("workspace.closing".into()),
                    prefix: " ".into(),
                    ..SegmentConfig::default()
                },
                SegmentConfig::text(" "),
            ],
            detail: vec![
                SegmentConfig::text("    "),
                SegmentConfig {
                    token: Some("workspace.git_branch".into()),
                    style: Some(SemanticStyle::Muted),
                    ..SegmentConfig::default()
                },
                SegmentConfig {
                    token: Some("workspace.git_added".into()),
                    prefix: " ".into(),
                    ..SegmentConfig::default()
                },
                SegmentConfig {
                    token: Some("workspace.git_deleted".into()),
                    prefix: " ".into(),
                    ..SegmentConfig::default()
                },
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
    vec![SegmentConfig {
        token: Some("sidebar.status".into()),
        style: Some(SemanticStyle::Muted),
        ..SegmentConfig::default()
    }]
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
    pub(super) bindings: BindingsConfig,
    pub(super) icons: IconsConfig,
    pub(super) styles: StylesConfig,
    pub(super) tab_bar: TabBarConfig,
    pub(super) sidebar: SidebarConfig,
    #[serde(skip)]
    pub(super) extensions: Vec<Extension>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            pane_layout: PaneLayoutPolicy::Splits,
            confirm_close: true,
            bindings: BindingsConfig::default(),
            icons: IconsConfig::default(),
            styles: StylesConfig::default(),
            tab_bar: TabBarConfig::default(),
            sidebar: SidebarConfig::default(),
            extensions: Vec::new(),
        }
    }
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
            icons.vertical_divider,
            icons.pill_left,
            icons.pill_right,
        ]
        .into_iter()
        .filter(|icon| !icon.is_empty())
        .collect()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    ui: UiConfig,
    trusted_commands: BTreeMap<String, PaletteCommand>,
    extension_commands: BTreeMap<String, ExtensionCommandConfig>,
    extensions: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigLocation {
    pub path: Option<PathBuf>,
    pub explicit: bool,
    pub source: &'static str,
}

#[derive(Debug)]
pub(crate) struct LoadedConfig {
    pub ui: UiConfig,
    pub extensions: Vec<Extension>,
    pub present: bool,
}

pub(crate) fn resolve_location(config_dir: Option<&std::path::Path>) -> Result<ConfigLocation> {
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

pub(super) fn load(config_dir: Option<&std::path::Path>) -> Result<UiConfig> {
    let location = resolve_location(config_dir)?;
    Ok(load_location(&location)?.ui)
}

pub(crate) fn load_location(location: &ConfigLocation) -> Result<LoadedConfig> {
    location.path.as_ref().map_or_else(
        || {
            Ok(LoadedConfig {
                ui: UiConfig::default(),
                extensions: Vec::new(),
                present: false,
            })
        },
        |path| load_path_outcome(path, location.explicit),
    )
}

/// Load only the daemon-owned extension declarations. Client presentation
/// mistakes must not prevent the daemon from starting; the interactive client
/// validates the complete configuration independently before changing terminal
/// state.
pub(crate) fn load_extensions_location(location: &ConfigLocation) -> Result<Vec<Extension>> {
    let Some(path) = &location.path else {
        return Ok(Vec::new());
    };
    let Some(source) = read_config_source(path, location.explicit)? else {
        return Ok(Vec::new());
    };

    #[derive(Default, Deserialize)]
    struct ExtensionConfig {
        #[serde(default)]
        extensions: Vec<PathBuf>,
    }

    let config = toml::from_str::<ExtensionConfig>(&source)
        .with_context(|| format!("parse extension paths from Fut config {}", path.display()))?;
    extensions::load(&config.extensions)
        .with_context(|| format!("load extensions from Fut config {}", path.display()))
}

#[cfg(test)]
fn load_path(path: &std::path::Path, explicit: bool) -> Result<UiConfig> {
    Ok(load_path_outcome(path, explicit)?.ui)
}

fn load_path_outcome(path: &std::path::Path, explicit: bool) -> Result<LoadedConfig> {
    let Some(source) = read_config_source(path, explicit)? else {
        return Ok(LoadedConfig {
            ui: UiConfig::default(),
            extensions: Vec::new(),
            present: false,
        });
    };
    let mut config = toml::from_str::<Config>(&source)
        .with_context(|| format!("parse Fut config {}", path.display()))?;
    let loaded_extensions = extensions::load(&config.extensions)
        .with_context(|| format!("load extensions from Fut config {}", path.display()))?;
    config.ui.bindings.commands = config.trusted_commands.into_values().collect();
    for extension in &loaded_extensions {
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
                size: launcher.size(),
                activate_opened: launcher.activate_opened(),
                extension: Some(identity),
            });
        }
    }
    if let Some(slug) = config.extension_commands.keys().next() {
        bail!("unknown extension_commands command {slug:?}");
    }
    validate(&config.ui, &loaded_extensions)
        .with_context(|| format!("validate Fut config {}", path.display()))?;
    config.ui.extensions = loaded_extensions.clone();
    Ok(LoadedConfig {
        ui: config.ui,
        extensions: loaded_extensions,
        present: true,
    })
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

fn validate(ui: &UiConfig, extensions: &[Extension]) -> Result<()> {
    let valid_binding_keys = ALL_ACTIONS
        .into_iter()
        .map(config_key)
        .collect::<HashSet<_>>();
    let mut bound_suffixes = HashSet::new();
    for (key, value) in &ui.bindings.values {
        if !valid_binding_keys.contains(key.as_str()) {
            bail!("unknown ui.bindings action {key:?}");
        }
        if parse_suffix(value).is_none() {
            bail!(
                "ui.bindings.{key} must be one character or prefix, ctrl-s, space, enter, tab, esc, up, or down"
            );
        }
    }
    for action in ALL_ACTIONS {
        if !bound_suffixes.insert(ui.bindings.suffix(action)) {
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
        command
            .size
            .validate()
            .context("validate command popup size")?;
        if let Some(binding) = &command.binding {
            let Some((suffix, _)) = parse_suffix(binding) else {
                bail!("command bindings must be one character or a named key");
            };
            if !command_suffixes.insert(suffix.clone()) {
                bail!("commands must not assign the same key to multiple commands");
            }
            if ALL_ACTIONS.into_iter().any(|action| {
                explicitly_bound.contains(config_key(action))
                    && ui.bindings.suffix(action) == suffix
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
        ("vertical_divider", &ui.icons.vertical_divider),
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
                .any(|segment| segment.component.is_some())
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
        let selectors = usize::from(segment.text.is_some())
            + usize::from(segment.token.is_some())
            + usize::from(segment.component.is_some());
        if selectors != 1 {
            bail!("{path} must set exactly one of text, token, or component");
        }
        for (field, value) in [
            ("text", segment.text.as_deref()),
            ("prefix", Some(segment.prefix.as_str())),
            ("suffix", Some(segment.suffix.as_str())),
        ] {
            if let Some(value) = value {
                validate_text(&format!("{path}.{field}"), value)?;
            }
        }
        if segment.text.is_some()
            && (!segment.prefix.is_empty()
                || !segment.suffix.is_empty()
                || segment.max_width.is_some())
        {
            bail!("{path} text segments do not accept prefix, suffix, or max_width");
        }
        if let Some(token) = segment.token.as_deref()
            && !token_allowed(scope, token, extensions)
        {
            bail!("{path} contains unknown or out-of-scope token {token:?}");
        }
        if let Some(component) = segment.component.as_deref() {
            if !components || component != "tabs" {
                bail!("{path} contains unknown or out-of-scope component {component:?}");
            }
            if segment.style.is_some()
                || !segment.prefix.is_empty()
                || !segment.suffix.is_empty()
                || segment.max_width.is_some()
            {
                bail!("{path} component does not accept style, prefix, suffix, or max_width");
            }
            *component_count += 1;
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

[ui.styles.attention]
foreground = "#12abEF"
add_modifiers = ["bold", "underlined"]

[ui.tab_bar]
position = "bottom"
left = [{ segments = [{ token = "workspace.name", max_width = 20 }], priority = 200 }]
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
        assert_eq!(config.icons.resolve().pill_left, "\u{e0b6}");
        assert_eq!(config.icons.resolve().pill_right, "\u{e0b4}");
        assert_eq!(UiConfig::default().icons.resolve().pill_left, "");
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
                .any(|segment| segment.component.as_deref() == Some("tabs"))
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
    fn strict_validation_rejects_ambiguous_unsafe_and_out_of_scope_segments() {
        let temporary = tempfile::tempdir().unwrap();
        for source in [
            "[ui.tab_bar]\nleft = [{ segments = [{ text = 'x', token = 'fut' }] }]\n",
            "[ui.tab_bar]\nleft = [{ segments = [{ token = 'tab.marker' }] }]\n",
            "[ui.tab_bar.item]\nsegments = [{ component = 'tabs' }]\n",
            "[ui.tab_bar]\nleft = [{ segments = [{ text = \"x\\n\" }] }]\n",
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
            "[ui.bindings]\nopen_command_bar = 'ctrl-x'\n",
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
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            "[ui.tab_bar]\nleft = [{ segments = [{ token = 'not.a.token' }] }]\n",
        )
        .unwrap();
        let location = ConfigLocation {
            path: Some(path),
            explicit: true,
            source: "test",
        };

        assert!(load_location(&location).is_err());
        assert!(load_extensions_location(&location).unwrap().is_empty());
    }

    #[test]
    fn global_config_loads_explicit_extensions_atomically() {
        let temporary = tempfile::tempdir().unwrap();
        let extension_root = temporary.path().join("extension");
        fs::create_dir(&extension_root).unwrap();
        fs::write(
            extension_root.join(extensions::MANIFEST_FILE_NAME),
            "id = 'configured'\n[commands.launch]\ntitle = 'Launch configured extension'\nargv = ['./bin/launch', '--ready']\nsize = { width = 90, height = 24 }\nactivate_opened = true\n",
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
        assert_eq!(command.size.width, Some(90));
        assert_eq!(command.size.height, Some(24));
        assert!(command.activate_opened);
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
            "id = 'INVALID'\n",
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
    fn extension_tokens_are_validated_against_each_newly_loaded_catalog() {
        let temporary = tempfile::tempdir().unwrap();
        let extension_root = temporary.path().join("extension");
        fs::create_dir(&extension_root).unwrap();
        let manifest = extension_root.join(extensions::MANIFEST_FILE_NAME);
        fs::write(
            &manifest,
            "id = 'status'\n[[presentation_tokens]]\nname = 'whole'\nscope = 'session'\n[[presentation_tokens]]\nname = 'state'\nscope = 'workspace'\n[[presentation_tokens]]\nname = 'badge'\nscope = 'tab'\n[[presentation_tokens]]\nname = 'mark'\nscope = 'pane'\n",
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

        fs::write(&manifest, "id = 'status'\n").unwrap();
        let error = format!("{:#}", load_path_outcome(&path, true).unwrap_err());
        assert!(error.contains("unknown or out-of-scope token"), "{error}");

        fs::write(
            &manifest,
            "id = 'status'\n[[presentation_tokens]]\nname = 'state'\nscope = 'workspace'\n",
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
        assert_eq!(config.bindings.command(0).unwrap().size.width, Some(120));
        assert_eq!(config.bindings.command(0).unwrap().size.height, Some(40));

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
