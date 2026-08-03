use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;
use uuid::Uuid;

use crate::{
    client,
    daemon::{
        DaemonConfig,
        autostart::{ensure_daemon, protocol_ready},
        path::socket_path,
        run_daemon,
    },
    domain::WorkspaceId,
    protocol::{
        AcknowledgedCommand, ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, RenameSelector,
        ServerMessage, codec, decode_payload, encode_payload,
    },
    resources::{ResourceSnapshot, SessionSelector, TargetSelector},
};

#[derive(Parser)]
#[command(
    name = "fut",
    version,
    about = "A project-oriented terminal multiplexer"
)]
pub struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Attach {
        /// Target: session:<uuid-or-name>, workspace:<uuid>, tab:<uuid>, pane:<uuid>, terminal:<uuid>, or legacy session shorthand.
        #[arg(value_name = "TARGET")]
        target: Option<String>,
    },
    New {
        name: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    NewTab {
        #[arg(value_name = "WORKSPACE", value_parser = workspace_parent)]
        workspace_id: WorkspaceId,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    List,
    Daemon {
        #[arg(long)]
        foreground: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    Ping,
    Close {
        /// Target selector (the same forms accepted by attach).
        #[arg(value_name = "TARGET")]
        target: Option<String>,
    },
    Rename {
        /// Target: session:<uuid-or-name>, id:<uuid>, name:<exact>, workspace:<uuid>, tab:<uuid>, or session shorthand.
        #[arg(value_name = "TARGET")]
        target: String,
        #[arg(value_name = "NAME")]
        name: String,
    },
    Shutdown,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let socket = socket_path(cli.socket.as_deref())?;
    match cli.command {
        None | Some(Command::Attach { target: None }) => {
            let cwd = std::env::current_dir().context("read current directory")?;
            open_and_attach(&socket, cwd).await
        }
        Some(Command::Attach {
            target: Some(target),
        }) => client::attach(&socket, Some(selector(&target)?)).await,
        Some(Command::New { name, cwd, command }) => {
            let cwd = cwd.unwrap_or(std::env::current_dir()?);
            let (program, argv) = command
                .split_first()
                .map_or((None, vec![]), |(program, argv)| {
                    (Some(PathBuf::from(program)), argv.to_vec())
                });
            match control(
                &socket,
                ClientMessage::OpenLocation {
                    name: Some(name),
                    cwd,
                    program,
                    argv,
                },
            )
            .await?
            {
                ServerMessage::LocationOpened {
                    selected,
                    disposition,
                } => {
                    println!(
                        "disposition={} session={} workspace={} tab={} pane={} terminal={} pid={}",
                        match disposition {
                            crate::protocol::OpenDisposition::Existing => "existing",
                            crate::protocol::OpenDisposition::WorkspaceCreated =>
                                "workspace_created",
                            crate::protocol::OpenDisposition::SessionCreated => "session_created",
                        },
                        selected.session_id,
                        selected.workspace_id,
                        selected.tab_id,
                        selected.pane_id,
                        selected.terminal_id,
                        selected.child_pid
                    );
                    Ok(())
                }
                other => unexpected(other),
            }
        }
        Some(Command::NewTab {
            workspace_id,
            name,
            cwd,
            command,
        }) => {
            let (program, argv) = command
                .split_first()
                .map_or((None, vec![]), |(program, argv)| {
                    (Some(PathBuf::from(program)), argv.to_vec())
                });
            match control(
                &socket,
                ClientMessage::CreateTab {
                    workspace_id,
                    name,
                    cwd,
                    program,
                    argv,
                },
            )
            .await?
            {
                ServerMessage::TabCreated { selected } => {
                    println!(
                        "session={} workspace={} tab={} pane={} terminal={} pid={}",
                        selected.session_id,
                        selected.workspace_id,
                        selected.tab_id,
                        selected.pane_id,
                        selected.terminal_id,
                        selected.child_pid
                    );
                    Ok(())
                }
                other => unexpected(other),
            }
        }
        Some(Command::List) => match control(&socket, ClientMessage::ListResources).await? {
            ServerMessage::Resources { snapshot } => {
                print_resources(&snapshot);
                Ok(())
            }
            other => unexpected(other),
        },
        Some(Command::Daemon {
            foreground,
            cwd,
            command,
        }) => {
            if !foreground {
                bail!("daemon currently requires --foreground");
            }
            let cwd = cwd.unwrap_or(std::env::current_dir()?);
            let mut config = DaemonConfig::shell(socket, cwd);
            if let Some(program) = command.first() {
                config.spawn.program = program.into();
                config.spawn.argv = command[1..].to_vec();
            }
            run_daemon(config).await
        }
        Some(Command::Ping) => {
            let response = control(&socket, ClientMessage::Ping).await?;
            match response {
                ServerMessage::Pong { daemon_pid } => println!("pong pid={daemon_pid}"),
                other => unexpected(other)?,
            }
            Ok(())
        }
        Some(Command::Close { target }) => {
            let selector = match target {
                Some(value) => selector(&value)?,
                None => TargetSelector::Session(only_session(&socket).await?),
            };
            response_ok(
                control(&socket, ClientMessage::CloseTarget { selector }).await?,
                AcknowledgedCommand::CloseTarget,
            )?;
            println!("closed=true");
            Ok(())
        }
        Some(Command::Rename { target, name }) => {
            response_ok(
                control(
                    &socket,
                    ClientMessage::RenameTarget {
                        selector: rename_selector(&target)?,
                        name,
                    },
                )
                .await?,
                AcknowledgedCommand::RenameTarget,
            )?;
            println!("renamed=true");
            Ok(())
        }
        Some(Command::Shutdown) => {
            response_ok(
                control(&socket, ClientMessage::Shutdown).await?,
                AcknowledgedCommand::Shutdown,
            )?;
            println!("shutdown=true");
            Ok(())
        }
    }
}

async fn open_and_attach(socket: &std::path::Path, cwd: PathBuf) -> Result<()> {
    let selected = open_current_location(socket, &cwd).await?;
    client::attach(socket, Some(TargetSelector::Terminal(selected.terminal_id))).await
}

/// Idempotently opens `cwd`, surviving the old daemon's last-terminal shutdown.
///
/// This is public so process-level tests can exercise the race without requiring
/// a host terminal merely to test the control-plane operation.
#[doc(hidden)]
pub async fn open_current_location(
    socket: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<crate::protocol::SelectedTarget> {
    const RETRIES: usize = 2;

    ensure_daemon(socket, cwd).await?;
    for attempt in 0..=RETRIES {
        let response = control(
            socket,
            ClientMessage::OpenLocation {
                name: None,
                cwd: cwd.to_owned(),
                program: None,
                argv: vec![],
            },
        )
        .await;

        match response {
            Ok(ServerMessage::LocationOpened { selected, .. }) => return Ok(selected),
            Ok(ServerMessage::Error { ref code, .. }) if code == "shutting_down" => {}
            Ok(other) => return unexpected(other),
            Err(error) => {
                // A transport failure is retryable only if protocol readiness
                // disappears, distinguishing shutdown from a semantic failure.
                if !wait_until_protocol_stops(socket).await {
                    return Err(error);
                }
            }
        }

        if attempt == RETRIES {
            bail!(
                "daemon repeatedly shut down while opening {}",
                cwd.display()
            );
        }
        wait_until_protocol_stops(socket).await;
        ensure_daemon(socket, cwd).await?;
    }
    unreachable!()
}

async fn wait_until_protocol_stops(socket: &std::path::Path) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if !protocol_ready(socket).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    !protocol_ready(socket).await
}

async fn control(socket: &std::path::Path, command: ClientMessage) -> Result<ServerMessage> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
    let mut framed = Framed::new(stream, codec());
    send(
        &mut framed,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
            mode: ClientMode::Control,
        },
    )
    .await?;
    match receive(
        &mut framed,
        Duration::from_secs(2),
        "daemon handshake timed out",
    )
    .await?
    {
        ServerMessage::Welcome { .. } => {}
        other => return unexpected(other),
    }
    send(&mut framed, command).await?;
    receive(
        &mut framed,
        Duration::from_secs(15),
        "daemon response timed out",
    )
    .await
}

