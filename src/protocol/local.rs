//! Stable local generation 1. `local.v1` freezes the complete current local
//! contract, including metadata, extensions, images, deltas and chunked screens.
//! Package versions are diagnostic only. New behavior needs deliberate mapping
//! here; it must not silently become part of an existing baseline.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{ClientMessage, ClientMode, ExtensionCatalog, SelectedView, ServerMessage, remote};
use remote::{EndpointError, capability_list, validate_name, validate_names, validate_version};

pub const GENERATION: u16 = 1;
pub const CODEC: &str = "msgpack-map-v1";
pub const BASELINE: &str = "local.v1";
pub const WORKSPACE_ORDER: &str = "workspace-order.v1";
pub const PANE_TITLES: &str = "pane-titles.v1";

/// An older daemon cannot decode local_hello, so its negative response cannot
/// echo the request ID. Never treat a malformed welcome as a legacy rejection.
pub fn is_legacy_rejection(request_id: Option<uuid::Uuid>, error: &EndpointError) -> bool {
    request_id.is_none() && *error == EndpointError::InvalidHandshake
}

#[derive(Debug, Error)]
#[error(
    "daemon does not support local negotiation; run `fut daemon shutdown` with the same socket override, if any, then retry"
)]
pub struct UnsupportedHandshake;

