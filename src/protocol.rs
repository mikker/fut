//! Versioned messages and bounded MessagePack framing for the local Fut
//! protocol.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio_util::codec::LengthDelimitedCodec;
use uuid::Uuid;

use std::path::PathBuf;

use crate::{
    domain::{
        AgentActivity, AgentReport, AgentReportMetadata, CopyModeAction, CopyModeError, MouseEvent,
        PaneId, ScreenDelta, ScreenSnapshot, SessionId, SplitId, TabId, TerminalId, TerminalOutput,
        TerminalOutputMatcher, TerminalOutputSource, TerminalSize, WorkspaceId,
    },
    resources::{PresentationTokenTarget, ResourceSnapshot, SessionSelector, TargetSelector},
    splits::{SplitDirection, SplitRatio, SplitTree},
};

/// Protocol version used by released Fut 0.1 builds.
pub const PROTOCOL_VERSION_0_1: u16 = 0;
/// Current clients and daemons require an exact protocol match.
pub const PROTOCOL_VERSION: u16 = 23;
/// Enough for 50,000 individually styled MessagePack-encoded cells while
/// remaining a firm pre-allocation bound for the length-delimited transport.
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClientPresenceSnapshot {
    pub revision: u64,
    pub sessions: Vec<SessionPresence>,
}