fn selector(value: &str) -> Result<TargetSelector> {
    if value.is_empty() {
        bail!("target must not be empty");
    }
    if let Some(id) = value.strip_prefix("workspace:") {
        return id
            .parse()
            .map(TargetSelector::Workspace)
            .context("invalid workspace id: expected a UUID");
    }
    if let Some(id) = value.strip_prefix("tab:") {
        return id
            .parse()
            .map(TargetSelector::Tab)
            .context("invalid tab id: expected a UUID");
    }
    if let Some(id) = value.strip_prefix("pane:") {
        return id
            .parse()
            .map(TargetSelector::Pane)
            .context("invalid pane id: expected a UUID");
    }
    if let Some(id) = value.strip_prefix("terminal:") {
        return id
            .parse()
            .map(TargetSelector::Terminal)
            .context("invalid terminal id: expected a UUID");
    }
    if let Some(session) = value.strip_prefix("session:") {
        if session.is_empty() {
            bail!("session selector must not be empty");
        }
        return Ok(TargetSelector::Session(session_selector(session)));
    }
    if value.contains(':') && !value.starts_with("id:") && !value.starts_with("name:") {
        bail!("unknown target selector prefix; use name:<exact> for names containing colons");
    }
    Ok(TargetSelector::Session(session_selector_explicit(value)?))
}

