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
    daemon::{DaemonConfig, autostart::ensure_daemon, path::socket_path, run_daemon},
    protocol::{
        AcknowledgedCommand, ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, ServerMessage,
        codec, decode_payload, encode_payload,
    },
    resources::{ResourceSnapshot, SessionSelector},
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
        /// Session selector: id:<uuid>, name:<exact>, UUID, or exact name.
        session: Option<String>,
    },
    New {
        name: String,
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
        /// Session selector: id:<uuid>, name:<exact>, UUID, or exact name.
        session: Option<String>,
    },
    Shutdown,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let socket = socket_path(cli.socket.as_deref())?;
    match cli.command {
        None | Some(Command::Attach { session: None }) => {
            let cwd = std::env::current_dir().context("read current directory")?;
            ensure_daemon(&socket, &cwd).await?;
            client::attach(&socket, None).await
        }
        Some(Command::Attach {
            session: Some(session),
        }) => client::attach(&socket, Some(selector(&session)?)).await,
        Some(Command::New { name, cwd, command }) => {
            let cwd = cwd.unwrap_or(std::env::current_dir()?);
            let (program, argv) = command
                .split_first()
                .map_or((None, vec![]), |(program, argv)| {
                    (Some(PathBuf::from(program)), argv.to_vec())
                });
            match control(
                &socket,
                ClientMessage::CreateSession {
                    name,
                    cwd,
                    program,
                    argv,
                },
            )
            .await?
            {
                ServerMessage::SessionCreated { selected } => {
                    println!(
                        "session={} terminal={} pid={}",
                        selected.session_id, selected.terminal_id, selected.child_pid
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
        Some(Command::Close { session }) => {
            let selector = match session {
                Some(value) => selector(&value)?,
                None => only_session(&socket).await?,
            };
            response_ok(
                control(&socket, ClientMessage::CloseSession { selector }).await?,
                AcknowledgedCommand::CloseSession,
            )?;
            println!("closed=true");
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

fn selector(value: &str) -> Result<SessionSelector> {
    if let Some(id) = value.strip_prefix("id:") {
        return id
            .parse()
            .map(SessionSelector::Id)
            .context("invalid id: session selector must contain a UUID");
    }
    if let Some(name) = value.strip_prefix("name:") {
        return Ok(SessionSelector::Name(name.into()));
    }
    Ok(value
        .parse()
        .map(SessionSelector::Id)
        .unwrap_or_else(|_| SessionSelector::Name(value.into())))
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
    fn parses_explicit_and_convenient_session_selectors() {
        let id = crate::domain::SessionId::new();
        assert_eq!(
            selector(&format!("id:{id}")).unwrap(),
            SessionSelector::Id(id)
        );
        assert_eq!(selector(&id.to_string()).unwrap(), SessionSelector::Id(id));
        assert_eq!(
            selector(&format!("name:{id}")).unwrap(),
            SessionSelector::Name(id.to_string())
        );
        assert_eq!(
            selector("name:雪 λ").unwrap(),
            SessionSelector::Name("雪 λ".into())
        );
        assert!(
            selector("id:not-a-uuid")
                .unwrap_err()
                .to_string()
                .contains("invalid id")
        );
    }
}
