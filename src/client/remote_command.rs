//! Local and SSH-backed configured command surfaces share the same UI contract.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use crossterm::event::{KeyModifiers, MouseButton as HostMouseButton, MouseEvent, MouseEventKind};
use futures_util::StreamExt;
use ratatui::layout::Rect;
use tokio::{net::UnixStream, time::timeout};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use uuid::Uuid;

use crate::{
    command::PopupSize,
    domain::{MouseButton, PaneId, ScreenSnapshot, TerminalId, TerminalKeyEvent, TerminalSize},
    extensions::ExtensionCommandExecution,
    protocol::{
        ClientMessage, ClientMode, Envelope, RemoteClipboardCopy, RemoteCommandIdentity,
        RemoteCommandOperation, RemoteCommandReply, RemoteCommandSpec, RemoteMouseKind,
        SelectedTarget, ServerMessage, decode_payload, remote::Capability,
    },
    ssh_bridge::SshBridge,
    terminal::TerminalConfig,
};

use super::{
    config::PaletteCommand,
    remote,
    temporary_command::{
        ExtensionCommandContext, TemporaryClipboardCopy, TemporaryCommandSurface,
        TemporaryCommandUpdate,
    },
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct CommandSurface {
    runtime: CommandRuntime,
}

enum CommandRuntime {
    Local(Box<TemporaryCommandSurface>),
    Remote(Box<RemoteCommandSurface>),
}

impl CommandSurface {
    pub(super) async fn spawn(
        terminal_config: TerminalConfig,
        command: &PaletteCommand,
        pid: u32,
        size: TerminalSize,
        socket_path: &Path,
        extension_context: Option<&ExtensionCommandContext>,
        form: Option<&BTreeMap<String, String>>,
    ) -> anyhow::Result<Self> {
        let surface = TemporaryCommandSurface::spawn(
            terminal_config,
            command,
            pid,
            size,
            socket_path,
            extension_context,
            form,
        )
        .await?;
        Ok(Self {
            runtime: CommandRuntime::Local(Box::new(surface)),
        })
    }

    pub(super) async fn spawn_remote(
        command: &PaletteCommand,
        target: TerminalId,
        size: TerminalSize,
        ssh_target: &str,
        form: Option<&BTreeMap<String, String>>,
    ) -> anyhow::Result<Self> {
        let ExtensionCommandExecution::Interactive {
            size: popup_size, ..
        } = command.execution
        else {
            bail!("background commands cannot open an interactive surface");
        };
        let (mut session, bridge) = connect(command, target, size, ssh_target, form).await?;
        timeout(HANDSHAKE_TIMEOUT, session.wait_ready())
            .await
            .context("remote command start timed out")??;
        Ok(Self {
            runtime: CommandRuntime::Remote(Box::new(RemoteCommandSurface {
                title: command.title.clone(),
                size: popup_size,
                session,
                _bridge: bridge,
            })),
        })
    }

    pub(super) fn title(&self) -> &str {
        match &self.runtime {
            CommandRuntime::Local(surface) => surface.title(),
            CommandRuntime::Remote(surface) => &surface.title,
        }
    }

    pub(super) fn size(&self) -> PopupSize {
        match &self.runtime {
            CommandRuntime::Local(surface) => surface.size(),
            CommandRuntime::Remote(surface) => surface.size,
        }
    }

    pub(super) fn activated_target(&self) -> anyhow::Result<Option<PaneId>> {
        match &self.runtime {
            CommandRuntime::Local(surface) => surface.activated_target(),
            CommandRuntime::Remote(surface) => Ok(surface
                .session
                .done
                .as_ref()
                .and_then(|done| done.activated_target)),
        }
    }

    pub(super) fn screen(&self) -> &ScreenSnapshot {
        match &self.runtime {
            CommandRuntime::Local(surface) => &surface.screen,
            CommandRuntime::Remote(surface) => surface
                .session
                .screen
                .as_ref()
                .expect("remote command provided a ready screen"),
        }
    }

    pub(super) async fn input(&mut self, bytes: Vec<u8>) -> anyhow::Result<()> {
        match &mut self.runtime {
            CommandRuntime::Local(surface) => surface.input(bytes).await,
            CommandRuntime::Remote(surface) => surface
                .session
                .operation(RemoteCommandOperation::Input(bytes))
                .await
                .map(|_| ()),
        }
    }

    pub(super) async fn key_input(&mut self, event: TerminalKeyEvent) -> anyhow::Result<()> {
        match &mut self.runtime {
            CommandRuntime::Local(surface) => surface.key_input(event).await,
            CommandRuntime::Remote(surface) => surface
                .session
                .operation(RemoteCommandOperation::KeyInput(event))
                .await
                .map(|_| ()),
        }
    }

    pub(super) async fn paste(&mut self, text: String) -> anyhow::Result<()> {
        match &mut self.runtime {
            CommandRuntime::Local(surface) => surface.paste(text).await,
            CommandRuntime::Remote(surface) => surface
                .session
                .operation(RemoteCommandOperation::Paste(text))
                .await
                .map(|_| ()),
        }
    }

    pub(super) async fn resize(&mut self, size: TerminalSize) -> anyhow::Result<()> {
        match &mut self.runtime {
            CommandRuntime::Local(surface) => surface.resize(size).await,
            CommandRuntime::Remote(surface) => surface
                .session
                .operation(RemoteCommandOperation::Resize(size))
                .await
                .map(|_| ()),
        }
    }

    pub(super) async fn mouse(
        &mut self,
        event: MouseEvent,
        content: Rect,
    ) -> anyhow::Result<Option<TemporaryClipboardCopy>> {
        match &mut self.runtime {
            CommandRuntime::Local(surface) => surface.mouse(event, content).await,
            CommandRuntime::Remote(surface) => surface
                .session
                .operation(mouse_operation(event, content))
                .await
                .map(|ack| {
                    ack.clipboard.map(|copy| TemporaryClipboardCopy {
                        request_id: copy.request_id,
                        text: copy.text,
                    })
                }),
        }
    }

    pub(super) async fn finish_clipboard(
        &mut self,
        request_id: Uuid,
        copied: bool,
    ) -> anyhow::Result<bool> {
        match &mut self.runtime {
            CommandRuntime::Local(surface) => surface.finish_clipboard(request_id, copied).await,
            CommandRuntime::Remote(surface) => surface
                .session
                .operation(RemoteCommandOperation::FinishClipboard { request_id, copied })
                .await
                .map(|ack| ack.handled),
        }
    }

    pub(super) async fn cancel_mouse_copy(&mut self) {
        match &mut self.runtime {
            CommandRuntime::Local(surface) => surface.cancel_mouse_copy().await,
            CommandRuntime::Remote(surface) => {
                let _ = surface
                    .session
                    .operation(RemoteCommandOperation::CancelMouseCopy)
                    .await;
            }
        }
    }

    /// As for the local surface, this is the *local daemon socket path*, not
    /// the remote socket: downloaded logs live beside it in command-logs/.
    pub(super) async fn write_failure_log(&self, socket_path: &Path) -> anyhow::Result<PathBuf> {
        match &self.runtime {
            CommandRuntime::Local(surface) => surface.write_failure_log(socket_path).await,
            CommandRuntime::Remote(surface) => {
                crate::daemon::path::prepare_runtime_dir(socket_path)?;
                let output = surface
                    .session
                    .done
                    .as_ref()
                    .and_then(|done| done.failure_output.as_deref())
                    .context("remote command did not provide failure output")?;
                let directory = socket_path
                    .parent()
                    .context("resolve command log directory")?
                    .join("command-logs");
                tokio::fs::create_dir_all(&directory)
                    .await
                    .with_context(|| {
                        format!("create command log directory {}", directory.display())
                    })?;
                let path = directory.join(format!("command-{}.log", surface.session.start_id));
                tokio::fs::write(&path, output)
                    .await
                    .with_context(|| format!("write command log {}", path.display()))?;
                Ok(path)
            }
        }
    }

    pub(super) async fn update(&mut self) -> TemporaryCommandUpdate {
        match &mut self.runtime {
            CommandRuntime::Local(surface) => surface.update().await,
            CommandRuntime::Remote(surface) => surface.session.update().await,
        }
    }
}

pub(super) struct RemoteCommandSurface {
    title: String,
    size: PopupSize,
    session: RemoteSession,
    // Dropping the bridge closes the control connection and cancels its child.
    _bridge: SshBridge,
}

struct Completion {
    exit_code: Option<i32>,
    activated_target: Option<PaneId>,
    failure_output: Option<String>,
}

struct Ack {
    clipboard: Option<RemoteClipboardCopy>,
    handled: bool,
}

struct RemoteSession {
    framed: Framed<UnixStream, LengthDelimitedCodec>,
    start_id: Uuid,
    screen: Option<ScreenSnapshot>,
    screen_changed: bool,
    done: Option<Completion>,
    stopped: bool,
}

async fn connect(
    command: &PaletteCommand,
    target: TerminalId,
    size: TerminalSize,
    ssh_target: &str,
    form: Option<&BTreeMap<String, String>>,
) -> anyhow::Result<(RemoteSession, SshBridge)> {
    let (mut session, bridge) = connect_session(ssh_target).await?;
    timeout(
        HANDSHAKE_TIMEOUT,
        super::send_request(
            &mut session.framed,
            Some(session.start_id),
            ClientMessage::CommandStart {
                command: command_spec(command),
                target,
                size,
                form: form.cloned(),
            },
        ),
    )
    .await
    .context("send remote command start timed out")??;
    Ok((session, bridge))
}

async fn connect_session(ssh_target: &str) -> anyhow::Result<(RemoteSession, SshBridge)> {
    let (stream, bridge) = SshBridge::connect_background(ssh_target)?;
    let connection = timeout(
        HANDSHAKE_TIMEOUT,
        remote::negotiate(
            stream,
            ClientMode::Control,
            env!("CARGO_PKG_VERSION"),
            HANDSHAKE_TIMEOUT,
        ),
    )
    .await
    .context("remote command handshake timed out")??;
    if !connection
        .capabilities
        .contains(Capability::ConfiguredCommands)
    {
        bail!(
            "remote daemon (Fut {}) does not support configured commands; update Fut on the remote host and restart its daemon safely",
            connection.welcome.server_version
        );
    }
    let session = RemoteSession {
        framed: connection.framed,
        start_id: Uuid::new_v4(),
        screen: None,
        screen_changed: false,
        done: None,
        stopped: false,
    };
    Ok((session, bridge))
}

fn command_spec(command: &PaletteCommand) -> RemoteCommandSpec {
    RemoteCommandSpec {
        title: command.title.clone(),
        program: command.program.clone(),
        args: command.args.clone(),
        execution: command.execution.clone(),
        extension: command
            .extension
            .as_ref()
            .map(|identity| RemoteCommandIdentity {
                id: identity.id.clone(),
                command: identity.command.clone(),
            }),
        extension_args_override: command.extension_args_override,
    }
}

pub(super) async fn field_defaults(
    command: &PaletteCommand,
    target: TerminalId,
    ssh_target: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let (mut session, _bridge) = connect_session(ssh_target).await?;
    timeout(HANDSHAKE_TIMEOUT, async {
        super::send_request(
            &mut session.framed,
            Some(session.start_id),
            ClientMessage::CommandDefaults {
                command: command_spec(command),
                target,
            },
        )
        .await?;
        let (id, reply) = session.receive().await?;
        anyhow::ensure!(
            id == Some(session.start_id),
            "remote command defaults response ID mismatch"
        );
        match reply {
            RemoteCommandReply::Defaults { values } => Ok(values),
            RemoteCommandReply::Error { message } => {
                bail!("remote command defaults failed: {message}")
            }
            _ => bail!("expected remote command defaults"),
        }
    })
    .await
    .context("remote command defaults timed out")?
}

impl RemoteSession {
    async fn receive(&mut self) -> anyhow::Result<(Option<Uuid>, RemoteCommandReply)> {
        let result = async {
            let frame = self
                .framed
                .next()
                .await
                .context("remote command disconnected")??;
            let envelope: Envelope<ServerMessage> = decode_payload(&frame)?;
            match envelope.message {
                ServerMessage::CommandReply { reply } => Ok((envelope.request_id, reply)),
                ServerMessage::Error { code, message } => {
                    bail!("remote command error ({code}): {message}")
                }
                ServerMessage::EndpointError { error } => Err(error.into()),
                _ => bail!("unexpected response on remote command connection"),
            }
        }
        .await;
        if result.is_err() {
            self.stopped = true;
        }
        result
    }

    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        let (id, reply) = self.receive().await?;
        if id != Some(self.start_id) {
            bail!("remote command start response ID mismatch");
        }
        match reply {
            RemoteCommandReply::Ready { screen } => {
                self.screen = Some(screen);
                Ok(())
            }
            RemoteCommandReply::Error { message } => {
                bail!("remote command start failed: {message}")
            }
            _ => bail!("expected remote command Ready"),
        }
    }

    /// Consume events without losing the last screen or the completion metadata
    /// when they arrive before an operation's correlated acknowledgement.
    fn unsolicited(&mut self, id: Option<Uuid>, reply: RemoteCommandReply) -> anyhow::Result<()> {
        if id.is_some() {
            bail!("unexpected correlated remote command response");
        }
        match reply {
            RemoteCommandReply::Screen { screen } => {
                self.screen = Some(screen);
                self.screen_changed = true;
            }
            RemoteCommandReply::Done {
                exit_code,
                activated_target,
                failure_output,
            } => {
                self.done = Some(Completion {
                    exit_code,
                    activated_target,
                    failure_output,
                });
            }
            RemoteCommandReply::Error { message } => bail!("remote command failed: {message}"),
            _ => bail!("unexpected unsolicited remote command response"),
        }
        Ok(())
    }

    async fn operation(&mut self, operation: RemoteCommandOperation) -> anyhow::Result<Ack> {
        match timeout(HANDSHAKE_TIMEOUT, self.operation_inner(operation)).await {
            Ok(result) => result,
            Err(error) => {
                self.stopped = true;
                Err(error).context("remote command operation timed out")
            }
        }
    }

    async fn operation_inner(&mut self, operation: RemoteCommandOperation) -> anyhow::Result<Ack> {
        if self.done.is_some() {
            return Ok(Ack {
                clipboard: None,
                handled: false,
            });
        }
        if self.stopped {
            bail!("remote command connection stopped");
        }
        let request_id = Uuid::new_v4();
        super::send_request(
            &mut self.framed,
            Some(request_id),
            ClientMessage::CommandOperation { operation },
        )
        .await?;
        loop {
            let (id, reply) = self.receive().await?;
            if id == Some(request_id) {
                return match reply {
                    RemoteCommandReply::Ack { clipboard, handled } => {
                        Ok(Ack { clipboard, handled })
                    }
                    RemoteCommandReply::Error { message } => {
                        bail!("remote command operation failed: {message}")
                    }
                    _ => bail!("expected remote command operation Ack"),
                };
            }
            self.unsolicited(id, reply)?;
            if self.done.is_some() {
                return Ok(Ack {
                    clipboard: None,
                    handled: false,
                });
            }
        }
    }

    async fn update(&mut self) -> TemporaryCommandUpdate {
        loop {
            if let Some(done) = &self.done {
                return TemporaryCommandUpdate::Exited(done.exit_code);
            }
            if std::mem::take(&mut self.screen_changed) {
                return TemporaryCommandUpdate::Screen;
            }
            if self.stopped {
                return TemporaryCommandUpdate::Stopped;
            }
            let result = match self.receive().await {
                Ok((id, reply)) => self.unsolicited(id, reply),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                return TemporaryCommandUpdate::Error(format!("{error:#}"));
            }
        }
    }
}

