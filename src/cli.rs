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
    domain::TerminalSize,
    protocol::{
        AcknowledgedCommand, ClientKind, ClientMessage, Envelope, PROTOCOL_VERSION, ServerMessage,
        codec, decode_payload, encode_payload,
    },
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
    Attach,
    Daemon {
        #[arg(long)]
        foreground: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    Ping,
    Close,
    Shutdown,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let socket = socket_path(cli.socket.as_deref())?;
    match cli.command {
        None | Some(Command::Attach) => {
            let cwd = std::env::current_dir().context("read current directory")?;
            ensure_daemon(&socket, &cwd).await?;
            client::attach(&socket).await
        }
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
        Some(Command::Close) => {
            response_ok(
                control(&socket, ClientMessage::CloseTerminal).await?,
                AcknowledgedCommand::CloseTerminal,
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
            kind: ClientKind::Control,
            size: TerminalSize {
                columns: 80,
                rows: 24,
            },
        },
    )
    .await?;
    match receive(&mut framed).await? {
        ServerMessage::Welcome { .. } => {}
        other => return unexpected(other),
    }
    send(&mut framed, command).await?;
    receive(&mut framed).await
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
) -> Result<ServerMessage> {
    let frame = tokio::time::timeout(Duration::from_secs(2), framed.next())
        .await
        .context("daemon response timed out")?
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
