use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Deserializer};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::actions::{ALL_ACTIONS, ClientAction, config_key, default_suffix, parse_suffix};

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WorkspaceSidebarPosition {
    #[default]
    Left,
    Right,
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
    Current,
    Selected,
    Closing,
    Attention,
    Error,
    Divider,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(transparent)]
pub(super) struct BindingsConfig {
    values: BTreeMap<String, String>,
}

impl BindingsConfig {
    pub(super) fn suffix(&self, action: ClientAction) -> Vec<u8> {
        self.values
            .get(config_key(action))
            .and_then(|value| parse_suffix(value))
            .map_or_else(|| default_suffix(action).to_vec(), |(bytes, _)| bytes)
    }

    pub(super) fn label(&self, action: ClientAction) -> String {
        let suffix = self.values.get(config_key(action)).map_or_else(
            || {
                if default_suffix(action) == b" " {
                    "Space".into()
                } else {
                    String::from_utf8_lossy(default_suffix(action)).into_owned()
                }
            },
            |value| parse_suffix(value).expect("bindings are validated").1,
        );
        format!("Ctrl-b {suffix}")
    }

    pub(super) fn action_for_suffix(&self, suffix: &[u8]) -> Option<ClientAction> {
        ALL_ACTIONS
            .into_iter()
            .find(|action| self.suffix(*action) == suffix)
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
    current: StyleConfig,
    selected: StyleConfig,
    closing: StyleConfig,
    attention: StyleConfig,
    error: StyleConfig,
    divider: StyleConfig,
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
    current: Option<StylePatch>,
    selected: Option<StylePatch>,
    closing: Option<StylePatch>,
    attention: Option<StylePatch>,
    error: Option<StylePatch>,
    divider: Option<StylePatch>,
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
            (patch.current, &mut styles.current),
            (patch.selected, &mut styles.selected),
            (patch.closing, &mut styles.closing),
            (patch.attention, &mut styles.attention),
            (patch.error, &mut styles.error),
            (patch.divider, &mut styles.divider),
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
            current: StyleConfig {
                background: Some(UiColor::DarkGray),
                remove_modifiers: vec![ModifierName::Reversed, ModifierName::Underlined],
                ..StyleConfig::default()
            },
            selected: StyleConfig {
                background: Some(UiColor::DarkGray),
                add_modifiers: vec![ModifierName::Underlined],
                remove_modifiers: vec![ModifierName::Reversed],
                ..StyleConfig::default()
            },
            closing: with(ModifierName::Dim),
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
        }
    }
}