impl ClientPresenceSnapshot {
    #[must_use]
    pub fn client_count(&self, session_id: SessionId) -> usize {
        self.sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .map_or(0, |session| session.clients as usize)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionPresence {
    pub session_id: SessionId,
    pub clients: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    pub message: T,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientMode {
    Interactive {
        size: TerminalSize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<TargetSelector>,
    },
    Control,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcknowledgedCommand {
    Input,
    Paste,
    ReportAgent,
    AcknowledgeAgent,
    TerminalInput,
    CloseTarget,
    RetireWorkspace,
    RenameTarget,
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalInputOperation {
    Text { text: String },
    Keys { bytes: Vec<u8> },
    Run { text: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum RenameSelector {
    Session(SessionSelector),
    Workspace(WorkspaceId),
    Tab(TabId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelectedTarget {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub child_pid: u32,
}

/// The complete live ancestry resolved from a calling terminal. Contextual
/// control operations carry this back to the daemon so it can reject a race
/// that moved or began closing the terminal after the client's snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalContext {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextScope {
    Session,
    Workspace,
    Tab,
    Pane,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextualCommand {
    CreateTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
        #[serde(default)]
        argv: Vec<String>,
    },
    CreatePane {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
        #[serde(default)]
        argv: Vec<String>,
    },
    SplitPane {
        direction: SplitDirection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
        #[serde(default)]
        argv: Vec<String>,
    },
    MovePane {
        destination_tab_id: TabId,
    },
    Close {
        scope: ContextScope,
    },
    Rename {
        scope: ContextScope,
        name: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "id", rename_all = "snake_case")]
pub enum SelectionExpectation {
    Tab(TabId),
    Workspace(WorkspaceId),
    Session(SessionId),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelectedView {
    /// Resource-tree revision from which this complete view was derived.
    pub resource_revision: u64,
    /// The one terminal receiving this client's input and resize commands.
    pub focused: SelectedTarget,
    /// Open panes in resource-tree order for simultaneous read-only rendering.
    pub panes: Vec<SelectedTarget>,
    /// Open authored topology covering `panes` in the same leaf order.
    pub layout: SplitTree,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenDisposition {
    Existing,
    WorkspaceCreated,
    SessionCreated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        version: u16,
        client_version: String,
        mode: ClientMode,
    },
    Input {
        bytes: Vec<u8>,
    },
    Paste {
        text: String,
    },
    TerminalInput {
        terminal_id: TerminalId,
        operation: TerminalInputOperation,
    },
    ReadTerminalOutput {
        terminal_id: TerminalId,
        source: TerminalOutputSource,
        rows: usize,
        ansi: bool,
    },
    WaitTerminalOutput {
        terminal_id: TerminalId,
        source: TerminalOutputSource,
        rows: usize,
        matcher: TerminalOutputMatcher,
        timeout_ms: u64,
    },
    PromptAgent {
        terminal_id: TerminalId,
        text: String,
        wait: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    WaitAgent {
        terminal_id: TerminalId,
        timeout_ms: u64,
    },
    /// Fire-and-forget; its envelope must not carry a request ID.
    MouseInput {
        terminal_id: TerminalId,
        event: MouseEvent,
    },
    /// Fire-and-forget; its envelope must not carry a request ID.
    ResetViewport {
        terminal_id: TerminalId,
    },
    /// Resync safety net: ask for a full snapshot after a
    /// [`ServerMessage::SnapshotDelta`] the client could not apply.
    /// Fire-and-forget; its envelope must not carry a request ID.
    RefreshTerminal {
        terminal_id: TerminalId,
    },
    CopyMode {
        terminal_id: TerminalId,
        action: CopyModeAction,
    },
    Resize {
        terminal_id: TerminalId,
        size: TerminalSize,
    },
    /// Fire-and-forget shared authored-layout update. The split ID is scoped
    /// to `tab_id`; stale disposable drag targets are ignored by the daemon.
    ResizeSplit {
        tab_id: TabId,
        split_id: SplitId,
        ratio: SplitRatio,
    },
    SelectTarget {
        selector: TargetSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected: Option<SelectionExpectation>,
    },
    Detach,
    OpenLocation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        cwd: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
        #[serde(default)]
        argv: Vec<String>,
    },
    CreateWorkspace {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
        #[serde(default)]
        argv: Vec<String>,
    },
    CreateTab {
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
        #[serde(default)]
        argv: Vec<String>,
    },
    CreatePane {
        tab_id: TabId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
        #[serde(default)]
        argv: Vec<String>,
    },
    SplitPane {
        pane_id: PaneId,
        direction: SplitDirection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
        #[serde(default)]
        argv: Vec<String>,
    },
    MovePane {
        pane_id: PaneId,
        destination_tab_id: TabId,
    },
    Contextual {
        context: TerminalContext,
        command: ContextualCommand,
    },
    ListResources,
    /// Ask a control connection to stream resource and interactive-client
    /// presence changes. The correlated response is a
    /// [`ServerMessage::Resources`] snapshot of both, followed by unsolicited
    /// [`ServerMessage::ResourcesChanged`] and [`ServerMessage::PresenceChanged`]
    /// updates.
    WatchResources,
    CloseTarget {
        selector: TargetSelector,
    },
    /// Mark one workspace closing, acknowledge the control caller, then
    /// terminate its terminals after that caller disconnects.
    RetireWorkspace {
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<TerminalContext>,
    },
    RenameTarget {
        selector: RenameSelector,
        name: String,
    },
    PublishToken {
        extension_id: String,
        token: String,
        value: String,
        target: PresentationTokenTarget,
    },
    ReportAgent {
        terminal_id: TerminalId,
        report: AgentReport,
        #[serde(default, skip_serializing_if = "AgentReportMetadata::is_empty")]
        metadata: AgentReportMetadata,
    },
    AcknowledgeAgent {
        terminal_id: TerminalId,
        event_revision: u64,
    },
    Ping,
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        version: u16,
        server_version: String,
        selected: Option<SelectedView>,
    },
    LocationOpened {
        selected: SelectedTarget,
        disposition: OpenDisposition,
    },
    WorkspaceCreated {
        selected: SelectedTarget,
    },
    TabCreated {
        selected: SelectedTarget,
    },
    PaneCreated {
        selected: SelectedTarget,
    },
    PaneMoved {
        source_tab_id: TabId,
        moved: bool,
        source_tab_closed: bool,
        selected: SelectedTarget,
    },
    TargetRenamed {
        resource_revision: u64,
    },
    TokenPublished {
        resource_revision: u64,
        changed: bool,
    },
    TargetSelected {
        selected: SelectedView,
    },
    Resources {
        snapshot: ResourceSnapshot,
        presence: ClientPresenceSnapshot,
    },
    ResourcesChanged {
        snapshot: ResourceSnapshot,
    },
    PresenceChanged {
        presence: ClientPresenceSnapshot,
    },
    Snapshot {
        terminal_id: TerminalId,
        screen: ScreenSnapshot,
    },
    /// Rows changed since `delta.base_revision`, in place of a full
    /// [`Self::Snapshot`]. The client must verify `base_revision` and `size`
    /// against its current grid before applying; on mismatch it should
    /// discard the delta and send [`ClientMessage::RefreshTerminal`].
    SnapshotDelta {
        terminal_id: TerminalId,
        delta: ScreenDelta,
    },
    CopyModeSnapshot {
        terminal_id: TerminalId,
        screen: ScreenSnapshot,
    },
    CopyModePrepared {
        terminal_id: TerminalId,
        copy_id: Uuid,
        text: String,
    },
    CopyModeFinalized {
        terminal_id: TerminalId,
        screen: ScreenSnapshot,
    },
    CopyModeCancelled {
        terminal_id: TerminalId,
        screen: ScreenSnapshot,
    },
    CopyModeError {
        terminal_id: TerminalId,
        error: CopyModeError,
    },
    TerminalExited {
        terminal_id: TerminalId,
        exit_code: Option<i32>,
    },
    TerminalOutput {
        output: TerminalOutput,
    },
    TerminalOutputMatched {
        output: TerminalOutput,
        start: usize,
        end: usize,
        matched: String,
    },
    AgentPrompted {
        terminal_id: TerminalId,
        barrier_revision: u64,
    },
    AgentSettled {
        terminal_id: TerminalId,
        barrier_revision: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_revision: Option<u64>,
        activity: AgentActivity,
    },
    Pong {
        daemon_pid: u32,
    },
    TerminalResized {
        terminal_id: TerminalId,
        size: TerminalSize,
    },
    CommandCompleted {
        command: AcknowledgedCommand,
    },
    Detached,
    IncompatibleProtocol {
        client: u16,
        server: u16,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame payload is empty")]
    Empty,
    #[error("frame payload is {actual} bytes; maximum is {maximum}")]
    TooLarge { actual: usize, maximum: usize },
    #[error("invalid MessagePack payload: {0}")]
    InvalidEncoding(#[from] rmp_serde::encode::Error),
    #[error("invalid MessagePack payload: {0}")]
    InvalidDecoding(#[from] rmp_serde::decode::Error),
}

pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_length(4)
        .length_field_type::<u32>()
        .big_endian()
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

/// MessagePack with named struct maps (not the positional/compact array
/// form): required so the derived `skip_serializing_if`/`default` field
/// attributes still line up correctly on decode.
pub fn encode_payload<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let payload = rmp_serde::to_vec_named(value)?;
    validate_payload_len(payload.len())?;
    Ok(payload)
}

/// Trailing bytes after a valid value are not rejected: the transport frame
/// (see [`codec`]) already carries an exact length, so a conforming encoder
/// never produces any, and detecting them would require giving up
/// `rmp_serde`'s zero-copy slice reader for one that copies every string and
/// byte payload — the dominant cost of decoding a densely styled frame.
pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    validate_payload_len(payload.len())?;
    let value = rmp_serde::from_slice(payload)?;
    Ok(value)
}

fn validate_payload_len(length: usize) -> Result<(), FrameError> {
    match length {
        0 => Err(FrameError::Empty),
        length if length > MAX_FRAME_LEN => Err(FrameError::TooLarge {
            actual: length,
            maximum: MAX_FRAME_LEN,
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use bytes::{BufMut, Bytes, BytesMut};
    use tokio_util::codec::{Decoder, Encoder};

    use super::*;
    use crate::domain::{
        Cell, Cursor, CursorShape, MAX_CELL_CONTENT_BYTES, MAX_VISIBLE_CELLS, MouseButton,
        MouseButtons, MouseEvent, MouseEventKind, MouseModifiers, MouseWheelDirection,
    };

    #[test]
    fn agent_report_metadata_round_trips_and_old_requests_default_it() {
        let terminal_id = TerminalId::new();
        let message = ClientMessage::ReportAgent {
            terminal_id,
            report: AgentReport::Completed,
            metadata: AgentReportMetadata {
                source: Some("claude-code".into()),
                agent_session_id: Some("session-1".into()),
                turn_id: Some("turn-9".into()),
            },
        };
        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&message).unwrap()).unwrap(),
            message
        );

        let legacy = format!(
            r#"{{"type":"report_agent","terminal_id":"{terminal_id}","report":"working"}}"#
        );
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&legacy).unwrap(),
            ClientMessage::ReportAgent {
                terminal_id,
                report: AgentReport::Working,
                metadata: AgentReportMetadata::default(),
            }
        );
    }

    fn ping(request_id: Option<Uuid>) -> Envelope<ClientMessage> {
        Envelope {
            request_id,
            message: ClientMessage::Ping,
        }
    }

    #[test]
    fn wire_pins_a_styled_cells_exact_messagepack_bytes() {
        // Pins the on-the-wire MessagePack encoding of one styled cell, so a
        // future format change (e.g. switching to compact/positional struct
        // encoding, or unpacking CellStyle back into a field map) is caught
        // deliberately rather than silently.
        let cell = Cell {
            contents: "o".into(),
            style: crate::domain::CellStyle::new(
                Some(crate::domain::CellColor::Rgb(crate::domain::Rgb {
                    red: 10,
                    green: 20,
                    blue: 30,
                })),
                None,
                true,
                false,
                false,
                false,
            ),
            selected: false,
            hyperlink: None,
        };
        let payload = encode_payload(&cell).unwrap();
        assert_eq!(decode_payload::<Cell>(&payload).unwrap(), cell);

        // fixarray["o", packed_style].
        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x92, // fixarray, 2 elements
            0xa1, b'o', // contents: "o"
            0xcf, 0x00, 0x10, 0x00, 0x00, 0x02, 0x0a, 0x14, 0x1e,
            // style: fg RGB(10,20,30), no bg, bold
        ];
        assert_eq!(payload, expected, "payload: {payload:02x?}");
    }

    #[test]
    fn cursor_shape_fields_are_additive_and_unknown_shapes_fall_back_to_block() {
        #[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
        struct LegacyCursor {
            #[serde(rename = "c")]
            column: u16,
            #[serde(rename = "r")]
            row: u16,
            #[serde(rename = "v")]
            visible: bool,
        }

        let legacy = LegacyCursor {
            column: 3,
            row: 4,
            visible: true,
        };
        assert_eq!(
            decode_payload::<Cursor>(&encode_payload(&legacy).unwrap()).unwrap(),
            Cursor {
                column: 3,
                row: 4,
                visible: true,
                shape: CursorShape::Block,
                blinking: false,
            }
        );

        let current = Cursor {
            column: 5,
            row: 6,
            visible: true,
            shape: CursorShape::Bar,
            blinking: true,
        };
        assert_eq!(
            decode_payload::<LegacyCursor>(&encode_payload(&current).unwrap()).unwrap(),
            LegacyCursor {
                column: 5,
                row: 6,
                visible: true,
            }
        );

        let unknown: Cursor =
            serde_json::from_str(r#"{"c":1,"r":2,"v":true,"s":"future","b":true}"#).unwrap();
        assert_eq!(unknown.shape, CursorShape::Block);
        assert!(unknown.blinking);
    }

    #[test]
    fn envelope_round_trips_with_and_without_request_id() {
        for envelope in [ping(None), ping(Some(Uuid::new_v4()))] {
            let payload = encode_payload(&envelope).unwrap();
            assert_eq!(
                decode_payload::<Envelope<ClientMessage>>(&payload).unwrap(),
                envelope
            );
        }
    }

    #[test]
    fn typed_unicode_paste_round_trips() {
        let message = ClientMessage::Paste {
            text: "first λ 雪\nsecond\0\x1b[201~".into(),
        };

        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&message).unwrap()).unwrap(),
            message
        );
    }

    #[test]
    fn targeted_terminal_input_operations_round_trip() {
        let terminal_id = TerminalId::new();
        for operation in [
            TerminalInputOperation::Text {
                text: "literal λ\n".into(),
            },
            TerminalInputOperation::Keys {
                bytes: b"\x03\x1b[A".to_vec(),
            },
            TerminalInputOperation::Run {
                text: "printf '雪'".into(),
            },
        ] {
            let message = ClientMessage::TerminalInput {
                terminal_id,
                operation,
            };
            assert_eq!(
                decode_payload::<ClientMessage>(&encode_payload(&message).unwrap()).unwrap(),
                message
            );
        }
    }

    #[test]
    fn agent_prompt_and_wait_messages_round_trip() {
        let terminal_id = TerminalId::new();
        for message in [
            ClientMessage::PromptAgent {
                terminal_id,
                text: "explain λ".into(),
                wait: true,
                timeout_ms: Some(30_000),
            },
            ClientMessage::WaitAgent {
                terminal_id,
                timeout_ms: 30_000,
            },
        ] {
            assert_eq!(
                decode_payload::<ClientMessage>(&encode_payload(&message).unwrap()).unwrap(),
                message
            );
        }

        let activity = AgentActivity {
            integration: Some(Default::default()),
            detection: None,
            state: crate::domain::AgentState::Blocked,
            revision: 9,
            updated_at_ms: 12,
            last_event: Some(crate::domain::AgentEvent {
                revision: 9,
                kind: AgentReport::Blocked,
                occurred_at_ms: 12,
                turn_id: Some("turn-1".into()),
            }),
            read_revision: 0,
        };
        let message = ServerMessage::AgentSettled {
            terminal_id,
            barrier_revision: 6,
            working_revision: Some(7),
            activity,
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&message).unwrap()).unwrap(),
            message
        );
    }

    #[test]
    fn terminal_output_requests_and_results_round_trip_unicode() {
        let terminal_id = TerminalId::new();
        for message in [
            ClientMessage::ReadTerminalOutput {
                terminal_id,
                source: TerminalOutputSource::RecentUnwrapped,
                rows: 200,
                ansi: true,
            },
            ClientMessage::WaitTerminalOutput {
                terminal_id,
                source: TerminalOutputSource::Recent,
                rows: 40,
                matcher: TerminalOutputMatcher::Regex("ready λ [0-9]+".into()),
                timeout_ms: 30_000,
            },
        ] {
            assert_eq!(
                decode_payload::<ClientMessage>(&encode_payload(&message).unwrap()).unwrap(),
                message
            );
        }

        let output = TerminalOutput {
            version: 1,
            terminal_id,
            revision: 7,
            source: TerminalOutputSource::RecentUnwrapped,
            requested_rows: 20,
            returned_rows: 20,
            truncated: true,
            starts_mid_logical_line: true,
            ansi: false,
            text: "partial 雪\nready λ".into(),
        };
        let message = ServerMessage::TerminalOutputMatched {
            output,
            start: 12,
            end: 20,
            matched: "ready λ".into(),
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&message).unwrap()).unwrap(),
            message
        );
    }

