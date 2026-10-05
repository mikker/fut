//! Metadata-only adapters for the private socket and background SSH bridge.
//!
//! Register alongside `client::federation` when integrating. Construction does
//! not connect or start a daemon; the caller supplies one stable alert identity
//! for the lifetime of its supervisor, including reconnects.

use std::{collections::VecDeque, path::PathBuf, time::Duration};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::{net::UnixStream, time};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use uuid::Uuid;

use super::federation::{
    Async, Connected, Connection, Connector, EndpointSpec, Failure, FailureKind, InitialMetadata,
    Negotiation, Update,
};
use crate::{
    alerts::ClientAlertSnapshot,
    domain::ClientId,
    protocol::{
        ClientMessage, ClientMode, ClientPresenceSnapshot, Envelope, ExtensionCatalog,
        PROTOCOL_VERSION, codec, decode_payload, encode_payload,
        remote::{
            Capabilities, Capability, EndpointError, RemoteHello, RemoteWelcome, decode_handshake,
        },
    },
    resources::ResourceSnapshot,
    ssh_bridge::SshBridge,
};

const INITIAL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_INITIAL_RESPONSES: usize = 64;
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_millis(250);

type Transport = Framed<UnixStream, LengthDelimitedCodec>;

pub(crate) struct SystemConnector {
    local_socket: PathBuf,
    alert_client_id: ClientId,
}

impl SystemConnector {
    pub(crate) fn new(local_socket: PathBuf, alert_client_id: ClientId) -> Self {
        Self {
            local_socket,
            alert_client_id,
        }
    }
}

impl Connector for SystemConnector {
    fn connect(&self, spec: EndpointSpec) -> Async<'_, Result<Connected, Failure>> {
        Box::pin(async move {
            let (stream, owner) = match spec {
                EndpointSpec::Local => (
                    time::timeout(INITIAL_TIMEOUT, UnixStream::connect(&self.local_socket))
                        .await
                        .map_err(|_| transient("local socket connection timed out"))?
                        .map_err(transport_failure)?,
                    None,
                ),
                EndpointSpec::Ssh(machine) => {
                    crate::ssh_bridge::validate_destination(&machine.target)
                        .map_err(compatibility)?;
                    let (stream, owner) =
                        SshBridge::connect_background(&machine.target).map_err(|error| {
                            if error
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                            {
                                failure(
                                    FailureKind::Installation,
                                    "OpenSSH executable is unavailable",
                                )
                            } else {
                                transient(error)
                            }
                        })?;
                    (stream, Some(owner))
                }
            };
            let mut connection = SystemConnection::new(stream, owner);
            let initial =
                time::timeout(INITIAL_TIMEOUT, connection.initialize(self.alert_client_id))
                    .await
                    .unwrap_or_else(|_| Err(transient("initial metadata timed out")));
            match initial {
                Ok(initial) => Ok(Connected {
                    initial,
                    connection: Box::new(connection),
                }),
                Err(error) => Err(connection.failed(error).await),
            }
        })
    }
}

