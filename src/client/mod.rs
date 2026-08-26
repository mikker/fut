//! Interactive terminal client for a running Fut daemon.

mod actions;
mod agent_dialog;
mod agents;
mod cheatsheet;
mod chrome;
mod command_bar;
mod command_form;
pub mod config;
mod context_menu;
mod copy_mode;
mod dialog;
mod fuzzy;
mod graphics;
mod hotkey;
pub(crate) mod input;
mod layout;
mod navigation;
mod navigator;
mod notifications;
mod perf;
mod presentation;
mod project_opener;
mod rename;
mod sidebar;
mod tab_bar;
mod temporary_command;
mod toast;

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    future::Future,
    io::{self, Write},
    num::NonZeroU16,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use actions::{ClientAction, FocusDirection, HistoryScope, NavigationScope};
use agent_dialog::{AgentsAction, AgentsDialog};
use anyhow::{Context, bail};
use bytes::Bytes;
use chrome::{
    ClientLayout, MIN_DOCKED_TERMINAL_WIDTH, ResourceState, client_layout, render_tab_bar,
    sanitize, sidebar_drawer,
};
use command_bar::{CommandBarAction, CommandBarState};
use command_form::{CommandFormAction, CommandFormState};
use config::{
    MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH, PaletteCommand, PaneLayoutPolicy, SidebarDisplay,
    StagedUiConfig, UiConfig,
};
use context_menu::{ContextMenuAction, ContextMenuState};
use copy_mode::{
    CopyModeErrorDisposition, CopyModeInput, CopyModePaste, CopyModeReply, CopyModeState,
};
use crossterm::{
    SynchronizedUpdate,
    cursor::{Hide, SetCursorStyle, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton as HostMouseButton,
        MouseEvent as HostMouseEvent, MouseEventKind as HostMouseEventKind,
    },
    execute,
    terminal::{
        DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use futures_util::{SinkExt, StreamExt};
use input::{PrefixAction, PrefixState, encode_key, terminal_key_event};
use layout::{
    PaneLayout, SplitDivider, authored_layout, authored_navigation_layout, directional_neighbor,
    navigation_pane_layouts, pane_layouts,
};
use navigation::NavigationHistory;
use navigator::{NavigatorAction, NavigatorState};
use notifications::{
    AttentionAcknowledgement, DirectAttentionAction, NotificationsAction, NotificationsDialog,
};
use project_opener::{ProjectOpenChoice, ProjectOpenerAction, ProjectOpenerState};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Clear, Widget},
};
use rename::{RenameAction, RenameState};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    signal::unix::{Signal, SignalKind, signal},
    sync::mpsc,
    task::JoinHandle,
    time,
};
use tokio_util::codec::Framed;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use sidebar::{ComponentEffect, SidebarComponentKind, SidebarSide, SidebarState, render_sidebar};
use tab_bar::{TabBarAction, TabBarState};
use temporary_command::{
    BackgroundCommandResult, ExtensionCommandContext, TemporaryCommandSurface,
    TemporaryCommandUpdate, dispatch_background_command,
};
use toast::{Toast, ToastState};

use crate::{
    domain::{
        CellColor, CellStyle, CursorShape, MouseButton, MouseButtons, MouseEvent, MouseEventKind,
        MouseModifiers, MouseWheelDirection, ScreenDelta, ScreenSnapshot, ScrollPosition,
        SessionId, SplitId, TerminalId, TerminalSize,
    },
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, SelectedTarget, SelectedView,
        SelectionExpectation, ServerMessage, codec, decode_payload, encode_payload,
    },
    resources::{ResourceSnapshot, TargetSelector},
    splits::{SplitRatio, SplitTree},
};

enum ClientSurface {
    Navigator(NavigatorState),
    ProjectOpener(ProjectOpenerState),
    Agents(AgentsDialog),
    Notifications(NotificationsDialog),
    Sidebar(SidebarState),
    TabBar(TabBarState),
    CommandBar(CommandBarState),
    ContextMenu(ContextMenuState),
    CommandForm(Box<CommandFormState>),
}

struct ExtensionReloadState {
    session_id: SessionId,
    phase: ExtensionReloadPhase,
}

enum ExtensionReloadPhase {
    Preparing {
        request_id: Uuid,
        staged: StagedUiConfig,
    },
    Committing {
        request_id: Uuid,
        base_generation: u64,
        ui: UiConfig,
        catalog: crate::protocol::ExtensionCatalog,
    },
}

impl ExtensionReloadState {
    fn request_id(&self) -> Uuid {
        match self.phase {
            ExtensionReloadPhase::Preparing { request_id, .. }
            | ExtensionReloadPhase::Committing { request_id, .. } => request_id,
        }
    }
}

struct ProjectPreparationTask {
    request_id: Uuid,
    handle: JoinHandle<()>,
}

impl ProjectPreparationTask {
    fn spawn(
        choice: ProjectOpenChoice,
        config_location: config::ConfigLocation,
        extensions: Vec<crate::extensions::Extension>,
        results: mpsc::UnboundedSender<ProjectPreparationCompletion>,
    ) -> Self {
        let request_id = Uuid::new_v4();
        Self::spawn_with(
            request_id,
            async move { prepare_project_open(choice, &config_location, &extensions).await },
            results,
        )
    }

    fn spawn_with<F>(
        request_id: Uuid,
        preparation: F,
        results: mpsc::UnboundedSender<ProjectPreparationCompletion>,
    ) -> Self
    where
        F: Future<Output = anyhow::Result<ProjectOpenPreparation>> + Send + 'static,
    {
        let handle = tokio::spawn(async move {
            let result = preparation.await;
            let _ = results.send(ProjectPreparationCompletion { request_id, result });
        });
        Self { request_id, handle }
    }

    fn is_for(&self, request_id: Uuid) -> bool {
        self.request_id == request_id
    }
}

impl Drop for ProjectPreparationTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct ProjectPreparationCompletion {
    request_id: Uuid,
    result: anyhow::Result<ProjectOpenPreparation>,
}

struct ProjectConfigReloadState {
    request_id: Uuid,
    kind: ProjectConfigReloadKind,
}

#[derive(Clone, Copy)]
enum ProjectConfigReloadKind {
    Configuration,
    ProjectOnly,
}

