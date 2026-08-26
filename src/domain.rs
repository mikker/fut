//! Fut-owned terminal state exchanged between the daemon and its clients.
//!
//! These types deliberately contain no PTY, terminal-emulator, transport, or
//! presentation-library types.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

/// Length of Fut's complete, URL-safe representation of a 128-bit resource ID.
pub const COMPACT_ID_LEN: usize = 23;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("expected a UUID or {COMPACT_ID_LEN}-character compact Fut ID")]
pub struct IdParseError;

#[must_use]
pub fn compact_id(id: Uuid) -> String {
    format!("f{}", URL_SAFE_NO_PAD.encode(id.as_bytes()))
}

pub fn parse_id(value: &str) -> Result<Uuid, IdParseError> {
    if let Ok(id) = Uuid::parse_str(value) {
        return Ok(id);
    }
    if value.len() != COMPACT_ID_LEN {
        return Err(IdParseError);
    }
    let encoded = value.strip_prefix('f').ok_or(IdParseError)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| IdParseError)?;
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| IdParseError)?;
    let id = Uuid::from_bytes(bytes);
    (compact_id(id) == value).then_some(id).ok_or(IdParseError)
}

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
/// Maximum UTF-8 bytes retained for one explicit OSC 8 hyperlink URI.
pub const MAX_HYPERLINK_URI_BYTES: usize = 4 * 1024;
/// Maximum unique hyperlink URI bytes retained in one visible snapshot.
pub const MAX_SCREEN_HYPERLINK_BYTES: usize = 512 * 1024;
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

            /// The canonical UUID retained on the wire and in structured output.
            #[must_use]
            pub fn uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&compact_id(self.0))
            }
        }

        impl std::str::FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_id(value).map(Self)
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
id_type!(SplitId);

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

/// A physical cursor key whose bytes must be chosen from terminal-owned modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalKeyCode {
    Up,
    Down,
    Right,
    Left,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalKeyModifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalKeyEvent {
    pub code: TerminalKeyCode,
    #[serde(default)]
    pub modifiers: TerminalKeyModifiers,
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
    BeginSelection { column: u16, row: u16 },
    SetSelectionEnd { column: u16, row: u16 },
    Move { movement: CopyModeMovement },
    ToggleSelection,
    Search { query: String },
    RepeatSearch { direction: SearchDirection },
    Copy,
    FinalizeCopy { copy_id: Uuid },
    Cancel,
}

impl CopyModeAction {
    pub(crate) fn begins(&self) -> bool {
        matches!(self, Self::Begin | Self::BeginSelection { .. })
    }
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
    #[error(
        "copy-mode position ({column}, {row}) is outside the {columns}x{rows} terminal viewport"
    )]
    PositionOutOfBounds {
        column: u16,
        row: u16,
        columns: u16,
        rows: u16,
    },
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
    Exited,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentIntegration {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
}

impl Default for AgentIntegration {
    fn default() -> Self {
        Self {
            active: true,
            source: None,
            agent_session_id: None,
        }
    }
}

const fn default_true() -> bool {
    true
}

