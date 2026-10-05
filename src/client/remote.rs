use super::*;
use crate::protocol::remote::{
    Capabilities, Capability, EndpointError, RemoteHello, RemoteWelcome, decode_handshake,
};

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
            bail!(
                "remote daemon error ({}): {}",
                sanitize(&code),
                sanitize(&message)
            );
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