impl StylesConfig {
    pub fn apply(&self, role: SemanticStyle, style: Style) -> Style {
        match role {
            SemanticStyle::Normal => &self.normal,
            SemanticStyle::Muted => &self.muted,
            SemanticStyle::Current => &self.current,
            SemanticStyle::Selected => &self.selected,
            SemanticStyle::Closing => &self.closing,
            SemanticStyle::Attention => &self.attention,
            SemanticStyle::Error => &self.error,
            SemanticStyle::Divider => &self.divider,
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
}

impl IconsConfig {
    pub fn resolve(&self) -> IconSet {
        let defaults = match self.preset {
            IconPreset::Ascii => ["*", "x", "...", "", "", "zoom", "|"],
            IconPreset::Unicode => ["●", "×", "…", "", "", "zoom", "│"],
            IconPreset::NerdFont => ["󰄬", "󰅖", "…", "󰉋", "󰓩", "󰍉", "│"],
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

fn default_tab_min_width() -> u16 {
    12
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ItemFormat {
    pub segments: Vec<SegmentConfig>,
    #[serde(default = "default_tab_min_width")]
    pub min_width: u16,
}

impl Default for ItemFormat {
    fn default() -> Self {
        Self {
            segments: vec![
                SegmentConfig::text(" "),
                SegmentConfig::token("tab.index"),
                SegmentConfig {
                    token: Some("tab.closing".into()),
                    prefix: " ".into(),
                    ..SegmentConfig::default()
                },
                SegmentConfig::text(" "),
            ],
            min_width: default_tab_min_width(),
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
            ],
            item: ItemFormat::default(),
        }
    }
}

fn default_sidebar_width() -> u16 {
    24
}

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
            left: vec![
                SegmentConfig::token("workspace.marker"),
                SegmentConfig::text(" "),
            ],
            body: vec![SegmentConfig::token("workspace.name")],
            right: vec![SegmentConfig {
                token: Some("workspace.closing".into()),
                prefix: " ".into(),
                ..SegmentConfig::default()
            }],
            detail: vec![SegmentConfig {
                token: Some("workspace.root".into()),
                style: Some(SemanticStyle::Muted),
                ..SegmentConfig::default()
            }],
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct WorkspaceSidebarConfig {
    pub position: WorkspaceSidebarPosition,
    #[serde(default = "default_sidebar_width")]
    pub width: u16,
    #[serde(default = "default_true")]
    pub hide_when_single: bool,
    pub header: Vec<SegmentConfig>,
    pub footer: Vec<SegmentConfig>,
    pub row: SidebarRowConfig,
}

impl Default for WorkspaceSidebarConfig {
    fn default() -> Self {
        Self {
            position: WorkspaceSidebarPosition::Left,
            width: default_sidebar_width(),
            hide_when_single: true,
            header: Vec::new(),
            footer: vec![SegmentConfig {
                token: Some("sidebar.status".into()),
                style: Some(SemanticStyle::Muted),
                ..SegmentConfig::default()
            }],
            row: SidebarRowConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct UiConfig {
    pub(super) pane_layout: PaneLayoutPolicy,
    pub(super) bindings: BindingsConfig,
    pub(super) icons: IconsConfig,
    pub(super) styles: StylesConfig,
    pub(super) tab_bar: TabBarConfig,
    pub(super) workspace_sidebar: WorkspaceSidebarConfig,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            pane_layout: PaneLayoutPolicy::Splits,
            bindings: BindingsConfig::default(),
            icons: IconsConfig::default(),
            styles: StylesConfig::default(),
            tab_bar: TabBarConfig::default(),
            workspace_sidebar: WorkspaceSidebarConfig::default(),
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigLocation {
    pub path: Option<PathBuf>,
    pub explicit: bool,
    pub source: &'static str,
}

pub(crate) struct LoadedUiConfig {
    pub ui: UiConfig,
    pub present: bool,
}

pub(crate) fn resolve_location() -> Result<ConfigLocation> {
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

pub(super) fn load() -> Result<UiConfig> {
    let location = resolve_location()?;
    Ok(load_location(&location)?.ui)
}

pub(crate) fn load_location(location: &ConfigLocation) -> Result<LoadedUiConfig> {
    location.path.as_ref().map_or_else(
        || {
            Ok(LoadedUiConfig {
                ui: UiConfig::default(),
                present: false,
            })
        },
        |path| load_path_outcome(path, location.explicit),
    )
}

#[cfg(test)]
fn load_path(path: &std::path::Path, explicit: bool) -> Result<UiConfig> {
    Ok(load_path_outcome(path, explicit)?.ui)
}

fn load_path_outcome(path: &std::path::Path, explicit: bool) -> Result<LoadedUiConfig> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !explicit => {
            return Ok(LoadedUiConfig {
                ui: UiConfig::default(),
                present: false,
            });
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
    let config = toml::from_str::<Config>(&source)
        .with_context(|| format!("parse Fut config {}", path.display()))?;
    validate(&config.ui).with_context(|| format!("validate Fut config {}", path.display()))?;
    Ok(LoadedUiConfig {
        ui: config.ui,
        present: true,
    })
}

fn validate(ui: &UiConfig) -> Result<()> {
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
            bail!("ui.bindings.{key} must be one character or space, enter, tab, or esc");
        }
    }
    for action in ALL_ACTIONS {
        if !bound_suffixes.insert(ui.bindings.suffix(action)) {
            bail!("ui.bindings must not assign the same key to multiple actions");
        }
    }
    if !(4..=80).contains(&ui.workspace_sidebar.width) {
        bail!("ui.workspace_sidebar.width must be between 4 and 80");
    }
    if ui.tab_bar.item.min_width > 256 {
        bail!("ui.tab_bar.item.min_width must be at most 256");
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
    )?;
    for (name, segments, scope) in [
        ("header", &ui.workspace_sidebar.header, TokenScope::Sidebar),
        ("footer", &ui.workspace_sidebar.footer, TokenScope::Sidebar),
        (
            "row.left",
            &ui.workspace_sidebar.row.left,
            TokenScope::Workspace,
        ),
        (
            "row.body",
            &ui.workspace_sidebar.row.body,
            TokenScope::Workspace,
        ),
        (
            "row.right",
            &ui.workspace_sidebar.row.right,
            TokenScope::Workspace,
        ),
        (
            "row.detail",
            &ui.workspace_sidebar.row.detail,
            TokenScope::Workspace,
        ),
    ] {
        validate_segments(
            &format!("ui.workspace_sidebar.{name}"),
            segments,
            scope,
            false,
            &mut 0,
        )?;
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
            && !token_allowed(scope, token)
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

fn token_allowed(scope: TokenScope, token: &str) -> bool {
    match scope {
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
        ),
        TokenScope::Workspace => matches!(
            token,
            "workspace.marker"
                | "workspace.index"
                | "workspace.name"
                | "workspace.id"
                | "workspace.root"
                | "workspace.root_name"
                | "workspace.closing"
                | "workspace.tab_count"
                | "workspace.icon"
        ),
        TokenScope::Sidebar => matches!(
            token,
            "fut" | "session.name" | "workspace.name" | "workspace.icon" | "sidebar.status"
        ),
    }
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
    fn parses_complete_chrome_configuration_and_exact_colors() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        fs::write(
            &path,
            r##"
[ui]
pane_layout = "accordion"

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

[ui.workspace_sidebar]
position = "right"
width = 30
header = [{ token = "session.name" }]
footer = [{ token = "sidebar.status" }]

[ui.workspace_sidebar.row]
left = [{ token = "workspace.marker" }]
body = [{ token = "workspace.name" }]
right = [{ token = "workspace.tab_count" }]
"##,
        )
        .unwrap();
        let config = load_path(&path, true).unwrap();
        assert_eq!(config.pane_layout, PaneLayoutPolicy::Accordion);
        assert_eq!(config.tab_bar.position, TabBarPosition::Bottom);
        assert_eq!(
            config.workspace_sidebar.position,
            WorkspaceSidebarPosition::Right
        );
        assert_eq!(config.workspace_sidebar.width, 30);
        assert_eq!(config.tab_bar.item.min_width, 12);
        assert_eq!(config.icons.resolve().workspace, "W");
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
            "[ui.bindings]\nopen_command_bar = ':'\n\n[ui.tab_bar]\nposition = 'bottom'\n\n[ui.styles.current]\nforeground = 'red'\n\n[ui.styles.attention]\nforeground = 'blue'\n\n[ui.workspace_sidebar.row]\nbody = [{ text = 'custom' }]\n",
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
        assert_eq!(config.tab_bar.item.min_width, 12);
        assert!(config.tab_bar.left.iter().any(|group| {
            group
                .segments
                .iter()
                .any(|segment| segment.component.as_deref() == Some("tabs"))
        }));
        assert_eq!(config.styles.current.background, Some(UiColor::DarkGray));
        assert_eq!(
            config.styles.current.remove_modifiers,
            vec![ModifierName::Reversed, ModifierName::Underlined]
        );
        assert_eq!(config.styles.current.foreground, Some(UiColor::Red));
        assert_eq!(config.styles.attention.foreground, Some(UiColor::Blue));
        assert_eq!(
            config.workspace_sidebar.row.left,
            SidebarRowConfig::default().left
        );
        assert_eq!(
            config.workspace_sidebar.row.right,
            SidebarRowConfig::default().right
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
            "[ui]\nexecute = 'surprise'\n",
            "[ui.workspace_sidebar]\nwidth = 2\n",
            "[ui.tab_bar.item]\nmin_width = 257\n",
            "[ui.bindings]\nunknown = 'x'\n",
            "[ui.bindings]\nopen_command_bar = 'g'\n",
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
}