impl ProjectConfigReloadKind {
    fn success_message(self) -> &'static str {
        match self {
            Self::Configuration => "config reloaded",
            Self::ProjectOnly => "project config reloaded",
        }
    }

    fn error_message(self) -> &'static str {
        match self {
            Self::Configuration => "project config reload failed after global config reloaded",
            Self::ProjectOnly => "project config reload failed",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum PaneMouseAction {
    Input {
        terminal_id: TerminalId,
        event: MouseEvent,
    },
    Focus(crate::domain::PaneId),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum MouseButtonState {
    #[default]
    Idle,
    Suppressed,
    Selecting {
        terminal_id: TerminalId,
        anchor: (u16, u16),
    },
    Captured {
        terminal_id: TerminalId,
        column: u16,
        row: u16,
        modifiers: MouseModifiers,
    },
}

#[derive(Default)]
struct MouseInputState {
    buttons: [MouseButtonState; 3],
    ui_drag: Option<UiDrag>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiDrag {
    Sidebar {
        position: sidebar::SidebarSide,
        last_width: u16,
        max_width: u16,
    },
    Split {
        tab_id: crate::domain::TabId,
        split_id: SplitId,
        last_cell: (u16, u16),
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiResizeAction {
    Sidebar {
        side: sidebar::SidebarSide,
        width: u16,
    },
    Split {
        tab_id: crate::domain::TabId,
        split_id: SplitId,
        ratio: SplitRatio,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiMouseRoute {
    NotOwned,
    Owned(Option<UiResizeAction>),
}

enum UiActivation {
    Sidebar(ComponentEffect),
    Tab(TabBarAction),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SidebarDivider {
    position: sidebar::SidebarSide,
    area: Rect,
    current_width: u16,
    max_width: u16,
}

enum ClipboardResult {
    Copied { request_id: Uuid, bytes: usize },
    Failed { request_id: Uuid, message: String },
}

/// Hesitation after the prefix before the which-key cheatsheet appears.
const CHEATSHEET_DELAY: Duration = Duration::from_millis(700);

const PBCOPY_TIMEOUT: Duration = Duration::from_secs(2);
const PBCOPY_REAP_TIMEOUT: Duration = Duration::from_secs(1);

/// Attach an interactive full-screen client to an already-running daemon.
pub async fn attach(
    socket_path: &Path,
    selector: Option<TargetSelector>,
    config_location: &config::ConfigLocation,
) -> anyhow::Result<()> {
    attach_with_ui(
        socket_path,
        selector,
        stage_ui_config(config_location)?,
        config_location,
    )
    .await
}

/// Open a lease-free global navigator on an existing daemon, then attach only
/// after the user chooses a destination.
pub async fn attach_navigator(
    socket_path: &Path,
    config_location: &config::ConfigLocation,
) -> anyhow::Result<()> {
    let staged = stage_ui_config(config_location)?;
    let (mut navigator_connection, catalog) = connect_control_navigator(socket_path).await?;
    let ui = staged.materialize(&catalog)?;
    let (snapshot, presence) =
        match time::timeout(Duration::from_secs(2), receive(&mut navigator_connection))
            .await
            .context("daemon resource snapshot timed out")??
        {
            ServerMessage::Resources { snapshot, presence } => (snapshot, presence),
            ServerMessage::Error { code, message } => bail!("daemon error ({code}): {message}"),
            message => bail!("expected resources from daemon, received {message:?}"),
        };

    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let selector = initial_navigator(
        &mut terminal,
        &mut navigator_connection,
        snapshot,
        presence,
        &ui,
    )
    .await?;
    drop(navigator_connection);
    let Some(selector) = selector else {
        drop(terminal);
        drop(guard);
        return Ok(());
    };

    let staged = stage_ui_config(config_location)?;
    let (columns, rows) = crossterm::terminal::size().context("read terminal size")?;
    let size = TerminalSize { columns, rows };
    let (mut framed, selected, catalog, alerts) = connect_interactive(
        socket_path,
        Some(selector),
        size,
        alert_client_id(socket_path)?,
    )
    .await?;
    let ui = staged.materialize(&catalog)?;
    let result = run(
        &mut terminal,
        &mut framed,
        selected,
        alerts,
        ui,
        catalog.generation,
        socket_path,
        config_location,
    )
    .await;
    drop(terminal);
    drop(guard);
    result
}

pub(crate) fn stage_ui_config(
    config_location: &config::ConfigLocation,
) -> anyhow::Result<StagedUiConfig> {
    config::stage_location(config_location)
}

pub(crate) async fn attach_with_ui(
    socket_path: &Path,
    selector: Option<TargetSelector>,
    staged: StagedUiConfig,
    config_location: &config::ConfigLocation,
) -> anyhow::Result<()> {
    let (columns, rows) = crossterm::terminal::size().context("read terminal size")?;
    let alert_client_id = alert_client_id(socket_path)?;
    let (mut framed, selected, catalog, alerts) = connect_interactive(
        socket_path,
        selector,
        TerminalSize { columns, rows },
        alert_client_id,
    )
    .await?;
    let ui = staged.materialize(&catalog)?;

    // Host terminal state is changed only after a successful handshake.
    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = run(
        &mut terminal,
        &mut framed,
        selected,
        alerts,
        ui,
        catalog.generation,
        socket_path,
        config_location,
    )
    .await;
    drop(terminal);
    drop(guard);
    result
}

fn alert_client_id(socket_path: &Path) -> anyhow::Result<crate::domain::ClientId> {
    if let Ok(value) = std::env::var("FUT_ALERT_CLIENT_ID") {
        return value.parse().context("parse FUT_ALERT_CLIENT_ID");
    }
    let identity = fs::canonicalize("/dev/fd/0")
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|_| std::env::var("TERM_SESSION_ID"))
        .unwrap_or_else(|_| format!("pid-{}", std::process::id()));
    let digest = Sha256::digest(identity.as_bytes());
    let key = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let directory = socket_path.parent().unwrap_or_else(|| Path::new("."));
    let path = directory.join(format!("alert-client-{key}"));
    if let Ok(value) = fs::read_to_string(&path) {
        return value
            .trim()
            .parse()
            .context("parse retained alert client ID");
    }
    let client_id = crate::domain::ClientId::new();
    fs::write(&path, client_id.to_string())
        .with_context(|| format!("retain alert client identity at {}", path.display()))?;
    Ok(client_id)
}

async fn connect_interactive(
    socket_path: &Path,
    selector: Option<TargetSelector>,
    size: TerminalSize,
    alert_client_id: crate::domain::ClientId,
) -> anyhow::Result<(
    Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    SelectedView,
    crate::protocol::ExtensionCatalog,
    crate::alerts::ClientAlertSnapshot,
)> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect to {}", socket_path.display()))?;
    let mut framed = Framed::new(stream, codec());
    send(
        &mut framed,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
            mode: ClientMode::Interactive { size, selector },
        },
    )
    .await?;
    let selected = match time::timeout(Duration::from_secs(2), receive(&mut framed))
        .await
        .context("daemon handshake timed out")??
    {
        ServerMessage::Welcome {
            version,
            selected: Some(selected),
            extension_catalog,
            ..
        } if version == PROTOCOL_VERSION => (selected, extension_catalog),
        ServerMessage::Welcome { selected: None, .. } => bail!("daemon did not select a terminal"),
        ServerMessage::Welcome { version, .. } => {
            bail!("daemon welcomed client with unsupported protocol version {version}")
        }
        ServerMessage::IncompatibleProtocol { client, server } => {
            bail!("incompatible protocol: client {client}, server {server}")
        }
        ServerMessage::Error { code, message } => bail!("daemon error ({code}): {message}"),
        message => bail!("expected welcome from daemon, received {message:?}"),
    };
    send_request(
        &mut framed,
        None,
        ClientMessage::WatchAlerts {
            client_id: alert_client_id,
        },
    )
    .await?;
    Ok((
        framed,
        selected.0,
        selected.1,
        crate::alerts::ClientAlertSnapshot::default(),
    ))
}

async fn connect_control_navigator(
    socket_path: &Path,
) -> anyhow::Result<(
    Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    crate::protocol::ExtensionCatalog,
)> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect to {}", socket_path.display()))?;
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
    let extension_catalog = match time::timeout(Duration::from_secs(2), receive(&mut framed))
        .await
        .context("daemon handshake timed out")??
    {
        ServerMessage::Welcome {
            version,
            selected: None,
            extension_catalog,
            ..
        } if version == PROTOCOL_VERSION => extension_catalog,
        ServerMessage::IncompatibleProtocol { client, server } => {
            bail!("incompatible protocol: client {client}, server {server}")
        }
        ServerMessage::Error { code, message } => bail!("daemon error ({code}): {message}"),
        message => bail!("expected control welcome from daemon, received {message:?}"),
    };
    send_request(
        &mut framed,
        Some(Uuid::new_v4()),
        ClientMessage::WatchResources,
    )
    .await?;
    Ok((framed, extension_catalog))
}

async fn initial_navigator(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    snapshot: ResourceSnapshot,
    presence: crate::protocol::ClientPresenceSnapshot,
    ui: &UiConfig,
) -> anyhow::Result<Option<TargetSelector>> {
    let mut navigator = NavigatorState::open();
    navigator.accept_global_resources(&snapshot);
    navigator.accept_presence(&presence);
    let mut events = EventStream::new();
    let mut termination = TerminationSignals::subscribe()?;
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            frame.render_widget(Clear, area);
            navigator.render(area, 0, &ui.styles, frame.buffer_mut());
        })?;
        tokio::select! {
            name = termination.recv() => bail!("terminated by {name}"),
            event = events.next() => match event.transpose()? {
                Some(Event::Key(key)) => {
                    let visible = navigator::dialog_body_rows(terminal.size()?.into());
                    match navigator.key(key, visible) {
                        NavigatorAction::Stay => {}
                        NavigatorAction::Close => return Ok(None),
                        NavigatorAction::Select(selector) => return Ok(Some(selector)),
                    }
                }
                Some(Event::Paste(text)) => navigator.paste(&text),
                Some(Event::Resize(_, _)) => {}
                Some(_) => {}
                None => return Ok(None),
            },
            frame = framed.next() => {
                let Some(frame) = frame else { bail!("daemon disconnected while navigator was open") };
                let envelope: Envelope<ServerMessage> = decode_payload(&frame?)?;
                match envelope.message {
                    ServerMessage::ResourcesChanged { snapshot } => {
                        navigator.accept_global_resources(&snapshot);
                    }
                    ServerMessage::Resources { snapshot, presence } => {
                        navigator.accept_global_resources(&snapshot);
                        navigator.accept_presence(&presence);
                    }
                    ServerMessage::PresenceChanged { presence } => {
                        navigator.accept_presence(&presence);
                    }
                    ServerMessage::Error { code, message } => bail!("daemon error ({code}): {message}"),
                    _ => {}
                }
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the client runtime coordinates transport, initial snapshots, UI, and configuration"
)]
async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    selected: SelectedView,
    alerts: crate::alerts::ClientAlertSnapshot,
    ui: UiConfig,
    catalog_generation: u64,
    socket_path: &Path,
    config_location: &config::ConfigLocation,
) -> anyhow::Result<()> {
    let mut client_hooks = crate::extensions::ClientHookRuntime::new(
        ui.extensions.clone(),
        std::env::current_exe().context("resolve current Fut executable")?,
        socket_path.to_owned(),
    );
    let result = run_loop(
        terminal,
        framed,
        selected,
        alerts,
        ui,
        catalog_generation,
        socket_path,
        config_location,
        &mut client_hooks,
    )
    .await;
    client_hooks.shutdown().await;
    result
}

#[allow(
    clippy::too_many_arguments,
    reason = "the event loop owns the coordinated terminal, protocol, UI, and hook runtimes"
)]
async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    selected: SelectedView,
    alerts: crate::alerts::ClientAlertSnapshot,
    mut ui: UiConfig,
    mut extension_generation: u64,
    socket_path: &Path,
    config_location: &config::ConfigLocation,
    client_hooks: &mut crate::extensions::ClientHookRuntime,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut termination = TerminationSignals::subscribe()?;
    let mut prefix = PrefixState::new(ui.bindings.clone());
    let mut mouse_input = MouseInputState::default();
    let mut view = ViewState::new(selected)?;
    let mut resources = ResourceState::default();
    resources.accept_alerts(alerts);
    let mut surface: Option<ClientSurface> = None;
    let mut temporary_command: Option<TemporaryCommandSurface> = None;
    let mut copy_mode: Option<CopyModeState> = None;
    let mut rename: Option<RenameState> = None;
    let mut workspace_history = NavigationHistory::default();
    workspace_history.record(view.focused());
    let mut create = CreateCoordinator::default();
    let mut close_target = CloseTargetState::default();
    let mut focus = FocusState::default();
    let mut toasts = ToastState::default();
    let mut pending_focused_exit: Option<Option<i32>> = None;
    let mut force_draw = false;
    let mut cheatsheet_at: Option<time::Instant> = None;
    let mut cheatsheet_visible = false;
    let mut spinner_frame = 0usize;
    let mut host_cursor = HostCursorState::default();
    let mut kitty_graphics = graphics::Renderer::new();
    let mut perf = perf::PerfLog::from_env();
    let mut extension_reload: Option<ExtensionReloadState> = None;
    let mut project_config_reload: Option<ProjectConfigReloadState> = None;
    let mut project_preparation: Option<ProjectPreparationTask> = None;
    let mut redraw = time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut spinner = time::interval(Duration::from_millis(100));
    spinner.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let (clipboard_results, mut clipboard_result) = mpsc::channel(1);
    let (background_results, mut background_result) = mpsc::unbounded_channel();
    let (project_preparation_results, mut project_preparation_result) =
        mpsc::unbounded_channel::<ProjectPreparationCompletion>();
    send_request(framed, Some(Uuid::new_v4()), ClientMessage::ListResources).await?;
    resize_view(framed, terminal.size()?.into(), &mut view, &resources, &ui).await?;

    loop {
        tokio::select! {
            name = termination.recv() => {
                // Returning unwinds through TerminalGuard, restoring the host
                // terminal (mouse tracking off, cooked mode) before exit.
                bail!("terminated by {name}");
            }
            update = async { temporary_command.as_mut().expect("guarded command").update().await }, if temporary_command.is_some() => {
                match update {
                    TemporaryCommandUpdate::Screen => force_draw = true,
                    TemporaryCommandUpdate::Error(message) => {
                        toasts.error(format!("command failed · {}", sanitize(&message)));
                        force_draw = true;
                    }
                    TemporaryCommandUpdate::Exited(exit_code) => {
                        let activated_target = if exit_code == Some(0) {
                            match temporary_command
                                .as_ref()
                                .expect("command exists")
                                .activated_target()
                            {
                                Ok(target) => target,
                                Err(error) => {
                                    toasts.error(format!(
                                        "command activation failed · {}",
                                        one_line_error(&error)
                                    ));
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        let failure_log = if exit_code == Some(0) {
                            None
                        } else {
                            Some(
                                temporary_command
                                    .as_ref()
                                    .expect("command exists")
                                    .write_failure_log(socket_path)
                                    .await,
                            )
                        };
                        temporary_command = None;
                        view.invalidate_drawn();
                        match (exit_code, failure_log) {
                            (Some(0), _) => {}
                            (Some(code), Some(Ok(path))) => toasts.error(format!(
                                "command exited · status {code} · see {}",
                                path.display()
                            )),
                            (None, Some(Ok(path))) => toasts.error(format!(
                                "command exited · status unknown · see {}",
                                path.display()
                            )),
                            (Some(code), Some(Err(error))) => toasts.error(format!(
                                "command exited · status {code} · log failed: {}",
                                one_line_error(&error)
                            )),
                            (None, Some(Err(error))) => toasts.error(format!(
                                "command exited · status unknown · log failed: {}",
                                one_line_error(&error)
                            )),
                            (_, None) => unreachable!("failures produce a log result"),
                        }
                        resize_view(framed, terminal.size()?.into(), &mut view, &resources, &ui).await?;
                        if let Some(pane_id) = activated_target
                            && let Some(request) = focus.begin(FocusOrigin::Workspace)
                        {
                            send_request(
                                framed,
                                Some(request),
                                ClientMessage::SelectTarget {
                                    selector: TargetSelector::Pane(pane_id),
                                    expected: None,
                                },
                            )
                            .await?;
                        }
                        force_draw = true;
                    }
                    TemporaryCommandUpdate::Stopped => {
                        temporary_command = None;
                        view.invalidate_drawn();
                        toasts.error("command failed · terminal runtime stopped");
                        resize_view(framed, terminal.size()?.into(), &mut view, &resources, &ui).await?;
                        force_draw = true;
                    }
                }
            }
            result = background_result.recv() => {
                if let Some(BackgroundCommandResult::Failed(message)) = result {
                    toasts.error(format!("command failed · {}", sanitize(&message)));
                    force_draw = true;
                }
            }
            completion = project_preparation_result.recv() => {
                let Some(completion) = completion else {
                    unreachable!("the event loop retains a project preparation sender")
                };
                if !project_preparation
                    .as_ref()
                    .is_some_and(|task| task.is_for(completion.request_id))
                {
                    continue;
                }
                project_preparation = None;
                let accepted = match surface.as_ref() {
                    Some(ClientSurface::ProjectOpener(opener)) => {
                        opener.accepts_preparation(completion.request_id)
                    }
                    _ => false,
                };
                if !accepted {
                    continue;
                }
                match completion.result {
                    Ok(ProjectOpenPreparation::Ready { project, cwd }) => {
                        let opener = project_opener_mut(&mut surface);
                        request_project_open(framed, opener, project, cwd).await?;
                    }
                    Ok(ProjectOpenPreparation::Approval {
                        project,
                        cwd,
                        path,
                        digest,
                        source,
                    }) => {
                        project_opener_mut(&mut surface)
                            .confirm_approval(project, cwd, path, digest, &source);
                    }
                    Err(error) => {
                        project_opener_mut(&mut surface).show_error(one_line_error(&error));
                    }
                }
                force_draw = true;
            }
            frame = framed.next() => {
                let Some(frame) = frame else {
                    if let Some(code) = failed_exit_code(pending_focused_exit) {
                        bail!("terminal exited with status {code}");
                    }
                    break;
                };
                let frame = frame?;
                let decode_started = std::time::Instant::now();
                let envelope: Envelope<ServerMessage> = decode_payload(&frame)?;
                if let Some(perf) = perf.as_mut() {
                    perf.record("decode", decode_started.elapsed(), frame.len());
                }
                let request_id = envelope.request_id;
                match envelope.message {
                    ServerMessage::Snapshot { terminal_id, screen } => {
                        let accepted = view.accept(terminal_id, screen);
                        let switching_completed = accepted
                            && terminal_id == view.focused().terminal_id
                            && match surface.as_mut() {
                                Some(ClientSurface::Navigator(nav)) => {
                                    nav.accept_switch_snapshot()
                                }
                                Some(ClientSurface::ProjectOpener(opener)) => {
                                    opener.accept_snapshot()
                                }
                                _ => false,
                            };
                        if switching_completed {
                            surface = None;
                        }
                    }
                    ServerMessage::AlertsChanged { snapshot } => {
                        let waiting_before = resources.snapshot().map_or(0, |snapshot| {
                            resources.notifications().waiting_count(snapshot)
                        });
                        if resources.accept_alerts(snapshot) {
                            let waiting_after = resources.snapshot().map_or(0, |snapshot| {
                                resources.notifications().waiting_count(snapshot)
                            });
                            if ui.alerts.signal_outer_terminal && waiting_after > waiting_before {
                                io::stdout().write_all(b"\x07")?;
                                io::stdout().flush()?;
                            }
                            if let Some(snapshot) = resources.snapshot() {
                                refresh_surface_resources(
                                    &mut surface,
                                    snapshot,
                                    view.focused(),
                                    &workspace_history,
                                    resources.notifications(),
                                );
                            }
                            force_draw = true;
                        }
                    }
                    ServerMessage::SnapshotDelta { terminal_id, delta } => {
                        match view.accept_delta(terminal_id, delta) {
                            DeltaApplyResult::Applied => {
                                let switching_completed = terminal_id == view.focused().terminal_id
                                    && match surface.as_mut() {
                                        Some(ClientSurface::Navigator(nav)) => {
                                            nav.accept_switch_snapshot()
                                        }
                                        Some(ClientSurface::ProjectOpener(opener)) => {
                                            opener.accept_snapshot()
                                        }
                                        _ => false,
                                    };
                                if switching_completed {
                                    surface = None;
                                }
                            }
                            DeltaApplyResult::NeedsRefresh => {
                                send(framed, ClientMessage::RefreshTerminal { terminal_id }).await?;
                            }
                            DeltaApplyResult::Ignored => {}
                        }
                    }
                    ServerMessage::CopyModeSnapshot { terminal_id, screen } => {
                        let accepted = copy_mode.as_mut().is_some_and(|state| {
                            state.complete(
                                terminal_id,
                                request_id,
                                CopyModeReply::Snapshot,
                            )
                        });
                        if accepted {
                            if view.accept(terminal_id, screen) {
                                force_draw = true;
                            }
                            pump_copy_mode(
                                framed,
                                copy_mode.as_mut().expect("accepted copy-mode snapshot"),
                            ).await?;
                        }
                    }
                    ServerMessage::CopyModePrepared {
                        terminal_id,
                        copy_id,
                        text,
                    } => {
                        let accepted = request_id.and_then(|request_id| {
                            let state = copy_mode.as_mut()?;
                            (state.complete(
                                terminal_id,
                                Some(request_id),
                                CopyModeReply::Prepared,
                            )
                                && state.begin_clipboard(request_id, copy_id))
                                .then_some(request_id)
                        });
                        if let Some(request_id) = accepted {
                            spawn_pbcopy(
                                request_id,
                                text,
                                clipboard_results.clone(),
                            );
                            force_draw = true;
                        }
                    }
                    ServerMessage::ExtensionReloadPrepared {
                        base_generation,
                        catalog,
                    } => {
                        let Some(ExtensionReloadState {
                            session_id,
                            phase:
                                ExtensionReloadPhase::Preparing {
                                    request_id: expected,
                                    staged,
                                },
                        }) = extension_reload.take()
                        else {
                            bail!("unexpected extension reload preparation from daemon");
                        };
                        if request_id != Some(expected) {
                            bail!("extension reload preparation used an unexpected request ID");
                        }
                        match staged.materialize(&catalog) {
                            Ok(candidate_ui) => {
                                let commit_request = Uuid::new_v4();
                                send_request(
                                    framed,
                                    Some(commit_request),
                                    ClientMessage::CommitExtensionReload { base_generation },
                                ).await?;
                                extension_reload = Some(ExtensionReloadState {
                                    session_id,
                                    phase: ExtensionReloadPhase::Committing {
                                        request_id: commit_request,
                                        base_generation,
                                        ui: candidate_ui,
                                        catalog,
                                    },
                                });
                            }
                            Err(error) => {
                                toasts.error(format!(
                                    "config reload failed · {}",
                                    one_line_error(&error)
                                ));
                                force_draw = true;
                            }
                        }
                    }
                    ServerMessage::ExtensionsReloaded { catalog, .. } => {
                        let Some(ExtensionReloadState {
                            session_id,
                            phase:
                                ExtensionReloadPhase::Committing {
                                    request_id: expected,
                                    base_generation,
                                    ui: candidate_ui,
                                    catalog: prepared_catalog,
                                },
                        }) = extension_reload.take()
                        else {
                            bail!("unexpected extension reload commit from daemon");
                        };
                        if request_id != Some(expected) {
                            bail!("extension reload commit used an unexpected request ID");
                        }
                        if catalog != prepared_catalog
                            || !matches!(
                                catalog.generation,
                                generation if generation == base_generation
                                    || generation == base_generation.saturating_add(1)
                            )
                        {
                            bail!("daemon committed a different extension catalog than it prepared");
                        }
                        install_ui_config(
                            candidate_ui,
                            framed,
                            terminal.size()?.into(),
                            &mut view,
                            &resources,
                            &mut surface,
                            &workspace_history,
                            &mut prefix,
                            &mut ui,
                            client_hooks,
                        ).await?;
                        extension_generation = catalog.generation;
                        let request_id = Uuid::new_v4();
                        send_request(
                            framed,
                            Some(request_id),
                            ClientMessage::ReloadProjectConfig {
                                session_id,
                            },
                        )
                        .await?;
                        project_config_reload = Some(ProjectConfigReloadState {
                            request_id,
                            kind: ProjectConfigReloadKind::Configuration,
                        });
                        force_draw = true;
                    }
                    ServerMessage::ExtensionCatalogChanged { catalog } => {
                        if catalog.generation <= extension_generation {
                            continue;
                        }
                        // Each attached client stages its own complete local
                        // config on publication, so its command overrides and
                        // bindings advance with the daemon declarations too.
                        let candidate_ui = stage_ui_config(config_location)
                            .with_context(|| {
                                format!(
                                    "stage local UI for daemon extension generation {}",
                                    catalog.generation
                                )
                            })
                            .and_then(|staged| {
                                staged.materialize(&catalog).with_context(|| {
                                    format!(
                                        "materialize daemon extension generation {}",
                                        catalog.generation
                                    )
                                })
                            });
                        let candidate_ui = match candidate_ui {
                            Ok(candidate_ui) => candidate_ui,
                            Err(error) => {
                                // The shared generation is already committed. Keep this
                                // client's last usable UI rather than ending its loop;
                                // a later compatible generation can recover it.
                                extension_generation = catalog.generation;
                                toasts.error(format!(
                                    "extensions reloaded, but this client kept its previous config · {}",
                                    one_line_error(&error)
                                ));
                                force_draw = true;
                                continue;
                            }
                        };
                        install_ui_config(
                            candidate_ui,
                            framed,
                            terminal.size()?.into(),
                            &mut view,
                            &resources,
                            &mut surface,
                            &workspace_history,
                            &mut prefix,
                            &mut ui,
                            client_hooks,
                        ).await?;
                        extension_generation = catalog.generation;
                        toasts.info("extensions reloaded");
                        force_draw = true;
                    }
                    ServerMessage::CopyModeFinalized { terminal_id, screen } => {
                        let accepted = copy_mode.as_mut().is_some_and(|state| {
                            state.complete(
                                terminal_id,
                                request_id,
                                CopyModeReply::Finalized,
                            )
                        });
                        if accepted {
                            let copied_bytes = copy_mode
                                .as_mut()
                                .and_then(CopyModeState::take_copied_bytes);
                            view.accept(terminal_id, screen);
                            copy_mode = None;
                            if let Some(bytes) = copied_bytes {
                                toasts.info(format!("copied {bytes} bytes to clipboard"));
                            }
                            force_draw = true;
                        }
                    }
                    ServerMessage::CopyModeCancelled { terminal_id, screen } => {
                        let accepted = copy_mode.as_mut().is_some_and(|state| {
                            state.complete(
                                terminal_id,
                                request_id,
                                CopyModeReply::Cancelled,
                            )
                        });
                        if accepted {
                            view.accept(terminal_id, screen);
                            copy_mode = None;
                            toasts.info("copy mode cancelled");
                            force_draw = true;
                        }
                    }
                    ServerMessage::CopyModeError { terminal_id, error } => {
                        let disposition = copy_mode.as_mut().map_or(
                            CopyModeErrorDisposition::Ignored,
                            |state| {
                                state.copy_mode_error(
                                    terminal_id,
                                    request_id,
                                    &error,
                                )
                            },
                        );
                        match disposition {
                            CopyModeErrorDisposition::Ignored => {}
                            CopyModeErrorDisposition::Continue => {
                                toasts.error(format!("copy mode · {error}"));
                                pump_copy_mode(
                                    framed,
                                    copy_mode.as_mut().expect("recoverable copy-mode error"),
                                ).await?;
                                force_draw = true;
                            }
                            CopyModeErrorDisposition::Exit => {
                                toasts.error(format!("copy mode · {error}"));
                                copy_mode = None;
                                force_draw = true;
                            }
                        }
                    }
                    ServerMessage::Resources { snapshot, presence } => {
                        let presence_changed = resources.accept_presence(presence);
                        if resources.accept(snapshot) {
                            let snapshot = resources.snapshot().expect("accepted resources exist");
                            client_hooks.observe(snapshot, view.focused());
                            refresh_surface_resources(
                                &mut surface,
                                snapshot,
                                view.focused(),
                                &workspace_history,
                                resources.notifications(),
                            );
                            refresh_surface_presence(&mut surface, resources.presence());
                            reconcile_resource_barriers(
                                snapshot,
                                &mut create,
                                &mut rename,
                            );
                            resize_view(
                                framed,
                                terminal.size()?.into(),
                                &mut view,
                                &resources,
                                &ui,
                            ).await?;
                            force_draw = true;
                        } else if presence_changed {
                            refresh_surface_presence(&mut surface, resources.presence());
                            force_draw = true;
                        }
                    }
                    ServerMessage::ResourcesChanged { snapshot } => {
                        if resources.accept(snapshot) {
                            let snapshot = resources.snapshot().expect("accepted resources exist");
                            client_hooks.observe(snapshot, view.focused());
                            refresh_surface_resources(
                                &mut surface,
                                snapshot,
                                view.focused(),
                                &workspace_history,
                                resources.notifications(),
                            );
                            reconcile_resource_barriers(
                                snapshot,
                                &mut create,
                                &mut rename,
                            );
                            resize_view(
                                framed,
                                terminal.size()?.into(),
                                &mut view,
                                &resources,
                                &ui,
                            ).await?;
                            force_draw = true;
                        }
                    }
                    ServerMessage::PresenceChanged { presence } => {
                        if resources.accept_presence(presence) {
                            refresh_surface_presence(&mut surface, resources.presence());
                            force_draw = true;
                        }
                    }
                    ServerMessage::TargetSelected { selected: target } => {
                        let old_terminal = view.focused().terminal_id;
                        let previous_target = view.focused().clone();
                        let project_open_selected = match surface.as_mut() {
                            Some(ClientSurface::ProjectOpener(opener)) => {
                                opener.selection_received(request_id)
                            }
                            _ => false,
                        };
                        let navigator_selected = match surface.as_mut() {
                            Some(ClientSurface::Navigator(nav)) => nav.switch_selected(request_id),
                            _ => false,
                        };
                        let focus_origin = focus.complete(request_id);
                        let workspace_selected = matches!(focus_origin, Some(FocusOrigin::Workspace));
                        let tab_selected = matches!(focus_origin, Some(FocusOrigin::Tab));
                        let sidebar_selected = matches!(focus_origin, Some(FocusOrigin::Sidebar { .. }));
                        let notification_selected =
                            matches!(focus_origin, Some(FocusOrigin::Notification));
                        let observed_revision = resources.snapshot().map(|snapshot| snapshot.revision);
                        let create_selected =
                            create.selected(request_id, &target, observed_revision);
                        if !view.replace(target)? {
                            if navigator_selected
                                || project_open_selected
                                || workspace_selected
                                || tab_selected
                                || sidebar_selected
                            {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            if create_selected.is_some() {
                                force_draw = true;
                            }
                            continue;
                        }
                        let stale_split_drag = mouse_input
                            .reconcile_ui_drag(view.focused().tab_id, &view.layout);
                        if old_terminal != view.focused().terminal_id {
                            mouse_input.clear();
                        }
                        if stale_split_drag {
                            toasts.info("Layout changed in another client");
                        }
                        workspace_history.record_transition(&previous_target, view.focused());
                        if copy_mode.as_ref().is_some_and(|copy_mode| {
                            copy_mode.terminal_id() != view.focused().terminal_id
                        }) {
                            copy_mode = None;
                            toasts.info("copy mode cancelled · focus changed");
                        }
                        if !resources
                            .snapshot()
                            .is_some_and(|snapshot| view.resources_are_current(snapshot))
                        {
                            send(framed, ClientMessage::ListResources).await?;
                        }
                        pending_focused_exit = None;
                        resize_view(
                            framed,
                            terminal.size()?.into(),
                            &mut view,
                            &resources,
                            &ui,
                        ).await?;
                        if let Some(snapshot) = resources.snapshot() {
                            client_hooks.observe(snapshot, view.focused());
                            refresh_surface_resources(
                                &mut surface,
                                snapshot,
                                view.focused(),
                                &workspace_history,
                                resources.notifications(),
                            );
                        }
                        if navigator_selected && view.focused().terminal_id == old_terminal {
                            surface = None;
                            view.invalidate_drawn();
                        }
                        if project_open_selected && view.focused().terminal_id == old_terminal {
                            surface = None;
                            view.invalidate_drawn();
                        }
                        if workspace_selected {
                            surface = None;
                            view.invalidate_drawn();
                        }
                        if tab_selected {
                            surface = None;
                            view.invalidate_drawn();
                        }
                        if sidebar_selected {
                            surface = None;
                            view.invalidate_drawn();
                        }
                        if notification_selected {
                            view.invalidate_drawn();
                        }
                        if create_selected.is_some() {
                            view.invalidate_drawn();
                        }
                        force_draw = true;
                    }
                    ServerMessage::WorkspaceCreated { selected: target } => {
                        if !create.created(request_id, CreateKind::Workspace, target.terminal_id) {
                            continue;
                        }
                        force_draw = true;
                    }
                    ServerMessage::TabCreated { selected: target } => {
                        if !create.created(request_id, CreateKind::Tab, target.terminal_id) {
                            continue;
                        }
                        if target.terminal_id == view.focused().terminal_id {
                            view.invalidate_drawn();
                        }
                        force_draw = true;
                    }
                    ServerMessage::PaneCreated { selected: target } => {
                        if !create.created(request_id, CreateKind::SplitPane, target.terminal_id) {
                            continue;
                        }
                        force_draw = true;
                    }
                    ServerMessage::PaneMoved { .. } => {
                        // Pane movement is currently a control-plane operation.
                        bail!("unexpected pane movement response")
                    }
                    ServerMessage::ProjectConfigReloaded { .. } => {
                        let Some(reload) = project_config_reload.take() else {
                            bail!("unexpected project configuration reload response");
                        };
                        if request_id != Some(reload.request_id) {
                            bail!("project configuration reload used an unexpected request ID");
                        }
                        toasts.info(reload.kind.success_message());
                        force_draw = true;
                    }
                    ServerMessage::TargetRenamed { resource_revision } => {
                        if rename
                            .as_mut()
                            .is_some_and(|rename| rename.complete(request_id, resource_revision))
                        {
                            rename = None;
                            force_draw = true;
                        }
                    }
                    ServerMessage::TerminalResized { terminal_id, size } => {
                        if view.complete_resize(request_id, terminal_id, size) {
                            force_draw = true;
                        }
                    }
                    ServerMessage::CommandCompleted { command } => {
                        if command == crate::protocol::AcknowledgedCommand::CloseTarget
                            && close_target.complete(request_id)
                        {
                            force_draw = true;
                        }
                    }
                    ServerMessage::LocationOpened { .. } => {
                        let accepted = match surface.as_mut() {
                            Some(ClientSurface::ProjectOpener(opener)) => opener.opened(request_id),
                            _ => false,
                        };
                        if !accepted {
                            bail!("unexpected location-open response on interactive connection");
                        }
                        force_draw = true;
                    }
                    ServerMessage::Pong { .. } => {}
                    ServerMessage::TerminalExited { terminal_id, exit_code } => {
                        if copy_mode
                            .as_ref()
                            .is_some_and(|copy_mode| copy_mode.terminal_id() == terminal_id)
                        {
                            copy_mode = None;
                        }
                        if terminal_id == view.focused().terminal_id {
                            if mouse_input.cancel_split_drag() {
                                toasts.info("Layout changed in another client");
                            }
                            mouse_input.clear();
                            pending_focused_exit = Some(exit_code);
                            force_draw = true;
                            continue;
                        }
                        view.remove(terminal_id);
                        if mouse_input.reconcile_ui_drag(view.focused().tab_id, &view.layout) {
                            toasts.info("Layout changed in another client");
                        }
                        force_draw = true;
                    }
                    ServerMessage::Detached => break,
                    ServerMessage::Error { code, message } => {
                        let reload_failed = extension_reload
                            .as_ref()
                            .is_some_and(|reload| request_id == Some(reload.request_id()));
                        if reload_failed {
                            extension_reload = None;
                            toasts.error(format!("config reload failed · {message}"));
                            force_draw = true;
                            continue;
                        }
                        let project_reload_failed = project_config_reload
                            .as_ref()
                            .is_some_and(|reload| request_id == Some(reload.request_id));
                        if project_reload_failed {
                            let reload = project_config_reload.take().expect("checked reload");
                            let prefix = reload.kind.error_message();
                            toasts.error(format!("{prefix} · {message}"));
                            force_draw = true;
                            continue;
                        }
                        if view.reject_resize(request_id) {
                            continue;
                        }
                        let copy_failure = copy_mode
                            .as_mut()
                            .map_or(CopyModeErrorDisposition::Ignored, |copy_mode| {
                                copy_mode.fail(request_id)
                            });
                        match copy_failure {
                            CopyModeErrorDisposition::Ignored => {}
                            CopyModeErrorDisposition::Continue => {
                                toasts.error(format!("copy mode failed · {message}"));
                                pump_copy_mode(
                                    framed,
                                    copy_mode.as_mut().expect("recoverable copy-mode failure"),
                                ).await?;
                                force_draw = true;
                                continue;
                            }
                            CopyModeErrorDisposition::Exit => {
                                copy_mode = None;
                                toasts.error(format!("copy mode failed · {message}"));
                                force_draw = true;
                                continue;
                            }
                        }
                        if rename
                            .as_mut()
                            .is_some_and(|rename| rename.fail(request_id, message.clone()))
                        {
                            force_draw = true;
                            continue;
                        }
                        if let Some(kind) = create.fail(request_id) {
                            toasts.error(format!("{} failed · {message}", kind.label()));
                            force_draw = true;
                            continue;
                        }
                        if close_target.fail(request_id) {
                            toasts.error(format!("close failed · {message}"));
                            force_draw = true;
                            continue;
                        }
                        let project_open_failed = match surface.as_mut() {
                            Some(ClientSurface::ProjectOpener(opener)) => {
                                opener.fail(request_id, message.clone())
                            }
                            _ => false,
                        };
                        if project_open_failed {
                            force_draw = true;
                            continue;
                        }
                        let handled = match surface.as_mut() {
                            Some(ClientSurface::Navigator(nav)) => {
                                nav.switch_error(request_id, message.clone())
                            }
                            _ => false,
                        };
                        if handled {
                            force_draw = true;
                        } else {
                            match focus.complete(request_id) {
                                Some(FocusOrigin::Sidebar { scope, component }) => {
                                    if component == SidebarComponentKind::Workspaces
                                        && let Some(ClientSurface::Sidebar(sidebar)) = surface.as_mut()
                                        && sidebar.switch_error(message.clone())
                                    {
                                        force_draw = true;
                                        continue;
                                    }
                                    let label = match component {
                                        SidebarComponentKind::Workspaces => "workspace".into(),
                                        SidebarComponentKind::Agents => {
                                            format!("{} agent", scope.label())
                                        }
                                    };
                                    toasts.error(format!("{label} unavailable · {message}"));
                                    force_draw = true;
                                }
                                Some(FocusOrigin::Workspace) => {
                                    toasts.error(format!("workspace unavailable · {message}"));
                                    force_draw = true;
                                }
                                Some(FocusOrigin::Pane) => {
                                    toasts.error(format!("pane unavailable · {message}"));
                                    force_draw = true;
                                }
                                Some(FocusOrigin::Tab) => {
                                    toasts.error(format!("tab unavailable · {message}"));
                                    force_draw = true;
                                }
                                Some(FocusOrigin::Session) => {
                                    toasts.error(format!("session unavailable · {message}"));
                                    force_draw = true;
                                }
                                Some(FocusOrigin::Notification) => {
                                    toasts.error(format!("notification unavailable · {message}"));
                                    force_draw = true;
                                }
                                None => bail!("daemon error ({code}): {message}"),
                            }
                        }
                    }
                    ServerMessage::IncompatibleProtocol { client, server } => {
                        bail!("protocol became incompatible: client {client}, server {server}")
                    }
                    ServerMessage::Welcome { .. } => bail!("unexpected second welcome from daemon"),
                    ServerMessage::TerminalOutput { .. }
                    | ServerMessage::TerminalOutputMatched { .. }
                    | ServerMessage::AgentPrompted { .. }
                    | ServerMessage::AgentSettled { .. }
                    | ServerMessage::ExtensionCatalog { .. }
                    | ServerMessage::TokenPublished { .. } => {
                        bail!("unexpected control response on interactive connection")
                    }
                }
            }
            event = events.next(), if accepts_client_input(&focus, &create, &close_target, &pending_focused_exit) => {
                let Some(event) = event else {
                    release_captured_mouse_input(
                        framed,
                        &mut mouse_input,
                        view.focused().terminal_id,
                    ).await?;
                    break
                };
                let event = event?;
                if !matches!(&event, Event::Mouse(_)) {
                    mouse_input.cancel_local_drags();
                }
                match close_target.route_confirmation_input(&event) {
                    CloseConfirmationInput::Pass => {}
                    CloseConfirmationInput::Confirm {
                        request_id,
                        selector,
                    } => {
                        toasts.clear();
                        send_request(
                            framed,
                            Some(request_id),
                            ClientMessage::CloseTarget { selector },
                        )
                        .await?;
                        force_draw = true;
                        continue;
                    }
                    CloseConfirmationInput::Cancel => {
                        toasts.clear();
                        force_draw = true;
                        continue;
                    }
                    CloseConfirmationInput::Consume => continue,
                }
                match event {
                    Event::Key(key) if temporary_command.is_some() => {
                        let command = temporary_command.as_mut().expect("command exists");
                        if let Some(event) = terminal_key_event(key) {
                            command.key_input(event).await?;
                        } else if let Some(bytes) = encode_key(key) {
                            command.input(bytes).await?;
                        }
                    }
                    Event::Paste(text) if temporary_command.is_some() => {
                        temporary_command.as_mut().expect("command exists").paste(text).await?;
                    }
                    Event::Resize(columns, rows) if temporary_command.is_some() && columns > 0 && rows > 0 => {
                        let host = Rect::new(0, 0, columns, rows);
                        let command = temporary_command.as_mut().expect("command exists");
                        let content = temporary_command_content(command.size().area(host));
                        command.resize(TerminalSize {
                            columns: content.width,
                            rows: content.height,
                        }).await?;
                        force_draw = true;
                    }
                    Event::Mouse(mouse) if temporary_command.is_some() => {
                        let host: Rect = terminal.size()?.into();
                        let command = temporary_command.as_mut().expect("command exists");
                        let content = temporary_command_content(command.size().area(host));
                        match command.mouse(mouse, content).await {
                            Ok(Some(copy)) => {
                                spawn_pbcopy(copy.request_id, copy.text, clipboard_results.clone());
                            }
                            Ok(None) => {}
                            Err(error) => {
                                command.cancel_mouse_copy().await;
                                toasts.error(format!("COPY FAILED · {}", one_line_error(&error)));
                            }
                        }
                        force_draw = true;
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::CommandForm(_))) => {
                        toasts.clear();
                        let action = match surface.as_mut().expect("command form exists") {
                            ClientSurface::CommandForm(form) => form.key(key),
                            _ => unreachable!("surface guard ensures command form"),
                        };
                        match action {
                            CommandFormAction::Stay => {}
                            CommandFormAction::Cancel => surface = None,
                            CommandFormAction::Submit => {
                                let Some(ClientSurface::CommandForm(form)) = surface.take() else {
                                    unreachable!("surface guard ensures command form")
                                };
                                let submission = form.submit();
                                let host: Rect = terminal.size()?.into();
                                match spawn_temporary_command(
                                    &submission.command,
                                    view.focused().child_pid,
                                    host,
                                    socket_path,
                                    Some(&submission.context),
                                    Some(&submission.values),
                                ).await {
                                    Ok(command) => temporary_command = Some(command),
                                    Err(error) => toasts.error(format!("command failed · {}", one_line_error(&error))),
                                }
                            }
                        }
                        force_draw = true;
                    }
                    Event::Paste(text) if matches!(surface.as_ref(), Some(ClientSurface::CommandForm(_))) => {
                        if let Some(ClientSurface::CommandForm(form)) = surface.as_mut() {
                            form.paste(&text);
                            force_draw = true;
                        }
                    }
                    Event::Key(key) if copy_mode.is_some() => {
                        toasts.clear();
                        let input = copy_mode.as_mut().expect("copy mode exists").key(key);
                        match input {
                            CopyModeInput::Stay => {}
                            CopyModeInput::Pump => {
                                pump_copy_mode(
                                    framed,
                                    copy_mode.as_mut().expect("copy mode exists"),
                                ).await?;
                            }
                            CopyModeInput::Notice(message) => toasts.error(message),
                        }
                        force_draw = true;
                    }
                    Event::Paste(text) if copy_mode.is_some() => {
                        toasts.clear();
                        if matches!(
                            copy_mode
                            .as_mut()
                            .expect("copy mode exists")
                            .paste(&text),
                            CopyModePaste::TooLarge
                        ) {
                            toasts.error("search query is too large; paste was not added");
                        }
                        force_draw = true;
                    }
                    Event::Mouse(mouse)
                        if copy_mode
                            .as_ref()
                            .is_some_and(CopyModeState::is_mouse_dragging) =>
                    {
                        let host = terminal.size()?.into();
                        let layout = client_layout(
                            host,
                            &ui,
                            resources.sidebar_relevance(view.focused(), &ui),
                        );
                        let state = copy_mode.as_mut().expect("mouse copy mode exists");
                        let terminal_id = state.terminal_id();
                        let input = match mouse.kind {
                            HostMouseEventKind::Drag(HostMouseButton::Left) => view
                                .copy_cell(
                                    layout.terminal,
                                    ui.pane_layout,
                                    terminal_id,
                                    mouse.column,
                                    mouse.row,
                                    true,
                                )
                                .map_or(CopyModeInput::Stay, |(column, row)| {
                                    state.mouse_drag(column, row)
                                }),
                            HostMouseEventKind::Up(HostMouseButton::Left) => {
                                let position = view.copy_cell(
                                    layout.terminal,
                                    ui.pane_layout,
                                    terminal_id,
                                    mouse.column,
                                    mouse.row,
                                    true,
                                );
                                match position {
                                    Some((column, row)) => state.mouse_release(column, row),
                                    None => state.mouse_release_current(),
                                }
                            }
                            _ => CopyModeInput::Stay,
                        };
                        mouse_input.discard(mouse);
                        match input {
                            CopyModeInput::Stay => {}
                            CopyModeInput::Pump => {
                                pump_copy_mode(framed, state).await?;
                            }
                            CopyModeInput::Notice(message) => toasts.error(message),
                        }
                        force_draw = true;
                    }
                    Event::Key(key) if rename.is_some() => {
                        let action = rename.as_mut().expect("rename exists").key(key);
                        match action {
                            RenameAction::Stay => force_draw = true,
                            RenameAction::Close => {
                                rename = None;
                                force_draw = true;
                            }
                            RenameAction::Submit { request_id, selector, name } => {
                                if let Some(snapshot) = resources.snapshot() {
                                    rename
                                        .as_mut()
                                        .expect("submitted rename exists")
                                        .accept_resources(snapshot);
                                }
                                send_request(
                                    framed,
                                    Some(request_id),
                                    ClientMessage::RenameTarget { selector, name },
                                ).await?;
                                force_draw = true;
                            }
                        }
                    }
                    Event::Paste(text) if rename.is_some() => {
                        rename.as_mut().expect("rename exists").paste(&text);
                        force_draw = true;
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::ContextMenu(_))) => {
                        toasts.clear();
                        let action = match surface.as_mut().expect("context menu exists") {
                            ClientSurface::ContextMenu(menu) => menu.key(key),
                            _ => unreachable!("surface guard ensures context menu"),
                        };
                        dispatch_context_menu_action(
                            action,
                            framed,
                            &mut view,
                            &resources,
                            &mut surface,
                            &mut rename,
                            &mut create,
                            &mut close_target,
                            &mut focus,
                            &mut mouse_input,
                            &mut ui,
                            &mut toasts,
                            terminal.size()?.into(),
                        ).await?;
                        force_draw = true;
                    }
                    Event::Mouse(mouse) if matches!(surface.as_ref(), Some(ClientSurface::ContextMenu(_))) => {
                        let host = terminal.size()?.into();
                        let action = match surface.as_mut().expect("context menu exists") {
                            ClientSurface::ContextMenu(menu) => match mouse.kind {
                                HostMouseEventKind::Moved => {
                                    menu.mouse_move(host, mouse.column, mouse.row);
                                    ContextMenuAction::Stay
                                }
                                HostMouseEventKind::Down(HostMouseButton::Left) => {
                                    menu.click(host, mouse.column, mouse.row)
                                }
                                _ => ContextMenuAction::Stay,
                            },
                            _ => unreachable!("surface guard ensures context menu"),
                        };
                        mouse_input.discard(mouse);
                        dispatch_context_menu_action(
                            action,
                            framed,
                            &mut view,
                            &resources,
                            &mut surface,
                            &mut rename,
                            &mut create,
                            &mut close_target,
                            &mut focus,
                            &mut mouse_input,
                            &mut ui,
                            &mut toasts,
                            host,
                        ).await?;
                        force_draw = true;
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::ProjectOpener(_))) => {
                        toasts.clear();
                        let size = terminal.size()?;
                        let visible = project_opener::dialog_body_rows(Rect::new(0, 0, size.width, size.height));
                        let action = project_opener_mut(&mut surface).key(key, visible);
                        match action {
                            ProjectOpenerAction::Stay => {}
                            ProjectOpenerAction::Close => {
                                surface = None;
                                view.invalidate_drawn();
                            }
                            ProjectOpenerAction::Submit(choice) => {
                                let task = ProjectPreparationTask::spawn(
                                    choice,
                                    config_location.clone(),
                                    ui.extensions.clone(),
                                    project_preparation_results.clone(),
                                );
                                project_opener_mut(&mut surface)
                                    .begin_preparation(task.request_id);
                                project_preparation = Some(task);
                            }
                            ProjectOpenerAction::CancelPreparation { request_id } => {
                                if project_opener_mut(&mut surface)
                                    .cancel_preparation(request_id)
                                    && project_preparation
                                        .as_ref()
                                        .is_some_and(|task| task.is_for(request_id))
                                {
                                    project_preparation = None;
                                }
                            }
                            ProjectOpenerAction::Approve { project, cwd, digest } => {
                                match approve_project_recipe(
                                    &project,
                                    &digest,
                                    config_location,
                                    &ui.extensions,
                                ) {
                                    Ok(_) => {
                                        let opener = project_opener_mut(&mut surface);
                                        request_project_open(framed, opener, Some(project), cwd).await?;
                                    }
                                    Err(error) => {
                                        project_opener_mut(&mut surface).show_error(format!(
                                            "trust failed · {}",
                                            one_line_error(&error)
                                        ));
                                    }
                                }
                            }
                        }
                        force_draw = true;
                    }
                    Event::Paste(text) if matches!(surface.as_ref(), Some(ClientSurface::ProjectOpener(_))) => {
                        if let Some(ClientSurface::ProjectOpener(opener)) = surface.as_mut() {
                            opener.paste(&text);
                            force_draw = true;
                        }
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::Notifications(_))) => {
                        toasts.clear();
                        let size = terminal.size()?;
                        let visible = notifications::dialog_body_rows(Rect::new(0, 0, size.width, size.height));
                        let action = match surface.as_mut().expect("notifications exist") {
                            ClientSurface::Notifications(dialog) => dialog.key(key, visible),
                            _ => unreachable!("surface guard ensures notifications"),
                        };
                        match action {
                            NotificationsAction::Stay => force_draw = true,
                            NotificationsAction::Close => {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            NotificationsAction::Select(pane_id) => {
                                release_captured_mouse_input(
                                    framed,
                                    &mut mouse_input,
                                    view.focused().terminal_id,
                                ).await?;
                                surface = None;
                                view.invalidate_drawn();
                                if let Some(request) = focus.begin(FocusOrigin::Notification) {
                                    send_request(
                                        framed,
                                        Some(request),
                                        ClientMessage::SelectTarget {
                                            selector: TargetSelector::Pane(pane_id),
                                            expected: None,
                                        },
                                    ).await?;
                                }
                                force_draw = true;
                            }
                            NotificationsAction::Acknowledge(acknowledgement) => {
                                acknowledge_attention(framed, acknowledgement).await?;
                                force_draw = true;
                            }
                        }
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::Navigator(_))) => {
                        toasts.clear();
                        let size = terminal.size()?;
                        let visible = navigator::dialog_body_rows(Rect::new(0, 0, size.width, size.height));
                        let action = match surface.as_mut().expect("navigator exists") {
                            ClientSurface::Navigator(nav) => nav.key(key, visible),
                            _ => unreachable!("surface guard ensures navigator"),
                        };
                        match action {
                            NavigatorAction::Stay => force_draw = true,
                            NavigatorAction::Close => {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            NavigatorAction::Select(selector) => {
                                release_captured_mouse_input(
                                    framed,
                                    &mut mouse_input,
                                    view.focused().terminal_id,
                                ).await?;
                                let request = Uuid::new_v4();
                                match surface.as_mut().expect("navigator exists") {
                                    ClientSurface::Navigator(nav) => nav.begin_switch(request),
                                    _ => unreachable!("surface guard ensures navigator"),
                                }
                                send_request(
                                    framed,
                                    Some(request),
                                    ClientMessage::SelectTarget {
                                        selector,
                                        expected: None,
                                    },
                                ).await?;
                                force_draw = true;
                            }
                        }
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::Agents(_))) => {
                        toasts.clear();
                        let size = terminal.size()?;
                        let visible = agent_dialog::dialog_body_rows(Rect::new(0, 0, size.width, size.height));
                        let action = match surface.as_mut().expect("agents exist") {
                            ClientSurface::Agents(dialog) => dialog.key(key, visible),
                            _ => unreachable!("surface guard ensures agents"),
                        };
                        match action {
                            AgentsAction::Stay => force_draw = true,
                            AgentsAction::Close => {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            AgentsAction::Select(pane_id) => {
                                release_captured_mouse_input(
                                    framed,
                                    &mut mouse_input,
                                    view.focused().terminal_id,
                                ).await?;
                                surface = None;
                                view.invalidate_drawn();
                                if let Some(request) = focus.begin(FocusOrigin::Sidebar {
                                    scope: NavigationScope::Global,
                                    component: SidebarComponentKind::Agents,
                                }) {
                                    send_request(
                                        framed,
                                        Some(request),
                                        ClientMessage::SelectTarget {
                                            selector: TargetSelector::Pane(pane_id),
                                            expected: None,
                                        },
                                    ).await?;
                                }
                                force_draw = true;
                            }
                        }
                    }
                    Event::Paste(text) if matches!(surface.as_ref(), Some(ClientSurface::Agents(_))) => {
                        if let Some(ClientSurface::Agents(dialog)) = surface.as_mut() {
                            dialog.paste(&text);
                            force_draw = true;
                        }
                    }
                    Event::Paste(text) if matches!(surface.as_ref(), Some(ClientSurface::Navigator(_))) => {
                        if let Some(ClientSurface::Navigator(navigator)) = surface.as_mut() {
                            navigator.paste(&text);
                            force_draw = true;
                        }
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::Sidebar(_))) => {
                        toasts.clear();
                        let size = terminal.size()?;
                        let action = match surface.as_mut().expect("sidebar exists") {
                            ClientSurface::Sidebar(sidebar) => {
                                let host = Rect::new(0, 0, size.width, size.height);
                                let area = sidebar_drawer(host, &ui, sidebar.side())
                                    .unwrap_or(Rect::new(0, 0, 0, 0));
                                sidebar.key(key, area, &ui)
                            }
                            _ => unreachable!("surface guard ensures sidebar"),
                        };
                        match action {
                            ComponentEffect::Stay => force_draw = true,
                            ComponentEffect::CloseSidebar => {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            ComponentEffect::CreateWorkspace => {
                                if let Some(request) = create.begin(CreateKind::Workspace) {
                                    send_request(
                                        framed,
                                        Some(request),
                                        ClientMessage::CreateWorkspace {
                                            session_id: view.focused().session_id,
                                            name: None,
                                            cwd: None,
                                            program: None,
                                            argv: Vec::new(),
                                        },
                                    ).await?;
                                }
                                force_draw = true;
                            }
                            ComponentEffect::CycleVisibility => {
                                if let ClientSurface::Sidebar(sidebar) =
                                    surface.as_ref().expect("sidebar exists")
                                {
                                    sidebar.side().config_mut(&mut ui).visibility.cycle();
                                }
                                resize_view(
                                    framed,
                                    terminal.size()?.into(),
                                    &mut view,
                                    &resources,
                                    &ui,
                                ).await?;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            ComponentEffect::ToggleDisplay => {
                                if let ClientSurface::Sidebar(sidebar) =
                                    surface.as_ref().expect("sidebar exists")
                                {
                                    sidebar.side().config_mut(&mut ui).display.toggle();
                                }
                                resize_view(
                                    framed,
                                    terminal.size()?.into(),
                                    &mut view,
                                    &resources,
                                    &ui,
                                ).await?;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            ComponentEffect::RenameWorkspace(workspace_id, name) => {
                                rename = Some(RenameState::open(
                                    crate::protocol::RenameSelector::Workspace(workspace_id),
                                    "workspace",
                                    name,
                                ));
                                force_draw = true;
                            }
                            ComponentEffect::Navigate(pane_id, scope, component) => {
                                release_captured_mouse_input(
                                    framed,
                                    &mut mouse_input,
                                    view.focused().terminal_id,
                                ).await?;
                                if let Some(request) = focus.begin(FocusOrigin::Sidebar {
                                    scope,
                                    component,
                                }) {
                                    match surface.as_mut().expect("sidebar exists") {
                                        ClientSurface::Sidebar(sidebar) => sidebar.begin_switch(),
                                        _ => unreachable!("surface guard ensures sidebar"),
                                    }
                                    send_request(
                                        framed,
                                        Some(request),
                                        ClientMessage::SelectTarget {
                                            selector: TargetSelector::Pane(pane_id),
                                            expected: resources.snapshot().and_then(|snapshot| {
                                                selection_expectation(
                                                    snapshot,
                                                    pane_id,
                                                    scope,
                                                )
                                            }),
                                        },
                                    )
                                    .await?;
                                    force_draw = true;
                                }
                            }
                        }
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::TabBar(_))) => {
                        toasts.clear();
                        let action = match surface.as_mut().expect("tab bar exists") {
                            ClientSurface::TabBar(tab_bar) => tab_bar.key(key),
                            _ => unreachable!("surface guard ensures tab bar"),
                        };
                        match action {
                            TabBarAction::Stay => force_draw = true,
                            TabBarAction::Close => {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            TabBarAction::Create => {
                                if let Some(request) = create.begin(CreateKind::Tab) {
                                    send_request(
                                        framed,
                                        Some(request),
                                        ClientMessage::CreateTab {
                                            workspace_id: view.focused().workspace_id,
                                            name: None,
                                            cwd: None,
                                            program: None,
                                            argv: Vec::new(),
                                        },
                                    ).await?;
                                }
                                force_draw = true;
                            }
                            TabBarAction::Rename(tab_id, name) => {
                                rename = Some(RenameState::open(
                                    crate::protocol::RenameSelector::Tab(tab_id),
                                    "tab",
                                    name,
                                ));
                                force_draw = true;
                            }
                            TabBarAction::Select(pane_id) => {
                                release_captured_mouse_input(
                                    framed,
                                    &mut mouse_input,
                                    view.focused().terminal_id,
                                ).await?;
                                if let Some(request) = focus.begin(FocusOrigin::Tab) {
                                    send_request(
                                        framed,
                                        Some(request),
                                        ClientMessage::SelectTarget {
                                            selector: TargetSelector::Pane(pane_id),
                                            expected: resources.snapshot().and_then(|snapshot| {
                                                selection_expectation(
                                                    snapshot,
                                                    pane_id,
                                                    NavigationScope::Tab,
                                                )
                                            }),
                                        },
                                    ).await?;
                                    force_draw = true;
                                }
                            }
                        }
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::CommandBar(_))) => {
                        toasts.clear();
                        let action = match surface.as_mut().expect("command bar exists") {
                            ClientSurface::CommandBar(command_bar) => command_bar.key(key),
                            _ => unreachable!("surface guard ensures command bar"),
                        };
                        match action {
                            CommandBarAction::Stay => force_draw = true,
                            CommandBarAction::Close => {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            CommandBarAction::Dispatch(action) => {
                                release_captured_mouse_input(
                                    framed,
                                    &mut mouse_input,
                                    view.focused().terminal_id,
                                ).await?;
                                surface = None;
                                view.invalidate_drawn();
                                toasts.replace(dispatch_client_action(
                                    action,
                                    framed,
                                    &mut view,
                                    &resources,
                                    &mut surface,
                                    &workspace_history,
                                    &mut create,
                                    &mut close_target,
                                    &mut focus,
                                    &mut copy_mode,
                                    &mut rename,
                                    terminal.size()?.into(),
                                    &mut ui,
                                    &mut temporary_command,
                                    socket_path,
                                    &background_results,
                                    config_location,
                                    &mut extension_reload,
                                    &mut project_config_reload,
                                    true,
                                ).await?);
                                force_draw = true;
                            }
                        }
                    }
                    Event::Paste(text) if matches!(surface.as_ref(), Some(ClientSurface::CommandBar(_))) => {
                        if let Some(ClientSurface::CommandBar(command_bar)) = surface.as_mut() {
                            command_bar.paste(&text);
                            force_draw = true;
                        }
                    }
                    Event::Mouse(mouse) => {
                        let host = terminal.size()?.into();
                        let layout = client_layout(
                            host,
                            &ui,
                            resources.sidebar_relevance(view.focused(), &ui),
                        );
                        let unobstructed = app_overlay_clear(
                            surface.is_none(),
                            rename.is_none(),
                            copy_mode.is_none(),
                            cheatsheet_visible,
                        );
                        let open_sidebar = rename.is_none()
                            && copy_mode.is_none()
                            && matches!(surface.as_ref(), Some(ClientSurface::Sidebar(_)));
                        let visible_sidebars = if open_sidebar {
                            surface
                                .as_ref()
                                .and_then(|surface| match surface {
                                    ClientSurface::Sidebar(sidebar) => Some(sidebar.side()),
                                    _ => None,
                                })
                                .and_then(|side| {
                                    sidebar_drawer(host, &ui, side).and_then(|area| {
                                        sidebar_divider(area, side, MAX_SIDEBAR_WIDTH)
                                    })
                                })
                                .into_iter()
                                .collect::<Vec<_>>()
                        } else if unobstructed {
                            SidebarSide::ALL
                                .into_iter()
                                .filter(|side| {
                                    side.config(&ui).display != SidebarDisplay::Minimized
                                })
                                .filter_map(|side| {
                                    let area = layout.sidebar(side)?.docked()?;
                                    let max_width =
                                        docked_sidebar_max_width(host, layout, side);
                                    sidebar_divider(area, side, max_width)
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let split_dividers = if unobstructed {
                            view.pane_layouts(layout.terminal, ui.pane_layout).1
                        } else {
                            Vec::new()
                        };
                        if matches!(mouse.kind, HostMouseEventKind::Down(HostMouseButton::Right))
                            && matches!(
                                surface.as_ref(),
                                None
                                    | Some(ClientSurface::Sidebar(_))
                                    | Some(ClientSurface::TabBar(_))
                            )
                            && let Some(snapshot) = resources.snapshot()
                        {
                            let anchor = (mouse.column, mouse.row);
                            let (sidebar_hit, workspace_target) = if let Some(
                                ClientSurface::Sidebar(sidebar),
                            ) = surface.as_ref()
                            {
                                let area = sidebar_drawer(host, &ui, sidebar.side());
                                let hit = area.is_some_and(|area| {
                                    rect_contains(area, mouse.column, mouse.row)
                                });
                                let target = area
                                    .filter(|_| hit)
                                    .and_then(|area| {
                                        sidebar.workspace_item_id_at(
                                            area,
                                            &ui,
                                            mouse.column,
                                            mouse.row,
                                        )
                                    })
                                    .map(|workspace_id| (sidebar.side(), workspace_id));
                                (hit, target)
                            } else {
                                let hit = SidebarSide::ALL.into_iter().find_map(|side| {
                                    let area = layout.sidebar(side)?.docked()?;
                                    rect_contains(area, mouse.column, mouse.row)
                                        .then_some((side, area))
                                });
                                let target = hit.and_then(|(side, area)| {
                                    SidebarState::open(
                                        snapshot,
                                        view.focused(),
                                        &workspace_history,
                                        resources.notifications(),
                                        side,
                                        &ui,
                                    )?
                                    .workspace_item_id_at(area, &ui, mouse.column, mouse.row)
                                    .map(|workspace_id| (side, workspace_id))
                                });
                                (hit.is_some(), target)
                            };
                            let workspace_menu = workspace_target.and_then(|(side, workspace_id)| {
                                let sidebar = side.config(&ui);
                                ContextMenuState::for_workspace(
                                    snapshot,
                                    view.focused(),
                                    &workspace_history,
                                    workspace_id,
                                    side,
                                    sidebar,
                                    anchor,
                                )
                            });
                            let menu = if sidebar_hit {
                                workspace_menu
                            } else {
                                workspace_menu.or_else(|| {
                                    let area = layout.tab_bar?;
                                    let tab_id = TabBarState::item_at(
                                        snapshot,
                                        view.focused(),
                                        view.is_zoomed(),
                                        &ui,
                                        resources.notifications(),
                                        spinner_frame,
                                        area,
                                        mouse.column,
                                        mouse.row,
                                    )?;
                                    ContextMenuState::for_tab(
                                        snapshot,
                                        view.focused(),
                                        &workspace_history,
                                        tab_id,
                                        anchor,
                                    )
                                })
                            };
                            if let Some(menu) = menu {
                                mouse_input.discard(mouse);
                                surface = Some(ClientSurface::ContextMenu(menu));
                                force_draw = true;
                                continue;
                            }
                            if sidebar_hit {
                                mouse_input.discard(mouse);
                                force_draw = true;
                                continue;
                            }
                        }
                        let activation = matches!(mouse.kind, HostMouseEventKind::Down(HostMouseButton::Left))
                            .then(|| {
                                if visible_sidebars.iter().any(|divider| {
                                    rect_contains(divider.area, mouse.column, mouse.row)
                                }) {
                                    return None;
                                }
                                let snapshot = resources.snapshot()?;
                                if let Some(ClientSurface::Sidebar(sidebar)) = surface.as_mut() {
                                    let area = sidebar_drawer(host, &ui, sidebar.side())?;
                                    if rect_contains(area, mouse.column, mouse.row) {
                                        return Some(UiActivation::Sidebar(sidebar.click(
                                            area,
                                            &ui,
                                            mouse.column,
                                            mouse.row,
                                        )));
                                    }
                                } else if surface.is_none() {
                                    for side in SidebarSide::ALL {
                                        let Some(area) = layout
                                            .sidebar(side)
                                            .and_then(|sidebar| sidebar.docked())
                                        else {
                                            continue;
                                        };
                                        if !rect_contains(area, mouse.column, mouse.row) {
                                            continue;
                                        }
                                        let sidebar = SidebarState::open(
                                            snapshot,
                                            view.focused(),
                                            &workspace_history,
                                            resources.notifications(),
                                            side,
                                            &ui,
                                        )?;
                                        return Some(UiActivation::Sidebar(sidebar.passive_click(
                                            area,
                                            &ui,
                                            mouse.column,
                                            mouse.row,
                                        )));
                                    }
                                }
                                if matches!(surface.as_ref(), None | Some(ClientSurface::TabBar(_)))
                                    && let Some(area) = layout.tab_bar
                                    && rect_contains(area, mouse.column, mouse.row)
                                {
                                    let action = if let Some(ClientSurface::TabBar(tab_bar)) = surface.as_ref() {
                                        tab_bar.click(
                                            snapshot,
                                            view.focused(),
                                            view.is_zoomed(),
                                            &ui,
                                            resources.notifications(),
                                            spinner_frame,
                                            area,
                                            mouse.column,
                                            mouse.row,
                                        )
                                    } else {
                                        TabBarState::open(snapshot, view.focused(), &workspace_history)?.click(
                                            snapshot,
                                            view.focused(),
                                            view.is_zoomed(),
                                            &ui,
                                            resources.notifications(),
                                            spinner_frame,
                                            area,
                                            mouse.column,
                                            mouse.row,
                                        )
                                    };
                                    return Some(UiActivation::Tab(action));
                                }
                                None
                            })
                            .flatten();
                        if let Some(activation) = activation {
                            mouse_input.discard(mouse);
                            match activation {
                                UiActivation::Sidebar(ComponentEffect::Navigate(
                                    pane_id,
                                    scope,
                                    component,
                                )) => {
                                    if let Some(request) = focus.begin(FocusOrigin::Sidebar {
                                        scope,
                                        component,
                                    }) {
                                        if let Some(ClientSurface::Sidebar(sidebar)) = surface.as_mut() {
                                            sidebar.begin_switch();
                                        }
                                        send_request(
                                            framed,
                                            Some(request),
                                            ClientMessage::SelectTarget {
                                                selector: TargetSelector::Pane(pane_id),
                                                expected: selection_expectation(
                                                    resources.snapshot().expect("activation requires resources"),
                                                    pane_id,
                                                    scope,
                                                ),
                                            },
                                        ).await?;
                                    }
                                }
                                UiActivation::Tab(TabBarAction::Select(pane_id)) => {
                                    if let Some(request) = focus.begin(FocusOrigin::Tab) {
                                        send_request(
                                            framed,
                                            Some(request),
                                            ClientMessage::SelectTarget {
                                                selector: TargetSelector::Pane(pane_id),
                                                expected: selection_expectation(
                                                    resources.snapshot().expect("activation requires resources"),
                                                    pane_id,
                                                    NavigationScope::Tab,
                                                ),
                                            },
                                        ).await?;
                                    }
                                }
                                UiActivation::Tab(TabBarAction::Create) => {
                                    if let Some(request) = create.begin(CreateKind::Tab) {
                                        send_request(
                                            framed,
                                            Some(request),
                                            ClientMessage::CreateTab {
                                                workspace_id: view.focused().workspace_id,
                                                name: None,
                                                cwd: None,
                                                program: None,
                                                argv: Vec::new(),
                                            },
                                        ).await?;
                                    }
                                }
                                UiActivation::Tab(TabBarAction::Rename(tab_id, name)) => {
                                    rename = Some(RenameState::open(
                                        crate::protocol::RenameSelector::Tab(tab_id),
                                        "tab",
                                        name,
                                    ));
                                }
                                UiActivation::Sidebar(ComponentEffect::CycleVisibility) => {
                                    if let Some(ClientSurface::Sidebar(sidebar)) = surface.as_ref() {
                                        sidebar.side().config_mut(&mut ui).visibility.cycle();
                                    }
                                    resize_view(framed, host, &mut view, &resources, &ui).await?;
                                    view.invalidate_drawn();
                                }
                                UiActivation::Sidebar(ComponentEffect::ToggleDisplay) => {
                                    if let Some(ClientSurface::Sidebar(sidebar)) = surface.as_ref() {
                                        sidebar.side().config_mut(&mut ui).display.toggle();
                                    }
                                    resize_view(framed, host, &mut view, &resources, &ui).await?;
                                    view.invalidate_drawn();
                                }
                                UiActivation::Sidebar(ComponentEffect::CreateWorkspace) => {
                                    if let Some(request) = create.begin(CreateKind::Workspace) {
                                        send_request(
                                            framed,
                                            Some(request),
                                            ClientMessage::CreateWorkspace {
                                                session_id: view.focused().session_id,
                                                name: None,
                                                cwd: None,
                                                program: None,
                                                argv: Vec::new(),
                                            },
                                        )
                                        .await?;
                                    }
                                }
                                UiActivation::Sidebar(ComponentEffect::RenameWorkspace(
                                    workspace_id,
                                    name,
                                )) => {
                                    rename = Some(RenameState::open(
                                        crate::protocol::RenameSelector::Workspace(workspace_id),
                                        "workspace",
                                        name,
                                    ));
                                }
                                UiActivation::Sidebar(ComponentEffect::CloseSidebar) => {
                                    if matches!(surface.as_ref(), Some(ClientSurface::Sidebar(_))) {
                                        surface = None;
                                        view.invalidate_drawn();
                                    }
                                }
                                UiActivation::Sidebar(ComponentEffect::Stay)
                                | UiActivation::Tab(TabBarAction::Stay) => {}
                                UiActivation::Tab(TabBarAction::Close) => {
                                    if matches!(surface.as_ref(), Some(ClientSurface::TabBar(_))) {
                                        surface = None;
                                        view.invalidate_drawn();
                                    }
                                }
                            }
                            force_draw = true;
                            continue;
                        }
                        if let MouseButtonState::Selecting {
                            terminal_id,
                            anchor,
                        } = mouse_input.button(MouseButton::Left)
                        {
                            match mouse.kind {
                                HostMouseEventKind::Drag(HostMouseButton::Left) => {
                                    if let Some(position) = view.copy_cell(
                                        layout.terminal,
                                        ui.pane_layout,
                                        terminal_id,
                                        mouse.column,
                                        mouse.row,
                                        true,
                                    ) {
                                        let mut state = CopyModeState::enter_mouse(
                                            terminal_id,
                                            anchor,
                                            position,
                                        );
                                        pump_copy_mode(framed, &mut state).await?;
                                        copy_mode = Some(state);
                                        force_draw = true;
                                    }
                                    mouse_input.discard(mouse);
                                    continue;
                                }
                                HostMouseEventKind::Up(HostMouseButton::Left) => {
                                    mouse_input.discard(mouse);
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        match mouse_input.route_ui(
                            mouse,
                            host,
                            &visible_sidebars,
                            view.focused().tab_id,
                            &split_dividers,
                        ) {
                            UiMouseRoute::Owned(Some(UiResizeAction::Sidebar { side, width })) => {
                                side.config_mut(&mut ui).width = width;
                                resize_view(framed, host, &mut view, &resources, &ui).await?;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            UiMouseRoute::Owned(Some(UiResizeAction::Split {
                                tab_id,
                                split_id,
                                ratio,
                            })) => {
                                view.resize_split(tab_id, split_id, ratio);
                                send(
                                    framed,
                                    ClientMessage::ResizeSplit {
                                        tab_id,
                                        split_id,
                                        ratio,
                                    },
                                ).await?;
                                resize_view(framed, host, &mut view, &resources, &ui).await?;
                                force_draw = true;
                            }
                            UiMouseRoute::Owned(None) => {}
                            UiMouseRoute::NotOwned if unobstructed => {
                                if mouse_input.can_begin_selection()
                                    && let Some((terminal_id, anchor)) = mouse_selection_anchor(
                                        &view,
                                        layout.terminal,
                                        ui.pane_layout,
                                        mouse,
                                    )
                                {
                                    mouse_input.begin_selection(terminal_id, anchor);
                                    continue;
                                }
                                if let Some(action) = mouse_input.route(
                                    &view,
                                    layout.terminal,
                                    ui.pane_layout,
                                    mouse,
                                ) {
                                    match action {
                                        PaneMouseAction::Input { terminal_id, event } => {
                                            send(
                                                framed,
                                                ClientMessage::MouseInput {
                                                    terminal_id,
                                                    event,
                                                },
                                            ).await?;
                                            mouse_input.finish_release(terminal_id, event);
                                        }
                                        PaneMouseAction::Focus(pane_id) => {
                                            if let Some(request) = focus.begin(FocusOrigin::Pane) {
                                                release_captured_mouse_input(
                                                    framed,
                                                    &mut mouse_input,
                                                    view.focused().terminal_id,
                                                ).await?;
                                                send_request(
                                                    framed,
                                                    Some(request),
                                                    ClientMessage::SelectTarget {
                                                        selector: TargetSelector::Pane(pane_id),
                                                        expected: Some(SelectionExpectation::Tab(
                                                            view.focused().tab_id,
                                                        )),
                                                    },
                                                ).await?;
                                            }
                                        }
                                    }
                                }
                            }
                            UiMouseRoute::NotOwned => mouse_input.discard(mouse),
                            }
                        }
                    Event::Key(key) if surface.is_none() && copy_mode.is_none() => if let Some(bytes) = encode_key(key) {
                        let terminal_key = terminal_key_event(key);
                        let prefix_was_waiting = prefix.waiting();
                        let focused_terminal_was_covered =
                            cheatsheet_visible || toasts.is_visible();
                        toasts.clear();
                        let was_visible = cheatsheet_visible;
                        cheatsheet_at = None;
                        cheatsheet_visible = false;
                        if was_visible {
                            view.invalidate_drawn();
                            force_draw = true;
                        }
                        match prefix.feed(bytes) {
                            PrefixAction::Wait => {
                                cheatsheet_at = Some(time::Instant::now() + CHEATSHEET_DELAY);
                                send(
                                    framed,
                                    ClientMessage::ResetViewport {
                                        terminal_id: view.focused().terminal_id,
                                    },
                                ).await?;
                            }
                            PrefixAction::Dispatch(action) => {
                                release_captured_mouse_input(
                                    framed,
                                    &mut mouse_input,
                                    view.focused().terminal_id,
                                ).await?;
                                send(
                                    framed,
                                    ClientMessage::ResetViewport {
                                        terminal_id: view.focused().terminal_id,
                                    },
                                ).await?;
                                toasts.replace(dispatch_client_action(
                                    action,
                                    framed,
                                    &mut view,
                                    &resources,
                                    &mut surface,
                                    &workspace_history,
                                    &mut create,
                                    &mut close_target,
                                    &mut focus,
                                    &mut copy_mode,
                                    &mut rename,
                                    terminal.size()?.into(),
                                    &mut ui,
                                    &mut temporary_command,
                                    socket_path,
                                    &background_results,
                                    config_location,
                                    &mut extension_reload,
                                    &mut project_config_reload,
                                    focused_terminal_was_covered,
                                ).await?);
                                force_draw = true;
                            }
                            PrefixAction::Send(bytes) => {
                                if prefix_was_waiting {
                                    send(framed, ClientMessage::Input { bytes }).await?;
                                } else if let Some(event) = terminal_key {
                                    send(framed, ClientMessage::KeyInput { event }).await?;
                                } else {
                                    send(framed, ClientMessage::Input { bytes }).await?;
                                }
                            }
                        }
                    },
                    Event::Paste(text) if surface.is_none() && copy_mode.is_none() => {
                        send(framed, ClientMessage::Paste { text }).await?
                    }
                    Event::Resize(columns, rows) if columns > 0 && rows > 0 => {
                        release_captured_mouse_input(
                            framed,
                            &mut mouse_input,
                            view.focused().terminal_id,
                        ).await?;
                        if copy_mode.is_none() {
                            send(
                                framed,
                                ClientMessage::ResetViewport {
                                    terminal_id: view.focused().terminal_id,
                                },
                            ).await?;
                        }
                        resize_view(
                            framed,
                            Rect::new(0, 0, columns, rows),
                            &mut view,
                            &resources,
                            &ui,
                        ).await?;
                        force_draw = true;
                    }
                    _ => {}
                }
            }
            result = clipboard_result.recv() => {
                if let Some(result) = result {
                    match result {
                        ClipboardResult::Copied { request_id, bytes } => {
                            if let Some(command) = temporary_command.as_mut() {
                                match command.finish_clipboard(request_id, true).await {
                                    Ok(true) => {
                                        toasts.info(format!("copied {bytes} bytes"));
                                        force_draw = true;
                                        continue;
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        command.cancel_mouse_copy().await;
                                        toasts.error(format!("COPY FAILED · {}", one_line_error(&error)));
                                        force_draw = true;
                                        continue;
                                    }
                                }
                            }
                            if let Some(copy_id) = copy_mode
                                .as_mut()
                                .and_then(|state| state.finish_clipboard(request_id))
                            {
                                let state = copy_mode
                                    .as_mut()
                                    .expect("accepted clipboard copy is active");
                                state.finalize_copy(copy_id, bytes);
                                toasts.clear();
                                pump_copy_mode(framed, state).await?;
                                force_draw = true;
                            }
                        }
                        ClipboardResult::Failed { request_id, message } => {
                            if let Some(command) = temporary_command.as_mut() {
                                match command.finish_clipboard(request_id, false).await {
                                    Ok(true) => {
                                        toasts.error(format!("COPY FAILED · {message}"));
                                        force_draw = true;
                                        continue;
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        command.cancel_mouse_copy().await;
                                        toasts.error(format!("COPY FAILED · {}", one_line_error(&error)));
                                        force_draw = true;
                                        continue;
                                    }
                                }
                            }
                            if copy_mode
                                .as_mut()
                                .and_then(|state| state.finish_clipboard(request_id))
                                .is_some()
                            {
                                toasts.error(format!(
                                    "COPY FAILED · press y to retry · {message}"
                                ));
                                pump_copy_mode(
                                    framed,
                                    copy_mode.as_mut().expect("failed clipboard copy is active"),
                                ).await?;
                                force_draw = true;
                            }
                        }
                    }
                }
            }
            _ = async { time::sleep_until(cheatsheet_at.expect("deadline is set")).await }, if cheatsheet_at.is_some() => {
                cheatsheet_at = None;
                cheatsheet_visible = true;
                force_draw = true;
            }
            _ = async { time::sleep_until(toasts.deadline().expect("deadline is set")).await }, if toasts.deadline().is_some() => {
                toasts.expire();
                force_draw = true;
            }
            _ = spinner.tick(), if resources.has_working() || resources.has_animated_extension_token(&ui) => {
                spinner_frame = spinner_frame.wrapping_add(1);
                force_draw = true;
            }
            _ = redraw.tick(), if force_draw || view.needs_draw() => {
                let focused_terminal_id = view.focused().terminal_id;
                let focused_terminal_rendered = focused_terminal_is_rendered(
                    temporary_command.is_some(),
                    surface.as_ref(),
                    rename.is_some(),
                    toasts.is_visible(),
                );
                let rendered_attention = focused_terminal_rendered
                .then(|| resources.attention_revision(focused_terminal_id))
                .flatten();
                let rendered_alert = focused_terminal_rendered
                .then(|| resources.notifications().terminal_alert(focused_terminal_id))
                .flatten()
                .filter(|alert| alert.unseen())
                .map(|alert| alert.state.cursor());
                let draw_started = std::time::Instant::now();
                io::stdout().sync_update(|stdout| -> io::Result<()> {
                    let mut rendered_cursor = None;
                    let mut graphics_area = None;
                    terminal.draw(|frame| {
                        let area = frame.area();
                        if let Some(command) = temporary_command.as_ref() {
                            let command_area = command.size().area(area);
                            let content = render_temporary_command_frame(
                                command_area,
                                command.title(),
                                &ui.styles,
                                frame.buffer_mut(),
                            );
                            frame.render_widget(Screen(&command.screen), content);
                            if command.screen.cursor.visible
                                && command.screen.cursor.column < content.width
                                && command.screen.cursor.row < content.height
                            {
                                frame.set_cursor_position((
                                    content.x + command.screen.cursor.column,
                                    content.y + command.screen.cursor.row,
                                ));
                                rendered_cursor = Some(RenderedCursor {
                                    column: content.x + command.screen.cursor.column,
                                    row: content.y + command.screen.cursor.row,
                                    shape: command.screen.cursor.shape,
                                    blinking: command.screen.cursor.blinking,
                                });
                            }
                            toasts.render(
                                area,
                                ui.tab_bar.position,
                                &ui.styles,
                                frame.buffer_mut(),
                            );
                            return;
                        }
                        let layout = client_layout(
                            area,
                            &ui,
                            resources.sidebar_relevance(view.focused(), &ui),
                        );
                        graphics_area = Some(layout.terminal);
                        let cursor = render_view(
                            &view,
                            layout.terminal,
                            ui.pane_layout,
                            &ui.styles,
                            frame.buffer_mut(),
                        );
                        if let Some(tab_bar) = layout.tab_bar {
                            if let Some(ClientSurface::TabBar(state)) = surface.as_ref() {
                                state.render(
                                    resources.snapshot(),
                                    view.focused(),
                                    view.is_zoomed(),
                                    &ui,
                                    resources.notifications(),
                                    spinner_frame,
                                    tab_bar,
                                    frame.buffer_mut(),
                                );
                            } else {
                                render_tab_bar(
                                    resources.snapshot(),
                                    view.focused(),
                                    view.is_zoomed(),
                                    None,
                                    resources.notifications(),
                                    spinner_frame,
                                    &ui,
                                    tab_bar,
                                    frame.buffer_mut(),
                                );
                            }
                        }
                        for side in SidebarSide::ALL {
                            if let Some(sidebar_area) =
                                layout.sidebar(side).and_then(|sidebar| sidebar.docked())
                            {
                                render_sidebar(
                                    resources.snapshot(),
                                    view.focused(),
                                    &workspace_history,
                                    resources.notifications(),
                                    spinner_frame,
                                    sidebar_area,
                                    side,
                                    &ui,
                                    frame.buffer_mut(),
                                );
                            }
                        }
                        if let Some(ClientSurface::Sidebar(sidebar)) = surface.as_ref()
                            && let Some(sidebar_area) = sidebar_drawer(area, &ui, sidebar.side())
                        {
                            sidebar.render(
                                sidebar_area,
                                &ui,
                                spinner_frame,
                                frame.buffer_mut(),
                            );
                        }
                        match surface.as_mut() {
                            Some(ClientSurface::Navigator(nav)) => {
                                nav.render(area, spinner_frame, &ui.styles, frame.buffer_mut());
                            }
                            Some(ClientSurface::ProjectOpener(opener)) => {
                                opener.render(layout.terminal, frame.buffer_mut());
                            }
                            Some(ClientSurface::Agents(dialog)) => {
                                dialog.render(area, spinner_frame, &ui.styles, frame.buffer_mut());
                            }
                            Some(ClientSurface::Notifications(dialog)) => {
                                dialog.render(area, frame.buffer_mut());
                            }
                            Some(ClientSurface::CommandBar(command_bar)) => {
                                command_bar.render(layout.terminal, frame.buffer_mut());
                            }
                            Some(ClientSurface::ContextMenu(menu)) => {
                                menu.render(area, &ui.styles, frame.buffer_mut());
                            }
                            Some(ClientSurface::CommandForm(form)) => {
                                form.render(layout.terminal, frame.buffer_mut());
                            }
                            Some(ClientSurface::Sidebar(_))
                            | Some(ClientSurface::TabBar(_))
                            | None => {}
                        }
                        if cheatsheet_visible {
                            cheatsheet::render(&ui.bindings, layout.terminal, frame.buffer_mut());
                        }
                        if let Some(rename) = rename.as_ref() {
                            rename.render(layout.terminal, frame.buffer_mut());
                        }
                        if let Some(copy_mode) = copy_mode.as_ref() {
                            copy_mode.render(layout.terminal, frame.buffer_mut());
                        }
                        if app_overlay_clear(
                            surface.is_none(),
                            rename.is_none(),
                            copy_mode.is_none(),
                            cheatsheet_visible,
                        )
                            && !toasts.is_visible()
                            && let Some(cursor) = cursor
                        {
                            frame.set_cursor_position((cursor.column, cursor.row));
                            rendered_cursor = Some(cursor);
                        }
                        toasts.render(
                            area,
                            ui.tab_bar.position,
                            &ui.styles,
                            frame.buffer_mut(),
                        );
                    })?;
                    host_cursor.apply(stdout, rendered_cursor)?;
                    kitty_graphics.sync(
                        stdout,
                        &view,
                        graphics_area,
                        ui.pane_layout,
                        temporary_command.is_none()
                            && surface.is_none()
                            && rename.is_none()
                            && copy_mode.is_none()
                            && !cheatsheet_visible
                            && !toasts.is_visible(),
                    )
                })??;
                if let Some(perf) = perf.as_mut() {
                    perf.record("draw", draw_started.elapsed(), 0);
                }
                view.mark_drawn();
                force_draw = false;
                if let Some(event_revision) = rendered_attention {
                    send(
                        framed,
                        ClientMessage::AcknowledgeAgent {
                            terminal_id: focused_terminal_id,
                            event_revision,
                        },
                    )
                    .await?;
                }
                if let Some(observed) = rendered_alert {
                    send(
                        framed,
                        ClientMessage::AcknowledgeAlerts {
                            terminal_id: focused_terminal_id,
                            observed,
                        },
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

fn focused_terminal_is_rendered(
    temporary_command_open: bool,
    surface: Option<&ClientSurface>,
    rename_open: bool,
    toast_visible: bool,
) -> bool {
    !temporary_command_open
        && !rename_open
        && !toast_visible
        && matches!(
            surface,
            None | Some(ClientSurface::Sidebar(_))
                | Some(ClientSurface::TabBar(_))
                | Some(ClientSurface::ContextMenu(_))
        )
}

fn failed_exit_code(exit: Option<Option<i32>>) -> Option<i32> {
    exit.flatten().filter(|code| *code != 0)
}

#[allow(
    clippy::too_many_arguments,
    reason = "context-menu actions share the interactive client's request and safeguard state"
)]
async fn dispatch_context_menu_action(
    action: ContextMenuAction,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    view: &mut ViewState,
    resources: &ResourceState,
    surface: &mut Option<ClientSurface>,
    rename: &mut Option<RenameState>,
    create: &mut CreateCoordinator,
    close_target: &mut CloseTargetState,
    focus: &mut FocusState,
    mouse_input: &mut MouseInputState,
    ui: &mut UiConfig,
    toasts: &mut ToastState,
    host: Rect,
) -> anyhow::Result<()> {
    match action {
        ContextMenuAction::Stay => return Ok(()),
        ContextMenuAction::Dismiss => {}
        ContextMenuAction::SwitchTab(pane_id) | ContextMenuAction::SwitchWorkspace(pane_id) => {
            release_captured_mouse_input(framed, mouse_input, view.focused().terminal_id).await?;
            let origin = if matches!(action, ContextMenuAction::SwitchTab(_)) {
                FocusOrigin::Tab
            } else {
                FocusOrigin::Workspace
            };
            if let Some(request) = focus.begin(origin) {
                let scope = if matches!(action, ContextMenuAction::SwitchTab(_)) {
                    NavigationScope::Tab
                } else {
                    NavigationScope::Workspace
                };
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::SelectTarget {
                        selector: TargetSelector::Pane(pane_id),
                        expected: resources
                            .snapshot()
                            .and_then(|snapshot| selection_expectation(snapshot, pane_id, scope)),
                    },
                )
                .await?;
            }
        }
        ContextMenuAction::CreateTab(workspace_id) => {
            if let Some(request) = create.begin(CreateKind::Tab) {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::CreateTab {
                        workspace_id,
                        name: None,
                        cwd: None,
                        program: None,
                        argv: Vec::new(),
                    },
                )
                .await?;
            }
        }
        ContextMenuAction::CreateWorkspace(session_id) => {
            if let Some(request) = create.begin(CreateKind::Workspace) {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::CreateWorkspace {
                        session_id,
                        name: None,
                        cwd: None,
                        program: None,
                        argv: Vec::new(),
                    },
                )
                .await?;
            }
        }
        ContextMenuAction::Rename(selector, name) => {
            let label = match selector {
                crate::protocol::RenameSelector::Workspace(_) => "workspace",
                crate::protocol::RenameSelector::Tab(_) => "tab",
                crate::protocol::RenameSelector::Session(_) => "session",
            };
            *rename = Some(RenameState::open(selector, label, name));
        }
        ContextMenuAction::Close(selector, label) => {
            toasts.replace(
                start_target_close(framed, close_target, selector, label, ui.confirm_close).await?,
            );
        }
        ContextMenuAction::SetDisplay(side, display) => {
            side.config_mut(ui).display.set(display);
            resize_view(framed, host, view, resources, ui).await?;
        }
        ContextMenuAction::SetVisibility(side, visibility) => {
            side.config_mut(ui).visibility.set(visibility);
            resize_view(framed, host, view, resources, ui).await?;
        }
    }
    *surface = None;
    view.invalidate_drawn();
    Ok(())
}

fn refresh_surface_resources(
    surface: &mut Option<ClientSurface>,
    snapshot: &crate::resources::ResourceSnapshot,
    focused: &SelectedTarget,
    workspace_history: &NavigationHistory,
    notifications: &notifications::NotificationState,
) {
    match surface.as_mut() {
        Some(ClientSurface::Navigator(nav)) => {
            nav.accept_resources_with_notifications(
                snapshot,
                focused,
                workspace_history,
                notifications,
            );
        }
        Some(ClientSurface::Agents(dialog)) => {
            dialog.accept_resources(snapshot, focused, notifications);
        }
        Some(ClientSurface::Sidebar(sidebar)) => {
            sidebar.accept_resources(snapshot, focused, workspace_history, notifications);
        }
        Some(ClientSurface::TabBar(tab_bar)) => {
            tab_bar.accept_resources(snapshot, focused, workspace_history);
        }
        Some(ClientSurface::Notifications(dialog)) => {
            dialog.accept_resources(snapshot, notifications);
        }
        Some(
            ClientSurface::CommandBar(_)
            | ClientSurface::ProjectOpener(_)
            | ClientSurface::ContextMenu(_)
            | ClientSurface::CommandForm(_),
        )
        | None => {}
    }
}

fn refresh_surface_presence(
    surface: &mut Option<ClientSurface>,
    presence: &crate::protocol::ClientPresenceSnapshot,
) {
    if let Some(ClientSurface::Navigator(navigator)) = surface.as_mut() {
        navigator.accept_presence(presence);
    }
}

fn reconcile_resource_barriers(
    snapshot: &crate::resources::ResourceSnapshot,
    create: &mut CreateCoordinator,
    rename: &mut Option<RenameState>,
) {
    create.accept_resources(snapshot.revision);
    if rename
        .as_mut()
        .is_some_and(|rename| rename.accept_resources(snapshot))
    {
        *rename = None;
    }
}

fn selection_expectation(
    snapshot: &crate::resources::ResourceSnapshot,
    pane_id: crate::domain::PaneId,
    scope: NavigationScope,
) -> Option<SelectionExpectation> {
    if scope == NavigationScope::Global {
        return None;
    }
    snapshot
        .pane_paths()
        .find(|path| path.pane.id == pane_id)
        .map(|path| match scope {
            NavigationScope::Pane | NavigationScope::Tab => SelectionExpectation::Tab(path.tab.id),
            NavigationScope::Workspace => SelectionExpectation::Workspace(path.workspace.id),
            NavigationScope::Session => SelectionExpectation::Session(path.session.id),
            NavigationScope::Global => unreachable!("handled before ancestry lookup"),
        })
}

fn accepts_client_input(
    focus: &FocusState,
    create: &CreateCoordinator,
    close_target: &CloseTargetState,
    pending_focused_exit: &Option<Option<i32>>,
) -> bool {
    focus.request_id.is_none()
        && !create.blocks_input()
        && !close_target.blocks_input()
        && pending_focused_exit.is_none()
}

impl MouseInputState {
    fn can_begin_selection(&self) -> bool {
        self.buttons
            .iter()
            .all(|button| matches!(button, MouseButtonState::Idle))
    }

    fn begin_selection(&mut self, terminal_id: TerminalId, anchor: (u16, u16)) {
        self.set_button(
            MouseButton::Left,
            MouseButtonState::Selecting {
                terminal_id,
                anchor,
            },
        );
    }

    fn route_ui(
        &mut self,
        mouse: HostMouseEvent,
        host: Rect,
        sidebars: &[SidebarDivider],
        tab_id: crate::domain::TabId,
        split_dividers: &[SplitDivider],
    ) -> UiMouseRoute {
        if let Some(drag) = self.ui_drag {
            let finish = matches!(mouse.kind, HostMouseEventKind::Up(HostMouseButton::Left));
            let resize = match drag {
                UiDrag::Sidebar {
                    position,
                    last_width,
                    max_width,
                } => matches!(
                    mouse.kind,
                    HostMouseEventKind::Drag(HostMouseButton::Left)
                        | HostMouseEventKind::Moved
                        | HostMouseEventKind::Up(HostMouseButton::Left)
                )
                .then(|| {
                    let width = sidebar_width_at(host, position, mouse.column, max_width);
                    (width != last_width).then_some(UiResizeAction::Sidebar {
                        side: position,
                        width,
                    })
                })
                .flatten(),
                UiDrag::Split {
                    tab_id,
                    split_id,
                    last_cell,
                } => matches!(
                    mouse.kind,
                    HostMouseEventKind::Drag(HostMouseButton::Left)
                        | HostMouseEventKind::Moved
                        | HostMouseEventKind::Up(HostMouseButton::Left)
                )
                .then(|| {
                    let divider = split_dividers
                        .iter()
                        .copied()
                        .find(|divider| divider.split_id == split_id)?;
                    let first = divider.first_size_at(mouse.column, mouse.row);
                    (first != divider.first_size && (divider.available, first) != last_cell).then(
                        || UiResizeAction::Split {
                            tab_id,
                            split_id,
                            ratio: SplitRatio::from_cells(first, divider.available)
                                .expect("layout minima keep both split children nonempty"),
                        },
                    )
                })
                .flatten(),
            };
            if let Some(resize) = resize {
                match (&mut self.ui_drag, resize) {
                    (
                        Some(UiDrag::Sidebar { last_width, .. }),
                        UiResizeAction::Sidebar { width, .. },
                    ) => {
                        *last_width = width;
                    }
                    (
                        Some(UiDrag::Split { last_cell, .. }),
                        UiResizeAction::Split {
                            split_id, ratio, ..
                        },
                    ) => {
                        if let Some(divider) = split_dividers
                            .iter()
                            .find(|divider| divider.split_id == split_id)
                        {
                            *last_cell = (divider.available, ratio.first_cells(divider.available));
                        }
                    }
                    _ => unreachable!("UI resize action matches its drag owner"),
                }
            }
            if finish {
                self.ui_drag = None;
            }
            return UiMouseRoute::Owned(resize);
        }

        if !matches!(mouse.kind, HostMouseEventKind::Down(HostMouseButton::Left))
            || !self
                .buttons
                .iter()
                .all(|button| matches!(button, MouseButtonState::Idle))
        {
            return UiMouseRoute::NotOwned;
        }
        if let Some(sidebar) = sidebars
            .iter()
            .copied()
            .find(|sidebar| rect_contains(sidebar.area, mouse.column, mouse.row))
        {
            self.ui_drag = Some(UiDrag::Sidebar {
                position: sidebar.position,
                last_width: sidebar.current_width,
                max_width: sidebar.max_width,
            });
            return UiMouseRoute::Owned(None);
        }
        if let Some(divider) = split_dividers
            .iter()
            .copied()
            .find(|divider| divider.contains(mouse.column, mouse.row))
        {
            self.ui_drag = Some(UiDrag::Split {
                tab_id,
                split_id: divider.split_id,
                last_cell: (divider.available, divider.first_size),
            });
            return UiMouseRoute::Owned(None);
        }
        UiMouseRoute::NotOwned
    }

    fn cancel_local_drags(&mut self) {
        self.ui_drag = None;
        if matches!(
            self.button(MouseButton::Left),
            MouseButtonState::Selecting { .. }
        ) {
            self.suppress(MouseButton::Left);
        }
    }

    fn cancel_split_drag(&mut self) -> bool {
        if matches!(self.ui_drag, Some(UiDrag::Split { .. })) {
            self.ui_drag = None;
            true
        } else {
            false
        }
    }

    fn reconcile_ui_drag(&mut self, tab_id: crate::domain::TabId, layout: &SplitTree) -> bool {
        if matches!(
            self.ui_drag,
            Some(UiDrag::Split {
                tab_id: drag_tab,
                split_id,
                ..
            }) if drag_tab != tab_id || layout.ratio(split_id).is_none()
        ) {
            self.ui_drag = None;
            true
        } else {
            false
        }
    }

    fn route(
        &mut self,
        view: &ViewState,
        area: Rect,
        policy: PaneLayoutPolicy,
        mouse: HostMouseEvent,
    ) -> Option<PaneMouseAction> {
        match mouse.kind {
            HostMouseEventKind::ScrollUp | HostMouseEventKind::ScrollDown => {
                let (target, column, row) = view.pane_at(area, policy, mouse.column, mouse.row)?;
                Some(PaneMouseAction::Input {
                    terminal_id: target.terminal_id,
                    event: normalized_mouse_event(
                        mouse,
                        MouseEventKind::Wheel {
                            direction: if mouse.kind == HostMouseEventKind::ScrollUp {
                                MouseWheelDirection::Up
                            } else {
                                MouseWheelDirection::Down
                            },
                        },
                        column,
                        row,
                        self.captured_buttons(target.terminal_id, None),
                    ),
                })
            }
            HostMouseEventKind::ScrollLeft | HostMouseEventKind::ScrollRight => None,
            HostMouseEventKind::Down(button) => {
                let button = normalize_mouse_button(button);
                if !matches!(self.button(button), MouseButtonState::Idle) {
                    return None;
                }
                let Some((target, column, row)) =
                    view.pane_at(area, policy, mouse.column, mouse.row)
                else {
                    self.suppress(button);
                    return None;
                };
                if target.terminal_id != view.focused().terminal_id {
                    self.suppress(button);
                    return (button == MouseButton::Left)
                        .then_some(PaneMouseAction::Focus(target.pane_id));
                }

                self.set_button(
                    button,
                    MouseButtonState::Captured {
                        terminal_id: target.terminal_id,
                        column,
                        row,
                        modifiers: normalized_mouse_modifiers(mouse.modifiers),
                    },
                );
                Some(PaneMouseAction::Input {
                    terminal_id: target.terminal_id,
                    event: normalized_mouse_event(
                        mouse,
                        MouseEventKind::Press { button },
                        column,
                        row,
                        self.captured_buttons(target.terminal_id, None),
                    ),
                })
            }
            HostMouseEventKind::Up(button) => {
                let button = normalize_mouse_button(button);
                let MouseButtonState::Captured {
                    terminal_id,
                    column: previous_column,
                    row: previous_row,
                    ..
                } = self.button(button)
                else {
                    self.set_button(button, MouseButtonState::Idle);
                    return None;
                };
                if terminal_id != view.focused().terminal_id {
                    self.set_button(button, MouseButtonState::Idle);
                    return None;
                }
                let (column, row) = view
                    .terminal_cell(area, policy, terminal_id, mouse.column, mouse.row, true)
                    .unwrap_or((previous_column, previous_row));
                self.set_button(
                    button,
                    MouseButtonState::Captured {
                        terminal_id,
                        column,
                        row,
                        modifiers: normalized_mouse_modifiers(mouse.modifiers),
                    },
                );
                Some(PaneMouseAction::Input {
                    terminal_id,
                    event: normalized_mouse_event(
                        mouse,
                        MouseEventKind::Release { button },
                        column,
                        row,
                        self.captured_buttons(terminal_id, Some(button)),
                    ),
                })
            }
            HostMouseEventKind::Drag(button) => {
                let button = normalize_mouse_button(button);
                let MouseButtonState::Captured { terminal_id, .. } = self.button(button) else {
                    self.suppress(button);
                    return None;
                };
                if terminal_id != view.focused().terminal_id {
                    self.set_button(button, MouseButtonState::Idle);
                    return None;
                }
                let (column, row) =
                    view.terminal_cell(area, policy, terminal_id, mouse.column, mouse.row, true)?;
                self.set_button(
                    button,
                    MouseButtonState::Captured {
                        terminal_id,
                        column,
                        row,
                        modifiers: normalized_mouse_modifiers(mouse.modifiers),
                    },
                );
                Some(PaneMouseAction::Input {
                    terminal_id,
                    event: normalized_mouse_event(
                        mouse,
                        MouseEventKind::Motion {
                            button: Some(button),
                        },
                        column,
                        row,
                        self.captured_buttons(terminal_id, None),
                    ),
                })
            }
            HostMouseEventKind::Moved => {
                let (target, column, row) = view.pane_at(area, policy, mouse.column, mouse.row)?;
                if target.terminal_id != view.focused().terminal_id {
                    return None;
                }
                Some(PaneMouseAction::Input {
                    terminal_id: target.terminal_id,
                    event: normalized_mouse_event(
                        mouse,
                        MouseEventKind::Motion { button: None },
                        column,
                        row,
                        self.captured_buttons(target.terminal_id, None),
                    ),
                })
            }
        }
    }

    fn discard(&mut self, mouse: HostMouseEvent) {
        match mouse.kind {
            HostMouseEventKind::Down(button) | HostMouseEventKind::Drag(button) => {
                let button = normalize_mouse_button(button);
                self.suppress(button);
            }
            HostMouseEventKind::Up(button) => {
                let button = normalize_mouse_button(button);
                self.set_button(button, MouseButtonState::Idle);
            }
            HostMouseEventKind::Moved
            | HostMouseEventKind::ScrollDown
            | HostMouseEventKind::ScrollUp
            | HostMouseEventKind::ScrollLeft
            | HostMouseEventKind::ScrollRight => {}
        }
    }

    fn suppress(&mut self, button: MouseButton) {
        if !matches!(self.button(button), MouseButtonState::Captured { .. }) {
            self.set_button(button, MouseButtonState::Suppressed);
        }
    }

    fn button(&self, button: MouseButton) -> MouseButtonState {
        self.buttons[mouse_button_index(button)]
    }

    fn set_button(&mut self, button: MouseButton, state: MouseButtonState) {
        self.buttons[mouse_button_index(button)] = state;
    }

    fn captured_buttons(
        &self,
        terminal_id: TerminalId,
        excluding: Option<MouseButton>,
    ) -> MouseButtons {
        let mut buttons = MouseButtons::default();
        for button in MouseButton::ALL {
            if excluding != Some(button)
                && matches!(
                    self.button(button),
                    MouseButtonState::Captured {
                        terminal_id: captured,
                        ..
                    } if captured == terminal_id
                )
            {
                buttons.set(button, true);
            }
        }
        buttons
    }

    fn synthetic_releases(&self, focused_terminal_id: TerminalId) -> Vec<PaneMouseAction> {
        let mut held = self.captured_buttons(focused_terminal_id, None);
        let mut releases = Vec::new();
        for button in MouseButton::ALL {
            let MouseButtonState::Captured {
                terminal_id,
                column,
                row,
                modifiers,
            } = self.button(button)
            else {
                continue;
            };
            if terminal_id != focused_terminal_id {
                continue;
            }
            held.set(button, false);
            releases.push(PaneMouseAction::Input {
                terminal_id,
                event: MouseEvent {
                    kind: MouseEventKind::Release { button },
                    column,
                    row,
                    modifiers,
                    buttons: held,
                },
            });
        }
        releases
    }

    fn finish_release(&mut self, terminal_id: TerminalId, event: MouseEvent) {
        let MouseEventKind::Release { button } = event.kind else {
            return;
        };
        if matches!(
            self.button(button),
            MouseButtonState::Captured {
                terminal_id: captured,
                ..
            } if captured == terminal_id
        ) {
            self.set_button(button, MouseButtonState::Idle);
        }
    }

    fn clear(&mut self) {
        self.buttons.fill(MouseButtonState::Idle);
        self.ui_drag = None;
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn mouse_selection_anchor(
    view: &ViewState,
    area: Rect,
    policy: PaneLayoutPolicy,
    mouse: HostMouseEvent,
) -> Option<(TerminalId, (u16, u16))> {
    if !matches!(mouse.kind, HostMouseEventKind::Down(HostMouseButton::Left)) {
        return None;
    }
    let (target, _, _) = view.pane_at(area, policy, mouse.column, mouse.row)?;
    if target.terminal_id != view.focused().terminal_id
        || (view.mouse_tracking(target.terminal_id)
            && !mouse.modifiers.contains(KeyModifiers::SHIFT))
    {
        return None;
    }
    let cell = view.copy_cell(
        area,
        policy,
        target.terminal_id,
        mouse.column,
        mouse.row,
        false,
    )?;
    Some((target.terminal_id, cell))
}

fn app_overlay_clear(
    surface_clear: bool,
    rename_clear: bool,
    copy_mode_clear: bool,
    cheatsheet_visible: bool,
) -> bool {
    surface_clear && rename_clear && copy_mode_clear && !cheatsheet_visible
}

fn sidebar_width_at(
    host: Rect,
    position: sidebar::SidebarSide,
    column: u16,
    max_width: u16,
) -> u16 {
    let width = match position {
        sidebar::SidebarSide::Left => column.saturating_sub(host.x).saturating_add(1),
        sidebar::SidebarSide::Right => host.right().saturating_sub(column),
    };
    width.clamp(MIN_SIDEBAR_WIDTH, max_width)
}

fn sidebar_divider(
    sidebar: Rect,
    position: sidebar::SidebarSide,
    max_width: u16,
) -> Option<SidebarDivider> {
    if sidebar.width < MIN_SIDEBAR_WIDTH || sidebar.height == 0 {
        return None;
    }
    let x = match position {
        sidebar::SidebarSide::Left => sidebar.right() - 1,
        sidebar::SidebarSide::Right => sidebar.x,
    };
    Some(SidebarDivider {
        position,
        area: Rect::new(x, sidebar.y, 1, sidebar.height),
        current_width: sidebar.width,
        max_width,
    })
}

fn docked_sidebar_max_width(host: Rect, layout: ClientLayout, side: SidebarSide) -> u16 {
    let other_width = SidebarSide::ALL
        .into_iter()
        .find(|other| *other != side)
        .and_then(|other| layout.sidebar(other))
        .and_then(|sidebar| sidebar.docked())
        .map_or(0, |area| area.width);
    host.width
        .saturating_sub(MIN_DOCKED_TERMINAL_WIDTH)
        .saturating_sub(other_width)
        .clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH)
}

fn normalized_mouse_event(
    mouse: HostMouseEvent,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    buttons: MouseButtons,
) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: normalized_mouse_modifiers(mouse.modifiers),
        buttons,
    }
}

fn normalized_mouse_modifiers(modifiers: KeyModifiers) -> MouseModifiers {
    MouseModifiers {
        shift: modifiers.contains(KeyModifiers::SHIFT),
        control: modifiers.contains(KeyModifiers::CONTROL),
        alt: modifiers.contains(KeyModifiers::ALT),
    }
}

fn normalize_mouse_button(button: HostMouseButton) -> MouseButton {
    match button {
        HostMouseButton::Left => MouseButton::Left,
        HostMouseButton::Middle => MouseButton::Middle,
        HostMouseButton::Right => MouseButton::Right,
    }
}

fn mouse_button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

async fn release_captured_mouse_input(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    state: &mut MouseInputState,
    focused_terminal_id: TerminalId,
) -> anyhow::Result<()> {
    for action in state.synthetic_releases(focused_terminal_id) {
        let PaneMouseAction::Input { terminal_id, event } = action else {
            unreachable!("synthetic mouse releases are terminal input")
        };
        send(framed, ClientMessage::MouseInput { terminal_id, event }).await?;
    }
    state.clear();
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "catalog installation synchronously swaps the coordinated client UI states"
)]
async fn install_ui_config(
    candidate: UiConfig,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    host: Rect,
    view: &mut ViewState,
    resources: &ResourceState,
    surface: &mut Option<ClientSurface>,
    workspace_history: &NavigationHistory,
    prefix: &mut PrefixState,
    ui: &mut UiConfig,
    client_hooks: &mut crate::extensions::ClientHookRuntime,
) -> anyhow::Result<()> {
    // Command launch surfaces own declarations derived from the current
    // extension generation. Close them before replacing that generation.
    if matches!(
        surface,
        Some(
            ClientSurface::CommandBar(_)
                | ClientSurface::ProjectOpener(_)
                | ClientSurface::CommandForm(_),
        )
    ) {
        *surface = None;
    }
    prefix.replace_bindings(candidate.bindings.clone());
    client_hooks.reconfigure(candidate.extensions.clone());
    *ui = candidate;
    let close_sidebar = if let (Some(ClientSurface::Sidebar(sidebar)), Some(snapshot)) =
        (surface.as_mut(), resources.snapshot())
    {
        !sidebar.reconfigure(
            snapshot,
            view.focused(),
            workspace_history,
            resources.notifications(),
            ui,
        )
    } else {
        false
    };
    if close_sidebar {
        *surface = None;
    }
    resize_view(framed, host, view, resources, ui).await?;
    view.invalidate_drawn();
    Ok(())
}

enum ProjectOpenPreparation {
    Ready {
        project: Option<String>,
        cwd: PathBuf,
    },
    Approval {
        project: String,
        cwd: PathBuf,
        path: PathBuf,
        digest: String,
        source: String,
    },
}

fn project_opener_mut(surface: &mut Option<ClientSurface>) -> &mut ProjectOpenerState {
    let Some(ClientSurface::ProjectOpener(opener)) = surface else {
        unreachable!("project opener event requires a project opener surface")
    };
    opener
}

async fn prepare_project_open(
    choice: ProjectOpenChoice,
    config_location: &config::ConfigLocation,
    extensions: &[crate::extensions::Extension],
) -> anyhow::Result<ProjectOpenPreparation> {
    let catalog = config::load_projects_location(config_location)?;
    let (project, cwd) = match choice {
        ProjectOpenChoice::Configured { name, path } => (Some(name), path),
        ProjectOpenChoice::Path(path) => {
            let resolver = crate::project::ProjectResolver::default();
            let requested = resolver
                .resolve(&path)
                .await
                .with_context(|| format!("resolve project path {}", path.display()))?;
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
                        "project identity for {} matches multiple configured projects",
                        path.display()
                    );
                }
                matched = Some(name.to_owned());
            }
            (matched, path)
        }
    };
    let Some(name) = project else {
        return Ok(ProjectOpenPreparation::Ready { project: None, cwd });
    };
    let configured = catalog
        .get(&name)
        .with_context(|| format!("configured project {name:?} is unavailable"))?;
    match crate::project_definition::load(Some(&name), configured, extensions) {
        Ok(_) => Ok(ProjectOpenPreparation::Ready {
            project: Some(name),
            cwd,
        }),
        Err(crate::project_definition::ProjectDefinitionError::UntrustedRecipe {
            path,
            digest,
            ..
        }) => {
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("read project recipe {} for review", path.display()))?;
            let reviewed_digest = format!("{:x}", Sha256::digest(source.as_bytes()));
            if reviewed_digest != digest {
                bail!("project recipe changed while preparing review; retry opening it");
            }
            Ok(ProjectOpenPreparation::Approval {
                project: name,
                cwd,
                path,
                digest,
                source,
            })
        }
        Err(error) => Err(error.into()),
    }
}

fn approve_project_recipe(
    project: &str,
    digest: &str,
    config_location: &config::ConfigLocation,
    extensions: &[crate::extensions::Extension],
) -> anyhow::Result<()> {
    let catalog = config::load_projects_location(config_location)?;
    let configured = catalog
        .get(project)
        .with_context(|| format!("configured project {project:?} is unavailable"))?;
    crate::project_definition::trust_digest(configured, extensions, digest)?;
    Ok(())
}

async fn request_project_open(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    opener: &mut ProjectOpenerState,
    project: Option<String>,
    cwd: PathBuf,
) -> anyhow::Result<()> {
    let request_id = Uuid::new_v4();
    opener.begin_open(request_id);
    send_request(
        framed,
        Some(request_id),
        ClientMessage::OpenLocation {
            project,
            name: None,
            cwd,
            program: None,
            argv: Vec::new(),
        },
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "the dispatcher explicitly borrows the small client states it coordinates"
)]
async fn dispatch_client_action(
    action: ClientAction,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    view: &mut ViewState,
    resources: &ResourceState,
    surface: &mut Option<ClientSurface>,
    workspace_history: &NavigationHistory,
    create: &mut CreateCoordinator,
    close_target: &mut CloseTargetState,
    focus: &mut FocusState,
    copy_mode: &mut Option<CopyModeState>,
    rename: &mut Option<RenameState>,
    host: Rect,
    ui: &mut UiConfig,
    temporary_command: &mut Option<TemporaryCommandSurface>,
    socket_path: &Path,
    background_results: &mpsc::UnboundedSender<BackgroundCommandResult>,
    config_location: &config::ConfigLocation,
    extension_reload: &mut Option<ExtensionReloadState>,
    project_config_reload: &mut Option<ProjectConfigReloadState>,
    focused_terminal_was_covered: bool,
) -> anyhow::Result<Option<Toast>> {
    match action {
        ClientAction::RunCommand(index) => {
            let Some(command) = ui.bindings.command(index) else {
                return Ok(Some(Toast::error(
                    "configured command is no longer available",
                )));
            };
            let command = command.clone();
            let workspace_context = command.extension.as_ref().and_then(|_| {
                resources.snapshot().and_then(|snapshot| {
                    snapshot.sessions.iter().find_map(|session| {
                        session
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.id == view.focused().workspace_id)
                            .map(|workspace| {
                                (
                                    workspace.root.clone(),
                                    session.trusted_project_config.clone(),
                                )
                            })
                    })
                })
            });
            if command.extension.is_some() && workspace_context.is_none() {
                return Ok(Some(Toast::error(
                    "command unavailable · focused workspace root is not loaded",
                )));
            }
            let (workspace_root, project_config) = workspace_context
                .map(|(root, config)| (Some(root), config))
                .unwrap_or((None, None));
            if command.mode() == crate::extensions::ExtensionCommandMode::Background {
                dispatch_background_command(
                    command,
                    ui.clone(),
                    view.focused().clone(),
                    workspace_root.expect("extension command resolved workspace root"),
                    project_config,
                    socket_path.to_owned(),
                    background_results.clone(),
                );
                return Ok(None);
            }
            let extension_context = workspace_root
                .as_deref()
                .map(|root| {
                    ExtensionCommandContext::resolve(
                        ui,
                        &command,
                        view.focused(),
                        root,
                        project_config.as_ref(),
                    )
                })
                .transpose();
            let extension_context = match extension_context {
                Ok(context) => context,
                Err(error) => {
                    return Ok(Some(Toast::error(format!(
                        "command config failed · {}",
                        one_line_error(&error)
                    ))));
                }
            };
            if !command.fields.is_empty() {
                let context = extension_context.expect("extension form has extension context");
                match CommandFormState::open(command, context) {
                    Ok(form) => *surface = Some(ClientSurface::CommandForm(Box::new(form))),
                    Err(error) => {
                        return Ok(Some(Toast::error(format!(
                            "command form failed · {}",
                            one_line_error(&error)
                        ))));
                    }
                }
                return Ok(None);
            }
            match spawn_temporary_command(
                &command,
                view.focused().child_pid,
                host,
                socket_path,
                extension_context.as_ref(),
                None,
            )
            .await
            {
                Ok(command) => *temporary_command = Some(command),
                Err(error) => {
                    return Ok(Some(Toast::error(format!(
                        "command failed · {}",
                        one_line_error(&error)
                    ))));
                }
            }
        }
        ClientAction::OpenCommandBar => {
            *surface = Some(ClientSurface::CommandBar(
                CommandBarState::open_with_bindings(ui.bindings.clone()),
            ));
        }
        ClientAction::OpenProject => {
            let catalog = match config::load_projects_location(config_location) {
                Ok(catalog) => catalog,
                Err(error) => {
                    return Ok(Some(Toast::error(format!(
                        "project catalog failed · {}",
                        one_line_error(&error)
                    ))));
                }
            };
            let base = resources
                .snapshot()
                .and_then(|snapshot| {
                    snapshot.sessions.iter().find_map(|session| {
                        session
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.id == view.focused().workspace_id)
                            .map(|workspace| workspace.root.clone())
                    })
                })
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("/"));
            *surface = Some(ClientSurface::ProjectOpener(ProjectOpenerState::open(
                &catalog, base,
            )));
        }
        ClientAction::ReloadConfig => {
            if extension_reload.is_some() || project_config_reload.is_some() {
                return Ok(Some(Toast::info("config reload already in progress")));
            }
            let staged = match stage_ui_config(config_location) {
                Ok(staged) => staged,
                Err(error) => {
                    return Ok(Some(Toast::error(format!(
                        "config reload failed · {}",
                        one_line_error(&error)
                    ))));
                }
            };
            let request_id = Uuid::new_v4();
            send_request(
                framed,
                Some(request_id),
                ClientMessage::PrepareExtensionReload,
            )
            .await?;
            *extension_reload = Some(ExtensionReloadState {
                session_id: view.focused().session_id,
                phase: ExtensionReloadPhase::Preparing { request_id, staged },
            });
            return Ok(Some(Toast::info("validating config reload")));
        }
        ClientAction::ReloadProjectConfig => {
            if extension_reload.is_some() || project_config_reload.is_some() {
                return Ok(Some(Toast::info("config reload already in progress")));
            }
            let request_id = Uuid::new_v4();
            send_request(
                framed,
                Some(request_id),
                ClientMessage::ReloadProjectConfig {
                    session_id: view.focused().session_id,
                },
            )
            .await?;
            *project_config_reload = Some(ProjectConfigReloadState {
                request_id,
                kind: ProjectConfigReloadKind::ProjectOnly,
            });
            return Ok(Some(Toast::info("validating project config reload")));
        }
        ClientAction::EnterCopyMode => {
            if copy_mode.is_none() {
                let mut state = CopyModeState::enter(view.focused().terminal_id);
                pump_copy_mode(framed, &mut state).await?;
                *copy_mode = Some(state);
            }
        }
        ClientAction::OpenNavigator => {
            let mut navigator = NavigatorState::open();
            if let Some(snapshot) = resources.snapshot() {
                navigator.accept_resources_with_notifications(
                    snapshot,
                    view.focused(),
                    workspace_history,
                    resources.notifications(),
                );
            }
            navigator.accept_presence(resources.presence());
            *surface = Some(ClientSurface::Navigator(navigator));
        }
        ClientAction::OpenAgents => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("agents are still loading")));
            };
            *surface = Some(ClientSurface::Agents(AgentsDialog::open(
                snapshot,
                view.focused(),
                resources.notifications(),
            )));
        }
        ClientAction::OpenLeftSidebar | ClientAction::OpenRightSidebar => {
            let side = if action == ClientAction::OpenLeftSidebar {
                SidebarSide::Left
            } else {
                SidebarSide::Right
            };
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("workspaces are still loading")));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some(Toast::error("navigation is syncing")));
            }
            let Some(sidebar) = SidebarState::open(
                snapshot,
                view.focused(),
                workspace_history,
                resources.notifications(),
                side,
                ui,
            ) else {
                return Ok(Some(Toast::error("sidebar has no components")));
            };
            *surface = Some(ClientSurface::Sidebar(sidebar));
        }
        ClientAction::OpenTabBar => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("tabs are still loading")));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some(Toast::error("navigation is syncing")));
            }
            if client_layout(host, ui, resources.sidebar_relevance(view.focused(), ui))
                .tab_bar
                .is_none()
            {
                return Ok(Some(Toast::error("tab bar is unavailable at this size")));
            }
            let Some(tab_bar) = TabBarState::open(snapshot, view.focused(), workspace_history)
            else {
                return Ok(Some(Toast::error("no tab available")));
            };
            *surface = Some(ClientSurface::TabBar(tab_bar));
        }
        ClientAction::OpenNotifications => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("notifications are still loading")));
            };
            *surface = Some(ClientSurface::Notifications(NotificationsDialog::open(
                snapshot,
                resources.notifications(),
            )));
        }
        ClientAction::FocusNextNotification => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("notifications are still loading")));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some(Toast::error("navigation is syncing")));
            }
            let Some(action) = resources.notifications().next_action(
                snapshot,
                view.focused().terminal_id,
                focused_terminal_was_covered,
            ) else {
                return Ok(None);
            };
            match action {
                DirectAttentionAction::RevealCurrent => {
                    // The invoking surface has just been dismissed. Do not
                    // acknowledge content it covered; the next draw will do
                    // that only after the managed terminal is visible.
                    view.invalidate_drawn();
                    return Ok(None);
                }
                DirectAttentionAction::AcknowledgeCurrent(acknowledgements) => {
                    for acknowledgement in acknowledgements.into_iter().flatten() {
                        acknowledge_attention(framed, acknowledgement).await?;
                    }
                    return Ok(Some(Toast::info("current attention cleared")));
                }
                DirectAttentionAction::Navigate(pane_id) => {
                    if let Some(request) = focus.begin(FocusOrigin::Notification) {
                        send_request(
                            framed,
                            Some(request),
                            ClientMessage::SelectTarget {
                                selector: TargetSelector::Pane(pane_id),
                                expected: None,
                            },
                        )
                        .await?;
                    }
                }
            }
        }
        ClientAction::CreateWorkspace => {
            if let Some(request) = create.begin(CreateKind::Workspace) {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::CreateWorkspace {
                        session_id: view.focused().session_id,
                        name: None,
                        cwd: None,
                        program: None,
                        argv: Vec::new(),
                    },
                )
                .await?;
            }
        }
        ClientAction::CreateTab => {
            if let Some(request) = create.begin(CreateKind::Tab) {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::CreateTab {
                        workspace_id: view.focused().workspace_id,
                        name: None,
                        cwd: None,
                        program: None,
                        argv: Vec::new(),
                    },
                )
                .await?;
            }
        }
        ClientAction::RenameSession | ClientAction::RenameWorkspace | ClientAction::RenameTab => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("resources are still loading")));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some(Toast::error("resources are syncing")));
            }
            let Some(session) = snapshot
                .sessions
                .iter()
                .find(|session| session.id == view.focused().session_id)
            else {
                return Ok(Some(Toast::error("focused session is unavailable")));
            };
            let target = match action {
                ClientAction::RenameSession => Some((
                    crate::protocol::RenameSelector::Session(
                        crate::resources::SessionSelector::Id(session.id),
                    ),
                    "session",
                    session.name.clone(),
                )),
                ClientAction::RenameWorkspace => session
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == view.focused().workspace_id)
                    .map(|workspace| {
                        (
                            crate::protocol::RenameSelector::Workspace(workspace.id),
                            "workspace",
                            workspace.name.clone(),
                        )
                    }),
                ClientAction::RenameTab => session
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == view.focused().workspace_id)
                    .and_then(|workspace| {
                        workspace
                            .tabs
                            .iter()
                            .find(|tab| tab.id == view.focused().tab_id)
                    })
                    .map(|tab| {
                        (
                            crate::protocol::RenameSelector::Tab(tab.id),
                            "tab",
                            tab.name.clone(),
                        )
                    }),
                _ => unreachable!("rename action guard"),
            };
            let Some((selector, kind, name)) = target else {
                return Ok(Some(Toast::error("focused resource is unavailable")));
            };
            *rename = Some(RenameState::open(selector, kind, name));
        }
        ClientAction::FocusNextTab | ClientAction::FocusPreviousTab => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("tabs are still loading")));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some(Toast::error("navigation is syncing")));
            }
            let forward = action == ClientAction::FocusNextTab;
            if let Some(pane_id) = workspace_history.adjacent_tab(snapshot, view.focused(), forward)
                && let Some(request) = focus.begin(FocusOrigin::Tab)
            {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::SelectTarget {
                        selector: TargetSelector::Pane(pane_id),
                        expected: selection_expectation(snapshot, pane_id, NavigationScope::Tab),
                    },
                )
                .await?;
            }
        }
        ClientAction::FocusNextWorkspace | ClientAction::FocusPreviousWorkspace => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("workspaces are still loading")));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some(Toast::error("navigation is syncing")));
            }
            let forward = action == ClientAction::FocusNextWorkspace;
            if let Some(pane_id) =
                workspace_history.adjacent_workspace(snapshot, view.focused(), forward)
                && let Some(request) = focus.begin(FocusOrigin::Workspace)
            {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::SelectTarget {
                        selector: TargetSelector::Pane(pane_id),
                        expected: selection_expectation(
                            snapshot,
                            pane_id,
                            NavigationScope::Workspace,
                        ),
                    },
                )
                .await?;
            }
        }
        ClientAction::SplitPaneRight | ClientAction::SplitPaneDown => {
            if let Some(request) = create.begin(CreateKind::SplitPane) {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::SplitPane {
                        pane_id: view.focused().pane_id,
                        direction: if action == ClientAction::SplitPaneRight {
                            crate::splits::SplitDirection::Right
                        } else {
                            crate::splits::SplitDirection::Down
                        },
                        cwd: None,
                        program: None,
                        argv: Vec::new(),
                    },
                )
                .await?;
            }
        }
        ClientAction::FocusNextPane | ClientAction::FocusPreviousPane => {
            let forward = action == ClientAction::FocusNextPane;
            if let Some(target) = view.cycle(forward)
                && let Some(request) = focus.begin(FocusOrigin::Pane)
            {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::SelectTarget {
                        selector: TargetSelector::Pane(target.pane_id),
                        expected: Some(SelectionExpectation::Tab(view.focused().tab_id)),
                    },
                )
                .await?;
            }
        }
        ClientAction::FocusPane(direction) => {
            let terminal =
                client_layout(host, ui, resources.sidebar_relevance(view.focused(), ui)).terminal;
            if let Some(target) = view.directional(direction, terminal, ui.pane_layout)
                && let Some(request) = focus.begin(FocusOrigin::Pane)
            {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::SelectTarget {
                        selector: TargetSelector::Pane(target.pane_id),
                        expected: Some(SelectionExpectation::Tab(view.focused().tab_id)),
                    },
                )
                .await?;
            }
        }
        ClientAction::FocusLast(scope) => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("resources are still loading")));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some(Toast::error("navigation is syncing")));
            }
            let pane_id = match scope {
                HistoryScope::Pane => workspace_history.last_pane(snapshot, view.focused()),
                HistoryScope::Tab => workspace_history.last_tab(snapshot, view.focused()),
                HistoryScope::Workspace => {
                    workspace_history.last_workspace(snapshot, view.focused())
                }
                HistoryScope::Session => workspace_history.last_session(snapshot, view.focused()),
            };
            let Some(pane_id) = pane_id else {
                return Ok(Some(Toast::error(format!("no previous {}", scope.label()))));
            };
            let navigation_scope = scope.navigation_scope();
            if let Some(request) = focus.begin(FocusOrigin::for_history_scope(scope)) {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::SelectTarget {
                        selector: TargetSelector::Pane(pane_id),
                        expected: selection_expectation(snapshot, pane_id, navigation_scope),
                    },
                )
                .await?;
            }
        }
        ClientAction::FocusTab(number) => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some(Toast::error("tabs are still loading")));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some(Toast::error("navigation is syncing")));
            }
            let number = number.get();
            let Some(pane_id) = workspace_history.numbered_tab(snapshot, view.focused(), number)
            else {
                return Ok(Some(Toast::error(format!("tab {number} is unavailable"))));
            };
            if let Some(request) = focus.begin(FocusOrigin::Tab) {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::SelectTarget {
                        selector: TargetSelector::Pane(pane_id),
                        expected: selection_expectation(snapshot, pane_id, NavigationScope::Tab),
                    },
                )
                .await?;
            }
        }
        ClientAction::TogglePaneZoom => {
            let Some(_) = view.toggle_zoom() else {
                return Ok(Some(Toast::error("pane zoom needs more than one pane")));
            };
            resize_view(framed, host, view, resources, ui).await?;
        }
        ClientAction::CloseSession
        | ClientAction::CloseWorkspace
        | ClientAction::CloseTab
        | ClientAction::ClosePane => {
            let (selector, label) = match action {
                ClientAction::CloseSession => (
                    TargetSelector::Session(crate::resources::SessionSelector::Id(
                        view.focused().session_id,
                    )),
                    "session",
                ),
                ClientAction::CloseWorkspace => (
                    TargetSelector::Workspace(view.focused().workspace_id),
                    "workspace",
                ),
                ClientAction::CloseTab => (TargetSelector::Tab(view.focused().tab_id), "tab"),
                ClientAction::ClosePane => (TargetSelector::Pane(view.focused().pane_id), "pane"),
                _ => unreachable!("close action guard"),
            };
            return start_target_close(framed, close_target, selector, label, ui.confirm_close)
                .await;
        }
        ClientAction::Detach => send(framed, ClientMessage::Detach).await?,
    }
    Ok(None)
}

