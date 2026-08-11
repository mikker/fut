//! Interactive terminal client for a running Fut daemon.

mod actions;
mod cheatsheet;
mod chrome;
mod command_bar;
pub(crate) mod config;
mod copy_mode;
mod dialog;
mod fuzzy;
mod git;
pub(crate) mod input;
mod layout;
mod navigation;
mod navigator;
mod notifications;
mod perf;
mod presentation;
mod rename;
mod sidebar;
mod tab_bar;
mod temporary_command;

use std::{io, path::Path, process::Stdio, time::Duration};

use actions::{ClientAction, FocusDirection, NavigationScope};
use anyhow::{Context, bail};
use bytes::Bytes;
use chrome::{MIN_DOCKED_TERMINAL_WIDTH, ResourceState, client_layout, render_tab_bar, sanitize};
use command_bar::{CommandBarAction, CommandBarState};
use config::{MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH, PaneLayoutPolicy, UiConfig};
use copy_mode::{
    CopyModeErrorDisposition, CopyModeInput, CopyModePaste, CopyModeReply, CopyModeState,
};
use crossterm::{
    SynchronizedUpdate,
    cursor::{Hide, SetCursorStyle, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyModifiers, MouseButton as HostMouseButton,
        MouseEvent as HostMouseEvent, MouseEventKind as HostMouseEventKind,
    },
    execute,
    terminal::{
        DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use futures_util::{SinkExt, StreamExt};
use git::GitStatusCache;
use input::{PrefixAction, PrefixState, encode_key};
use layout::{
    PaneLayout, SplitDivider, authored_layout, authored_navigation_layout, directional_neighbor,
    navigation_pane_layouts, pane_layouts,
};
use navigation::NavigationHistory;
use navigator::{NavigatorAction, NavigatorState};
use notifications::{NotificationsAction, NotificationsDialog};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Clear, Widget},
};
use rename::{RenameAction, RenameState};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    signal::unix::{Signal, SignalKind, signal},
    sync::mpsc,
    time,
};
use tokio_util::codec::Framed;
use uuid::Uuid;

use sidebar::{WorkspaceSidebarAction, WorkspaceSidebarState, render_workspace_sidebar};
use tab_bar::{TabBarAction, TabBarState};
use temporary_command::{TemporaryCommandSurface, TemporaryCommandUpdate};

