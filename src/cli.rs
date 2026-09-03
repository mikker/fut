use std::{
    ffi::OsString,
    fs,
    io::{self, BufRead, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    process::ExitCode,
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::{
    Arg, ArgAction, ArgGroup, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    ValueHint,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::UnixStream;
use tokio_util::codec::Framed;
use uuid::Uuid;

mod completion;

// Bundled at build time so the printed skill always matches this binary's release.
const AGENT_SKILL: &str = include_str!("../skills/fut/SKILL.md");
const BUILD_VERSION: &str = if cfg!(debug_assertions) {
    concat!(env!("CARGO_PKG_VERSION"), "-dev")
} else {
    env!("CARGO_PKG_VERSION")
};

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
        AgentActivity, AgentReport, AgentReportMetadata, AgentState, AttentionKind,
        MAX_AGENT_METADATA_VALUE_BYTES, MAX_TERMINAL_OUTPUT_ROWS, PaneId, SessionId, TabId,
        TerminalId, TerminalOutput, TerminalOutputMatcher, TerminalOutputSource, WorkspaceId,
    },
    protocol::{
        AcknowledgedCommand, AgentPromptMode, ClientMessage, ClientMode, ContextScope,
        ContextualCommand, Envelope, ExtensionCapabilityDeclaration, ExtensionCatalog,
        ExtensionDeclaration, PROTOCOL_VERSION, PresentationTokenPublishAction, RenameSelector,
        ServerMessage, TerminalContext, TerminalInputOperation, codec, decode_payload,
        encode_payload,
    },
    resources::{
        PanePathRef, PaneSnapshot, PresentationTokenTarget, ResourceSnapshot, SessionSelector,
        TabSnapshot, TargetSelector,
    },
    splits::{SplitAxis, SplitDirection, SplitTree},
};

#[derive(Parser)]
#[command(
    name = "fut",
    version = BUILD_VERSION,
    about = "A project-oriented terminal multiplexer",
    after_help = "Enable shell completion with, for example: source <(COMPLETE=zsh fut)"
)]
pub struct Cli {
    /// Override the Unix socket used to contact the daemon.
    #[arg(long, global = true, value_hint = ValueHint::FilePath)]
    socket: Option<PathBuf>,
    /// Read config.toml from this directory instead of the standard location.
    #[arg(long, global = true, value_hint = ValueHint::DirPath, conflicts_with = "no_config")]
    config_dir: Option<PathBuf>,
    /// Use built-in defaults without loading configuration files.
    #[arg(long, global = true)]
    no_config: bool,
    /// Emit versioned JSON for noninteractive commands only.
    #[arg(long, global = true)]
    json: bool,
    /// Launch the standalone UI playground without starting or contacting a daemon.
    #[arg(long)]
    ui_playground: bool,
    /// Command to run; omit it to open the current directory and attach.
    #[command(subcommand)]
    command: Option<Command>,
}

impl Cli {
    fn config_location(&self) -> Result<client::config::ConfigLocation> {
        if self.no_config {
            Ok(client::config::ConfigLocation::disabled())
        } else {
            client::config::resolve_location(self.config_dir.as_deref())
        }
    }
}

fn cli_command() -> clap::Command {
    Cli::command().disable_version_flag(true).arg(
        Arg::new("version")
            .short('v')
            .short_alias('V')
            .long("version")
            .action(ArgAction::Version)
            .help("Print version"),
    )
}

fn try_parse_cli_from<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = cli_command().try_get_matches_from(args)?;
    Cli::from_arg_matches(&matches)
}

