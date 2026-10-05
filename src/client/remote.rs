use super::*;
use crate::protocol::remote::{
    Capabilities, Capability, EndpointError, RemoteHello, RemoteWelcome, decode_handshake,
};

#[derive(Debug, thiserror::Error)]
#[error("remote daemon error ({code}): {message}")]
pub(super) struct OperationError {
    code: String,
    message: String,
}

pub(super) struct RemoteConnection {
    pub framed: Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    pub welcome: RemoteWelcome,
    pub capabilities: Capabilities,
}

pub(super) async fn negotiate(
    stream: UnixStream,
    mode: ClientMode,
    client_version: &str,
    deadline: Duration,
) -> anyhow::Result<RemoteConnection> {
    let hello = RemoteHello::new(mode, client_version);
    let mut framed = Framed::new(stream, codec());
    let request_id = Some(Uuid::new_v4());
    send_request(
        &mut framed,
        request_id,
        ClientMessage::RemoteHello(hello.clone()),
    )
    .await?;
    let frame = time::timeout(deadline, framed.next())
        .await
        .context("remote handshake timed out")?
        .context("remote daemon disconnected during handshake")?
        .map_err(|_| EndpointError::InvalidHandshake)?;
    let envelope: Envelope<ServerMessage> = decode_handshake(&frame)?;
    if envelope.request_id != request_id {
        return Err(EndpointError::InvalidHandshake.into());
    }
    let welcome = match envelope.message {
        ServerMessage::RemoteWelcome(welcome) => welcome,
        ServerMessage::EndpointError { error } => return Err(error.into()),
        ServerMessage::Error { code, message } => {
            return Err(OperationError {
                code: sanitize(&code),
                message: sanitize(&message),
            }
            .into());
        }
        _ => return Err(EndpointError::InvalidHandshake.into()),
    };
    let capabilities = hello.accept(&welcome)?;
    if let Some(selected) = &welcome.selected {
        ViewState::new(Locality::Remote, selected.clone())
            .map_err(|_| EndpointError::InvalidHandshake)?;
    }
    Ok(RemoteConnection {
        framed,
        welcome,
        capabilities,
    })
}

pub(super) async fn navigator(
    stream: UnixStream,
    deadline: Duration,
) -> anyhow::Result<RemoteConnection> {
    let mut connection = negotiate(
        stream,
        ClientMode::Control,
        env!("CARGO_PKG_VERSION"),
        deadline,
    )
    .await?;
    send_request(
        &mut connection.framed,
        Some(Uuid::new_v4()),
        ClientMessage::WatchResources,
    )
    .await?;
    Ok(connection)
}

pub(super) async fn interactive(
    stream: UnixStream,
    selector: TargetSelector,
    size: TerminalSize,
    deadline: Duration,
) -> anyhow::Result<RemoteConnection> {
    let mut connection = negotiate(
        stream,
        ClientMode::Interactive {
            size,
            selector: Some(selector),
        },
        env!("CARGO_PKG_VERSION"),
        deadline,
    )
    .await?;
    if connection.capabilities.contains(Capability::Alerts) {
        send_request(
            &mut connection.framed,
            None,
            ClientMessage::WatchAlerts {
                client_id: crate::domain::ClientId::new(),
            },
        )
        .await?;
    }
    Ok(connection)
}

/// An optional remote catalog can be newer than this renderer. In that case,
/// disable only extension presentation instead of losing the attachment.
pub(super) fn materialize_ui(
    staged: &config::StagedUiConfig,
    catalog: Option<&crate::protocol::ExtensionCatalog>,
) -> anyhow::Result<UiConfig> {
    match staged.materialize_remote(catalog) {
        Ok(ui) => Ok(ui),
        Err(_) if catalog.is_some() => staged.materialize_remote(None),
        Err(error) => Err(error),
    }
}

// Preserve typed endpoint failures through the public attachment/probe APIs.
pub(super) fn report_error(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<EndpointError>().is_some() {
        error
    } else {
        anyhow::anyhow!(one_line_error(&error))
    }
}

pub(super) async fn open_project(
    stream: UnixStream,
    project: &str,
    deadline: Duration,
) -> anyhow::Result<TargetSelector> {
    let mut connection = negotiate(
        stream,
        ClientMode::Control,
        env!("CARGO_PKG_VERSION"),
        deadline,
    )
    .await?;
    if !connection.capabilities.contains(Capability::ProjectOpen) {
        bail!(
            "remote daemon does not support opening named projects; update Fut on the remote host"
        );
    }
    send_request(
        &mut connection.framed,
        Some(Uuid::new_v4()),
        ClientMessage::OpenProject {
            project: project.to_owned(),
        },
    )
    .await?;
    match time::timeout(deadline, receive(&mut connection.framed))
        .await
        .context("remote project open timed out")??
    {
        ServerMessage::LocationOpened { selected, .. } => {
            Ok(TargetSelector::Terminal(selected.terminal_id))
        }
        ServerMessage::Error { code, message } => bail!(
            "remote project open failed ({}): {}",
            sanitize(&code),
            sanitize(&message)
        ),
        ServerMessage::EndpointError { error } => Err(error.into()),
        _ => bail!("unexpected response to remote project open"),
    }
}

/// Both handoffs and reconnection run in raw mode: use noninteractive SSH and
/// retain its owner until the prepared attachment is committed or discarded.
pub(super) async fn prepare_attachment(
    target: &str,
    selector: TargetSelector,
    size: TerminalSize,
    staged: &StagedUiConfig,
) -> anyhow::Result<PreparedMachineAttachment> {
    let (stream, mut bridge) = crate::ssh_bridge::SshBridge::connect_background(target)?;
    let result = async {
        let remote = interactive(stream, selector, size, Duration::from_secs(15))
            .await
            .context(REMOTE_HANDSHAKE_FAILED)?;
        let ui = materialize_ui(staged, remote.welcome.extension_catalog.as_ref())?;
        let catalog_generation = remote
            .welcome
            .extension_catalog
            .as_ref()
            .map_or(0, |catalog| catalog.generation);
        Ok::<_, anyhow::Error>(PreparedMachineAttachment {
            framed: remote.framed,
            selected: remote
                .welcome
                .selected
                .expect("validated interactive welcome"),
            alerts: Default::default(),
            ui,
            catalog_generation,
            attachment: Attachment::remote(remote.capabilities, target),
            bridge: None,
        })
    }
    .await;
    match result {
        Ok(mut prepared) => {
            prepared.bridge = Some(bridge);
            Ok(prepared)
        }
        Err(error) => {
            bridge.finish_diagnostics(Duration::from_millis(250)).await;
            let failure = federation_transport::classify_ssh_failure(
                federation::Failure {
                    kind: federation::FailureKind::Transient,
                    message: one_line_error(&error),
                },
                &bridge.diagnostic(),
            );
            let _ = bridge.shutdown().await;
            if failure.kind != federation::FailureKind::Transient {
                return Err(connection::Attention(failure.message).into());
            }
            Err(error)
        }
    }
}