async fn spawn_temporary_command(
    command: &PaletteCommand,
    child_pid: u32,
    host: Rect,
    socket_path: &Path,
    extension_context: Option<&ExtensionCommandContext>,
    form: Option<&BTreeMap<String, String>>,
) -> anyhow::Result<TemporaryCommandSurface> {
    let crate::extensions::ExtensionCommandExecution::Interactive { size, .. } = &command.execution
    else {
        anyhow::bail!("background commands cannot open an interactive surface");
    };
    let content = temporary_command_content(size.area(host));
    let fallback = std::env::current_dir().unwrap_or_else(|_| "/".into());
    TemporaryCommandSurface::spawn(
        command,
        child_pid,
        &fallback,
        TerminalSize {
            columns: content.width,
            rows: content.height,
        },
        socket_path,
        extension_context,
        form,
    )
    .await
}

async fn pump_copy_mode(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    state: &mut CopyModeState,
) -> anyhow::Result<()> {
    let Some(submission) = state.start_next() else {
        return Ok(());
    };
    send_request(
        framed,
        Some(submission.request_id),
        ClientMessage::CopyMode {
            terminal_id: state.terminal_id(),
            action: submission.action,
        },
    )
    .await
}

fn spawn_pbcopy(request_id: Uuid, text: String, results: mpsc::Sender<ClipboardResult>) {
    tokio::spawn(async move {
        let result = pbcopy(text).await;
        let _ = results
            .send(match result {
                Ok(bytes) => ClipboardResult::Copied { request_id, bytes },
                Err(error) => ClipboardResult::Failed {
                    request_id,
                    message: error.to_string(),
                },
            })
            .await;
    });
}

