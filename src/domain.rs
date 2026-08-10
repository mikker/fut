//! Fut-owned terminal state exchanged between the daemon and its clients.
//!
//! These types deliberately contain no PTY, terminal-emulator, transport, or
//! presentation-library types.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

/// Upper bound for the number of cells in a terminal's visible grid.
///
/// This is deliberately independent of the transport frame limit: validating
/// dimensions before constructing a grid prevents hostile `u16` dimensions
/// from driving multi-gigabyte allocations.
pub const MAX_VISIBLE_CELLS: usize = 50_000;
/// Maximum UTF-8 bytes retained for one serialized terminal cell.
///
/// Cell contents are also constrained to one control-free grapheme, and the
/// wire format (MessagePack) stores strings as raw length-prefixed bytes
/// with no escaping overhead. At 32 bytes, 50,000 fully styled and selected
/// cells still fit below the protocol's eight-MiB limit, while ordinary
/// emoji clusters remain exact. Pathological combining sequences are
/// represented by a fixed marker.
pub const MAX_CELL_CONTENT_BYTES: usize = 32;
pub const OVERSIZED_CELL_CONTENT_MARKER: &str = "�";
/// Maximum plain-text payload returned by a copy-mode request. At one MiB,
/// this remains well below the eight-MiB frame limit.
pub const MAX_COPY_BYTES: usize = 1024 * 1024;
/// Maximum physical cells traversed while formatting one selection.
///
/// This is deliberately separate from [`MAX_COPY_BYTES`]: a large blank
/// selection can produce very little text while still requiring substantial
/// emulator work.
pub const MAX_COPY_CELLS: usize = 250_000;
/// Maximum literal query accepted by the bounded on-demand scrollback scan.
pub const MAX_SEARCH_QUERY_BYTES: usize = 4 * 1024;
/// Maximum physical rows selected by terminal output inspection.
pub const MAX_TERMINAL_OUTPUT_ROWS: usize = 2_000;
/// Maximum cells traversed by one terminal output inspection.
pub const MAX_TERMINAL_OUTPUT_CELLS: usize = 250_000;
/// Maximum UTF-8/ANSI bytes returned by terminal output inspection.
pub const MAX_TERMINAL_OUTPUT_BYTES: usize = 1024 * 1024;
/// Maximum literal or regular-expression pattern accepted by output waits.
pub const MAX_TERMINAL_OUTPUT_PATTERN_BYTES: usize = 4 * 1024;
/// Maximum number of terminal cells inspected by one literal search. The
/// search map uses three `u32` values per retained cell, bounding its primary
/// temporary allocation to roughly three MiB.
pub const MAX_SEARCH_CELLS: usize = 250_000;
/// Maximum UTF-8 search material assembled from those cells.
pub const MAX_SEARCH_TEXT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum Unicode scalar values accepted from one terminal cell while
/// building search text. This bounds the temporary `Vec<char>` independently
/// of the UTF-8 text cap.
pub const MAX_SEARCH_CELL_CODEPOINTS: usize = 4 * 1024;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

