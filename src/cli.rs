use std::{ffi::OsString, path::PathBuf, process::ExitCode, time::Duration};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::{Parser, Subcommand, ValueHint};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::json;
use tokio::net::UnixStream;
use tokio_util::codec::Framed;
use uuid::Uuid;

mod completion;

use clap_complete::engine::ArgValueCompleter;

use crate::{
    client,
    daemon::{
        DaemonConfig,
        autostart::{ensure_daemon, protocol_ready},
        path::socket_path,
        run_daemon,
    },
    domain::{PaneId, SessionId, TabId, TerminalId, WorkspaceId},
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
    about = "A project-oriented terminal multiplexer",
    after_help = "Enable shell completion with, for example: source <(COMPLETE=zsh fut)"
)]
pub struct Cli {
    /// Override the Unix socket used to contact the daemon.
    #[arg(long, global = true, value_hint = ValueHint::FilePath)]
    socket: Option<PathBuf>,
    /// Emit versioned JSON for noninteractive commands only.
    #[arg(long, global = true)]
    json: bool,
    /// Command to run; omit it to open the current directory and attach.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open a location through an existing daemon without attaching.
    Open {
        /// Directory to open; defaults to the current directory.
        #[arg(value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: Option<PathBuf>,
        /// Name for the new session or workspace created for this location.
        #[arg(long)]
        name: Option<String>,
        /// Child program and its direct argv, following `--`; defaults to the shell.
        #[arg(last = true, value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,
    },
    /// Attach, rename, or close a session.
    Session {
        /// Session operation to perform.
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Attach, rename, or close a workspace.
    Workspace {
        /// Workspace operation to perform.
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Create, attach, rename, or close a tab.
    Tab {
        /// Tab operation to perform.
        #[command(subcommand)]
        command: TabCommand,
    },
    /// Create, attach, move, or close a pane.
    Pane {
        /// Pane operation to perform.
        #[command(subcommand)]
        command: PaneCommand,
    },
    /// Attach to a terminal.
    Terminal {
        /// Terminal operation to perform.
        #[command(subcommand)]
        command: TerminalCommand,
    },
    /// List resources from the existing daemon.
    List,
    /// Diagnose configuration, terminal capabilities, and daemon connectivity.
    Doctor,
    /// Run or control the daemon.
    Daemon {
        /// Daemon operation to perform.
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Attach to a session that contains exactly one open terminal.
    Attach {
        /// Session UUID or name; a UUID-shaped value is always treated as an ID.
        #[arg(add = ArgValueCompleter::new(completion::session_attach))]
        session: String,
    },
    /// Rename a session by raw UUID on the existing daemon.
    Rename {
        /// Raw session UUID.
        #[arg(add = ArgValueCompleter::new(completion::session_rename))]
        session_id: SessionId,
        /// New session name.
        name: String,
    },
    /// Close a session by raw UUID on the existing daemon.
    Close {
        /// Raw session UUID.
        #[arg(add = ArgValueCompleter::new(completion::session_close))]
        session_id: SessionId,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Attach to a workspace on the existing daemon.
    Attach {
        /// Raw workspace UUID; the workspace must contain exactly one open terminal.
        #[arg(add = ArgValueCompleter::new(completion::workspace_attach))]
        workspace_id: WorkspaceId,
    },
    /// Rename a workspace by raw UUID on the existing daemon.
    Rename {
        /// Raw workspace UUID.
        #[arg(add = ArgValueCompleter::new(completion::workspace_rename))]
        workspace_id: WorkspaceId,
        /// New workspace name.
        name: String,
    },
    /// Close a workspace by raw UUID on the existing daemon.
    Close {
        /// Raw workspace UUID.
        #[arg(add = ArgValueCompleter::new(completion::workspace_close))]
        workspace_id: WorkspaceId,
    },
}

#[derive(Subcommand)]
enum TabCommand {
    /// Create a tab through an existing daemon without attaching.
    New {
        /// Raw UUID of the workspace that will own the tab.
        #[arg(add = ArgValueCompleter::new(completion::tab_new))]
        workspace_id: WorkspaceId,
        /// Name for the new tab; defaults to shell, shell-2, and so on.
        #[arg(long)]
        name: Option<String>,
        /// Working directory for the child; defaults to the workspace root.
        #[arg(long, value_hint = ValueHint::DirPath)]
        cwd: Option<PathBuf>,
        /// Child program and its direct argv, following `--`; defaults to the shell.
        #[arg(last = true, value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,
    },
    /// Attach to a tab on the existing daemon.
    Attach {
        /// Raw tab UUID; the tab must contain exactly one open terminal.
        #[arg(add = ArgValueCompleter::new(completion::tab_attach))]
        tab_id: TabId,
    },
    /// Rename a tab by raw UUID on the existing daemon.
    Rename {
        /// Raw tab UUID.
        #[arg(add = ArgValueCompleter::new(completion::tab_rename))]
        tab_id: TabId,
        /// New tab name.
        name: String,
    },
    /// Close a tab by raw UUID on the existing daemon.
    Close {
        /// Raw tab UUID.
        #[arg(add = ArgValueCompleter::new(completion::tab_close))]
        tab_id: TabId,
    },
}

#[derive(Subcommand)]
enum PaneCommand {
    /// Create a pane through an existing daemon without attaching.
    New {
        /// Raw UUID of the tab that will own the pane.
        #[arg(add = ArgValueCompleter::new(completion::pane_new))]
        tab_id: TabId,
        /// Working directory for the child; defaults to the workspace root.
        #[arg(long, value_hint = ValueHint::DirPath)]
        cwd: Option<PathBuf>,
        /// Child program and its direct argv, following `--`; defaults to the shell.
        #[arg(last = true, value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,
    },
    /// Attach to a pane on the existing daemon.
    Attach {
        /// Raw pane UUID identifying one terminal placement.
        #[arg(add = ArgValueCompleter::new(completion::pane_attach))]
        pane_id: PaneId,
    },
    /// Move a pane to another tab in the same workspace.
    Move {
        /// Raw UUID of the pane to move.
        #[arg(add = ArgValueCompleter::new(completion::pane_move_source))]
        pane_id: PaneId,
        /// Raw UUID of the destination tab.
        #[arg(add = ArgValueCompleter::new(completion::pane_move_destination))]
        destination_tab_id: TabId,
    },
    /// Close a pane by raw UUID on the existing daemon.
    Close {
        /// Raw pane UUID.
        #[arg(add = ArgValueCompleter::new(completion::pane_close))]
        pane_id: PaneId,
    },
}

#[derive(Subcommand)]
enum TerminalCommand {
    /// Attach to a terminal on the existing daemon.
    Attach {
        /// Raw terminal UUID identifying one process-bearing terminal.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Run the daemon in the foreground.
    Run {
        /// Initial child working directory; defaults to the current directory.
        #[arg(long, value_hint = ValueHint::DirPath)]
        cwd: Option<PathBuf>,
        /// Initial child program and its direct argv, following `--`; defaults to the shell.
        #[arg(last = true, value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,
    },
    /// Check whether the existing daemon is responsive.
    Ping,
    /// Ask the existing daemon to shut down.
    Shutdown,
}

pub fn complete() {
    completion::complete_env();
}

pub async fn run() -> ExitCode {
    run_from(std::env::args_os()).await
}

async fn run_from(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let args = args.into_iter().collect::<Vec<_>>();
    let json_requested = json_requested(&args);
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) if error.use_stderr() && json_requested => {
            render_json_error("invalid_arguments", error.to_string());
            return ExitCode::from(error.exit_code() as u8);
        }
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(code as u8);
        }
    };
    let json_output = cli.json;
    if matches!(cli.command, Some(Command::Doctor)) {
        let socket = match socket_path(cli.socket.as_deref()) {
            Ok(socket) => socket,
            Err(error) => {
                if json_output {
                    render_json_error("command_failed", format!("{error:#}"));
                } else {
                    eprintln!("Error: {error:#}");
                }
                return ExitCode::FAILURE;
            }
        };
        let report = crate::doctor::run(&socket).await;
        if json_output {
            if let Err(error) = output(true, "doctor", &report, "") {
                render_json_error("command_failed", format!("{error:#}"));
                return ExitCode::FAILURE;
            }
        } else {
            print!("{}", report.render_human());
        }
        return if report.has_errors() {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }
    match execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if json_output {
                let code = error
                    .downcast_ref::<CliError>()
                    .map_or("command_failed", |error| error.code.as_str());
                render_json_error(code, format!("{error:#}"));
            } else {
                eprintln!("Error: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn execute(cli: Cli) -> Result<()> {
    let socket = socket_path(cli.socket.as_deref())?;
    reject_interactive_json(&cli)?;
    match cli.command {
        None => {
            let cwd = std::env::current_dir().context("read current directory")?;
            open_and_attach(&socket, cwd).await
        }
        Some(Command::Open {
            path,
            name,
            command,
        }) => {
            let cwd = path.unwrap_or(std::env::current_dir()?);
            let (program, argv) = child_command(command);
            match control(
                &socket,
                ClientMessage::OpenLocation {
                    name,
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
                } => output(
                    cli.json,
                    "open",
                    json!({ "disposition": disposition, "selected": selected }),
                    format!(
                        "disposition={disposition:?} session={} workspace={} tab={} pane={} terminal={} pid={}",
                        selected.session_id,
                        selected.workspace_id,
                        selected.tab_id,
                        selected.pane_id,
                        selected.terminal_id,
                        selected.child_pid
                    ),
                ),
                other => unexpected(other),
            }
        }
        Some(Command::Session {
            command: SessionCommand::Attach { session },
        }) => {
            client::attach(
                &socket,
                Some(TargetSelector::Session(session_selector(&session))),
            )
            .await
        }
        Some(Command::Workspace {
            command: WorkspaceCommand::Attach { workspace_id },
        }) => client::attach(&socket, Some(TargetSelector::Workspace(workspace_id))).await,
        Some(Command::Tab {
            command: TabCommand::Attach { tab_id },
        }) => client::attach(&socket, Some(TargetSelector::Tab(tab_id))).await,
        Some(Command::Pane {
            command: PaneCommand::Attach { pane_id },
        }) => client::attach(&socket, Some(TargetSelector::Pane(pane_id))).await,
        Some(Command::Pane {
            command:
                PaneCommand::New {
                    tab_id,
                    cwd,
                    command,
                },
        }) => {
            let (program, argv) = child_command(command);
            match control(
                &socket,
                ClientMessage::CreatePane {
                    tab_id,
                    cwd,
                    program,
                    argv,
                },
            )
            .await?
            {
                ServerMessage::PaneCreated { selected } => output(
                    cli.json,
                    "pane.new",
                    json!({ "selected": selected }),
                    format!(
                        "session={} workspace={} tab={} pane={} terminal={} pid={}",
                        selected.session_id,
                        selected.workspace_id,
                        selected.tab_id,
                        selected.pane_id,
                        selected.terminal_id,
                        selected.child_pid
                    ),
                ),
                other => unexpected(other),
            }
        }
        Some(Command::Pane {
            command:
                PaneCommand::Move {
                    pane_id,
                    destination_tab_id,
                },
        }) => match control(
            &socket,
            ClientMessage::MovePane {
                pane_id,
                destination_tab_id,
            },
        )
        .await?
        {
            ServerMessage::PaneMoved {
                source_tab_id,
                moved,
                source_tab_closed,
                selected,
            } => output(
                cli.json,
                "pane.move",
                json!({
                    "source_tab_id": source_tab_id,
                    "moved": moved,
                    "source_tab_closed": source_tab_closed,
                    "selected": selected,
                }),
                format!(
                    "source_tab_id={source_tab_id} moved={moved} source_tab_closed={source_tab_closed} session={} workspace={} tab={} pane={} terminal={} pid={}",
                    selected.session_id,
                    selected.workspace_id,
                    selected.tab_id,
                    selected.pane_id,
                    selected.terminal_id,
                    selected.child_pid
                ),
            ),
            other => unexpected(other),
        },
        Some(Command::Terminal {
            command: TerminalCommand::Attach { terminal_id },
        }) => client::attach(&socket, Some(TargetSelector::Terminal(terminal_id))).await,
        Some(Command::Tab {
            command:
                TabCommand::New {
                    workspace_id,
                    name,
                    cwd,
                    command,
                },
        }) => {
            let (program, argv) = child_command(command);
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
                ServerMessage::TabCreated { selected } => output(
                    cli.json,
                    "tab.new",
                    json!({ "selected": selected }),
                    format!(
                        "session={} workspace={} tab={} pane={} terminal={} pid={}",
                        selected.session_id,
                        selected.workspace_id,
                        selected.tab_id,
                        selected.pane_id,
                        selected.terminal_id,
                        selected.child_pid
                    ),
                ),
                other => unexpected(other),
            }
        }
        Some(Command::List) => match control(&socket, ClientMessage::ListResources).await? {
            ServerMessage::Resources { snapshot } => {
                if cli.json {
                    output(true, "list", &snapshot, String::new())
                } else {
                    print_resources(&snapshot);
                    Ok(())
                }
            }
            other => unexpected(other),
        },
        Some(Command::Daemon {
            command: DaemonCommand::Run { cwd, command },
        }) => {
            let cwd = cwd.unwrap_or(std::env::current_dir()?);
            let mut config = DaemonConfig::shell(socket, cwd);
            if let Some(program) = command.first() {
                config.spawn.program = program.into();
                config.spawn.argv = command[1..].to_vec();
            }
            run_daemon(config).await
        }
        Some(Command::Daemon {
            command: DaemonCommand::Ping,
        }) => {
            let response = control(&socket, ClientMessage::Ping).await?;
            match response {
                ServerMessage::Pong { daemon_pid } => {
                    return output(
                        cli.json,
                        "daemon.ping",
                        json!({ "daemon_pid": daemon_pid }),
                        format!("pong pid={daemon_pid}"),
                    );
                }
                other => unexpected(other)?,
            }
            Ok(())
        }
        Some(Command::Daemon {
            command: DaemonCommand::Shutdown,
        }) => {
            response_ok(
                shutdown_control(&socket).await?,
                AcknowledgedCommand::Shutdown,
            )?;
            output(
                cli.json,
                "daemon.shutdown",
                json!({ "shutdown": true }),
                "shutdown=true",
            )
        }
        Some(Command::Doctor) => unreachable!("doctor is handled before command execution"),
        Some(command) => run_mutation(&socket, cli.json, command).await,
    }
}

fn child_command(command: Vec<String>) -> (Option<PathBuf>, Vec<String>) {
    command
        .split_first()
        .map_or((None, vec![]), |(program, argv)| {
            (Some(program.into()), argv.to_vec())
        })
}

fn reject_interactive_json(cli: &Cli) -> Result<()> {
    if !cli.json {
        return Ok(());
    }
    let interactive = cli.command.is_none()
        || matches!(
            cli.command,
            Some(Command::Session {
                command: SessionCommand::Attach { .. }
            }) | Some(Command::Workspace {
                command: WorkspaceCommand::Attach { .. }
            }) | Some(Command::Tab {
                command: TabCommand::Attach { .. }
            }) | Some(Command::Pane {
                command: PaneCommand::Attach { .. }
            }) | Some(Command::Terminal {
                command: TerminalCommand::Attach { .. }
            }) | Some(Command::Daemon {
                command: DaemonCommand::Run { .. }
            })
        );
    if interactive {
        return Err(CliError::new(
            "invalid_arguments",
            "--json is not supported for interactive commands",
        )
        .into());
    }
    Ok(())
}

async fn run_mutation(socket: &std::path::Path, json_output: bool, command: Command) -> Result<()> {
    enum Mutation {
        Close(TargetSelector),
        Rename(RenameSelector, String),
    }

    let (mutation, command_name, result) = match command {
        Command::Session {
            command: SessionCommand::Rename { session_id, name },
        } => (
            Mutation::Rename(
                RenameSelector::Session(SessionSelector::Id(session_id)),
                name.clone(),
            ),
            "session.rename",
            json!({"session_id": session_id, "name": name}),
        ),
        Command::Session {
            command: SessionCommand::Close { session_id },
        } => (
            Mutation::Close(TargetSelector::Session(SessionSelector::Id(session_id))),
            "session.close",
            json!({"session_id": session_id}),
        ),
        Command::Workspace {
            command: WorkspaceCommand::Rename { workspace_id, name },
        } => (
            Mutation::Rename(RenameSelector::Workspace(workspace_id), name.clone()),
            "workspace.rename",
            json!({"workspace_id": workspace_id, "name": name}),
        ),
        Command::Workspace {
            command: WorkspaceCommand::Close { workspace_id },
        } => (
            Mutation::Close(TargetSelector::Workspace(workspace_id)),
            "workspace.close",
            json!({"workspace_id": workspace_id}),
        ),
        Command::Tab {
            command: TabCommand::Rename { tab_id, name },
        } => (
            Mutation::Rename(RenameSelector::Tab(tab_id), name.clone()),
            "tab.rename",
            json!({"tab_id": tab_id, "name": name}),
        ),
        Command::Tab {
            command: TabCommand::Close { tab_id },
        } => (
            Mutation::Close(TargetSelector::Tab(tab_id)),
            "tab.close",
            json!({"tab_id": tab_id}),
        ),
        Command::Pane {
            command: PaneCommand::Close { pane_id },
        } => (
            Mutation::Close(TargetSelector::Pane(pane_id)),
            "pane.close",
            json!({"pane_id": pane_id}),
        ),
        _ => unreachable!("all non-mutation commands handled by run"),
    };
    match mutation {
        Mutation::Rename(selector, name) => {
            response_ok(
                control(socket, ClientMessage::RenameTarget { selector, name }).await?,
                AcknowledgedCommand::RenameTarget,
            )?;
            output(json_output, command_name, result, "renamed=true")
        }
        Mutation::Close(selector) => {
            response_ok(
                control(socket, ClientMessage::CloseTarget { selector }).await?,
                AcknowledgedCommand::CloseTarget,
            )?;
            output(json_output, command_name, result, "closed=true")
        }
    }
}

fn output(
    result_as_json: bool,
    command: &str,
    result: impl Serialize,
    human: impl AsRef<str>,
) -> Result<()> {
    if result_as_json {
        println!(
            "{}",
            serde_json::to_string(&json!({ "version": 1, "command": command, "result": result }))?
        );
    } else {
        println!("{}", human.as_ref());
    }
    Ok(())
}

#[derive(Debug)]
struct CliError {
    code: String,
    message: String,
}

impl CliError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

fn json_requested(args: &[OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|argument| argument.as_os_str() != "--")
        .any(|argument| argument.as_os_str() == "--json")
}

fn error_envelope(code: &str, message: impl Into<String>) -> serde_json::Value {
    json!({ "version": 1, "error": { "code": code, "message": message.into() } })
}

fn render_json_error(code: &str, message: impl Into<String>) {
    eprintln!(
        "{}",
        serde_json::to_string(&error_envelope(code, message))
            .expect("the CLI error envelope is always serializable")
    );
}

async fn open_and_attach(socket: &std::path::Path, cwd: PathBuf) -> Result<()> {
    let ui = client::load_ui_config()?;
    let selected = open_current_location(socket, &cwd).await?;
    client::attach_with_ui(
        socket,
        Some(TargetSelector::Terminal(selected.terminal_id)),
        ui,
    )
    .await
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
    match control_attempt(socket, command, PROTOCOL_VERSION).await? {
        ControlAttempt::Response(response) => Ok(response),
        ControlAttempt::Incompatible { server } => bail!(
            "daemon at {} uses protocol {server}, but this Fut client requires protocol \
             {PROTOCOL_VERSION}",
            socket.display()
        ),
    }
}

async fn shutdown_control(socket: &std::path::Path) -> Result<ServerMessage> {
    match control_attempt(socket, ClientMessage::Shutdown, PROTOCOL_VERSION).await? {
        ControlAttempt::Response(response) => Ok(response),
        ControlAttempt::Incompatible { server } => {
            match control_attempt(socket, ClientMessage::Shutdown, server).await? {
                ControlAttempt::Response(response) => Ok(response),
                ControlAttempt::Incompatible { server: changed } => bail!(
                    "daemon at {} changed protocol from {server} to {changed} during shutdown",
                    socket.display()
                ),
            }
        }
    }
}

enum ControlAttempt {
    Response(ServerMessage),
    Incompatible { server: u16 },
}

async fn control_attempt(
    socket: &std::path::Path,
    command: ClientMessage,
    version: u16,
) -> Result<ControlAttempt> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
    let mut framed = Framed::new(stream, codec());
    send(
        &mut framed,
        ClientMessage::Hello {
            version,
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
        ServerMessage::Welcome {
            version: server, ..
        } if server == version => {}
        ServerMessage::IncompatibleProtocol { server, .. } => {
            return Ok(ControlAttempt::Incompatible { server });
        }
        other => return unexpected(other),
    }
    send(&mut framed, command).await?;
    Ok(ControlAttempt::Response(
        receive(
            &mut framed,
            Duration::from_secs(15),
            "daemon response timed out",
        )
        .await?,
    ))
}

fn session_selector(value: &str) -> SessionSelector {
    value
        .parse()
        .map(SessionSelector::Id)
        .unwrap_or_else(|_| SessionSelector::Name(value.into()))
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
                println!(
                    "    tab {} {:?}{}",
                    tab.id,
                    tab.name,
                    if tab.closing { " closing" } else { "" }
                );
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
        ServerMessage::Error { code, message } => {
            Err(CliError::new(code, format!("daemon error: {message}")).into())
        }
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_json_only_before_the_child_argv_delimiter() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(json_requested(&args(&["fut", "open", "--json"])));
        assert!(json_requested(&args(&["fut", "open", "--json", "--", "x"])));
        assert!(!json_requested(&args(&[
            "fut", "open", "--", "x", "--json"
        ])));
        assert!(!json_requested(&args(&["fut", "--json=value", "list"])));
    }

    #[test]
    fn error_envelope_has_the_stable_shape() {
        assert_eq!(
            error_envelope("invalid_arguments", "bad input"),
            json!({"version": 1, "error": {"code": "invalid_arguments", "message": "bad input"}})
        );
    }

    #[test]
    fn daemon_errors_remain_typed_through_anyhow() {
        let error = unexpected::<()>(ServerMessage::Error {
            code: "not_found".into(),
            message: "missing session".into(),
        })
        .unwrap_err();
        let cli_error = error.downcast_ref::<CliError>().unwrap();
        assert_eq!(cli_error.code, "not_found");
        assert_eq!(cli_error.to_string(), "daemon error: missing session");
    }

    #[tokio::test]
    async fn shutdown_reconnects_with_the_running_daemon_protocol() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("fut.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, codec());
            let first = framed.next().await.unwrap().unwrap();
            let first: Envelope<ClientMessage> = decode_payload(&first).unwrap();
            assert!(matches!(
                first.message,
                ClientMessage::Hello {
                    version: PROTOCOL_VERSION,
                    ..
                }
            ));
            framed
                .send(Bytes::from(
                    encode_payload(&Envelope {
                        request_id: first.request_id,
                        message: ServerMessage::IncompatibleProtocol {
                            client: PROTOCOL_VERSION,
                            server: 4,
                        },
                    })
                    .unwrap(),
                ))
                .await
                .unwrap();
            drop(framed);

            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, codec());
            let second = framed.next().await.unwrap().unwrap();
            let second: Envelope<ClientMessage> = decode_payload(&second).unwrap();
            assert!(matches!(
                second.message,
                ClientMessage::Hello { version: 4, .. }
            ));
            framed
                .send(Bytes::from(
                    encode_payload(&Envelope {
                        request_id: second.request_id,
                        message: ServerMessage::Welcome {
                            version: 4,
                            server_version: "old".into(),
                            selected: None,
                        },
                    })
                    .unwrap(),
                ))
                .await
                .unwrap();
            let shutdown = framed.next().await.unwrap().unwrap();
            let shutdown: Envelope<ClientMessage> = decode_payload(&shutdown).unwrap();
            assert_eq!(shutdown.message, ClientMessage::Shutdown);
            framed
                .send(Bytes::from(
                    encode_payload(&Envelope {
                        request_id: shutdown.request_id,
                        message: ServerMessage::CommandCompleted {
                            command: AcknowledgedCommand::Shutdown,
                        },
                    })
                    .unwrap(),
                ))
                .await
                .unwrap();
        });

        assert_eq!(
            shutdown_control(&socket).await.unwrap(),
            ServerMessage::CommandCompleted {
                command: AcknowledgedCommand::Shutdown,
            }
        );
        server.await.unwrap();
    }

