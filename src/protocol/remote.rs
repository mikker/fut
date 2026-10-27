//! Remote generation 2. Package versions are diagnostics, never a
//! compatibility decision. See docs/remote-protocol.md before changing any
//! reused payload or adding a method to the allowlists below.

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize, de};
use thiserror::Error;

use super::{ClientMessage, ClientMode, ExtensionCatalog, SelectedView, ServerMessage};

pub const GENERATION: u16 = 2;
pub const CODEC: &str = "msgpack-map-v1";
pub const MAX_CAPABILITIES: usize = 32;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_VERSION_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteHello {
    pub generation: u16,
    pub codec: String,
    pub client_version: String,
    #[serde(deserialize_with = "capability_list")]
    pub required: Vec<String>,
    #[serde(deserialize_with = "capability_list")]
    pub optional: Vec<String>,
    pub mode: ClientMode,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteWelcome {
    pub generation: u16,
    pub codec: String,
    pub server_version: String,
    #[serde(deserialize_with = "capability_list")]
    pub capabilities: Vec<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub selected: Option<SelectedView>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub extension_catalog: Option<ExtensionCatalog>,
}

/// No lifecycle/retry instructions or peer-supplied prose cross this boundary.
#[derive(Clone, Debug, Eq, PartialEq, Error, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case", deny_unknown_fields)]
pub enum EndpointError {
    #[error("incompatible endpoint generation: client {client}, server {server}")]
    IncompatibleGeneration { client: u16, server: u16 },
    #[error("unsupported endpoint codec")]
    UnsupportedCodec,
    #[error("endpoint is missing a required capability")]
    MissingRequiredCapability,
    #[error("invalid endpoint handshake")]
    InvalidHandshake,
    #[error("endpoint method was not negotiated")]
    MethodNotNegotiated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    Metadata,
    NestedWorkspaces,
    Interactive,
    Health,
    Alerts,
    ControlAlerts,
    ExtensionCatalog,
    TerminalColors,
    ProjectOpen,
    Attachments,
    LargeScreens,
    ScrollDeltas,
    ChunkedScreens,
    ConfiguredCommands,
    WorkspaceOrder,
}

impl Capability {
    pub const ALL: [Self; 15] = [
        Self::Metadata,
        Self::NestedWorkspaces,
        Self::Interactive,
        Self::Health,
        Self::Alerts,
        Self::ControlAlerts,
        Self::ExtensionCatalog,
        Self::TerminalColors,
        Self::ProjectOpen,
        Self::Attachments,
        Self::LargeScreens,
        Self::ScrollDeltas,
        Self::ChunkedScreens,
        Self::ConfiguredCommands,
        Self::WorkspaceOrder,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Metadata => "metadata.v2",
            Self::NestedWorkspaces => "nested-workspaces.v1",
            Self::Interactive => "interactive.v1",
            Self::Health => "health.v1",
            Self::Alerts => "alerts.v1",
            Self::ControlAlerts => "control-alerts.v1",
            Self::ExtensionCatalog => "extension-catalog.v1",
            Self::TerminalColors => "terminal-colors.v1",
            Self::ProjectOpen => "project-open.v1",
            Self::Attachments => "attachments.v1",
            Self::LargeScreens => "large-screens.v1",
            Self::ScrollDeltas => "scroll-deltas.v1",
            Self::ChunkedScreens => "chunked-screens.v1",
            Self::ConfiguredCommands => "configured-commands.v1",
            Self::WorkspaceOrder => "workspace-order.v1",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities(u16);

impl Capabilities {
    pub const ALL: Self = Self((1 << Capability::ALL.len()) - 1);

    pub fn contains(self, capability: Capability) -> bool {
        self.0 & (1 << capability as u8) != 0
    }

    pub fn names(self) -> Vec<String> {
        Capability::ALL
            .into_iter()
            .filter(|cap| self.contains(*cap))
            .map(|cap| cap.name().into())
            .collect()
    }

    /// Whether screens reference Kitty images delivered in separate
    /// `kitty_image` frames rather than embedding their pixels.
    pub fn separate_images(self) -> bool {
        self.contains(Capability::LargeScreens)
    }

    /// Whether screens larger than one frame may arrive as row-range chunks.
    /// Chunk headers carry image references, so this also needs separate
    /// images.
    pub fn chunked_screens(self) -> bool {
        self.contains(Capability::ChunkedScreens) && self.separate_images()
    }

    /// Largest visible grid this peer may request or receive.
    pub fn max_visible_cells(self) -> usize {
        if self.chunked_screens() {
            crate::domain::MAX_GRID_CELLS
        } else if self.contains(Capability::LargeScreens) {
            crate::domain::MAX_VISIBLE_CELLS
        } else {
            crate::domain::LEGACY_MAX_VISIBLE_CELLS
        }
    }

    fn from_names(names: &[String]) -> Self {
        Self(
            Capability::ALL
                .into_iter()
                .filter(|cap| names.iter().any(|name| name == cap.name()))
                .fold(0, |bits, cap| bits | (1 << cap as u8)),
        )
    }

    /// An allowlist, not all current/future private protocol messages. In
    /// particular, remote peers cannot shut down or reconfigure the daemon.
    pub fn allows_client(self, message: &ClientMessage) -> bool {
        use ClientMessage::*;
        let capability = match message {
            Detach => return true,
            ListResources | WatchResources => Capability::Metadata,
            OpenProject { .. } => Capability::ProjectOpen,
            MoveWorkspace { .. } => Capability::WorkspaceOrder,
            Ping => Capability::Health,
            GetExtensionCatalog => Capability::ExtensionCatalog,
            CommandDefaults { .. } | CommandStart { .. } | CommandOperation { .. } => {
                Capability::ConfiguredCommands
            }
            TerminalColors { .. } => Capability::TerminalColors,
            AttachmentBegin { .. }
            | AttachmentChunk { .. }
            | AttachmentCommit { .. }
            | AttachmentCancel { .. } => Capability::Attachments,
            WatchAlerts { .. } | AcknowledgeAlerts { .. } => {
                return self.contains(Capability::Alerts)
                    || self.contains(Capability::ControlAlerts);
            }
            Input { .. }
            | KeyInput { .. }
            | Paste { .. }
            | MouseInput { .. }
            | ResetViewport { .. }
            | RefreshTerminal { .. }
            | CopyMode { .. }
            | Resize { .. }
            | ResizeSplit { .. }
            | SelectTarget { .. }
            | CreateWorkspace { .. }
            | CreateTab { .. }
            | CreatePane { .. }
            | SplitPane { .. }
            | RenameTarget { .. }
            | CloseTarget { .. }
            | AcknowledgeAgent { .. } => Capability::Interactive,
            _ => return false,
        };
        self.contains(capability)
    }

    pub fn allows_server(self, message: &ServerMessage) -> bool {
        use ServerMessage::*;
        let capability = match message {
            Detached | Error { .. } | EndpointError { .. } => return true,
            Resources { .. } | ResourcesChanged { .. } | PresenceChanged { .. } => {
                Capability::Metadata
            }
            LocationOpened { .. } => Capability::ProjectOpen,
            WorkspaceMoved { .. } => Capability::WorkspaceOrder,
            Pong { .. } => Capability::Health,
            CommandReply { .. } => Capability::ConfiguredCommands,
            KittyImage { .. } => Capability::LargeScreens,
            SnapshotDelta { delta, .. } if delta.shift.is_some() => {
                return self.contains(Capability::Interactive)
                    && self.contains(Capability::ScrollDeltas);
            }
            ScreenBegin { .. } | ScreenRows { .. } | ScreenEnd { .. } => {
                return self.contains(Capability::Interactive) && self.chunked_screens();
            }
            ExtensionCatalog { .. } | ExtensionCatalogChanged { .. } => {
                Capability::ExtensionCatalog
            }
            AlertsChanged { .. } => {
                return self.contains(Capability::Alerts)
                    || self.contains(Capability::ControlAlerts);
            }
            CommandCompleted {
                command: super::AcknowledgedCommand::AcknowledgeAlerts,
            } => {
                return self.contains(Capability::Alerts)
                    || self.contains(Capability::ControlAlerts);
            }
            CommandCompleted {
                command: super::AcknowledgedCommand::Paste,
            } if self.contains(Capability::Attachments) => return true,
            CommandCompleted {
                command:
                    super::AcknowledgedCommand::Input
                    | super::AcknowledgedCommand::Paste
                    | super::AcknowledgedCommand::AcknowledgeAgent
                    | super::AcknowledgedCommand::CloseTarget
                    | super::AcknowledgedCommand::RenameTarget,
            } => Capability::Interactive,
            WorkspaceCreated { .. }
            | TabCreated { .. }
            | PaneCreated { .. }
            | TargetRenamed { .. }
            | TargetSelected { .. }
            | Snapshot { .. }
            | SnapshotDelta { .. }
            | CopyModeSnapshot { .. }
            | CopyModePrepared { .. }
            | CopyModeFinalized { .. }
            | CopyModeCancelled { .. }
            | CopyModeError { .. }
            | TerminalExited { .. }
            | TerminalResized { .. } => Capability::Interactive,
            _ => return false,
        };
        self.contains(capability)
    }
}

impl RemoteHello {
    pub fn new(mode: ClientMode, client_version: impl Into<String>) -> Self {
        let mut required = vec![Capability::Metadata.name().into()];
        if matches!(mode, ClientMode::Interactive { .. }) {
            required.push(Capability::Interactive.name().into());
        }
        let alerts = if matches!(mode, ClientMode::Control) {
            Capability::ControlAlerts
        } else {
            Capability::Alerts
        };
        let mut optional = vec![
            Capability::Health.name().into(),
            alerts.name().into(),
            Capability::ExtensionCatalog.name().into(),
            Capability::NestedWorkspaces.name().into(),
            Capability::ProjectOpen.name().into(),
            Capability::ConfiguredCommands.name().into(),
            Capability::WorkspaceOrder.name().into(),
        ];
        if matches!(mode, ClientMode::Interactive { .. }) {
            optional.push(Capability::TerminalColors.name().into());
            optional.push(Capability::Attachments.name().into());
            optional.push(Capability::LargeScreens.name().into());
            optional.push(Capability::ScrollDeltas.name().into());
            optional.push(Capability::ChunkedScreens.name().into());
        }
        Self {
            generation: GENERATION,
            codec: CODEC.into(),
            client_version: client_version.into(),
            required,
            optional,
            mode,
        }
    }

    pub fn negotiate(&self, supported: Capabilities) -> Result<Capabilities, EndpointError> {
        validate_version(&self.client_version)?;
        validate_names(self.required.iter().chain(&self.optional))?;
        validate_name(&self.codec)?;
        if self.generation != GENERATION {
            return Err(EndpointError::IncompatibleGeneration {
                client: self.generation,
                server: GENERATION,
            });
        }
        if self.codec != CODEC {
            return Err(EndpointError::UnsupportedCodec);
        }
        let supported = supported.names();
        if self.required.iter().any(|name| !supported.contains(name)) {
            return Err(EndpointError::MissingRequiredCapability);
        }
        // A mode's baseline cannot be made optional by a malformed offer.
        if !self
            .required
            .iter()
            .any(|name| name == Capability::Metadata.name())
            || matches!(self.mode, ClientMode::Interactive { .. })
                && !self
                    .required
                    .iter()
                    .any(|name| name == Capability::Interactive.name())
        {
            return Err(EndpointError::MissingRequiredCapability);
        }
        if matches!(self.mode, ClientMode::Control)
            && self
                .required
                .iter()
                .chain(&self.optional)
                .any(|name| name == Capability::Interactive.name())
        {
            return Err(EndpointError::InvalidHandshake);
        }
        let selected = self
            .required
            .iter()
            .chain(&self.optional)
            .filter(|name| supported.contains(name))
            .cloned()
            .collect::<Vec<_>>();
        let selected = Capabilities::from_names(&selected);
        if let ClientMode::Interactive { size, .. } = &self.mode {
            size.validate_within(selected.max_visible_cells())
                .map_err(|_| EndpointError::InvalidHandshake)?;
        }
        Ok(selected)
    }

    pub fn accept(&self, welcome: &RemoteWelcome) -> Result<Capabilities, EndpointError> {
        validate_version(&welcome.server_version)?;
        validate_names(welcome.capabilities.iter())?;
        validate_name(&welcome.codec)?;
        if welcome.generation != self.generation {
            return Err(EndpointError::IncompatibleGeneration {
                client: self.generation,
                server: welcome.generation,
            });
        }
        if welcome.codec != self.codec {
            return Err(EndpointError::UnsupportedCodec);
        }
        let capabilities = Capabilities::from_names(&welcome.capabilities);
        if welcome
            .capabilities
            .iter()
            .any(|name| !self.required.contains(name) && !self.optional.contains(name))
            || capabilities.names().len() != welcome.capabilities.len()
        {
            return Err(EndpointError::InvalidHandshake);
        }
        if self
            .required
            .iter()
            .any(|name| !welcome.capabilities.contains(name))
        {
            return Err(EndpointError::MissingRequiredCapability);
        }
        if matches!(self.mode, ClientMode::Interactive { .. }) != welcome.selected.is_some()
            || capabilities.contains(Capability::ExtensionCatalog)
                != welcome.extension_catalog.is_some()
        {
            return Err(EndpointError::InvalidHandshake);
        }
        Ok(capabilities)
    }
}

pub(super) fn validate_version(version: &str) -> Result<(), EndpointError> {
    if version.is_empty()
        || version.len() > MAX_VERSION_BYTES
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-+_".contains(&byte))
    {
        return Err(EndpointError::InvalidHandshake);
    }
    Ok(())
}

pub(super) fn validate_name(name: &str) -> Result<(), EndpointError> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
    {
        return Err(EndpointError::InvalidHandshake);
    }
    Ok(())
}

pub(super) fn validate_names<'a>(
    names: impl Iterator<Item = &'a String>,
) -> Result<(), EndpointError> {
    let mut seen = HashSet::new();
    for name in names {
        validate_name(name)?;
        if !seen.insert(name) || seen.len() > MAX_CAPABILITIES {
            return Err(EndpointError::InvalidHandshake);
        }
    }
    Ok(())
}

pub(super) fn capability_list<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    struct List;
    impl<'de> de::Visitor<'de> for List {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {MAX_CAPABILITIES} capability names")
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            if seq.size_hint().is_some_and(|size| size > MAX_CAPABILITIES) {
                return Err(de::Error::custom("too many capabilities"));
            }
            let mut names = Vec::new();
            while let Some(name) = seq.next_element::<String>()? {
                validate_name(&name).map_err(de::Error::custom)?;
                if names.len() == MAX_CAPABILITIES || names.contains(&name) {
                    return Err(de::Error::custom("too many or duplicate capabilities"));
                }
                names.push(name);
            }
            Ok(names)
        }
    }
    deserializer.deserialize_seq(List)
}