// Deliberately not ServerMessage: no terminal screen/delta type is decoded by
// this adapter. Unknown variants (including screens) are protocol violations.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MetadataMessage {
    Welcome {
        version: u16,
        server_version: String,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        selected: Option<serde::de::IgnoredAny>,
        extension_catalog: ExtensionCatalog,
    },
    RemoteWelcome(Box<RemoteWelcome>),
    EndpointError {
        error: EndpointError,
    },
    IncompatibleProtocol {
        client: u16,
        server: u16,
    },
    Error {
        code: String,
        message: String,
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
    AlertsChanged {
        snapshot: ClientAlertSnapshot,
    },
    ExtensionCatalog {
        catalog: ExtensionCatalog,
    },
    ExtensionCatalogChanged {
        catalog: ExtensionCatalog,
    },
    Pong {
        daemon_pid: u32,
    },
    Detached,
    #[serde(other)]
    Unexpected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Probe {
    Ping(Uuid),
    Resources(Uuid),
}

impl Probe {
    fn matches(self, envelope: &Envelope<MetadataMessage>) -> bool {
        match (self, &envelope.message) {
            (Self::Ping(id), MetadataMessage::Pong { .. })
            | (Self::Resources(id), MetadataMessage::Resources { .. }) => {
                envelope.request_id == Some(id)
            }
            _ => false,
        }
    }
}

struct SystemConnection {
    framed: Transport,
    // Owns the child, diagnostics task and attachment shutdown handle. Dropping
    // this connection or a cancelled connect future also drops the SSH owner.
    owner: Option<SshBridge>,
    strict_remote_handshake: bool,
    capabilities: Option<Capabilities>,
    pending: VecDeque<Update>,
    probe: Option<Probe>,
    terminal_failure: Option<Failure>,
}

impl SystemConnection {
    fn new(stream: UnixStream, owner: Option<SshBridge>) -> Self {
        let strict_remote_handshake = owner.is_some();
        Self {
            framed: Framed::new(stream, codec()),
            owner,
            strict_remote_handshake,
            capabilities: None,
            pending: VecDeque::new(),
            probe: None,
            terminal_failure: None,
        }
    }

    async fn send(
        &mut self,
        request_id: Option<Uuid>,
        message: ClientMessage,
    ) -> Result<(), Failure> {
        let payload = encode_payload(&Envelope {
            request_id,
            message,
        })
        .map_err(compatibility)?;
        self.framed
            .send(Bytes::from(payload))
            .await
            .map_err(transport_failure)
    }

    async fn receive(&mut self) -> Result<Option<Envelope<MetadataMessage>>, Failure> {
        let Some(frame) = self.framed.next().await else {
            return Ok(None);
        };
        let frame = frame.map_err(transport_failure)?;
        if self.strict_remote_handshake {
            decode_handshake(&frame).map(Some).map_err(compatibility)
        } else {
            decode_payload(&frame).map(Some).map_err(compatibility)
        }
    }

    async fn initialize(&mut self, client_id: ClientId) -> Result<InitialMetadata, Failure> {
        let remote = self.owner.is_some();
        let hello = RemoteHello::new(ClientMode::Control, env!("CARGO_PKG_VERSION"));
        // The private control hello is uncorrelated, exactly as hello_control
        // in client/mod.rs. It never retries with the daemon's private version.
        let request_id = remote.then(Uuid::new_v4);
        self.send(
            request_id,
            if remote {
                ClientMessage::RemoteHello(hello.clone())
            } else {
                ClientMessage::Hello {
                    version: PROTOCOL_VERSION,
                    client_version: env!("CARGO_PKG_VERSION").into(),
                    mode: ClientMode::Control,
                }
            },
        )
        .await?;
        let response = self
            .receive()
            .await?
            .ok_or_else(|| transient("endpoint disconnected during handshake"))?;
        self.strict_remote_handshake = false;
        if response.request_id != request_id {
            return Err(compatibility(EndpointError::InvalidHandshake));
        }
        let (negotiation, mut catalog) = match response.message {
            MetadataMessage::RemoteWelcome(welcome) if remote => {
                let welcome = *welcome;
                let capabilities = hello.accept(&welcome).map_err(compatibility)?;
                self.capabilities = Some(capabilities);
                (
                    Negotiation::Ssh {
                        protocol_generation: welcome.generation,
                        server_version: welcome.server_version,
                        capabilities,
                    },
                    welcome.extension_catalog,
                )
            }
            MetadataMessage::Welcome {
                version,
                server_version,
                selected: None,
                extension_catalog,
            } if !remote
                && version == PROTOCOL_VERSION
                && !server_version.is_empty()
                && server_version.len() <= 128
                && !server_version.chars().any(char::is_control) =>
            {
                (
                    Negotiation::Local {
                        protocol_version: version,
                    },
                    Some(extension_catalog),
                )
            }
            message => return Err(message_failure(message)),
        };
        let resources_id = Uuid::new_v4();
        let alerts_id = self.has(Capability::ControlAlerts).then(Uuid::new_v4);
        self.send(Some(resources_id), ClientMessage::WatchResources)
            .await?;
        if let Some(id) = alerts_id {
            self.send(Some(id), ClientMessage::WatchAlerts { client_id })
                .await?;
        }
        let mut resources = None;
        let mut presence = None;
        let mut alerts = None;
        let mut resources_ready = false;
        let mut alerts_ready = alerts_id.is_none();
        for _ in 0..MAX_INITIAL_RESPONSES {
            let response = self
                .receive()
                .await?
                .ok_or_else(|| transient("endpoint disconnected before initial metadata"))?;
            resources_ready |= response.request_id == Some(resources_id)
                && matches!(response.message, MetadataMessage::Resources { .. });
            alerts_ready |= alerts_id.is_some()
                && response.request_id == alerts_id
                && matches!(response.message, MetadataMessage::AlertsChanged { .. });
            self.accept(response)?;
            while let Some(update) = self.pending.pop_front() {
                match update {
                    Update::Resources(value) => resources = Some(value),
                    Update::Presence(value) => presence = Some(value),
                    Update::Alerts(value) => alerts = Some(value),
                    Update::Catalog(value) => catalog = Some(value),
                    Update::Healthy => unreachable!("no probe during initialization"),
                }
            }
            if resources_ready && alerts_ready {
                return Ok(InitialMetadata {
                    negotiation,
                    resources: resources
                        .ok_or_else(|| compatibility("missing initial resources"))?,
                    presence: presence.ok_or_else(|| compatibility("missing initial presence"))?,
                    alerts,
                    catalog,
                });
            }
        }
        Err(transient("initial metadata response limit exceeded"))
    }

    fn has(&self, capability: Capability) -> bool {
        self.capabilities
            .is_none_or(|caps| caps.contains(capability))
    }

    fn accept(&mut self, response: Envelope<MetadataMessage>) -> Result<(), Failure> {
        let healthy = self.probe.is_some_and(|probe| probe.matches(&response));
        match response.message {
            MetadataMessage::Resources { snapshot, presence } => {
                self.pending.push_back(Update::Resources(snapshot));
                self.pending.push_back(Update::Presence(presence));
            }
            MetadataMessage::ResourcesChanged { snapshot } => {
                self.pending.push_back(Update::Resources(snapshot));
            }
            MetadataMessage::PresenceChanged { presence } => {
                self.pending.push_back(Update::Presence(presence));
            }
            MetadataMessage::AlertsChanged { snapshot } if self.has(Capability::ControlAlerts) => {
                self.pending.push_back(Update::Alerts(snapshot));
            }
            MetadataMessage::ExtensionCatalog { catalog }
            | MetadataMessage::ExtensionCatalogChanged { catalog }
                if self.has(Capability::ExtensionCatalog) =>
            {
                self.pending.push_back(Update::Catalog(catalog));
            }
            MetadataMessage::Pong { daemon_pid } if self.has(Capability::Health) => {
                let _ = daemon_pid;
            }
            message => return Err(message_failure(message)),
        }
        if healthy {
            self.probe = None;
            self.pending.push_back(Update::Healthy);
        }
        Ok(())
    }

    async fn failed(&mut self, error: Failure) -> Failure {
        // Preserve the failure across cancellation of next(), including while
        // waiting for stderr's independent pipe reader to reach EOF.
        self.terminal_failure = Some(error.clone());
        let error = if error.kind == FailureKind::Transient {
            if let Some(owner) = &mut self.owner {
                owner.finish_diagnostics(DIAGNOSTIC_TIMEOUT).await;
                classify_ssh_failure(error, &owner.diagnostic())
            } else {
                error
            }
        } else {
            error
        };
        self.terminal_failure = Some(error.clone());
        error
    }
}

impl Connection for SystemConnection {
    fn next(&mut self) -> Async<'_, Result<Option<Update>, Failure>> {
        Box::pin(async move {
            if let Some(error) = self.terminal_failure.clone() {
                return Err(self.failed(error).await);
            }
            loop {
                if let Some(update) = self.pending.pop_front() {
                    return Ok(Some(update));
                }
                // Framed retains partial headers/payloads when this await is
                // cancelled. Decoding and queuing a whole frame has no await.
                let result = match self.receive().await {
                    Ok(Some(response)) => self.accept(response),
                    Ok(None) if self.owner.is_none() => return Ok(None),
                    Ok(None) => Err(transient("SSH endpoint disconnected (EOF)")),
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    return Err(self.failed(error).await);
                }
            }
        })
    }

    fn ping(&mut self) -> Async<'_, Result<(), Failure>> {
        Box::pin(async move {
            if let Some(error) = self.terminal_failure.clone() {
                return Err(error);
            }
            let id = Uuid::new_v4();
            let (probe, message) = if self.has(Capability::Health) {
                (Probe::Ping(id), ClientMessage::Ping)
            } else {
                (Probe::Resources(id), ClientMessage::ListResources)
            };
            // Supersede old probes before writing; a stale pong, metadata event,
            // or response of the wrong kind cannot satisfy this request.
            self.probe = Some(probe);
            self.pending
                .retain(|update| !matches!(update, Update::Healthy));
            if let Err(error) = self.send(Some(id), message).await {
                return Err(self.failed(error).await);
            }
            Ok(())
        })
    }
}