use crate::{
    domain::{
        CellColor, CellStyle, CursorShape, MouseButton, MouseButtons, MouseEvent, MouseEventKind,
        MouseModifiers, MouseWheelDirection, ScreenDelta, ScreenSnapshot, ScrollPosition, SplitId,
        TerminalId, TerminalSize,
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
    Notifications(NotificationsDialog),
    WorkspaceSidebar(WorkspaceSidebarState),
    TabBar(TabBarState),
    CommandBar(CommandBarState),
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
        position: config::WorkspaceSidebarPosition,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SidebarDivider {
    position: config::WorkspaceSidebarPosition,
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
pub async fn attach(socket_path: &Path, selector: Option<TargetSelector>) -> anyhow::Result<()> {
    attach_with_ui(socket_path, selector, load_ui_config()?).await
}

/// Open a lease-free global navigator on an existing daemon, then attach only
/// after the user chooses a destination.
pub async fn attach_navigator(socket_path: &Path) -> anyhow::Result<()> {
    let ui = load_ui_config()?;
    let mut navigator_connection = connect_control_navigator(socket_path).await?;
    let snapshot = match time::timeout(Duration::from_secs(2), receive(&mut navigator_connection))
        .await
        .context("daemon resource snapshot timed out")??
    {
        ServerMessage::Resources { snapshot } => snapshot,
        ServerMessage::Error { code, message } => bail!("daemon error ({code}): {message}"),
        message => bail!("expected resources from daemon, received {message:?}"),
    };

    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let selector = initial_navigator(&mut terminal, &mut navigator_connection, snapshot).await?;
    drop(navigator_connection);
    let Some(selector) = selector else {
        drop(terminal);
        drop(guard);
        return Ok(());
    };

    let (columns, rows) = crossterm::terminal::size().context("read terminal size")?;
    let size = TerminalSize { columns, rows };
    let (mut framed, selected) = connect_interactive(socket_path, Some(selector), size).await?;
    let result = run(&mut terminal, &mut framed, selected, ui).await;
    drop(terminal);
    drop(guard);
    result
}

pub(crate) fn load_ui_config() -> anyhow::Result<UiConfig> {
    config::load()
}

pub(crate) async fn attach_with_ui(
    socket_path: &Path,
    selector: Option<TargetSelector>,
    ui: UiConfig,
) -> anyhow::Result<()> {
    let (columns, rows) = crossterm::terminal::size().context("read terminal size")?;
    let (mut framed, selected) =
        connect_interactive(socket_path, selector, TerminalSize { columns, rows }).await?;

    // Host terminal state is changed only after a successful handshake.
    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = run(&mut terminal, &mut framed, selected, ui).await;
    drop(terminal);
    drop(guard);
    result
}

async fn connect_interactive(
    socket_path: &Path,
    selector: Option<TargetSelector>,
    size: TerminalSize,
) -> anyhow::Result<(
    Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    SelectedView,
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
            ..
        } if version == PROTOCOL_VERSION => selected,
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
    Ok((framed, selected))
}

async fn connect_control_navigator(
    socket_path: &Path,
) -> anyhow::Result<Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>> {
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
    match time::timeout(Duration::from_secs(2), receive(&mut framed))
        .await
        .context("daemon handshake timed out")??
    {
        ServerMessage::Welcome {
            version,
            selected: None,
            ..
        } if version == PROTOCOL_VERSION => {}
        ServerMessage::IncompatibleProtocol { client, server } => {
            bail!("incompatible protocol: client {client}, server {server}")
        }
        ServerMessage::Error { code, message } => bail!("daemon error ({code}): {message}"),
        message => bail!("expected control welcome from daemon, received {message:?}"),
    }
    send_request(
        &mut framed,
        Some(Uuid::new_v4()),
        ClientMessage::WatchResources,
    )
    .await?;
    Ok(framed)
}

async fn initial_navigator(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    snapshot: ResourceSnapshot,
) -> anyhow::Result<Option<TargetSelector>> {
    let mut navigator = NavigatorState::open_global();
    navigator.accept_global_resources(&snapshot);
    let mut events = EventStream::new();
    let mut termination = TerminationSignals::subscribe()?;
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            frame.render_widget(Clear, area);
            navigator.render(area, 0, frame.buffer_mut());
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
                    ServerMessage::ResourcesChanged { snapshot } | ServerMessage::Resources { snapshot } => {
                        navigator.accept_global_resources(&snapshot);
                    }
                    ServerMessage::Error { code, message } => bail!("daemon error ({code}): {message}"),
                    _ => {}
                }
            }
        }
    }
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    selected: SelectedView,
    mut ui: UiConfig,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut termination = TerminationSignals::subscribe()?;
    let mut prefix = PrefixState::new(ui.bindings.clone());
    let mut mouse_input = MouseInputState::default();
    let mut view = ViewState::new(selected)?;
    let mut resources = ResourceState::default();
    let mut surface: Option<ClientSurface> = None;
    let mut temporary_command: Option<TemporaryCommandSurface> = None;
    let mut copy_mode: Option<CopyModeState> = None;
    let mut rename: Option<RenameState> = None;
    let mut workspace_history = NavigationHistory::default();
    workspace_history.record(view.focused());
    let mut create_workspace = CreateState::default();
    let mut create_tab = CreateState::default();
    let mut split_pane = CreateState::default();
    let mut focus = FocusState::default();
    let mut notice: Option<String> = None;
    let mut pending_focused_exit: Option<Option<i32>> = None;
    let mut force_draw = false;
    let mut cheatsheet_at: Option<time::Instant> = None;
    let mut cheatsheet_visible = false;
    let mut spinner_frame = 0usize;
    let mut host_cursor = HostCursorState::default();
    let mut perf = perf::PerfLog::from_env();
    let mut redraw = time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut spinner = time::interval(Duration::from_millis(100));
    spinner.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut git_refresh = time::interval(git::REFRESH_INTERVAL);
    git_refresh.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let (clipboard_results, mut clipboard_result) = mpsc::channel(1);
    let (git_updates, mut git_update) = mpsc::channel(1);
    let git = GitStatusCache::new(git_updates);
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
                    TemporaryCommandUpdate::Event(event) => {
                        temporary_command = None;
                        view.invalidate_drawn();
                        if let Some(crate::terminal::TerminalEvent::Error { message }) = event {
                            notice = Some(format!("command failed · {}", sanitize(&message)));
                        }
                        resize_view(framed, terminal.size()?.into(), &mut view, &resources, &ui).await?;
                        force_draw = true;
                    }
                }
            }
            frame = framed.next() => {
                let Some(frame) = frame else {
                    if let Some(Some(code)) = pending_focused_exit {
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
                        if view.accept(terminal_id, screen)
                            && terminal_id == view.focused().terminal_id
                            && matches!(
                                surface.as_ref(),
                                Some(ClientSurface::Navigator(nav))
                                    if nav.switch_request.is_none()
                                        && matches!(nav.status, navigator::NavigatorStatus::Switching)
                            )
                        {
                            surface = None;
                        }
                    }
                    ServerMessage::SnapshotDelta { terminal_id, delta } => {
                        match view.accept_delta(terminal_id, delta) {
                            DeltaApplyResult::Applied => {
                                if terminal_id == view.focused().terminal_id
                                    && matches!(
                                        surface.as_ref(),
                                        Some(ClientSurface::Navigator(nav))
                                            if nav.switch_request.is_none()
                                                && matches!(nav.status, navigator::NavigatorStatus::Switching)
                                    )
                                {
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
                                notice = Some(format!("copied {bytes} bytes to clipboard"));
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
                            notice = Some("copy mode cancelled".into());
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
                                notice = Some(format!("copy mode · {error}"));
                                pump_copy_mode(
                                    framed,
                                    copy_mode.as_mut().expect("recoverable copy-mode error"),
                                ).await?;
                                force_draw = true;
                            }
                            CopyModeErrorDisposition::Exit => {
                                notice = Some(format!("copy mode · {error}"));
                                copy_mode = None;
                                force_draw = true;
                            }
                        }
                    }
                    ServerMessage::Resources { snapshot } => {
                        if resources.accept(snapshot) {
                            let snapshot = resources.snapshot().expect("accepted resources exist");
                            git.refresh(snapshot.sessions.iter().flat_map(|session| &session.workspaces).map(|workspace| workspace.root.as_path()));
                            refresh_surface_resources(
                                &mut surface,
                                snapshot,
                                view.focused(),
                                &workspace_history,
                                resources.notifications(),
                            );
                            reconcile_resource_barriers(
                                snapshot,
                                &mut create_workspace,
                                &mut create_tab,
                                &mut split_pane,
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
                    ServerMessage::ResourcesChanged { snapshot } => {
                        if resources.accept(snapshot) {
                            let snapshot = resources.snapshot().expect("accepted resources exist");
                            git.refresh(snapshot.sessions.iter().flat_map(|session| &session.workspaces).map(|workspace| workspace.root.as_path()));
                            refresh_surface_resources(
                                &mut surface,
                                snapshot,
                                view.focused(),
                                &workspace_history,
                                resources.notifications(),
                            );
                            reconcile_resource_barriers(
                                snapshot,
                                &mut create_workspace,
                                &mut create_tab,
                                &mut split_pane,
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
                    ServerMessage::TargetSelected { selected: target } => {
                        let old_terminal = view.focused().terminal_id;
                        let previous_target = view.focused().clone();
                        let navigator_selected = match surface.as_mut() {
                            Some(ClientSurface::Navigator(nav)) => nav.switch_selected(request_id),
                            _ => false,
                        };
                        let focus_origin = focus.complete(request_id);
                        let workspace_selected = matches!(focus_origin, Some(FocusOrigin::Workspace));
                        let tab_selected = matches!(focus_origin, Some(FocusOrigin::Tab));
                        let notification_selected =
                            matches!(focus_origin, Some(FocusOrigin::Notification));
                        let observed_revision = resources.snapshot().map(|snapshot| snapshot.revision);
                        let workspace_created_selected = create_workspace.selected(
                            request_id,
                            &target,
                            observed_revision,
                        );
                        let create_selected =
                            create_tab.selected(request_id, &target, observed_revision);
                        let split_selected =
                            split_pane.selected(request_id, &target, observed_revision);
                        if !view.replace(target)? {
                            if navigator_selected || workspace_selected || tab_selected {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            if workspace_created_selected || create_selected || split_selected {
                                force_draw = true;
                            }
                            continue;
                        }
                        if old_terminal != view.focused().terminal_id {
                            mouse_input.clear();
                        } else {
                            mouse_input.reconcile_ui_drag(view.focused().tab_id, &view.layout);
                        }
                        workspace_history.record_transition(&previous_target, view.focused());
                        if copy_mode.as_ref().is_some_and(|copy_mode| {
                            copy_mode.terminal_id() != view.focused().terminal_id
                        }) {
                            copy_mode = None;
                            notice = Some("copy mode cancelled · focus changed".into());
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
                        } else if navigator_selected
                            && let Some(ClientSurface::Navigator(nav)) = surface.as_mut()
                        {
                            nav.status = navigator::NavigatorStatus::Switching;
                        }
                        if workspace_selected {
                            surface = None;
                            view.invalidate_drawn();
                        }
                        if tab_selected {
                            surface = None;
                            view.invalidate_drawn();
                        }
                        if notification_selected {
                            view.invalidate_drawn();
                        }
                        if workspace_created_selected || create_selected || split_selected {
                            view.invalidate_drawn();
                        }
                        force_draw = true;
                    }
                    ServerMessage::WorkspaceCreated { selected: target } => {
                        if !create_workspace.created(request_id, target.terminal_id) {
                            continue;
                        }
                        force_draw = true;
                    }
                    ServerMessage::TabCreated { selected: target } => {
                        if !create_tab.created(request_id, target.terminal_id) {
                            continue;
                        }
                        if target.terminal_id == view.focused().terminal_id {
                            view.invalidate_drawn();
                        }
                        force_draw = true;
                    }
                    ServerMessage::PaneCreated { selected: target } => {
                        if !split_pane.created(request_id, target.terminal_id) {
                            continue;
                        }
                        force_draw = true;
                    }
                    ServerMessage::PaneMoved { .. } => {
                        // Pane movement is currently a control-plane operation.
                        bail!("unexpected pane movement response")
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
                    ServerMessage::Pong { .. }
                    | ServerMessage::CommandCompleted { .. }
                    | ServerMessage::LocationOpened { .. } => {}
                    ServerMessage::TerminalExited { terminal_id, exit_code } => {
                        if copy_mode
                            .as_ref()
                            .is_some_and(|copy_mode| copy_mode.terminal_id() == terminal_id)
                        {
                            copy_mode = None;
                        }
                        if terminal_id == view.focused().terminal_id {
                            mouse_input.clear();
                            pending_focused_exit = Some(exit_code);
                            force_draw = true;
                            continue;
                        }
                        view.remove(terminal_id);
                        mouse_input.reconcile_ui_drag(view.focused().tab_id, &view.layout);
                        force_draw = true;
                    }
                    ServerMessage::Detached => break,
                    ServerMessage::Error { code, message } => {
                        let copy_failure = copy_mode
                            .as_mut()
                            .map_or(CopyModeErrorDisposition::Ignored, |copy_mode| {
                                copy_mode.fail(request_id)
                            });
                        match copy_failure {
                            CopyModeErrorDisposition::Ignored => {}
                            CopyModeErrorDisposition::Continue => {
                                notice = Some(format!("copy mode failed · {message}"));
                                pump_copy_mode(
                                    framed,
                                    copy_mode.as_mut().expect("recoverable copy-mode failure"),
                                ).await?;
                                force_draw = true;
                                continue;
                            }
                            CopyModeErrorDisposition::Exit => {
                                copy_mode = None;
                                notice = Some(format!("copy mode failed · {message}"));
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
                        if create_workspace.fail(request_id) {
                            notice = Some(format!("create workspace failed · {message}"));
                            force_draw = true;
                            continue;
                        }
                        if create_tab.fail(request_id) {
                            notice = Some(format!("create tab failed · {message}"));
                            force_draw = true;
                            continue;
                        }
                        if split_pane.fail(request_id) {
                            notice = Some(format!("split failed · {message}"));
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
                                Some(FocusOrigin::Workspace) => {
                                    if let Some(ClientSurface::WorkspaceSidebar(sidebar)) = surface.as_mut() {
                                        sidebar.switch_error(message);
                                        force_draw = true;
                                    } else {
                                        notice = Some(format!("workspace unavailable · {message}"));
                                        force_draw = true;
                                    }
                                }
                                Some(FocusOrigin::Pane) => {
                                    notice = Some(format!("pane unavailable · {message}"));
                                    force_draw = true;
                                }
                                Some(FocusOrigin::Tab) => {
                                    notice = Some(format!("tab unavailable · {message}"));
                                    force_draw = true;
                                }
                                Some(FocusOrigin::Session) => {
                                    notice = Some(format!("session unavailable · {message}"));
                                    force_draw = true;
                                }
                                Some(FocusOrigin::Notification) => {
                                    notice = Some(format!("notification unavailable · {message}"));
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
                    | ServerMessage::AgentSettled { .. } => {
                        bail!("unexpected control response on interactive connection")
                    }
                }
            }
            event = events.next(), if accepts_client_input(&focus, &create_workspace, &create_tab, &split_pane, &pending_focused_exit) => {
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
                    mouse_input.cancel_ui_drag();
                }
                match event {
                    Event::Key(key) if temporary_command.is_some() => {
                        if let Some(bytes) = encode_key(key) {
                            temporary_command.as_ref().expect("command exists").input(bytes).await?;
                        }
                    }
                    Event::Paste(text) if temporary_command.is_some() => {
                        temporary_command.as_ref().expect("command exists").paste(text).await?;
                    }
                    Event::Resize(columns, rows) if temporary_command.is_some() && columns > 0 && rows > 0 => {
                        temporary_command.as_ref().expect("command exists").resize(TerminalSize { columns, rows }).await?;
                        force_draw = true;
                    }
                    Event::Key(key) if copy_mode.is_some() => {
                        notice = None;
                        let input = copy_mode.as_mut().expect("copy mode exists").key(key);
                        match input {
                            CopyModeInput::Stay => {}
                            CopyModeInput::Pump => {
                                pump_copy_mode(
                                    framed,
                                    copy_mode.as_mut().expect("copy mode exists"),
                                ).await?;
                            }
                            CopyModeInput::Notice(message) => notice = Some(message.into()),
                        }
                        force_draw = true;
                    }
                    Event::Paste(text) if copy_mode.is_some() => {
                        notice = match copy_mode
                            .as_mut()
                            .expect("copy mode exists")
                            .paste(&text)
                        {
                            CopyModePaste::Accepted | CopyModePaste::Ignored => None,
                            CopyModePaste::TooLarge => Some(
                                "search query is too large; paste was not added".into(),
                            ),
                        };
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
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::Notifications(_))) => {
                        notice = None;
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
                        }
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::Navigator(_))) => {
                        notice = None;
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
                    Event::Paste(text) if matches!(surface.as_ref(), Some(ClientSurface::Navigator(_))) => {
                        if let Some(ClientSurface::Navigator(navigator)) = surface.as_mut() {
                            navigator.paste(&text);
                            force_draw = true;
                        }
                    }
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::WorkspaceSidebar(_))) => {
                        notice = None;
                        let action = match surface.as_mut().expect("workspace sidebar exists") {
                            ClientSurface::WorkspaceSidebar(sidebar) => sidebar.key(key),
                            _ => unreachable!("surface guard ensures workspace sidebar"),
                        };
                        match action {
                            WorkspaceSidebarAction::Stay => force_draw = true,
                            WorkspaceSidebarAction::Close => {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            WorkspaceSidebarAction::Create => {
                                if let Some(request) = create_workspace.begin() {
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
                            WorkspaceSidebarAction::CycleVisibility => {
                                ui.workspace_sidebar.visibility.cycle();
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
                            WorkspaceSidebarAction::Rename(workspace_id, name) => {
                                rename = Some(RenameState::open(
                                    crate::protocol::RenameSelector::Workspace(workspace_id),
                                    "workspace",
                                    name,
                                ));
                                force_draw = true;
                            }
                            WorkspaceSidebarAction::Select(pane_id) => {
                                release_captured_mouse_input(
                                    framed,
                                    &mut mouse_input,
                                    view.focused().terminal_id,
                                ).await?;
                                if let Some(request) = focus.begin(FocusOrigin::Workspace) {
                                    match surface.as_mut().expect("workspace sidebar exists") {
                                        ClientSurface::WorkspaceSidebar(sidebar) => sidebar.begin_switch(),
                                        _ => unreachable!("surface guard ensures workspace sidebar"),
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
                                                    NavigationScope::Workspace,
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
                        notice = None;
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
                                if let Some(request) = create_tab.begin() {
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
                        notice = None;
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
                                notice = dispatch_client_action(
                                    action,
                                    framed,
                                    &mut view,
                                    &resources,
                                    &mut surface,
                                    &workspace_history,
                                    &mut create_tab,
                                    &mut split_pane,
                                    &mut focus,
                                    &mut copy_mode,
                                    &mut prefix,
                                    terminal.size()?.into(),
                                    &mut ui,
                                    &mut temporary_command,
                                ).await?;
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
                            resources.workspace_count(view.focused()),
                        );
                        let unobstructed = app_overlay_clear(
                            surface.is_none(),
                            rename.is_none(),
                            copy_mode.is_none(),
                            cheatsheet_visible,
                        );
                        let open_sidebar = rename.is_none()
                            && copy_mode.is_none()
                            && matches!(surface.as_ref(), Some(ClientSurface::WorkspaceSidebar(_)));
                        let visible_sidebar = layout.workspace_sidebar.and_then(|sidebar| {
                            (open_sidebar || (unobstructed && sidebar.docked().is_some()))
                                .then(|| {
                                    let max_width = if sidebar.docked().is_some() {
                                        host.width
                                            .saturating_sub(MIN_DOCKED_TERMINAL_WIDTH)
                                            .clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH)
                                    } else {
                                        MAX_SIDEBAR_WIDTH
                                    };
                                    sidebar_divider(
                                        sidebar.area(),
                                        ui.workspace_sidebar.position,
                                        max_width,
                                    )
                                })
                                .flatten()
                        });
                        let split_dividers = if unobstructed {
                            view.pane_layouts(layout.terminal, ui.pane_layout).1
                        } else {
                            Vec::new()
                        };
                        match mouse_input.route_ui(
                            mouse,
                            host,
                            visible_sidebar,
                            view.focused().tab_id,
                            &split_dividers,
                        ) {
                            UiMouseRoute::Owned(Some(UiResizeAction::Sidebar { width })) => {
                                ui.workspace_sidebar.width = width;
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
                        notice = None;
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
                                notice = dispatch_client_action(
                                    action,
                                    framed,
                                    &mut view,
                                    &resources,
                                    &mut surface,
                                    &workspace_history,
                                    &mut create_tab,
                                    &mut split_pane,
                                    &mut focus,
                                    &mut copy_mode,
                                    &mut prefix,
                                    terminal.size()?.into(),
                                    &mut ui,
                                    &mut temporary_command,
                                ).await?;
                                force_draw = true;
                            }
                            PrefixAction::Send(bytes) => send(framed, ClientMessage::Input { bytes }).await?,
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
                            if let Some(copy_id) = copy_mode
                                .as_mut()
                                .and_then(|state| state.finish_clipboard(request_id))
                            {
                                let state = copy_mode
                                    .as_mut()
                                    .expect("accepted clipboard copy is active");
                                state.finalize_copy(copy_id, bytes);
                                notice = None;
                                pump_copy_mode(framed, state).await?;
                                force_draw = true;
                            }
                        }
                        ClipboardResult::Failed { request_id, message } => {
                            if copy_mode
                                .as_mut()
                                .and_then(|state| state.finish_clipboard(request_id))
                                .is_some()
                            {
                                notice = Some(format!(
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
            Some(()) = git_update.recv() => {
                force_draw = true;
            }
            _ = git_refresh.tick(), if resources.snapshot().is_some() => {
                let snapshot = resources.snapshot().expect("git refresh requires resources");
                git.refresh(
                    snapshot
                        .sessions
                        .iter()
                        .flat_map(|session| &session.workspaces)
                        .map(|workspace| workspace.root.as_path()),
                );
            }
            _ = spinner.tick(), if resources.has_working() => {
                spinner_frame = spinner_frame.wrapping_add(1);
                force_draw = true;
            }
            _ = redraw.tick(), if force_draw || view.needs_draw() => {
                let focused_terminal_id = view.focused().terminal_id;
                let rendered_attention = matches!(
                    surface.as_ref(),
                    None
                        | Some(ClientSurface::WorkspaceSidebar(_))
                        | Some(ClientSurface::TabBar(_))
                )
                .then(|| resources.attention_revision(focused_terminal_id))
                .flatten()
                .filter(|_| rename.is_none() && notice.is_none());
                let draw_started = std::time::Instant::now();
                io::stdout().sync_update(|stdout| -> io::Result<()> {
                    let mut rendered_cursor = None;
                    terminal.draw(|frame| {
                        let area = frame.area();
                        if let Some(command) = temporary_command.as_ref() {
                            frame.render_widget(Screen(&command.screen), area);
                            if command.screen.cursor.visible {
                                frame.set_cursor_position((command.screen.cursor.column, command.screen.cursor.row));
                                rendered_cursor = Some(RenderedCursor {
                                    column: command.screen.cursor.column,
                                    row: command.screen.cursor.row,
                                    shape: command.screen.cursor.shape,
                                    blinking: command.screen.cursor.blinking,
                                });
                            }
                            return;
                        }
                        let layout = client_layout(
                            area,
                            &ui,
                            resources.workspace_count(view.focused()),
                        );
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
                        if let Some(ClientSurface::WorkspaceSidebar(sidebar)) = surface.as_ref() {
                            if let Some(sidebar_area) =
                                layout.workspace_sidebar.map(|sidebar| sidebar.area())
                            {
                                sidebar.render(
                                    sidebar_area,
                                    ui.workspace_sidebar.position,
                                    &ui,
                                    &git,
                                    spinner_frame,
                                    frame.buffer_mut(),
                                );
                            }
                        } else if let Some(sidebar_area) =
                            layout.workspace_sidebar.and_then(|sidebar| sidebar.docked())
                        {
                            render_workspace_sidebar(
                                resources.snapshot(),
                                view.focused(),
                                &workspace_history,
                                resources.notifications(),
                                spinner_frame,
                                sidebar_area,
                                ui.workspace_sidebar.position,
                                &ui,
                                &git,
                                frame.buffer_mut(),
                            );
                        }
                        match surface.as_mut() {
                            Some(ClientSurface::Navigator(nav)) => {
                                nav.render(area, spinner_frame, frame.buffer_mut());
                            }
                            Some(ClientSurface::Notifications(dialog)) => {
                                dialog.render(area, frame.buffer_mut());
                            }
                            Some(ClientSurface::CommandBar(command_bar)) => {
                                command_bar.render(layout.terminal, frame.buffer_mut());
                            }
                            Some(ClientSurface::WorkspaceSidebar(_))
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
                            copy_mode.render(
                                layout.terminal,
                                frame.buffer_mut(),
                                notice.as_deref(),
                            );
                        }
                        if app_overlay_clear(
                            surface.is_none(),
                            rename.is_none(),
                            copy_mode.is_none(),
                            cheatsheet_visible,
                        )
                            && notice.is_none()
                            && let Some(cursor) = cursor
                        {
                            frame.set_cursor_position((cursor.column, cursor.row));
                            rendered_cursor = Some(cursor);
                        }
                        if copy_mode.is_none()
                            && let Some(message) = notice.as_deref()
                        {
                            render_notice(area, frame.buffer_mut(), message);
                        }
                    })?;
                    host_cursor.apply(stdout, rendered_cursor)
                })??;
                if let Some(perf) = perf.as_mut() {
                    perf.record("draw", draw_started.elapsed(), 0);
                }
                view.mark_drawn();
                force_draw = rendered_attention
                    .is_some_and(|revision| resources.observe(focused_terminal_id, revision));
            }
        }
    }
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
            nav.accept_resources_with_notifications(snapshot, focused, notifications);
        }
        Some(ClientSurface::WorkspaceSidebar(sidebar)) => {
            sidebar.accept_resources(snapshot, focused, workspace_history, notifications);
        }
        Some(ClientSurface::TabBar(tab_bar)) => {
            tab_bar.accept_resources(snapshot, focused, workspace_history);
        }
        Some(ClientSurface::Notifications(dialog)) => {
            dialog.accept_resources(snapshot, notifications);
        }
        Some(ClientSurface::CommandBar(_)) | None => {}
    }
}

fn reconcile_resource_barriers(
    snapshot: &crate::resources::ResourceSnapshot,
    create_workspace: &mut CreateState,
    create_tab: &mut CreateState,
    split_pane: &mut CreateState,
    rename: &mut Option<RenameState>,
) {
    create_workspace.accept_resources(snapshot.revision);
    create_tab.accept_resources(snapshot.revision);
    split_pane.accept_resources(snapshot.revision);
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
    snapshot.sessions.iter().find_map(|session| {
        session.workspaces.iter().find_map(|workspace| {
            workspace.tabs.iter().find_map(|tab| {
                tab.panes
                    .iter()
                    .any(|pane| pane.id == pane_id)
                    .then_some(match scope {
                        NavigationScope::Pane | NavigationScope::Tab => {
                            SelectionExpectation::Tab(tab.id)
                        }
                        NavigationScope::Workspace => SelectionExpectation::Workspace(workspace.id),
                        NavigationScope::Session => SelectionExpectation::Session(session.id),
                    })
            })
        })
    })
}

fn accepts_client_input(
    focus: &FocusState,
    create_workspace: &CreateState,
    create_tab: &CreateState,
    split_pane: &CreateState,
    pending_focused_exit: &Option<Option<i32>>,
) -> bool {
    focus.request_id.is_none()
        && !create_workspace.blocks_input()
        && !create_tab.blocks_input()
        && !split_pane.blocks_input()
        && pending_focused_exit.is_none()
}

impl MouseInputState {
    fn route_ui(
        &mut self,
        mouse: HostMouseEvent,
        host: Rect,
        sidebar: Option<SidebarDivider>,
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
                    (width != last_width).then_some(UiResizeAction::Sidebar { width })
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
                        UiResizeAction::Sidebar { width },
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
        if let Some(sidebar) = sidebar
            && rect_contains(sidebar.area, mouse.column, mouse.row)
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

    fn cancel_ui_drag(&mut self) {
        self.ui_drag = None;
    }

    fn reconcile_ui_drag(&mut self, tab_id: crate::domain::TabId, layout: &SplitTree) {
        if matches!(
            self.ui_drag,
            Some(UiDrag::Split {
                tab_id: drag_tab,
                split_id,
                ..
            }) if drag_tab != tab_id || layout.ratio(split_id).is_none()
        ) {
            self.ui_drag = None;
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
    position: config::WorkspaceSidebarPosition,
    column: u16,
    max_width: u16,
) -> u16 {
    let width = match position {
        config::WorkspaceSidebarPosition::Left => column.saturating_sub(host.x).saturating_add(1),
        config::WorkspaceSidebarPosition::Right => host.right().saturating_sub(column),
    };
    width.clamp(MIN_SIDEBAR_WIDTH, max_width)
}

fn sidebar_divider(
    sidebar: Rect,
    position: config::WorkspaceSidebarPosition,
    max_width: u16,
) -> Option<SidebarDivider> {
    if sidebar.width < MIN_SIDEBAR_WIDTH || sidebar.height == 0 {
        return None;
    }
    let x = match position {
        config::WorkspaceSidebarPosition::Left => sidebar.right() - 1,
        config::WorkspaceSidebarPosition::Right => sidebar.x,
    };
    Some(SidebarDivider {
        position,
        area: Rect::new(x, sidebar.y, 1, sidebar.height),
        current_width: sidebar.width,
        max_width,
    })
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
    reason = "the dispatcher explicitly borrows the small client states it coordinates"
)]
async fn dispatch_client_action(
    action: ClientAction,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    view: &mut ViewState,
    resources: &ResourceState,
    surface: &mut Option<ClientSurface>,
    workspace_history: &NavigationHistory,
    create_tab: &mut CreateState,
    split_pane: &mut CreateState,
    focus: &mut FocusState,
    copy_mode: &mut Option<CopyModeState>,
    prefix: &mut PrefixState,
    host: Rect,
    ui: &mut UiConfig,
    temporary_command: &mut Option<TemporaryCommandSurface>,
) -> anyhow::Result<Option<String>> {
    match action {
        ClientAction::RunCommand(index) => {
            let Some(command) = ui.bindings.command(index) else {
                return Ok(Some("configured command is no longer available".into()));
            };
            let size = TerminalSize {
                columns: host.width,
                rows: host.height,
            };
            let fallback = std::env::current_dir().unwrap_or_else(|_| "/".into());
            match TemporaryCommandSurface::spawn(command, view.focused().child_pid, &fallback, size)
                .await
            {
                Ok(command) => *temporary_command = Some(command),
                Err(error) => {
                    return Ok(Some(format!("command failed · {}", one_line_error(&error))));
                }
            }
        }
        ClientAction::OpenCommandBar => {
            *surface = Some(ClientSurface::CommandBar(
                CommandBarState::open_with_bindings(ui.bindings.clone()),
            ));
        }
        ClientAction::ReloadConfig => match load_ui_config() {
            Ok(reloaded) => {
                prefix.replace_bindings(reloaded.bindings.clone());
                *ui = reloaded;
                resize_view(framed, host, view, resources, ui).await?;
                view.invalidate_drawn();
                return Ok(Some("config reloaded".into()));
            }
            Err(error) => {
                return Ok(Some(format!(
                    "config reload failed · {}",
                    one_line_error(&error)
                )));
            }
        },
        ClientAction::EnterCopyMode => {
            if copy_mode.is_none() {
                let mut state = CopyModeState::enter(view.focused().terminal_id);
                pump_copy_mode(framed, &mut state).await?;
                *copy_mode = Some(state);
            }
        }
        ClientAction::OpenNavigator => {
            let mut navigator = NavigatorState::open(view.focused());
            if let Some(snapshot) = resources.snapshot() {
                navigator.accept_resources_with_notifications(
                    snapshot,
                    view.focused(),
                    resources.notifications(),
                );
            }
            *surface = Some(ClientSurface::Navigator(navigator));
        }
        ClientAction::OpenWorkspaceSidebar => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some("workspaces are still loading".into()));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some("navigation is syncing".into()));
            }
            let Some(sidebar) = WorkspaceSidebarState::open(
                snapshot,
                view.focused(),
                workspace_history,
                resources.notifications(),
            ) else {
                return Ok(Some("no workspace available".into()));
            };
            *surface = Some(ClientSurface::WorkspaceSidebar(sidebar));
        }
        ClientAction::OpenTabBar => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some("tabs are still loading".into()));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some("navigation is syncing".into()));
            }
            if client_layout(host, ui, resources.workspace_count(view.focused()))
                .tab_bar
                .is_none()
            {
                return Ok(Some("tab bar is unavailable at this size".into()));
            }
            let Some(tab_bar) = TabBarState::open(snapshot, view.focused(), workspace_history)
            else {
                return Ok(Some("no tab available".into()));
            };
            *surface = Some(ClientSurface::TabBar(tab_bar));
        }
        ClientAction::OpenNotifications => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some("notifications are still loading".into()));
            };
            *surface = Some(ClientSurface::Notifications(NotificationsDialog::open(
                snapshot,
                resources.notifications(),
            )));
        }
        ClientAction::FocusNextNotification => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some("notifications are still loading".into()));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some("navigation is syncing".into()));
            }
            let Some(pane_id) = resources
                .notifications()
                .next(snapshot, view.focused().terminal_id)
            else {
                return Ok(None);
            };
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
        ClientAction::CreateTab => {
            if let Some(request) = create_tab.begin() {
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
        ClientAction::FocusNextTab | ClientAction::FocusPreviousTab => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some("tabs are still loading".into()));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some("navigation is syncing".into()));
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
                return Ok(Some("workspaces are still loading".into()));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some("navigation is syncing".into()));
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
            if let Some(request) = split_pane.begin() {
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
                client_layout(host, ui, resources.workspace_count(view.focused())).terminal;
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
                return Ok(Some("resources are still loading".into()));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some("navigation is syncing".into()));
            }
            let pane_id = match scope {
                NavigationScope::Pane => workspace_history.last_pane(snapshot, view.focused()),
                NavigationScope::Tab => workspace_history.last_tab(snapshot, view.focused()),
                NavigationScope::Workspace => {
                    workspace_history.last_workspace(snapshot, view.focused())
                }
                NavigationScope::Session => {
                    workspace_history.last_session(snapshot, view.focused())
                }
            };
            let Some(pane_id) = pane_id else {
                return Ok(Some(format!("no previous {}", scope.label())));
            };
            if let Some(request) = focus.begin(FocusOrigin::for_scope(scope)) {
                send_request(
                    framed,
                    Some(request),
                    ClientMessage::SelectTarget {
                        selector: TargetSelector::Pane(pane_id),
                        expected: selection_expectation(snapshot, pane_id, scope),
                    },
                )
                .await?;
            }
        }
        ClientAction::FocusTab(number) => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some("tabs are still loading".into()));
            };
            if !view.resources_are_current(snapshot) {
                return Ok(Some("navigation is syncing".into()));
            }
            let number = number.get();
            let Some(pane_id) = workspace_history.numbered_tab(snapshot, view.focused(), number)
            else {
                return Ok(Some(format!("tab {number} is unavailable")));
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
                return Ok(Some("pane zoom needs more than one pane".into()));
            };
            resize_view(framed, host, view, resources, ui).await?;
        }
        ClientAction::Detach => send(framed, ClientMessage::Detach).await?,
    }
    Ok(None)
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

#[derive(Default)]
enum CreateState {
    #[default]
    Idle,
    AwaitingCreated {
        request_id: Uuid,
    },
    AwaitingSelected {
        request_id: Uuid,
        terminal_id: TerminalId,
    },
    AwaitingResources {
        request_id: Uuid,
        resource_revision: u64,
    },
}

impl CreateState {
    fn begin(&mut self) -> Option<Uuid> {
        if !matches!(self, Self::Idle) {
            return None;
        }
        let request_id = Uuid::new_v4();
        *self = Self::AwaitingCreated { request_id };
        Some(request_id)
    }

    fn created(&mut self, request_id: Option<Uuid>, terminal_id: TerminalId) -> bool {
        let Self::AwaitingCreated {
            request_id: expected,
        } = self
        else {
            return false;
        };
        if request_id != Some(*expected) {
            return false;
        }
        *self = Self::AwaitingSelected {
            request_id: *expected,
            terminal_id,
        };
        true
    }

    fn selected(
        &mut self,
        request_id: Option<Uuid>,
        selected: &SelectedView,
        observed_revision: Option<u64>,
    ) -> bool {
        let Self::AwaitingSelected {
            request_id: expected,
            terminal_id,
        } = self
        else {
            return false;
        };
        if request_id != Some(*expected) || *terminal_id != selected.focused.terminal_id {
            return false;
        }
        if observed_revision.is_some_and(|revision| revision >= selected.resource_revision) {
            *self = Self::Idle;
        } else {
            *self = Self::AwaitingResources {
                request_id: *expected,
                resource_revision: selected.resource_revision,
            };
        }
        true
    }

    fn accept_resources(&mut self, revision: u64) -> bool {
        let Self::AwaitingResources {
            resource_revision, ..
        } = self
        else {
            return false;
        };
        if revision < *resource_revision {
            return false;
        }
        *self = Self::Idle;
        true
    }

    fn fail(&mut self, request_id: Option<Uuid>) -> bool {
        let expected = match self {
            Self::Idle => return false,
            Self::AwaitingCreated { request_id }
            | Self::AwaitingSelected { request_id, .. }
            | Self::AwaitingResources { request_id, .. } => *request_id,
        };
        if request_id != Some(expected) {
            return false;
        }
        *self = Self::Idle;
        true
    }

    fn blocks_input(&self) -> bool {
        !matches!(self, Self::Idle)
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
    Notification,
}

impl FocusOrigin {
    fn for_scope(scope: NavigationScope) -> Self {
        match scope {
            NavigationScope::Pane => Self::Pane,
            NavigationScope::Tab => Self::Tab,
            NavigationScope::Workspace => Self::Workspace,
            NavigationScope::Session => Self::Session,
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
            refresh_requested: false,
        }
    }

    fn accept(&mut self, screen: ScreenSnapshot) -> bool {
        if self
            .newest_revision
            .is_some_and(|revision| screen.revision <= revision)
        {
            return false;
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
        if delta.base_revision != current.revision || delta.size != current.size {
            return DeltaOutcome::Mismatch;
        }
        let columns = usize::from(current.size.columns);
        for row in delta.rows {
            let start = usize::from(row.index) * columns;
            if row.cells.len() != columns {
                return DeltaOutcome::Mismatch;
            }
            let Some(slice) = current.cells.get_mut(start..start + columns) else {
                return DeltaOutcome::Mismatch;
            };
            slice.clone_from_slice(&row.cells);
        }
        current.revision = delta.revision;
        current.cursor = delta.cursor;
        current.scroll = delta.scroll;
        self.newest_revision = Some(delta.revision);
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
    let terminal = client_layout(area, ui, resources.workspace_count(view.focused())).terminal;
    for (terminal_id, size) in view.resize_requests(terminal, ui.pane_layout) {
        send(framed, ClientMessage::Resize { terminal_id, size }).await?;
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

fn render_notice(area: Rect, buffer: &mut Buffer, message: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = format!(" {message} ");
    buffer.set_stringn(
        area.x,
        area.y + area.height - 1,
        text,
        usize::from(area.width),
        Style::default().add_modifier(Modifier::REVERSED),
    );
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
        // Adjacent cells frequently share the same style (runs of plain
        // text, whole-line backgrounds, ...), so remember the last resolved
        // `Style` and reuse it instead of rebuilding one per cell.
        let mut resolved: Option<(CellStyle, bool, Style)> = None;
        for row in 0..screen.size.rows.min(area.height) {
            for column in 0..screen.size.columns.min(area.width) {
                let index =
                    usize::from(row) * usize::from(screen.size.columns) + usize::from(column);
                let Some(cell) = screen.cells.get(index) else {
                    continue;
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
                if let Some(target) = buffer.cell_mut((area.x + column, area.y + row)) {
                    target.set_symbol(&cell.contents).set_style(cell_style);
                }
            }
        }
    }
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
    fn create_tab_state_blocks_input_through_correlated_ack_and_selection() {
        let workspace = CreateState::default();
        let mut state = CreateState::default();
        let split = CreateState::default();
        let focus = FocusState::default();
        let no_exit = None;
        let request = state.begin().expect("first request starts");
        assert!(state.begin().is_none());
        assert!(state.blocks_input());
        assert!(!accepts_client_input(
            &focus, &workspace, &state, &split, &no_exit
        ));
        let target = targets(1).remove(0);
        assert!(!state.created(None, target.terminal_id));
        assert!(!state.created(Some(Uuid::new_v4()), target.terminal_id));
        assert!(state.created(Some(request), target.terminal_id));
        assert!(state.blocks_input());
        assert!(!accepts_client_input(
            &focus, &workspace, &state, &split, &no_exit
        ));
        let selected = selected_view(2, target.clone(), vec![target]);
        assert!(!state.selected(Some(Uuid::new_v4()), &selected, Some(1)));
        assert!(state.begin().is_none());
        assert!(state.selected(Some(request), &selected, Some(1)));
        assert!(state.blocks_input());
        assert!(!state.accept_resources(1));
        assert!(state.accept_resources(2));
        assert!(!state.blocks_input());
        assert!(accepts_client_input(
            &focus, &workspace, &state, &split, &no_exit
        ));
        assert!(state.begin().is_some());

        let mut failed = CreateState::default();
        let request = failed.begin().unwrap();
        assert!(!failed.fail(Some(Uuid::new_v4())));
        assert!(failed.fail(Some(request)));
        assert!(!failed.blocks_input());
        assert!(accepts_client_input(
            &focus, &workspace, &failed, &split, &no_exit
        ));
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
                    cursor: Cursor {
                        column: 0,
                        row: 0,
                        visible: true,
                        shape: CursorShape::Underline,
                        blinking: true,
                    },
                    scroll: ScrollPosition::default(),
                },
            ),
            DeltaApplyResult::Applied
        ));
        let cursor = state.panes[0].pending.as_ref().unwrap().cursor;
        assert_eq!(cursor.shape, CursorShape::Underline);
        assert!(cursor.blinking);
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
                None,
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
                None,
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
                None,
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
                None,
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
    fn sidebar_precedes_panes_and_both_sidebar_edges_resize_without_app_capture() {
        let panes = targets(2);
        let state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        let host = Rect::new(0, 0, 140, 24);
        let dividers = state.pane_layouts(host, PaneLayoutPolicy::Splits).1;
        let pane_divider = dividers[0];
        let sidebar = SidebarDivider {
            position: config::WorkspaceSidebarPosition::Left,
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
                Some(sidebar),
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
                None,
                panes[0].tab_id,
                &[],
            ),
            UiMouseRoute::Owned(Some(UiResizeAction::Sidebar { width: 44 }))
        );
        input.clear();

        let right = SidebarDivider {
            position: config::WorkspaceSidebarPosition::Right,
            area: Rect::new(112, 0, 1, 24),
            current_width: 28,
            max_width: 80,
        };
        input.route_ui(
            HostMouseEvent {
                kind: HostMouseEventKind::Down(HostMouseButton::Left),
                column: right.area.x,
                row: 3,
                modifiers: KeyModifiers::NONE,
            },
            host,
            Some(right),
            panes[0].tab_id,
            &dividers,
        );
        assert_eq!(
            input.route_ui(
                HostMouseEvent {
                    kind: HostMouseEventKind::Up(HostMouseButton::Left),
                    column: 119,
                    row: 3,
                    modifiers: KeyModifiers::NONE,
                },
                host,
                None,
                panes[0].tab_id,
                &dividers,
            ),
            UiMouseRoute::Owned(Some(UiResizeAction::Sidebar { width: 21 }))
        );
        assert!(input.synthetic_releases(panes[0].terminal_id).is_empty());

        assert_eq!(
            sidebar_divider(
                Rect::new(0, 0, MIN_SIDEBAR_WIDTH - 1, 24),
                config::WorkspaceSidebarPosition::Left,
                MAX_SIDEBAR_WIDTH,
            ),
            None,
            "a host-clipped drawer cannot replace the configured width on click"
        );
    }

    #[test]
    fn cheatsheet_obstructs_application_mouse_and_cursor_routing() {
        assert!(app_overlay_clear(true, true, true, false));
        assert!(!app_overlay_clear(true, true, true, true));
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
            input.route_ui(drag, host, None, panes[0].tab_id, &dividers),
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
