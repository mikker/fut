//! Fut-owned terminal state exchanged between the daemon and its clients.
//!
//! These types deliberately contain no PTY, terminal-emulator, transport, or
//! presentation-library types.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Upper bound for the number of cells in a terminal's visible grid.
///
/// This is deliberately independent of the transport frame limit: validating
/// dimensions before constructing a grid prevents hostile `u16` dimensions
/// from driving multi-gigabyte allocations.
pub const MAX_VISIBLE_CELLS: usize = 50_000;

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

/// A wheel event normalized to zero-based terminal cell coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MouseWheelEvent {
    pub direction: MouseWheelDirection,
    pub column: u16,
    pub row: u16,
    #[serde(default)]
    pub modifiers: MouseModifiers,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentActivity {
    pub state: AgentState,
    pub revision: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<AgentAttention>,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CellStyle {
    #[serde(rename = "f", skip_serializing_if = "Option::is_none")]
    pub foreground: Option<CellColor>,
    #[serde(rename = "b", skip_serializing_if = "Option::is_none")]
    pub background: Option<CellColor>,
    #[serde(rename = "B", skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(rename = "i", skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(rename = "u", skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(rename = "v", skip_serializing_if = "is_false")]
    pub inverse: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    #[serde(rename = "c")]
    pub contents: String,
    #[serde(rename = "s", default, skip_serializing_if = "CellStyle::is_default")]
    pub style: CellStyle,
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
        cells: Vec<Cell>,
        cursor: Cursor,
    ) -> Result<Self, SnapshotError> {
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

        Ok(Self {
            revision,
            size,
            cells,
            cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let json = serde_json::to_string(&cell).unwrap();
        assert_eq!(serde_json::from_str::<Cell>(&json).unwrap(), cell);
        assert!(json.contains("\"f\":1"));
        assert!(json.contains("\"b\":{\"r\":250,\"g\":240,\"b\":230}"));
        assert_eq!(
            serde_json::to_string(&Cell::default()).unwrap(),
            r#"{"c":" "}"#
        );
    }
}