fn failure(kind: FailureKind, message: impl std::fmt::Display) -> Failure {
    Failure {
        kind,
        message: message
            .to_string()
            .chars()
            .filter(|character| !character.is_control())
            .take(1024)
            .collect(),
    }
}

fn transient(message: impl std::fmt::Display) -> Failure {
    failure(FailureKind::Transient, message)
}

fn compatibility(message: impl std::fmt::Display) -> Failure {
    failure(FailureKind::Compatibility, message)
}

fn transport_failure(error: std::io::Error) -> Failure {
    // LengthDelimitedCodec reports invalid/oversized frames as InvalidData;
    // broken pipes, resets, partial-frame EOF and other I/O failures retry.
    if error.kind() == std::io::ErrorKind::InvalidData {
        compatibility(error)
    } else {
        transient(error)
    }
}

fn message_failure(message: MetadataMessage) -> Failure {
    match message {
        MetadataMessage::EndpointError { error } => compatibility(error),
        MetadataMessage::IncompatibleProtocol { client, server } => compatibility(format!(
            "incompatible local protocol: client {client}, server {server}"
        )),
        MetadataMessage::Error { code, message } => {
            compatibility(format!("daemon error ({code}): {message}"))
        }
        MetadataMessage::Detached => transient("endpoint detached"),
        _ => compatibility("unexpected message on metadata control connection"),
    }
}

