//! Versioned messages and bounded JSON framing for the local Fut protocol.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio_util::codec::LengthDelimitedCodec;
use uuid::Uuid;

use std::path::PathBuf;

use crate::{
    domain::{PaneId, ScreenSnapshot, SessionId, TabId, TerminalId, TerminalSize, WorkspaceId},
    resources::{ResourceSnapshot, SessionSelector, TargetSelector},
    splits::{SplitDirection, SplitTree},
};

/// Reserved development epoch. Wire compatibility is intentionally not
/// maintained between builds until Fut's protocol stabilizes.
pub const PROTOCOL_VERSION: u16 = 0;
/// Enough for 50,000 individually styled JSON cells while remaining a firm
/// pre-allocation bound for the length-delimited transport.
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

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
    Resize,
    CloseTarget,
    RenameTarget,
    Shutdown,
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
    Resize {
        terminal_id: TerminalId,
        size: TerminalSize,
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
    ListResources,
    CloseTarget {
        selector: TargetSelector,
    },
    RenameTarget {
        selector: RenameSelector,
        name: String,
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
    TargetSelected {
        selected: SelectedView,
    },
    Resources {
        snapshot: ResourceSnapshot,
    },
    ResourcesChanged {
        snapshot: ResourceSnapshot,
    },
    Snapshot {
        terminal_id: TerminalId,
        screen: ScreenSnapshot,
    },
    TerminalExited {
        terminal_id: TerminalId,
        exit_code: Option<i32>,
    },
    Pong {
        daemon_pid: u32,
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
    #[error("invalid JSON payload: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_length(4)
        .length_field_type::<u32>()
        .big_endian()
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

pub fn encode_payload<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(value)?;
    validate_payload_len(payload.len())?;
    Ok(payload)
}

pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    validate_payload_len(payload.len())?;
    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    let value = T::deserialize(&mut deserializer)?;
    deserializer.end()?;
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
    use crate::domain::{Cell, Cursor, MAX_VISIBLE_CELLS};

    fn ping(request_id: Option<Uuid>) -> Envelope<ClientMessage> {
        Envelope {
            request_id,
            message: ClientMessage::Ping,
        }
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
    fn payload_rejects_empty_oversized_invalid_and_trailing_data() {
        assert!(matches!(
            decode_payload::<ClientMessage>(&[]),
            Err(FrameError::Empty)
        ));
        assert!(matches!(
            decode_payload::<ClientMessage>(&vec![b' '; MAX_FRAME_LEN + 1]),
            Err(FrameError::TooLarge { .. })
        ));
        assert!(matches!(
            decode_payload::<ClientMessage>(b"not json"),
            Err(FrameError::InvalidJson(_))
        ));

        let mut payload = encode_payload(&ClientMessage::Ping).unwrap();
        payload.extend_from_slice(b" {}");
        assert!(matches!(
            decode_payload::<ClientMessage>(&payload),
            Err(FrameError::InvalidJson(_))
        ));
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
                command: AcknowledgedCommand::Resize,
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
        let resources = ServerMessage::Resources {
            snapshot: ResourceSnapshot {
                revision: 9,
                sessions: vec![],
            },
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
        assert_eq!(PROTOCOL_VERSION, 0);
    }

    #[test]
    fn resize_and_selected_view_round_trip() {
        let terminal_id = TerminalId::new();
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