    #[test]
    fn help_and_version_are_successful_clap_results() {
        for args in [&["fut", "--help"][..], &["fut", "--json", "--version"][..]] {
            let error = match Cli::try_parse_from(args) {
                Ok(_) => panic!("help/version unexpectedly parsed as a command"),
                Err(error) => error,
            };
            assert!(!error.use_stderr());
            assert_eq!(error.exit_code(), 0);
        }
    }

    #[test]
    fn parses_entire_command_tree() {
        let session = SessionId::new().to_string();
        let workspace = WorkspaceId::new().to_string();
        let tab = TabId::new().to_string();
        let pane = PaneId::new().to_string();
        let terminal = TerminalId::new().to_string();
        for args in [
            vec!["fut", "open"],
            vec!["fut", "list"],
            vec!["fut", "session", "attach", "a name"],
            vec!["fut", "session", "rename", &session, "new"],
            vec!["fut", "session", "close", &session],
            vec!["fut", "workspace", "attach", &workspace],
            vec!["fut", "workspace", "rename", &workspace, "new"],
            vec!["fut", "workspace", "close", &workspace],
            vec!["fut", "tab", "new", &workspace],
            vec!["fut", "tab", "attach", &tab],
            vec!["fut", "tab", "rename", &tab, "new"],
            vec!["fut", "tab", "close", &tab],
            vec!["fut", "pane", "new", &tab],
            vec!["fut", "pane", "attach", &pane],
            vec!["fut", "pane", "move", &pane, &tab],
            vec!["fut", "pane", "close", &pane],
            vec!["fut", "terminal", "attach", &terminal],
            vec!["fut", "daemon", "run"],
            vec!["fut", "daemon", "ping"],
            vec!["fut", "daemon", "shutdown"],
        ] {
            Cli::try_parse_from(args).unwrap();
        }
    }