async fn pbcopy(text: String) -> anyhow::Result<usize> {
    copy_to_clipboard(Path::new("pbcopy"), text, PBCOPY_TIMEOUT).await
}

async fn copy_to_clipboard(
    program: &Path,
    text: String,
    deadline: Duration,
) -> anyhow::Result<usize> {
    let bytes = text.len();
    let mut child = tokio::process::Command::new(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start {}", program.display()))?;
    let Some(mut stdin) = child.stdin.take() else {
        kill_and_reap(&mut child).await;
        bail!("open clipboard process stdin");
    };
    let operation = async {
        stdin
            .write_all(text.as_bytes())
            .await
            .context("write selected text to clipboard process")?;
        stdin
            .shutdown()
            .await
            .context("close clipboard process stdin")?;
        drop(stdin);
        let status = child.wait().await.context("wait for clipboard process")?;
        if !status.success() {
            bail!("clipboard process exited with {status}");
        }
        Ok(bytes)
    };

    match time::timeout(deadline, operation).await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => {
            kill_and_reap(&mut child).await;
            Err(error)
        }
        Err(_) => {
            kill_and_reap(&mut child).await;
            bail!(
                "clipboard process timed out after {} ms",
                deadline.as_millis()
            )
        }
    }
}

async fn kill_and_reap(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = time::timeout(PBCOPY_REAP_TIMEOUT, child.wait()).await;
}

