//! Frozen remote generation 1. Package versions are diagnostics, never a
//! compatibility decision. See docs/remote-protocol.md before changing any
//! reused payload or adding a method to the allowlists below.

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize, de};
use thiserror::Error;

use super::{ClientMessage, ClientMode, ExtensionCatalog, SelectedView, ServerMessage};

pub const GENERATION: u16 = 1;
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
    #[error("incompatible remote generation: client {client}, server {server}")]
    IncompatibleGeneration { client: u16, server: u16 },
    #[error("unsupported remote codec")]
    UnsupportedCodec,
    #[error("remote endpoint is missing a required capability")]
    MissingRequiredCapability,
    #[error("invalid remote handshake")]
    InvalidHandshake,
    #[error("remote method was not negotiated")]
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
}

impl Capability {
    pub const ALL: [Self; 7] = [
        Self::Metadata,
        Self::NestedWorkspaces,
        Self::Interactive,
        Self::Health,
        Self::Alerts,
        Self::ControlAlerts,
        Self::ExtensionCatalog,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Metadata => "metadata.v1",
            Self::NestedWorkspaces => "nested-workspaces.v1",
            Self::Interactive => "interactive.v1",
            Self::Health => "health.v1",
            Self::Alerts => "alerts.v1",
            Self::ControlAlerts => "control-alerts.v1",
            Self::ExtensionCatalog => "extension-catalog.v1",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities(u8);

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
            Ping => Capability::Health,
            GetExtensionCatalog => Capability::ExtensionCatalog,
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
            Pong { .. } => Capability::Health,
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
        Self {
            generation: GENERATION,
            codec: CODEC.into(),
            client_version: client_version.into(),
            required,
            optional: vec![
                Capability::Health.name().into(),
                alerts.name().into(),
                Capability::ExtensionCatalog.name().into(),
                Capability::NestedWorkspaces.name().into(),
            ],
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
        if let ClientMode::Interactive { size, .. } = &self.mode {
            size.validate()
                .map_err(|_| EndpointError::InvalidHandshake)?;
        }
        let selected = self
            .required
            .iter()
            .chain(&self.optional)
            .filter(|name| supported.contains(name))
            .cloned()
            .collect::<Vec<_>>();
        Ok(Capabilities::from_names(&selected))
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

fn validate_version(version: &str) -> Result<(), EndpointError> {
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

fn validate_name(name: &str) -> Result<(), EndpointError> {
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

fn validate_names<'a>(names: impl Iterator<Item = &'a String>) -> Result<(), EndpointError> {
    let mut seen = HashSet::new();
    for name in names {
        validate_name(name)?;
        if !seen.insert(name) || seen.len() > MAX_CAPABILITIES {
            return Err(EndpointError::InvalidHandshake);
        }
    }
    Ok(())
}

fn capability_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
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

/// Strict decoding is only needed at the remote handshake boundary; retain
/// the existing fast slice decoder for terminal traffic and the local protocol.
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
    use crate::protocol::{Envelope, MAX_FRAME_LEN, codec, encode_payload};
    use serde_json::json;
    use tokio_util::codec::Decoder;

    #[test]
    fn generation_one_handshake_wire_fixture_is_independent_of_package_version() {
        let fixture = json!({"message": {
            "type": "remote_hello", "generation": 1, "codec": "msgpack-map-v1",
            "client_version": "0.1.2", "required": ["metadata.v1"],
            "optional": ["future.v1"], "mode": "control"
        }});
        let envelope: Envelope<ClientMessage> =
            decode_handshake(&encode_payload(&fixture).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&envelope).unwrap(), fixture);
        let ClientMessage::RemoteHello(hello) = envelope.message else {
            panic!()
        };
        let caps = hello.negotiate(Capabilities::ALL).unwrap();
        assert_eq!(caps.names(), ["metadata.v1"]);
        let fixture = json!({"message": {
            "type": "remote_welcome", "generation": 1, "codec": "msgpack-map-v1",
            "server_version": "9.123.4-dev", "capabilities": ["metadata.v1"],
            "selected": null, "extension_catalog": null
        }});
        let envelope: Envelope<ServerMessage> =
            decode_handshake(&encode_payload(&fixture).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&envelope).unwrap(), fixture);
        let ServerMessage::RemoteWelcome(welcome) = envelope.message else {
            panic!()
        };
        assert_eq!(hello.accept(&welcome).unwrap(), caps);
        assert_eq!(GENERATION, 1);
    }

    #[test]
    fn generation_one_allowed_message_shapes_are_pinned() {
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
        bad.optional.push("metadata.v1".into());
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
        let welcome = json!({"generation": 1, "codec": CODEC, "server_version": "0.1.0",
            "capabilities": ["metadata.v1"], "selected": null, "extension_catalog": null});
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