#[derive(Subcommand)]
enum Command {
    /// Attach to an existing daemon with the global navigator open.
    #[command(alias = "a")]
    Attach,
    /// Open a location and attach to it.
    #[command(alias = "o")]
    Open {
        /// Directory to open; defaults to the current directory.
        #[arg(value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: Option<PathBuf>,
        /// Open a project from the configured project catalog.
        #[arg(
            short = 'p',
            long,
            value_name = "NAME",
            add = ArgValueCompleter::new(completion::project)
        )]
        project: Option<String>,
        /// Name for the new session or workspace created for this location.
        #[arg(long)]
        name: Option<String>,
        /// Open in the background without attaching.
        #[arg(short = 'b', long)]
        background: bool,
        /// Child program and its direct argv, following `--`; defaults to the shell.
        #[arg(last = true, value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,
    },
    /// Initialize, list, approve, or revoke project recipes.
    Project {
        /// Project operation to perform.
        #[command(subcommand)]
        command: ProjectCommand,
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
    /// Inspect, manage, validate, or reload extension packages.
    Extension {
        /// Extension operation to perform.
        #[command(subcommand)]
        command: ExtensionCommand,
    },
    /// Publish a declared extension presentation token.
    Token {
        /// Token operation to perform.
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Resolve the current terminal ancestry from Fut's environment.
    Context,
    /// Look up any Fut resource by its ID.
    Get {
        /// Session, workspace, tab, pane, or terminal compact ID or UUID.
        #[arg(add = ArgValueCompleter::new(completion::get))]
        id: String,
    },
    /// List resources from the existing daemon.
    #[command(alias = "ls")]
    List {
        /// Include resource IDs, roots, terminals, and the snapshot revision.
        #[arg(short, long)]
        verbose: bool,
    },
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
enum ProjectCommand {
    /// Create .fut/project.toml in the current directory.
    Init,
    /// List configured projects.
    #[command(alias = "ls")]
    List,
    /// Approve the exact current repository recipe after validating it.
    Trust {
        /// Configured project name.
        #[arg(add = ArgValueCompleter::new(completion::project))]
        name: String,
    },
    /// Revoke this machine's approval of a repository recipe.
    Untrust {
        /// Configured project name.
        #[arg(add = ArgValueCompleter::new(completion::project))]
        name: String,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Attach to a session that contains exactly one open terminal.
    Attach {
        /// Session compact ID, UUID, or name; an ID-shaped value is always treated as an ID.
        #[arg(add = ArgValueCompleter::new(completion::session_attach))]
        session: String,
    },
    /// Rename a session by ID, or infer the caller's session.
    Rename {
        /// New name, or a session ID when followed by NAME.
        #[arg(add = ArgValueCompleter::new(completion::session_rename))]
        session_or_name: String,
        /// New session name when SESSION_ID is provided.
        name: Option<String>,
    },
    /// Close a session by ID, or infer the caller's session.
    Close {
        /// Compact session ID or UUID.
        #[arg(add = ArgValueCompleter::new(completion::session_close))]
        session_id: Option<SessionId>,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Attach to a workspace on the existing daemon.
    Attach {
        /// Compact workspace ID or UUID; the workspace must contain exactly one open terminal.
        #[arg(add = ArgValueCompleter::new(completion::workspace_attach))]
        workspace_id: WorkspaceId,
    },
    /// Rename a workspace by ID, or infer the caller's workspace.
    Rename {
        /// New name, or a workspace ID when followed by NAME.
        #[arg(add = ArgValueCompleter::new(completion::workspace_rename))]
        workspace_or_name: String,
        /// New workspace name when WORKSPACE_ID is provided.
        name: Option<String>,
    },
    /// Close a workspace by ID, or infer the caller's workspace.
    Close {
        /// Compact workspace ID or UUID.
        #[arg(add = ArgValueCompleter::new(completion::workspace_close))]
        workspace_id: Option<WorkspaceId>,
    },
    /// Acknowledge this workspace's retirement before terminating its terminals.
    Retire {
        /// Compact workspace ID or UUID; defaults to the caller's workspace.
        #[arg(add = ArgValueCompleter::new(completion::workspace_close))]
        workspace_id: Option<WorkspaceId>,
    },
}

#[derive(Subcommand)]
enum TabCommand {
    /// Create a tab through an existing daemon without attaching.
    New {
        /// Compact owner workspace ID or UUID; defaults to the caller's workspace.
        #[arg(add = ArgValueCompleter::new(completion::tab_new))]
        workspace_id: Option<WorkspaceId>,
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
        /// Compact owner workspace ID or UUID; defaults to the caller's workspace.
        #[arg(add = ArgValueCompleter::new(completion::tab_new))]
        workspace_id: Option<WorkspaceId>,
    },
    /// Attach to a tab on the existing daemon.
    Attach {
        /// Compact tab ID or UUID; the tab must contain exactly one open terminal.
        #[arg(add = ArgValueCompleter::new(completion::tab_attach))]
        tab_id: TabId,
    },
    /// Rename a tab by ID, or infer the caller's tab.
    Rename {
        /// New name, or a tab ID when followed by NAME.
        #[arg(add = ArgValueCompleter::new(completion::tab_rename))]
        tab_or_name: String,
        /// New tab name when TAB_ID is provided.
        name: Option<String>,
    },
    /// Close a tab by ID, or infer the caller's tab.
    Close {
        /// Compact tab ID or UUID.
        #[arg(add = ArgValueCompleter::new(completion::tab_close))]
        tab_id: Option<TabId>,
    },
}

#[derive(Subcommand)]
enum PaneCommand {
    /// Create a pane through an existing daemon without attaching.
    New {
        /// Compact owner tab ID or UUID; defaults to the caller's tab.
        #[arg(add = ArgValueCompleter::new(completion::pane_new))]
        tab_id: Option<TabId>,
        /// Working directory for the child; defaults to the workspace root.
        #[arg(long, value_hint = ValueHint::DirPath)]
        cwd: Option<PathBuf>,
        /// Child program and its direct argv, following `--`; defaults to the shell.
        #[arg(last = true, value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,
    },
    /// Split a pane through an existing daemon without attaching.
    Split {
        /// Direction, or a pane ID when followed by DIRECTION.
        #[arg(add = ArgValueCompleter::new(completion::pane_split_anchor_or_direction))]
        pane_or_direction: String,
        /// Direction when PANE_ID is provided.
        #[arg(value_enum, add = ArgValueCompleter::new(completion::pane_split_direction))]
        direction: Option<PaneSplitDirection>,
        /// Working directory for the child; defaults to the anchor pane's directory.
        #[arg(long, value_hint = ValueHint::DirPath)]
        cwd: Option<PathBuf>,
        /// Child program and its direct argv, following `--`; defaults to the shell.
        #[arg(last = true, value_hint = ValueHint::CommandWithArguments)]
        command: Vec<String>,
    },
    /// List the panes of a tab, including the tab's split layout.
    List {
        /// Compact owner tab ID or UUID; defaults to the caller's tab.
        #[arg(add = ArgValueCompleter::new(completion::pane_new))]
        tab_id: Option<TabId>,
    },
    /// Attach to a pane on the existing daemon.
    Attach {
        /// Compact pane ID or UUID identifying one terminal placement.
        #[arg(add = ArgValueCompleter::new(completion::pane_attach))]
        pane_id: PaneId,
    },
    /// Move a pane to another tab in the same workspace.
    Move {
        /// Destination tab ID, or source pane ID when followed by DESTINATION_TAB_ID.
        #[arg(add = ArgValueCompleter::new(completion::pane_move_source))]
        pane_or_destination_id: String,
        /// Destination tab ID when PANE_ID is provided.
        #[arg(add = ArgValueCompleter::new(completion::pane_move_destination))]
        destination_tab_id: Option<TabId>,
    },
    /// Close a pane by ID, or infer the caller's pane.
    Close {
        /// Compact pane ID or UUID.
        #[arg(add = ArgValueCompleter::new(completion::pane_close))]
        pane_id: Option<PaneId>,
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
        /// Compact terminal ID or UUID identifying one process-bearing terminal.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
    },
    /// Send literal text without submitting it.
    SendText {
        /// Compact terminal ID or UUID of the terminal that will receive the text.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
        /// Literal Unicode text, encoded using the terminal's current paste mode.
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Send validated logical keys or control chords.
    SendKeys {
        /// Compact terminal ID or UUID of the terminal that will receive the keys.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
        /// Named key, one character, or chord such as ctrl+c or alt+left.
        #[arg(required = true, num_args = 1..)]
        keys: Vec<LogicalKey>,
    },
    /// Send literal command text and Enter as one atomic operation.
    Run {
        /// Compact terminal ID or UUID of the terminal that will receive the command.
        #[arg(add = ArgValueCompleter::new(completion::terminal_attach))]
        terminal_id: TerminalId,
        /// Literal command text to submit.
        #[arg(allow_hyphen_values = true)]
        command: String,
    },
    /// Read a bounded terminal output snapshot.
    Read {
        /// Compact terminal ID or UUID of the terminal to inspect.
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
        /// Compact terminal ID or UUID of the terminal to observe.
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
        /// Compact terminal ID or UUID; defaults to FUT_TERMINAL_ID inside Fut.
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
    event: crate::domain::TerminalKeyEvent,
}

impl FromStr for LogicalKey {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let event = logical_key_event(value)?;
        let event = client::input::terminal_key_event(event)
            .ok_or_else(|| format!("unsupported logical key: {value}"))?;
        Ok(Self { event })
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
    code.map(|code| KeyEvent::new(code, parsed_modifiers))
        .ok_or_else(|| format!("unknown logical key `{key}` in `{value}`"))
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
        /// Compact ID or UUID of the integrated terminal.
        #[arg(add = ArgValueCompleter::new(completion::agent))]
        terminal_id: TerminalId,
    },
    /// Submit one prompt as literal text followed atomically by Enter.
    Prompt {
        /// Compact ID or UUID of the integrated terminal.
        #[arg(add = ArgValueCompleter::new(completion::agent))]
        terminal_id: TerminalId,
        /// Literal Unicode prompt text.
        #[arg(allow_hyphen_values = true, required_unless_present = "stdin")]
        text: Option<String>,
        /// Read literal Unicode prompt text from standard input.
        #[arg(long, conflicts_with = "text")]
        stdin: bool,
        /// Wait for a fresh working transition and subsequent settled report.
        #[arg(long)]
        wait: bool,
        /// Required with --wait; such as 500ms, 30s, or 2m.
        #[arg(long, value_parser = parse_output_timeout)]
        timeout: Option<Duration>,
    },
    /// Wait for a currently working agent to settle, or return current settled state.
    Wait {
        /// Compact ID or UUID of the integrated terminal.
        #[arg(add = ArgValueCompleter::new(completion::agent))]
        terminal_id: TerminalId,
        /// Required deadline, such as 500ms, 30s, or 2m.
        #[arg(long, value_parser = parse_output_timeout)]
        timeout: Duration,
    },
    /// Read bounded output together with current agent availability.
    Read {
        /// Compact ID or UUID of the integrated terminal.
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
        /// Compact terminal ID or UUID; defaults to FUT_TERMINAL_ID inside Fut.
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
    /// Receive a native coding-agent notification.
    Notify {
        /// Coding-agent notification format.
        #[arg(value_enum)]
        source: AgentNotificationSource,
        /// JSON notification payload supplied by the coding agent.
        payload: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum AgentNotificationSource {
    Codex,
}

#[derive(Clone, Copy, ValueEnum)]
enum AgentReportArg {
    Idle,
    Working,
    Blocked,
    Completed,
    Exited,
}

impl From<AgentReportArg> for AgentReport {
    fn from(value: AgentReportArg) -> Self {
        match value {
            AgentReportArg::Idle => Self::Idle,
            AgentReportArg::Working => Self::Working,
            AgentReportArg::Blocked => Self::Blocked,
            AgentReportArg::Completed => Self::Completed,
            AgentReportArg::Exited => Self::Exited,
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
    Shutdown {
        /// Terminate the socket-owning daemon without using its protocol.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ExtensionCommand {
    /// List the daemon's active extension generation.
    List,
    /// Show one active extension from the daemon's catalog.
    Show {
        /// Active extension ID.
        #[arg(add = ArgValueCompleter::new(completion::extension))]
        id: String,
    },
    /// Validate a package directory without activating or executing it.
    Validate {
        /// Extension package directory containing fut-extension.toml.
        #[arg(value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: PathBuf,
    },
    /// Copy a trusted local package into Fut's managed store, initially disabled.
    Install {
        /// Local extension package directory containing fut-extension.toml.
        #[arg(value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: PathBuf,
    },
    /// Fetch and install one exact commit from an explicit Git remote.
    InstallGit {
        /// HTTPS or absolute file URL of the Git repository.
        #[arg(value_name = "URL")]
        url: String,
        /// Exact full commit SHA; branches, tags, and HEAD are rejected.
        #[arg(long, value_name = "COMMIT")]
        rev: String,
        /// Expected SHA-256 of Fut's normalized installed package content.
        #[arg(long, value_name = "DIGEST")]
        sha256: Option<String>,
    },
    /// Replace a Git-installed extension with a newly supplied exact commit.
    Update {
        /// Installed Git extension ID.
        id: String,
        /// New exact full commit SHA; branches, tags, and HEAD are rejected.
        #[arg(long, value_name = "COMMIT")]
        rev: String,
        /// Expected SHA-256 of Fut's normalized installed package content.
        #[arg(long, value_name = "DIGEST")]
        sha256: Option<String>,
    },
    /// Include an installed managed extension in future loads.
    Enable {
        /// Installed managed extension ID.
        id: String,
    },
    /// Exclude an installed managed extension from future loads.
    Disable {
        /// Installed managed extension ID.
        id: String,
    },
    /// Remove a disabled package from Fut's managed store.
    Remove {
        /// Installed managed extension ID.
        id: String,
    },
    /// Ask the daemon to atomically reload its configured extensions.
    Reload,
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Materialize unstyled presentation text on one live resource.
    Publish(TokenPublishArgs),
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .multiple(false)
        .args(["session_id", "workspace_id", "tab_id", "pane_id"])
))]
struct TokenPublishArgs {
    /// Configured extension ID.
    extension: String,
    /// Manifest presentation-token declaration name (not its qualified name).
    token: String,
    /// Plain UTF-8 presentation text, at most 1 KiB.
    #[arg(allow_hyphen_values = true)]
    value: String,
    /// Publish to this session.
    #[arg(long)]
    session_id: Option<SessionId>,
    /// Publish to this workspace.
    #[arg(long)]
    workspace_id: Option<WorkspaceId>,
    /// Publish to this tab.
    #[arg(long)]
    tab_id: Option<TabId>,
    /// Publish to this pane.
    #[arg(long)]
    pane_id: Option<PaneId>,
    /// Make the token navigate to this live descendant pane when activated.
    #[arg(long, conflicts_with = "action_command")]
    action_pane_id: Option<PaneId>,
    /// Make the token run this command from the publishing extension.
    #[arg(long, conflicts_with = "action_pane_id")]
    action_command: Option<String>,
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
    let cli = match try_parse_cli_from(&args) {
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
    if matches!(cli.command, Some(Command::Doctor)) && !cli.ui_playground {
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
        let config_location = match cli.config_location() {
            Ok(location) => location,
            Err(error) => {
                if json_output {
                    render_json_error("command_failed", format!("{error:#}"));
                } else {
                    eprintln!("Error: {error:#}");
                }
                return ExitCode::FAILURE;
            }
        };
        let report = crate::doctor::run(&socket, &config_location).await;
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
    if cli.ui_playground {
        if cli.command.is_some() {
            bail!("--ui-playground cannot be combined with a command");
        }
        reject_interactive_json(&cli)?;
        return client::launch_ui_playground(&cli.config_location()?).await;
    }

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

    if let Some(Command::Agent {
        command: AgentCommand::Notify { source, payload },
    }) = &cli.command
    {
        return receive_agent_notification(cli.socket.as_deref(), *source, payload).await;
    }

    if let Some(Command::Extension { command }) = &cli.command {
        match command {
            ExtensionCommand::Validate { path } => {
                return validate_extension_package(path, cli.json);
            }
            ExtensionCommand::Install { path } => {
                return install_extension_package(path, cli.json);
            }
            ExtensionCommand::InstallGit { url, rev, sha256 } => {
                return install_git_extension_package(url, rev, sha256.as_deref(), cli.json).await;
            }
            ExtensionCommand::Update { id, rev, sha256 } => {
                return update_git_extension_package(id, rev, sha256.as_deref(), cli.json).await;
            }
            ExtensionCommand::Enable { id } => {
                return mutate_managed_extension(id, true, cli.json);
            }
            ExtensionCommand::Disable { id } => {
                return mutate_managed_extension(id, false, cli.json);
            }
            ExtensionCommand::Remove { id } => {
                return remove_managed_extension(id, cli.json);
            }
            ExtensionCommand::List | ExtensionCommand::Show { .. } | ExtensionCommand::Reload => {}
        }
    }

    reject_interactive_json(&cli)?;
    let config_location = cli.config_location()?;
    let socket = socket_path(cli.socket.as_deref())?;
    reject_nested_client(&cli)?;
    match cli.command {
        None => {
            let current_dir = std::env::current_dir().context("read current directory")?;
            let (cwd, configured_project) =
                resolve_project_open(None, None, &current_dir, &config_location).await?;
            if let Some((name, configured)) = configured_project.as_ref() {
                confirm_project_recipe(name, configured, &config_location, cli.json)?;
            }
            open_and_attach(&socket, cwd, &config_location).await
        }
        Some(Command::Attach) => client::attach_navigator(&socket, &config_location).await,
        Some(Command::Open {
            path,
            project,
            name,
            background,
            command,
        }) => {
            let current_dir = std::env::current_dir()?;
            let (cwd, configured_project) =
                resolve_project_open(path, project.as_deref(), &current_dir, &config_location)
                    .await?;
            if let Some((name, configured)) = configured_project.as_ref() {
                confirm_project_recipe(name, configured, &config_location, cli.json)?;
            }
            let (program, argv) = child_command(command);
            let ui = if background {
                None
            } else {
                Some(client::stage_ui_config(&config_location)?)
            };
            let (selected, disposition) = open_location_with_config(
                &socket,
                ClientMessage::OpenLocation {
                    project,
                    name,
                    cwd: cwd.clone(),
                    program,
                    argv,
                },
                &cwd,
                &config_location,
            )
            .await?;
            notify_command_activation(selected.pane_id)?;
            match ui {
                Some(ui) => {
                    client::attach_with_ui(
                        &socket,
                        Some(TargetSelector::Terminal(selected.terminal_id)),
                        ui,
                        &config_location,
                    )
                    .await
                }
                None => output(
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
            }
        }
        Some(Command::Project { command }) => {
            run_project_command(&config_location, cli.json, command)
        }
        Some(Command::Session {
            command: SessionCommand::Attach { session },
        }) => {
            client::attach(
                &socket,
                Some(TargetSelector::Session(session_selector(&session))),
                &config_location,
            )
            .await
        }
        Some(Command::Workspace {
            command: WorkspaceCommand::Attach { workspace_id },
        }) => {
            client::attach(
                &socket,
                Some(TargetSelector::Workspace(workspace_id)),
                &config_location,
            )
            .await
        }
        Some(Command::Tab {
            command: TabCommand::Attach { tab_id },
        }) => client::attach(&socket, Some(TargetSelector::Tab(tab_id)), &config_location).await,
        Some(Command::Pane {
            command: PaneCommand::Attach { pane_id },
        }) => {
            client::attach(
                &socket,
                Some(TargetSelector::Pane(pane_id)),
                &config_location,
            )
            .await
        }
        Some(Command::Pane {
            command:
                PaneCommand::New {
                    tab_id,
                    cwd,
                    command,
                },
        }) => {
            let (program, argv) = child_command(command);
            let message = match tab_id {
                Some(tab_id) => ClientMessage::CreatePane {
                    tab_id,
                    cwd,
                    program,
                    argv,
                },
                None => {
                    let context = live_context(&socket).await?;
                    ClientMessage::Contextual {
                        context,
                        command: ContextualCommand::CreatePane { cwd, program, argv },
                    }
                }
            };
            match control(&socket, message).await? {
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
                    pane_or_direction,
                    direction,
                    cwd,
                    command,
                },
        }) => {
            let (program, argv) = child_command(command);
            let (pane_id, direction, message) = match direction {
                Some(direction) => {
                    let pane_id = parse_id::<PaneId>(&pane_or_direction, "pane")?;
                    let direction = SplitDirection::from(direction);
                    (
                        pane_id,
                        direction,
                        ClientMessage::SplitPane {
                            pane_id,
                            direction,
                            cwd,
                            program,
                            argv,
                        },
                    )
                }
                None => {
                    let direction = PaneSplitDirection::from_str(&pane_or_direction, true)
                        .map(SplitDirection::from)
                        .map_err(|_| {
                            CliError::new(
                                "invalid_arguments",
                                "inferred pane split requires `right` or `down`; explicit form is `pane split PANE_ID DIRECTION`",
                            )
                        })?;
                    let context = live_context(&socket).await?;
                    (
                        context.pane_id,
                        direction,
                        ClientMessage::Contextual {
                            context,
                            command: ContextualCommand::SplitPane {
                                direction,
                                cwd,
                                program,
                                argv,
                            },
                        },
                    )
                }
            };
            let direction_name = match direction {
                SplitDirection::Right => "right",
                SplitDirection::Down => "down",
            };
            match control(&socket, message).await? {
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
                    pane_or_destination_id,
                    destination_tab_id,
                },
        }) => {
            let message = match destination_tab_id {
                Some(destination_tab_id) => {
                    let pane_id = parse_id::<PaneId>(&pane_or_destination_id, "pane")?;
                    ClientMessage::MovePane {
                        pane_id,
                        destination_tab_id,
                    }
                }
                None => {
                    let destination_tab_id =
                        parse_id::<TabId>(&pane_or_destination_id, "destination tab")?;
                    let context = live_context(&socket).await?;
                    ClientMessage::Contextual {
                        context,
                        command: ContextualCommand::MovePane { destination_tab_id },
                    }
                }
            };
            match control(&socket, message).await? {
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
            }
        }
        Some(Command::Agent {
            command: AgentCommand::List,
        }) => {
            let snapshot = list_resources(&socket).await?;
            let agents = integrated_agents(&snapshot).collect::<Vec<_>>();
            let unread_count = agents.iter().filter(|agent| agent.unread).count();
            let human = agents
                .iter()
                .map(render_agent)
                .collect::<Vec<_>>()
                .join("\n");
            output(
                cli.json,
                "agent.list",
                AgentListWire {
                    agents,
                    revision: snapshot.revision,
                    unread_count,
                },
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
                AgentGetWire {
                    agent,
                    revision: snapshot.revision,
                },
                render_agent(&agent),
            )
        }
        Some(Command::Agent {
            command:
                AgentCommand::Prompt {
                    terminal_id,
                    text,
                    stdin,
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
            let text = if stdin {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .context("failed to read agent prompt from standard input")?;
                text
            } else {
                text.expect("clap requires prompt text unless --stdin is present")
            };
            let mode = match timeout {
                Some(timeout) => AgentPromptMode::Wait {
                    timeout_ms: timeout.as_millis() as u64,
                },
                None => AgentPromptMode::Submit,
            };
            match control(
                &socket,
                ClientMessage::PromptAgent {
                    terminal_id,
                    text,
                    mode,
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
                AgentReadWire {
                    agent,
                    output: captured,
                    revision: snapshot.revision,
                },
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
        }) => {
            client::attach(
                &socket,
                Some(TargetSelector::Terminal(terminal_id)),
                &config_location,
            )
            .await
        }
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
            let events = keys.into_iter().map(|key| key.event).collect::<Vec<_>>();
            terminal_input(
                &socket,
                ClientMessage::TerminalInput {
                    terminal_id,
                    operation: TerminalInputOperation::Keys { events },
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
        Some(Command::Token {
            command: TokenCommand::Publish(args),
        }) => {
            let target = match (
                args.session_id,
                args.workspace_id,
                args.tab_id,
                args.pane_id,
            ) {
                (Some(id), None, None, None) => PresentationTokenTarget::Session(id),
                (None, Some(id), None, None) => PresentationTokenTarget::Workspace(id),
                (None, None, Some(id), None) => PresentationTokenTarget::Tab(id),
                (None, None, None, Some(id)) => PresentationTokenTarget::Pane(id),
                _ => unreachable!("clap requires exactly one token target"),
            };
            let action = match (args.action_pane_id, args.action_command) {
                (Some(pane_id), None) => Some(PresentationTokenPublishAction::Pane { pane_id }),
                (None, Some(command)) => {
                    Some(PresentationTokenPublishAction::ExtensionCommand { command })
                }
                (None, None) => None,
                (Some(_), Some(_)) => unreachable!("clap requires at most one token action"),
            };
            let published_action = action.clone();
            match control(
                &socket,
                ClientMessage::PublishToken {
                    extension_id: args.extension.clone(),
                    token: args.token.clone(),
                    value: args.value,
                    target,
                    action,
                },
            )
            .await?
            {
                ServerMessage::TokenPublished {
                    resource_revision,
                    changed,
                } => output(
                    cli.json,
                    "token.publish",
                    json!({
                        "extension": args.extension,
                        "token": args.token,
                        "target": target,
                        "action": published_action,
                        "revision": resource_revision,
                        "changed": changed,
                    }),
                    format!("revision={resource_revision} changed={changed}"),
                ),
                other => unexpected(other),
            }
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
            let message = match workspace_id {
                Some(workspace_id) => ClientMessage::CreateTab {
                    workspace_id,
                    name,
                    cwd,
                    program,
                    argv,
                },
                None => ClientMessage::Contextual {
                    context: live_context(&socket).await?,
                    command: ContextualCommand::CreateTab {
                        name,
                        cwd,
                        program,
                        argv,
                    },
                },
            };
            match control(&socket, message).await? {
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
            let terminal_id = terminal_context_id()?;
            let snapshot = list_resources(&socket).await?;
            let context = context_for_terminal(&snapshot, terminal_id)?;
            let target = discover_target(&snapshot, context.terminal_id.uuid())?;
            output(
                cli.json,
                "context",
                json!({ "revision": snapshot.revision, "target": target }),
                render_discovered_target(snapshot.revision, &target),
            )
        }
        Some(Command::Get { id }) => {
            let snapshot = list_resources(&socket).await?;
            let id = crate::domain::parse_id(&id).map_err(|_| {
                CliError::new(
                    "invalid_arguments",
                    "resource ID must be a compact Fut ID or UUID",
                )
            })?;
            let target = discover_target(&snapshot, id)?;
            output(
                cli.json,
                "get",
                json!({ "revision": snapshot.revision, "target": target }),
                render_discovered_target(snapshot.revision, &target),
            )
        }
        Some(Command::List { verbose }) => {
            let snapshot = list_resources(&socket).await?;
            if cli.json {
                output(true, "list", &snapshot, String::new())
            } else {
                println!(
                    "{}",
                    if verbose {
                        render_verbose_resources(&snapshot)
                    } else {
                        render_resource_tree(&snapshot)
                    }
                );
                Ok(())
            }
        }
        Some(Command::Extension {
            command: ExtensionCommand::List,
        }) => {
            let catalog = extension_catalog(&socket).await?;
            output(
                cli.json,
                "extension.list",
                extension_catalog_result(&catalog),
                render_extension_list(&catalog),
            )
        }
        Some(Command::Extension {
            command: ExtensionCommand::Show { id },
        }) => {
            let catalog = extension_catalog(&socket).await?;
            let extension = catalog
                .extensions
                .iter()
                .find(|extension| extension.id == id)
                .ok_or_else(|| {
                    CliError::new("not_found", format!("active extension {id:?} not found"))
                })?;
            let defaults = catalog
                .config
                .defaults
                .get(&id)
                .cloned()
                .unwrap_or_default();
            output(
                cli.json,
                "extension.show",
                json!({
                    "generation": catalog.generation,
                    "fingerprint": catalog.fingerprint,
                    "extension": extension,
                    "config": {
                        "defaults": defaults,
                        "source": catalog.config.source,
                    },
                }),
                render_extension_show(&catalog, extension, &defaults),
            )
        }
        Some(Command::Extension {
            command: ExtensionCommand::Reload,
        }) => match control(&socket, ClientMessage::ReloadExtensions).await? {
            ServerMessage::ExtensionsReloaded { catalog, changed } => output(
                cli.json,
                "extension.reload",
                json!({
                    "changed": changed,
                    "catalog": extension_catalog_result(&catalog),
                }),
                format!(
                    "generation={} fingerprint={} extensions={} changed={changed}",
                    catalog.generation,
                    catalog.fingerprint,
                    catalog.extensions.len(),
                ),
            ),
            other => unexpected(other),
        },
        Some(Command::Tab {
            command: TabCommand::List { workspace_id },
        }) => {
            let snapshot = list_resources(&socket).await?;
            let workspace_id = match workspace_id {
                Some(workspace_id) => workspace_id,
                None => context_from_snapshot(&snapshot)?.workspace_id,
            };
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
            let tab_id = match tab_id {
                Some(tab_id) => tab_id,
                None => context_from_snapshot(&snapshot)?.tab_id,
            };
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
            let mut config = DaemonConfig::shell(socket, cwd, config_location);
            if let Some(program) = command.first() {
                config.spawn.program = program.into();
                config.spawn.argv = command[1..].to_vec();
                config.recipe_command_override = true;
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
            command: DaemonCommand::Shutdown { force },
        }) => {
            if force {
                force_shutdown(&socket).await?;
            } else {
                shutdown_control(&socket).await?;
            }
            output(
                cli.json,
                "daemon.shutdown",
                json!({ "shutdown": true }),
                "shutdown=true",
            )
        }
        Some(Command::Workspace {
            command: WorkspaceCommand::Retire { workspace_id },
        }) => retire_workspace(&socket, cli.json, workspace_id).await,
        Some(Command::Doctor) => unreachable!("doctor is handled before command execution"),
        Some(Command::Extension {
            command:
                ExtensionCommand::Validate { .. }
                | ExtensionCommand::Install { .. }
                | ExtensionCommand::InstallGit { .. }
                | ExtensionCommand::Update { .. }
                | ExtensionCommand::Enable { .. }
                | ExtensionCommand::Disable { .. }
                | ExtensionCommand::Remove { .. },
        }) => unreachable!("daemonless extension command was handled before daemon setup"),
        Some(Command::Agent {
            command: AgentCommand::Skill,
        }) => unreachable!("agent skill is handled before daemon setup"),
        Some(Command::Agent {
            command: AgentCommand::Notify { .. },
        }) => unreachable!("agent notifications are handled before daemon setup"),
        Some(command) => run_mutation(&socket, cli.json, command).await,
    }
}

fn run_project_command(
    config_location: &client::config::ConfigLocation,
    json_output: bool,
    command: ProjectCommand,
) -> Result<()> {
    if matches!(command, ProjectCommand::Init) {
        return init_project(json_output);
    }
    let catalog = client::config::load_projects_location(config_location)?;
    if matches!(command, ProjectCommand::List) {
        let projects = catalog
            .iter()
            .map(|(name, project)| {
                json!({
                    "name": name,
                    "path": project.path(),
                    "recipe": project.recipe(),
                })
            })
            .collect::<Vec<_>>();
        let human = catalog
            .iter()
            .map(|(name, project)| format!("{name}\t{}", project.path().display()))
            .collect::<Vec<_>>()
            .join("\n");
        return output(
            json_output,
            "project.list",
            json!({ "projects": projects }),
            human,
        );
    }

    let (name, trust, command_name) = match command {
        ProjectCommand::Trust { name } => (name, true, "project.trust"),
        ProjectCommand::Untrust { name } => (name, false, "project.untrust"),
        ProjectCommand::Init | ProjectCommand::List => {
            unreachable!("daemonless project command returned above")
        }
    };
    let project = catalog_project(&catalog, &name)?;
    let change = if trust {
        let loaded = client::config::load_extensions_location(config_location)?;
        crate::project_definition::trust(project, &loaded.extensions)
    } else {
        crate::project_definition::untrust(&name, project)
    }
    .map_err(|error| match error {
        error @ crate::project_definition::ProjectDefinitionError::InherentlyTrusted { .. } => {
            anyhow::Error::new(CliError::new("inherently_trusted", error.to_string()))
        }
        error => anyhow::Error::new(error),
    })?;
    let trust_source = if change.inherently_trusted {
        "global_config"
    } else {
        "machine_state"
    };
    output(
        json_output,
        command_name,
        json!({
            "name": name,
            "recipe": change.source,
            "sha256": change.digest,
            "trusted": change.trusted,
            "changed": change.changed,
            "inherently_trusted": change.inherently_trusted,
        }),
        format!(
            "project={name} trusted={} changed={} source={trust_source} recipe={}",
            change.trusted,
            change.changed,
            change.source.display()
        ),
    )
}

const PROJECT_RECIPE_TEMPLATE: &str = r#"#:schema https://fut.sh/schemas/project.json
# Project recipe documentation: https://fut.sh/projects/

[[workspaces]]

[[workspaces.tabs]]
title = "agent"
panes = [{ command = ["codex"] }]

[[workspaces.tabs]]
title = "vim"
panes = [{ command = ["vim"] }]
"#;

fn init_project(json_output: bool) -> Result<()> {
    let root = std::env::current_dir().context("read current directory")?;
    let directory = root.join(".fut");
    let path = directory.join("project.toml");
    std::fs::create_dir_all(&directory).with_context(|| {
        format!(
            "create project configuration directory {}",
            directory.display()
        )
    })?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::Error::new(CliError::new(
                    "already_exists",
                    format!("project recipe already exists at {}", path.display()),
                ))
            } else {
                anyhow::Error::new(error)
                    .context(format!("create project recipe {}", path.display()))
            }
        })?;
    if let Err(error) = file
        .write_all(PROJECT_RECIPE_TEMPLATE.as_bytes())
        .with_context(|| format!("write project recipe {}", path.display()))
    {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    output(
        json_output,
        "project.init",
        json!({ "path": path }),
        format!("created={}", path.display()),
    )
}

fn validate_extension_package(path: &std::path::Path, json_output: bool) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .context("read current directory for extension validation")?
            .join(path)
    };
    let extension = crate::extensions::validate_package(&absolute).map_err(|error| {
        CliError::new(
            "invalid_extension",
            format!("extension package is invalid: {error:#}"),
        )
    })?;
    output(
        json_output,
        "extension.validate",
        json!({ "valid": true, "extension": extension }),
        format!("valid {}", render_extension_summary(&extension)),
    )
}

const EXTENSION_TRUST: &str = "trusted_local_code";

fn install_extension_package(path: &std::path::Path, json_output: bool) -> Result<()> {
    let change = crate::extension_store::install(path).map_err(|error| {
        CliError::new(
            "extension_install_failed",
            format!("could not install extension package: {error:#}"),
        )
    })?;
    let extension = &change.extension;
    output(
        json_output,
        "extension.install",
        json!({
            "extension": extension,
            "changed": change.changed,
            "scripts_executed": false,
            "trust": EXTENSION_TRUST,
            "reload_required": extension.enabled,
        }),
        format!(
            "installed extension {} version={} enabled={} changed={} sha256={} source={:?} path={:?}\nNo package scripts were executed. Installed extensions are trusted local code; review this package before enabling it.",
            extension.id,
            extension.version,
            extension.enabled,
            change.changed,
            extension.content_sha256,
            extension.source,
            extension.install_path,
        ),
    )
}

async fn install_git_extension_package(
    url: &str,
    revision: &str,
    expected_digest: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let change = crate::extension_store::install_git(url, revision, expected_digest)
        .await
        .map_err(|error| {
            CliError::new(
                "extension_git_install_failed",
                format!("could not install extension from Git: {error:#}"),
            )
        })?;
    let extension = &change.extension;
    let (remote_url, commit) = git_extension_provenance(extension)
        .expect("a successful Git install records Git provenance");
    output(
        json_output,
        "extension.install-git",
        json!({
            "extension": extension,
            "changed": change.changed,
            "scripts_executed": false,
            "trust": EXTENSION_TRUST,
            "reload_required": extension.enabled,
        }),
        format!(
            "installed Git extension {} version={} enabled={} changed={} sha256={} remote={remote_url:?} commit={commit} path={:?}\nNo hooks, submodules, LFS filters, package scripts, or build scripts were executed. Installed extensions are trusted local code; review this package before enabling it.",
            extension.id,
            extension.version,
            extension.enabled,
            change.changed,
            extension.content_sha256,
            extension.install_path,
        ),
    )
}

async fn update_git_extension_package(
    id: &str,
    revision: &str,
    expected_digest: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let update = crate::extension_store::update_git(id, revision, expected_digest)
        .await
        .map_err(|error| {
            CliError::new(
                "extension_git_update_failed",
                format!("could not update extension from Git: {error:#}"),
            )
        })?;
    let previous = &update.previous;
    let extension = &update.current.extension;
    let (remote_url, previous_commit) =
        git_extension_provenance(previous).expect("Git updates start from Git provenance");
    let (_, commit) =
        git_extension_provenance(extension).expect("Git updates retain Git provenance");
    output(
        json_output,
        "extension.update",
        json!({
            "previous": previous,
            "extension": extension,
            "changed": update.current.changed,
            "scripts_executed": false,
            "trust": EXTENSION_TRUST,
            "reload_required": extension.enabled,
        }),
        format!(
            "updated Git extension {} version={}->{} enabled={} changed={} sha256={}->{} remote={remote_url:?} commit={previous_commit}->{commit}\nNo hooks, submodules, LFS filters, package scripts, or build scripts were executed.{}",
            extension.id,
            previous.version,
            extension.version,
            extension.enabled,
            update.current.changed,
            previous.content_sha256,
            extension.content_sha256,
            if extension.enabled {
                " Run `fut extension reload` to activate the updated package in a running daemon."
            } else {
                ""
            },
        ),
    )
}

fn git_extension_provenance(
    extension: &crate::extension_store::ManagedExtension,
) -> Option<(&str, &str)> {
    match extension.provenance.as_ref()? {
        crate::extension_store::ExtensionProvenance::Git { remote_url, commit } => {
            Some((remote_url, commit))
        }
    }
}

fn mutate_managed_extension(id: &str, enabled: bool, json_output: bool) -> Result<()> {
    let operation = if enabled { "enable" } else { "disable" };
    let change = if enabled {
        crate::extension_store::enable(id)
    } else {
        crate::extension_store::disable(id)
    }
    .map_err(|error| managed_extension_error(operation, error))?;
    let extension = &change.extension;
    let human = if enabled {
        format!(
            "enabled extension {} version={} changed={} path={:?} reload_required=true\nTrust boundary: Fut may execute this package's declared commands and hooks with your user permissions. Run `fut extension reload` to update a running daemon.",
            extension.id, extension.version, change.changed, extension.install_path,
        )
    } else {
        format!(
            "disabled extension {} version={} changed={} reload_required=true\nThe current daemon may retain its active copy until `fut extension reload` or restart.",
            extension.id, extension.version, change.changed,
        )
    };
    output(
        json_output,
        if enabled {
            "extension.enable"
        } else {
            "extension.disable"
        },
        json!({
            "extension": extension,
            "changed": change.changed,
            "scripts_executed": false,
            "trust": EXTENSION_TRUST,
            "reload_required": true,
        }),
        human,
    )
}

fn remove_managed_extension(id: &str, json_output: bool) -> Result<()> {
    let change = crate::extension_store::remove(id)
        .map_err(|error| managed_extension_error("remove", error))?;
    let extension = &change.extension;
    output(
        json_output,
        "extension.remove",
        json!({
            "extension": extension,
            "changed": change.changed,
            "scripts_executed": false,
            "reload_required": true,
        }),
        format!(
            "removed extension {} version={} changed={} reload_required=true\nA running daemon may retain its prior catalog until `fut extension reload` or restart.",
            extension.id, extension.version, change.changed,
        ),
    )
}

fn managed_extension_error(
    operation: &str,
    error: crate::extension_store::StoreMutationError,
) -> anyhow::Error {
    match error {
        crate::extension_store::StoreMutationError::NotFound { id } => CliError::new(
            "not_found",
            format!("managed extension {id:?} is not installed"),
        )
        .into(),
        crate::extension_store::StoreMutationError::Enabled { id } => CliError::new(
            "extension_enabled",
            format!("managed extension {id:?} is enabled; disable it before removing it"),
        )
        .into(),
        crate::extension_store::StoreMutationError::Failed(error) => CliError::new(
            "extension_store_failed",
            format!("could not {operation} managed extension: {error:#}"),
        )
        .into(),
    }
}

fn extension_catalog_result(catalog: &ExtensionCatalog) -> serde_json::Value {
    json!({
        "generation": catalog.generation,
        "fingerprint": catalog.fingerprint,
        "count": catalog.extensions.len(),
        "extensions": catalog.extensions,
        "config": catalog.config,
    })
}

fn render_extension_list(catalog: &ExtensionCatalog) -> String {
    std::iter::once(format!(
        "generation={} fingerprint={} extensions={}",
        catalog.generation,
        catalog.fingerprint,
        catalog.extensions.len(),
    ))
    .chain(catalog.extensions.iter().map(render_extension_summary))
    .collect::<Vec<_>>()
    .join("\n")
}

fn render_extension_show(
    catalog: &ExtensionCatalog,
    extension: &ExtensionDeclaration,
    defaults: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let commands = extension
        .commands
        .iter()
        .map(|(name, declaration)| {
            let mode = match declaration.execution {
                crate::protocol::ExtensionCommandExecutionDeclaration::Interactive { .. } => {
                    "interactive"
                }
                crate::protocol::ExtensionCommandExecutionDeclaration::Background => "background",
            };
            format!("{name} ({mode})")
        })
        .collect::<Vec<_>>();
    let tokens = extension
        .presentation_tokens
        .iter()
        .map(|token| {
            let scope = match token.scope {
                crate::protocol::ExtensionPresentationScope::Session => "session",
                crate::protocol::ExtensionPresentationScope::Workspace => "workspace",
                crate::protocol::ExtensionPresentationScope::Tab => "tab",
                crate::protocol::ExtensionPresentationScope::Pane => "pane",
            };
            let presentation = match token.presentation {
                crate::protocol::ExtensionTokenPresentation::Plain => "plain",
                crate::protocol::ExtensionTokenPresentation::Spinner => "spinner",
                crate::protocol::ExtensionTokenPresentation::Pulse => "pulse",
                crate::protocol::ExtensionTokenPresentation::Wave => "wave",
            };
            format!("{} ({scope}, {presentation})", token.name)
        })
        .collect::<Vec<_>>();
    let source = catalog
        .config
        .source
        .as_ref()
        .map_or_else(|| "-".into(), |path| format!("{path:?}"));
    format!(
        "extension {}\n  generation: {}\n  fingerprint: {}\n  version: {}\n  api_version: {}\n  fut: {}\n  capabilities: {}\n  root: {:?}\n  manifest: {:?}\n  hooks: {}\n  commands: {}\n  presentation_tokens: {}\n  config_source: {}\n  config_defaults: {}",
        extension.id,
        catalog.generation,
        catalog.fingerprint,
        extension.version,
        extension.api_version,
        extension.fut,
        extension_capabilities(extension),
        extension.root,
        extension.root.join(crate::extensions::MANIFEST_FILE_NAME),
        joined_or_dash(extension.hooks.keys().cloned()),
        joined_or_dash(commands),
        joined_or_dash(tokens),
        source,
        serde_json::Value::Object(defaults.clone()),
    )
}

fn render_extension_summary(extension: &ExtensionDeclaration) -> String {
    format!(
        "extension {} version={} api={} fut={} capabilities={} root={:?}",
        extension.id,
        extension.version,
        extension.api_version,
        extension.fut,
        extension_capabilities(extension),
        extension.root,
    )
}

fn extension_capabilities(extension: &ExtensionDeclaration) -> String {
    joined_or_dash(
        extension
            .capabilities
            .iter()
            .map(|capability| match capability {
                ExtensionCapabilityDeclaration::Commands => "commands",
                ExtensionCapabilityDeclaration::Hooks => "hooks",
                ExtensionCapabilityDeclaration::PresentationTokens => "presentation_tokens",
            }),
    )
}

fn joined_or_dash(values: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let values = values
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect::<Vec<_>>();
    if values.is_empty() {
        "-".into()
    } else {
        values.join(", ")
    }
}

async fn resolve_project_open(
    path: Option<PathBuf>,
    project: Option<&str>,
    current_dir: &std::path::Path,
    config_location: &client::config::ConfigLocation,
) -> Result<(PathBuf, Option<(String, client::config::ProjectConfig)>)> {
    let catalog = client::config::load_projects_location(config_location)?;
    if let Some(project_name) = project {
        let configured = catalog_project(&catalog, project_name)?.clone();
        let cwd = path.map_or_else(
            || configured.path().to_owned(),
            |path| resolve_open_path(Some(path), current_dir),
        );
        return Ok((cwd, Some((project_name.to_owned(), configured))));
    }

    let cwd = resolve_open_path(path, current_dir);
    let resolver = crate::project::ProjectResolver::default();
    let requested = resolver
        .resolve(&cwd)
        .await
        .with_context(|| format!("resolve project path {}", cwd.display()))?;
    let mut matched = None;
    for (name, configured) in catalog.iter() {
        let Ok(candidate) = resolver.resolve(configured.path()).await else {
            continue;
        };
        if candidate.project != requested.project {
            continue;
        }
        if matched.is_some() {
            bail!(
                "project identity for {} matches multiple configured projects, including {name:?}",
                requested.cwd.display()
            );
        }
        matched = Some((name.to_owned(), configured.clone()));
    }
    Ok((cwd, matched))
}

fn catalog_project<'a>(
    catalog: &'a client::config::ProjectCatalog,
    name: &str,
) -> Result<&'a client::config::ProjectConfig> {
    catalog.get(name).ok_or_else(|| {
        let available = catalog
            .iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
            .join(", ");
        let message = if available.is_empty() {
            format!("unknown project {name:?}; no projects are configured")
        } else {
            format!("unknown project {name:?}; configured projects: {available}")
        };
        anyhow::Error::new(CliError::new("unknown_project", message))
    })
}

fn confirm_project_recipe(
    name: &str,
    project: &client::config::ProjectConfig,
    config_location: &client::config::ConfigLocation,
    json_output: bool,
) -> Result<()> {
    let loaded = client::config::load_extensions_location(config_location)?;
    let error = match crate::project_definition::load(Some(name), project, &loaded.extensions) {
        Ok(_) => return Ok(()),
        Err(error) => error,
    };
    let crate::project_definition::ProjectDefinitionError::UntrustedRecipe { path, digest, .. } =
        &error
    else {
        return Err(error.into());
    };
    if json_output {
        return Err(error.into());
    }

    eprintln!(
        "Project {name:?} contains an untrusted recipe:\n  {}\n  SHA-256 {digest}",
        path.display()
    );
    let mut stdin = std::io::stdin().lock();
    loop {
        eprint!("Trust this recipe and continue? [y/N/i] ");
        std::io::stderr().flush().context("flush trust prompt")?;
        let mut answer = String::new();
        stdin
            .read_line(&mut answer)
            .context("read trust response")?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => break,
            "i" | "inspect" => {
                let source = std::fs::read_to_string(path)
                    .with_context(|| format!("read project recipe {}", path.display()))?;
                eprintln!("\n{source}");
            }
            _ => bail!("project recipe was not trusted"),
        }
    }

    crate::project_definition::trust_digest(project, &loaded.extensions, digest)?;
    eprintln!("Trusted project recipe for {name:?}.");
    Ok(())
}

fn notify_command_activation(pane_id: PaneId) -> anyhow::Result<()> {
    let Some(path) = std::env::var_os(crate::command::ACTIVATE_OPENED_SOCKET_ENV) else {
        return Ok(());
    };
    let socket =
        std::os::unix::net::UnixDatagram::unbound().context("create command activation socket")?;
    socket
        .send_to(pane_id.to_string().as_bytes(), &path)
        .with_context(|| {
            format!(
                "notify parent Fut client through activation socket {}",
                PathBuf::from(path).display()
            )
        })?;
    Ok(())
}

fn resolve_open_path(path: Option<PathBuf>, current_dir: &std::path::Path) -> PathBuf {
    match path {
        Some(path) if path.is_relative() => current_dir.join(path),
        Some(path) => path,
        None => current_dir.to_owned(),
    }
}

/// Keep the control connection open until the acceptance response has been
/// rendered. The daemon deliberately waits for its EOF before it starts
/// terminating the workspace that may contain this CLI process.
async fn retire_workspace(
    socket: &std::path::Path,
    json_output: bool,
    workspace_id: Option<WorkspaceId>,
) -> Result<()> {
    let (workspace_id, context) = match workspace_id {
        Some(workspace_id) => (workspace_id, None),
        None => {
            let context = live_context(socket).await?;
            (context.workspace_id, Some(context))
        }
    };
    let mut framed = connected_control(socket).await?;
    let request_id = send(
        &mut framed,
        ClientMessage::RetireWorkspace {
            workspace_id,
            context,
        },
    )
    .await?;
    let response = receive(
        &mut framed,
        request_id,
        Duration::from_secs(15),
        "daemon response timed out",
    )
    .await?;
    response_ok(response, AcknowledgedCommand::RetireWorkspace)?;
    output(
        json_output,
        "workspace.retire",
        json!({ "workspace_id": workspace_id, "retiring": true }),
        "retiring=true",
    )?;
    std::io::stdout()
        .flush()
        .context("flush retirement response")?;
    drop(framed);
    Ok(())
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

async fn receive_agent_notification(
    socket_override: Option<&std::path::Path>,
    source: AgentNotificationSource,
    payload: &str,
) -> Result<()> {
    let Some(terminal_id) = std::env::var("FUT_TERMINAL_ID")
        .ok()
        .and_then(|value| value.parse().ok())
    else {
        return Ok(());
    };
    if socket_override.is_none() && std::env::var_os("FUT_SOCKET").is_none() {
        return Ok(());
    }

    let metadata = match source {
        AgentNotificationSource::Codex => codex_completion_metadata(payload),
    };
    let Some(metadata) = metadata else {
        return Ok(());
    };
    let Ok(socket) = socket_path(socket_override) else {
        return Ok(());
    };

    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        control(
            &socket,
            ClientMessage::ReportAgent {
                terminal_id,
                report: AgentReport::Completed,
                metadata,
            },
        ),
    )
    .await;
    Ok(())
}

fn codex_completion_metadata(payload: &str) -> Option<AgentReportMetadata> {
    let event = serde_json::from_str::<CodexNotification>(payload).ok()?;
    (event.kind == "agent-turn-complete").then(|| AgentReportMetadata {
        source: Some("codex".into()),
        agent_session_id: bounded_notification_string(event.thread_id),
        turn_id: bounded_notification_string(event.turn_id),
    })
}

#[derive(Deserialize)]
struct CodexNotification {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "thread-id")]
    thread_id: Option<String>,
    #[serde(rename = "turn-id")]
    turn_id: Option<String>,
}

fn bounded_notification_string(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty() && value.len() <= MAX_AGENT_METADATA_VALUE_BYTES)
}

#[derive(Clone, Copy, Debug, Serialize)]
struct AgentWire<'a> {
    activity: &'a AgentActivity,
    available: bool,
    pane_id: PaneId,
    session: AgentSessionWire<'a>,
    tab: AgentTabWire<'a>,
    terminal_id: TerminalId,
    unread: bool,
    workspace: AgentWorkspaceWire<'a>,
}

impl<'a> From<PanePathRef<'a>> for AgentWire<'a> {
    fn from(path: PanePathRef<'a>) -> Self {
        Self {
            activity: &path.pane.activity,
            available: !path.session.closing
                && !path.workspace.closing
                && !path.tab.closing
                && !path.pane.closing
                && path.pane.activity.state != AgentState::Working,
            pane_id: path.pane.id,
            session: AgentSessionWire {
                id: path.session.id,
                name: &path.session.name,
            },
            tab: AgentTabWire {
                id: path.tab.id,
                name: &path.tab.name,
            },
            terminal_id: path.pane.terminal_id,
            unread: path.pane.activity.has_unread_attention(),
            workspace: AgentWorkspaceWire {
                id: path.workspace.id,
                name: &path.workspace.name,
                root: &path.workspace.root,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
struct AgentSessionWire<'a> {
    id: SessionId,
    name: &'a str,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct AgentTabWire<'a> {
    id: TabId,
    name: &'a str,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct AgentWorkspaceWire<'a> {
    id: WorkspaceId,
    name: &'a str,
    root: &'a std::path::Path,
}

#[derive(Debug, Serialize)]
struct AgentListWire<'a> {
    agents: Vec<AgentWire<'a>>,
    revision: u64,
    unread_count: usize,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct AgentGetWire<'a> {
    agent: AgentWire<'a>,
    revision: u64,
}

#[derive(Debug, Serialize)]
struct AgentReadWire<'a> {
    agent: AgentWire<'a>,
    output: TerminalOutput,
    revision: u64,
}

fn integrated_agents(snapshot: &ResourceSnapshot) -> impl Iterator<Item = AgentWire<'_>> {
    snapshot
        .pane_paths()
        .filter(|path| path.pane.activity.has_active_integration())
        .map(Into::into)
}

fn resolve_agent<'a>(
    snapshot: &'a ResourceSnapshot,
    terminal_id: TerminalId,
) -> Result<AgentWire<'a>> {
    let Some(path) = snapshot
        .pane_paths()
        .find(|path| path.pane.terminal_id == terminal_id)
    else {
        return Err(
            CliError::new("not_found", format!("terminal {terminal_id} was not found")).into(),
        );
    };
    if !path.pane.activity.has_active_integration() {
        Err(CliError::new(
            "not_an_agent",
            format!("terminal {terminal_id} has no agent integration"),
        )
        .into())
    } else {
        Ok(path.into())
    }
}

fn agent_state_name(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
    }
}

fn render_agent(agent: &AgentWire<'_>) -> String {
    format!(
        "agent={} state={} available={} unread={} session={} workspace={} tab={} pane={}",
        agent.terminal_id,
        agent_state_name(agent.activity.state),
        agent.available,
        agent.unread,
        agent.session.id,
        agent.workspace.id,
        agent.tab.id,
        agent.pane_id,
    )
}

fn reject_interactive_json(cli: &Cli) -> Result<()> {
    if !cli.json {
        return Ok(());
    }
    let interactive = attaches_client(&cli.command)
        || matches!(
            cli.command,
            Some(Command::Daemon {
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

fn attaches_client(command: &Option<Command>) -> bool {
    command.is_none()
        || matches!(command, Some(Command::Attach))
        || matches!(
            command,
            Some(Command::Open {
                background: false,
                ..
            })
        )
        || matches!(
            command,
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
            })
        )
}

fn reject_nested_client(cli: &Cli) -> Result<()> {
    if attaches_client(&cli.command)
        && std::env::var_os("FUT_TERMINAL_ID").is_some()
        && std::env::var_os("FUT_ALLOW_NESTED").is_none()
    {
        return Err(CliError::new(
            "nested_client",
            "clients should be nested with care, set $FUT_ALLOW_NESTED to force",
        )
        .into());
    }
    Ok(())
}

async fn run_mutation(socket: &std::path::Path, json_output: bool, command: Command) -> Result<()> {
    let (message, acknowledged, command_name, result, human) = match command {
        Command::Session {
            command:
                SessionCommand::Rename {
                    session_or_name,
                    name,
                },
        } => {
            let (target, name) = rename_arguments::<SessionId>(session_or_name, name, "session")?;
            let (session_id, message) = match target {
                Some(session_id) => (
                    session_id,
                    ClientMessage::RenameTarget {
                        selector: RenameSelector::Session(SessionSelector::Id(session_id)),
                        name: name.clone(),
                    },
                ),
                None => {
                    let context = live_context(socket).await?;
                    (
                        context.session_id,
                        ClientMessage::Contextual {
                            context,
                            command: ContextualCommand::Rename {
                                scope: ContextScope::Session,
                                name: name.clone(),
                            },
                        },
                    )
                }
            };
            (
                message,
                AcknowledgedCommand::RenameTarget,
                "session.rename",
                json!({"session_id": session_id, "name": name}),
                "renamed=true",
            )
        }
        Command::Session {
            command: SessionCommand::Close { session_id },
        } => {
            let (session_id, message) = match session_id {
                Some(session_id) => (
                    session_id,
                    ClientMessage::CloseTarget {
                        selector: TargetSelector::Session(SessionSelector::Id(session_id)),
                    },
                ),
                None => {
                    let context = live_context(socket).await?;
                    (
                        context.session_id,
                        ClientMessage::Contextual {
                            context,
                            command: ContextualCommand::Close {
                                scope: ContextScope::Session,
                            },
                        },
                    )
                }
            };
            (
                message,
                AcknowledgedCommand::CloseTarget,
                "session.close",
                json!({"session_id": session_id}),
                "closed=true",
            )
        }
        Command::Workspace {
            command:
                WorkspaceCommand::Rename {
                    workspace_or_name,
                    name,
                },
        } => {
            let (target, name) =
                rename_arguments::<WorkspaceId>(workspace_or_name, name, "workspace")?;
            let (workspace_id, message) = match target {
                Some(workspace_id) => (
                    workspace_id,
                    ClientMessage::RenameTarget {
                        selector: RenameSelector::Workspace(workspace_id),
                        name: name.clone(),
                    },
                ),
                None => {
                    let context = live_context(socket).await?;
                    (
                        context.workspace_id,
                        ClientMessage::Contextual {
                            context,
                            command: ContextualCommand::Rename {
                                scope: ContextScope::Workspace,
                                name: name.clone(),
                            },
                        },
                    )
                }
            };
            (
                message,
                AcknowledgedCommand::RenameTarget,
                "workspace.rename",
                json!({"workspace_id": workspace_id, "name": name}),
                "renamed=true",
            )
        }
        Command::Workspace {
            command: WorkspaceCommand::Close { workspace_id },
        } => {
            let (workspace_id, message) = match workspace_id {
                Some(workspace_id) => (
                    workspace_id,
                    ClientMessage::CloseTarget {
                        selector: TargetSelector::Workspace(workspace_id),
                    },
                ),
                None => {
                    let context = live_context(socket).await?;
                    (
                        context.workspace_id,
                        ClientMessage::Contextual {
                            context,
                            command: ContextualCommand::Close {
                                scope: ContextScope::Workspace,
                            },
                        },
                    )
                }
            };
            (
                message,
                AcknowledgedCommand::CloseTarget,
                "workspace.close",
                json!({"workspace_id": workspace_id}),
                "closed=true",
            )
        }
        Command::Tab {
            command: TabCommand::Rename { tab_or_name, name },
        } => {
            let (target, name) = rename_arguments::<TabId>(tab_or_name, name, "tab")?;
            let (tab_id, message) = match target {
                Some(tab_id) => (
                    tab_id,
                    ClientMessage::RenameTarget {
                        selector: RenameSelector::Tab(tab_id),
                        name: name.clone(),
                    },
                ),
                None => {
                    let context = live_context(socket).await?;
                    (
                        context.tab_id,
                        ClientMessage::Contextual {
                            context,
                            command: ContextualCommand::Rename {
                                scope: ContextScope::Tab,
                                name: name.clone(),
                            },
                        },
                    )
                }
            };
            (
                message,
                AcknowledgedCommand::RenameTarget,
                "tab.rename",
                json!({"tab_id": tab_id, "name": name}),
                "renamed=true",
            )
        }
        Command::Tab {
            command: TabCommand::Close { tab_id },
        } => {
            let (tab_id, message) = match tab_id {
                Some(tab_id) => (
                    tab_id,
                    ClientMessage::CloseTarget {
                        selector: TargetSelector::Tab(tab_id),
                    },
                ),
                None => {
                    let context = live_context(socket).await?;
                    (
                        context.tab_id,
                        ClientMessage::Contextual {
                            context,
                            command: ContextualCommand::Close {
                                scope: ContextScope::Tab,
                            },
                        },
                    )
                }
            };
            (
                message,
                AcknowledgedCommand::CloseTarget,
                "tab.close",
                json!({"tab_id": tab_id}),
                "closed=true",
            )
        }
        Command::Pane {
            command: PaneCommand::Close { pane_id },
        } => {
            let (pane_id, message) = match pane_id {
                Some(pane_id) => (
                    pane_id,
                    ClientMessage::CloseTarget {
                        selector: TargetSelector::Pane(pane_id),
                    },
                ),
                None => {
                    let context = live_context(socket).await?;
                    (
                        context.pane_id,
                        ClientMessage::Contextual {
                            context,
                            command: ContextualCommand::Close {
                                scope: ContextScope::Pane,
                            },
                        },
                    )
                }
            };
            (
                message,
                AcknowledgedCommand::CloseTarget,
                "pane.close",
                json!({"pane_id": pane_id}),
                "closed=true",
            )
        }
        _ => unreachable!("all non-mutation commands handled by run"),
    };
    response_ok(control(socket, message).await?, acknowledged)?;
    output(json_output, command_name, result, human)
}

#[derive(Debug, Serialize)]
struct OutputEnvelope<T> {
    command: &'static str,
    result: T,
    version: u8,
}

fn output(
    result_as_json: bool,
    command: &'static str,
    result: impl Serialize,
    human: impl AsRef<str>,
) -> Result<()> {
    if result_as_json {
        println!(
            "{}",
            serde_json::to_string(&OutputEnvelope {
                command,
                result,
                version: 1,
            })?
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

async fn open_and_attach(
    socket: &std::path::Path,
    cwd: PathBuf,
    config_location: &client::config::ConfigLocation,
) -> Result<()> {
    let ui = client::stage_ui_config(config_location)?;
    let selected = open_current_location_with_config(socket, &cwd, config_location).await?;
    client::attach_with_ui(
        socket,
        Some(TargetSelector::Terminal(selected.terminal_id)),
        ui,
        config_location,
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
    let config_location = client::config::resolve_location(None)?;
    open_current_location_with_config(socket, cwd, &config_location).await
}

async fn open_current_location_with_config(
    socket: &std::path::Path,
    cwd: &std::path::Path,
    config_location: &client::config::ConfigLocation,
) -> Result<crate::protocol::SelectedTarget> {
    open_location_with_config(
        socket,
        ClientMessage::OpenLocation {
            project: None,
            name: None,
            cwd: cwd.to_owned(),
            program: None,
            argv: vec![],
        },
        cwd,
        config_location,
    )
    .await
    .map(|(selected, _)| selected)
}

async fn open_location_with_config(
    socket: &std::path::Path,
    message: ClientMessage,
    daemon_cwd: &std::path::Path,
    config_location: &client::config::ConfigLocation,
) -> Result<(
    crate::protocol::SelectedTarget,
    crate::protocol::OpenDisposition,
)> {
    const RETRIES: usize = 2;

    ensure_daemon(socket, daemon_cwd, config_location).await?;
    for attempt in 0..=RETRIES {
        let response = control(socket, message.clone()).await;

        match response {
            Ok(ServerMessage::LocationOpened {
                selected,
                disposition,
            }) => return Ok((selected, disposition)),
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
                daemon_cwd.display()
            );
        }
        wait_until_protocol_stops(socket).await;
        ensure_daemon(socket, daemon_cwd, config_location).await?;
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
        ControlHandshake::Incompatible { server, .. } => bail!(
            "daemon at {} uses protocol {server}, but this Fut client requires protocol \
             {PROTOCOL_VERSION}; run `fut --socket {} daemon shutdown --force` to terminate it",
            socket.display(),
            socket.display()
        ),
    }
}

async fn shutdown_control(socket: &std::path::Path) -> Result<()> {
    match control_handshake(socket, PROTOCOL_VERSION).await? {
        ControlHandshake::Connected(framed) => response_ok(
            request(framed, ClientMessage::Shutdown).await?,
            AcknowledgedCommand::Shutdown,
        ),
        ControlHandshake::Incompatible { server, peer } => {
            if let Some(peer) = peer
                && peer.uid == effective_user_id()
                && std::env::var_os("FUT_SHUTDOWN_DELEGATED").is_none()
                && shutdown_with_daemon_binary(socket, peer.pid).await.is_ok()
            {
                return Ok(());
            }
            bail!(
                "daemon at {} uses incompatible protocol {server}; run `fut --socket {} \
                 daemon shutdown --force` to terminate it",
                socket.display(),
                socket.display()
            )
        }
    }
}

enum ControlHandshake {
    Connected(Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>),
    Incompatible {
        server: u16,
        peer: Option<PeerCredentials>,
    },
}

async fn control_handshake(socket: &std::path::Path, version: u16) -> Result<ControlHandshake> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
    let peer = peer_credentials(&stream).ok();
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
            Ok(ControlHandshake::Incompatible { server, peer })
        }
        other => unexpected(other),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PeerCredentials {
    pid: libc::pid_t,
    uid: libc::uid_t,
}

fn effective_user_id() -> libc::uid_t {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
fn peer_credentials(stream: &UnixStream) -> Result<PeerCredentials> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the kernel writes at most `length` bytes to the valid output buffer.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("read daemon peer credentials");
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        bail!("daemon peer credentials had an unexpected size");
    }
    // SAFETY: getsockopt succeeded and filled the complete structure.
    let credentials = unsafe { credentials.assume_init() };
    Ok(PeerCredentials {
        pid: credentials.pid,
        uid: credentials.uid,
    })
}

#[cfg(target_os = "macos")]
fn peer_credentials(stream: &UnixStream) -> Result<PeerCredentials> {
    let descriptor = stream.as_raw_fd();
    let mut pid = 0;
    let mut pid_length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: the kernel writes at most `pid_length` bytes to the valid PID pointer.
    if unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut pid_length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("read daemon peer PID");
    }
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: getpeereid writes to both valid output pointers.
    if unsafe { libc::getpeereid(descriptor, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error()).context("read daemon peer identity");
    }
    Ok(PeerCredentials { pid, uid })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_credentials(_stream: &UnixStream) -> Result<PeerCredentials> {
    bail!("forced daemon shutdown is supported only on Linux and macOS")
}

async fn shutdown_with_daemon_binary(socket: &Path, pid: libc::pid_t) -> Result<()> {
    let executable = daemon_executable(pid).await?;
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(&executable)
            .arg("--socket")
            .arg(socket)
            .args(["daemon", "shutdown"])
            .env("FUT_SHUTDOWN_DELEGATED", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
    .await
    .with_context(|| format!("daemon binary {} timed out", executable.display()))??;
    if !status.success() {
        bail!(
            "daemon binary {} could not shut itself down",
            executable.display()
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn matches(self, metadata: &fs::Metadata) -> bool {
        metadata.dev() == self.device && metadata.ino() == self.inode
    }
}

async fn force_shutdown(socket: &Path) -> Result<()> {
    let identity = owned_socket_identity(socket)?;
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
    let peer = peer_credentials(&stream)?;
    drop(stream);
    if peer.uid != effective_user_id() {
        bail!(
            "refusing to terminate daemon PID {} owned by user {}",
            peer.pid,
            peer.uid
        );
    }
    if peer.pid <= 1 || peer.pid == std::process::id() as libc::pid_t {
        bail!("refusing to terminate invalid daemon PID {}", peer.pid);
    }
    match peer_at_socket(socket, identity).await? {
        Some(confirmed) if confirmed == peer => {}
        Some(confirmed) => bail!(
            "daemon socket {} changed owner from PID {} to PID {}",
            socket.display(),
            peer.pid,
            confirmed.pid
        ),
        None => {
            remove_original_stale_socket(socket, identity)?;
            return Ok(());
        }
    }

    signal_process(peer.pid, libc::SIGTERM)?;
    if !wait_for_socket_peer_exit(socket, identity, peer, Duration::from_secs(2)).await? {
        signal_process(peer.pid, libc::SIGKILL)?;
        if !wait_for_socket_peer_exit(socket, identity, peer, Duration::from_secs(2)).await? {
            bail!("daemon PID {} did not exit after SIGKILL", peer.pid);
        }
    }
    remove_original_stale_socket(socket, identity)?;
    Ok(())
}

fn owned_socket_identity(socket: &Path) -> Result<SocketIdentity> {
    let metadata = fs::symlink_metadata(socket)
        .with_context(|| format!("inspect daemon socket {}", socket.display()))?;
    if !metadata.file_type().is_socket() {
        bail!("refusing to use non-socket path {}", socket.display());
    }
    if metadata.uid() != effective_user_id() {
        bail!(
            "refusing to use daemon socket not owned by the current user: {}",
            socket.display()
        );
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn signal_process(pid: libc::pid_t, signal: libc::c_int) -> Result<()> {
    // SAFETY: kill receives a validated positive PID and a standard signal number.
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("signal daemon PID {pid}"))
}

async fn wait_for_socket_peer_exit(
    socket: &Path,
    identity: SocketIdentity,
    expected: PeerCredentials,
    duration: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < deadline {
        match peer_at_socket(socket, identity).await? {
            None => return Ok(true),
            Some(peer) if peer == expected => tokio::time::sleep(Duration::from_millis(25)).await,
            Some(peer) => bail!(
                "daemon socket {} changed owner from PID {} to PID {}",
                socket.display(),
                expected.pid,
                peer.pid
            ),
        }
    }
    Ok(false)
}

async fn peer_at_socket(
    socket: &Path,
    identity: SocketIdentity,
) -> Result<Option<PeerCredentials>> {
    match fs::symlink_metadata(socket) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect daemon socket {}", socket.display()));
        }
        Ok(metadata) => {
            if !identity.matches(&metadata) {
                bail!("daemon socket {} was replaced", socket.display());
            }
        }
    }
    match UnixStream::connect(socket).await {
        Ok(stream) => peer_credentials(&stream).map(Some),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).with_context(|| format!("connect to {}", socket.display())),
    }
}

fn remove_original_stale_socket(socket: &Path, identity: SocketIdentity) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(socket) else {
        return Ok(());
    };
    if metadata.file_type().is_socket() && identity.matches(&metadata) {
        fs::remove_file(socket)
            .with_context(|| format!("remove stale daemon socket {}", socket.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn daemon_executable(pid: libc::pid_t) -> Result<PathBuf> {
    let executable = PathBuf::from(format!("/proc/{pid}/exe"));
    tokio::fs::metadata(&executable)
        .await
        .with_context(|| format!("resolve executable for daemon PID {pid}"))?;
    Ok(executable)
}

#[cfg(target_os = "macos")]
async fn daemon_executable(pid: libc::pid_t) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: proc_pidpath writes at most buffer.len() bytes into the valid buffer.
    let length = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).expect("process path buffer length fits u32"),
        )
    };
    if length <= 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("resolve executable for daemon PID {pid}"));
    }
    buffer.truncate(length as usize);
    Ok(PathBuf::from(OsString::from_vec(buffer)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn daemon_executable(_pid: libc::pid_t) -> Result<PathBuf> {
    bail!("daemon executable discovery is supported only on Linux and macOS")
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
            mode: AgentPromptMode::Wait { timeout_ms },
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
        ServerMessage::Resources { snapshot, .. } => output(true, "events", &snapshot, "")?,
        other => return unexpected(other),
    }
    while let Some(frame) = framed.next().await {
        let envelope: Envelope<ServerMessage> = decode_payload(&frame?)?;
        match envelope.message {
            ServerMessage::ResourcesChanged { snapshot } => {
                output(true, "events", &snapshot, "")?;
            }
            ServerMessage::PresenceChanged { .. } => {}
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
        ServerMessage::Resources { snapshot, .. } => Ok(snapshot),
        other => unexpected(other),
    }
}

async fn extension_catalog(socket: &std::path::Path) -> Result<ExtensionCatalog> {
    match control(socket, ClientMessage::GetExtensionCatalog).await? {
        ServerMessage::ExtensionCatalog { catalog } => Ok(catalog),
        other => unexpected(other),
    }
}

async fn live_context(socket: &std::path::Path) -> Result<TerminalContext> {
    let terminal_id = terminal_context_id()?;
    let snapshot = list_resources(socket).await?;
    context_for_terminal(&snapshot, terminal_id)
}

fn context_from_snapshot(snapshot: &ResourceSnapshot) -> Result<TerminalContext> {
    context_for_terminal(snapshot, terminal_context_id()?)
}

fn terminal_context_id() -> Result<TerminalId> {
    match std::env::var("FUT_TERMINAL_ID") {
        Ok(value) => value.parse().map_err(|_| {
            CliError::new(
                "invalid_context",
                "FUT_TERMINAL_ID is not a valid compact terminal ID or UUID",
            )
            .into()
        }),
        Err(std::env::VarError::NotPresent) => Err(CliError::new(
            "missing_context",
            "Fut terminal context is unavailable; run inside Fut or pass an explicit ID",
        )
        .into()),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(CliError::new("invalid_context", "FUT_TERMINAL_ID is not valid UTF-8").into())
        }
    }
}

fn context_for_terminal(
    snapshot: &ResourceSnapshot,
    terminal_id: TerminalId,
) -> Result<TerminalContext> {
    let path = snapshot.live_terminal_path(terminal_id).map_err(|error| {
        let (code, message) = match error {
            crate::resources::ResourceError::NotFound(_) => (
                "stale_context",
                format!("FUT_TERMINAL_ID {terminal_id} is not live on this daemon"),
            ),
            crate::resources::ResourceError::Closing(kind) => (
                "closing_context",
                format!("calling Fut terminal has a closing {kind} ancestor"),
            ),
            other => ("invalid_context", other.to_string()),
        };
        CliError::new(code, message)
    })?;
    Ok(TerminalContext {
        session_id: path.session_id,
        workspace_id: path.workspace_id,
        tab_id: path.tab_id,
        pane_id: path.pane_id,
        terminal_id: path.terminal_id,
    })
}

fn parse_id<T>(value: &str, kind: &str) -> Result<T>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| {
        CliError::new(
            "invalid_arguments",
            format!("explicit {kind} target must be a compact Fut ID or UUID"),
        )
        .into()
    })
}

fn rename_arguments<T>(
    target_or_name: String,
    name: Option<String>,
    kind: &str,
) -> Result<(Option<T>, String)>
where
    T: std::str::FromStr,
{
    match name {
        Some(name) => Ok((Some(parse_id::<T>(&target_or_name, kind)?), name)),
        None if target_or_name.parse::<T>().is_ok() => {
            Err(CliError::new("invalid_arguments", format!("missing NAME after {kind} ID")).into())
        }
        None => Ok((None, target_or_name)),
    }
}

fn discover_target(snapshot: &ResourceSnapshot, id: Uuid) -> Result<serde_json::Value> {
    let mut matches = Vec::new();
    for session in &snapshot.sessions {
        let session_json = json!({
            "id": session.id,
            "name": session.name,
            "closing": session.closing,
        });
        if session.id.uuid() == id {
            matches.push(json!({ "kind": "session", "session": session_json }));
        }
        for workspace in &session.workspaces {
            let workspace_json = json!({
                "id": workspace.id,
                "name": workspace.name,
                "root": workspace.root,
                "closing": workspace.closing,
            });
            if workspace.id.uuid() == id {
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
                if tab.id.uuid() == id {
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
                    if pane.id.uuid() == id {
                        matches.push(json!({
                            "kind": "pane",
                            "session": session_json,
                            "workspace": workspace_json,
                            "tab": tab_json,
                            "pane": pane_json,
                            "terminal": terminal_json,
                        }));
                    }
                    if pane.terminal_id.uuid() == id {
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
            format!("ID {id} identifies more than one resource"),
        )
        .into()),
    }
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
            ratio,
            first,
            second,
            ..
        } => format!(
            "{}({ratio},{},{})",
            match axis {
                SplitAxis::Horizontal => "horizontal",
                SplitAxis::Vertical => "vertical",
            },
            render_layout(first),
            render_layout(second)
        ),
    }
}

fn render_resource_tree(snapshot: &ResourceSnapshot) -> String {
    if snapshot.sessions.is_empty() {
        return "No resources".into();
    }

    let mut lines = Vec::new();
    for session in &snapshot.sessions {
        lines.push(resource_line(
            0,
            &session.name,
            session.closing,
            session
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .flat_map(|tab| &tab.panes),
        ));
        for workspace in &session.workspaces {
            let workspace_closing = session.closing || workspace.closing;
            lines.push(resource_line(
                1,
                &workspace.name,
                workspace_closing,
                workspace.tabs.iter().flat_map(|tab| &tab.panes),
            ));
            for (tab_index, tab) in workspace.tabs.iter().enumerate() {
                let label = if tab.name.is_empty() {
                    format!("tab {}", tab_index + 1)
                } else {
                    tab.name.clone()
                };
                let single_pane = match tab.panes.as_slice() {
                    [pane] => Some(pane),
                    _ => None,
                };
                let label = if single_pane.is_some() {
                    format!("{label} · pane")
                } else {
                    label
                };
                let tab_closing = workspace_closing
                    || tab.closing
                    || single_pane.is_some_and(|pane| pane.closing);
                lines.push(resource_line(2, &label, tab_closing, &tab.panes));
                if single_pane.is_none() {
                    for (pane_index, pane) in tab.panes.iter().enumerate() {
                        lines.push(resource_line(
                            3,
                            &format!("pane {}", pane_index + 1),
                            tab_closing || pane.closing,
                            std::iter::once(pane),
                        ));
                    }
                }
            }
        }
    }
    lines.join("\n")
}

fn resource_line<'a>(
    depth: usize,
    label: &str,
    closing: bool,
    panes: impl IntoIterator<Item = &'a PaneSnapshot>,
) -> String {
    let marker = if closing {
        "×"
    } else {
        activity_marker(panes)
    };
    format!("{}{marker} {label}", "  ".repeat(depth))
}

fn activity_marker<'a>(panes: impl IntoIterator<Item = &'a PaneSnapshot>) -> &'static str {
    let mut blocked = false;
    let mut completed = false;
    let mut working = false;
    for pane in panes {
        let attention = pane
            .activity
            .attention()
            .filter(|_| pane.activity.has_unread_attention());
        blocked |= pane.activity.state == AgentState::Blocked
            || attention.is_some_and(|attention| attention.kind == AttentionKind::Blocked);
        completed |= attention.is_some_and(|attention| attention.kind == AttentionKind::Completed);
        working |= pane.activity.state == AgentState::Working;
    }
    if blocked {
        "!"
    } else if completed {
        "•"
    } else if working {
        "⠋"
    } else {
        " "
    }
}

fn render_verbose_resources(snapshot: &ResourceSnapshot) -> String {
    let mut lines = vec![format!("revision={}", snapshot.revision)];
    for session in &snapshot.sessions {
        lines.push(format!(
            "session {} {:?}{}",
            session.id,
            session.name,
            if session.closing { " closing" } else { "" }
        ));
        for workspace in &session.workspaces {
            lines.push(format!(
                "  workspace {} {:?} {}",
                workspace.id,
                workspace.name,
                workspace.root.display()
            ));
            for tab in &workspace.tabs {
                lines.push(format!(
                    "    tab {} {:?}{}",
                    tab.id,
                    tab.name,
                    if tab.closing { " closing" } else { "" }
                ));
                for pane in &tab.panes {
                    lines.push(format!(
                        "      pane {} terminal={}{}",
                        pane.id,
                        pane.terminal_id,
                        if pane.closing { " closing" } else { "" }
                    ));
                }
            }
        }
    }
    lines.join("\n")
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

    fn empty_extension_catalog() -> crate::protocol::ExtensionCatalog {
        crate::protocol::ExtensionCatalog {
            generation: 1,
            fingerprint: "0".repeat(64),
            extensions: Vec::new(),
            config: crate::protocol::ExtensionCatalogConfig::default(),
        }
    }
    use crate::{
        domain::AgentIntegration,
        resources::{
            PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
        },
    };

    fn agent_fixture() -> ResourceSnapshot {
        let session_id = "11111111-1111-1111-1111-111111111111".parse().unwrap();
        let workspace_id = "22222222-2222-2222-2222-222222222222".parse().unwrap();
        let tab_id = "33333333-3333-3333-3333-333333333333".parse().unwrap();
        let pane_id = "44444444-4444-4444-4444-444444444444".parse().unwrap();
        let terminal_id = "55555555-5555-5555-5555-555555555555".parse().unwrap();
        ResourceSnapshot {
            revision: 42,
            sessions: vec![SessionSnapshot {
                id: session_id,
                name: "session".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory("/project".into()),
                },
                trusted_project_config: None,
                closing: false,
                tokens: Default::default(),
                workspaces: vec![WorkspaceSnapshot {
                    id: workspace_id,
                    name: "workspace".into(),
                    root: "/project".into(),
                    closing: false,
                    tokens: Default::default(),
                    tabs: vec![TabSnapshot {
                        id: tab_id,
                        name: "tab".into(),
                        closing: false,
                        tokens: Default::default(),
                        layout: SplitTree::leaf(pane_id),
                        panes: vec![PaneSnapshot {
                            id: pane_id,
                            terminal_id,
                            closing: false,
                            tokens: Default::default(),
                            activity: AgentActivity {
                                integration: Some(AgentIntegration {
                                    source: Some("codex".into()),
                                    ..AgentIntegration::default()
                                }),
                                detection: None,
                                state: AgentState::Blocked,
                                revision: 9,
                                updated_at_ms: 1234,
                                last_event: None,
                                read_revision: 0,
                            },
                            cwd: None,
                            worktree: None,
                        }],
                    }],
                }],
            }],
        }
    }

    #[test]
    fn token_publish_accepts_hyphen_leading_presentation_text() {
        let workspace_id = WorkspaceId::new();
        let parsed = try_parse_cli_from([
            "fut",
            "token",
            "publish",
            "status",
            "deletions",
            "-5",
            "--workspace-id",
            &workspace_id.to_string(),
        ])
        .unwrap();

        assert!(matches!(
            parsed.command,
            Some(Command::Token {
                command: TokenCommand::Publish(TokenPublishArgs { value, .. })
            }) if value == "-5"
        ));
    }

    #[test]
    fn token_publish_requires_exactly_one_explicit_target() {
        assert!(
            try_parse_cli_from(["fut", "token", "publish", "status", "state", "ready"]).is_err()
        );
        assert!(
            try_parse_cli_from([
                "fut",
                "token",
                "publish",
                "status",
                "state",
                "ready",
                "--workspace-id",
                &WorkspaceId::new().to_string(),
                "--tab-id",
                &TabId::new().to_string(),
            ])
            .is_err()
        );
        let workspace_id = WorkspaceId::new();
        let parsed = try_parse_cli_from([
            "fut",
            "token",
            "publish",
            "status",
            "state",
            "ready",
            "--workspace-id",
            &workspace_id.to_string(),
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::Token {
                command: TokenCommand::Publish(TokenPublishArgs {
                    workspace_id: Some(id),
                    ..
                })
            }) if id == workspace_id
        ));
    }

    #[test]
    fn token_publish_accepts_one_optional_action() {
        let workspace_id = WorkspaceId::new();
        let pane_id = PaneId::new();
        let workspace_id_text = workspace_id.to_string();
        let pane_id_text = pane_id.to_string();
        let base = [
            "fut",
            "token",
            "publish",
            "status",
            "state",
            "ready",
            "--workspace-id",
            workspace_id_text.as_str(),
        ];
        let parsed = try_parse_cli_from(
            base.into_iter()
                .chain(["--action-pane-id", pane_id_text.as_str()]),
        )
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::Token {
                command: TokenCommand::Publish(TokenPublishArgs {
                    action_pane_id: Some(id),
                    action_command: None,
                    ..
                })
            }) if id == pane_id
        ));

        let parsed =
            try_parse_cli_from(base.into_iter().chain(["--action-command", "logs"])).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::Token {
                command: TokenCommand::Publish(TokenPublishArgs {
                    action_pane_id: None,
                    action_command: Some(command),
                    ..
                })
            }) if command == "logs"
        ));

        assert!(
            try_parse_cli_from(base.into_iter().chain([
                "--action-pane-id",
                pane_id_text.as_str(),
                "--action-command",
                "logs",
            ]))
            .is_err()
        );
    }

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
    fn agent_list_and_get_keep_the_versioned_json_golden_shape() {
        let snapshot = agent_fixture();
        let agents = integrated_agents(&snapshot).collect::<Vec<_>>();
        let list = serde_json::to_string(&OutputEnvelope {
            command: "agent.list",
            result: AgentListWire {
                agents,
                revision: snapshot.revision,
                unread_count: 0,
            },
            version: 1,
        })
        .unwrap();
        assert_eq!(
            list,
            r#"{"command":"agent.list","result":{"agents":[{"activity":{"integration":{"source":"codex"},"state":"blocked","revision":9,"updated_at_ms":1234},"available":true,"pane_id":"44444444-4444-4444-4444-444444444444","session":{"id":"11111111-1111-1111-1111-111111111111","name":"session"},"tab":{"id":"33333333-3333-3333-3333-333333333333","name":"tab"},"terminal_id":"55555555-5555-5555-5555-555555555555","unread":false,"workspace":{"id":"22222222-2222-2222-2222-222222222222","name":"workspace","root":"/project"}}],"revision":42,"unread_count":0},"version":1}"#
        );

        let agent = resolve_agent(
            &snapshot,
            "55555555-5555-5555-5555-555555555555".parse().unwrap(),
        )
        .unwrap();
        let get = serde_json::to_string(&OutputEnvelope {
            command: "agent.get",
            result: AgentGetWire {
                agent,
                revision: snapshot.revision,
            },
            version: 1,
        })
        .unwrap();
        assert_eq!(
            get,
            r#"{"command":"agent.get","result":{"agent":{"activity":{"integration":{"source":"codex"},"state":"blocked","revision":9,"updated_at_ms":1234},"available":true,"pane_id":"44444444-4444-4444-4444-444444444444","session":{"id":"11111111-1111-1111-1111-111111111111","name":"session"},"tab":{"id":"33333333-3333-3333-3333-333333333333","name":"tab"},"terminal_id":"55555555-5555-5555-5555-555555555555","unread":false,"workspace":{"id":"22222222-2222-2222-2222-222222222222","name":"workspace","root":"/project"}},"revision":42},"version":1}"#
        );
    }

    #[test]
    fn agent_projection_derives_availability_and_resolution_from_typed_paths() {
        let mut snapshot = agent_fixture();
        let terminal_id = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
        assert!(resolve_agent(&snapshot, terminal_id).unwrap().available);

        snapshot.sessions[0].closing = true;
        assert!(!resolve_agent(&snapshot, terminal_id).unwrap().available);
        snapshot.sessions[0].closing = false;
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .state = AgentState::Working;
        assert!(!resolve_agent(&snapshot, terminal_id).unwrap().available);

        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .last_event = Some(crate::domain::AgentEvent {
            revision: 10,
            kind: AgentReport::Exited,
            occurred_at_ms: 1235,
            turn_id: None,
        });
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .integration
            .as_mut()
            .unwrap()
            .active = false;
        assert!(integrated_agents(&snapshot).next().is_none());
        let error = resolve_agent(&snapshot, terminal_id).unwrap_err();
        assert_eq!(
            error.downcast_ref::<CliError>().unwrap().code,
            "not_an_agent"
        );

        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .last_event = None;
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .integration = None;
        let error = resolve_agent(&snapshot, terminal_id).unwrap_err();
        assert_eq!(
            error.downcast_ref::<CliError>().unwrap().code,
            "not_an_agent"
        );

        let error = resolve_agent(&snapshot, TerminalId::new()).unwrap_err();
        assert_eq!(error.downcast_ref::<CliError>().unwrap().code, "not_found");
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
    async fn unix_socket_peer_credentials_identify_the_current_process() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let client = UnixStream::connect(&socket).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let client_peer = peer_credentials(&client).unwrap();
        let server_peer = peer_credentials(&server).unwrap();
        let pid = std::process::id() as libc::pid_t;
        let uid = effective_user_id();
        assert_eq!(client_peer, PeerCredentials { pid, uid });
        assert_eq!(server_peer, PeerCredentials { pid, uid });
        assert!(owned_socket_identity(&socket).is_ok());
    }

    #[tokio::test]
    async fn daemon_executable_resolves_the_current_process() {
        let executable = daemon_executable(std::process::id() as libc::pid_t)
            .await
            .unwrap();
        assert!(executable.is_absolute());
        assert!(executable.exists());
    }

    #[test]
    fn forced_shutdown_rejects_non_socket_paths() {
        let temporary = tempfile::NamedTempFile::new().unwrap();
        let error = owned_socket_identity(temporary.path()).unwrap_err();
        assert!(error.to_string().contains("non-socket"));
    }

    #[test]
    fn handshake_and_command_responses_require_the_exact_request_id() {
        let expected = Uuid::new_v4();
        let messages = [
            ServerMessage::IncompatibleProtocol {
                client: PROTOCOL_VERSION,
                server: PROTOCOL_VERSION + 1,
            },
            ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                server_version: "test".into(),
                selected: None,
                extension_catalog: empty_extension_catalog(),
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
    fn help_and_version_are_successful_clap_results() {
        try_parse_cli_from(["fut"]).unwrap();

        for args in [
            &["fut", "--help"][..],
            &["fut", "-v"][..],
            &["fut", "-V"][..],
            &["fut", "--json", "--version"][..],
        ] {
            let error = match try_parse_cli_from(args) {
                Ok(_) => panic!("help/version unexpectedly parsed as a command"),
                Err(error) => error,
            };
            assert!(!error.use_stderr());
            assert_eq!(error.exit_code(), 0);
            if args.contains(&"-v") || args.contains(&"-V") || args.contains(&"--version") {
                assert_eq!(error.to_string(), format!("fut {BUILD_VERSION}\n"));
            }
        }
    }

    #[test]
    fn no_config_disables_files_and_conflicts_with_config_dir() {
        let cli = try_parse_cli_from(["fut", "--no-config", "list"]).unwrap();
        let location = cli.config_location().unwrap();
        assert!(location.is_disabled());
        assert_eq!(location.path, None);

        assert!(
            try_parse_cli_from([
                "fut",
                "--no-config",
                "--config-dir",
                "/tmp/fut-config",
                "list",
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_entire_command_tree() {
        let session = SessionId::new().to_string();
        let workspace = WorkspaceId::new().to_string();
        let tab = TabId::new().to_string();
        let pane = PaneId::new().to_string();
        let terminal = TerminalId::new().to_string();
        for args in [
            vec!["fut", "attach"],
            vec!["fut", "a"],
            vec!["fut", "open"],
            vec!["fut", "list"],
            vec!["fut", "ls"],
            vec!["fut", "events"],
            vec!["fut", "session", "attach", "a name"],
            vec!["fut", "session", "rename", &session, "new"],
            vec!["fut", "session", "close", &session],
            vec!["fut", "workspace", "attach", &workspace],
            vec!["fut", "workspace", "rename", &workspace, "new"],
            vec!["fut", "workspace", "close", &workspace],
            vec!["fut", "workspace", "retire", &workspace],
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
            vec!["fut", "session", "rename", "new"],
            vec!["fut", "session", "close"],
            vec!["fut", "workspace", "rename", "new"],
            vec!["fut", "workspace", "close"],
            vec!["fut", "workspace", "retire"],
            vec!["fut", "tab", "new"],
            vec!["fut", "tab", "list"],
            vec!["fut", "tab", "rename", "new"],
            vec!["fut", "tab", "close"],
            vec!["fut", "pane", "new"],
            vec!["fut", "pane", "split", "right"],
            vec!["fut", "pane", "list"],
            vec!["fut", "pane", "move", &tab],
            vec!["fut", "pane", "close"],
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
            vec!["fut", "agent", "prompt", &terminal, "--stdin"],
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
            vec![
                "fut",
                "agent",
                "notify",
                "codex",
                r#"{"type":"agent-turn-complete"}"#,
            ],
            vec!["fut", "context"],
            vec!["fut", "get", &terminal],
            vec!["fut", "daemon", "run"],
            vec!["fut", "daemon", "ping"],
            vec!["fut", "daemon", "shutdown"],
            vec!["fut", "daemon", "shutdown", "--force"],
        ] {
            Cli::try_parse_from(args).unwrap();
        }
    }

    #[test]
    fn agent_prompt_requires_exactly_one_text_source() {
        let terminal = TerminalId::new().to_string();
        assert!(Cli::try_parse_from(["fut", "agent", "prompt", &terminal]).is_err());
        assert!(
            Cli::try_parse_from(["fut", "agent", "prompt", &terminal, "literal", "--stdin",])
                .is_err()
        );

        let cli = Cli::try_parse_from(["fut", "agent", "prompt", &terminal, "--stdin"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Prompt {
                    text: None,
                    stdin: true,
                    ..
                }
            })
        ));
    }

    #[test]
    fn codex_completion_notification_maps_supported_payloads_only() {
        let metadata = codex_completion_metadata(
            r#"{"type":"agent-turn-complete","thread-id":"session-1","turn-id":"turn-1"}"#,
        )
        .unwrap();
        assert_eq!(metadata.source.as_deref(), Some("codex"));
        assert_eq!(metadata.agent_session_id.as_deref(), Some("session-1"));
        assert_eq!(metadata.turn_id.as_deref(), Some("turn-1"));

        assert!(codex_completion_metadata("not-json").is_none());
        assert!(codex_completion_metadata(r#"{"type":"approval-requested"}"#).is_none());
        assert!(
            codex_completion_metadata(
                &json!({
                    "type": "agent-turn-complete",
                    "turn-id": "x".repeat(MAX_AGENT_METADATA_VALUE_BYTES + 1),
                })
                .to_string()
            )
            .unwrap()
            .turn_id
            .is_none()
        );
    }

    #[test]
    fn relative_open_paths_are_anchored_to_the_calling_process() {
        let current = std::path::Path::new("/caller/project");
        assert_eq!(
            resolve_open_path(Some(PathBuf::from("../feature")), current),
            PathBuf::from("/caller/project/../feature")
        );
        assert_eq!(
            resolve_open_path(Some(PathBuf::from("/absolute/feature")), current),
            PathBuf::from("/absolute/feature")
        );
        assert_eq!(resolve_open_path(None, current), current);
    }

    #[test]
    fn rename_parser_distinguishes_implicit_names_from_explicit_id_name_pairs() {
        let session = SessionId::new().to_string();
        let implicit =
            Cli::try_parse_from(["fut", "session", "rename", "project UUID-ish"]).unwrap();
        assert!(matches!(
            implicit.command,
            Some(Command::Session {
                command: SessionCommand::Rename {
                    session_or_name,
                    name: None,
                },
            }) if session_or_name == "project UUID-ish"
        ));

        let explicit =
            Cli::try_parse_from(["fut", "session", "rename", session.as_str(), "project"]).unwrap();
        assert!(matches!(
            explicit.command,
            Some(Command::Session {
                command: SessionCommand::Rename {
                    session_or_name,
                    name: Some(name),
                },
            }) if session_or_name == session && name == "project"
        ));
    }

    #[tokio::test]
    async fn rename_processing_rejects_a_sole_uuid_before_context_or_daemon_lookup() {
        let missing_socket = tempfile::tempdir().unwrap().path().join("missing.sock");
        let cases = [
            (
                "session",
                Command::Session {
                    command: SessionCommand::Rename {
                        session_or_name: SessionId::new().to_string(),
                        name: None,
                    },
                },
            ),
            (
                "workspace",
                Command::Workspace {
                    command: WorkspaceCommand::Rename {
                        workspace_or_name: WorkspaceId::new().to_string(),
                        name: None,
                    },
                },
            ),
            (
                "tab",
                Command::Tab {
                    command: TabCommand::Rename {
                        tab_or_name: TabId::new().to_string(),
                        name: None,
                    },
                },
            ),
        ];

        for (kind, command) in cases {
            let error = run_mutation(&missing_socket, false, command)
                .await
                .unwrap_err();
            let error = error.downcast_ref::<CliError>().unwrap();
            assert_eq!(error.code, "invalid_arguments");
            assert_eq!(error.message, format!("missing NAME after {kind} ID"));
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
            tokens: Default::default(),
            id,
            terminal_id: TerminalId::new(),
            closing: false,
            activity: Default::default(),
            cwd: None,
            worktree: None,
        };
        let tab = TabSnapshot {
            tokens: Default::default(),
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
    fn resource_listing_matches_the_navigator_tree_and_verbose_keeps_ids() {
        let snapshot = agent_fixture();

        assert_eq!(
            render_resource_tree(&snapshot),
            "! session\n  ! workspace\n    ! tab · pane"
        );

        let verbose = render_verbose_resources(&snapshot);
        assert!(verbose.starts_with(&format!("revision=42\nsession {}", snapshot.sessions[0].id)));
        assert!(verbose.contains(&format!(
            "terminal={}",
            snapshot.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id
        )));
        assert_eq!(
            render_resource_tree(&ResourceSnapshot {
                revision: 43,
                sessions: Vec::new(),
            }),
            "No resources"
        );
    }

    #[test]
    fn resource_listing_accepts_verbose_after_list_or_ls() {
        for command in ["list", "ls"] {
            let cli = try_parse_cli_from(["fut", command, "-v"]).unwrap();
            assert!(matches!(cli.command, Some(Command::List { verbose: true })));
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
    fn logical_keys_validate_as_structured_terminal_events() {
        let events = ["é", "enter", "ctrl+c", "alt+left", "shift+tab", "f12"]
            .into_iter()
            .map(|key| key.parse::<LogicalKey>().unwrap().event)
            .collect::<Vec<_>>();
        assert_eq!(
            events[0].code,
            crate::domain::TerminalKeyCode::Character('é')
        );
        assert_eq!(events[0].text.as_deref(), Some("é"));
        assert_eq!(events[1].code, crate::domain::TerminalKeyCode::Enter);
        assert!(events[2].modifiers.control);
        assert_eq!(events[3].code, crate::domain::TerminalKeyCode::Left);
        assert!(events[3].modifiers.alt);
        assert_eq!(events[4].code, crate::domain::TerminalKeyCode::Tab);
        assert!(events[4].modifiers.shift);
        assert_eq!(events[5].code, crate::domain::TerminalKeyCode::Function(12));
        for accepted in ["ctrl+left", "ctrl+é", "shift+up"] {
            assert!(
                accepted.parse::<LogicalKey>().is_ok(),
                "rejected {accepted:?}"
            );
        }
        for invalid in ["", "no-such-key", "ctrl+", "hyper+c", "f13"] {
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
    fn open_accepts_a_catalog_project_with_an_optional_worktree_path() {
        for flag in ["-p", "--project"] {
            let cli = Cli::try_parse_from(["fut", "open", "../feature", flag, "fut"]).unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::Open {
                    path: Some(path),
                    project: Some(project),
                    ..
                }) if path == std::path::Path::new("../feature") && project == "fut"
            ));
        }
    }

    #[test]
    fn open_attaches_by_default_and_accepts_a_background_flag() {
        let foreground = Cli::try_parse_from(["fut", "open"]).unwrap();
        assert!(attaches_client(&foreground.command));

        let alias = Cli::try_parse_from(["fut", "o", "--background"]).unwrap();
        assert!(matches!(
            alias.command,
            Some(Command::Open {
                background: true,
                ..
            })
        ));

        for flag in ["-b", "--background"] {
            let background = Cli::try_parse_from(["fut", "open", flag]).unwrap();
            assert!(!attaches_client(&background.command));
            assert!(matches!(
                background.command,
                Some(Command::Open {
                    background: true,
                    ..
                })
            ));
        }
    }

    #[test]
    fn ui_playground_is_a_top_level_launch_flag() {
        let playground = Cli::try_parse_from(["fut", "--ui-playground"]).unwrap();
        assert!(playground.ui_playground);
        assert!(playground.command.is_none());

        let with_command = Cli::try_parse_from(["fut", "--ui-playground", "attach"]).unwrap();
        assert!(with_command.ui_playground);
        assert!(matches!(with_command.command, Some(Command::Attach)));
    }

    #[test]
    fn project_commands_parse_with_global_json() {
        let init = Cli::try_parse_from(["fut", "project", "init"]).unwrap();
        assert!(matches!(
            init.command,
            Some(Command::Project {
                command: ProjectCommand::Init
            })
        ));

        for command in ["list", "ls"] {
            let cli = Cli::try_parse_from(["fut", "project", command]).unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::Project {
                    command: ProjectCommand::List
                })
            ));
        }

        let cli = Cli::try_parse_from(["fut", "--json", "project", "trust", "fut"]).unwrap();
        assert!(cli.json);
        assert!(matches!(
            cli.command,
            Some(Command::Project {
                command: ProjectCommand::Trust { name }
            }) if name == "fut"
        ));

        let cli = Cli::try_parse_from(["fut", "project", "untrust", "fut"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Project {
                command: ProjectCommand::Untrust { name }
            }) if name == "fut"
        ));
    }

    #[tokio::test]
    async fn configured_project_supplies_its_root_and_preserves_an_explicit_path() {
        let temporary = tempfile::tempdir().unwrap();
        let main = temporary.path().join("main");
        let linked = temporary.path().join("linked");
        let config = temporary.path().join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(
            config.join("config.toml"),
            format!("[projects.fut]\npath = {:?}\n", main),
        )
        .unwrap();
        let config_location = client::config::resolve_location(Some(&config)).unwrap();

        assert_eq!(
            resolve_project_open(None, Some("fut"), temporary.path(), &config_location)
                .await
                .unwrap()
                .0,
            main
        );
        assert_eq!(
            resolve_project_open(
                Some(PathBuf::from("linked")),
                Some("fut"),
                temporary.path(),
                &config_location,
            )
            .await
            .unwrap()
            .0,
            linked
        );
    }

    #[test]
    fn rejects_legacy_forms_typed_prefixes_and_unambiguous_bad_ids() {
        for args in [
            ["fut", "new"],
            ["fut", "new-tab"],
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
        assert!(parse_id::<SessionId>("a-name", "session").is_err());
        assert!(Cli::try_parse_from(["fut", "move-pane"]).is_err());
        assert!(Cli::try_parse_from(["fut", "pane", "move", "bad", "also-bad"]).is_err());
        let pane = PaneId::new().to_string();
        assert!(Cli::try_parse_from(["fut", "pane", "move", &pane]).is_ok());
    }

    #[test]
    fn rejects_json_for_interactive_commands() {
        let terminal = TerminalId::new().to_string();
        for args in [
            vec!["fut", "--json"],
            vec!["fut", "--json", "attach"],
            vec!["fut", "--json", "open"],
            vec!["fut", "--json", "terminal", "attach", &terminal],
            vec!["fut", "--json", "daemon", "run"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(reject_interactive_json(&cli).is_err());
        }
        let cli = Cli::try_parse_from(["fut", "--json", "list"]).unwrap();
        assert!(reject_interactive_json(&cli).is_ok());
        let cli = Cli::try_parse_from(["fut", "--json", "open", "--background"]).unwrap();
        assert!(reject_interactive_json(&cli).is_ok());
        let tab = TabId::new().to_string();
        let cli = Cli::try_parse_from(["fut", "--json", "pane", "new", &tab]).unwrap();
        assert!(reject_interactive_json(&cli).is_ok());
    }

    #[test]
    fn identifies_only_client_attaching_commands_as_clients() {
        let session = SessionId::new().to_string();
        let workspace = WorkspaceId::new().to_string();
        let tab = TabId::new().to_string();
        let pane = PaneId::new().to_string();
        let terminal = TerminalId::new().to_string();
        for args in [
            vec!["fut"],
            vec!["fut", "attach"],
            vec!["fut", "session", "attach", &session],
            vec!["fut", "workspace", "attach", &workspace],
            vec!["fut", "tab", "attach", &tab],
            vec!["fut", "pane", "attach", &pane],
            vec!["fut", "terminal", "attach", &terminal],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(attaches_client(&cli.command));
        }

        for args in [
            vec!["fut", "list"],
            vec!["fut", "open", "--background"],
            vec!["fut", "daemon", "run"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(!attaches_client(&cli.command));
        }
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
        let command = cli_command();
        let names: Vec<_> = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(
            names,
            [
                "attach",
                "open",
                "project",
                "session",
                "workspace",
                "tab",
                "pane",
                "terminal",
                "agent",
                "extension",
                "token",
                "context",
                "get",
                "list",
                "events",
                "doctor",
                "daemon"
            ]
        );

        let mut help = Vec::new();
        cli_command().write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(help.contains("versioned JSON for noninteractive commands only"));
        assert!(help.contains("Open a location and attach to it"));

        let command = cli_command();
        let project = command.find_subcommand("project").unwrap();
        assert_eq!(
            project
                .get_subcommands()
                .map(clap::Command::get_name)
                .collect::<Vec<_>>(),
            ["init", "list", "trust", "untrust"]
        );

        let command = cli_command();
        let extension = command.find_subcommand("extension").unwrap();
        assert_eq!(
            extension
                .get_subcommands()
                .map(clap::Command::get_name)
                .collect::<Vec<_>>(),
            [
                "list",
                "show",
                "validate",
                "install",
                "install-git",
                "update",
                "enable",
                "disable",
                "remove",
                "reload"
            ]
        );
        let validate = extension.find_subcommand("validate").unwrap();
        assert_eq!(
            validate
                .get_arguments()
                .map(clap::Arg::get_id)
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["path"]
        );
        let install_git = extension.find_subcommand("install-git").unwrap();
        assert_eq!(
            install_git
                .get_arguments()
                .map(clap::Arg::get_id)
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["url", "rev", "sha256"]
        );
        let update = extension.find_subcommand("update").unwrap();
        assert_eq!(
            update
                .get_arguments()
                .map(clap::Arg::get_id)
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["id", "rev", "sha256"]
        );

        let command = cli_command();
        let pane = command.find_subcommand("pane").unwrap();
        let movement = pane.find_subcommand("move").unwrap();
        let positional_names: Vec<_> = movement
            .get_arguments()
            .map(clap::Arg::get_id)
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            positional_names,
            ["pane_or_destination_id", "destination_tab_id"]
        );
    }
}