async fn send(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    message: ClientMessage,
) -> anyhow::Result<()> {
    send_request(framed, None, message).await
}

async fn acknowledge_attention(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    acknowledgement: AttentionAcknowledgement,
) -> anyhow::Result<()> {
    let message = match acknowledgement {
        AttentionAcknowledgement::Agent {
            terminal_id,
            event_revision,
        } => ClientMessage::AcknowledgeAgent {
            terminal_id,
            event_revision,
        },
        AttentionAcknowledgement::Alert {
            terminal_id,
            observed,
        } => ClientMessage::AcknowledgeAlerts {
            terminal_id,
            observed,
        },
    };
    send(framed, message).await
}

async fn send_request(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    request_id: Option<Uuid>,
    message: ClientMessage,
) -> anyhow::Result<()> {
    framed
        .send(Bytes::from(encode_payload(&Envelope {
            request_id,
            message,
        })?))
        .await?;
    Ok(())
}

async fn receive(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
) -> anyhow::Result<ServerMessage> {
    let frame = framed
        .next()
        .await
        .context("daemon disconnected during handshake")??;
    Ok(decode_payload::<Envelope<ServerMessage>>(&frame)?.message)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CreateKind {
    Workspace,
    Tab,
    SplitPane,
}

impl CreateKind {
    fn label(self) -> &'static str {
        match self {
            Self::Workspace => "create workspace",
            Self::Tab => "create tab",
            Self::SplitPane => "split",
        }
    }
}

#[derive(Default)]
struct CreateCoordinator {
    flow: CreateFlow,
}

#[derive(Default)]
enum CreateFlow {
    #[default]
    Idle,
    AwaitingCreated {
        kind: CreateKind,
        request_id: Uuid,
    },
    AwaitingSelected {
        kind: CreateKind,
        request_id: Uuid,
        terminal_id: TerminalId,
    },
    AwaitingResources {
        kind: CreateKind,
        request_id: Uuid,
        resource_revision: u64,
    },
}

impl CreateCoordinator {
    fn begin(&mut self, kind: CreateKind) -> Option<Uuid> {
        if !matches!(self.flow, CreateFlow::Idle) {
            return None;
        }
        let request_id = Uuid::new_v4();
        self.flow = CreateFlow::AwaitingCreated { kind, request_id };
        Some(request_id)
    }

    fn created(
        &mut self,
        request_id: Option<Uuid>,
        kind: CreateKind,
        terminal_id: TerminalId,
    ) -> bool {
        let CreateFlow::AwaitingCreated {
            kind: expected_kind,
            request_id: expected,
        } = self.flow
        else {
            return false;
        };
        if request_id != Some(expected) || kind != expected_kind {
            return false;
        }
        self.flow = CreateFlow::AwaitingSelected {
            kind,
            request_id: expected,
            terminal_id,
        };
        true
    }

    fn selected(
        &mut self,
        request_id: Option<Uuid>,
        selected: &SelectedView,
        observed_revision: Option<u64>,
    ) -> Option<CreateKind> {
        let CreateFlow::AwaitingSelected {
            kind,
            request_id: expected,
            terminal_id,
        } = self.flow
        else {
            return None;
        };
        if request_id != Some(expected) || terminal_id != selected.focused.terminal_id {
            return None;
        }
        if observed_revision.is_some_and(|revision| revision >= selected.resource_revision) {
            self.flow = CreateFlow::Idle;
        } else {
            self.flow = CreateFlow::AwaitingResources {
                kind,
                request_id: expected,
                resource_revision: selected.resource_revision,
            };
        }
        Some(kind)
    }

    fn accept_resources(&mut self, revision: u64) -> bool {
        let CreateFlow::AwaitingResources {
            resource_revision, ..
        } = self.flow
        else {
            return false;
        };
        if revision < resource_revision {
            return false;
        }
        self.flow = CreateFlow::Idle;
        true
    }

