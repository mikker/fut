//! Versioned messages and bounded JSON framing for the local Fut protocol.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio_util::codec::LengthDelimitedCodec;
use uuid::Uuid;

use std::path::PathBuf;

use crate::{
    domain::{PaneId, ScreenSnapshot, SessionId, TabId, TerminalId, TerminalSize, WorkspaceId},
    resources::{ResourceSnapshot, SessionSelector},
};

pub const PROTOCOL_VERSION: u16 = 2;
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
        selector: Option<SessionSelector>,
    },
    Control,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcknowledgedCommand {
    Input,
    Resize,
    CloseSession,
    Shutdown,
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
        size: TerminalSize,
    },
    Detach,
    CreateSession {
        name: String,
        cwd: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
        #[serde(default)]
        argv: Vec<String>,
    },
    ListResources,
    CloseSession {
        selector: SessionSelector,
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
        selected: Option<SelectedTarget>,
    },
    SessionCreated {
        selected: SelectedTarget,
    },
    Resources {
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
        let selector = SessionSelector::Name("project λ".into());
        let message = ClientMessage::CreateSession {
            name: "project λ".into(),
            cwd: PathBuf::from("/tmp/project"),
            program: Some(PathBuf::from("/bin/sh")),
            argv: vec!["-c".into(), "echo ok".into()],
        };
        let encoded = encode_payload(&message).unwrap();
        assert_eq!(decode_payload::<ClientMessage>(&encoded).unwrap(), message);

        let close = ClientMessage::CloseSession { selector };
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
    }
}