/// Only explicit, actionable SSH diagnostics inhibit automatic retries. A
/// refused/missing daemon socket, DNS failure, reset or timeout is transient.
pub(super) fn classify_ssh_failure(original: Failure, diagnostic: &str) -> Failure {
    if original.kind != FailureKind::Transient {
        return original;
    }
    let text = diagnostic.to_ascii_lowercase();
    let kind = if [
        "host key verification failed",
        "remote host identification has changed",
        "host key has changed",
        "no matching host key type found",
        "requested strict checking",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
    {
        FailureKind::HostKey
    } else if [
        "permission denied (",
        "authentication failed",
        "too many authentication failures",
        "no supported authentication methods",
        "sign_and_send_pubkey: signing failed",
        "load key ",
        "unprotected private key file",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
    {
        FailureKind::Authentication
    } else if text.lines().any(|line| {
        (line.contains("fut")
            && [
                "command not found",
                "not found",
                "no such file or directory",
                "permission denied",
                "cannot execute",
                "exec format error",
            ]
            .iter()
            .any(|pattern| line.contains(pattern))
            && !line.contains("connect")
            && !line.contains(".sock"))
            || (line.contains("__stdio-bridge")
                && [
                    "unrecognized",
                    "unknown",
                    "unexpected",
                    "invalid subcommand",
                ]
                .iter()
                .any(|pattern| line.contains(pattern)))
            || line.contains("error while loading shared libraries")
            || line.contains("dyld: library not loaded")
    }) {
        FailureKind::Installation
    } else {
        FailureKind::Transient
    };
    if diagnostic.trim().is_empty() {
        original
    } else {
        failure(kind, format!("{}: {}", original.message, diagnostic.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ExtensionCatalogConfig, ServerMessage};

    fn catalog(generation: u64) -> ExtensionCatalog {
        ExtensionCatalog {
            generation,
            fingerprint: String::new(),
            extensions: Vec::new(),
            config: ExtensionCatalogConfig::default(),
        }
    }

    fn resources(revision: u64) -> ServerMessage {
        ServerMessage::Resources {
            snapshot: ResourceSnapshot {
                revision,
                sessions: Vec::new(),
            },
            presence: ClientPresenceSnapshot {
                revision,
                sessions: Vec::new(),
            },
        }
    }

    async fn reply(peer: &mut Transport, id: Option<Uuid>, message: ServerMessage) {
        peer.send(Bytes::from(
            encode_payload(&Envelope {
                request_id: id,
                message,
            })
            .unwrap(),
        ))
        .await
        .unwrap();
    }

    async fn request(peer: &mut Transport) -> Envelope<ClientMessage> {
        decode_payload(&peer.next().await.unwrap().unwrap()).unwrap()
    }

    fn pair() -> (SystemConnection, Transport) {
        let (client, server) = UnixStream::pair().unwrap();
        (
            SystemConnection::new(client, None),
            Framed::new(server, codec()),
        )
    }

    #[test]
    fn ssh_diagnostics_only_stop_retries_for_actionable_failures() {
        for (diagnostic, expected) in [
            ("Host key verification failed.", FailureKind::HostKey),
            (
                "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!",
                FailureKind::HostKey,
            ),
            (
                "No ED25519 host key is known and you have requested strict checking.",
                FailureKind::HostKey,
            ),
            (
                "user@host: Permission denied (publickey).",
                FailureKind::Authentication,
            ),
            (
                "Received disconnect: Too many authentication failures",
                FailureKind::Authentication,
            ),
            (
                "bash: line 1: fut: command not found",
                FailureKind::Installation,
            ),
            ("zsh: command not found: fut", FailureKind::Installation),
            ("sh: fut: Permission denied", FailureKind::Installation),
            (
                "error: unrecognized subcommand '__stdio-bridge'",
                FailureKind::Installation,
            ),
            (
                "connect bridge to /tmp/fut/daemon.sock: No such file or directory",
                FailureKind::Transient,
            ),
            (
                "connect bridge to /tmp/fut/daemon.sock: Permission denied",
                FailureKind::Transient,
            ),
            (
                "ssh: connect to host work port 22: Connection refused",
                FailureKind::Transient,
            ),
            (
                "ssh: Could not resolve hostname work",
                FailureKind::Transient,
            ),
            ("Connection timed out", FailureKind::Transient),
            ("", FailureKind::Transient),
        ] {
            assert_eq!(
                classify_ssh_failure(transient("EOF"), diagnostic).kind,
                expected,
                "{diagnostic}"
            );
        }
        let typed = compatibility(EndpointError::UnsupportedCodec);
        assert_eq!(
            classify_ssh_failure(typed.clone(), "Permission denied (publickey)."),
            typed
        );
        assert!(
            !classify_ssh_failure(transient("EOF"), "\x1bpermission denied (publickey).\n")
                .message
                .chars()
                .any(char::is_control)
        );
    }

    #[test]
    fn protocol_failures_need_attention_but_io_failures_retry() {
        assert_eq!(
            message_failure(MetadataMessage::EndpointError {
                error: EndpointError::MethodNotNegotiated,
            })
            .kind,
            FailureKind::Compatibility
        );
        assert_eq!(
            message_failure(MetadataMessage::IncompatibleProtocol {
                client: 1,
                server: 2
            })
            .kind,
            FailureKind::Compatibility
        );
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
        ] {
            assert_eq!(transport_failure(kind.into()).kind, FailureKind::Transient);
        }
        assert_eq!(
            transport_failure(std::io::ErrorKind::InvalidData.into()).kind,
            FailureKind::Compatibility
        );
    }

    #[tokio::test]
    async fn ping_requires_current_id_and_pong_and_is_consumed_once() {
        let (mut connection, mut peer) = pair();
        connection.ping().await.unwrap();
        let first = request(&mut peer).await;
        connection.ping().await.unwrap();
        let current = request(&mut peer).await;
        assert!(matches!(current.message, ClientMessage::Ping));
        assert_ne!(first.request_id, current.request_id);
        reply(
            &mut peer,
            first.request_id,
            ServerMessage::Pong { daemon_pid: 1 },
        )
        .await;
        reply(&mut peer, None, ServerMessage::Pong { daemon_pid: 1 }).await;
        // Even the correct ID on a different reply kind is not a pong.
        reply(&mut peer, current.request_id, resources(1)).await;
        assert!(matches!(
            connection.next().await.unwrap(),
            Some(Update::Resources(_))
        ));
        assert!(matches!(
            connection.next().await.unwrap(),
            Some(Update::Presence(_))
        ));
        assert!(
            time::timeout(Duration::from_millis(10), connection.next())
                .await
                .is_err()
        );
        reply(
            &mut peer,
            current.request_id,
            ServerMessage::Pong { daemon_pid: 1 },
        )
        .await;
        assert!(matches!(
            connection.next().await.unwrap(),
            Some(Update::Healthy)
        ));
        reply(
            &mut peer,
            current.request_id,
            ServerMessage::Pong { daemon_pid: 1 },
        )
        .await;
        drop(peer);
        assert!(connection.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn metadata_probe_requires_correlated_resources_and_keeps_both_snapshots() {
        let (mut connection, mut peer) = pair();
        let mut hello = RemoteHello::new(ClientMode::Control, "1.0.0");
        hello.optional.clear();
        connection.capabilities = Some(hello.negotiate(Capabilities::ALL).unwrap());
        connection.ping().await.unwrap();
        let probe = request(&mut peer).await;
        assert!(matches!(probe.message, ClientMessage::ListResources));
        reply(&mut peer, Some(Uuid::new_v4()), resources(1)).await;
        reply(
            &mut peer,
            probe.request_id,
            ServerMessage::ResourcesChanged {
                snapshot: ResourceSnapshot {
                    revision: 2,
                    sessions: Vec::new(),
                },
            },
        )
        .await;
        for _ in 0..3 {
            assert!(matches!(
                connection.next().await.unwrap(),
                Some(Update::Resources(_) | Update::Presence(_))
            ));
        }
        assert!(
            time::timeout(Duration::from_millis(10), connection.next())
                .await
                .is_err()
        );
        reply(&mut peer, probe.request_id, resources(3)).await;
        assert!(
            matches!(connection.next().await.unwrap(), Some(Update::Resources(snapshot)) if snapshot.revision == 3)
        );
        assert!(
            matches!(connection.next().await.unwrap(), Some(Update::Presence(snapshot)) if snapshot.revision == 3)
        );
        assert!(matches!(
            connection.next().await.unwrap(),
            Some(Update::Healthy)
        ));
    }

    #[tokio::test]
    async fn local_handshake_subscribes_with_stable_identity_and_retains_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let client_id = ClientId::new();
        let connector = SystemConnector::new(path, client_id);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut peer = Framed::new(stream, codec());
                let hello = request(&mut peer).await;
                assert!(hello.request_id.is_none());
                assert!(matches!(
                    hello.message,
                    ClientMessage::Hello {
                        version: PROTOCOL_VERSION,
                        mode: ClientMode::Control,
                        ..
                    }
                ));
                reply(
                    &mut peer,
                    None,
                    ServerMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        server_version: "0.22.0".into(),
                        selected: None,
                        extension_catalog: catalog(7),
                    },
                )
                .await;
                let watch = request(&mut peer).await;
                assert!(matches!(watch.message, ClientMessage::WatchResources));
                let alerts = request(&mut peer).await;
                assert!(
                    matches!(alerts.message, ClientMessage::WatchAlerts { client_id: id } if id == client_id)
                );
                reply(&mut peer, watch.request_id, resources(1)).await;
                reply(
                    &mut peer,
                    alerts.request_id,
                    ServerMessage::AlertsChanged {
                        snapshot: ClientAlertSnapshot {
                            revision: 1,
                            terminals: Vec::new(),
                        },
                    },
                )
                .await;
            }
        });
        for _ in 0..2 {
            let connected = connector.connect(EndpointSpec::Local).await.unwrap();
            assert_eq!(connected.initial.catalog.unwrap().generation, 7);
            assert_eq!(connected.initial.resources.revision, 1);
            assert!(connected.initial.alerts.is_some());
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn initial_responses_are_bounded_even_when_peer_keeps_sending() {
        let (mut connection, mut peer) = pair();
        let server = tokio::spawn(async move {
            request(&mut peer).await;
            reply(
                &mut peer,
                None,
                ServerMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    server_version: "0.22.0".into(),
                    selected: None,
                    extension_catalog: catalog(1),
                },
            )
            .await;
            request(&mut peer).await;
            request(&mut peer).await;
            for _ in 0..MAX_INITIAL_RESPONSES {
                reply(&mut peer, None, ServerMessage::Pong { daemon_pid: 1 }).await;
            }
        });
        let error = connection.initialize(ClientId::new()).await.err().unwrap();
        assert_eq!(error.kind, FailureKind::Transient);
        assert!(error.message.contains("response limit"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_read_keeps_partial_frame_and_never_decodes_screens() {
        use tokio::io::AsyncWriteExt;
        let (mut connection, peer) = pair();
        let mut stream = peer.into_inner();
        // Deliberately invalid screen data: the metadata decoder must not try
        // to materialize a ScreenSnapshot even when a peer sends this variant.
        let payload = encode_payload(&serde_json::json!({"message": {
            "type": "snapshot", "screen": "not a screen", "terminal_id": false
        }}))
        .unwrap();
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend(payload);
        stream.write_all(&frame[..6]).await.unwrap();
        assert!(
            time::timeout(Duration::from_millis(10), connection.next())
                .await
                .is_err()
        );
        stream.write_all(&frame[6..]).await.unwrap();
        let error = connection.next().await.err().unwrap();
        assert_eq!(error.kind, FailureKind::Compatibility);
        assert!(error.message.contains("unexpected message"));
    }
}