    fn fail(&mut self, request_id: Option<Uuid>) -> Option<CreateKind> {
        let (kind, expected) = match self.flow {
            CreateFlow::Idle => return None,
            CreateFlow::AwaitingCreated { kind, request_id }
            | CreateFlow::AwaitingSelected {
                kind, request_id, ..
            }
            | CreateFlow::AwaitingResources {
                kind, request_id, ..
            } => (kind, request_id),
        };
        if request_id != Some(expected) {
            return None;
        }
        self.flow = CreateFlow::Idle;
        Some(kind)
    }

    fn blocks_input(&self) -> bool {
        !matches!(self.flow, CreateFlow::Idle)
    }
}

#[derive(Default)]
enum CloseTargetState {
    #[default]
    Idle,
    Confirming {
        selector: TargetSelector,
    },
    Closing(Uuid),
}

async fn start_target_close(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    state: &mut CloseTargetState,
    selector: TargetSelector,
    label: &'static str,
    confirm: bool,
) -> anyhow::Result<Option<Toast>> {
    match state.begin(selector, confirm) {
        CloseTargetStart::Busy => Ok(None),
        CloseTargetStart::Confirming => Ok(Some(Toast::prompt(format!("Close {label}? (y/n)")))),
        CloseTargetStart::Closing {
            request_id,
            selector,
        } => {
            send_request(
                framed,
                Some(request_id),
                ClientMessage::CloseTarget { selector },
            )
            .await?;
            Ok(None)
        }
    }
}

enum CloseTargetStart {
    Busy,
    Confirming,
    Closing {
        request_id: Uuid,
        selector: TargetSelector,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum CloseConfirmationInput {
    Pass,
    Confirm {
        request_id: Uuid,
        selector: TargetSelector,
    },
    Cancel,
    Consume,
}

impl CloseTargetState {
    fn begin(&mut self, selector: TargetSelector, confirm: bool) -> CloseTargetStart {
        if !matches!(self, Self::Idle) {
            return CloseTargetStart::Busy;
        }
        if confirm {
            *self = Self::Confirming { selector };
            return CloseTargetStart::Confirming;
        }
        let request_id = Uuid::new_v4();
        *self = Self::Closing(request_id);
        CloseTargetStart::Closing {
            request_id,
            selector,
        }
    }

    fn is_confirming(&self) -> bool {
        matches!(self, Self::Confirming { .. })
    }

    fn route_confirmation_input(&mut self, event: &Event) -> CloseConfirmationInput {
        if !self.is_confirming() {
            return CloseConfirmationInput::Pass;
        }
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    let (request_id, selector) = self
                        .confirm()
                        .expect("confirming close has a captured target");
                    CloseConfirmationInput::Confirm {
                        request_id,
                        selector,
                    }
                } else {
                    self.cancel();
                    CloseConfirmationInput::Cancel
                }
            }
            Event::Resize(_, _) => CloseConfirmationInput::Pass,
            _ => CloseConfirmationInput::Consume,
        }
    }

    fn confirm(&mut self) -> Option<(Uuid, TargetSelector)> {
        let Self::Confirming { selector, .. } = self else {
            return None;
        };
        let selector = selector.clone();
        let request_id = Uuid::new_v4();
        *self = Self::Closing(request_id);
        Some((request_id, selector))
    }

    fn cancel(&mut self) {
        if matches!(self, Self::Confirming { .. }) {
            *self = Self::Idle;
        }
    }

    fn complete(&mut self, request_id: Option<Uuid>) -> bool {
        self.finish(request_id)
    }

    fn fail(&mut self, request_id: Option<Uuid>) -> bool {
        self.finish(request_id)
    }

    fn finish(&mut self, request_id: Option<Uuid>) -> bool {
        let Self::Closing(expected) = self else {
            return false;
        };
        if request_id != Some(*expected) {
            return false;
        }
        *self = Self::Idle;
        true
    }

    fn blocks_input(&self) -> bool {
        matches!(self, Self::Closing(_))
    }
}

#[derive(Default)]
struct FocusState {
    request_id: Option<Uuid>,
    origin: Option<FocusOrigin>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusOrigin {
    Pane,
    Tab,
    Workspace,
    Session,
    Sidebar {
        scope: NavigationScope,
        component: SidebarComponentKind,
    },
    Notification,
}

impl FocusOrigin {
    fn for_history_scope(scope: HistoryScope) -> Self {
        match scope {
            HistoryScope::Pane => Self::Pane,
            HistoryScope::Tab => Self::Tab,
            HistoryScope::Workspace => Self::Workspace,
            HistoryScope::Session => Self::Session,
        }
    }
}

impl FocusState {
    fn begin(&mut self, origin: FocusOrigin) -> Option<Uuid> {
        if self.request_id.is_some() {
            return None;
        }
        let request_id = Uuid::new_v4();
        self.request_id = Some(request_id);
        self.origin = Some(origin);
        Some(request_id)
    }

    fn complete(&mut self, request_id: Option<Uuid>) -> Option<FocusOrigin> {
        if request_id.is_none() || request_id != self.request_id {
            return None;
        }
        self.request_id = None;
        self.origin.take()
    }
}

struct PaneState {
    target: SelectedTarget,
    newest_revision: Option<u64>,
    drawn_revision: Option<u64>,
    /// The materialized grid this pane renders: a full snapshot with
    /// [`ScreenDelta`] rows spliced in as they arrive.
    pending: Option<ScreenSnapshot>,
    last_size: Option<TerminalSize>,
    resize_request_id: Option<Uuid>,
    constrained_size: bool,
    /// Set once a [`ClientMessage::RefreshTerminal`] has been sent for an
    /// unapplicable delta, and cleared when a full snapshot arrives, so a
    /// run of mismatched deltas sends at most one refresh request.
    refresh_requested: bool,
}

/// Outcome of applying a [`ServerMessage::SnapshotDelta`] to a [`PaneState`].
enum DeltaOutcome {
    Applied,
    /// The delta's base revision or size didn't match the pane's current
    /// grid; the caller should drop it and request a full resync.
    Mismatch,
}

impl PaneState {
    fn new(target: SelectedTarget) -> Self {
        Self {
            target,
            newest_revision: None,
            drawn_revision: None,
            pending: None,
            last_size: None,
            resize_request_id: None,
            constrained_size: false,
            refresh_requested: false,
        }
    }

    fn complete_resize(&mut self, request_id: Uuid, size: TerminalSize) -> bool {
        if self.resize_request_id != Some(request_id) {
            return false;
        }
        self.observe_authoritative_size(size);
        self.resize_request_id = None;
        true
    }

    fn observe_authoritative_size(&mut self, size: TerminalSize) {
        self.constrained_size = self.last_size.is_some_and(|requested| {
            size.columns < requested.columns || size.rows < requested.rows
        });
    }

    fn accept(&mut self, screen: ScreenSnapshot) -> bool {
        if self
            .newest_revision
            .is_some_and(|revision| screen.revision <= revision)
        {
            return false;
        }
        if self.resize_request_id.is_none() {
            self.observe_authoritative_size(screen.size);
        }
        self.newest_revision = Some(screen.revision);
        self.pending = Some(screen);
        self.refresh_requested = false;
        true
    }

    fn accept_delta(&mut self, delta: ScreenDelta) -> DeltaOutcome {
        let Some(current) = self.pending.as_mut() else {
            return DeltaOutcome::Mismatch;
        };
        let size = delta.size;
        let revision = delta.revision;
        if current.apply_delta(delta).is_err() {
            return DeltaOutcome::Mismatch;
        }
        if self.resize_request_id.is_none() {
            self.observe_authoritative_size(size);
        }
        self.newest_revision = Some(revision);
        DeltaOutcome::Applied
    }
}

/// Result of [`ViewState::accept_delta`].
enum DeltaApplyResult {
    Applied,
    /// The delta couldn't be applied; the pane needs a full resync.
    NeedsRefresh,
    /// No open pane matches, or a refresh is already outstanding.
    Ignored,
}

struct ViewState {
    focused: TerminalId,
    selected_revision: u64,
    panes: Vec<PaneState>,
    layout: SplitTree,
    zoomed: bool,
    resize_requests: HashMap<Uuid, TerminalId>,
}

impl ViewState {
    fn new(selected: SelectedView) -> anyhow::Result<Self> {
        let focused = selected.focused.terminal_id;
        let mut view = Self {
            focused,
            selected_revision: 0,
            panes: Vec::new(),
            layout: selected.layout.clone(),
            zoomed: false,
            resize_requests: HashMap::new(),
        };
        view.replace(selected)?;
        Ok(view)
    }

    fn replace(&mut self, selected: SelectedView) -> anyhow::Result<bool> {
        if selected.resource_revision < self.selected_revision {
            return Ok(false);
        }
        let previous_focus = self.focused;
        let previous_tab = self
            .panes
            .iter()
            .find(|pane| pane.target.terminal_id == self.focused)
            .map(|pane| pane.target.tab_id);
        if selected.panes.is_empty() {
            bail!("daemon selected an empty pane view");
        }
        let focused = selected.focused.terminal_id;
        if selected
            .panes
            .iter()
            .filter(|pane| pane.terminal_id == focused)
            .count()
            != 1
        {
            bail!("daemon pane view does not contain its focused terminal exactly once");
        }
        if selected
            .panes
            .iter()
            .find(|pane| pane.terminal_id == focused)
            != Some(&selected.focused)
        {
            bail!("daemon pane view focus metadata is inconsistent");
        }
        if selected
            .panes
            .iter()
            .any(|pane| pane.tab_id != selected.focused.tab_id)
        {
            bail!("daemon pane view spans multiple tabs");
        }
        if !selected.layout.validate() {
            bail!("daemon pane view contains an invalid split layout");
        }
        for (index, pane) in selected.panes.iter().enumerate() {
            if selected.panes[..index].iter().any(|previous| {
                previous.terminal_id == pane.terminal_id || previous.pane_id == pane.pane_id
            }) {
                bail!("daemon pane view contains a duplicate pane or terminal");
            }
        }
        if selected.layout.leaf_ids()
            != selected
                .panes
                .iter()
                .map(|pane| pane.pane_id)
                .collect::<Vec<_>>()
        {
            bail!("daemon pane layout does not match its pane order");
        }

        let mut old = std::mem::take(&mut self.panes);
        self.panes = selected
            .panes
            .into_iter()
            .map(|target| {
                if let Some(index) = old
                    .iter()
                    .position(|pane| pane.target.terminal_id == target.terminal_id)
                {
                    let mut pane = old.remove(index);
                    pane.target = target;
                    pane
                } else {
                    PaneState::new(target)
                }
            })
            .collect();
        self.focused = focused;
        self.layout = selected.layout;
        self.selected_revision = selected.resource_revision;
        if previous_tab.is_some_and(|tab_id| tab_id != selected.focused.tab_id)
            || self.panes.len() < 2
        {
            self.zoomed = false;
        }
        if previous_focus != focused
            && let Some(pane) = self
                .panes
                .iter_mut()
                .find(|pane| pane.target.terminal_id == focused)
        {
            pane.last_size = None;
        }
        Ok(true)
    }

    fn focused(&self) -> &SelectedTarget {
        &self
            .panes
            .iter()
            .find(|pane| pane.target.terminal_id == self.focused)
            .expect("focused terminal belongs to the client view")
            .target
    }

    fn resources_are_current(&self, snapshot: &crate::resources::ResourceSnapshot) -> bool {
        snapshot.revision >= self.selected_revision
    }

    fn is_zoomed(&self) -> bool {
        self.zoomed
    }

    fn toggle_zoom(&mut self) -> Option<bool> {
        if self.panes.len() < 2 {
            return None;
        }
        self.zoomed = !self.zoomed;
        self.invalidate_drawn();
        Some(self.zoomed)
    }

    fn accept(&mut self, terminal_id: TerminalId, screen: ScreenSnapshot) -> bool {
        self.panes
            .iter_mut()
            .find(|pane| pane.target.terminal_id == terminal_id)
            .is_some_and(|pane| pane.accept(screen))
    }

    fn mark_resize_requested(&mut self, terminal_id: TerminalId, request_id: Uuid) {
        if let Some(pane) = self
            .panes
            .iter_mut()
            .find(|pane| pane.target.terminal_id == terminal_id)
        {
            pane.resize_request_id = Some(request_id);
            self.resize_requests.insert(request_id, terminal_id);
        }
    }

    fn complete_resize(
        &mut self,
        request_id: Option<Uuid>,
        terminal_id: TerminalId,
        size: TerminalSize,
    ) -> bool {
        let Some(request_id) = request_id else {
            return false;
        };
        if self.resize_requests.get(&request_id) != Some(&terminal_id) {
            return false;
        }
        self.resize_requests.remove(&request_id);
        self.panes
            .iter_mut()
            .find(|pane| pane.target.terminal_id == terminal_id)
            .is_none_or(|pane| {
                if pane.resize_request_id == Some(request_id) {
                    pane.complete_resize(request_id, size)
                } else {
                    true
                }
            })
    }

    fn reject_resize(&mut self, request_id: Option<Uuid>) -> bool {
        let Some(request_id) = request_id else {
            return false;
        };
        let Some(terminal_id) = self.resize_requests.remove(&request_id) else {
            return false;
        };
        if let Some(pane) = self
            .panes
            .iter_mut()
            .find(|pane| pane.target.terminal_id == terminal_id)
            .filter(|pane| pane.resize_request_id == Some(request_id))
        {
            pane.resize_request_id = None;
            pane.last_size = None;
        }
        true
    }

    /// Applies a delta for `terminal_id`'s pane, if that pane is currently
    /// open. Rate-limited per pane: [`DeltaApplyResult::NeedsRefresh`] is
    /// returned at most once per run of unapplicable deltas, so a burst of
    /// mismatches can't spam the daemon with refresh requests.
    fn accept_delta(&mut self, terminal_id: TerminalId, delta: ScreenDelta) -> DeltaApplyResult {
        let Some(pane) = self
            .panes
            .iter_mut()
            .find(|pane| pane.target.terminal_id == terminal_id)
        else {
            return DeltaApplyResult::Ignored;
        };
        match pane.accept_delta(delta) {
            DeltaOutcome::Applied => DeltaApplyResult::Applied,
            DeltaOutcome::Mismatch if pane.refresh_requested => DeltaApplyResult::Ignored,
            DeltaOutcome::Mismatch => {
                pane.refresh_requested = true;
                DeltaApplyResult::NeedsRefresh
            }
        }
    }

    fn needs_draw(&self) -> bool {
        self.panes
            .iter()
            .any(|pane| pane.newest_revision != pane.drawn_revision)
    }

    fn mark_drawn(&mut self) {
        for pane in &mut self.panes {
            pane.drawn_revision = pane.newest_revision;
        }
    }

    fn invalidate_drawn(&mut self) {
        for pane in &mut self.panes {
            pane.drawn_revision = None;
        }
    }

    fn cycle(&self, forward: bool) -> Option<SelectedTarget> {
        if self.panes.len() < 2 {
            return None;
        }
        let current = self
            .panes
            .iter()
            .position(|pane| pane.target.terminal_id == self.focused)
            .expect("focused terminal belongs to the client view");
        let next = if forward {
            (current + 1) % self.panes.len()
        } else if current == 0 {
            self.panes.len() - 1
        } else {
            current - 1
        };
        Some(self.panes[next].target.clone())
    }

    fn directional(
        &self,
        direction: FocusDirection,
        area: Rect,
        policy: PaneLayoutPolicy,
    ) -> Option<SelectedTarget> {
        if self.panes.len() < 2 {
            return None;
        }
        match policy {
            PaneLayoutPolicy::Splits => {
                let focused = self.focused().pane_id;
                let layout = authored_navigation_layout(area, &self.layout, focused);
                let order = self
                    .panes
                    .iter()
                    .map(|pane| pane.target.pane_id)
                    .collect::<Vec<_>>();
                let pane_id = directional_neighbor(&layout.panes, &order, focused, direction)?;
                self.panes
                    .iter()
                    .find(|pane| pane.target.pane_id == pane_id)
                    .map(|pane| pane.target.clone())
            }
            PaneLayoutPolicy::Accordion => {
                let order = self.terminal_ids();
                let layout = navigation_pane_layouts(area, &order, self.focused);
                let panes = layout
                    .into_iter()
                    .map(|(terminal_id, layout)| (terminal_id, layout.content))
                    .collect();
                let terminal_id = directional_neighbor(&panes, &order, self.focused, direction)?;
                self.panes
                    .iter()
                    .find(|pane| pane.target.terminal_id == terminal_id)
                    .map(|pane| pane.target.clone())
            }
        }
    }

    fn remove(&mut self, terminal_id: TerminalId) {
        let Some(index) = self
            .panes
            .iter()
            .position(|pane| pane.target.terminal_id == terminal_id)
        else {
            return;
        };
        let pane_id = self.panes[index].target.pane_id;
        self.panes.remove(index);
        if let Some(layout) = self.layout.clone().without(pane_id) {
            self.layout = layout;
        }
        if self.panes.len() < 2 {
            self.zoomed = false;
        }
        if self.focused == terminal_id
            && let Some(pane) = self.panes.get(index).or_else(|| self.panes.last())
        {
            self.focused = pane.target.terminal_id;
        }
    }

    fn terminal_ids(&self) -> Vec<TerminalId> {
        self.panes
            .iter()
            .map(|pane| pane.target.terminal_id)
            .collect()
    }

    fn pane_layouts(
        &self,
        area: Rect,
        policy: PaneLayoutPolicy,
    ) -> (
        std::collections::BTreeMap<TerminalId, PaneLayout>,
        Vec<SplitDivider>,
    ) {
        match policy {
            PaneLayoutPolicy::Accordion => (
                pane_layouts(area, &self.terminal_ids(), self.focused, self.zoomed),
                Vec::new(),
            ),
            PaneLayoutPolicy::Splits => {
                let authored =
                    authored_layout(area, &self.layout, self.focused().pane_id, self.zoomed);
                let panes = authored
                    .panes
                    .into_iter()
                    .filter_map(|(pane_id, content)| {
                        self.panes
                            .iter()
                            .find(|pane| pane.target.pane_id == pane_id)
                            .map(|pane| {
                                (
                                    pane.target.terminal_id,
                                    PaneLayout {
                                        rail: None,
                                        content,
                                    },
                                )
                            })
                    })
                    .collect();
                (panes, authored.dividers)
            }
        }
    }

    fn resize_split(&mut self, tab_id: crate::domain::TabId, split_id: SplitId, ratio: SplitRatio) {
        if self.focused().tab_id == tab_id && self.layout.resize(split_id, ratio) {
            self.invalidate_drawn();
        }
    }

    fn pane_at(
        &self,
        area: Rect,
        policy: PaneLayoutPolicy,
        column: u16,
        row: u16,
    ) -> Option<(SelectedTarget, u16, u16)> {
        let layouts = self.pane_layouts(area, policy).0;
        self.panes.iter().find_map(|pane| {
            let content = layouts.get(&pane.target.terminal_id)?.content;
            (column >= content.x
                && column < content.x.saturating_add(content.width)
                && row >= content.y
                && row < content.y.saturating_add(content.height))
            .then(|| (pane.target.clone(), column - content.x, row - content.y))
        })
    }

    fn terminal_cell(
        &self,
        area: Rect,
        policy: PaneLayoutPolicy,
        terminal_id: TerminalId,
        column: u16,
        row: u16,
        clamp: bool,
    ) -> Option<(u16, u16)> {
        let content = self.pane_layouts(area, policy).0.get(&terminal_id)?.content;
        if content.width == 0 || content.height == 0 {
            return None;
        }
        let inside = column >= content.x
            && column < content.x.saturating_add(content.width)
            && row >= content.y
            && row < content.y.saturating_add(content.height);
        if !inside && !clamp {
            return None;
        }
        Some((
            column.saturating_sub(content.x).min(content.width - 1),
            row.saturating_sub(content.y).min(content.height - 1),
        ))
    }

    fn copy_cell(
        &self,
        area: Rect,
        policy: PaneLayoutPolicy,
        terminal_id: TerminalId,
        column: u16,
        row: u16,
        clamp: bool,
    ) -> Option<(u16, u16)> {
        let pane = self
            .panes
            .iter()
            .find(|pane| pane.target.terminal_id == terminal_id)?;
        let screen = pane.pending.as_ref()?;
        let (column, row) = self.terminal_cell(area, policy, terminal_id, column, row, clamp)?;
        if !clamp && (column >= screen.size.columns || row >= screen.size.rows) {
            return None;
        }
        Some((
            column.min(screen.size.columns.saturating_sub(1)),
            row.min(screen.size.rows.saturating_sub(1)),
        ))
    }

    fn mouse_tracking(&self, terminal_id: TerminalId) -> bool {
        self.panes
            .iter()
            .find(|pane| pane.target.terminal_id == terminal_id)
            .and_then(|pane| pane.pending.as_ref())
            .is_some_and(|screen| screen.mouse_tracking)
    }

    fn resize_requests(
        &mut self,
        area: Rect,
        policy: PaneLayoutPolicy,
    ) -> Vec<(TerminalId, TerminalSize)> {
        let layouts = self.pane_layouts(area, policy).0;
        let Some(pane) = self
            .panes
            .iter_mut()
            .find(|pane| pane.target.terminal_id == self.focused)
        else {
            return Vec::new();
        };
        let Some(layout) = layouts.get(&pane.target.terminal_id) else {
            return Vec::new();
        };
        let size = TerminalSize {
            columns: layout.content.width,
            rows: layout.content.height,
        };
        if pane.last_size == Some(size) {
            Vec::new()
        } else {
            pane.last_size = Some(size);
            pane.constrained_size = false;
            vec![(pane.target.terminal_id, size)]
        }
    }
}

async fn resize_view(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    area: Rect,
    view: &mut ViewState,
    resources: &ResourceState,
    ui: &UiConfig,
) -> anyhow::Result<()> {
    let terminal =
        client_layout(area, ui, resources.sidebar_relevance(view.focused(), ui)).terminal;
    for (terminal_id, size) in view.resize_requests(terminal, ui.pane_layout) {
        let request_id = Uuid::new_v4();
        view.mark_resize_requested(terminal_id, request_id);
        send_request(
            framed,
            Some(request_id),
            ClientMessage::Resize { terminal_id, size },
        )
        .await?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderedCursor {
    column: u16,
    row: u16,
    shape: CursorShape,
    blinking: bool,
}

impl RenderedCursor {
    fn host_style(self) -> SetCursorStyle {
        match (self.shape, self.blinking) {
            (CursorShape::Block, true) => SetCursorStyle::BlinkingBlock,
            (CursorShape::Block, false) => SetCursorStyle::SteadyBlock,
            (CursorShape::Underline, true) => SetCursorStyle::BlinkingUnderScore,
            (CursorShape::Underline, false) => SetCursorStyle::SteadyUnderScore,
            (CursorShape::Bar, true) => SetCursorStyle::BlinkingBar,
            (CursorShape::Bar, false) => SetCursorStyle::SteadyBar,
        }
    }
}

#[derive(Default)]
struct HostCursorState {
    applied: Option<SetCursorStyle>,
}

impl HostCursorState {
    fn apply(
        &mut self,
        writer: &mut impl io::Write,
        cursor: Option<RenderedCursor>,
    ) -> io::Result<()> {
        let style = cursor.map_or(SetCursorStyle::DefaultUserShape, RenderedCursor::host_style);
        if self.applied == Some(style) {
            return Ok(());
        }
        execute!(writer, style)?;
        self.applied = Some(style);
        Ok(())
    }
}

fn temporary_command_content(area: Rect) -> Rect {
    if area.width < 4 || area.height < 3 {
        area
    } else {
        Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
    }
}

fn render_temporary_command_frame(
    area: Rect,
    title: &str,
    styles: &config::StylesConfig,
    buffer: &mut Buffer,
) -> Rect {
    let content = temporary_command_content(area);
    if content == area {
        return content;
    }
    let muted = styles.apply(
        config::SemanticStyle::Muted,
        styles.apply(config::SemanticStyle::Normal, Style::default()),
    );
    let title_style = muted.add_modifier(Modifier::BOLD);
    for column in area.x..area.right() {
        if let Some(top) = buffer.cell_mut((column, area.y)) {
            top.set_symbol("┄").set_style(muted);
        }
        if let Some(bottom) = buffer.cell_mut((column, area.bottom() - 1)) {
            bottom.set_symbol("┄").set_style(muted);
        }
    }
    for row in area.y..area.bottom() {
        if let Some(left) = buffer.cell_mut((area.x, row)) {
            left.set_symbol("┆").set_style(muted);
        }
        if let Some(right) = buffer.cell_mut((area.right() - 1, row)) {
            right.set_symbol("┆").set_style(muted);
        }
    }
    for (column, row) in [
        (area.x, area.y),
        (area.right() - 1, area.y),
        (area.x, area.bottom() - 1),
        (area.right() - 1, area.bottom() - 1),
    ] {
        if let Some(cell) = buffer.cell_mut((column, row)) {
            cell.set_symbol("·").set_style(muted);
        }
    }
    buffer.set_stringn(
        area.x + 2,
        area.y,
        format!(" {} ", sanitize(title)),
        usize::from(area.width.saturating_sub(4)),
        title_style,
    );
    let footer = " temporary · returns when command exits ";
    let footer_width = u16::try_from(footer.width()).unwrap_or(u16::MAX);
    if area.width > footer_width.saturating_add(4) {
        buffer.set_stringn(
            area.right() - footer_width - 2,
            area.bottom() - 1,
            footer,
            usize::from(footer_width),
            muted,
        );
    }
    content
}

fn render_view(
    view: &ViewState,
    area: Rect,
    policy: PaneLayoutPolicy,
    styles: &config::StylesConfig,
    buffer: &mut Buffer,
) -> Option<RenderedCursor> {
    let (layouts, dividers) = view.pane_layouts(area, policy);
    let divider_style = styles.apply(
        config::SemanticStyle::Divider,
        styles.apply(config::SemanticStyle::Normal, Style::default()),
    );
    for divider in dividers {
        let divider = divider.area;
        let symbol = if divider.width == 1 { "│" } else { "─" };
        for row in divider.y..divider.y + divider.height {
            for column in divider.x..divider.x + divider.width {
                if let Some(cell) = buffer.cell_mut((column, row)) {
                    cell.set_symbol(symbol).set_style(divider_style);
                }
            }
        }
    }
    let mut cursor = None;
    for pane in &view.panes {
        let Some(PaneLayout { rail, content }) = layouts.get(&pane.target.terminal_id) else {
            continue;
        };
        if let Some(rail) = rail {
            let focused = pane.target.terminal_id == view.focused;
            let style = if focused {
                divider_style.add_modifier(Modifier::BOLD)
            } else {
                divider_style
            };
            let symbol = if focused { "┃" } else { "│" };
            for row in rail.y..rail.y + rail.height {
                if let Some(cell) = buffer.cell_mut((rail.x, row)) {
                    cell.set_symbol(symbol).set_style(style);
                }
            }
        }
        let Some(screen) = pane.pending.as_ref() else {
            continue;
        };
        if pane.constrained_size {
            render_shared_size_gutter(
                *content,
                screen.size,
                styles.apply(
                    config::SemanticStyle::Muted,
                    styles.apply(config::SemanticStyle::Normal, Style::default()),
                ),
                styles.apply(
                    config::SemanticStyle::Divider,
                    styles.apply(config::SemanticStyle::Normal, Style::default()),
                ),
                buffer,
            );
        }
        Screen(screen).render(*content, buffer);
        render_scrollbar(
            screen.scroll,
            *content,
            pane.target.terminal_id == view.focused,
            divider_style,
            buffer,
        );
        if pane.target.terminal_id == view.focused
            && screen.cursor.visible
            && screen.cursor.column < content.width
            && screen.cursor.row < content.height
        {
            cursor = Some(RenderedCursor {
                column: content.x + screen.cursor.column,
                row: content.y + screen.cursor.row,
                shape: screen.cursor.shape,
                blinking: screen.cursor.blinking,
            });
        }
    }
    cursor
}

/// A larger attached client cannot grow a shared PTY beyond the smallest
/// client. Make that intentional constraint visible instead of presenting an
/// unexplained blank rectangle.
fn render_shared_size_gutter(
    area: Rect,
    screen: TerminalSize,
    star_style: Style,
    border_style: Style,
    buffer: &mut Buffer,
) {
    if screen.columns >= area.width && screen.rows >= area.height {
        return;
    }
    let right_border = (screen.columns < area.width).then_some(screen.columns);
    let bottom_border = (screen.rows < area.height).then_some(screen.rows);
    for row in 0..area.height {
        for column in 0..area.width {
            let border_cell = (right_border == Some(column) && row < screen.rows)
                || (bottom_border == Some(row) && column < screen.columns)
                || (right_border == Some(column) && bottom_border == Some(row));
            if (column < screen.columns && row < screen.rows) || border_cell {
                continue;
            }
            let hash = u32::from(column).wrapping_mul(73_856_093)
                ^ u32::from(row).wrapping_mul(19_349_663);
            let symbol = match hash % 67 {
                0 => "✦",
                1 => "⋆",
                2 => "·",
                _ => " ",
            };
            if let Some(cell) = buffer.cell_mut((area.x + column, area.y + row)) {
                cell.set_symbol(symbol).set_style(star_style);
            }
        }
    }
    if let Some(column) = right_border {
        for row in 0..screen.rows.min(area.height) {
            if let Some(cell) = buffer.cell_mut((area.x + column, area.y + row)) {
                cell.set_symbol("│").set_style(border_style);
            }
        }
    }
    if let Some(row) = bottom_border {
        for column in 0..screen.columns.min(area.width) {
            if let Some(cell) = buffer.cell_mut((area.x + column, area.y + row)) {
                cell.set_symbol("─").set_style(border_style);
            }
        }
    }
    if let (Some(column), Some(row)) = (right_border, bottom_border)
        && let Some(cell) = buffer.cell_mut((area.x + column, area.y + row))
    {
        cell.set_symbol("┘").set_style(border_style);
    }
}

fn one_line_error(error: &anyhow::Error) -> String {
    let collapsed = format!("{error:#}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    sanitize(&collapsed)
}

/// Overlay scrollbar on a pane's right edge, shown only while the viewport
/// is scrolled into history so the terminal never reflows for it.
fn render_scrollbar(
    scroll: ScrollPosition,
    area: Rect,
    focused: bool,
    track_style: Style,
    buffer: &mut Buffer,
) {
    if scroll.offset_from_bottom == 0 || area.width == 0 {
        return;
    }
    let scrolled_from_top = scroll
        .max_offset_from_bottom
        .saturating_sub(scroll.offset_from_bottom);
    let Some((top, len)) = dialog::scrollbar_thumb(
        scrolled_from_top,
        scroll.max_offset_from_bottom,
        area.height,
    ) else {
        return;
    };
    let column = area.x + area.width - 1;
    for row in 0..area.height {
        if let Some(cell) = buffer.cell_mut((column, area.y + row)) {
            cell.set_symbol("▕").set_style(track_style);
        }
    }
    let symbol = if focused { "▐" } else { "▕" };
    let style = track_style.add_modifier(Modifier::BOLD);
    for row in top..top.saturating_add(len).min(area.height) {
        if let Some(cell) = buffer.cell_mut((column, area.y + row)) {
            cell.set_symbol(symbol).set_style(style);
        }
    }
}

struct Screen<'a>(&'a ScreenSnapshot);

/// Benchmark-only access to the snapshot-to-ratatui-buffer path.
#[doc(hidden)]
pub mod bench {
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

    use crate::domain::ScreenSnapshot;

    pub fn render_snapshot(screen: &ScreenSnapshot) -> Buffer {
        let area = Rect::new(0, 0, screen.size.columns, screen.size.rows);
        let mut buffer = Buffer::empty(area);
        super::Screen(screen).render(area, &mut buffer);
        buffer
    }
}

impl Widget for Screen<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let screen = self.0;
        let columns = screen.size.columns.min(area.width);
        // Adjacent cells frequently share the same style (runs of plain
        // text, whole-line backgrounds, ...), so remember the last resolved
        // `Style` and reuse it instead of rebuilding one per cell.
        let mut resolved: Option<(CellStyle, bool, Style)> = None;
        for row in 0..screen.size.rows.min(area.height) {
            let mut column = 0;
            while column < columns {
                let index =
                    usize::from(row) * usize::from(screen.size.columns) + usize::from(column);
                let Some(cell) = screen.cells.get(index) else {
                    break;
                };
                let cell_style = match resolved {
                    Some((style, selected, cell_style))
                        if style == cell.style && selected == cell.selected =>
                    {
                        cell_style
                    }
                    _ => {
                        let cell_style = style(cell.style, cell.selected);
                        resolved = Some((cell.style, cell.selected, cell_style));
                        cell_style
                    }
                };

                if let Some(uri) = hyperlink_uri(screen, cell) {
                    let width = u16::try_from(UnicodeWidthStr::width(cell.contents.as_str()))
                        .unwrap_or(1)
                        .max(1)
                        .min(columns - column);
                    if let Some(target) = buffer.cell_mut((area.x + column, area.y + row)) {
                        // Keep every linked grapheme independently composable. A single
                        // ForcedWidth run for a whole filename makes Ratatui skip every
                        // overlaid cell in that run, so dialogs cannot cover part of it.
                        let symbol = format!("\x1b]8;;{uri}\x1b\\{}\x1b]8;;\x1b\\", cell.contents);
                        target
                            .set_symbol(&symbol)
                            .set_diff_option(CellDiffOption::ForcedWidth(
                                NonZeroU16::new(width).expect("hyperlink cell is non-empty"),
                            ))
                            .set_style(cell_style);
                    }
                    for trailing in column + 1..column + width {
                        if let Some(target) = buffer.cell_mut((area.x + trailing, area.y + row)) {
                            target.set_diff_option(CellDiffOption::Skip);
                        }
                    }
                    column += width;
                } else {
                    if let Some(target) = buffer.cell_mut((area.x + column, area.y + row)) {
                        target
                            .set_symbol(&cell.contents)
                            .set_diff_option(CellDiffOption::None)
                            .set_style(cell_style);
                    }
                    column += 1;
                }
            }
        }
    }
}

fn hyperlink_uri<'a>(screen: &'a ScreenSnapshot, cell: &crate::domain::Cell) -> Option<&'a str> {
    cell.hyperlink
        .and_then(|id| screen.hyperlinks.get(usize::from(id)))
        .filter(|uri| {
            uri.len() <= crate::domain::MAX_HYPERLINK_URI_BYTES
                && !uri.chars().any(char::is_control)
        })
        .map(|uri| uri.as_str())
}