fn rename_selector(value: &str) -> Result<RenameSelector> {
    match selector(value)? {
        TargetSelector::Session(selector) => Ok(RenameSelector::Session(selector)),
        TargetSelector::Workspace(id) => Ok(RenameSelector::Workspace(id)),
        TargetSelector::Tab(id) => Ok(RenameSelector::Tab(id)),
        TargetSelector::Pane(_) | TargetSelector::Terminal(_) => {
            bail!("rename target must be a session, workspace, or tab")
        }
    }
}

fn workspace_parent(value: &str) -> std::result::Result<WorkspaceId, String> {
    let id = value.strip_prefix("workspace:").unwrap_or(value);
    if value.contains(':') && id == value {
        return Err("invalid workspace: expected a UUID or workspace:<uuid>".into());
    }
    id.parse()
        .map_err(|_| "invalid workspace: expected a UUID or workspace:<uuid>".into())
}

fn session_selector_explicit(value: &str) -> Result<SessionSelector> {
    if let Some(id) = value.strip_prefix("id:") {
        if id.is_empty() {
            bail!("id selector must not be empty");
        }
        return id
            .parse()
            .map(SessionSelector::Id)
            .context("invalid id: session selector must contain a UUID");
    }
    if let Some(name) = value.strip_prefix("name:") {
        if name.is_empty() {
            bail!("name selector must not be empty");
        }
        return Ok(SessionSelector::Name(name.into()));
    }
    Ok(value
        .parse()
        .map(SessionSelector::Id)
        .unwrap_or_else(|_| SessionSelector::Name(value.into())))
}

fn session_selector(value: &str) -> SessionSelector {
    value
        .parse()
        .map(SessionSelector::Id)
        .unwrap_or_else(|_| SessionSelector::Name(value.into()))
}

async fn only_session(socket: &std::path::Path) -> Result<SessionSelector> {
    match control(socket, ClientMessage::ListResources).await? {
        ServerMessage::Resources { snapshot } => {
            let open = snapshot
                .sessions
                .iter()
                .filter(|session| !session.closing)
                .collect::<Vec<_>>();
            if open.len() != 1 {
                bail!(
                    "session selector required when {} sessions are open",
                    open.len()
                );
            }
            Ok(SessionSelector::Id(open[0].id))
        }
        other => unexpected(other),
    }
}

fn print_resources(snapshot: &ResourceSnapshot) {
    println!("revision={}", snapshot.revision);
    for session in &snapshot.sessions {
        println!(
            "session {} {:?}{}",
            session.id,
            session.name,
            if session.closing { " closing" } else { "" }
        );
        for workspace in &session.workspaces {
            println!(
                "  workspace {} {:?} {}",
                workspace.id,
                workspace.name,
                workspace.root.display()
            );
            for tab in &workspace.tabs {
                println!("    tab {} {:?}", tab.id, tab.name);
                for pane in &tab.panes {
                    println!(
                        "      pane {} terminal={}{}",
                        pane.id,
                        pane.terminal_id,
                        if pane.closing { " closing" } else { "" }
                    );
                }
            }
        }
    }
}

async fn send(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    message: ClientMessage,
) -> Result<()> {
    framed
        .send(Bytes::from(encode_payload(&Envelope {
            request_id: Some(Uuid::new_v4()),
            message,
        })?))
        .await?;
    Ok(())
}

async fn receive(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    duration: Duration,
    timeout_message: &'static str,
) -> Result<ServerMessage> {
    let frame = tokio::time::timeout(duration, framed.next())
        .await
        .context(timeout_message)?
        .context("daemon disconnected")??;
    Ok(decode_payload::<Envelope<ServerMessage>>(&frame)?.message)
}

fn response_ok(response: ServerMessage, expected: AcknowledgedCommand) -> Result<()> {
    match response {
        ServerMessage::CommandCompleted { command } if command == expected => Ok(()),
        other => unexpected(other),
    }
}