    #[test]
    fn typed_copy_mode_commands_results_and_errors_round_trip() {
        let terminal_id = TerminalId::new();
        let actions = [
            CopyModeAction::Begin,
            CopyModeAction::BeginSelection { column: 2, row: 3 },
            CopyModeAction::SetSelectionEnd { column: 8, row: 4 },
            CopyModeAction::Move {
                movement: crate::domain::CopyModeMovement::PageUp,
            },
            CopyModeAction::ToggleSelection,
            CopyModeAction::Search {
                query: "literal λ 雪".into(),
            },
            CopyModeAction::RepeatSearch {
                direction: crate::domain::SearchDirection::Backward,
            },
            CopyModeAction::Copy,
            CopyModeAction::FinalizeCopy {
                copy_id: Uuid::new_v4(),
            },
            CopyModeAction::Cancel,
        ];
        for action in actions {
            let message = ClientMessage::CopyMode {
                terminal_id,
                action,
            };
            assert_eq!(
                decode_payload::<ClientMessage>(&encode_payload(&message).unwrap()).unwrap(),
                message
            );
        }

        let error = ServerMessage::CopyModeError {
            terminal_id,
            error: CopyModeError::SearchSpaceTooLarge {
                actual: 250_001,
                maximum: 250_000,
            },
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&error).unwrap()).unwrap(),
            error
        );
    }

    #[test]
    fn maximum_copy_text_frame_has_headroom_without_a_screen() {
        let terminal_id = TerminalId::new();
        let message = Envelope {
            request_id: Some(Uuid::new_v4()),
            message: ServerMessage::CopyModePrepared {
                terminal_id,
                copy_id: Uuid::new_v4(),
                // MAX_COPY_BYTES of raw text, with no escaping overhead in
                // MessagePack's binary encoding.
                text: "\0".repeat(crate::domain::MAX_COPY_BYTES),
            },
        };
        assert!(encode_payload(&message).unwrap().len() < MAX_FRAME_LEN);
    }

    #[test]
    fn payload_rejects_empty_oversized_and_invalid_data_but_ignores_trailing_bytes() {
        assert!(matches!(
            decode_payload::<ClientMessage>(&[]),
            Err(FrameError::Empty)
        ));
        assert!(matches!(
            decode_payload::<ClientMessage>(&vec![b' '; MAX_FRAME_LEN + 1]),
            Err(FrameError::TooLarge { .. })
        ));
        assert!(matches!(
            decode_payload::<ClientMessage>(b"not msgpack"),
            Err(FrameError::InvalidDecoding(_))
        ));

        // A conforming encoder never produces trailing bytes (the transport
        // frame already carries an exact length), so decoding tolerates and
        // ignores them rather than paying for an explicit check.
        let mut payload = encode_payload(&ClientMessage::Ping).unwrap();
        payload.extend_from_slice(b" {}");
        assert_eq!(
            decode_payload::<ClientMessage>(&payload).unwrap(),
            ClientMessage::Ping
        );
    }

    #[test]
    fn codec_handles_fragmented_and_multiple_frames() {
        let first = encode_payload(&ping(None)).unwrap();
        let second = encode_payload(&ping(Some(Uuid::new_v4()))).unwrap();
        let mut wire = BytesMut::new();
        wire.put_u32(first.len() as u32);
        wire.extend_from_slice(&first);
        wire.put_u32(second.len() as u32);
        wire.extend_from_slice(&second);

        let mut decoder = codec();
        let mut incoming = wire.split_to(3);
        assert!(decoder.decode(&mut incoming).unwrap().is_none());
        incoming.extend_from_slice(&wire);
        let decoded_first = decoder.decode(&mut incoming).unwrap().unwrap();
        let decoded_second = decoder.decode(&mut incoming).unwrap().unwrap();
        assert_eq!(
            decode_payload::<Envelope<ClientMessage>>(&decoded_first).unwrap(),
            ping(None)
        );
        assert!(
            decode_payload::<Envelope<ClientMessage>>(&decoded_second)
                .unwrap()
                .request_id
                .is_some()
        );
        assert!(decoder.decode(&mut incoming).unwrap().is_none());
    }

    #[test]
    fn maximum_blank_snapshot_round_trips_below_frame_limit() {
        let size = TerminalSize {
            columns: 250,
            rows: 200,
        };
        assert_eq!(size.cell_count().unwrap(), MAX_VISIBLE_CELLS);
        let snapshot = ScreenSnapshot::new(
            42,
            size,
            vec![Cell::default(); MAX_VISIBLE_CELLS],
            Cursor {
                column: 249,
                row: 199,
                visible: true,
                shape: Default::default(),
                blinking: false,
            },
        )
        .unwrap();
        let payload = encode_payload(&snapshot).unwrap();
        assert!(payload.len() < MAX_FRAME_LEN);
        assert_eq!(
            decode_payload::<ScreenSnapshot>(&payload).unwrap(),
            snapshot
        );
    }

    #[test]
    fn maximum_content_selected_styled_snapshot_and_copy_frames_fit() {
        let size = TerminalSize {
            columns: 250,
            rows: 200,
        };
        let maximum_content = format!("a{}\u{1ab0}", "\u{301}".repeat(14));
        assert_eq!(maximum_content.len(), MAX_CELL_CONTENT_BYTES);
        let cell = Cell {
            contents: maximum_content.into(),
            style: crate::domain::CellStyle::new(
                Some(crate::domain::CellColor::Rgb(crate::domain::Rgb {
                    red: 255,
                    green: 255,
                    blue: 255,
                })),
                Some(crate::domain::CellColor::Rgb(crate::domain::Rgb {
                    red: 255,
                    green: 255,
                    blue: 255,
                })),
                true,
                true,
                true,
                true,
            ),
            selected: true,
            hyperlink: None,
        };
        let screen = ScreenSnapshot::new(
            u64::MAX,
            size,
            vec![cell; MAX_VISIBLE_CELLS],
            Cursor {
                column: 249,
                row: 199,
                visible: false,
                shape: Default::default(),
                blinking: false,
            },
        )
        .unwrap();
        assert_eq!(screen.cells[0].contents.len(), MAX_CELL_CONTENT_BYTES);
        let terminal_id = TerminalId::new();
        for message in [
            ServerMessage::Snapshot {
                terminal_id,
                screen: screen.clone(),
            },
            ServerMessage::CopyModeSnapshot {
                terminal_id,
                screen,
            },
        ] {
            let payload = encode_payload(&Envelope {
                request_id: Some(Uuid::new_v4()),
                message,
            })
            .unwrap();
            assert!(
                payload.len() < MAX_FRAME_LEN,
                "maximum snapshot frame was {} bytes",
                payload.len()
            );
        }
    }

    #[test]
    fn codec_accepts_exact_limit_and_rejects_oversized_header() {
        let mut exact = BytesMut::with_capacity(4 + MAX_FRAME_LEN);
        exact.put_u32(MAX_FRAME_LEN as u32);
        exact.resize(4 + MAX_FRAME_LEN, b' ');
        let decoded = codec().decode(&mut exact).unwrap().unwrap();
        assert_eq!(decoded.len(), MAX_FRAME_LEN);

        let mut oversized = BytesMut::new();
        oversized.put_u32((MAX_FRAME_LEN + 1) as u32);
        assert!(codec().decode(&mut oversized).is_err());
    }

    #[test]
    fn codec_encoder_and_payload_codec_round_trip() {
        let envelope = ping(Some(Uuid::new_v4()));
        let payload = encode_payload(&envelope).unwrap();
        let mut wire = BytesMut::new();
        codec().encode(Bytes::from(payload), &mut wire).unwrap();
        let decoded = codec().decode(&mut wire).unwrap().unwrap();
        assert_eq!(
            decode_payload::<Envelope<ClientMessage>>(&decoded).unwrap(),
            envelope
        );
    }

    #[test]
    fn codec_waits_for_incomplete_headers_and_payloads() {
        let mut short_header = BytesMut::from(&[0, 0][..]);
        assert!(codec().decode(&mut short_header).unwrap().is_none());

        let mut short_payload = BytesMut::from(&[0, 0, 0, 2, b'{'][..]);
        assert!(codec().decode(&mut short_payload).unwrap().is_none());
    }

    #[test]
    fn command_completion_round_trips_with_request_id() {
        let envelope = Envelope {
            request_id: Some(Uuid::new_v4()),
            message: ServerMessage::CommandCompleted {
                command: AcknowledgedCommand::Input,
            },
        };
        let frame = encode_payload(&envelope).unwrap();
        assert_eq!(
            decode_payload::<Envelope<ServerMessage>>(&frame).unwrap(),
            envelope
        );
    }

    #[test]
    fn selectors_resource_commands_and_snapshots_round_trip() {
        let selector =
            TargetSelector::Session(crate::resources::SessionSelector::Name("project λ".into()));
        let message = ClientMessage::OpenLocation {
            name: Some("project λ".into()),
            cwd: PathBuf::from("/tmp/project"),
            program: Some(PathBuf::from("/bin/sh")),
            argv: vec!["-c".into(), "echo ok".into()],
        };
        let encoded = encode_payload(&message).unwrap();
        assert_eq!(decode_payload::<ClientMessage>(&encoded).unwrap(), message);

        let select = ClientMessage::SelectTarget {
            selector: selector.clone(),
            expected: Some(SelectionExpectation::Tab(TabId::new())),
        };
        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&select).unwrap()).unwrap(),
            select
        );
        let close = ClientMessage::CloseTarget { selector };
        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&close).unwrap()).unwrap(),
            close
        );
        let context = TerminalContext {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let retire = ClientMessage::RetireWorkspace {
            workspace_id: context.workspace_id,
            context: Some(context),
        };
        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&retire).unwrap()).unwrap(),
            retire
        );
        let publish = ClientMessage::PublishToken {
            extension_id: "review-status".into(),
            token: "state".into(),
            value: "ready".into(),
            target: PresentationTokenTarget::Workspace(WorkspaceId::new()),
        };
        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&publish).unwrap()).unwrap(),
            publish
        );
        let published = ServerMessage::TokenPublished {
            resource_revision: 10,
            changed: true,
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&published).unwrap()).unwrap(),
            published
        );
        let resources = ServerMessage::Resources {
            snapshot: ResourceSnapshot {
                revision: 9,
                sessions: vec![],
            },
            presence: ClientPresenceSnapshot::default(),
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&resources).unwrap()).unwrap(),
            resources
        );
        let opened = ServerMessage::LocationOpened {
            selected: SelectedTarget {
                session_id: SessionId::new(),
                workspace_id: WorkspaceId::new(),
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
                child_pid: 42,
            },
            disposition: OpenDisposition::WorkspaceCreated,
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&opened).unwrap()).unwrap(),
            opened
        );
        let selected = match opened {
            ServerMessage::LocationOpened { selected, .. } => selected,
            _ => unreachable!(),
        };
        let switched = ServerMessage::TargetSelected {
            selected: SelectedView {
                resource_revision: 9,
                focused: selected.clone(),
                layout: SplitTree::leaf(selected.pane_id),
                panes: vec![selected],
            },
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&switched).unwrap()).unwrap(),
            switched
        );
        assert_eq!(PROTOCOL_VERSION_0_1, 0);
        assert_eq!(PROTOCOL_VERSION, 23);

        let watch = ClientMessage::WatchResources;
        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&watch).unwrap()).unwrap(),
            watch
        );
    }

    #[test]
    fn resize_and_selected_view_round_trip() {
        let terminal_id = TerminalId::new();
        for kind in [
            MouseEventKind::Press {
                button: MouseButton::Left,
            },
            MouseEventKind::Release {
                button: MouseButton::Middle,
            },
            MouseEventKind::Motion {
                button: Some(MouseButton::Right),
            },
            MouseEventKind::Motion { button: None },
            MouseEventKind::Wheel {
                direction: MouseWheelDirection::Up,
            },
        ] {
            let mouse = ClientMessage::MouseInput {
                terminal_id,
                event: MouseEvent {
                    kind,
                    column: 12,
                    row: 4,
                    modifiers: MouseModifiers {
                        shift: true,
                        control: false,
                        alt: true,
                    },
                    buttons: MouseButtons {
                        left: true,
                        middle: false,
                        right: true,
                    },
                },
            };
            assert_eq!(
                decode_payload::<ClientMessage>(&encode_payload(&mouse).unwrap()).unwrap(),
                mouse
            );
        }
        let reset = ClientMessage::ResetViewport { terminal_id };
        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&reset).unwrap()).unwrap(),
            reset
        );
        let resize = ClientMessage::Resize {
            terminal_id,
            size: TerminalSize {
                columns: 120,
                rows: 40,
            },
        };
        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&resize).unwrap()).unwrap(),
            resize
        );
        let resized = ServerMessage::TerminalResized {
            terminal_id,
            size: TerminalSize {
                columns: 80,
                rows: 24,
            },
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&resized).unwrap()).unwrap(),
            resized
        );
        let resize_split = ClientMessage::ResizeSplit {
            tab_id: TabId::new(),
            split_id: SplitId::new(),
            ratio: SplitRatio::from_cells(37, 79).unwrap(),
        };
        assert_eq!(
            decode_payload::<ClientMessage>(&encode_payload(&resize_split).unwrap()).unwrap(),
            resize_split
        );

        let focused = SelectedTarget {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
            terminal_id,
            child_pid: 42,
        };
        let welcome = ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            server_version: "test".into(),
            selected: Some(SelectedView {
                resource_revision: 1,
                focused: focused.clone(),
                layout: SplitTree::leaf(focused.pane_id),
                panes: vec![focused],
            }),
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&welcome).unwrap()).unwrap(),
            welcome
        );
    }

    #[test]
    fn streamed_resource_snapshot_round_trips_as_an_unsolicited_message() {
        let envelope = Envelope {
            request_id: None,
            message: ServerMessage::ResourcesChanged {
                snapshot: ResourceSnapshot {
                    revision: 12,
                    sessions: vec![],
                },
            },
        };
        assert_eq!(
            decode_payload::<Envelope<ServerMessage>>(&encode_payload(&envelope).unwrap()).unwrap(),
            envelope
        );

        let presence = ServerMessage::PresenceChanged {
            presence: ClientPresenceSnapshot {
                revision: 3,
                sessions: vec![SessionPresence {
                    session_id: SessionId::new(),
                    clients: 2,
                }],
            },
        };
        assert_eq!(
            decode_payload::<ServerMessage>(&encode_payload(&presence).unwrap()).unwrap(),
            presence
        );
    }

    #[test]
    fn rename_target_and_correlated_ack_round_trip_all_selectors_and_exact_names() {
        let cases = [
            (
                RenameSelector::Session(SessionSelector::Name("project λ".into())),
                "開発 λ",
            ),
            (
                RenameSelector::Workspace(WorkspaceId::new()),
                "  exact name  ",
            ),
            (RenameSelector::Tab(TabId::new()), "雪/タブ"),
        ];

        for (selector, name) in cases {
            let request_id = Uuid::new_v4();
            let request = Envelope {
                request_id: Some(request_id),
                message: ClientMessage::RenameTarget {
                    selector,
                    name: name.into(),
                },
            };
            let decoded_request =
                decode_payload::<Envelope<ClientMessage>>(&encode_payload(&request).unwrap())
                    .unwrap();
            assert_eq!(decoded_request, request);
            assert!(matches!(
                &decoded_request.message,
                ClientMessage::RenameTarget { name: decoded, .. } if decoded == name
            ));

            let response = Envelope {
                request_id: Some(request_id),
                message: ServerMessage::CommandCompleted {
                    command: AcknowledgedCommand::RenameTarget,
                },
            };
            assert_eq!(
                decode_payload::<Envelope<ServerMessage>>(&encode_payload(&response).unwrap())
                    .unwrap(),
                response
            );
            assert_eq!(response.request_id, request.request_id);

            let interactive_response = Envelope {
                request_id: Some(request_id),
                message: ServerMessage::TargetRenamed {
                    resource_revision: 42,
                },
            };
            assert_eq!(
                decode_payload::<Envelope<ServerMessage>>(
                    &encode_payload(&interactive_response).unwrap()
                )
                .unwrap(),
                interactive_response
            );
        }
    }

    #[test]
    fn create_pane_and_correlated_response_round_trip_unicode_argv() {
        let request_id = Uuid::new_v4();
        let request = Envelope {
            request_id: Some(request_id),
            message: ClientMessage::CreatePane {
                tab_id: TabId::new(),
                cwd: Some(PathBuf::from("/tmp/雪 workspace")),
                program: Some(PathBuf::from("/opt/工具/bin/シェル")),
                argv: vec!["--題=λ".into(), "こんにちは 世界".into()],
            },
        };
        assert_eq!(
            decode_payload::<Envelope<ClientMessage>>(&encode_payload(&request).unwrap()).unwrap(),
            request
        );

        let response = Envelope {
            request_id: Some(request_id),
            message: ServerMessage::PaneCreated {
                selected: SelectedTarget {
                    session_id: SessionId::new(),
                    workspace_id: WorkspaceId::new(),
                    tab_id: TabId::new(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                    child_pid: 4242,
                },
            },
        };
        assert_eq!(
            decode_payload::<Envelope<ServerMessage>>(&encode_payload(&response).unwrap()).unwrap(),
            response
        );
        assert_eq!(response.request_id, request.request_id);
    }

    #[test]
    fn split_pane_direction_round_trips() {
        for direction in [SplitDirection::Right, SplitDirection::Down] {
            let message = ClientMessage::SplitPane {
                pane_id: PaneId::new(),
                direction,
                cwd: None,
                program: None,
                argv: Vec::new(),
            };
            assert_eq!(
                decode_payload::<ClientMessage>(&encode_payload(&message).unwrap()).unwrap(),
                message
            );
        }
    }

    #[test]
    fn move_pane_and_correlated_response_round_trip() {
        let request_id = Uuid::new_v4();
        let pane_id = PaneId::new();
        let source_tab_id = TabId::new();
        let destination_tab_id = TabId::new();
        let selected = SelectedTarget {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            tab_id: destination_tab_id,
            pane_id,
            terminal_id: TerminalId::new(),
            child_pid: 42,
        };
        let request = Envelope {
            request_id: Some(request_id),
            message: ClientMessage::MovePane {
                pane_id,
                destination_tab_id,
            },
        };
        let response = Envelope {
            request_id: Some(request_id),
            message: ServerMessage::PaneMoved {
                source_tab_id,
                moved: true,
                source_tab_closed: true,
                selected,
            },
        };

        assert_eq!(
            decode_payload::<Envelope<ClientMessage>>(&encode_payload(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            decode_payload::<Envelope<ServerMessage>>(&encode_payload(&response).unwrap()).unwrap(),
            response
        );
        assert_eq!(response.request_id, request.request_id);
    }

    #[test]
    fn contextual_layout_commands_round_trip_complete_terminal_ancestry() {
        let context = TerminalContext {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let commands = [
            ContextualCommand::CreateTab {
                name: Some("fresh".into()),
                cwd: Some("/tmp/project".into()),
                program: Some("/bin/sh".into()),
                argv: vec!["-l".into()],
            },
            ContextualCommand::CreatePane {
                cwd: None,
                program: None,
                argv: Vec::new(),
            },
            ContextualCommand::SplitPane {
                direction: SplitDirection::Down,
                cwd: None,
                program: None,
                argv: Vec::new(),
            },
            ContextualCommand::MovePane {
                destination_tab_id: TabId::new(),
            },
            ContextualCommand::Close {
                scope: ContextScope::Workspace,
            },
            ContextualCommand::Rename {
                scope: ContextScope::Tab,
                name: "renamed".into(),
            },
        ];
        for command in commands {
            let message = ClientMessage::Contextual { context, command };
            assert_eq!(
                decode_payload::<ClientMessage>(&encode_payload(&message).unwrap()).unwrap(),
                message
            );
        }
    }

    #[test]
    fn create_tab_and_correlated_response_round_trip_unicode_values() {
        let request_id = Uuid::new_v4();
        let request = Envelope {
            request_id: Some(request_id),
            message: ClientMessage::CreateTab {
                workspace_id: WorkspaceId::new(),
                name: Some("開発 λ".into()),
                cwd: Some(PathBuf::from("/tmp/雪 workspace")),
                program: Some(PathBuf::from("/opt/工具/bin/シェル")),
                argv: vec!["--題=λ".into(), "こんにちは 世界".into()],
            },
        };
        assert_eq!(
            decode_payload::<Envelope<ClientMessage>>(&encode_payload(&request).unwrap()).unwrap(),
            request
        );

        let response = Envelope {
            request_id: Some(request_id),
            message: ServerMessage::TabCreated {
                selected: SelectedTarget {
                    session_id: SessionId::new(),
                    workspace_id: WorkspaceId::new(),
                    tab_id: TabId::new(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                    child_pid: 4242,
                },
            },
        };
        assert_eq!(
            decode_payload::<Envelope<ServerMessage>>(&encode_payload(&response).unwrap()).unwrap(),
            response
        );
        assert_eq!(response.request_id, request.request_id);
    }

    #[test]
    fn create_workspace_round_trips_logical_duplicate_root_request() {
        let request_id = Uuid::new_v4();
        let request = Envelope {
            request_id: Some(request_id),
            message: ClientMessage::CreateWorkspace {
                session_id: SessionId::new(),
                name: Some("focused context".into()),
                cwd: None,
                program: None,
                argv: Vec::new(),
            },
        };
        assert_eq!(
            decode_payload::<Envelope<ClientMessage>>(&encode_payload(&request).unwrap()).unwrap(),
            request
        );
        let response = Envelope {
            request_id: Some(request_id),
            message: ServerMessage::WorkspaceCreated {
                selected: SelectedTarget {
                    session_id: SessionId::new(),
                    workspace_id: WorkspaceId::new(),
                    tab_id: TabId::new(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                    child_pid: 42,
                },
            },
        };
        assert_eq!(
            decode_payload::<Envelope<ServerMessage>>(&encode_payload(&response).unwrap()).unwrap(),
            response
        );
    }
}