fn style(source: CellStyle, selected: bool) -> Style {
    let mut target = Style::default();
    if let Some(color) = source.foreground() {
        target = target.fg(color.into());
    }
    if let Some(color) = source.background() {
        target = target.bg(color.into());
    }
    for (enabled, modifier) in [
        (source.bold(), Modifier::BOLD),
        (source.italic(), Modifier::ITALIC),
        (source.underline(), Modifier::UNDERLINED),
        (source.inverse() ^ selected, Modifier::REVERSED),
    ] {
        if enabled {
            target = target.add_modifier(modifier);
        }
    }
    target
}

impl From<CellColor> for Color {
    fn from(color: CellColor) -> Self {
        match color {
            CellColor::Indexed(index) => Self::Indexed(index),
            CellColor::Rgb(color) => Self::Rgb(color.red, color.green, color.blue),
        }
    }
}

/// Termination signals the interactive client intercepts so the host
/// terminal is restored before exit. Without this, `kill` leaves the host in
/// raw mode with mouse tracking on, spewing SGR mouse reports as text.
struct TerminationSignals {
    terminate: Signal,
    hangup: Signal,
    quit: Signal,
}

impl TerminationSignals {
    fn subscribe() -> io::Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
            quit: signal(SignalKind::quit())?,
        })
    }

    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.hangup.recv() => "SIGHUP",
            _ = self.quit.recv() => "SIGQUIT",
        }
    }
}

struct TerminalGuard {
    raw: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
    mouse_capture: bool,
    cursor_hidden: bool,
    line_wrap_disabled: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut guard = Self {
            raw: false,
            alternate_screen: false,
            bracketed_paste: false,
            mouse_capture: false,
            cursor_hidden: false,
            line_wrap_disabled: false,
        };
        enable_raw_mode()?;
        guard.raw = true;
        execute!(io::stdout(), EnterAlternateScreen)?;
        guard.alternate_screen = true;
        execute!(io::stdout(), EnableBracketedPaste)?;
        guard.bracketed_paste = true;
        // Mark capture before the multi-sequence command so a partial write or
        // flush failure still runs the disabling cleanup in Drop.
        guard.mouse_capture = true;
        execute!(io::stdout(), EnableMouseCapture)?;
        execute!(io::stdout(), Hide)?;
        guard.cursor_hidden = true;
        execute!(io::stdout(), DisableLineWrap)?;
        guard.line_wrap_disabled = true;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.line_wrap_disabled {
            let _ = execute!(stdout, EnableLineWrap);
        }
        if self.cursor_hidden {
            let _ = restore_host_cursor(&mut stdout);
        }
        if self.mouse_capture {
            let _ = execute!(stdout, DisableMouseCapture);
        }
        if self.bracketed_paste {
            let _ = execute!(stdout, DisableBracketedPaste);
        }
        if self.alternate_screen {
            let _ = execute!(stdout, LeaveAlternateScreen);
        }
        if self.raw {
            let _ = disable_raw_mode();
        }
    }
}

