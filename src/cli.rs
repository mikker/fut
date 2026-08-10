use std::{ffi::OsString, path::PathBuf, process::ExitCode, str::FromStr, time::Duration};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::{ArgGroup, Parser, Subcommand, ValueEnum, ValueHint};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::json;
use tokio::net::UnixStream;
use tokio_util::codec::Framed;
use uuid::Uuid;

mod completion;

// Bundled at build time so the printed skill always matches this binary's release.
const AGENT_SKILL: &str = include_str!("../skills/fut/SKILL.md");

use clap_complete::engine::ArgValueCompleter;

use crate::{
    client,
    daemon::{
        DaemonConfig,
        autostart::{ensure_daemon, protocol_ready},
        path::socket_path,
        run_daemon,
    },
    domain::{
        AgentReport, AgentReportMetadata, AgentState, MAX_TERMINAL_OUTPUT_ROWS, PaneId, SessionId,
        TabId, TerminalId, TerminalOutputMatcher, TerminalOutputSource, WorkspaceId,
    },
    protocol::{
        AcknowledgedCommand, ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION,
        PROTOCOL_VERSION_0_1, RenameSelector, ServerMessage, TerminalInputOperation, codec,
        decode_payload, encode_payload,
    },
    resources::{ResourceSnapshot, SessionSelector, TabSnapshot, TargetSelector},
    splits::{SplitAxis, SplitDirection, SplitTree},
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
    /// Create, list, attach, rename, or close a tab.
    Tab {
        /// Tab operation to perform.
        #[command(subcommand)]
        command: TabCommand,
    },
    /// Create, split, list, attach, move, or close a pane.
    Pane {
        /// Pane operation to perform.
        #[command(subcommand)]
        command: PaneCommand,
    },
    /// Attach to or send input to a terminal.
    Terminal {
        /// Terminal operation to perform.
        #[command(subcommand)]
        command: TerminalCommand,
    },
    /// Inspect or control coding-agent integrations.
    Agent {
        /// Agent operation to perform.
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Resolve the current terminal ancestry from Fut's environment.
    Context,
    /// Look up any Fut resource by its globally unique UUID.
    Get {
        /// Raw session, workspace, tab, pane, or terminal UUID.
        #[arg(add = ArgValueCompleter::new(completion::get))]
        id: Uuid,
    },
    /// List resources from the existing daemon.
    List,
    /// Stream resource changes from the existing daemon as JSON lines.
    ///
    /// The first line is the current state; every later line is the complete
    /// state after a change. Output is always versioned JSON.
    Events,
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
        /// Name for the new tab; unnamed by default.
        #[arg(long)]
        name: Option<String>,
        /// Working directory for the child; defaults to the workspace root.
        #[arg(long, value_hint = ValueHint::DirPath)]
        cwd: Option<PathBuf>,
        /// Child program and its direct argv, following `--`; defaults to the shell.
        #[arg(last = true, value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,
    },
    /// List the tabs of a workspace, including each tab's split layout.
    List {
        /// Raw UUID of the workspace that owns the tabs.
        #[arg(add = ArgValueCompleter::new(completion::tab_new))]
        workspace_id: WorkspaceId,
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
    /// Split an explicit pane through an existing daemon without attaching.
    Split {
        /// Raw UUID of the pane to split.
        #[arg(add = ArgValueCompleter::new(completion::pane_attach))]
        pane_id: PaneId,
        /// Place the new pane to the right of or below the anchor pane.
        #[arg(value_enum)]
        direction: PaneSplitDirection,
        /// Working directory for the child; defaults to the anchor pane's directory.
        #[arg(long, value_hint = ValueHint::DirPath)]
        cwd: Option<PathBuf>,
        /// Child program and its direct argv, following `--`; defaults to the shell.
        #[arg(last = true, value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,
    },
    /// List the panes of a tab, including the tab's split layout.
    List {
        /// Raw UUID of the tab that owns the panes.
        #[arg(add = ArgValueCompleter::new(completion::pane_new))]
        tab_id: TabId,
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

#[derive(Clone, Copy, ValueEnum)]
enum PaneSplitDirection {
    Right,
    Down,
}

impl From<PaneSplitDirection> for SplitDirection {
    fn from(value: PaneSplitDirection) -> Self {
        match value {
            PaneSplitDirection::Right => Self::Right,
            PaneSplitDirection::Down => Self::Down,
        }
    }
}

#[derive(Subcommand)]
enum TerminalCommand {
    /// Attach to a terminal on the existing daemon.
    Attach {
        /// Raw terminal UUID identifying one process-bearing terminal.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
    },
    /// Send literal text without submitting it.
    SendText {
        /// Raw UUID of the terminal that will receive the text.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
        /// Literal Unicode text, encoded using the terminal's current paste mode.
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Send validated logical keys or control chords.
    SendKeys {
        /// Raw UUID of the terminal that will receive the keys.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
        /// Named key, one character, or chord such as ctrl+c or alt+left.
        #[arg(required = true, num_args = 1..)]
        keys: Vec<LogicalKey>,
    },
    /// Send literal command text and Enter as one atomic operation.
    Run {
        /// Raw UUID of the terminal that will receive the command.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
        /// Literal command text to submit.
        #[arg(allow_hyphen_values = true)]
        command: String,
    },
    /// Read a bounded terminal output snapshot.
    Read {
        /// Raw UUID of the terminal to inspect.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
        /// Visible viewport or a bounded recent physical-row window.
        #[arg(long, value_enum, default_value = "visible")]
        source: TerminalOutputSourceArg,
        /// Physical rows for recent sources; defaults to 200.
        #[arg(long, value_parser = parse_output_lines)]
        lines: Option<usize>,
        /// Preserve terminal colors and styles as ANSI escape sequences.
        #[arg(long)]
        ansi: bool,
    },
    /// Wait for literal or regular-expression output without polling.
    #[command(group(ArgGroup::new("matcher").required(true).args(["literal", "regex"])))]
    WaitOutput {
        /// Raw UUID of the terminal to observe.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
        /// Literal Unicode text to match.
        #[arg(long, allow_hyphen_values = true)]
        literal: Option<String>,
        /// Rust regular expression to match.
        #[arg(long, allow_hyphen_values = true)]
        regex: Option<String>,
        /// Required deadline, such as 500ms, 30s, or 2m.
        #[arg(long, value_parser = parse_output_timeout)]
        timeout: Duration,
        /// Output source; defaults to recent soft-wrap reconstruction.
        #[arg(long, value_enum, default_value = "recent-unwrapped")]
        source: TerminalOutputSourceArg,
        /// Physical rows retained for matching; defaults to 200.
        #[arg(long, value_parser = parse_output_lines)]
        lines: Option<usize>,
    },
    /// Report explicit agent state for a terminal.
    Report {
        /// Agent state or completion event.
        #[arg(value_enum)]
        state: AgentReportArg,
        /// Terminal UUID; defaults to FUT_TERMINAL_ID inside Fut.
        #[arg(long)]
        terminal_id: Option<TerminalId>,
        /// Integration name, such as codex or claude-code.
        #[arg(long)]
        source: Option<String>,
        /// Integration-owned agent session identifier.
        #[arg(long)]
        agent_session_id: Option<String>,
        /// Integration-owned turn identifier for this report.
        #[arg(long)]
        turn_id: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TerminalOutputSourceArg {
    Visible,
    Recent,
    RecentUnwrapped,
}

impl From<TerminalOutputSourceArg> for TerminalOutputSource {
    fn from(value: TerminalOutputSourceArg) -> Self {
        match value {
            TerminalOutputSourceArg::Visible => Self::Visible,
            TerminalOutputSourceArg::Recent => Self::Recent,
            TerminalOutputSourceArg::RecentUnwrapped => Self::RecentUnwrapped,
        }
    }
}

fn parse_output_timeout(value: &str) -> Result<Duration, String> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else {
        return Err("timeout must end in ms, s, or m".into());
    };
    let amount = number
        .parse::<u64>()
        .map_err(|_| "timeout must contain a positive integer".to_owned())?;
    let milliseconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "timeout is too large".to_owned())?;
    if !(1..=3_600_000).contains(&milliseconds) {
        return Err("timeout must be between 1ms and 1h".into());
    }
    Ok(Duration::from_millis(milliseconds))
}

fn parse_output_lines(value: &str) -> Result<usize, String> {
    let lines = value
        .parse::<usize>()
        .map_err(|_| "lines must be a positive integer".to_owned())?;
    if !(1..=MAX_TERMINAL_OUTPUT_ROWS).contains(&lines) {
        return Err(format!(
            "lines must be between 1 and {MAX_TERMINAL_OUTPUT_ROWS}"
        ));
    }
    Ok(lines)
}

#[derive(Clone)]
struct LogicalKey {
    bytes: Vec<u8>,
}

impl FromStr for LogicalKey {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let event = logical_key_event(value)?;
        let bytes = client::input::encode_key(event)
            .ok_or_else(|| format!("unsupported logical key: {value}"))?;
        Ok(Self { bytes })
    }
}

fn logical_key_event(value: &str) -> Result<KeyEvent, String> {
    if value.is_empty() {
        return Err("logical key cannot be empty".into());
    }
    if let Some(code) = named_key_code(value) {
        return Ok(KeyEvent::new(code, KeyModifiers::NONE));
    }
    if value.chars().count() == 1 {
        return Ok(KeyEvent::new(
            KeyCode::Char(value.chars().next().expect("one character")),
            KeyModifiers::NONE,
        ));
    }

    let parts = value.split('+').collect::<Vec<_>>();
    let (key, modifiers) = parts
        .split_last()
        .ok_or_else(|| format!("invalid logical key: {value}"))?;
    if modifiers.is_empty() || key.is_empty() {
        return Err(format!("invalid logical key: {value}"));
    }
    let mut parsed_modifiers = KeyModifiers::NONE;
    for modifier in modifiers {
        parsed_modifiers |= match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "meta" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            _ => return Err(format!("unknown key modifier `{modifier}` in `{value}`")),
        };
    }
    let mut code = named_key_code(key)
        .or_else(|| (key.chars().count() == 1).then(|| KeyCode::Char(key.chars().next().unwrap())));
    if parsed_modifiers.contains(KeyModifiers::SHIFT) {
        code = match code {
            Some(KeyCode::Tab) => Some(KeyCode::BackTab),
            Some(KeyCode::Char(character)) if character.is_ascii_lowercase() => {
                Some(KeyCode::Char(character.to_ascii_uppercase()))
            }
            other => other,
        };
    }
    if parsed_modifiers.contains(KeyModifiers::CONTROL)
        && !matches!(code, Some(KeyCode::Char(character)) if control_character_supported(character))
    {
        return Err(format!("control modifier is unsupported for `{key}`"));
    }
    if parsed_modifiers.contains(KeyModifiers::SHIFT)
        && !matches!(
            code,
            Some(KeyCode::Char(_) | KeyCode::Tab | KeyCode::BackTab)
        )
    {
        return Err(format!("shift modifier is unsupported for `{key}`"));
    }
    code.map(|code| KeyEvent::new(code, parsed_modifiers))
        .ok_or_else(|| format!("unknown logical key `{key}` in `{value}`"))
}

fn control_character_supported(character: char) -> bool {
    matches!(
        character.to_ascii_lowercase(),
        '@' | ' ' | 'a'..='z' | '[' | '\\' | ']' | '^' | '_' | '?'
    )
}

fn named_key_code(value: &str) -> Option<KeyCode> {
    Some(match value.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" | "back-tab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "escape" | "esc" => KeyCode::Esc,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "right" => KeyCode::Right,
        "left" => KeyCode::Left,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "insert" => KeyCode::Insert,
        "delete" | "del" => KeyCode::Delete,
        "page-up" | "pageup" => KeyCode::PageUp,
        "page-down" | "pagedown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        function if function.starts_with('f') => {
            let number = function[1..].parse::<u8>().ok()?;
            if !(1..=12).contains(&number) {
                return None;
            }
            KeyCode::F(number)
        }
        _ => return None,
    })
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Print the bundled agent skill file.
    Skill,
    /// List integrated agent terminals.
    List,
    /// Inspect one integrated agent terminal.
    Get {
        /// Raw UUID of the integrated terminal.
        #[arg(add = ArgValueCompleter::new(completion::agent))]
        terminal_id: TerminalId,
    },
    /// Submit one prompt as literal text followed atomically by Enter.
    Prompt {
        /// Raw UUID of the integrated terminal.
        #[arg(add = ArgValueCompleter::new(completion::agent))]
        terminal_id: TerminalId,
        /// Literal Unicode prompt text.
        #[arg(allow_hyphen_values = true)]
        text: String,
        /// Wait for a fresh working transition and subsequent settled report.
        #[arg(long)]
        wait: bool,
        /// Required with --wait; such as 500ms, 30s, or 2m.
        #[arg(long, value_parser = parse_output_timeout)]
        timeout: Option<Duration>,
    },
    /// Wait for a currently working agent to settle, or return current settled state.
    Wait {
        /// Raw UUID of the integrated terminal.
        #[arg(add = ArgValueCompleter::new(completion::agent))]
        terminal_id: TerminalId,
        /// Required deadline, such as 500ms, 30s, or 2m.
        #[arg(long, value_parser = parse_output_timeout)]
        timeout: Duration,
    },
    /// Read bounded output together with current agent availability.
    Read {
        /// Raw UUID of the integrated terminal.
        #[arg(add = ArgValueCompleter::new(completion::agent))]
        terminal_id: TerminalId,
        /// Visible viewport or a bounded recent physical-row window.
        #[arg(long, value_enum, default_value = "visible")]
        source: TerminalOutputSourceArg,
        /// Physical rows for recent sources; defaults to 200.
        #[arg(long, value_parser = parse_output_lines)]
        lines: Option<usize>,
        /// Preserve terminal colors and styles as ANSI escape sequences.
        #[arg(long)]
        ansi: bool,
    },
    /// Report explicit lifecycle state for an agent terminal.
    Report {
        /// Agent state or completion event.
        #[arg(value_enum)]
        state: AgentReportArg,
        /// Terminal UUID; defaults to FUT_TERMINAL_ID inside Fut.
        #[arg(long)]
        terminal_id: Option<TerminalId>,
        /// Integration name, such as codex or claude-code.
        #[arg(long)]
        source: Option<String>,
        /// Integration-owned agent session identifier.
        #[arg(long)]
        agent_session_id: Option<String>,
        /// Integration-owned turn identifier for this report.
        #[arg(long)]
        turn_id: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum AgentReportArg {
    Idle,
    Working,
    Blocked,
    Completed,
}

impl From<AgentReportArg> for AgentReport {
    fn from(value: AgentReportArg) -> Self {
        match value {
            AgentReportArg::Idle => Self::Idle,
            AgentReportArg::Working => Self::Working,
            AgentReportArg::Blocked => Self::Blocked,
            AgentReportArg::Completed => Self::Completed,
        }
    }
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
    if matches!(
        &cli.command,
        Some(Command::Agent {
            command: AgentCommand::Skill
        })
    ) {
        if cli.json {
            return Err(CliError::new(
                "invalid_arguments",
                "--json is not supported for `fut agent skill`",
            )
            .into());
        }
        print!("{AGENT_SKILL}");
        return Ok(());
    }

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
                PaneCommand::Split {
                    pane_id,
                    direction,
                    cwd,
                    command,
                },
        }) => {
            let (program, argv) = child_command(command);
            let direction = SplitDirection::from(direction);
            let direction_name = match direction {
                SplitDirection::Right => "right",
                SplitDirection::Down => "down",
            };
            match control(
                &socket,
                ClientMessage::SplitPane {
                    pane_id,
                    direction,
                    cwd,
                    program,
                    argv,
                },
            )
            .await?
            {
                ServerMessage::PaneCreated { selected } => output(
                    cli.json,
                    "pane.split",
                    json!({
                        "anchor_pane_id": pane_id,
                        "direction": direction,
                        "selected": selected,
                    }),
                    format!(
                        "anchor={} direction={direction_name} session={} workspace={} tab={} pane={} terminal={} pid={}",
                        pane_id,
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
        Some(Command::Agent {
            command: AgentCommand::List,
        }) => {
            let snapshot = list_resources(&socket).await?;
            let agents = integrated_agents(&snapshot);
            let human = agents
                .iter()
                .map(render_agent)
                .collect::<Vec<_>>()
                .join("\n");
            output(
                cli.json,
                "agent.list",
                json!({ "revision": snapshot.revision, "agents": agents }),
                human,
            )
        }
        Some(Command::Agent {
            command: AgentCommand::Get { terminal_id },
        }) => {
            let snapshot = list_resources(&socket).await?;
            let agent = resolve_agent(&snapshot, terminal_id)?;
            output(
                cli.json,
                "agent.get",
                json!({ "revision": snapshot.revision, "agent": agent }),
                render_agent(&agent),
            )
        }
        Some(Command::Agent {
            command:
                AgentCommand::Prompt {
                    terminal_id,
                    text,
                    wait,
                    timeout,
                },
        }) => {
            if wait != timeout.is_some() {
                return Err(CliError::new(
                    "invalid_arguments",
                    "--wait and --timeout must be used together",
                )
                .into());
            }
            match control(
                &socket,
                ClientMessage::PromptAgent {
                    terminal_id,
                    text,
                    wait,
                    timeout_ms: timeout.map(|value| value.as_millis() as u64),
                },
            )
            .await?
            {
                ServerMessage::AgentPrompted {
                    terminal_id,
                    barrier_revision,
                } => output(
                    cli.json,
                    "agent.prompt",
                    json!({
                        "terminal_id": terminal_id,
                        "barrier_revision": barrier_revision,
                        "submitted": true,
                    }),
                    format!("agent={terminal_id} submitted=true barrier={barrier_revision}"),
                ),
                ServerMessage::AgentSettled {
                    terminal_id,
                    barrier_revision,
                    working_revision,
                    activity,
                } => output(
                    cli.json,
                    "agent.prompt",
                    json!({
                        "terminal_id": terminal_id,
                        "barrier_revision": barrier_revision,
                        "working_revision": working_revision,
                        "submitted": true,
                        "activity": activity,
                    }),
                    format!(
                        "agent={terminal_id} submitted=true state={:?} barrier={barrier_revision} working={}",
                        activity.state,
                        working_revision
                            .map_or_else(|| "-".into(), |revision| revision.to_string())
                    ),
                ),
                other => unexpected(other),
            }
        }
        Some(Command::Agent {
            command:
                AgentCommand::Wait {
                    terminal_id,
                    timeout,
                },
        }) => match control(
            &socket,
            ClientMessage::WaitAgent {
                terminal_id,
                timeout_ms: timeout.as_millis() as u64,
            },
        )
        .await?
        {
            ServerMessage::AgentSettled {
                terminal_id,
                barrier_revision,
                working_revision,
                activity,
            } => output(
                cli.json,
                "agent.wait",
                json!({
                    "terminal_id": terminal_id,
                    "barrier_revision": barrier_revision,
                    "working_revision": working_revision,
                    "activity": activity,
                }),
                format!("agent={terminal_id} state={:?}", activity.state),
            ),
            other => unexpected(other),
        },
        Some(Command::Agent {
            command:
                AgentCommand::Read {
                    terminal_id,
                    source,
                    lines,
                    ansi,
                },
        }) => {
            let source = TerminalOutputSource::from(source);
            if source == TerminalOutputSource::Visible && lines.is_some() {
                return Err(CliError::new(
                    "invalid_arguments",
                    "--lines is only valid with --source recent or recent-unwrapped",
                )
                .into());
            }
            // Resolve integration before observing the underlying terminal;
            // lifecycle presence is sticky for the terminal's lifetime.
            resolve_agent(&list_resources(&socket).await?, terminal_id)?;
            let captured = match control(
                &socket,
                ClientMessage::ReadTerminalOutput {
                    terminal_id,
                    source,
                    rows: lines.unwrap_or(200),
                    ansi,
                },
            )
            .await?
            {
                ServerMessage::TerminalOutput { output } => output,
                other => return unexpected(other),
            };
            let snapshot = list_resources(&socket).await?;
            let agent = resolve_agent(&snapshot, terminal_id)?;
            let human = captured.text.clone();
            output(
                cli.json,
                "agent.read",
                json!({
                    "revision": snapshot.revision,
                    "agent": agent,
                    "output": captured,
                }),
                human,
            )
        }
        Some(Command::Agent {
            command:
                AgentCommand::Report {
                    state,
                    terminal_id,
                    source,
                    agent_session_id,
                    turn_id,
                },
        }) => {
            report_agent_command(
                &socket,
                cli.json,
                "agent.report",
                state,
                terminal_id,
                AgentReportMetadata {
                    source,
                    agent_session_id,
                    turn_id,
                },
            )
            .await
        }
        Some(Command::Terminal {
            command: TerminalCommand::Attach { terminal_id },
        }) => client::attach(&socket, Some(TargetSelector::Terminal(terminal_id))).await,
        Some(Command::Terminal {
            command: TerminalCommand::SendText { terminal_id, text },
        }) => {
            let byte_count = text.len();
            terminal_input(
                &socket,
                ClientMessage::TerminalInput {
                    terminal_id,
                    operation: TerminalInputOperation::Text { text },
                },
            )
            .await?;
            output(
                cli.json,
                "terminal.send-text",
                json!({ "terminal_id": terminal_id, "bytes": byte_count }),
                format!("terminal={terminal_id} bytes={byte_count} submitted=false"),
            )
        }
        Some(Command::Terminal {
            command: TerminalCommand::SendKeys { terminal_id, keys },
        }) => {
            let key_count = keys.len();
            let bytes = keys
                .into_iter()
                .flat_map(|key| key.bytes)
                .collect::<Vec<_>>();
            terminal_input(
                &socket,
                ClientMessage::TerminalInput {
                    terminal_id,
                    operation: TerminalInputOperation::Keys { bytes },
                },
            )
            .await?;
            output(
                cli.json,
                "terminal.send-keys",
                json!({ "terminal_id": terminal_id, "keys": key_count }),
                format!("terminal={terminal_id} keys={key_count}"),
            )
        }
        Some(Command::Terminal {
            command:
                TerminalCommand::Run {
                    terminal_id,
                    command,
                },
        }) => {
            let byte_count = command.len();
            terminal_input(
                &socket,
                ClientMessage::TerminalInput {
                    terminal_id,
                    operation: TerminalInputOperation::Run { text: command },
                },
            )
            .await?;
            output(
                cli.json,
                "terminal.run",
                json!({ "terminal_id": terminal_id, "bytes": byte_count }),
                format!("terminal={terminal_id} bytes={byte_count} submitted=true"),
            )
        }
        Some(Command::Terminal {
            command:
                TerminalCommand::Read {
                    terminal_id,
                    source,
                    lines,
                    ansi,
                },
        }) => {
            let source = TerminalOutputSource::from(source);
            if source == TerminalOutputSource::Visible && lines.is_some() {
                return Err(CliError::new(
                    "invalid_arguments",
                    "--lines is only valid with --source recent or recent-unwrapped",
                )
                .into());
            }
            let rows = lines.unwrap_or(200);
            match control(
                &socket,
                ClientMessage::ReadTerminalOutput {
                    terminal_id,
                    source,
                    rows,
                    ansi,
                },
            )
            .await?
            {
                ServerMessage::TerminalOutput { output: captured } => {
                    let human = captured.text.clone();
                    output(cli.json, "terminal.read", captured, human)
                }
                other => unexpected(other),
            }
        }
        Some(Command::Terminal {
            command:
                TerminalCommand::WaitOutput {
                    terminal_id,
                    literal,
                    regex,
                    timeout,
                    source,
                    lines,
                },
        }) => {
            let source = TerminalOutputSource::from(source);
            if source == TerminalOutputSource::Visible && lines.is_some() {
                return Err(CliError::new(
                    "invalid_arguments",
                    "--lines is only valid with --source recent or recent-unwrapped",
                )
                .into());
            }
            let matcher = match (literal, regex) {
                (Some(value), None) => TerminalOutputMatcher::Literal(value),
                (None, Some(value)) => TerminalOutputMatcher::Regex(value),
                _ => unreachable!("clap requires exactly one output matcher"),
            };
            let rows = lines.unwrap_or(200);
            match control(
                &socket,
                ClientMessage::WaitTerminalOutput {
                    terminal_id,
                    source,
                    rows,
                    matcher,
                    timeout_ms: timeout.as_millis() as u64,
                },
            )
            .await?
            {
                ServerMessage::TerminalOutputMatched {
                    output: captured,
                    start,
                    end,
                    matched,
                } => output(
                    cli.json,
                    "terminal.wait-output",
                    json!({
                        "output": captured,
                        "match": { "start": start, "end": end, "text": matched },
                    }),
                    matched,
                ),
                other => unexpected(other),
            }
        }
        Some(Command::Terminal {
            command:
                TerminalCommand::Report {
                    state,
                    terminal_id,
                    source,
                    agent_session_id,
                    turn_id,
                },
        }) => {
            report_agent_command(
                &socket,
                cli.json,
                "terminal.report",
                state,
                terminal_id,
                AgentReportMetadata {
                    source,
                    agent_session_id,
                    turn_id,
                },
            )
            .await
        }
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
        Some(Command::Events) => stream_events(&socket).await,
        Some(Command::Context) => {
            let environment = context_environment()?;
            let snapshot = list_resources(&socket).await?;
            let terminal_uuid = environment
                .terminal_id
                .to_string()
                .parse()
                .expect("typed Fut IDs contain UUIDs");
            let target = discover_target(&snapshot, terminal_uuid)?;
            validate_context_target(&target, environment)?;
            output(
                cli.json,
                "context",
                json!({ "revision": snapshot.revision, "target": target }),
                render_discovered_target(snapshot.revision, &target),
            )
        }
        Some(Command::Get { id }) => {
            let snapshot = list_resources(&socket).await?;
            let target = discover_target(&snapshot, id)?;
            output(
                cli.json,
                "get",
                json!({ "revision": snapshot.revision, "target": target }),
                render_discovered_target(snapshot.revision, &target),
            )
        }
        Some(Command::List) => {
            let snapshot = list_resources(&socket).await?;
            if cli.json {
                output(true, "list", &snapshot, String::new())
            } else {
                print_resources(&snapshot);
                Ok(())
            }
        }
        Some(Command::Tab {
            command: TabCommand::List { workspace_id },
        }) => {
            let snapshot = list_resources(&socket).await?;
            let workspace = snapshot
                .sessions
                .iter()
                .flat_map(|session| &session.workspaces)
                .find(|workspace| workspace.id == workspace_id)
                .ok_or_else(|| {
                    CliError::new("not_found", format!("workspace {workspace_id} not found"))
                })?;
            output(
                cli.json,
                "tab.list",
                json!({
                    "revision": snapshot.revision,
                    "workspace_id": workspace_id,
                    "tabs": workspace.tabs,
                }),
                render_tabs(snapshot.revision, &workspace.tabs),
            )
        }
        Some(Command::Pane {
            command: PaneCommand::List { tab_id },
        }) => {
            let snapshot = list_resources(&socket).await?;
            let tab = snapshot
                .sessions
                .iter()
                .flat_map(|session| &session.workspaces)
                .flat_map(|workspace| &workspace.tabs)
                .find(|tab| tab.id == tab_id)
                .ok_or_else(|| CliError::new("not_found", format!("tab {tab_id} not found")))?;
            output(
                cli.json,
                "pane.list",
                json!({
                    "revision": snapshot.revision,
                    "tab_id": tab_id,
                    "layout": tab.layout,
                    "panes": tab.panes,
                }),
                render_tabs(snapshot.revision, std::slice::from_ref(tab)),
            )
        }
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
        Some(Command::Agent {
            command: AgentCommand::Skill,
        }) => unreachable!("agent skill is handled before daemon setup"),
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

async fn terminal_input(socket: &std::path::Path, message: ClientMessage) -> Result<()> {
    response_ok(
        control(socket, message).await?,
        AcknowledgedCommand::TerminalInput,
    )
}

async fn report_agent_command(
    socket: &std::path::Path,
    json_output: bool,
    command: &'static str,
    state: AgentReportArg,
    terminal_id: Option<TerminalId>,
    metadata: AgentReportMetadata,
) -> Result<()> {
    let terminal_id = match terminal_id {
        Some(terminal_id) => terminal_id,
        None => std::env::var("FUT_TERMINAL_ID")
            .context("FUT_TERMINAL_ID is unavailable; pass --terminal-id")?
            .parse()
            .context("FUT_TERMINAL_ID is invalid")?,
    };
    response_ok(
        control(
            socket,
            ClientMessage::ReportAgent {
                terminal_id,
                report: state.into(),
                metadata: metadata.clone(),
            },
        )
        .await?,
        AcknowledgedCommand::ReportAgent,
    )?;
    output(
        json_output,
        command,
        json!({
            "terminal_id": terminal_id,
            "state": AgentReport::from(state),
            "metadata": metadata,
        }),
        format!(
            "agent={terminal_id} state={}",
            state.to_possible_value().expect("value enum").get_name()
        ),
    )
}

fn integrated_agents(snapshot: &ResourceSnapshot) -> Vec<serde_json::Value> {
    let mut agents = Vec::new();
    for session in &snapshot.sessions {
        for workspace in &session.workspaces {
            for tab in &workspace.tabs {
                for pane in &tab.panes {
                    if pane.activity.integration.is_none() {
                        continue;
                    }
                    let available = !session.closing
                        && !workspace.closing
                        && !tab.closing
                        && !pane.closing
                        && pane.activity.state != AgentState::Working;
                    agents.push(json!({
                        "terminal_id": pane.terminal_id,
                        "pane_id": pane.id,
                        "tab": { "id": tab.id, "name": tab.name },
                        "workspace": {
                            "id": workspace.id,
                            "name": workspace.name,
                            "root": workspace.root,
                        },
                        "session": { "id": session.id, "name": session.name },
                        "available": available,
                        "activity": pane.activity,
                    }));
                }
            }
        }
    }
    agents
}

fn resolve_agent(
    snapshot: &ResourceSnapshot,
    terminal_id: TerminalId,
) -> Result<serde_json::Value> {
    if let Some(agent) = integrated_agents(snapshot)
        .into_iter()
        .find(|agent| agent["terminal_id"] == terminal_id.to_string())
    {
        return Ok(agent);
    }
    let terminal_exists = snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.workspaces)
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| &tab.panes)
        .any(|pane| pane.terminal_id == terminal_id);
    let (code, message) = if terminal_exists {
        (
            "not_an_agent",
            format!("terminal {terminal_id} has no agent integration"),
        )
    } else {
        ("not_found", format!("terminal {terminal_id} was not found"))
    };
    Err(CliError::new(code, message).into())
}

fn render_agent(agent: &serde_json::Value) -> String {
    format!(
        "agent={} state={} available={} session={} workspace={} tab={} pane={}",
        agent["terminal_id"].as_str().unwrap_or("-"),
        agent["activity"]["state"].as_str().unwrap_or("-"),
        agent["available"].as_bool().unwrap_or(false),
        agent["session"]["id"].as_str().unwrap_or("-"),
        agent["workspace"]["id"].as_str().unwrap_or("-"),
        agent["tab"]["id"].as_str().unwrap_or("-"),
        agent["pane_id"].as_str().unwrap_or("-"),
    )
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
    request(connected_control(socket).await?, command).await
}

/// Connects at the current protocol version, failing on a mismatched daemon.
async fn connected_control(
    socket: &std::path::Path,
) -> Result<Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>> {
    match control_handshake(socket, PROTOCOL_VERSION).await? {
        ControlHandshake::Connected(framed) => Ok(framed),
        ControlHandshake::Incompatible { server } => bail!(
            "daemon at {} uses protocol {server}, but this Fut client requires protocol \
             {PROTOCOL_VERSION}",
            socket.display()
        ),
    }
}

async fn shutdown_control(socket: &std::path::Path) -> Result<ServerMessage> {
    match control_handshake(socket, PROTOCOL_VERSION).await? {
        ControlHandshake::Connected(framed) => request(framed, ClientMessage::Shutdown).await,
        ControlHandshake::Incompatible { server } => match shutdown_downgrade_version(server) {
            Some(version) => match control_handshake(socket, version).await? {
                ControlHandshake::Connected(framed) => {
                    request(framed, ClientMessage::Shutdown).await
                }
                ControlHandshake::Incompatible { server: changed } => bail!(
                    "daemon at {} changed protocol from {server} to {changed} during shutdown",
                    socket.display()
                ),
            },
            None => bail!(
                "daemon at {} uses protocol {server}, but this Fut client can only shut down \
                 current protocol {PROTOCOL_VERSION} or Fut 0.1 protocol \
                 {PROTOCOL_VERSION_0_1}",
                socket.display()
            ),
        },
    }
}

fn shutdown_downgrade_version(server: u16) -> Option<u16> {
    (server == PROTOCOL_VERSION_0_1).then_some(server)
}

enum ControlHandshake {
    Connected(Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>),
    Incompatible { server: u16 },
}

async fn control_handshake(socket: &std::path::Path, version: u16) -> Result<ControlHandshake> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
    let mut framed = Framed::new(stream, codec());
    let hello_request_id = send(
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
        hello_request_id,
        Duration::from_secs(2),
        "daemon handshake timed out",
    )
    .await?
    {
        ServerMessage::Welcome {
            version: server, ..
        } if server == version => Ok(ControlHandshake::Connected(framed)),
        ServerMessage::IncompatibleProtocol { server, .. } => {
            Ok(ControlHandshake::Incompatible { server })
        }
        other => unexpected(other),
    }
}

async fn request(
    mut framed: Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    command: ClientMessage,
) -> Result<ServerMessage> {
    let response_timeout = match &command {
        ClientMessage::WaitTerminalOutput { timeout_ms, .. }
        | ClientMessage::WaitAgent { timeout_ms, .. } => {
            Duration::from_millis((*timeout_ms).min(3_600_000)) + Duration::from_secs(1)
        }
        ClientMessage::PromptAgent {
            wait: true,
            timeout_ms: Some(timeout_ms),
            ..
        } => Duration::from_millis((*timeout_ms).min(3_600_000)) + Duration::from_secs(1),
        _ => Duration::from_secs(15),
    };
    let request_id = send(&mut framed, command).await?;
    receive(
        &mut framed,
        request_id,
        response_timeout,
        "daemon response timed out",
    )
    .await
}

/// Streams the current resource snapshot and every later change as one
/// versioned JSON line each, until the daemon exits.
async fn stream_events(socket: &std::path::Path) -> Result<()> {
    let mut framed = connected_control(socket).await?;
    let request_id = send(&mut framed, ClientMessage::WatchResources).await?;
    match receive(
        &mut framed,
        request_id,
        Duration::from_secs(15),
        "daemon response timed out",
    )
    .await?
    {
        ServerMessage::Resources { snapshot } => output(true, "events", &snapshot, "")?,
        other => return unexpected(other),
    }
    while let Some(frame) = framed.next().await {
        let envelope: Envelope<ServerMessage> = decode_payload(&frame?)?;
        match envelope.message {
            ServerMessage::ResourcesChanged { snapshot } => {
                output(true, "events", &snapshot, "")?;
            }
            other => return unexpected(other),
        }
    }
    Ok(())
}

fn session_selector(value: &str) -> SessionSelector {
    value
        .parse()
        .map(SessionSelector::Id)
        .unwrap_or_else(|_| SessionSelector::Name(value.into()))
}

async fn list_resources(socket: &std::path::Path) -> Result<ResourceSnapshot> {
    match control(socket, ClientMessage::ListResources).await? {
        ServerMessage::Resources { snapshot } => Ok(snapshot),
        other => unexpected(other),
    }
}

#[derive(Clone, Copy)]
struct ContextEnvironment {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    tab_id: TabId,
    pane_id: PaneId,
    terminal_id: TerminalId,
}

fn context_environment() -> Result<ContextEnvironment> {
    const NAMES: [&str; 5] = [
        "FUT_SESSION_ID",
        "FUT_WORKSPACE_ID",
        "FUT_TAB_ID",
        "FUT_PANE_ID",
        "FUT_TERMINAL_ID",
    ];
    let present = NAMES
        .iter()
        .filter(|name| std::env::var_os(name).is_some())
        .count();
    if present == 0 {
        return Err(CliError::new(
            "missing_context",
            "Fut context is unavailable; run inside Fut or use `fut get <UUID>`",
        )
        .into());
    }
    if present != NAMES.len() {
        let missing = NAMES
            .iter()
            .filter(|name| std::env::var_os(name).is_none())
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CliError::new(
            "invalid_context",
            format!("Fut context is incomplete; missing {missing}"),
        )
        .into());
    }

    Ok(ContextEnvironment {
        session_id: context_id("FUT_SESSION_ID")?,
        workspace_id: context_id("FUT_WORKSPACE_ID")?,
        tab_id: context_id("FUT_TAB_ID")?,
        pane_id: context_id("FUT_PANE_ID")?,
        terminal_id: context_id("FUT_TERMINAL_ID")?,
    })
}

fn context_id<T>(name: &'static str) -> Result<T>
where
    T: std::str::FromStr,
{
    let value = std::env::var(name)
        .map_err(|_| CliError::new("invalid_context", format!("{name} is not valid UTF-8")))?;
    value
        .parse()
        .map_err(|_| CliError::new("invalid_context", format!("{name} is not a valid UUID")).into())
}

fn validate_context_target(
    target: &serde_json::Value,
    environment: ContextEnvironment,
) -> Result<()> {
    let expected = [
        ("/session/id", environment.session_id.to_string()),
        ("/workspace/id", environment.workspace_id.to_string()),
        ("/tab/id", environment.tab_id.to_string()),
        ("/pane/id", environment.pane_id.to_string()),
        ("/terminal/id", environment.terminal_id.to_string()),
    ];
    for (pointer, expected) in expected {
        if target.pointer(pointer).and_then(serde_json::Value::as_str) != Some(expected.as_str()) {
            return Err(CliError::new(
                "invalid_context",
                "Fut context IDs do not describe one live resource ancestry",
            )
            .into());
        }
    }
    Ok(())
}

fn discover_target(snapshot: &ResourceSnapshot, id: Uuid) -> Result<serde_json::Value> {
    let mut matches = Vec::new();
    for session in &snapshot.sessions {
        let session_json = json!({
            "id": session.id,
            "name": session.name,
            "closing": session.closing,
        });
        if uuid_matches(session.id, id) {
            matches.push(json!({ "kind": "session", "session": session_json }));
        }
        for workspace in &session.workspaces {
            let workspace_json = json!({
                "id": workspace.id,
                "name": workspace.name,
                "root": workspace.root,
                "closing": workspace.closing,
            });
            if uuid_matches(workspace.id, id) {
                matches.push(json!({
                    "kind": "workspace",
                    "session": session_json,
                    "workspace": workspace_json,
                }));
            }
            for tab in &workspace.tabs {
                let tab_json = json!({
                    "id": tab.id,
                    "name": tab.name,
                    "closing": tab.closing,
                });
                if uuid_matches(tab.id, id) {
                    matches.push(json!({
                        "kind": "tab",
                        "session": session_json,
                        "workspace": workspace_json,
                        "tab": tab_json,
                    }));
                }
                for pane in &tab.panes {
                    let pane_json = json!({
                        "id": pane.id,
                        "closing": pane.closing,
                        "activity": pane.activity,
                    });
                    let terminal_json = json!({ "id": pane.terminal_id });
                    if uuid_matches(pane.id, id) {
                        matches.push(json!({
                            "kind": "pane",
                            "session": session_json,
                            "workspace": workspace_json,
                            "tab": tab_json,
                            "pane": pane_json,
                            "terminal": terminal_json,
                        }));
                    }
                    if uuid_matches(pane.terminal_id, id) {
                        matches.push(json!({
                            "kind": "terminal",
                            "session": session_json,
                            "workspace": workspace_json,
                            "tab": tab_json,
                            "pane": pane_json,
                            "terminal": terminal_json,
                        }));
                    }
                }
            }
        }
    }

    match matches.len() {
        0 => Err(CliError::new("not_found", format!("resource {id} not found")).into()),
        1 => Ok(matches.pop().expect("one discovery match")),
        _ => Err(CliError::new(
            "ambiguous_target",
            format!("UUID {id} identifies more than one resource"),
        )
        .into()),
    }
}

fn uuid_matches(id: impl std::fmt::Display, expected: Uuid) -> bool {
    id.to_string().parse::<Uuid>().ok() == Some(expected)
}

fn render_discovered_target(revision: u64, target: &serde_json::Value) -> String {
    let field = |pointer| {
        target
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-")
    };
    format!(
        "revision={revision} kind={} session={} workspace={} tab={} pane={} terminal={} activity={}",
        field("/kind"),
        field("/session/id"),
        field("/workspace/id"),
        field("/tab/id"),
        field("/pane/id"),
        field("/terminal/id"),
        field("/pane/activity/state"),
    )
}

fn render_tabs(revision: u64, tabs: &[TabSnapshot]) -> String {
    let mut lines = vec![format!("revision={revision}")];
    for tab in tabs {
        lines.push(format!(
            "tab {} {:?}{} layout={}",
            tab.id,
            tab.name,
            if tab.closing { " closing" } else { "" },
            render_layout(&tab.layout)
        ));
        for pane in &tab.panes {
            lines.push(format!(
                "  pane {} terminal={}{}",
                pane.id,
                pane.terminal_id,
                if pane.closing { " closing" } else { "" }
            ));
        }
    }
    lines.join("\n")
}

fn render_layout(layout: &SplitTree) -> String {
    match layout {
        SplitTree::Leaf { pane_id } => pane_id.to_string(),
        SplitTree::Branch {
            axis,
            first_basis_points,
            first,
            second,
        } => format!(
            "{}({first_basis_points},{},{})",
            match axis {
                SplitAxis::Horizontal => "horizontal",
                SplitAxis::Vertical => "vertical",
            },
            render_layout(first),
            render_layout(second)
        ),
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
) -> Result<Uuid> {
    let request_id = Uuid::new_v4();
    framed
        .send(Bytes::from(encode_payload(&Envelope {
            request_id: Some(request_id),
            message,
        })?))
        .await?;
    Ok(request_id)
}

async fn receive(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    expected_request_id: Uuid,
    duration: Duration,
    timeout_message: &'static str,
) -> Result<ServerMessage> {
    let frame = tokio::time::timeout(duration, framed.next())
        .await
        .context(timeout_message)?
        .context("daemon disconnected")??;
    correlated_message(
        decode_payload::<Envelope<ServerMessage>>(&frame)?,
        expected_request_id,
    )
}

fn correlated_message(
    envelope: Envelope<ServerMessage>,
    expected_request_id: Uuid,
) -> Result<ServerMessage> {
    if envelope.request_id != Some(expected_request_id) {
        bail!(
            "daemon response request ID {:?} did not match expected request ID {expected_request_id}",
            envelope.request_id
        );
    }
    Ok(envelope.message)
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
    async fn shutdown_reconnects_with_fut_0_1_daemon_protocol() {
        assert_shutdown_reconnects_with(PROTOCOL_VERSION_0_1).await;
    }

    async fn assert_shutdown_reconnects_with(compatible_version: u16) {
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
                            server: compatible_version,
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
                ClientMessage::Hello {
                    version,
                    ..
                } if version == compatible_version
            ));
            framed
                .send(Bytes::from(
                    encode_payload(&Envelope {
                        request_id: second.request_id,
                        message: ServerMessage::Welcome {
                            version: compatible_version,
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
    fn handshake_and_command_responses_require_the_exact_request_id() {
        let expected = Uuid::new_v4();
        let messages = [
            ServerMessage::IncompatibleProtocol {
                client: PROTOCOL_VERSION,
                server: PROTOCOL_VERSION_0_1,
            },
            ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                server_version: "test".into(),
                selected: None,
            },
            ServerMessage::CommandCompleted {
                command: AcknowledgedCommand::Shutdown,
            },
        ];

        for message in messages {
            for request_id in [None, Some(Uuid::new_v4())] {
                let error = correlated_message(
                    Envelope {
                        request_id,
                        message: message.clone(),
                    },
                    expected,
                )
                .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("did not match expected request ID")
                );
            }
        }
    }

    #[test]
    fn shutdown_downgrade_is_restricted_to_known_compatible_protocols() {
        assert_eq!(
            shutdown_downgrade_version(PROTOCOL_VERSION_0_1),
            Some(PROTOCOL_VERSION_0_1)
        );
        assert_eq!(shutdown_downgrade_version(1), None);
        assert_eq!(shutdown_downgrade_version(2), None);
        assert_eq!(shutdown_downgrade_version(3), None);
        assert_eq!(shutdown_downgrade_version(PROTOCOL_VERSION), None);
        assert_eq!(shutdown_downgrade_version(PROTOCOL_VERSION + 1), None);
        assert_eq!(shutdown_downgrade_version(u16::MAX), None);
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
            vec!["fut", "events"],
            vec!["fut", "session", "attach", "a name"],
            vec!["fut", "session", "rename", &session, "new"],
            vec!["fut", "session", "close", &session],
            vec!["fut", "workspace", "attach", &workspace],
            vec!["fut", "workspace", "rename", &workspace, "new"],
            vec!["fut", "workspace", "close", &workspace],
            vec!["fut", "tab", "new", &workspace],
            vec!["fut", "tab", "list", &workspace],
            vec!["fut", "pane", "list", &tab],
            vec!["fut", "tab", "attach", &tab],
            vec!["fut", "tab", "rename", &tab, "new"],
            vec!["fut", "tab", "close", &tab],
            vec!["fut", "pane", "new", &tab],
            vec!["fut", "pane", "split", &pane, "right"],
            vec!["fut", "pane", "attach", &pane],
            vec!["fut", "pane", "move", &pane, &tab],
            vec!["fut", "pane", "close", &pane],
            vec!["fut", "terminal", "attach", &terminal],
            vec!["fut", "terminal", "send-text", &terminal, "literal"],
            vec!["fut", "terminal", "send-keys", &terminal, "ctrl+c"],
            vec!["fut", "terminal", "run", &terminal, "echo ok"],
            vec!["fut", "terminal", "read", &terminal],
            vec![
                "fut",
                "terminal",
                "read",
                &terminal,
                "--source",
                "recent-unwrapped",
                "--lines",
                "200",
            ],
            vec![
                "fut",
                "terminal",
                "wait-output",
                &terminal,
                "--literal",
                "ready 雪",
                "--timeout",
                "30s",
            ],
            vec!["fut", "agent", "skill"],
            vec!["fut", "agent", "list"],
            vec!["fut", "agent", "get", &terminal],
            vec!["fut", "agent", "prompt", &terminal, "review this"],
            vec![
                "fut",
                "agent",
                "prompt",
                &terminal,
                "review this",
                "--wait",
                "--timeout",
                "2m",
            ],
            vec!["fut", "agent", "wait", &terminal, "--timeout", "30s"],
            vec!["fut", "agent", "read", &terminal],
            vec![
                "fut",
                "agent",
                "report",
                "working",
                "--terminal-id",
                &terminal,
                "--source",
                "codex",
                "--agent-session-id",
                "session",
                "--turn-id",
                "turn",
            ],
            vec!["fut", "context"],
            vec!["fut", "get", &terminal],
            vec!["fut", "daemon", "run"],
            vec!["fut", "daemon", "ping"],
            vec!["fut", "daemon", "shutdown"],
        ] {
            Cli::try_parse_from(args).unwrap();
        }
    }

    #[test]
    fn terminal_output_arguments_require_bounded_lines_matcher_and_duration() {
        let terminal = TerminalId::new().to_string();
        for args in [
            vec![
                "fut",
                "terminal",
                "wait-output",
                &terminal,
                "--timeout",
                "1s",
            ],
            vec![
                "fut",
                "terminal",
                "wait-output",
                &terminal,
                "--literal",
                "x",
                "--regex",
                "x",
                "--timeout",
                "1s",
            ],
            vec![
                "fut",
                "terminal",
                "wait-output",
                &terminal,
                "--literal",
                "x",
                "--timeout",
                "0ms",
            ],
            vec![
                "fut", "terminal", "read", &terminal, "--source", "recent", "--lines", "2001",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        assert_eq!(parse_output_timeout("2m"), Ok(Duration::from_secs(120)));
        assert!(parse_output_timeout("10").is_err());
    }

    #[test]
    fn layout_listings_render_panes_in_leaf_order_with_the_split_tree() {
        use crate::{
            resources::PaneSnapshot,
            splits::{HALF_RATIO, SplitDirection},
        };

        let first = PaneId::new();
        let second = PaneId::new();
        let mut layout = SplitTree::leaf(first);
        assert!(layout.split(first, SplitDirection::Down, second));
        let pane = |id| PaneSnapshot {
            id,
            terminal_id: TerminalId::new(),
            closing: false,
            activity: Default::default(),
        };
        let tab = TabSnapshot {
            id: TabId::new(),
            name: "agent".into(),
            closing: false,
            layout,
            panes: vec![pane(first), pane(second)],
        };

        let rendered = render_tabs(7, std::slice::from_ref(&tab));
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines[0], "revision=7");
        assert_eq!(
            lines[1],
            format!(
                "tab {} \"agent\" layout=vertical({HALF_RATIO},{first},{second})",
                tab.id
            )
        );
        assert!(lines[2].starts_with(&format!("  pane {first} terminal=")));
        assert!(lines[3].starts_with(&format!("  pane {second} terminal=")));

        assert_eq!(render_tabs(3, &[]), "revision=3");
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
        let pane = PaneId::new().to_string();
        assert!(Cli::try_parse_from(["fut", "pane", "split", &pane, "right", "echo"]).is_err());
        let cli = Cli::try_parse_from([
            "fut", "pane", "split", &pane, "down", "--", "echo", "--flag",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Some(Command::Pane { command: PaneCommand::Split { command, .. } }) if command == ["echo", "--flag"])
        );
        assert!(Cli::try_parse_from(["fut", "daemon", "run", "echo"]).is_err());
    }

    #[test]
    fn logical_keys_validate_and_match_live_keyboard_encoding() {
        let encoded = ["é", "enter", "ctrl+c", "alt+left", "shift+tab", "f12"]
            .into_iter()
            .map(|key| key.parse::<LogicalKey>().unwrap().bytes)
            .collect::<Vec<_>>();
        assert_eq!(
            encoded,
            [
                "é".as_bytes().to_vec(),
                vec![b'\r'],
                vec![3],
                b"\x1b\x1b[D".to_vec(),
                b"\x1b[Z".to_vec(),
                b"\x1b[24~".to_vec(),
            ]
        );
        for invalid in [
            "",
            "no-such-key",
            "ctrl+",
            "hyper+c",
            "f13",
            "ctrl+left",
            "ctrl+é",
            "shift+up",
        ] {
            assert!(
                invalid.parse::<LogicalKey>().is_err(),
                "accepted {invalid:?}"
            );
        }

        let terminal = TerminalId::new().to_string();
        assert!(
            Cli::try_parse_from([
                "fut",
                "terminal",
                "send-keys",
                &terminal,
                "enter",
                "no-such-key",
            ])
            .is_err()
        );
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
                "agent",
                "context",
                "get",
                "list",
                "events",
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