pub(super) async fn run_remote_background(
    command: &PaletteCommand,
    focused: &SelectedTarget,
    ssh_target: &str,
) -> anyhow::Result<()> {
    let (mut session, _bridge) = connect(
        command,
        focused.terminal_id,
        TerminalSize {
            columns: 80,
            rows: 24,
        },
        ssh_target,
        None,
    )
    .await?;
    let (id, reply) = session.receive().await?;
    if id != Some(session.start_id) {
        bail!("remote background command response ID mismatch");
    }
    match reply {
        RemoteCommandReply::Done {
            exit_code: Some(0), ..
        } => Ok(()),
        RemoteCommandReply::Done {
            exit_code,
            failure_output,
            ..
        } => {
            let status = exit_code.map_or_else(|| "unknown".to_owned(), |code| code.to_string());
            bail!(
                "{} exited with status {status}: {}",
                command.title,
                failure_output.unwrap_or_default()
            );
        }
        RemoteCommandReply::Error { message } => bail!("{} failed: {message}", command.title),
        _ => bail!("expected remote background command Done"),
    }
}

fn mouse_operation(event: MouseEvent, content: Rect) -> RemoteCommandOperation {
    let button = |button| match button {
        HostMouseButton::Left => MouseButton::Left,
        HostMouseButton::Middle => MouseButton::Middle,
        HostMouseButton::Right => MouseButton::Right,
    };
    let kind = match event.kind {
        MouseEventKind::Down(value) => RemoteMouseKind::Down {
            button: button(value),
        },
        MouseEventKind::Up(value) => RemoteMouseKind::Up {
            button: button(value),
        },
        MouseEventKind::Drag(value) => RemoteMouseKind::Drag {
            button: button(value),
        },
        MouseEventKind::Moved => RemoteMouseKind::Moved,
        MouseEventKind::ScrollUp => RemoteMouseKind::ScrollUp,
        MouseEventKind::ScrollDown => RemoteMouseKind::ScrollDown,
        MouseEventKind::ScrollLeft => RemoteMouseKind::ScrollLeft,
        MouseEventKind::ScrollRight => RemoteMouseKind::ScrollRight,
    };
    RemoteCommandOperation::Mouse {
        kind,
        column: event.column,
        row: event.row,
        shift: event.modifiers.contains(KeyModifiers::SHIFT),
        control: event.modifiers.contains(KeyModifiers::CONTROL),
        alt: event.modifiers.contains(KeyModifiers::ALT),
        content: [content.x, content.y, content.width, content.height],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{codec, encode_payload};
    use bytes::Bytes;
    use futures_util::SinkExt;

    #[test]
    fn command_spec_preserves_explicit_empty_argument_override() {
        let mut command: PaletteCommand = serde_json::from_value(serde_json::json!({
            "title": "Remote command", "program": "tool", "args": []
        }))
        .unwrap();
        assert!(!command_spec(&command).extension_args_override);
        command.extension_args_override = true;
        let spec = command_spec(&command);
        assert!(spec.extension_args_override);
        assert!(spec.args.is_empty());
    }

    fn session_pair() -> (RemoteSession, Framed<UnixStream, LengthDelimitedCodec>) {
        let (client, server) = UnixStream::pair().unwrap();
        (
            RemoteSession {
                framed: Framed::new(client, codec()),
                start_id: Uuid::new_v4(),
                screen: None,
                screen_changed: false,
                done: None,
                stopped: false,
            },
            Framed::new(server, codec()),
        )
    }

    fn screen() -> ScreenSnapshot {
        ScreenSnapshot::new(
            1,
            TerminalSize {
                columns: 1,
                rows: 1,
            },
            vec![crate::domain::Cell::default()],
            crate::domain::Cursor {
                column: 0,
                row: 0,
                visible: true,
                shape: Default::default(),
                blinking: false,
            },
        )
        .unwrap()
    }

    async fn reply(
        server: &mut Framed<UnixStream, LengthDelimitedCodec>,
        request_id: Option<Uuid>,
        reply: RemoteCommandReply,
    ) {
        server
            .send(Bytes::from(
                encode_payload(&Envelope {
                    request_id,
                    message: ServerMessage::CommandReply { reply },
                })
                .unwrap(),
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn operation_retains_completion_when_the_process_exits_before_ack() {
        let (mut session, mut server) = session_pair();
        let activated = PaneId::new();
        let task = tokio::spawn(async move {
            let frame = server.next().await.unwrap().unwrap();
            let request: Envelope<ClientMessage> = decode_payload(&frame).unwrap();
            assert!(matches!(
                request.message,
                ClientMessage::CommandOperation {
                    operation: RemoteCommandOperation::Input(_)
                }
            ));
            reply(
                &mut server,
                None,
                RemoteCommandReply::Screen { screen: screen() },
            )
            .await;
            reply(
                &mut server,
                None,
                RemoteCommandReply::Done {
                    exit_code: Some(2),
                    activated_target: Some(activated),
                    failure_output: Some("actionable failure".into()),
                },
            )
            .await;
        });
        let ack = session
            .operation(RemoteCommandOperation::Input(vec![b'x']))
            .await
            .unwrap();
        assert!(!ack.handled);
        assert!(ack.clipboard.is_none());
        assert_eq!(session.screen.as_ref(), Some(&screen()));
        assert!(matches!(
            session.update().await,
            TemporaryCommandUpdate::Exited(Some(2))
        ));
        let done = session.done.as_ref().unwrap();
        assert_eq!(done.activated_target, Some(activated));
        assert_eq!(done.failure_output.as_deref(), Some("actionable failure"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn operation_rejects_an_ack_for_a_different_request() {
        let (mut session, mut server) = session_pair();
        let task = tokio::spawn(async move {
            server.next().await.unwrap().unwrap();
            reply(
                &mut server,
                Some(Uuid::new_v4()),
                RemoteCommandReply::Ack {
                    clipboard: None,
                    handled: true,
                },
            )
            .await;
        });
        assert!(
            session
                .operation(RemoteCommandOperation::CancelMouseCopy)
                .await
                .is_err()
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn ready_requires_start_id_and_updates_preserve_screen() {
        let (mut session, mut server) = session_pair();
        reply(
            &mut server,
            Some(session.start_id),
            RemoteCommandReply::Ready { screen: screen() },
        )
        .await;
        session.wait_ready().await.unwrap();
        reply(
            &mut server,
            None,
            RemoteCommandReply::Screen { screen: screen() },
        )
        .await;
        assert!(matches!(
            session.update().await,
            TemporaryCommandUpdate::Screen
        ));
        assert_eq!(session.screen, Some(screen()));

        let (mut session, mut server) = session_pair();
        reply(
            &mut server,
            Some(Uuid::new_v4()),
            RemoteCommandReply::Ready { screen: screen() },
        )
        .await;
        assert!(session.wait_ready().await.is_err());
    }

    #[tokio::test]
    async fn disconnect_reports_error_once_then_stops() {
        let (mut session, server) = session_pair();
        drop(server);
        assert!(matches!(
            session.update().await,
            TemporaryCommandUpdate::Error(_)
        ));
        assert!(matches!(
            session.update().await,
            TemporaryCommandUpdate::Stopped
        ));
    }
}