id_type!(TerminalId);
id_type!(ClientId);
id_type!(SessionId);
id_type!(WorkspaceId);
id_type!(TabId);
id_type!(PaneId);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyModeMovement {
    Left,
    Down,
    Up,
    Right,
    BeginningOfLine,
    EndOfLine,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchDirection {
    Forward,
    Backward,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CopyModeAction {
    Begin,
    Move { movement: CopyModeMovement },
    ToggleSelection,
    Search { query: String },
    RepeatSearch { direction: SearchDirection },
    Copy,
    FinalizeCopy { copy_id: Uuid },
    Cancel,
}

#[derive(Clone, Debug, Error, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CopyModeError {
    #[error("copy mode is not active")]
    NotActive,
    #[error("copy mode is already active")]
    AlreadyActive,
    #[error("copy-mode cursor was discarded by the terminal")]
    CursorLost,
    #[error("search query is empty")]
    EmptySearchQuery,
    #[error("search query is {actual} bytes; maximum is {maximum}")]
    SearchQueryTooLarge { actual: usize, maximum: usize },
    #[error("search would inspect {actual} cells; maximum is {maximum}")]
    SearchSpaceTooLarge { actual: usize, maximum: usize },
    #[error("search text is at least {actual} bytes; maximum is {maximum}")]
    SearchTextTooLarge { actual: usize, maximum: usize },
    #[error("there is no previous search")]
    NoSearch,
    #[error("literal text was not found")]
    NoMatch,
    #[error("search cell has {actual} codepoints; maximum is {maximum}")]
    SearchCellTooLarge { actual: usize, maximum: usize },
    #[error("selection spans {actual} cells; maximum is {maximum}")]
    CopySpanTooLarge { actual: usize, maximum: usize },
    #[error("selected text is {actual} bytes; maximum is {maximum}")]
    CopyTooLarge { actual: usize, maximum: usize },
    #[error("copy confirmation does not match the prepared selection")]
    CopyConfirmationMismatch,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MouseModifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseWheelDirection {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MouseButtons {
    pub left: bool,
    pub middle: bool,
    pub right: bool,
}

impl MouseButtons {
    #[must_use]
    pub fn contains(self, button: MouseButton) -> bool {
        match button {
            MouseButton::Left => self.left,
            MouseButton::Middle => self.middle,
            MouseButton::Right => self.right,
        }
    }

    pub fn set(&mut self, button: MouseButton, pressed: bool) {
        match button {
            MouseButton::Left => self.left = pressed,
            MouseButton::Middle => self.middle = pressed,
            MouseButton::Right => self.right = pressed,
        }
    }

    #[must_use]
    pub fn any(self) -> bool {
        self.left || self.middle || self.right
    }
}

impl MouseButton {
    pub const ALL: [Self; 3] = [Self::Left, Self::Middle, Self::Right];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MouseEventKind {
    Press {
        button: MouseButton,
    },
    Release {
        button: MouseButton,
    },
    Motion {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<MouseButton>,
    },
    Wheel {
        direction: MouseWheelDirection,
    },
}

/// Mouse input normalized to zero-based terminal cell coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub column: u16,
    pub row: u16,
    #[serde(default)]
    pub modifiers: MouseModifiers,
    /// Button state after this event. Ghostty uses this to distinguish a
    /// requested drag from unbuttoned motion.
    #[serde(default)]
    pub buttons: MouseButtons,
}

impl MouseEvent {
    /// Validate the post-event button snapshot supplied by an untrusted client.
    pub fn validate(self) -> Result<(), &'static str> {
        match self.kind {
            MouseEventKind::Press { button } if !self.buttons.contains(button) => {
                Err("pressed button is absent from the post-event button snapshot")
            }
            MouseEventKind::Release { button } if self.buttons.contains(button) => {
                Err("released button remains in the post-event button snapshot")
            }
            MouseEventKind::Motion {
                button: Some(button),
            } if !self.buttons.contains(button) => {
                Err("drag button is absent from the button snapshot")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    #[default]
    Idle,
    Working,
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentReport {
    Idle,
    Working,
    Blocked,
    Completed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutputSource {
    #[default]
    Visible,
    Recent,
    RecentUnwrapped,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum TerminalOutputMatcher {
    Literal(String),
    Regex(String),
}

impl TerminalOutputMatcher {
    #[must_use]
    pub fn value(&self) -> &str {
        match self {
            Self::Literal(value) | Self::Regex(value) => value,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalOutput {
    pub version: u8,
    pub terminal_id: TerminalId,
    pub revision: u64,
    pub source: TerminalOutputSource,
    pub requested_rows: usize,
    pub returned_rows: usize,
    pub truncated: bool,
    pub starts_mid_logical_line: bool,
    pub ansi: bool,
    pub text: String,
}

/// Optional identity supplied by an agent integration when reporting activity.
///
/// These values are descriptive correlation hints, not synchronization keys.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentReportMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

pub const MAX_AGENT_METADATA_VALUE_BYTES: usize = 256;
/// Prompts are deliberately smaller than the local protocol frame so malformed
/// callers cannot monopolize the terminal input queue with one message.
pub const MAX_AGENT_PROMPT_BYTES: usize = 1024 * 1024;

impl AgentReportMetadata {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.source.is_none() && self.agent_session_id.is_none() && self.turn_id.is_none()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if [&self.source, &self.agent_session_id, &self.turn_id]
            .into_iter()
            .flatten()
            .any(|value| value.len() > MAX_AGENT_METADATA_VALUE_BYTES)
        {
            return Err("agent report metadata value is too long");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentIntegration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub revision: u64,
    pub kind: AgentReport,
    pub occurred_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    Blocked,
    Completed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentAttention {
    pub revision: u64,
    pub kind: AttentionKind,
    pub occurred_at_ms: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct AgentActivity {
    /// Presence means this terminal has reported through an agent integration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<AgentIntegration>,
    pub state: AgentState,
    pub revision: u64,
    pub updated_at_ms: u64,
    /// The most recent lifecycle report, including completion events which map
    /// back to the current idle state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<AgentEvent>,
}

impl<'de> Deserialize<'de> for AgentActivity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireActivity {
            #[serde(default)]
            integration: Option<AgentIntegration>,
            #[serde(default)]
            state: AgentState,
            #[serde(default)]
            revision: u64,
            #[serde(default)]
            updated_at_ms: u64,
            #[serde(default)]
            last_event: Option<AgentEvent>,
            #[serde(default)]
            attention: Option<AgentAttention>,
        }

        let wire = WireActivity::deserialize(deserializer)?;
        let had_legacy_report =
            wire.revision != 0 || wire.state != AgentState::Idle || wire.attention.is_some();
        let has_report = had_legacy_report || wire.last_event.is_some();
        let integration = wire
            .integration
            .or_else(|| has_report.then(AgentIntegration::default));
        let last_event = wire.last_event.or_else(|| {
            had_legacy_report.then(|| {
                let kind = match wire.attention.filter(|a| a.revision == wire.revision) {
                    Some(AgentAttention {
                        kind: AttentionKind::Completed,
                        ..
                    }) => AgentReport::Completed,
                    Some(AgentAttention {
                        kind: AttentionKind::Blocked,
                        ..
                    }) => AgentReport::Blocked,
                    None => match wire.state {
                        AgentState::Idle => AgentReport::Idle,
                        AgentState::Working => AgentReport::Working,
                        AgentState::Blocked => AgentReport::Blocked,
                    },
                };
                AgentEvent {
                    revision: wire.revision,
                    kind,
                    occurred_at_ms: wire.updated_at_ms,
                    turn_id: None,
                }
            })
        });
        Ok(Self {
            integration,
            state: wire.state,
            revision: wire.revision,
            updated_at_ms: wire.updated_at_ms,
            last_event,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalSize {
    #[serde(rename = "c")]
    pub columns: u16,
    #[serde(rename = "r")]
    pub rows: u16,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TerminalSizeError {
    #[error("terminal dimensions must be non-zero")]
    Empty,
    #[error("terminal dimensions require {actual} visible cells; maximum is {maximum}")]
    TooLarge { actual: usize, maximum: usize },
}

impl TerminalSize {
    pub fn validate(self) -> Result<(), TerminalSizeError> {
        self.cell_count().map(|_| ())
    }

    pub fn cell_count(self) -> Result<usize, TerminalSizeError> {
        if self.columns == 0 || self.rows == 0 {
            return Err(TerminalSizeError::Empty);
        }

        let count = usize::from(self.columns) * usize::from(self.rows);
        if count > MAX_VISIBLE_CELLS {
            return Err(TerminalSizeError::TooLarge {
                actual: count,
                maximum: MAX_VISIBLE_CELLS,
            });
        }
        Ok(count)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Rgb {
    #[serde(rename = "r")]
    pub red: u8,
    #[serde(rename = "g")]
    pub green: u8,
    #[serde(rename = "b")]
    pub blue: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CellColor {
    Indexed(u8),
    Rgb(Rgb),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CellStyle {
    pub foreground: Option<CellColor>,
    pub background: Option<CellColor>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Wire tags packed into a color's top byte, keeping "no color" (0) distinct
/// from `Indexed(0)`.
const COLOR_TAG_INDEXED: u32 = 0x0100_0000;
const COLOR_TAG_RGB: u32 = 0x0200_0000;

const FLAG_BOLD: u8 = 1 << 0;
const FLAG_ITALIC: u8 = 1 << 1;
const FLAG_UNDERLINE: u8 = 1 << 2;
const FLAG_INVERSE: u8 = 1 << 3;

fn pack_color(color: Option<CellColor>) -> u32 {
    match color {
        None => 0,
        Some(CellColor::Indexed(index)) => COLOR_TAG_INDEXED | u32::from(index),
        Some(CellColor::Rgb(Rgb { red, green, blue })) => {
            COLOR_TAG_RGB | (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
        }
    }
}

fn unpack_color(packed: u32) -> Option<CellColor> {
    match packed & 0xff00_0000 {
        0 => None,
        COLOR_TAG_INDEXED => Some(CellColor::Indexed(packed as u8)),
        _ => Some(CellColor::Rgb(Rgb {
            red: (packed >> 16) as u8,
            green: (packed >> 8) as u8,
            blue: packed as u8,
        })),
    }
}

impl CellStyle {
    fn pack_flags(self) -> u8 {
        let mut flags = 0;
        if self.bold {
            flags |= FLAG_BOLD;
        }
        if self.italic {
            flags |= FLAG_ITALIC;
        }
        if self.underline {
            flags |= FLAG_UNDERLINE;
        }
        if self.inverse {
            flags |= FLAG_INVERSE;
        }
        flags
    }

    fn unpack_flags(flags: u8) -> (bool, bool, bool, bool) {
        (
            flags & FLAG_BOLD != 0,
            flags & FLAG_ITALIC != 0,
            flags & FLAG_UNDERLINE != 0,
            flags & FLAG_INVERSE != 0,
        )
    }
}

/// Wire form of [`CellStyle`]: a flat `[fg, bg, flags]` array instead of a
/// field-name map, avoiding per-cell key strings and nested color objects
/// for the common case of a fully (and often uniquely) styled frame.
impl Serialize for CellStyle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;

        let mut tuple = serializer.serialize_tuple(3)?;
        tuple.serialize_element(&pack_color(self.foreground))?;
        tuple.serialize_element(&pack_color(self.background))?;
        tuple.serialize_element(&self.pack_flags())?;
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for CellStyle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CellStyleVisitor;

        impl<'de> serde::de::Visitor<'de> for CellStyleVisitor {
            type Value = CellStyle;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a [fg, bg, flags] array")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let foreground: u32 = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                let background: u32 = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;
                let flags: u8 = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(2, &self))?;
                let (bold, italic, underline, inverse) = CellStyle::unpack_flags(flags);
                Ok(CellStyle {
                    foreground: unpack_color(foreground),
                    background: unpack_color(background),
                    bold,
                    italic,
                    underline,
                    inverse,
                })
            }
        }

        deserializer.deserialize_tuple(3, CellStyleVisitor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    #[serde(rename = "c")]
    pub contents: String,
    #[serde(rename = "s", default, skip_serializing_if = "CellStyle::is_default")]
    pub style: CellStyle,
    #[serde(rename = "x", default, skip_serializing_if = "is_false")]
    pub selected: bool,
}

impl CellStyle {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            contents: " ".into(),
            style: CellStyle::default(),
            selected: false,
        }
    }
}

impl Cell {
    fn bound_contents(&mut self) {
        if self.contents.len() > MAX_CELL_CONTENT_BYTES
            || self.contents.chars().any(char::is_control)
            || self.contents.graphemes(true).count() > 1
        {
            self.contents = OVERSIZED_CELL_CONTENT_MARKER.into();
        }
    }

    /// Cheap guard for cells already known to be a single trusted grapheme
    /// cluster with no control characters (built straight from libghostty
    /// state). libghostty never stores control characters as cell content
    /// and `graphemes_utf8` always yields one grapheme cluster per cell, so
    /// only the byte-length cap from [`Self::bound_contents`] still applies:
    /// libghostty can in principle attach an unbounded run of combining
    /// codepoints to a single base character, so length is the one thing
    /// still worth capping here.
    pub(crate) fn cap_length(&mut self) {
        if self.contents.len() > MAX_CELL_CONTENT_BYTES {
            self.contents = OVERSIZED_CELL_CONTENT_MARKER.into();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(rename = "c")]
    pub column: u16,
    #[serde(rename = "r")]
    pub row: u16,
    #[serde(rename = "v")]
    pub visible: bool,
}

/// Where the snapshot's viewport sits within the terminal's scrollback.
/// `max_offset_from_bottom` of zero means no scrollback exists yet.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScrollPosition {
    #[serde(rename = "b")]
    pub offset_from_bottom: usize,
    #[serde(rename = "m")]
    pub max_offset_from_bottom: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    #[serde(rename = "r")]
    pub revision: u64,
    #[serde(rename = "s")]
    pub size: TerminalSize,
    #[serde(rename = "c")]
    pub cells: Vec<Cell>,
    #[serde(rename = "p")]
    pub cursor: Cursor,
    // Defaulted so snapshots from daemons predating scroll metrics decode.
    #[serde(rename = "v", default)]
    pub scroll: ScrollPosition,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SnapshotError {
    #[error(transparent)]
    InvalidSize(#[from] TerminalSizeError),
    #[error("snapshot has {actual} cells but its dimensions require {expected}")]
    CellCount { expected: usize, actual: usize },
    #[error("cursor ({column}, {row}) is outside the snapshot")]
    CursorOutOfBounds { column: u16, row: u16 },
}

impl ScreenSnapshot {
    pub fn new(
        revision: u64,
        size: TerminalSize,
        mut cells: Vec<Cell>,
        cursor: Cursor,
    ) -> Result<Self, SnapshotError> {
        Self::check_bounds(size, &cells, cursor)?;
        for cell in &mut cells {
            cell.bound_contents();
        }
        Ok(Self::new_unchecked(revision, size, cells, cursor))
    }

    /// Build a snapshot from cells the caller has already produced in a
    /// trusted, well-formed shape (e.g. straight from libghostty state).
    /// Size and cursor bounds are still checked, and each cell still gets
    /// the cheap byte-length cap (see [`Cell::cap_length`]), but the
    /// expensive control-character scan and grapheme-cluster count that
    /// [`Self::new`] performs for untrusted callers are skipped.
    ///
    /// Only use this for cells that are known by construction to already
    /// be a single grapheme cluster with no control characters.
    pub(crate) fn from_terminal(
        revision: u64,
        size: TerminalSize,
        mut cells: Vec<Cell>,
        cursor: Cursor,
    ) -> Result<Self, SnapshotError> {
        Self::check_bounds(size, &cells, cursor)?;
        for cell in &mut cells {
            cell.cap_length();
        }
        Ok(Self::new_unchecked(revision, size, cells, cursor))
    }

    fn check_bounds(
        size: TerminalSize,
        cells: &[Cell],
        cursor: Cursor,
    ) -> Result<(), SnapshotError> {
        let expected = size.cell_count()?;
        if cells.len() != expected {
            return Err(SnapshotError::CellCount {
                expected,
                actual: cells.len(),
            });
        }
        if cursor.column >= size.columns || cursor.row >= size.rows {
            return Err(SnapshotError::CursorOutOfBounds {
                column: cursor.column,
                row: cursor.row,
            });
        }
        Ok(())
    }

    fn new_unchecked(revision: u64, size: TerminalSize, cells: Vec<Cell>, cursor: Cursor) -> Self {
        Self {
            revision,
            size,
            cells,
            cursor,
            scroll: ScrollPosition::default(),
        }
    }
}

/// One changed row of a [`ScreenDelta`]: its index within the grid plus a
/// full replacement row of cells.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeltaRow {
    #[serde(rename = "i")]
    pub index: u16,
    #[serde(rename = "c")]
    pub cells: Vec<Cell>,
}

/// Rows that changed since `base_revision`, in place of a full
/// [`ScreenSnapshot`]. Only valid against a grid the receiver already has at
/// exactly `base_revision` and `size`; anything else and the receiver must
/// fall back to requesting a full snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScreenDelta {
    #[serde(rename = "r")]
    pub revision: u64,
    #[serde(rename = "b")]
    pub base_revision: u64,
    #[serde(rename = "s")]
    pub size: TerminalSize,
    #[serde(rename = "d")]
    pub rows: Vec<DeltaRow>,
    #[serde(rename = "p")]
    pub cursor: Cursor,
    #[serde(rename = "v")]
    pub scroll: ScrollPosition,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_activity_distinguishes_unintegrated_idle_from_integrated_idle() {
        let unintegrated = AgentActivity::default();
        assert_eq!(unintegrated.state, AgentState::Idle);
        assert_eq!(unintegrated.integration, None);
        assert_eq!(unintegrated.last_event, None);

        let integrated = AgentActivity {
            integration: Some(AgentIntegration::default()),
            last_event: Some(AgentEvent {
                revision: 4,
                kind: AgentReport::Idle,
                occurred_at_ms: 12,
                turn_id: None,
            }),
            revision: 4,
            updated_at_ms: 12,
            ..AgentActivity::default()
        };
        assert_eq!(integrated.state, AgentState::Idle);
        assert!(integrated.integration.is_some());
    }

    #[test]
    fn legacy_agent_activity_infers_presence_and_latest_event() {
        let working: AgentActivity =
            serde_json::from_str(r#"{"state":"working","revision":7,"updated_at_ms":20}"#).unwrap();
        assert!(working.integration.is_some());
        assert_eq!(working.last_event.unwrap().kind, AgentReport::Working);

        let completed: AgentActivity = serde_json::from_str(
            r#"{"state":"idle","revision":8,"updated_at_ms":30,"attention":{"revision":8,"kind":"completed","occurred_at_ms":30}}"#,
        )
        .unwrap();
        assert!(completed.integration.is_some());
        assert_eq!(
            completed.last_event.as_ref().unwrap().kind,
            AgentReport::Completed
        );

        let serialized = serde_json::to_value(&completed).unwrap();
        assert!(serialized.get("attention").is_none());
        assert_eq!(serialized["last_event"]["kind"], "completed");

        let never_reported: AgentActivity =
            serde_json::from_str(r#"{"state":"idle","revision":0,"updated_at_ms":0}"#).unwrap();
        assert_eq!(never_reported.integration, None);
        assert_eq!(never_reported.last_event, None);
    }

    #[test]
    fn agent_report_metadata_is_bounded_per_value() {
        let accepted = AgentReportMetadata {
            source: Some("x".repeat(MAX_AGENT_METADATA_VALUE_BYTES)),
            ..AgentReportMetadata::default()
        };
        assert_eq!(accepted.validate(), Ok(()));
        let rejected = AgentReportMetadata {
            turn_id: Some("x".repeat(MAX_AGENT_METADATA_VALUE_BYTES + 1)),
            ..AgentReportMetadata::default()
        };
        assert_eq!(
            rejected.validate(),
            Err("agent report metadata value is too long")
        );
    }

    #[test]
    fn ids_are_opaque_unique_and_serde_round_trip() {
        let first = TerminalId::new();
        let second = TerminalId::new();
        assert_ne!(first, second);

        let json = serde_json::to_string(&first).unwrap();
        assert_eq!(serde_json::from_str::<TerminalId>(&json).unwrap(), first);
        assert_eq!(first.to_string().len(), 36);
    }

    #[test]
    fn snapshot_accepts_a_row_major_grid() {
        let snapshot = ScreenSnapshot::new(
            7,
            TerminalSize {
                columns: 2,
                rows: 2,
            },
            vec![Cell::default(); 4],
            Cursor {
                column: 1,
                row: 1,
                visible: true,
            },
        )
        .unwrap();

        assert_eq!(snapshot.revision, 7);
        assert_eq!(snapshot.cells.len(), 4);
    }

    #[test]
    fn snapshot_preserves_normal_graphemes_and_marks_overlong_cell_content() {
        let normal = ["a", "é", "雪", "😀", "👍🏽", "👨‍👩‍👧‍👦"];
        let mut cells = normal
            .iter()
            .map(|contents| Cell {
                contents: (*contents).into(),
                ..Cell::default()
            })
            .collect::<Vec<_>>();
        cells.push(Cell {
            contents: format!("a{}", "\u{301}".repeat(MAX_CELL_CONTENT_BYTES)),
            ..Cell::default()
        });
        let snapshot = ScreenSnapshot::new(
            1,
            TerminalSize {
                columns: cells.len() as u16,
                rows: 1,
            },
            cells,
            Cursor {
                column: 0,
                row: 0,
                visible: true,
            },
        )
        .unwrap();

        assert_eq!(
            snapshot.cells[..normal.len()]
                .iter()
                .map(|cell| cell.contents.as_str())
                .collect::<Vec<_>>(),
            normal
        );
        assert_eq!(
            snapshot.cells.last().unwrap().contents,
            OVERSIZED_CELL_CONTENT_MARKER
        );
        assert!(
            snapshot
                .cells
                .iter()
                .all(|cell| cell.contents.len() <= MAX_CELL_CONTENT_BYTES)
        );
    }

    #[test]
    fn snapshot_rejects_invalid_dimensions_cells_and_cursor() {
        let cursor = Cursor {
            column: 0,
            row: 0,
            visible: true,
        };
        assert_eq!(
            ScreenSnapshot::new(
                0,
                TerminalSize {
                    columns: 0,
                    rows: 1
                },
                vec![],
                cursor
            ),
            Err(SnapshotError::InvalidSize(TerminalSizeError::Empty))
        );
        assert!(matches!(
            ScreenSnapshot::new(
                0,
                TerminalSize {
                    columns: 2,
                    rows: 1
                },
                vec![Cell::default()],
                cursor
            ),
            Err(SnapshotError::CellCount { .. })
        ));
        assert!(matches!(
            ScreenSnapshot::new(
                0,
                TerminalSize {
                    columns: 1,
                    rows: 1
                },
                vec![Cell::default()],
                Cursor {
                    column: 1,
                    ..cursor
                }
            ),
            Err(SnapshotError::CursorOutOfBounds { .. })
        ));
    }

    #[test]
    fn snapshot_rejects_excessive_dimensions_before_cell_validation() {
        let size = TerminalSize {
            columns: u16::MAX,
            rows: u16::MAX,
        };
        assert!(matches!(
            ScreenSnapshot::new(
                0,
                size,
                Vec::new(),
                Cursor {
                    column: 0,
                    row: 0,
                    visible: false,
                },
            ),
            Err(SnapshotError::InvalidSize(TerminalSizeError::TooLarge {
                maximum: MAX_VISIBLE_CELLS,
                ..
            }))
        ));
    }

    #[test]
    fn compact_cell_style_round_trips_non_defaults() {
        let cell = Cell {
            contents: "λ".into(),
            style: CellStyle {
                foreground: Some(CellColor::Indexed(1)),
                background: Some(CellColor::Rgb(Rgb {
                    red: 250,
                    green: 240,
                    blue: 230,
                })),
                bold: true,
                italic: true,
                underline: true,
                inverse: true,
            },
            selected: true,
        };
        // Pins CellStyle's Serialize/Deserialize shape (format-agnostic: the
        // wire protocol itself uses MessagePack, see
        // protocol::tests::wire_pins_a_styled_cells_exact_messagepack_bytes).
        // Style is a flat [fg, bg, flags] array: fg packs the indexed-color
        // tag (0x0100_0000) with the palette index, bg packs the RGB tag
        // (0x0200_0000) with the r/g/b bytes, and flags is a bitfield
        // (bold|italic|underline|inverse = 0b1111).
        let json = serde_json::to_string(&cell).unwrap();
        assert_eq!(serde_json::from_str::<Cell>(&json).unwrap(), cell);
        assert_eq!(json, r#"{"c":"λ","s":[16777217,50000102,15],"x":true}"#);
        assert_eq!(
            serde_json::to_string(&Cell::default()).unwrap(),
            r#"{"c":" "}"#
        );
    }
}