fn restore_host_cursor(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(writer, SetCursorStyle::DefaultUserShape, Show)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Cell, Cursor, PaneId, Rgb, SessionId, TabId, WorkspaceId};

    #[test]
    fn full_reload_retains_its_invocation_session_through_global_commit() {
        let location = config::ConfigLocation {
            path: None,
            explicit: false,
            source: "test defaults",
        };
        let catalog = crate::extensions::ExtensionRegistry::new(
            1,
            Vec::new(),
            config::ExtensionConfigCatalog::default(),
        )
        .unwrap()
        .catalog()
        .unwrap();
        let invocation_session = SessionId::new();
        let reload = ExtensionReloadState {
            session_id: invocation_session,
            phase: ExtensionReloadPhase::Preparing {
                request_id: Uuid::new_v4(),
                staged: stage_ui_config(&location).unwrap(),
            },
        };
        let candidate = stage_ui_config(&location)
            .unwrap()
            .materialize(&catalog)
            .unwrap();

        let ExtensionReloadState { session_id, .. } = reload;
        let reload = ExtensionReloadState {
            session_id,
            phase: ExtensionReloadPhase::Committing {
                request_id: Uuid::new_v4(),
                base_generation: 0,
                ui: candidate,
                catalog,
            },
        };

        assert_eq!(reload.session_id, invocation_session);
    }

    #[tokio::test]
    async fn background_project_preparation_keeps_unrelated_updates_responsive() {
        let (preparation_results, _preparation_result) = mpsc::unbounded_channel();
        let request_id = Uuid::new_v4();
        let _task = ProjectPreparationTask::spawn_with(
            request_id,
            std::future::pending(),
            preparation_results,
        );
        let (updates, mut update) = mpsc::unbounded_channel();

        updates.send("resources changed").unwrap();

        assert_eq!(
            time::timeout(Duration::from_millis(50), update.recv())
                .await
                .unwrap(),
            Some("resources changed")
        );
    }

    #[tokio::test]
    async fn dropping_project_preparation_cancels_its_background_work() {
        struct CancellationSignal(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for CancellationSignal {
            fn drop(&mut self) {
                if let Some(signal) = self.0.take() {
                    let _ = signal.send(());
                }
            }
        }

        let (cancelled, cancellation) = tokio::sync::oneshot::channel();
        let (results, _result) = mpsc::unbounded_channel();
        let task = ProjectPreparationTask::spawn_with(
            Uuid::new_v4(),
            async move {
                let _signal = CancellationSignal(Some(cancelled));
                std::future::pending().await
            },
            results,
        );
        tokio::task::yield_now().await;

        drop(task);

        time::timeout(Duration::from_millis(50), cancellation)
            .await
            .expect("preparation task should be aborted")
            .expect("preparation cancellation signal should be delivered");
    }

    fn targets(count: usize) -> Vec<SelectedTarget> {
        let session_id = SessionId::new();
        let workspace_id = WorkspaceId::new();
        let tab_id = TabId::new();
        (0..count)
            .map(|index| SelectedTarget {
                session_id,
                workspace_id,
                tab_id,
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
                child_pid: u32::try_from(index + 1).unwrap(),
            })
            .collect()
    }

    fn selected_view(
        resource_revision: u64,
        focused: SelectedTarget,
        panes: Vec<SelectedTarget>,
    ) -> SelectedView {
        let mut layout = SplitTree::leaf(panes[0].pane_id);
        for pair in panes.windows(2) {
            assert!(layout.split(
                pair[0].pane_id,
                crate::splits::SplitDirection::Right,
                pair[1].pane_id,
            ));
        }
        SelectedView {
            resource_revision,
            focused,
            panes,
            layout,
        }
    }

    #[test]
    fn create_coordinator_blocks_overlapping_operations_through_ack_and_selection() {
        let mut state = CreateCoordinator::default();
        let close = CloseTargetState::default();
        let focus = FocusState::default();
        let no_exit = None;
        let request = state.begin(CreateKind::Tab).expect("first request starts");
        assert!(state.begin(CreateKind::Workspace).is_none());
        assert!(state.blocks_input());
        assert!(!accepts_client_input(&focus, &state, &close, &no_exit));
        let target = targets(1).remove(0);
        assert!(!state.created(None, CreateKind::Tab, target.terminal_id));
        assert!(!state.created(Some(Uuid::new_v4()), CreateKind::Tab, target.terminal_id));
        assert!(!state.created(Some(request), CreateKind::Workspace, target.terminal_id));
        assert!(state.created(Some(request), CreateKind::Tab, target.terminal_id));
        assert!(state.blocks_input());
        assert!(!accepts_client_input(&focus, &state, &close, &no_exit));
        let selected = selected_view(2, target.clone(), vec![target]);
        assert_eq!(
            state.selected(Some(Uuid::new_v4()), &selected, Some(1)),
            None
        );
        assert!(state.begin(CreateKind::SplitPane).is_none());
        assert_eq!(
            state.selected(Some(request), &selected, Some(1)),
            Some(CreateKind::Tab)
        );
        assert!(state.blocks_input());
        assert!(!state.accept_resources(1));
        assert!(state.accept_resources(2));
        assert!(!state.blocks_input());
        assert!(accepts_client_input(&focus, &state, &close, &no_exit));
        assert!(state.begin(CreateKind::Workspace).is_some());

        let mut failed = CreateCoordinator::default();
        let request = failed.begin(CreateKind::SplitPane).unwrap();
        assert_eq!(failed.fail(Some(Uuid::new_v4())), None);
        assert_eq!(failed.fail(Some(request)), Some(CreateKind::SplitPane));
        assert!(!failed.blocks_input());
        assert!(accepts_client_input(&focus, &failed, &close, &no_exit));
    }

    #[test]
    fn close_target_state_waits_for_confirmation_and_correlated_completion() {
        let pane = TargetSelector::Pane(crate::domain::PaneId::new());
        let mut state = CloseTargetState::default();
        assert!(matches!(
            state.begin(pane.clone(), true),
            CloseTargetStart::Confirming
        ));
        assert!(state.is_confirming());
        assert!(!state.blocks_input());
        state.cancel();
        assert!(!state.is_confirming());

        assert!(matches!(
            state.begin(pane.clone(), true),
            CloseTargetStart::Confirming
        ));
        let (request, selector) = state.confirm().unwrap();
        assert_eq!(selector, pane);
        assert!(state.blocks_input());
        assert!(!state.complete(Some(Uuid::new_v4())));
        assert!(state.complete(Some(request)));
        assert!(!state.blocks_input());

        let CloseTargetStart::Closing {
            request_id,
            selector,
        } = state.begin(pane.clone(), false)
        else {
            panic!("unconfirmed close did not start immediately")
        };
        assert_eq!(selector, pane);
        assert!(state.complete(Some(request_id)));
    }

    #[test]
    fn close_confirmation_exclusively_owns_paste_and_pointer_input() {
        let pane = TargetSelector::Pane(crate::domain::PaneId::new());
        let mut state = CloseTargetState::default();
        assert!(matches!(
            state.begin(pane, true),
            CloseTargetStart::Confirming
        ));

        assert_eq!(
            state.route_confirmation_input(&Event::Paste("echo leaked".into())),
            CloseConfirmationInput::Consume
        );
        assert_eq!(
            state.route_confirmation_input(&Event::Mouse(HostMouseEvent {
                kind: HostMouseEventKind::Down(HostMouseButton::Left),
                column: 12,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })),
            CloseConfirmationInput::Consume
        );
        assert_eq!(
            state.route_confirmation_input(&Event::Resize(120, 40)),
            CloseConfirmationInput::Pass
        );
        assert!(state.is_confirming());
    }

    #[test]
    fn successful_terminal_exit_is_not_a_client_failure() {
        assert_eq!(failed_exit_code(Some(Some(0))), None);
        assert_eq!(failed_exit_code(Some(Some(7))), Some(7));
        assert_eq!(failed_exit_code(Some(None)), None);
        assert_eq!(failed_exit_code(None), None);
    }

    #[test]
    fn focus_state_correlates_one_request_and_preserves_its_origin() {
        let mut state = FocusState::default();
        let request = state
            .begin(FocusOrigin::Workspace)
            .expect("first focus starts");
        assert!(state.begin(FocusOrigin::Pane).is_none());
        assert_eq!(state.complete(None), None);
        assert_eq!(state.complete(Some(Uuid::new_v4())), None);
        assert_eq!(state.complete(Some(request)), Some(FocusOrigin::Workspace));
        let pane = state.begin(FocusOrigin::Pane).expect("focus gate released");
        assert_eq!(state.complete(Some(pane)), Some(FocusOrigin::Pane));
        for scope in [
            NavigationScope::Tab,
            NavigationScope::Workspace,
            NavigationScope::Session,
            NavigationScope::Global,
        ] {
            let request = state
                .begin(FocusOrigin::Sidebar {
                    scope,
                    component: SidebarComponentKind::Agents,
                })
                .unwrap();
            assert_eq!(
                state.complete(Some(request)),
                Some(FocusOrigin::Sidebar {
                    scope,
                    component: SidebarComponentKind::Agents,
                })
            );
        }
    }

    #[test]
    fn pane_snapshots_are_independent_retained_and_reject_stale_revisions() {
        let snapshot = |revision| {
            ScreenSnapshot::new(
                revision,
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
        };
        let panes = targets(2);
        let first = panes[0].terminal_id;
        let second = panes[1].terminal_id;
        let mut state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        assert!(state.accept(first, snapshot(2)));
        assert!(state.accept(second, snapshot(1)));
        assert!(!state.accept(first, snapshot(1)));
        assert!(!state.accept(first, snapshot(2)));
        assert!(state.needs_draw());
        state.mark_drawn();
        assert!(!state.needs_draw());
        assert!(state.accept(first, snapshot(3)));
        state.mark_drawn();

        state
            .replace(selected_view(1, panes[1].clone(), panes))
            .unwrap();
        assert_eq!(state.focused().terminal_id, second);
        assert_eq!(
            state.panes[0]
                .pending
                .as_ref()
                .map(|screen| screen.revision),
            Some(3)
        );
        assert_eq!(
            state.panes[1]
                .pending
                .as_ref()
                .map(|screen| screen.revision),
            Some(1)
        );
        state.invalidate_drawn();
        assert!(state.needs_draw());
        assert!(!state.accept(TerminalId::new(), snapshot(100)));
    }

    #[test]
    fn shared_size_gutter_waits_for_the_correlated_resize_response() {
        let snapshot = |revision, size: TerminalSize| {
            ScreenSnapshot::new(
                revision,
                size,
                vec![crate::domain::Cell::default(); size.cell_count().unwrap()],
                crate::domain::Cursor {
                    column: 0,
                    row: 0,
                    visible: true,
                    shape: Default::default(),
                    blinking: false,
                },
            )
            .unwrap()
        };
        let requested = TerminalSize {
            columns: 100,
            rows: 30,
        };
        let shared = TerminalSize {
            columns: 80,
            rows: 24,
        };
        let mut pane = PaneState::new(targets(1).remove(0));
        pane.last_size = Some(requested);
        let request_id = Uuid::new_v4();
        pane.resize_request_id = Some(request_id);
        assert!(pane.accept(snapshot(1, shared)));
        assert!(!pane.constrained_size, "stale pre-resize frame stays plain");
        assert!(!pane.complete_resize(Uuid::new_v4(), shared));
        assert!(pane.complete_resize(request_id, shared));
        assert!(pane.constrained_size, "daemon confirmed the shared minimum");
        assert!(pane.accept(snapshot(3, requested)));
        assert!(
            !pane.constrained_size,
            "the gutter clears when the PTY expands"
        );
    }

    #[test]
    fn cursor_only_delta_updates_the_interactive_cursor_style() {
        let target = targets(1).remove(0);
        let terminal_id = target.terminal_id;
        let mut state = ViewState::new(selected_view(1, target.clone(), vec![target])).unwrap();
        let size = TerminalSize {
            columns: 1,
            rows: 1,
        };
        assert!(
            state.accept(
                terminal_id,
                ScreenSnapshot::new(
                    1,
                    size,
                    vec![Cell::default()],
                    Cursor {
                        column: 0,
                        row: 0,
                        visible: true,
                        shape: CursorShape::Block,
                        blinking: false,
                    },
                )
                .unwrap(),
            )
        );

        assert!(matches!(
            state.accept_delta(
                terminal_id,
                ScreenDelta {
                    revision: 2,
                    base_revision: 1,
                    size,
                    rows: Vec::new(),
                    hyperlinks: Vec::new(),
                    cursor: Cursor {
                        column: 0,
                        row: 0,
                        visible: true,
                        shape: CursorShape::Underline,
                        blinking: true,
                    },
                    scroll: ScrollPosition::default(),
                    mouse_tracking: true,
                    graphics: None,
                },
            ),
            DeltaApplyResult::Applied
        ));
        let cursor = state.panes[0].pending.as_ref().unwrap().cursor;
        assert_eq!(cursor.shape, CursorShape::Underline);
        assert!(cursor.blinking);
        assert!(state.panes[0].pending.as_ref().unwrap().mouse_tracking);
    }

    #[test]
    fn selected_view_revisions_only_reject_older_selected_views() {
        let panes = targets(3);
        let mut state =
            ViewState::new(selected_view(2, panes[0].clone(), panes[..2].to_vec())).unwrap();

        assert!(
            state
                .replace(selected_view(3, panes[1].clone(), panes[1..].to_vec()))
                .unwrap()
        );
        assert_eq!(state.focused().terminal_id, panes[1].terminal_id);
        assert_eq!(
            state
                .panes
                .iter()
                .map(|pane| pane.target.terminal_id)
                .collect::<Vec<_>>(),
            [panes[1].terminal_id, panes[2].terminal_id]
        );

        assert!(
            !state
                .replace(selected_view(2, panes[0].clone(), panes[..2].to_vec()))
                .unwrap()
        );
        assert_eq!(state.focused().terminal_id, panes[1].terminal_id);
    }

    #[test]
    fn authoritative_selected_view_replaces_an_optimistic_split_ratio() {
        let panes = targets(2);
        let initial = selected_view(1, panes[0].clone(), panes.clone());
        let split_id = match &initial.layout {
            SplitTree::Branch { split_id, .. } => *split_id,
            _ => panic!("two-pane view did not contain a split"),
        };
        let mut accepted = initial.clone();
        accepted.resource_revision = 2;
        let mut state = ViewState::new(initial).unwrap();
        let optimistic = SplitRatio::from_cells(1, 3).unwrap();
        let authoritative = SplitRatio::from_cells(2, 3).unwrap();

        state.resize_split(panes[0].tab_id, split_id, optimistic);
        assert_eq!(state.layout.ratio(split_id), Some(optimistic));

        assert!(accepted.layout.resize(split_id, authoritative));
        assert!(state.replace(accepted).unwrap());
        assert_eq!(state.layout.ratio(split_id), Some(authoritative));
    }

    #[test]
    fn replaced_views_still_own_in_flight_resize_errors() {
        let mut panes = targets(2);
        panes[1].tab_id = TabId::new();
        let old_terminal = panes[0].terminal_id;
        let request_id = Uuid::new_v4();
        let mut state =
            ViewState::new(selected_view(1, panes[0].clone(), vec![panes[0].clone()])).unwrap();
        state.mark_resize_requested(old_terminal, request_id);

        state
            .replace(selected_view(2, panes[1].clone(), vec![panes[1].clone()]))
            .unwrap();

        assert!(state.reject_resize(Some(request_id)));
        assert!(!state.reject_resize(Some(request_id)));
        assert_eq!(state.focused().terminal_id, panes[1].terminal_id);
    }

    #[test]
    fn view_rejects_inconsistent_focus_and_duplicate_identity() {
        let panes = targets(2);
        let mut inconsistent = panes[0].clone();
        inconsistent.child_pid += 100;
        assert!(ViewState::new(selected_view(1, inconsistent, panes.clone())).is_err());

        let mut duplicate = panes[1].clone();
        duplicate.pane_id = panes[0].pane_id;
        assert!(
            ViewState::new(selected_view(
                1,
                panes[0].clone(),
                vec![panes[0].clone(), duplicate],
            ))
            .is_err()
        );
    }

    #[test]
    fn view_resize_targets_only_focus_and_tracks_each_pane_independently() {
        let panes = targets(2);
        let first = panes[0].terminal_id;
        let second = panes[1].terminal_id;
        let mut state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let area = Rect::new(0, 0, 38, 4);
        assert_eq!(
            state.resize_requests(area, PaneLayoutPolicy::Accordion),
            [(
                first,
                TerminalSize {
                    columns: 24,
                    rows: 4
                }
            )]
        );
        assert!(
            state
                .resize_requests(area, PaneLayoutPolicy::Accordion)
                .is_empty()
        );
        let request_id = Uuid::new_v4();
        state.mark_resize_requested(first, request_id);
        assert!(state.reject_resize(Some(request_id)));
        assert_eq!(
            state.resize_requests(area, PaneLayoutPolicy::Accordion),
            [(
                first,
                TerminalSize {
                    columns: 24,
                    rows: 4
                }
            )]
        );
        assert_eq!(
            state.resize_requests(Rect::new(0, 0, 37, 4), PaneLayoutPolicy::Accordion),
            [(
                first,
                TerminalSize {
                    columns: 37,
                    rows: 4
                }
            )]
        );

        state
            .replace(selected_view(1, panes[1].clone(), panes))
            .unwrap();
        assert_eq!(
            state.resize_requests(area, PaneLayoutPolicy::Accordion),
            [(
                second,
                TerminalSize {
                    columns: 24,
                    rows: 4
                }
            )]
        );
    }

    #[test]
    fn pane_hit_testing_returns_the_typed_target_and_terminal_local_cell() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let area = Rect::new(7, 3, 49, 8);
        let layouts = state.pane_layouts(area, PaneLayoutPolicy::Splits).0;

        for pane in &panes {
            let content = layouts[&pane.terminal_id].content;
            let column = content.x + content.width / 2;
            let row = content.y + content.height / 2;
            assert_eq!(
                state.pane_at(area, PaneLayoutPolicy::Splits, column, row),
                Some((pane.clone(), column - content.x, row - content.y,))
            );
        }
        assert!(
            state
                .pane_at(area, PaneLayoutPolicy::Splits, 0, 0)
                .is_none()
        );
    }

    #[test]
    fn mouse_selection_defers_to_application_tracking_unless_shift_overrides_it() {
        let panes = targets(1);
        let terminal_id = panes[0].terminal_id;
        let mut state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let area = Rect::new(7, 3, 20, 8);
        let mut screen = ScreenSnapshot::new(
            1,
            TerminalSize {
                columns: 20,
                rows: 8,
            },
            vec![Cell::default(); 160],
            Cursor {
                column: 0,
                row: 0,
                visible: true,
                shape: CursorShape::Block,
                blinking: false,
            },
        )
        .unwrap();
        assert!(state.accept(terminal_id, screen.clone()));

        let down = |modifiers| HostMouseEvent {
            kind: HostMouseEventKind::Down(HostMouseButton::Left),
            column: area.x + 4,
            row: area.y + 2,
            modifiers,
        };
        assert_eq!(
            mouse_selection_anchor(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                down(KeyModifiers::NONE),
            ),
            Some((terminal_id, (4, 2)))
        );

        screen.revision = 2;
        screen.mouse_tracking = true;
        assert!(state.accept(terminal_id, screen));
        assert_eq!(
            mouse_selection_anchor(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                down(KeyModifiers::NONE),
            ),
            None
        );
        assert_eq!(
            mouse_selection_anchor(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                down(KeyModifiers::SHIFT),
            ),
            Some((terminal_id, (4, 2)))
        );
    }

    #[test]
    fn crossterm_wheel_is_hit_tested_and_normalized_for_the_terminal_protocol() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let area = Rect::new(7, 3, 49, 8);
        let content =
            state.pane_layouts(area, PaneLayoutPolicy::Splits).0[&panes[1].terminal_id].content;
        let mut input = MouseInputState::default();
        let action = input.route(
            &state,
            area,
            PaneLayoutPolicy::Splits,
            HostMouseEvent {
                kind: HostMouseEventKind::ScrollDown,
                column: content.x + 4,
                row: content.y + 2,
                modifiers: KeyModifiers::SHIFT | KeyModifiers::ALT,
            },
        );
        assert_eq!(
            action,
            Some(PaneMouseAction::Input {
                terminal_id: panes[1].terminal_id,
                event: MouseEvent {
                    kind: MouseEventKind::Wheel {
                        direction: MouseWheelDirection::Down,
                    },
                    column: 4,
                    row: 2,
                    modifiers: MouseModifiers {
                        shift: true,
                        control: false,
                        alt: true,
                    },
                    buttons: Default::default(),
                },
            })
        );
    }

    #[test]
    fn unfocused_left_click_focuses_and_swallows_the_complete_initiating_gesture() {
        let panes = targets(2);
        let mut state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let area = Rect::new(0, 0, 49, 8);
        let content =
            state.pane_layouts(area, PaneLayoutPolicy::Splits).0[&panes[1].terminal_id].content;
        let mut input = MouseInputState::default();
        let action = input.route(
            &state,
            area,
            PaneLayoutPolicy::Splits,
            HostMouseEvent {
                kind: HostMouseEventKind::Down(HostMouseButton::Left),
                column: content.x,
                row: content.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(action, Some(PaneMouseAction::Focus(panes[1].pane_id)));

        state
            .replace(selected_view(1, panes[1].clone(), panes.clone()))
            .unwrap();
        assert_eq!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Up(HostMouseButton::Left),
                    column: content.x,
                    row: content.y,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            None
        );

        let action = input.route(
            &state,
            area,
            PaneLayoutPolicy::Splits,
            HostMouseEvent {
                kind: HostMouseEventKind::Down(HostMouseButton::Left),
                column: content.x + 2,
                row: content.y + 1,
                modifiers: KeyModifiers::CONTROL,
            },
        );
        assert!(matches!(
            action,
            Some(PaneMouseAction::Input {
                terminal_id,
                event: MouseEvent {
                    kind: MouseEventKind::Press { button: MouseButton::Left },
                    column: 2,
                    row: 1,
                    modifiers: MouseModifiers { control: true, .. },
                    buttons: MouseButtons { left: true, .. },
                },
            }) if terminal_id == panes[1].terminal_id
        ));
    }

    #[test]
    fn focused_drag_is_captured_clamped_and_modal_mouse_is_discarded() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let area = Rect::new(7, 3, 49, 8);
        let content =
            state.pane_layouts(area, PaneLayoutPolicy::Splits).0[&panes[0].terminal_id].content;
        let background =
            state.pane_layouts(area, PaneLayoutPolicy::Splits).0[&panes[1].terminal_id].content;
        let mut input = MouseInputState::default();

        assert_eq!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Moved,
                    column: background.x,
                    row: background.y,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            None
        );
        assert!(matches!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Moved,
                    column: content.x,
                    row: content.y,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            Some(PaneMouseAction::Input {
                event: MouseEvent {
                    kind: MouseEventKind::Motion { button: None },
                    ..
                },
                ..
            })
        ));
        assert!(matches!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Left),
                    column: content.x + 1,
                    row: content.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            Some(PaneMouseAction::Input { .. })
        ));
        let drag = input.route(
            &state,
            area,
            PaneLayoutPolicy::Splits,
            HostMouseEvent {
                kind: HostMouseEventKind::Drag(HostMouseButton::Left),
                column: u16::MAX,
                row: u16::MAX,
                modifiers: KeyModifiers::ALT,
            },
        );
        assert!(matches!(
            drag,
            Some(PaneMouseAction::Input {
                terminal_id,
                event: MouseEvent {
                    kind: MouseEventKind::Motion { button: Some(MouseButton::Left) },
                    column,
                    row,
                    buttons: MouseButtons { left: true, .. },
                    ..
                },
            }) if terminal_id == panes[0].terminal_id
                && column == content.width - 1
                && row == content.height - 1
        ));
        assert!(matches!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Up(HostMouseButton::Left),
                    column: u16::MAX,
                    row: u16::MAX,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            Some(PaneMouseAction::Input {
                event: MouseEvent {
                    kind: MouseEventKind::Release { .. },
                    buttons: MouseButtons { left: false, .. },
                    ..
                },
                ..
            })
        ));

        input.discard(HostMouseEvent {
            kind: HostMouseEventKind::Down(HostMouseButton::Right),
            column: content.x,
            row: content.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Drag(HostMouseButton::Right),
                    column: content.x + 1,
                    row: content.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            None
        );
        input.discard(HostMouseEvent {
            kind: HostMouseEventKind::Up(HostMouseButton::Right),
            column: content.x + 1,
            row: content.y + 1,
            modifiers: KeyModifiers::NONE,
        });
    }

    #[test]
    fn per_button_capture_derives_destination_buttons_and_orders_synthetic_releases() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let area = Rect::new(7, 3, 49, 8);
        let layouts = state.pane_layouts(area, PaneLayoutPolicy::Splits).0;
        let focused = layouts[&panes[0].terminal_id].content;
        let background = layouts[&panes[1].terminal_id].content;
        let mut input = MouseInputState::default();

        assert!(matches!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Left),
                    column: focused.x + 1,
                    row: focused.y + 1,
                    modifiers: KeyModifiers::SHIFT,
                },
            ),
            Some(PaneMouseAction::Input {
                event: MouseEvent {
                    buttons: MouseButtons {
                        left: true,
                        middle: false,
                        right: false,
                    },
                    ..
                },
                ..
            })
        ));
        assert_eq!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Middle),
                    column: background.x,
                    row: background.y,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            None
        );
        assert!(matches!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::ScrollUp,
                    column: focused.x + 2,
                    row: focused.y + 2,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            Some(PaneMouseAction::Input {
                event: MouseEvent {
                    buttons: MouseButtons {
                        left: true,
                        middle: false,
                        right: false,
                    },
                    ..
                },
                ..
            })
        ));
        assert!(matches!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Right),
                    column: focused.x + 3,
                    row: focused.y + 2,
                    modifiers: KeyModifiers::CONTROL,
                },
            ),
            Some(PaneMouseAction::Input {
                event: MouseEvent {
                    buttons: MouseButtons {
                        left: true,
                        middle: false,
                        right: true,
                    },
                    ..
                },
                ..
            })
        ));
        input.route(
            &state,
            area,
            PaneLayoutPolicy::Splits,
            HostMouseEvent {
                kind: HostMouseEventKind::Drag(HostMouseButton::Left),
                column: focused.x + 4,
                row: focused.y + 3,
                modifiers: KeyModifiers::ALT,
            },
        );

        assert_eq!(
            input.synthetic_releases(panes[0].terminal_id),
            vec![
                PaneMouseAction::Input {
                    terminal_id: panes[0].terminal_id,
                    event: MouseEvent {
                        kind: MouseEventKind::Release {
                            button: MouseButton::Left,
                        },
                        column: 4,
                        row: 3,
                        modifiers: MouseModifiers {
                            alt: true,
                            ..Default::default()
                        },
                        buttons: MouseButtons {
                            right: true,
                            ..Default::default()
                        },
                    },
                },
                PaneMouseAction::Input {
                    terminal_id: panes[0].terminal_id,
                    event: MouseEvent {
                        kind: MouseEventKind::Release {
                            button: MouseButton::Right,
                        },
                        column: 3,
                        row: 2,
                        modifiers: MouseModifiers {
                            control: true,
                            ..Default::default()
                        },
                        buttons: MouseButtons::default(),
                    },
                },
            ]
        );
    }

    #[test]
    fn real_release_updates_capture_before_forwarding_and_clears_only_after_send() {
        let panes = targets(1);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let area = Rect::new(7, 3, 20, 8);
        let content =
            state.pane_layouts(area, PaneLayoutPolicy::Splits).0[&panes[0].terminal_id].content;
        let mut input = MouseInputState::default();
        input.route(
            &state,
            area,
            PaneLayoutPolicy::Splits,
            HostMouseEvent {
                kind: HostMouseEventKind::Down(HostMouseButton::Left),
                column: content.x,
                row: content.y,
                modifiers: KeyModifiers::NONE,
            },
        );

        let Some(PaneMouseAction::Input { terminal_id, event }) = input.route(
            &state,
            area,
            PaneLayoutPolicy::Splits,
            HostMouseEvent {
                kind: HostMouseEventKind::Up(HostMouseButton::Left),
                column: content.x + 5,
                row: content.y + 2,
                modifiers: KeyModifiers::ALT,
            },
        ) else {
            panic!("captured release was not forwarded")
        };
        assert_eq!(
            event,
            MouseEvent {
                kind: MouseEventKind::Release {
                    button: MouseButton::Left,
                },
                column: 5,
                row: 2,
                modifiers: MouseModifiers {
                    alt: true,
                    ..Default::default()
                },
                buttons: MouseButtons::default(),
            }
        );
        assert!(matches!(
            input.button(MouseButton::Left),
            MouseButtonState::Captured {
                column: 5,
                row: 2,
                modifiers: MouseModifiers { alt: true, .. },
                ..
            }
        ));
        input.finish_release(terminal_id, event);
        assert_eq!(input.button(MouseButton::Left), MouseButtonState::Idle);
    }

    #[test]
    fn failed_focus_gesture_stays_suppressed_without_replay() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let area = Rect::new(0, 0, 49, 8);
        let background =
            state.pane_layouts(area, PaneLayoutPolicy::Splits).0[&panes[1].terminal_id].content;
        let mut input = MouseInputState::default();

        assert_eq!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Left),
                    column: background.x,
                    row: background.y,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            Some(PaneMouseAction::Focus(panes[1].pane_id))
        );
        input.clear();
        assert_eq!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Drag(HostMouseButton::Left),
                    column: background.x + 1,
                    row: background.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            None
        );
        assert_eq!(
            input.button(MouseButton::Left),
            MouseButtonState::Suppressed
        );
        assert_eq!(
            input.route(
                &state,
                area,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Up(HostMouseButton::Left),
                    column: background.x + 1,
                    row: background.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            None
        );
        assert_eq!(input.button(MouseButton::Left), MouseButtonState::Idle);
    }

    #[test]
    fn ui_drag_ownership_is_left_only_stable_and_release_uses_the_final_cell() {
        let panes = targets(3);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let host = Rect::new(0, 0, 100, 30);
        let dividers = state.pane_layouts(host, PaneLayoutPolicy::Splits).1;
        let divider = dividers[0];
        let mut input = MouseInputState::default();

        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Right),
                    column: divider.area.x,
                    row: divider.area.y,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                &[],
                panes[0].tab_id,
                &dividers,
            ),
            UiMouseRoute::NotOwned
        );
        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Left),
                    column: divider.area.x,
                    row: divider.area.y,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                &[],
                panes[0].tab_id,
                &dividers,
            ),
            UiMouseRoute::Owned(None)
        );
        assert!(input.synthetic_releases(panes[0].terminal_id).is_empty());

        let final_column = divider.branch_area.x + divider.first_max;
        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Up(HostMouseButton::Left),
                    column: final_column,
                    row: u16::MAX,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                &[],
                panes[0].tab_id,
                &dividers,
            ),
            UiMouseRoute::Owned(Some(UiResizeAction::Split {
                tab_id: panes[0].tab_id,
                split_id: divider.split_id,
                ratio: SplitRatio::from_cells(divider.first_max, divider.available).unwrap(),
            }))
        );
        assert!(input.ui_drag.is_none());
    }

    #[test]
    fn authoritative_topology_change_cancels_a_stale_split_drag() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let host = Rect::new(0, 0, 80, 23);
        let dividers = state.pane_layouts(host, PaneLayoutPolicy::Splits).1;
        let divider = dividers[0];
        let mut input = MouseInputState::default();

        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Left),
                    column: divider.area.x,
                    row: divider.area.y,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                &[],
                panes[0].tab_id,
                &dividers,
            ),
            UiMouseRoute::Owned(None)
        );
        assert!(!input.reconcile_ui_drag(panes[0].tab_id, &state.layout));

        let collapsed = SplitTree::leaf(panes[0].pane_id);
        assert!(input.reconcile_ui_drag(panes[0].tab_id, &collapsed));
        assert!(input.ui_drag.is_none());
        assert!(!input.reconcile_ui_drag(panes[0].tab_id, &collapsed));
    }

    #[test]
    fn ui_drag_waits_until_every_application_mouse_button_is_idle() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let host = Rect::new(0, 0, 80, 23);
        let (layouts, dividers) = state.pane_layouts(host, PaneLayoutPolicy::Splits);
        let content = layouts[&panes[0].terminal_id].content;
        let divider = dividers[0];
        let mut input = MouseInputState::default();

        assert!(matches!(
            input.route(
                &state,
                host,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Right),
                    column: content.x,
                    row: content.y,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            Some(PaneMouseAction::Input {
                event: MouseEvent {
                    kind: MouseEventKind::Press {
                        button: MouseButton::Right
                    },
                    ..
                },
                ..
            })
        ));
        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Left),
                    column: divider.area.x,
                    row: divider.area.y,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                &[],
                panes[0].tab_id,
                &dividers,
            ),
            UiMouseRoute::NotOwned
        );
        assert!(input.ui_drag.is_none());
        assert!(matches!(
            input.route(
                &state,
                host,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Up(HostMouseButton::Right),
                    column: content.x,
                    row: content.y,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            Some(PaneMouseAction::Input {
                event: MouseEvent {
                    kind: MouseEventKind::Release {
                        button: MouseButton::Right
                    },
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn sidebar_drags_are_side_specific_and_resize_from_their_own_edge() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let host = Rect::new(0, 0, 140, 24);
        let dividers = state.pane_layouts(host, PaneLayoutPolicy::Splits).1;
        let pane_divider = dividers[0];
        let sidebar = SidebarDivider {
            position: sidebar::SidebarSide::Left,
            area: pane_divider.area,
            current_width: 28,
            max_width: 44,
        };
        let mut input = MouseInputState::default();
        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Left),
                    column: sidebar.area.x,
                    row: sidebar.area.y,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                &[sidebar],
                panes[0].tab_id,
                &dividers,
            ),
            UiMouseRoute::Owned(None)
        );
        assert!(matches!(input.ui_drag, Some(UiDrag::Sidebar { .. })));
        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Drag(HostMouseButton::Left),
                    column: 60,
                    row: 23,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                &[],
                panes[0].tab_id,
                &[],
            ),
            UiMouseRoute::Owned(Some(UiResizeAction::Sidebar {
                side: SidebarSide::Left,
                width: 44,
            }))
        );
        input.clear();

        let right = sidebar_divider(Rect::new(112, 0, 28, 24), SidebarSide::Right, 44).unwrap();
        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Left),
                    column: right.area.x,
                    row: right.area.y,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                &[sidebar, right],
                panes[0].tab_id,
                &dividers,
            ),
            UiMouseRoute::Owned(None)
        );
        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Drag(HostMouseButton::Left),
                    column: 99,
                    row: 23,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                &[],
                panes[0].tab_id,
                &[],
            ),
            UiMouseRoute::Owned(Some(UiResizeAction::Sidebar {
                side: SidebarSide::Right,
                width: 41,
            }))
        );
        input.clear();

        assert!(input.synthetic_releases(panes[0].terminal_id).is_empty());

        assert_eq!(
            sidebar_divider(
                Rect::new(0, 0, MIN_SIDEBAR_WIDTH - 1, 24),
                sidebar::SidebarSide::Left,
                MAX_SIDEBAR_WIDTH,
            ),
            None,
            "a host-clipped drawer cannot replace the configured width on click"
        );
    }

    #[test]
    fn docked_sidebar_drag_maximum_reserves_the_other_side_and_terminal() {
        let host = Rect::new(5, 2, 120, 24);
        let mut ui = UiConfig::default();
        ui.sidebar.left.width = 30;
        ui.sidebar.right.width = 20;
        let layout = client_layout(host, &ui, chrome::SidebarRelevance::default());
        assert_eq!(layout.terminal.width, 70);
        assert_eq!(
            docked_sidebar_max_width(host, layout, SidebarSide::Left),
            60
        );
        assert_eq!(
            docked_sidebar_max_width(host, layout, SidebarSide::Right),
            50
        );
    }

    #[test]
    fn cheatsheet_obstructs_application_mouse_and_cursor_routing() {
        assert!(app_overlay_clear(true, true, true, false));
        assert!(!app_overlay_clear(true, true, true, true));
    }

    #[test]
    fn temporary_command_keeps_covered_terminal_attention_unread() {
        assert!(focused_terminal_is_rendered(false, None, false, false));
        assert!(!focused_terminal_is_rendered(true, None, false, false));
    }

    #[test]
    fn terminal_originated_drag_never_turns_into_a_divider_drag() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let host = Rect::new(0, 0, 80, 23);
        let (layouts, dividers) = state.pane_layouts(host, PaneLayoutPolicy::Splits);
        let content = layouts[&panes[0].terminal_id].content;
        let mut input = MouseInputState::default();
        assert!(matches!(
            input.route(
                &state,
                host,
                PaneLayoutPolicy::Splits,
                HostMouseEvent {
                    kind: HostMouseEventKind::Down(HostMouseButton::Left),
                    column: content.x,
                    row: content.y,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            Some(PaneMouseAction::Input { .. })
        ));
        let divider = dividers[0];
        let drag = HostMouseEvent {
            kind: HostMouseEventKind::Drag(HostMouseButton::Left),
            column: divider.area.x,
            row: divider.area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            input.route_ui(drag, host, &[], panes[0].tab_id, &dividers),
            UiMouseRoute::NotOwned
        );
        assert!(matches!(
            input.route(&state, host, PaneLayoutPolicy::Splits, drag),
            Some(PaneMouseAction::Input {
                event: MouseEvent {
                    kind: MouseEventKind::Motion {
                        button: Some(MouseButton::Left)
                    },
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn pane_zoom_is_explicit_tracks_focus_and_resets_across_tabs() {
        let panes = targets(2);
        let first = panes[0].terminal_id;
        let second = panes[1].terminal_id;
        let area = Rect::new(0, 0, 38, 4);
        let mut state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();

        assert!(!state.is_zoomed());
        assert_eq!(
            state.resize_requests(area, PaneLayoutPolicy::Accordion)[0]
                .1
                .columns,
            24
        );
        assert_eq!(state.toggle_zoom(), Some(true));
        assert!(state.is_zoomed());
        assert_eq!(
            state.resize_requests(area, PaneLayoutPolicy::Accordion),
            [(
                first,
                TerminalSize {
                    columns: 38,
                    rows: 4,
                },
            )]
        );

        state
            .replace(selected_view(1, panes[1].clone(), panes.clone()))
            .unwrap();
        assert!(state.is_zoomed());
        assert_eq!(
            state
                .panes
                .iter()
                .find(|pane| pane.target.terminal_id == second)
                .unwrap()
                .last_size,
            None
        );
        assert_eq!(
            state.resize_requests(area, PaneLayoutPolicy::Accordion)[0],
            (
                second,
                TerminalSize {
                    columns: 38,
                    rows: 4
                }
            )
        );

        let other_tab = targets(2);
        state
            .replace(selected_view(2, other_tab[0].clone(), other_tab))
            .unwrap();
        assert!(!state.is_zoomed());

        state
            .replace(selected_view(3, panes[0].clone(), vec![panes[0].clone()]))
            .unwrap();
        assert_eq!(state.toggle_zoom(), None);
    }

    #[test]
    fn simultaneous_render_uses_neutral_focus_rails_and_one_cursor() {
        let panes = targets(2);
        let first = panes[0].terminal_id;
        let second = panes[1].terminal_id;
        let snapshot = |contents: &str, columns| {
            ScreenSnapshot::new(
                1,
                TerminalSize { columns, rows: 2 },
                vec![
                    Cell {
                        contents: contents.into(),
                        style: CellStyle::default(),
                        selected: false,
                        hyperlink: None,
                    };
                    usize::from(columns) * 2
                ],
                Cursor {
                    column: 0,
                    row: 0,
                    visible: true,
                    shape: CursorShape::Bar,
                    blinking: true,
                },
            )
            .unwrap()
        };
        let mut state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        assert!(state.accept(first, snapshot("A", 13)));
        assert!(state.accept(second, snapshot("B", 12)));

        let area = Rect::new(0, 0, 38, 2);
        let mut buffer = Buffer::empty(area);
        assert_eq!(
            render_view(
                &state,
                area,
                PaneLayoutPolicy::Accordion,
                &UiConfig::default().styles,
                &mut buffer,
            ),
            Some(RenderedCursor {
                column: 1,
                row: 0,
                shape: CursorShape::Bar,
                blinking: true,
            })
        );
        assert_eq!(buffer[(0, 0)].symbol(), "┃");
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 0)].fg, Color::DarkGray);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert_eq!(buffer[(1, 0)].symbol(), "A");
        assert_eq!(buffer[(25, 0)].symbol(), "│");
        assert_eq!(buffer[(25, 0)].fg, Color::DarkGray);
        assert_eq!(buffer[(26, 0)].symbol(), "B");

        state
            .replace(selected_view(1, panes[1].clone(), panes))
            .unwrap();
        let mut moved = Buffer::empty(area);
        assert_eq!(
            render_view(
                &state,
                area,
                PaneLayoutPolicy::Accordion,
                &UiConfig::default().styles,
                &mut moved,
            ),
            Some(RenderedCursor {
                column: 14,
                row: 0,
                shape: CursorShape::Bar,
                blinking: true,
            })
        );
        assert_eq!(moved[(0, 0)].fg, Color::DarkGray);
        assert!(moved[(13, 0)].modifier.contains(Modifier::BOLD));

        let tiny = Rect::new(0, 0, 20, 2);
        let mut tiny_buffer = Buffer::empty(tiny);
        assert_eq!(
            render_view(
                &state,
                tiny,
                PaneLayoutPolicy::Accordion,
                &UiConfig::default().styles,
                &mut tiny_buffer,
            ),
            Some(RenderedCursor {
                column: 0,
                row: 0,
                shape: CursorShape::Bar,
                blinking: true,
            })
        );
        assert_eq!(tiny_buffer[(0, 0)].symbol(), "B");
        assert!((0..tiny.width).all(|column| tiny_buffer[(column, 0)].symbol() != "┃"));
    }

    #[test]
    fn host_cursor_styles_cover_shapes_blinking_caching_and_modal_reset() {
        let mut state = HostCursorState::default();
        let mut output = Vec::new();
        let cursor = |shape, blinking| {
            Some(RenderedCursor {
                column: 0,
                row: 0,
                shape,
                blinking,
            })
        };

        state
            .apply(&mut output, cursor(CursorShape::Block, true))
            .unwrap();
        state
            .apply(&mut output, cursor(CursorShape::Block, true))
            .unwrap();
        state
            .apply(&mut output, cursor(CursorShape::Block, false))
            .unwrap();
        state
            .apply(&mut output, cursor(CursorShape::Underline, true))
            .unwrap();
        state
            .apply(&mut output, cursor(CursorShape::Underline, false))
            .unwrap();
        state
            .apply(&mut output, cursor(CursorShape::Bar, true))
            .unwrap();
        state
            .apply(&mut output, cursor(CursorShape::Bar, false))
            .unwrap();
        state.apply(&mut output, None).unwrap();
        state.apply(&mut output, None).unwrap();
        state
            .apply(&mut output, cursor(CursorShape::Bar, false))
            .unwrap();

        assert_eq!(
            output,
            b"\x1b[1 q\x1b[2 q\x1b[3 q\x1b[4 q\x1b[5 q\x1b[6 q\x1b[0 q\x1b[6 q"
        );

        let mut cleanup = Vec::new();
        restore_host_cursor(&mut cleanup).unwrap();
        assert_eq!(cleanup, b"\x1b[0 q\x1b[?25h");
    }

    #[test]
    fn style_conversion_preserves_indexed_rgb_and_modifiers() {
        let converted = style(
            CellStyle::new(
                Some(CellColor::Indexed(1)),
                Some(CellColor::Rgb(Rgb {
                    red: 4,
                    green: 5,
                    blue: 6,
                })),
                true,
                true,
                true,
                true,
            ),
            false,
        );
        assert_eq!(converted.fg, Some(Color::Indexed(1)));
        assert_eq!(converted.bg, Some(Color::Rgb(4, 5, 6)));
        assert!(converted.add_modifier.contains(
            Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::REVERSED
        ));

        assert_eq!(
            Color::from(CellColor::Rgb(Rgb {
                red: 1,
                green: 2,
                blue: 3,
            })),
            Color::Rgb(1, 2, 3)
        );

        assert!(
            style(CellStyle::default(), true)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !style(CellStyle::new(None, None, false, false, false, true), true,)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    fn linked_screen(text: &str, uri: &str) -> ScreenSnapshot {
        let mut screen = ScreenSnapshot::new(
            1,
            TerminalSize {
                columns: text.len() as u16,
                rows: 1,
            },
            text.chars()
                .map(|character| crate::domain::Cell {
                    contents: character.to_string().into(),
                    hyperlink: Some(0),
                    ..crate::domain::Cell::default()
                })
                .collect(),
            Cursor {
                column: 0,
                row: 0,
                visible: false,
                shape: CursorShape::Block,
                blinking: false,
            },
        )
        .unwrap();
        screen.hyperlinks.push(uri.into());
        screen
    }

    #[test]
    fn screen_rendering_re_emits_explicit_hyperlinks_with_their_cell_width() {
        let text = "Example link";
        let screen = linked_screen(text, "https://example.com");

        let rendered = bench::render_snapshot(&screen);
        for (column, character) in text.chars().enumerate() {
            let cell = &rendered[(column as u16, 0)];
            assert_eq!(
                cell.symbol(),
                format!("\x1b]8;;https://example.com\x1b\\{character}\x1b]8;;\x1b\\")
            );
            assert_eq!(
                cell.diff_option,
                CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap())
            );
        }
    }

    #[test]
    fn hyperlink_cells_do_not_span_dialog_overlays() {
        let text = "linked filename across dialog";
        let screen = linked_screen(text, "file:///tmp/example");

        let mut rendered = bench::render_snapshot(&screen);
        dialog::render_frame(Rect::new(8, 0, 10, 1), &mut rendered);

        assert!((8..18).all(|column| {
            rendered[(column, 0)].diff_option == CellDiffOption::None
                && rendered[(column, 0)].symbol() == " "
        }));
        assert_eq!(
            rendered[(7, 0)].diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap())
        );
    }

    #[test]
    fn temporary_commands_have_one_dashed_frame_and_an_inset_pty() {
        let host = Rect::new(0, 0, 80, 24);
        let area = crate::command::PopupSize {
            width: Some(48),
            height: Some(8),
        }
        .area(host);
        let mut buffer = Buffer::empty(host);
        let content = render_temporary_command_frame(
            area,
            "Repository diff",
            &config::StylesConfig::default(),
            &mut buffer,
        );

        assert_eq!(area, Rect::new(16, 8, 48, 8));
        assert_eq!(content, Rect::new(17, 9, 46, 6));
        let text = (area.y..area.bottom())
            .map(|row| {
                (area.x..area.right())
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Repository diff"));
        assert!(text.contains("temporary · returns when command exits"));
        assert_eq!(buffer[(area.x, area.y)].symbol(), "·");
        assert_eq!(buffer[(area.x + 1, area.y)].symbol(), "┄");
        assert_eq!(buffer[(area.x, area.y + 1)].symbol(), "┆");
    }

    #[test]
    fn shared_terminal_gutter_is_a_muted_sparse_star_field() {
        let area = Rect::new(0, 0, 30, 10);
        let mut buffer = Buffer::empty(area);
        render_shared_size_gutter(
            area,
            TerminalSize {
                columns: 4,
                rows: 2,
            },
            Style::default().add_modifier(Modifier::DIM),
            Style::default().add_modifier(Modifier::BOLD),
            &mut buffer,
        );

        for row in 0..2 {
            for column in 0..4 {
                assert_eq!(buffer[(column, row)].symbol(), " ");
            }
        }
        assert_eq!(buffer[(4, 0)].symbol(), "│");
        assert_eq!(buffer[(0, 2)].symbol(), "─");
        assert_eq!(buffer[(4, 2)].symbol(), "┘");
        let stars = (0..area.height)
            .flat_map(|row| (0..area.width).map(move |column| (column, row)))
            .filter(|&(column, row)| matches!(buffer[(column, row)].symbol(), "·" | "⋆" | "✦"))
            .count();
        assert!(stars > 0);
        assert!(stars < 20, "the star field stays sparse");
    }

    #[cfg(unix)]
    fn clipboard_script(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("clipboard");
        std::fs::write(&script, format!("#!/bin/sh\n{contents}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (directory, script)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clipboard_process_success_failure_timeout_and_missing_executable_are_bounded() {
        let capture_dir = tempfile::tempdir().unwrap();
        let capture = capture_dir.path().join("copied");
        let (_success_dir, success) = clipboard_script(&format!("cat > '{}'", capture.display()));
        assert_eq!(
            copy_to_clipboard(&success, "selected λ雪".into(), Duration::from_secs(1))
                .await
                .unwrap(),
            "selected λ雪".len()
        );
        assert_eq!(std::fs::read_to_string(capture).unwrap(), "selected λ雪");

        let (_failure_dir, failure) = clipboard_script("cat >/dev/null; exit 23");
        assert!(
            copy_to_clipboard(&failure, "retry me".into(), Duration::from_secs(1))
                .await
                .unwrap_err()
                .to_string()
                .contains("exit status: 23")
        );

        let pid_dir = tempfile::tempdir().unwrap();
        let pid_file = pid_dir.path().join("pid");
        let (_timeout_dir, timeout_script) = clipboard_script(&format!(
            "echo $$ > '{}'; exec sleep 30",
            pid_file.display()
        ));
        // The deadline must comfortably outlast shell startup: macOS can
        // spend over 100ms scanning a freshly written script on first exec,
        // and the pid file must exist by the time the timeout fires.
        let error = copy_to_clipboard(
            &timeout_script,
            "blocked".into(),
            Duration::from_millis(1000),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let pid = std::fs::read_to_string(pid_file).unwrap();
        let status = std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "timed-out clipboard process survived");

        assert!(
            copy_to_clipboard(
                Path::new("/definitely/missing/fut-pbcopy"),
                "text".into(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("start")
        );
    }
}