/// Strict decoding for negotiated local and remote handshake boundaries;
/// retain the existing fast slice decoder for terminal traffic.
pub fn decode_handshake<T: de::DeserializeOwned>(payload: &[u8]) -> Result<T, EndpointError> {
    super::validate_payload_len(payload.len()).map_err(|_| EndpointError::InvalidHandshake)?;
    let mut decoder = rmp_serde::Deserializer::new(std::io::Cursor::new(payload));
    let value = T::deserialize(&mut decoder).map_err(|_| EndpointError::InvalidHandshake)?;
    if decoder.position() != payload.len() as u64 {
        return Err(EndpointError::InvalidHandshake);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ProgramStatus, StatusRecord, StatusSource, StatusState};
    use crate::protocol::{Envelope, MAX_FRAME_LEN, codec, encode_payload};
    use serde_json::json;
    use tokio_util::codec::Decoder;

    #[test]
    fn generation_two_handshake_wire_fixture_is_independent_of_package_version() {
        let fixture = json!({"message": {
            "type": "remote_hello", "generation": 2, "codec": "msgpack-map-v1",
            "client_version": "0.1.2", "required": ["metadata.v2"],
            "optional": ["future.v1"], "mode": "control"
        }});
        let envelope: Envelope<ClientMessage> =
            decode_handshake(&encode_payload(&fixture).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&envelope).unwrap(), fixture);
        let ClientMessage::RemoteHello(hello) = envelope.message else {
            panic!()
        };
        let caps = hello.negotiate(Capabilities::ALL).unwrap();
        assert_eq!(caps.names(), ["metadata.v2"]);
        let fixture = json!({"message": {
            "type": "remote_welcome", "generation": 2, "codec": "msgpack-map-v1",
            "server_version": "9.123.4-dev", "capabilities": ["metadata.v2"],
            "selected": null, "extension_catalog": null
        }});
        let envelope: Envelope<ServerMessage> =
            decode_handshake(&encode_payload(&fixture).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&envelope).unwrap(), fixture);
        let ServerMessage::RemoteWelcome(welcome) = envelope.message else {
            panic!()
        };
        assert_eq!(hello.accept(&welcome).unwrap(), caps);
        assert_eq!(GENERATION, 2);
    }

    #[test]
    fn generation_two_allowed_message_shapes_are_pinned() {
        let client = [
            json!({"message": {"type": "watch_resources"}}),
            json!({"message": {"type": "ping"}}),
            json!({"message": {"type": "input", "bytes": [0, 255]}}),
            json!({"message": {"type": "detach"}}),
        ];
        for fixture in client {
            let envelope: Envelope<ClientMessage> =
                decode_handshake(&encode_payload(&fixture).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(&envelope).unwrap(), fixture);
            assert!(Capabilities::ALL.allows_client(&envelope.message));
        }

        let server = [
            json!({"message": {
                "type": "resources",
                "snapshot": {"revision": 1, "sessions": []},
                "presence": {"revision": 2, "sessions": []}
            }}),
            json!({"message": {
                "type": "alerts_changed",
                "snapshot": {"revision": 3, "terminals": []}
            }}),
            json!({"message": {"type": "pong", "daemon_pid": 7}}),
            json!({"message": {"type": "detached"}}),
        ];
        for fixture in server {
            let envelope: Envelope<ServerMessage> =
                decode_handshake(&encode_payload(&fixture).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(&envelope).unwrap(), fixture);
            assert!(Capabilities::ALL.allows_server(&envelope.message));
        }
    }

    #[test]
    fn metadata_v2_program_status_shape_is_pinned() {
        let status = ProgramStatus {
            records: vec![StatusRecord {
                id: "build/test".into(),
                state: StatusState::Done,
                kind: None,
                progress: None,
                app: Some("cargo".into()),
                title: None,
                message: Some("Passed".into()),
                revision: 7,
                updated_at_ms: 123,
                attention_revision: 7,
            }],
            source: Some(StatusSource::Report {
                source: Some("pi".into()),
                agent_session_id: None,
            }),
            revision: 7,
            attention_revision: 7,
            read_revision: 0,
            working_revision: 6,
        };
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({
                "records": [{
                    "id": "build/test",
                    "state": "done",
                    "app": "cargo",
                    "message": "Passed",
                    "revision": 7,
                    "updated_at_ms": 123,
                    "attention_revision": 7
                }],
                "source": {"type": "report", "source": "pi"},
                "revision": 7,
                "attention_revision": 7,
                "working_revision": 6
            })
        );
    }

    #[test]
    fn offers_validate_bounds_duplicates_and_diagnostic_versions() {
        let hello = RemoteHello::new(ClientMode::Control, "0.1.0");
        for version in [
            "",
            "0.1.0\n",
            "\x1b[2J",
            "version with spaces",
            "v\u{202e}1",
            &"x".repeat(MAX_VERSION_BYTES + 1),
        ] {
            let mut bad = hello.clone();
            bad.client_version = version.into();
            assert_eq!(
                bad.negotiate(Capabilities::ALL),
                Err(EndpointError::InvalidHandshake)
            );
        }
        for name in [
            "",
            "invalid_name",
            "CAP",
            "\x1b",
            &"x".repeat(MAX_NAME_BYTES + 1),
        ] {
            let mut bad = hello.clone();
            bad.optional = vec![name.into()];
            assert_eq!(
                bad.negotiate(Capabilities::ALL),
                Err(EndpointError::InvalidHandshake)
            );
        }
        let mut bad = hello.clone();
        bad.optional.push("metadata.v2".into());
        assert_eq!(
            bad.negotiate(Capabilities::ALL),
            Err(EndpointError::InvalidHandshake)
        );
        bad.optional = (0..MAX_CAPABILITIES)
            .map(|i| format!("future-{i}"))
            .collect();
        assert_eq!(
            bad.negotiate(Capabilities::ALL),
            Err(EndpointError::InvalidHandshake)
        );
        bad.optional.pop();
        assert!(bad.negotiate(Capabilities::ALL).is_ok());
        bad.required.push("future-required.v1".into());
        bad.optional.clear();
        assert_eq!(
            bad.negotiate(Capabilities::ALL),
            Err(EndpointError::MissingRequiredCapability)
        );
        let mut bad = RemoteHello::new(ClientMode::Control, "0.1.0");
        bad.optional.push(Capability::Interactive.name().into());
        assert_eq!(
            bad.negotiate(Capabilities::ALL),
            Err(EndpointError::InvalidHandshake)
        );
        assert!(
            Capabilities::ALL.allows_server(&ServerMessage::CommandCompleted {
                command: super::super::AcknowledgedCommand::RenameTarget,
            })
        );
    }

    #[test]
    fn malformed_and_oversized_handshakes_are_rejected() {
        let hello = RemoteHello::new(ClientMode::Control, "0.1.0");
        let mut fixture = serde_json::to_value(&hello).unwrap();
        for field in [
            "generation",
            "codec",
            "required",
            "optional",
            "mode",
            "client_version",
        ] {
            let mut bad = fixture.clone();
            bad.as_object_mut().unwrap().remove(field);
            assert!(
                decode_handshake::<RemoteHello>(&encode_payload(&bad).unwrap()).is_err(),
                "{field}"
            );
        }
        fixture["optional"] = json!(vec!["health.v1"; MAX_CAPABILITIES + 1]);
        assert!(decode_handshake::<RemoteHello>(&encode_payload(&fixture).unwrap()).is_err());
        fixture["optional"] = json!(["health.v1", "health.v1"]);
        assert!(decode_handshake::<RemoteHello>(&encode_payload(&fixture).unwrap()).is_err());
        fixture["optional"] = json!([]);
        fixture["unexpected"] = json!(true);
        assert!(decode_handshake::<RemoteHello>(&encode_payload(&fixture).unwrap()).is_err());
        let welcome = json!({"generation": 2, "codec": CODEC, "server_version": "0.1.0",
            "capabilities": ["metadata.v2"], "selected": null, "extension_catalog": null});
        for field in [
            "generation",
            "codec",
            "server_version",
            "capabilities",
            "selected",
            "extension_catalog",
        ] {
            let mut bad = welcome.clone();
            bad.as_object_mut().unwrap().remove(field);
            assert!(
                decode_handshake::<RemoteWelcome>(&encode_payload(&bad).unwrap()).is_err(),
                "{field}"
            );
        }
        let mut trailing = encode_payload(&hello).unwrap();
        trailing.push(0);
        for bytes in [vec![], vec![0xc1], trailing, vec![0; MAX_FRAME_LEN + 1]] {
            assert_eq!(
                decode_handshake::<RemoteHello>(&bytes),
                Err(EndpointError::InvalidHandshake)
            );
        }
        // The length prefix is rejected before buffering/allocating its body.
        let mut bytes =
            bytes::BytesMut::from(((MAX_FRAME_LEN + 1) as u32).to_be_bytes().as_slice());
        assert!(codec().decode(&mut bytes).is_err());
    }

    #[test]
    fn terminal_colors_require_their_own_optional_capability() {
        let mode = ClientMode::Interactive {
            size: crate::domain::TerminalSize {
                columns: 80,
                rows: 24,
            },
            selector: None,
        };
        let offer = RemoteHello::new(mode, "0.26.0");
        let message = ClientMessage::TerminalColors {
            terminal_id: crate::domain::TerminalId::new(),
            colors: Default::default(),
        };
        assert!(
            offer
                .optional
                .iter()
                .any(|name| name == "terminal-colors.v1")
        );
        assert!(
            offer
                .negotiate(Capabilities::ALL)
                .unwrap()
                .allows_client(&message)
        );
        let mut old = offer;
        old.optional.clear();
        let selected = old.negotiate(Capabilities::ALL).unwrap();
        assert!(!selected.allows_client(&message));
        assert!(selected.contains(Capability::Interactive));
    }

    #[test]
    fn attachments_require_explicit_optional_negotiation() {
        let mut hello = RemoteHello::new(
            ClientMode::Interactive {
                size: crate::domain::TerminalSize {
                    columns: 80,
                    rows: 24,
                },
                selector: None,
            },
            "0.1.0",
        );
        let transfer_id = uuid::Uuid::new_v4();
        let messages = [
            ClientMessage::AttachmentBegin {
                transfer_id,
                terminal_id: crate::domain::TerminalId::new(),
                size: 8,
            },
            ClientMessage::AttachmentChunk {
                transfer_id,
                bytes: vec![0; 8],
            },
            ClientMessage::AttachmentCommit { transfer_id },
            ClientMessage::AttachmentCancel { transfer_id },
        ];
        let selected = hello.negotiate(Capabilities::ALL).unwrap();
        assert!(selected.contains(Capability::Attachments));
        for message in &messages {
            assert!(selected.allows_client(message));
        }
        hello
            .optional
            .retain(|name| name != Capability::Attachments.name());
        let legacy = hello.negotiate(Capabilities::ALL).unwrap();
        for message in &messages {
            assert!(!legacy.allows_client(message));
        }
    }

    #[test]
    fn large_screens_are_optional_and_older_peers_keep_the_generation_one_bounds() {
        let interactive = |columns, rows| ClientMode::Interactive {
            size: crate::domain::TerminalSize { columns, rows },
            selector: None,
        };
        let image = ServerMessage::KittyImage {
            terminal_id: crate::domain::TerminalId::new(),
            image: crate::domain::KittyImage {
                id: 1,
                generation: 1,
                png: vec![1].into(),
            },
        };
        assert!(
            !RemoteHello::new(ClientMode::Control, "0.35.0")
                .optional
                .iter()
                .any(|name| name == "large-screens.v1")
        );

        // 508x160 is above the generation-1 bound but within the new one.
        let offer = RemoteHello::new(interactive(508, 160), "0.35.0");
        assert!(offer.optional.iter().any(|name| name == "large-screens.v1"));
        let selected = offer.negotiate(Capabilities::ALL).unwrap();
        assert!(selected.separate_images());
        assert!(selected.allows_server(&image));
        assert_eq!(selected.max_visible_cells(), crate::domain::MAX_GRID_CELLS);

        // A daemon that predates the capability ignores the optional offer.
        let older_daemon = Capabilities::from_names(
            &Capabilities::ALL
                .names()
                .into_iter()
                .filter(|name| name != "large-screens.v1")
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            offer.negotiate(older_daemon),
            Err(EndpointError::InvalidHandshake)
        );

        // An older client never offers it: embedded images, original bound.
        let mut old = RemoteHello::new(interactive(250, 200), "0.34.0");
        old.optional.retain(|name| name != "large-screens.v1");
        let selected = old.negotiate(Capabilities::ALL).unwrap();
        assert!(!selected.separate_images());
        assert!(!selected.allows_server(&image));
        assert_eq!(
            selected.max_visible_cells(),
            crate::domain::LEGACY_MAX_VISIBLE_CELLS
        );
        old.mode = interactive(508, 160);
        assert_eq!(
            old.negotiate(Capabilities::ALL),
            Err(EndpointError::InvalidHandshake)
        );
    }

    #[test]
    fn scroll_deltas_are_optional_and_older_peers_never_receive_row_shifts() {
        use crate::domain::{Cursor, RowShift, ScreenDelta, TerminalId, TerminalSize};
        let size = TerminalSize {
            columns: 80,
            rows: 24,
        };
        let delta = |shift| ServerMessage::SnapshotDelta {
            terminal_id: TerminalId::new(),
            delta: ScreenDelta {
                revision: 2,
                base_revision: 1,
                size,
                shift,
                rows: Vec::new(),
                hyperlinks: Vec::new(),
                cursor: Cursor {
                    column: 0,
                    row: 0,
                    visible: true,
                    shape: Default::default(),
                    blinking: false,
                },
                scroll: Default::default(),
                mouse_tracking: false,
                graphics: None,
            },
        };
        let plain = delta(None);
        let shifted = delta(Some(RowShift {
            top: 0,
            bottom: 24,
            by: 3,
        }));
        assert!(
            !RemoteHello::new(ClientMode::Control, "0.38.0")
                .optional
                .iter()
                .any(|name| name == "scroll-deltas.v1")
        );
        let interactive = ClientMode::Interactive {
            size,
            selector: None,
        };
        let offer = RemoteHello::new(interactive, "0.38.0");
        assert!(offer.optional.iter().any(|name| name == "scroll-deltas.v1"));
        let selected = offer.negotiate(Capabilities::ALL).unwrap();
        assert!(selected.allows_server(&plain));
        assert!(selected.allows_server(&shifted));

        // An older client never offers it, and a daemon that predates it
        // never selects it: both keep generation-1 deltas.
        let mut old = offer.clone();
        old.optional.retain(|name| name != "scroll-deltas.v1");
        let older_daemon = Capabilities::from_names(
            &Capabilities::ALL
                .names()
                .into_iter()
                .filter(|name| name != "scroll-deltas.v1")
                .collect::<Vec<_>>(),
        );
        for selected in [
            old.negotiate(Capabilities::ALL).unwrap(),
            offer.negotiate(older_daemon).unwrap(),
        ] {
            assert!(!selected.contains(Capability::ScrollDeltas));
            assert!(selected.allows_server(&plain));
            assert!(!selected.allows_server(&shifted));
        }
    }

    #[test]
    fn chunked_screens_are_optional_and_older_peers_keep_their_single_frame_bound() {
        use crate::domain::{TerminalId, TerminalSize};
        let interactive = |columns, rows| ClientMode::Interactive {
            size: TerminalSize { columns, rows },
            selector: None,
        };
        let terminal_id = TerminalId::new();
        let chunks = [
            ServerMessage::ScreenRows {
                terminal_id,
                revision: 1,
                rows: Vec::new(),
            },
            ServerMessage::ScreenEnd {
                terminal_id,
                revision: 1,
            },
        ];
        assert!(
            !RemoteHello::new(ClientMode::Control, "0.38.0")
                .optional
                .iter()
                .any(|name| name == "chunked-screens.v1")
        );

        // 508x320 is above the single-frame bound.
        let offer = RemoteHello::new(interactive(508, 320), "0.38.0");
        assert!(
            offer
                .optional
                .iter()
                .any(|name| name == "chunked-screens.v1")
        );
        let selected = offer.negotiate(Capabilities::ALL).unwrap();
        assert!(selected.chunked_screens());
        assert!(chunks.iter().all(|chunk| selected.allows_server(chunk)));
        assert_eq!(selected.max_visible_cells(), crate::domain::MAX_GRID_CELLS);
        let without = |name: &str| {
            Capabilities::from_names(
                &Capabilities::ALL
                    .names()
                    .into_iter()
                    .filter(|selected| selected != name)
                    .collect::<Vec<_>>(),
            )
        };
        // A daemon that predates the capability ignores the optional offer,
        // so the larger hello is rejected as before.
        assert_eq!(
            offer.negotiate(without("chunked-screens.v1")),
            Err(EndpointError::InvalidHandshake)
        );

        // An older client never offers it and keeps the single-frame bound.
        let mut old = RemoteHello::new(interactive(500, 300), "0.37.0");
        old.optional.retain(|name| name != "chunked-screens.v1");
        let selected = old.negotiate(Capabilities::ALL).unwrap();
        assert!(!selected.chunked_screens());
        assert!(!chunks.iter().any(|chunk| selected.allows_server(chunk)));
        assert_eq!(
            selected.max_visible_cells(),
            crate::domain::MAX_VISIBLE_CELLS
        );
        old.mode = interactive(508, 320);
        assert_eq!(
            old.negotiate(Capabilities::ALL),
            Err(EndpointError::InvalidHandshake)
        );

        // Chunk headers reference separately sent images, so chunking also
        // needs `large-screens.v1`; without it the generation-1 bound holds.
        let mut no_large = RemoteHello::new(interactive(250, 200), "0.38.0");
        no_large.optional.retain(|name| name != "large-screens.v1");
        let selected = no_large.negotiate(Capabilities::ALL).unwrap();
        assert!(selected.contains(Capability::ChunkedScreens));
        assert!(!selected.chunked_screens());
        assert!(!chunks.iter().any(|chunk| selected.allows_server(chunk)));
        assert_eq!(
            selected.max_visible_cells(),
            crate::domain::LEGACY_MAX_VISIBLE_CELLS
        );
    }

    #[test]
    fn configured_commands_are_independently_gated_and_round_trip() {
        use crate::protocol::{RemoteCommandOperation, RemoteCommandReply};
        let operation = ClientMessage::CommandOperation {
            operation: RemoteCommandOperation::Paste("remote text".into()),
        };
        let done = ServerMessage::CommandReply {
            reply: RemoteCommandReply::Done {
                exit_code: Some(7),
                activated_target: None,
                failure_output: Some("failure".into()),
            },
        };
        assert!(!Capabilities::default().allows_client(&operation));
        assert!(!Capabilities::default().allows_server(&done));
        let hello = RemoteHello::new(ClientMode::Control, "test");
        let caps = hello.negotiate(Capabilities::ALL).unwrap();
        assert!(caps.allows_client(&operation));
        assert!(caps.allows_server(&done));
        let encoded = rmp_serde::to_vec_named(&operation).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<ClientMessage>(&encoded).unwrap(),
            operation
        );
        let encoded = rmp_serde::to_vec_named(&done).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<ServerMessage>(&encoded).unwrap(),
            done
        );
    }

    #[test]
    fn capability_selection_is_an_exact_known_subset_and_optional_features_are_independent() {
        let hello = RemoteHello::new(ClientMode::Control, "0.1.0");
        let mut welcome = RemoteWelcome {
            generation: GENERATION,
            codec: CODEC.into(),
            server_version: "0.999.0".into(),
            capabilities: hello.required.clone(),
            selected: None,
            extension_catalog: None,
        };
        let caps = hello.accept(&welcome).unwrap();
        assert!(caps.allows_client(&ClientMessage::WatchResources));
        assert!(!caps.allows_client(&ClientMessage::Ping));
        assert!(!caps.allows_client(&ClientMessage::GetExtensionCatalog));
        assert!(!caps.allows_server(&ServerMessage::AlertsChanged {
            snapshot: Default::default()
        }));
        welcome.capabilities.push("health.v1".into());
        let caps = hello.accept(&welcome).unwrap();
        assert!(caps.allows_client(&ClientMessage::Ping));
        assert!(!caps.allows_client(&ClientMessage::GetExtensionCatalog));
        assert!(!caps.allows_client(&ClientMessage::Shutdown));
        assert!(!Capabilities::ALL.allows_client(&ClientMessage::Shutdown));
        assert!(!Capabilities::ALL.allows_client(&ClientMessage::ReloadExtensions));
        welcome.capabilities.push("interactive.v1".into()); // Known but not offered.
        assert_eq!(hello.accept(&welcome), Err(EndpointError::InvalidHandshake));
        welcome.capabilities.pop();
        welcome.capabilities.push("extension-catalog.v1".into()); // Missing its payload.
        assert_eq!(hello.accept(&welcome), Err(EndpointError::InvalidHandshake));
    }
}