fn unexpected<T>(message: ServerMessage) -> Result<T> {
    match message {
        ServerMessage::Error { code, message } => bail!("daemon error ({code}): {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_new_tab_workspace_parent() {
        let workspace_id = WorkspaceId::new();
        for parent in [
            workspace_id.to_string(),
            format!("workspace:{workspace_id}"),
        ] {
            let cli = Cli::try_parse_from(["fut", "new-tab", &parent]).unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::NewTab {
                    workspace_id: parsed,
                    ..
                }) if parsed == workspace_id
            ));
        }

        for parent in [
            "not-a-uuid",
            "workspace:not-a-uuid",
            "tab:00000000-0000-0000-0000-000000000000",
        ] {
            let error = match Cli::try_parse_from(["fut", "new-tab", parent]) {
                Ok(_) => panic!("accepted malformed workspace parent {parent:?}"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .contains("expected a UUID or workspace:<uuid>"),
                "unexpected error for {parent:?}: {error}"
            );
        }
    }

    #[test]
    fn parses_explicit_and_convenient_session_selectors() {
        let id = crate::domain::SessionId::new();
        assert_eq!(
            selector(&format!("id:{id}")).unwrap(),
            TargetSelector::Session(SessionSelector::Id(id))
        );
        assert_eq!(
            selector(&id.to_string()).unwrap(),
            TargetSelector::Session(SessionSelector::Id(id))
        );
        assert_eq!(
            selector(&format!("name:{id}")).unwrap(),
            TargetSelector::Session(SessionSelector::Name(id.to_string()))
        );
        assert_eq!(
            selector("name:雪 λ").unwrap(),
            TargetSelector::Session(SessionSelector::Name("雪 λ".into()))
        );
        assert!(
            selector("id:not-a-uuid")
                .unwrap_err()
                .to_string()
                .contains("invalid id")
        );
        let workspace = crate::domain::WorkspaceId::new();
        assert_eq!(
            selector(&format!("workspace:{workspace}")).unwrap(),
            TargetSelector::Workspace(workspace)
        );
        assert!(
            selector("terminal:not-a-uuid")
                .unwrap_err()
                .to_string()
                .contains("invalid terminal id")
        );
        for malformed in [
            "",
            "id:",
            "name:",
            "session:",
            "workspace:",
            "tab:",
            "pane:",
            "terminal:",
            "unknown:value",
        ] {
            assert!(selector(malformed).is_err(), "accepted {malformed:?}");
        }
        assert_eq!(
            selector("name:has:colons").unwrap(),
            TargetSelector::Session(SessionSelector::Name("has:colons".into()))
        );
    }

    #[test]
    fn parses_rename_selectors() {
        let session = crate::domain::SessionId::new();
        let workspace = crate::domain::WorkspaceId::new();
        let tab = crate::domain::TabId::new();

        for (target, expected) in [
            (
                format!("session:{session}"),
                RenameSelector::Session(SessionSelector::Id(session)),
            ),
            (
                format!("id:{session}"),
                RenameSelector::Session(SessionSelector::Id(session)),
            ),
            (
                "name:exact name".into(),
                RenameSelector::Session(SessionSelector::Name("exact name".into())),
            ),
            (
                "bare name".into(),
                RenameSelector::Session(SessionSelector::Name("bare name".into())),
            ),
            (
                format!("workspace:{workspace}"),
                RenameSelector::Workspace(workspace),
            ),
            (format!("tab:{tab}"), RenameSelector::Tab(tab)),
        ] {
            assert_eq!(
                rename_selector(&target).unwrap(),
                expected,
                "target {target:?}"
            );
        }

        let pane = crate::domain::PaneId::new();
        let terminal = crate::domain::TerminalId::new();
        for target in [format!("pane:{pane}"), format!("terminal:{terminal}")] {
            assert_eq!(
                rename_selector(&target).unwrap_err().to_string(),
                "rename target must be a session, workspace, or tab"
            );
        }
        for malformed in [
            "",
            "session:",
            "id:",
            "id:not-a-uuid",
            "name:",
            "workspace:",
            "workspace:not-a-uuid",
            "tab:",
            "tab:not-a-uuid",
            "unknown:value",
        ] {
            assert!(
                rename_selector(malformed).is_err(),
                "accepted {malformed:?}"
            );
        }
    }

    #[test]
    fn parses_rename_command_name_as_one_argument() {
        let cli = Cli::try_parse_from(["fut", "rename", "name:old", "new name"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Rename { target, name })
                if target == "name:old" && name == "new name"
        ));
    }
}