const fn is_true(value: &bool) -> bool {
    *value
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentDetection {
    pub agent: String,
    pub rule: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct AgentActivity {
    /// Presence means this terminal has reported through an agent integration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<AgentIntegration>,
    /// Presence means Fut inferred this activity from the live terminal screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<AgentDetection>,
    pub state: AgentState,
    pub revision: u64,
    pub updated_at_ms: u64,
    /// The most recent lifecycle report, including completion events which map
    /// back to the current idle state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<AgentEvent>,
    /// Latest attention event observed by any client. Read status belongs to
    /// the daemon so every attached client and external status reader agrees.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub read_revision: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl AgentActivity {
    /// Whether an integration currently owns this terminal. Exited sessions
    /// retain inactive integration metadata independently from screen-inferred
    /// events, and a later explicit report reactivates the terminal.
    pub fn has_active_integration(&self) -> bool {
        self.integration
            .as_ref()
            .is_some_and(|integration| integration.active)
    }

    pub fn attention(&self) -> Option<AgentAttention> {
        let event = self.last_event.as_ref()?;
        let kind = match event.kind {
            AgentReport::Blocked => AttentionKind::Blocked,
            AgentReport::Completed => AttentionKind::Completed,
            AgentReport::Idle | AgentReport::Working | AgentReport::Exited => return None,
        };
        Some(AgentAttention {
            revision: event.revision,
            kind,
            occurred_at_ms: event.occurred_at_ms,
        })
    }

    pub fn has_unread_attention(&self) -> bool {
        self.attention()
            .is_some_and(|attention| self.read_revision < attention.revision)
    }
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
            detection: Option<AgentDetection>,
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
            #[serde(default)]
            read_revision: u64,
        }

        let wire = WireActivity::deserialize(deserializer)?;
        let had_legacy_report =
            wire.revision != 0 || wire.state != AgentState::Idle || wire.attention.is_some();
        let has_report =
            (had_legacy_report && wire.detection.is_none()) || wire.last_event.is_some();
        let integration = wire
            .integration
            .or_else(|| has_report.then(AgentIntegration::default));
        let last_event = wire.last_event.or_else(|| {
            (had_legacy_report && wire.detection.is_none()).then(|| {
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
            detection: wire.detection,
            state: wire.state,
            revision: wire.revision,
            updated_at_ms: wire.updated_at_ms,
            last_event,
            read_revision: wire.read_revision,
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

/// Wire tags packed into a color's top byte, keeping "no color" (0) distinct
/// from `Indexed(0)`.
const COLOR_TAG_INDEXED: u32 = 0x0100_0000;
const COLOR_TAG_RGB: u32 = 0x0200_0000;

const FLAG_BOLD: u8 = 1 << 0;
const FLAG_ITALIC: u8 = 1 << 1;
const FLAG_UNDERLINE: u8 = 1 << 2;
const FLAG_INVERSE: u8 = 1 << 3;
const PACKED_COLOR_BITS: u32 = 26;
const PACKED_COLOR_MASK: u64 = (1 << PACKED_COLOR_BITS) - 1;

/// Compact in-memory and wire representation of terminal cell colors and
/// modifiers. Both 26-bit colors and four flags fit losslessly in one word.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CellStyle(u64);

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
    #[must_use]
    pub fn new(
        foreground: Option<CellColor>,
        background: Option<CellColor>,
        bold: bool,
        italic: bool,
        underline: bool,
        inverse: bool,
    ) -> Self {
        let mut flags = 0;
        if bold {
            flags |= FLAG_BOLD;
        }
        if italic {
            flags |= FLAG_ITALIC;
        }
        if underline {
            flags |= FLAG_UNDERLINE;
        }
        if inverse {
            flags |= FLAG_INVERSE;
        }
        Self(
            u64::from(pack_color(foreground))
                | (u64::from(pack_color(background)) << PACKED_COLOR_BITS)
                | (u64::from(flags) << (PACKED_COLOR_BITS * 2)),
        )
    }

    #[must_use]
    pub fn foreground(self) -> Option<CellColor> {
        unpack_color((self.0 & PACKED_COLOR_MASK) as u32)
    }

    #[must_use]
    pub fn background(self) -> Option<CellColor> {
        unpack_color(((self.0 >> PACKED_COLOR_BITS) & PACKED_COLOR_MASK) as u32)
    }

    #[must_use]
    pub fn bold(self) -> bool {
        self.flags() & FLAG_BOLD != 0
    }

    #[must_use]
    pub fn italic(self) -> bool {
        self.flags() & FLAG_ITALIC != 0
    }

    #[must_use]
    pub fn underline(self) -> bool {
        self.flags() & FLAG_UNDERLINE != 0
    }

    #[must_use]
    pub fn inverse(self) -> bool {
        self.flags() & FLAG_INVERSE != 0
    }

    fn flags(self) -> u8 {
        (self.0 >> (PACKED_COLOR_BITS * 2)) as u8
    }
}

/// Wire form of [`CellStyle`]: a flat `[fg, bg, flags]` array instead of a
/// field-name map, avoiding per-cell key strings and nested color objects
/// for the common case of a fully (and often uniquely) styled frame.
impl Serialize for CellStyle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;

        let mut tuple = serializer.serialize_tuple(3)?;
        tuple.serialize_element(&pack_color(self.foreground()))?;
        tuple.serialize_element(&pack_color(self.background()))?;
        tuple.serialize_element(&self.flags())?;
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
                Ok(CellStyle::new(
                    unpack_color(foreground),
                    unpack_color(background),
                    flags & FLAG_BOLD != 0,
                    flags & FLAG_ITALIC != 0,
                    flags & FLAG_UNDERLINE != 0,
                    flags & FLAG_INVERSE != 0,
                ))
            }
        }

        deserializer.deserialize_tuple(3, CellStyleVisitor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    pub contents: CompactString,
    pub style: CellStyle,
    pub selected: bool,
    /// Index into [`ScreenSnapshot::hyperlinks`].
    pub hyperlink: Option<u16>,
}

/// Cells dominate terminal frames, so their wire form avoids maps and nested
/// containers. Plain unselected cells are encoded as just their string;
/// styled cells are `[contents, packed_style]`, with a trailing `true` only
/// for selected cells. Hyperlinked cells append `selected, hyperlink_index`.
/// Both 26-bit colors and four flags fit in one `u64`.
impl Serialize for Cell {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.style.is_default() && !self.selected && self.hyperlink.is_none() {
            return serializer.serialize_str(&self.contents);
        }
        use serde::ser::SerializeTuple;
        let len = if self.hyperlink.is_some() {
            4
        } else if self.selected {
            3
        } else {
            2
        };
        let mut tuple = serializer.serialize_tuple(len)?;
        tuple.serialize_element(&self.contents)?;
        tuple.serialize_element(&self.style.0)?;
        if self.selected || self.hyperlink.is_some() {
            tuple.serialize_element(&self.selected)?;
        }
        if let Some(hyperlink) = self.hyperlink {
            tuple.serialize_element(&hyperlink)?;
        }
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for Cell {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CellVisitor;
        impl<'de> serde::de::Visitor<'de> for CellVisitor {
            type Value = Cell;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a cell string or [contents, packed_style, selected] array")
            }
            fn visit_borrowed_str<E: serde::de::Error>(self, value: &'de str) -> Result<Cell, E> {
                Ok(Cell {
                    contents: CompactString::new(value),
                    ..Cell::default()
                })
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Cell, E> {
                Ok(Cell {
                    contents: CompactString::new(value),
                    ..Cell::default()
                })
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Cell, E> {
                Ok(Cell {
                    contents: value.into(),
                    ..Cell::default()
                })
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Cell, A::Error> {
                let contents = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                let packed_style: u64 = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;
                let selected = seq.next_element()?.unwrap_or(false);
                let hyperlink = seq.next_element()?;
                Ok(Cell {
                    contents,
                    style: CellStyle(packed_style),
                    selected,
                    hyperlink,
                })
            }
        }
        deserializer.deserialize_any(CellVisitor)
    }
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
            hyperlink: None,
        }
    }
}

impl Cell {
    fn bound_contents(&mut self) {
        if self.contents.len() > MAX_CELL_CONTENT_BYTES
            || self.contents.chars().any(char::is_control)
            || self.contents.graphemes(true).count() > 1
        {
            self.contents = CompactString::new(OVERSIZED_CELL_CONTENT_MARKER);
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
            self.contents = CompactString::new(OVERSIZED_CELL_CONTENT_MARKER);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    Bar,
    Underline,
    /// Unknown future shapes and unsupported emulator-specific shapes render
    /// as the universally representable block cursor.
    #[default]
    #[serde(other)]
    Block,
}

impl CursorShape {
    fn is_block(&self) -> bool {
        *self == Self::Block
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
    // Named MessagePack fields are additive: old clients ignore these keys,
    // while their defaults let new clients read snapshots from old daemons.
    #[serde(rename = "s", default, skip_serializing_if = "CursorShape::is_block")]
    pub shape: CursorShape,
    #[serde(rename = "b", default, skip_serializing_if = "std::ops::Not::not")]
    pub blinking: bool,
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

/// A decoded Kitty image, recompressed as PNG for transport to attached
/// clients. Image generations are process-wide libghostty stamps and let a
/// client avoid retransmitting unchanged pixels to its host terminal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KittyImage {
    #[serde(rename = "i")]
    pub id: u32,
    #[serde(rename = "g")]
    pub generation: u64,
    #[serde(rename = "d", with = "serde_bytes")]
    pub png: Vec<u8>,
}

/// One visible, non-placeholder Kitty placement in terminal-grid space.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KittyPlacement {
    #[serde(rename = "i")]
    pub image_id: u32,
    #[serde(rename = "p")]
    pub placement_id: u32,
    #[serde(rename = "c")]
    pub column: i32,
    #[serde(rename = "r")]
    pub row: i32,
    #[serde(rename = "w")]
    pub columns: u32,
    #[serde(rename = "h")]
    pub rows: u32,
    #[serde(rename = "x")]
    pub source_x: u32,
    #[serde(rename = "y")]
    pub source_y: u32,
    #[serde(rename = "s")]
    pub source_width: u32,
    #[serde(rename = "t")]
    pub source_height: u32,
    #[serde(rename = "z")]
    pub z: i32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct KittyGraphics {
    #[serde(rename = "i")]
    pub images: Vec<KittyImage>,
    #[serde(rename = "p")]
    pub placements: Vec<KittyPlacement>,
}

impl KittyGraphics {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    #[serde(rename = "r")]
    pub revision: u64,
    #[serde(rename = "s")]
    pub size: TerminalSize,
    #[serde(rename = "c")]
    pub cells: Vec<Cell>,
    /// Deduplicated OSC 8 hyperlink URIs referenced by cells.
    #[serde(rename = "h", default, skip_serializing_if = "Vec::is_empty")]
    pub hyperlinks: Vec<CompactString>,
    #[serde(rename = "p")]
    pub cursor: Cursor,
    // Defaulted so snapshots from daemons predating scroll metrics decode.
    #[serde(rename = "v", default)]
    pub scroll: ScrollPosition,
    /// Whether the pane application currently owns mouse button gestures.
    #[serde(rename = "m", default, skip_serializing_if = "std::ops::Not::not")]
    pub mouse_tracking: bool,
    #[serde(rename = "g", default, skip_serializing_if = "KittyGraphics::is_empty")]
    pub graphics: KittyGraphics,
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
        hyperlinks: Vec<CompactString>,
        cursor: Cursor,
    ) -> Result<Self, SnapshotError> {
        Self::check_bounds(size, &cells, cursor)?;
        for cell in &mut cells {
            cell.cap_length();
        }
        let mut snapshot = Self::new_unchecked(revision, size, cells, cursor);
        snapshot.hyperlinks = hyperlinks;
        Ok(snapshot)
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
            hyperlinks: Vec::new(),
            cursor,
            scroll: ScrollPosition::default(),
            mouse_tracking: false,
            graphics: KittyGraphics::default(),
        }
    }

    /// Atomically applies a delta produced from this exact snapshot.
    ///
    /// All of `delta` is checked before this snapshot is changed. A rejected
    /// delta therefore leaves the receiver usable as the base for a later
    /// refresh or a correctly sequenced delta.
    pub fn apply_delta(&mut self, delta: ScreenDelta) -> Result<(), ScreenDeltaError> {
        if delta.base_revision != self.revision {
            return Err(ScreenDeltaError::BaseRevisionMismatch {
                expected: self.revision,
                actual: delta.base_revision,
            });
        }
        if delta.size != self.size {
            return Err(ScreenDeltaError::SizeMismatch {
                expected: self.size,
                actual: delta.size,
            });
        }
        if delta.revision <= self.revision {
            return Err(ScreenDeltaError::RevisionNotNewer {
                current: self.revision,
                actual: delta.revision,
            });
        }

        let columns = usize::from(self.size.columns);
        let rows = usize::from(self.size.rows);
        let mut seen_rows = vec![false; rows];
        for row in &delta.rows {
            let index = usize::from(row.index);
            if index >= rows {
                return Err(ScreenDeltaError::RowOutOfBounds {
                    row: row.index,
                    rows: self.size.rows,
                });
            }
            if std::mem::replace(&mut seen_rows[index], true) {
                return Err(ScreenDeltaError::DuplicateRow { row: row.index });
            }
            if row.cells.len() != columns {
                return Err(ScreenDeltaError::RowWidth {
                    row: row.index,
                    expected: columns,
                    actual: row.cells.len(),
                });
            }
        }
        if delta.cursor.column >= self.size.columns || delta.cursor.row >= self.size.rows {
            return Err(ScreenDeltaError::CursorOutOfBounds {
                column: delta.cursor.column,
                row: delta.cursor.row,
            });
        }

        for row in delta.rows {
            let start = usize::from(row.index) * columns;
            self.cells[start..start + columns].clone_from_slice(&row.cells);
        }
        self.revision = delta.revision;
        self.hyperlinks = delta.hyperlinks;
        self.cursor = delta.cursor;
        self.scroll = delta.scroll;
        self.mouse_tracking = delta.mouse_tracking;
        if let Some(graphics) = delta.graphics {
            self.graphics = graphics;
        }
        Ok(())
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
    #[serde(rename = "h", default, skip_serializing_if = "Vec::is_empty")]
    pub hyperlinks: Vec<CompactString>,
    #[serde(rename = "p")]
    pub cursor: Cursor,
    #[serde(rename = "v")]
    pub scroll: ScrollPosition,
    #[serde(rename = "m", default, skip_serializing_if = "std::ops::Not::not")]
    pub mouse_tracking: bool,
    /// Complete graphics state when it changed; omitted for ordinary text,
    /// cursor, and scroll updates so image bytes are not resent every frame.
    #[serde(rename = "g", default, skip_serializing_if = "Option::is_none")]
    pub graphics: Option<KittyGraphics>,
}

/// Why a [`ScreenDelta`] could not be applied to a [`ScreenSnapshot`].
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ScreenDeltaError {
    #[error("delta base revision {actual} does not match snapshot revision {expected}")]
    BaseRevisionMismatch { expected: u64, actual: u64 },
    #[error("delta size {actual:?} does not match snapshot size {expected:?}")]
    SizeMismatch {
        expected: TerminalSize,
        actual: TerminalSize,
    },
    #[error("delta revision {actual} is not newer than snapshot revision {current}")]
    RevisionNotNewer { current: u64, actual: u64 },
    #[error("delta row {row} is outside the {rows}-row snapshot")]
    RowOutOfBounds { row: u16, rows: u16 },
    #[error("delta contains row {row} more than once")]
    DuplicateRow { row: u16 },
    #[error("delta row {row} has {actual} cells; expected {expected}")]
    RowWidth {
        row: u16,
        expected: usize,
        actual: usize,
    },
    #[error("delta cursor ({column}, {row}) is outside the snapshot")]
    CursorOutOfBounds { column: u16, row: u16 },
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
    fn ids_are_compact_unambiguous_and_serde_round_trip_as_uuid() {
        let first = TerminalId::new();
        let second = TerminalId::new();
        assert_ne!(first, second);

        let json = serde_json::to_string(&first).unwrap();
        assert_eq!(serde_json::from_str::<TerminalId>(&json).unwrap(), first);
        assert_eq!(first.to_string().len(), COMPACT_ID_LEN);
        assert_eq!(first.to_string().parse::<TerminalId>().unwrap(), first);
        assert_eq!(
            first.uuid().to_string().parse::<TerminalId>().unwrap(),
            first
        );
        assert_eq!(json, format!("\"{}\"", first.uuid()));
        for malformed in ["short", &format!("{}A", first), &format!("{}=", first)] {
            assert!(malformed.parse::<TerminalId>().is_err());
        }
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
                shape: Default::default(),
                blinking: false,
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
            contents: format!("a{}", "\u{301}".repeat(MAX_CELL_CONTENT_BYTES)).into(),
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
                shape: Default::default(),
                blinking: false,
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
            shape: Default::default(),
            blinking: false,
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
                    shape: Default::default(),
                    blinking: false,
                },
            ),
            Err(SnapshotError::InvalidSize(TerminalSizeError::TooLarge {
                maximum: MAX_VISIBLE_CELLS,
                ..
            }))
        ));
    }

    fn delta_snapshot() -> ScreenSnapshot {
        ScreenSnapshot::new(
            10,
            TerminalSize {
                columns: 2,
                rows: 2,
            },
            vec![
                Cell {
                    contents: "a".into(),
                    hyperlink: Some(0),
                    ..Cell::default()
                },
                Cell {
                    contents: "b".into(),
                    ..Cell::default()
                },
                Cell {
                    contents: "c".into(),
                    ..Cell::default()
                },
                Cell {
                    contents: "d".into(),
                    ..Cell::default()
                },
            ],
            Cursor {
                column: 0,
                row: 0,
                visible: true,
                shape: CursorShape::Block,
                blinking: false,
            },
        )
        .unwrap()
    }

    fn valid_delta(revision: u64) -> ScreenDelta {
        ScreenDelta {
            revision,
            base_revision: 10,
            size: TerminalSize {
                columns: 2,
                rows: 2,
            },
            rows: vec![DeltaRow {
                index: 1,
                cells: vec![
                    Cell {
                        contents: "x".into(),
                        hyperlink: Some(0),
                        ..Cell::default()
                    },
                    Cell {
                        contents: "y".into(),
                        ..Cell::default()
                    },
                ],
            }],
            hyperlinks: vec!["https://example.test/new".into()],
            cursor: Cursor {
                column: 1,
                row: 1,
                visible: true,
                shape: CursorShape::Underline,
                blinking: true,
            },
            scroll: ScrollPosition {
                offset_from_bottom: 3,
                max_offset_from_bottom: 8,
            },
            mouse_tracking: true,
            graphics: None,
        }
    }

    #[test]
    fn delta_produces_the_same_complete_state_as_a_full_snapshot() {
        let mut from_delta = delta_snapshot();
        let mut delta = valid_delta(11);
        delta.graphics = Some(KittyGraphics {
            images: vec![KittyImage {
                id: 7,
                generation: 4,
                png: vec![1, 2, 3],
            }],
            placements: vec![KittyPlacement {
                image_id: 7,
                placement_id: 2,
                column: 0,
                row: 1,
                columns: 1,
                rows: 1,
                source_x: 0,
                source_y: 0,
                source_width: 1,
                source_height: 1,
                z: 0,
            }],
        });
        from_delta.apply_delta(delta).unwrap();

        let mut full = ScreenSnapshot::new(
            11,
            from_delta.size,
            vec![
                Cell {
                    contents: "a".into(),
                    hyperlink: Some(0),
                    ..Cell::default()
                },
                Cell {
                    contents: "b".into(),
                    ..Cell::default()
                },
                Cell {
                    contents: "x".into(),
                    hyperlink: Some(0),
                    ..Cell::default()
                },
                Cell {
                    contents: "y".into(),
                    ..Cell::default()
                },
            ],
            Cursor {
                column: 1,
                row: 1,
                visible: true,
                shape: CursorShape::Underline,
                blinking: true,
            },
        )
        .unwrap();
        full.hyperlinks = vec!["https://example.test/new".into()];
        full.scroll = ScrollPosition {
            offset_from_bottom: 3,
            max_offset_from_bottom: 8,
        };
        full.mouse_tracking = true;
        full.graphics = from_delta.graphics.clone();

        assert_eq!(from_delta, full);
    }

    #[test]
    fn delta_graphics_retain_change_and_clear() {
        let mut snapshot = delta_snapshot();
        let retained = KittyGraphics {
            images: vec![KittyImage {
                id: 1,
                generation: 1,
                png: vec![1],
            }],
            placements: Vec::new(),
        };
        snapshot.graphics = retained.clone();

        snapshot.apply_delta(valid_delta(11)).unwrap();
        assert_eq!(snapshot.graphics, retained);

        let changed = KittyGraphics {
            images: vec![KittyImage {
                id: 2,
                generation: 2,
                png: vec![2],
            }],
            placements: Vec::new(),
        };
        let mut change = valid_delta(12);
        change.base_revision = 11;
        change.graphics = Some(changed.clone());
        snapshot.apply_delta(change).unwrap();
        assert_eq!(snapshot.graphics, changed);

        let mut clear = valid_delta(13);
        clear.base_revision = 12;
        clear.graphics = Some(KittyGraphics::default());
        snapshot.apply_delta(clear).unwrap();
        assert_eq!(snapshot.graphics, KittyGraphics::default());
    }

    #[test]
    fn invalid_delta_is_rejected_without_changing_the_snapshot() {
        let malformed = [
            {
                let mut delta = valid_delta(11);
                delta.base_revision = 9;
                delta
            },
            {
                let mut delta = valid_delta(11);
                delta.size.columns = 3;
                delta
            },
            valid_delta(10),
            {
                let mut delta = valid_delta(11);
                delta.rows[0].index = 2;
                delta
            },
            {
                let mut delta = valid_delta(11);
                delta.rows.push(delta.rows[0].clone());
                delta
            },
            {
                let mut delta = valid_delta(11);
                delta.rows[0].cells.pop();
                delta
            },
            {
                let mut delta = valid_delta(11);
                delta.cursor.column = 2;
                delta
            },
        ];

        for delta in malformed {
            let mut snapshot = delta_snapshot();
            let before = snapshot.clone();
            assert!(snapshot.apply_delta(delta).is_err());
            assert_eq!(snapshot, before);
        }
    }

    #[test]
    fn compact_cell_style_round_trips_non_defaults() {
        let cell = Cell {
            contents: "λ".into(),
            style: CellStyle::new(
                Some(CellColor::Indexed(1)),
                Some(CellColor::Rgb(Rgb {
                    red: 250,
                    green: 240,
                    blue: 230,
                })),
                true,
                true,
                true,
                true,
            ),
            selected: true,
            hyperlink: None,
        };
        // Pins Cell and CellStyle's Serialize/Deserialize shapes (format-agnostic: the
        // wire protocol itself uses MessagePack, see
        // protocol::tests::wire_pins_a_styled_cells_exact_messagepack_bytes).
        // A cell is a [contents, style, selected] tuple when styled or selected.
        // Style is a flat [fg, bg, flags] array: fg packs the indexed-color
        // tag (0x0100_0000) with the palette index, bg packs the RGB tag
        // (0x0200_0000) with the r/g/b bytes, and flags is a bitfield
        // (bold|italic|underline|inverse = 0b1111).
        let json = serde_json::to_string(&cell).unwrap();
        assert_eq!(serde_json::from_str::<Cell>(&json).unwrap(), cell);
        assert_eq!(json, r#"["λ",70909444472438785,true]"#);
        assert_eq!(serde_json::to_string(&Cell::default()).unwrap(), r#"" ""#);
    }
}