impl UnsupportedHandshake {
    /// Context preserves downcasts to both the legacy marker and the endpoint
    /// error; a custom std::error::Error source would not.
    pub fn error() -> anyhow::Error {
        anyhow::Error::new(EndpointError::InvalidHandshake).context(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalHello {
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
pub struct LocalWelcome {
    pub generation: u16,
    pub codec: String,
    pub server_version: String,
    #[serde(deserialize_with = "capability_list")]
    pub capabilities: Vec<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub selected: Option<SelectedView>,
    pub extension_catalog: ExtensionCatalog,
}

/// The selected local contract. The default is the baseline alone, which is
/// all a legacy welcome can imply.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    workspace_order: bool,
    pane_titles: bool,
}

impl Capabilities {
    pub fn workspace_order(self) -> bool {
        self.workspace_order
    }

    pub fn pane_titles(self) -> bool {
        self.pane_titles
    }

    pub fn names(self) -> Vec<String> {
        let mut names = vec![BASELINE.into()];
        if self.workspace_order {
            names.push(WORKSPACE_ORDER.into());
        }
        if self.pane_titles {
            names.push(PANE_TITLES.into());
        }
        names
    }

    /// Intentionally exhaustive: every new method needs an explicit capability
    /// decision rather than inheriting local privileges through a wildcard.
    pub fn allows_client(self, message: &ClientMessage) -> bool {
        use ClientMessage::*;
        match message {
            Hello { .. } | LocalHello(_) | RemoteHello(_) => false,
            MoveWorkspace { .. } => self.workspace_order,
            RenameTarget {
                selector: super::RenameSelector::Pane(_),
                ..
            }
            | Contextual {
                command: super::ContextualCommand::RenamePane { .. },
                ..
            } => self.pane_titles,
            CommandDefaults { .. }
            | CommandStart { .. }
            | CommandOperation { .. }
            | Input { .. }
            | KeyInput { .. }
            | Paste { .. }
            | AttachmentBegin { .. }
            | AttachmentChunk { .. }
            | AttachmentCommit { .. }
            | AttachmentCancel { .. }
            | TerminalInput { .. }
            | ReadTerminalOutput { .. }
            | WaitTerminalOutput { .. }
            | PromptAgent { .. }
            | WaitAgent { .. }
            | MouseInput { .. }
            | ResetViewport { .. }
            | RefreshTerminal { .. }
            | CopyMode { .. }
            | TerminalColors { .. }
            | Resize { .. }
            | ResizeSplit { .. }
            | SelectTarget { .. }
            | Detach
            | OpenProject { .. }
            | OpenLocation { .. }
            | CreateWorkspace { .. }
            | CreateTab { .. }
            | CreatePane { .. }
            | SplitPane { .. }
            | MovePane { .. }
            | Contextual { .. }
            | ListResources
            | GetExtensionCatalog
            | WatchResources
            | CloseTarget { .. }
            | RetireWorkspace { .. }
            | RenameTarget { .. }
            | PublishToken { .. }
            | PrepareExtensionReload
            | CommitExtensionReload { .. }
            | ReloadProjectConfig { .. }
            | ReloadExtensions
            | ReportAgent { .. }
            | ReportAutomaticAgent { .. }
            | AcknowledgeAgent { .. }
            | WatchAlerts { .. }
            | AcknowledgeAlerts { .. }
            | Ping
            | Shutdown => true,
        }
    }

    pub fn allows_server(self, message: &ServerMessage) -> bool {
        use ServerMessage::*;
        match message {
            Welcome { .. } | RemoteWelcome(_) | IncompatibleProtocol { .. } => false,
            WorkspaceMoved { .. } => self.workspace_order,
            LocalWelcome(_)
            | EndpointError { .. }
            | CommandReply { .. }
            | LocationOpened { .. }
            | WorkspaceCreated { .. }
            | TabCreated { .. }
            | PaneCreated { .. }
            | PaneMoved { .. }
            | TargetRenamed { .. }
            | TokenPublished { .. }
            | ProjectConfigReloaded { .. }
            | ExtensionReloadPrepared { .. }
            | ExtensionsReloaded { .. }
            | ExtensionCatalogChanged { .. }
            | ExtensionCatalog { .. }
            | TargetSelected { .. }
            | Resources { .. }
            | ResourcesChanged { .. }
            | PresenceChanged { .. }
            | AlertsChanged { .. }
            | Snapshot { .. }
            | SnapshotDelta { .. }
            | KittyImage { .. }
            | CopyModeSnapshot { .. }
            | ScreenBegin { .. }
            | ScreenRows { .. }
            | ScreenEnd { .. }
            | CopyModePrepared { .. }
            | CopyModeFinalized { .. }
            | CopyModeCancelled { .. }
            | CopyModeError { .. }
            | TerminalExited { .. }
            | TerminalOutput { .. }
            | TerminalOutputMatched { .. }
            | AgentPrompted { .. }
            | AgentSettled { .. }
            | Pong { .. }
            | TerminalResized { .. }
            | CommandCompleted { .. }
            | Detached
            | Error { .. } => true,
        }
    }
}

impl LocalHello {
    pub fn new(mode: ClientMode, client_version: impl Into<String>) -> Self {
        Self {
            generation: GENERATION,
            codec: CODEC.into(),
            client_version: client_version.into(),
            required: vec![BASELINE.into()],
            optional: vec![WORKSPACE_ORDER.into(), PANE_TITLES.into()],
            mode,
        }
    }

    pub fn negotiate(&self) -> Result<Capabilities, EndpointError> {
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
        if !self.required.iter().any(|name| name == BASELINE)
            || self
                .required
                .iter()
                .any(|name| name != BASELINE && name != WORKSPACE_ORDER && name != PANE_TITLES)
        {
            return Err(EndpointError::MissingRequiredCapability);
        }
        // Size is an operational error, not a compatibility failure. The
        // daemon validates it before leasing and returns descriptive invalid_size.
        Ok(Capabilities {
            workspace_order: self
                .required
                .iter()
                .chain(&self.optional)
                .any(|name| name == WORKSPACE_ORDER),
            pane_titles: self
                .required
                .iter()
                .chain(&self.optional)
                .any(|name| name == PANE_TITLES),
        })
    }

    pub fn accept(&self, welcome: &LocalWelcome) -> Result<Capabilities, EndpointError> {
        self.negotiate()?;
        validate_version(&welcome.server_version)?;
        validate_names(welcome.capabilities.iter())?;
        validate_name(&welcome.codec)?;
        if welcome.generation != GENERATION {
            return Err(EndpointError::IncompatibleGeneration {
                client: self.generation,
                server: welcome.generation,
            });
        }
        if welcome.codec != CODEC {
            return Err(EndpointError::UnsupportedCodec);
        }
        if self
            .required
            .iter()
            .any(|name| !welcome.capabilities.contains(name))
        {
            return Err(EndpointError::MissingRequiredCapability);
        }
        if welcome.capabilities.iter().any(|name| {
            (name != BASELINE && name != WORKSPACE_ORDER && name != PANE_TITLES)
                || !self
                    .required
                    .iter()
                    .chain(&self.optional)
                    .any(|offered| offered == name)
        }) || self
            .required
            .iter()
            .any(|name| !welcome.capabilities.contains(name))
            || matches!(self.mode, ClientMode::Interactive { .. }) != welcome.selected.is_some()
        {
            return Err(EndpointError::InvalidHandshake);
        }
        Ok(Capabilities {
            workspace_order: welcome
                .capabilities
                .iter()
                .any(|name| name == WORKSPACE_ORDER),
            pane_titles: welcome.capabilities.iter().any(|name| name == PANE_TITLES),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TerminalSize;
    use crate::protocol::{Envelope, encode_payload};
    use remote::decode_handshake;
    use serde_json::json;

    fn welcome() -> LocalWelcome {
        LocalWelcome {
            generation: GENERATION,
            codec: CODEC.into(),
            server_version: "99.123.4-dev".into(),
            capabilities: vec![BASELINE.into()],
            selected: None,
            extension_catalog: ExtensionCatalog {
                generation: 7,
                fingerprint: "fixture".into(),
                extensions: Vec::new(),
                config: Default::default(),
            },
        }
    }

    #[test]
    fn local_v1_wire_fixtures_are_independent_of_package_version() {
        let fixture = json!({"message": {
            "type": "local_hello", "generation": 1, "codec": "msgpack-map-v1",
            "client_version": "0.1.2", "required": ["local.v1"],
            "optional": ["future.v1"], "mode": "control"
        }});
        let envelope: Envelope<ClientMessage> =
            decode_handshake(&encode_payload(&fixture).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&envelope).unwrap(), fixture);
        let ClientMessage::LocalHello(hello) = envelope.message else {
            panic!("expected local hello")
        };
        assert_eq!(hello.negotiate().unwrap().names(), ["local.v1"]);
        let fixture = json!({"message": {
            "type": "local_welcome", "generation": 1, "codec": "msgpack-map-v1",
            "server_version": "99.123.4-dev", "capabilities": ["local.v1"],
            "selected": null, "extension_catalog": {
                "generation": 7, "fingerprint": "fixture", "extensions": [],
                "config": {"defaults": {}}
            }
        }});
        let envelope: Envelope<ServerMessage> =
            decode_handshake(&encode_payload(&fixture).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&envelope).unwrap(), fixture);
        let ServerMessage::LocalWelcome(received) = envelope.message else {
            panic!("expected local welcome")
        };
        assert_eq!(received, welcome());
        assert_eq!(hello.accept(&received).unwrap(), hello.negotiate().unwrap());
        assert_eq!(GENERATION, 1);
    }

    #[test]
    fn legacy_rejection_is_narrow_and_keeps_upgrade_guidance_typed() {
        assert!(is_legacy_rejection(None, &EndpointError::InvalidHandshake));
        assert!(!is_legacy_rejection(
            Some(uuid::Uuid::new_v4()),
            &EndpointError::InvalidHandshake
        ));
        assert!(!is_legacy_rejection(
            None,
            &EndpointError::MissingRequiredCapability
        ));
        let error = UnsupportedHandshake::error();
        assert!(error.to_string().contains("fut daemon shutdown"));
        assert!(error.downcast_ref::<UnsupportedHandshake>().is_some());
        assert_eq!(
            error.downcast_ref::<EndpointError>(),
            Some(&EndpointError::InvalidHandshake)
        );
    }

    #[test]
    fn baseline_is_required_and_unknown_optional_is_ignored() {
        let mut hello = LocalHello::new(ClientMode::Control, "0.1.0");
        hello.optional.clear();
        hello.optional.push("future.v1".into());
        assert_eq!(hello.negotiate().unwrap().names(), [BASELINE]);
        hello.required.push("future-required.v1".into());
        assert_eq!(
            hello.negotiate(),
            Err(EndpointError::MissingRequiredCapability)
        );
        hello.required.clear();
        hello.optional = vec![BASELINE.into()];
        assert_eq!(
            hello.negotiate(),
            Err(EndpointError::MissingRequiredCapability)
        );
    }

    #[test]
    fn pane_titles_are_optional_and_gate_new_selectors_and_contextual_semantics() {
        let mut hello = LocalHello::new(ClientMode::Control, "0.1.0");
        let selected = hello.negotiate().unwrap();
        assert!(selected.pane_titles);
        let explicit = ClientMessage::RenameTarget {
            selector: super::super::RenameSelector::Pane(crate::domain::PaneId::new()),
            name: "Review".into(),
        };
        assert!(selected.allows_client(&explicit));
        assert!(!hello.accept(&welcome()).unwrap().pane_titles);
        hello.optional.clear();
        let baseline = hello.negotiate().unwrap();
        assert!(!baseline.allows_client(&explicit));
        let contextual = ClientMessage::Contextual {
            context: super::super::TerminalContext {
                session_id: crate::domain::SessionId::new(),
                workspace_id: crate::domain::WorkspaceId::new(),
                tab_id: crate::domain::TabId::new(),
                pane_id: crate::domain::PaneId::new(),
                terminal_id: crate::domain::TerminalId::new(),
            },
            command: super::super::ContextualCommand::RenamePane {
                name: "Task".into(),
            },
        };
        assert!(selected.allows_client(&contextual));
        assert!(!baseline.allows_client(&contextual));
        let mut unoffered = welcome();
        unoffered.capabilities.push(PANE_TITLES.into());
        assert_eq!(
            hello.accept(&unoffered),
            Err(EndpointError::InvalidHandshake)
        );
        hello.required.push(PANE_TITLES.into());
        assert_eq!(
            hello.accept(&welcome()),
            Err(EndpointError::MissingRequiredCapability)
        );
    }

    #[test]
    fn malformed_offers_fail_in_memory_and_on_wire() {
        let valid = LocalHello::new(ClientMode::Control, "0.1.0");
        let mut offers = Vec::new();
        for name in ["", "LOCAL.v1", "has space", "bad/name"] {
            let mut hello = valid.clone();
            hello.optional.push(name.into());
            offers.push(hello);
        }
        let mut duplicate = valid.clone();
        duplicate.required.push(BASELINE.into());
        offers.push(duplicate);
        let mut overlap = valid.clone();
        overlap.optional.push(BASELINE.into());
        offers.push(overlap);
        let mut too_many = valid.clone();
        too_many.optional = (0..remote::MAX_CAPABILITIES)
            .map(|i| format!("future-{i}.v1"))
            .collect();
        offers.push(too_many);
        for hello in offers {
            assert_eq!(hello.negotiate(), Err(EndpointError::InvalidHandshake));
            let decoded = decode_handshake::<LocalHello>(&encode_payload(&hello).unwrap());
            assert!(decoded.is_err() || decoded.unwrap().negotiate().is_err());
        }
        for version in ["", "bad version", "bad/version"] {
            let hello = LocalHello::new(ClientMode::Control, version);
            assert_eq!(hello.negotiate(), Err(EndpointError::InvalidHandshake));
        }
        let mut hello = valid;
        hello.codec = "other-codec.v1".into();
        assert_eq!(hello.negotiate(), Err(EndpointError::UnsupportedCodec));
        hello.codec = CODEC.into();
        hello.generation = 2;
        assert_eq!(
            hello.negotiate(),
            Err(EndpointError::IncompatibleGeneration {
                client: 2,
                server: 1,
            })
        );
    }

    #[test]
    fn strict_handshake_rejects_extra_fields_and_trailing_data() {
        let hello = LocalHello::new(ClientMode::Control, "0.1.0");
        let mut value = serde_json::to_value(&hello).unwrap();
        value["extra"] = json!(true);
        assert!(decode_handshake::<LocalHello>(&encode_payload(&value).unwrap()).is_err());
        let mut bytes = encode_payload(&hello).unwrap();
        bytes.push(0);
        assert!(decode_handshake::<LocalHello>(&bytes).is_err());
    }

    #[test]
    fn welcome_must_confirm_exact_baseline_and_mode() {
        let hello = LocalHello::new(ClientMode::Control, "0.1.0");
        assert!(hello.accept(&welcome()).is_ok());
        let mut reply = welcome();
        reply.capabilities.clear();
        assert_eq!(
            hello.accept(&reply),
            Err(EndpointError::MissingRequiredCapability)
        );
        for name in [BASELINE, "future.v1", "INVALID"] {
            let mut reply = welcome();
            reply.capabilities.push(name.into());
            assert_eq!(hello.accept(&reply), Err(EndpointError::InvalidHandshake));
        }
        let mut reply = welcome();
        reply.generation = 2;
        assert_eq!(
            hello.accept(&reply),
            Err(EndpointError::IncompatibleGeneration {
                client: 1,
                server: 2,
            })
        );
        reply = welcome();
        reply.codec = "other.v1".into();
        assert_eq!(hello.accept(&reply), Err(EndpointError::UnsupportedCodec));
        reply = welcome();
        reply.server_version = "bad version".into();
        assert_eq!(hello.accept(&reply), Err(EndpointError::InvalidHandshake));
        let interactive = LocalHello::new(
            ClientMode::Interactive {
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
                selector: None,
            },
            "0.1.0",
        );
        assert_eq!(
            interactive.accept(&welcome()),
            Err(EndpointError::InvalidHandshake)
        );
        for field in ["selected", "extension_catalog"] {
            let mut value = serde_json::to_value(welcome()).unwrap();
            value.as_object_mut().unwrap().remove(field);
            assert!(decode_handshake::<LocalWelcome>(&encode_payload(&value).unwrap()).is_err());
        }
        let mut value = serde_json::to_value(welcome()).unwrap();
        value["extension_catalog"] = serde_json::Value::Null;
        assert!(decode_handshake::<LocalWelcome>(&encode_payload(&value).unwrap()).is_err());
    }

    #[test]
    fn remote_cannot_negotiate_local_privileges() {
        let mut hello = remote::RemoteHello::new(ClientMode::Control, "0.1.0");
        hello.required.push(BASELINE.into());
        assert_eq!(
            hello.negotiate(remote::Capabilities::ALL),
            Err(EndpointError::MissingRequiredCapability)
        );
        hello.required.pop();
        hello.optional.push(BASELINE.into());
        let caps = hello.negotiate(remote::Capabilities::ALL).unwrap();
        assert!(!caps.names().iter().any(|name| name == BASELINE));
        assert!(!caps.allows_client(&ClientMessage::Shutdown));
        assert!(
            !caps.allows_client(&ClientMessage::LocalHello(LocalHello::new(
                ClientMode::Control,
                "0.1.0"
            )))
        );
        assert!(!caps.allows_server(&ServerMessage::LocalWelcome(welcome())));
        let local = LocalHello::new(ClientMode::Control, "0.1.0")
            .negotiate()
            .unwrap();
        assert!(local.allows_client(&ClientMessage::Shutdown));
        assert!(local.allows_server(&ServerMessage::LocalWelcome(welcome())));
        assert!(!local.allows_client(&ClientMessage::RemoteHello(hello)));
    }

    #[test]
    fn workspace_order_is_optional_and_gates_both_wire_directions() {
        let hello = LocalHello::new(ClientMode::Control, "0.1.0");
        let request = ClientMessage::MoveWorkspace {
            workspace_id: crate::domain::WorkspaceId::new(),
            position: 1,
        };
        let reply = ServerMessage::WorkspaceMoved {
            resource_revision: 42,
        };
        let request: ClientMessage =
            remote::decode_handshake(&encode_payload(&request).unwrap()).unwrap();
        let reply: ServerMessage =
            remote::decode_handshake(&encode_payload(&reply).unwrap()).unwrap();
        let old = hello.accept(&welcome()).unwrap();
        assert!(!old.allows_client(&request));
        assert!(!old.allows_server(&reply));
        let mut selected = welcome();
        selected.capabilities.push(WORKSPACE_ORDER.into());
        let new = hello.accept(&selected).unwrap();
        assert!(new.allows_client(&request));
        assert!(new.allows_server(&reply));
        let mut old_offer = hello.clone();
        old_offer.optional.clear();
        assert!(!old_offer.negotiate().unwrap().allows_client(&request));
        assert_eq!(
            old_offer.accept(&selected),
            Err(EndpointError::InvalidHandshake)
        );

        let mut remote_offer = remote::RemoteHello::new(ClientMode::Control, "0.1.0");
        let allowed = remote_offer.negotiate(remote::Capabilities::ALL).unwrap();
        assert!(allowed.allows_client(&request));
        assert!(allowed.allows_server(&reply));
        assert!(!allowed.allows_client(&ClientMessage::Shutdown));
        remote_offer.optional.retain(|name| name != WORKSPACE_ORDER);
        let denied = remote_offer.negotiate(remote::Capabilities::ALL).unwrap();
        assert!(!denied.allows_client(&request));
        assert!(!denied.allows_server(&reply));
    }
}