    #[test]
    fn session_attach_interprets_only_raw_uuid_as_id() {
        let id = SessionId::new();
        assert_eq!(session_selector(&id.to_string()), SessionSelector::Id(id));
        assert_eq!(
            session_selector("id:abc"),
            SessionSelector::Name("id:abc".into())
        );
        assert_eq!(
            session_selector("name:abc"),
            SessionSelector::Name("name:abc".into())
        );
    }

    #[test]
    fn child_commands_require_delimiter_and_preserve_flags() {
        assert!(Cli::try_parse_from(["fut", "open", ".", "echo"]).is_err());
        let cli = Cli::try_parse_from(["fut", "open", "--", "echo", "--flag"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Open { command, .. }) if command == ["echo", "--flag"])
        );
        let workspace = WorkspaceId::new().to_string();
        assert!(Cli::try_parse_from(["fut", "tab", "new", &workspace, "echo"]).is_err());
        let tab = TabId::new().to_string();
        assert!(Cli::try_parse_from(["fut", "pane", "new", &tab, "echo"]).is_err());
        let cli =
            Cli::try_parse_from(["fut", "pane", "new", &tab, "--", "echo", "--flag"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Pane { command: PaneCommand::New { command, .. } }) if command == ["echo", "--flag"])
        );
        assert!(Cli::try_parse_from(["fut", "daemon", "run", "echo"]).is_err());
    }

    #[test]
    fn rejects_legacy_forms_typed_prefixes_and_bad_mutation_ids() {
        for args in [
            ["fut", "new"],
            ["fut", "new-tab"],
            ["fut", "attach"],
            ["fut", "rename"],
            ["fut", "close"],
            ["fut", "ping"],
            ["fut", "shutdown"],
            ["fut", "sess"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        assert!(Cli::try_parse_from(["fut", "session", "att"]).is_err());
        assert!(Cli::try_parse_from(["fut", "workspace", "attach", "workspace:abc"]).is_err());
        assert!(Cli::try_parse_from(["fut", "session", "rename", "a-name", "new"]).is_err());
        assert!(Cli::try_parse_from(["fut", "move-pane"]).is_err());
        assert!(Cli::try_parse_from(["fut", "pane", "move", "bad", "also-bad"]).is_err());
        let pane = PaneId::new().to_string();
        assert!(Cli::try_parse_from(["fut", "pane", "move", &pane]).is_err());
    }

    #[test]
    fn rejects_json_for_interactive_commands() {
        let terminal = TerminalId::new().to_string();
        for args in [
            vec!["fut", "--json"],
            vec!["fut", "--json", "terminal", "attach", &terminal],
            vec!["fut", "--json", "daemon", "run"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(reject_interactive_json(&cli).is_err());
        }
        let cli = Cli::try_parse_from(["fut", "--json", "list"]).unwrap();
        assert!(reject_interactive_json(&cli).is_ok());
        let tab = TabId::new().to_string();
        let cli = Cli::try_parse_from(["fut", "--json", "pane", "new", &tab]).unwrap();
        assert!(reject_interactive_json(&cli).is_ok());
    }

    #[test]
    fn rejects_removed_attach_flags() {
        let workspace = WorkspaceId::new().to_string();
        assert!(Cli::try_parse_from(["fut", "open", "--attach"]).is_err());
        assert!(Cli::try_parse_from(["fut", "tab", "new", &workspace, "--attach"]).is_err());
        let tab = TabId::new().to_string();
        assert!(Cli::try_parse_from(["fut", "pane", "new", &tab, "--attach"]).is_err());
    }

    #[test]
    fn help_exposes_the_exact_noun_first_tree_and_command_contracts() {
        use clap::CommandFactory;

        let command = Cli::command();
        let names: Vec<_> = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(
            names,
            [
                "open",
                "session",
                "workspace",
                "tab",
                "pane",
                "terminal",
                "list",
                "doctor",
                "daemon"
            ]
        );

        let mut help = Vec::new();
        Cli::command().write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(help.contains("versioned JSON for noninteractive commands only"));
        assert!(help.contains("existing daemon without attaching"));

        let command = Cli::command();
        let pane = command.find_subcommand("pane").unwrap();
        let movement = pane.find_subcommand("move").unwrap();
        let positional_names: Vec<_> = movement
            .get_arguments()
            .map(clap::Arg::get_id)
            .map(ToString::to_string)
            .collect();
        assert_eq!(positional_names, ["pane_id", "destination_tab_id"]);
    }
}
