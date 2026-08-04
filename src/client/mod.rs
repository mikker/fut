//! Interactive terminal client for a running Fut daemon.

mod actions;
mod chrome;
mod command_bar;
mod config;
mod input;
mod layout;
mod navigator;
mod sidebar;

use std::{io, path::Path, time::Duration};

use actions::ClientAction;
use anyhow::{Context, bail};
use bytes::Bytes;
use chrome::{ResourceState, client_layout, render_tab_bar};
use command_bar::{CommandBarAction, CommandBarState};
use config::UiConfig;
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream},
    execute,
    terminal::{
        DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use futures_util::{SinkExt, StreamExt};
use input::{PrefixAction, PrefixState, encode_key};
use layout::{PaneLayout, pane_layouts};
use navigator::{NavigatorAction, NavigatorState};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};
use tokio::{net::UnixStream, time};
use tokio_util::codec::Framed;
use uuid::Uuid;

use sidebar::{
    WorkspaceHistory, WorkspaceSidebarAction, WorkspaceSidebarState, render_workspace_sidebar,
};

use crate::{
    domain::{CellStyle, ScreenSnapshot, TerminalId, TerminalSize},
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, SelectedTarget, SelectedView,
        ServerMessage, codec, decode_payload, encode_payload,
    },
    resources::TargetSelector,
};

enum ClientSurface {
    Navigator(NavigatorState),
    WorkspaceSidebar(WorkspaceSidebarState),
    CommandBar(CommandBarState),
}

/// Attach an interactive full-screen client to an already-running daemon.
pub async fn attach(socket_path: &Path, selector: Option<TargetSelector>) -> anyhow::Result<()> {
    attach_with_ui(socket_path, selector, load_ui_config()?).await
}

pub(crate) fn load_ui_config() -> anyhow::Result<UiConfig> {
    config::load()
}

pub(crate) async fn attach_with_ui(
    socket_path: &Path,
    selector: Option<TargetSelector>,
    ui: UiConfig,
) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect to {}", socket_path.display()))?;
    let (columns, rows) = crossterm::terminal::size().context("read terminal size")?;
    let mut framed = Framed::new(stream, codec());

    send(
        &mut framed,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
            mode: ClientMode::Interactive {
                size: TerminalSize { columns, rows },
                selector,
            },
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

    // Host terminal state is changed only after a successful handshake.
    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = run(&mut terminal, &mut framed, selected, ui).await;
    drop(terminal);
    drop(guard);
    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    selected: SelectedView,
    ui: UiConfig,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut prefix = PrefixState::default();
    let mut view = ViewState::new(selected)?;
    let mut resources = ResourceState::default();
    let mut surface: Option<ClientSurface> = None;
    let mut workspace_history = WorkspaceHistory::default();
    workspace_history.record(view.focused());
    let mut create_tab = CreateTabState::default();
    let mut focus = FocusState::default();
    let mut notice: Option<String> = None;
    let mut pending_focused_exit: Option<Option<i32>> = None;
    let mut force_draw = false;
    let mut redraw = time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    send_request(framed, Some(Uuid::new_v4()), ClientMessage::ListResources).await?;
    resize_view(framed, terminal.size()?.into(), &mut view, ui).await?;

    loop {
        tokio::select! {
            frame = framed.next() => {
                let Some(frame) = frame else {
                    if let Some(Some(code)) = pending_focused_exit {
                        bail!("terminal exited with status {code}");
                    }
                    break;
                };
                let envelope: Envelope<ServerMessage> = decode_payload(&frame?)?;
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
                    ServerMessage::Resources { snapshot } => {
                        if resources.accept(snapshot) {
                            refresh_surface_resources(
                                &mut surface,
                                resources.snapshot().expect("accepted resources exist"),
                                view.focused(),
                                &workspace_history,
                            );
                            force_draw = true;
                        }
                    }
                    ServerMessage::ResourcesChanged { snapshot } => {
                        if resources.accept(snapshot) {
                            refresh_surface_resources(
                                &mut surface,
                                resources.snapshot().expect("accepted resources exist"),
                                view.focused(),
                                &workspace_history,
                            );
                            force_draw = true;
                        }
                    }
                    ServerMessage::TargetSelected { selected: target } => {
                        let old_terminal = view.focused().terminal_id;
                        let navigator_selected = match surface.as_mut() {
                            Some(ClientSurface::Navigator(nav)) => nav.switch_selected(request_id),
                            _ => false,
                        };
                        let workspace_selected =
                            matches!(focus.complete(request_id), Some(FocusOrigin::Workspace));
                        let create_selected = create_tab.selected(request_id, &target.focused);
                        if !view.replace(target)? {
                            if navigator_selected || workspace_selected {
                                surface = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            if create_selected {
                                force_draw = true;
                            }
                            continue;
                        }
                        workspace_history.record(view.focused());
                        pending_focused_exit = None;
                        resize_view(framed, terminal.size()?.into(), &mut view, ui).await?;
                        if let Some(ClientSurface::WorkspaceSidebar(sidebar)) = surface.as_mut()
                            && let Some(snapshot) = resources.snapshot()
                        {
                            sidebar.accept_resources(snapshot, view.focused(), &workspace_history);
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
                        if create_selected {
                            view.invalidate_drawn();
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
                    ServerMessage::PaneCreated { .. } => {
                        // Raw protocol clients may create panes. The TUI keeps pane
                        // creation control-only until split direction and CWD
                        // inheritance are designed together.
                        bail!("unexpected pane creation response")
                    }
                    ServerMessage::PaneMoved { .. } => {
                        // Pane movement is currently a control-plane operation.
                        bail!("unexpected pane movement response")
                    }
                    ServerMessage::Pong { .. } | ServerMessage::CommandCompleted { .. } | ServerMessage::LocationOpened { .. } => {}
                    ServerMessage::TerminalExited { terminal_id, exit_code } => {
                        if terminal_id == view.focused().terminal_id {
                            if view.len() > 1 {
                                pending_focused_exit = Some(exit_code);
                                force_draw = true;
                                continue;
                            }
                            if let Some(code) = exit_code { bail!("terminal exited with status {code}") }
                            break;
                        }
                        view.remove(terminal_id);
                        force_draw = true;
                    }
                    ServerMessage::Detached => break,
                    ServerMessage::Error { code, message } => {
                        if create_tab.fail(request_id) {
                            notice = Some(format!("create tab failed · {message}"));
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
                                        bail!("daemon error ({code}): {message}")
                                    }
                                }
                                Some(FocusOrigin::Pane) => {
                                    notice = Some(format!("pane unavailable · {message}"));
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
                }
            }
            event = events.next(), if accepts_client_input(&focus, &create_tab, &pending_focused_exit) => {
                let Some(event) = event else { break };
                match event? {
                    Event::Key(key) if matches!(surface.as_ref(), Some(ClientSurface::Navigator(_))) => {
                        notice = None;
                        let visible = terminal.size()?.height.saturating_sub(2) as usize;
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
                                let request = Uuid::new_v4();
                                match surface.as_mut().expect("navigator exists") {
                                    ClientSurface::Navigator(nav) => nav.begin_switch(request),
                                    _ => unreachable!("surface guard ensures navigator"),
                                }
                                send_request(framed, Some(request), ClientMessage::SelectTarget { selector }).await?;
                                force_draw = true;
                            }
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
                            WorkspaceSidebarAction::Select(terminal_id) => {
                                if let Some(request) = focus.begin(FocusOrigin::Workspace) {
                                    match surface.as_mut().expect("workspace sidebar exists") {
                                        ClientSurface::WorkspaceSidebar(sidebar) => sidebar.begin_switch(),
                                        _ => unreachable!("surface guard ensures workspace sidebar"),
                                    }
                                    send_request(
                                        framed,
                                        Some(request),
                                        ClientMessage::SelectTarget {
                                            selector: TargetSelector::Terminal(terminal_id),
                                        },
                                    )
                                    .await?;
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
                                surface = None;
                                view.invalidate_drawn();
                                notice = dispatch_client_action(
                                    action,
                                    framed,
                                    &view,
                                    &resources,
                                    &mut surface,
                                    &workspace_history,
                                    &mut create_tab,
                                    &mut focus,
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
                    Event::Key(key) if surface.is_none() => if let Some(bytes) = encode_key(key) {
                        notice = None;
                        match prefix.feed(bytes) {
                            PrefixAction::Wait => {}
                            PrefixAction::Dispatch(action) => {
                                notice = dispatch_client_action(
                                    action,
                                    framed,
                                    &view,
                                    &resources,
                                    &mut surface,
                                    &workspace_history,
                                    &mut create_tab,
                                    &mut focus,
                                ).await?;
                                force_draw = true;
                            }
                            PrefixAction::Send(bytes) => send(framed, ClientMessage::Input { bytes }).await?,
                        }
                    },
                    Event::Paste(text) if surface.is_none() => send(framed, ClientMessage::Input { bytes: text.into_bytes() }).await?,
                    Event::Resize(columns, rows) if columns > 0 && rows > 0 => {
                        resize_view(framed, Rect::new(0, 0, columns, rows), &mut view, ui).await?;
                        force_draw = true;
                    }
                    _ => {}
                }
            }
            _ = redraw.tick(), if force_draw || view.needs_draw() => {
                terminal.draw(|frame| {
                    let area = frame.area();
                    let layout = client_layout(area, ui);
                    let cursor = render_view(&view, layout.terminal, frame.buffer_mut());
                    if let Some(tab_bar) = layout.tab_bar {
                        render_tab_bar(
                            resources.snapshot(),
                            view.focused(),
                            tab_bar,
                            frame.buffer_mut(),
                        );
                    }
                    if let Some(ClientSurface::WorkspaceSidebar(sidebar)) = surface.as_ref() {
                        if let Some(sidebar_area) =
                            layout.workspace_sidebar.map(|sidebar| sidebar.area())
                        {
                            sidebar.render(
                                sidebar_area,
                                ui.workspace_sidebar_position,
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
                            sidebar_area,
                            ui.workspace_sidebar_position,
                            frame.buffer_mut(),
                        );
                    }
                    match surface.as_mut() {
                        Some(ClientSurface::Navigator(nav)) => {
                            nav.render(area, frame.buffer_mut());
                        }
                        Some(ClientSurface::CommandBar(command_bar)) => {
                            command_bar.render(layout.terminal, frame.buffer_mut());
                        }
                        Some(ClientSurface::WorkspaceSidebar(_)) | None => {}
                    }
                    if surface.is_none()
                        && notice.is_none()
                        && let Some((column, row)) = cursor
                    {
                        frame.set_cursor_position((column, row));
                    }
                    if let Some(message) = notice.as_deref() {
                        render_notice(area, frame.buffer_mut(), message);
                    }
                })?;
                view.mark_drawn();
                force_draw = false;
            }
        }
    }
    Ok(())
}

fn refresh_surface_resources(
    surface: &mut Option<ClientSurface>,
    snapshot: &crate::resources::ResourceSnapshot,
    focused: &SelectedTarget,
    workspace_history: &WorkspaceHistory,
) {
    match surface.as_mut() {
        Some(ClientSurface::Navigator(nav)) => {
            nav.accept_resources(snapshot, focused);
        }
        Some(ClientSurface::WorkspaceSidebar(sidebar)) => {
            sidebar.accept_resources(snapshot, focused, workspace_history);
        }
        Some(ClientSurface::CommandBar(_)) | None => {}
    }
}

fn accepts_client_input(
    focus: &FocusState,
    create_tab: &CreateTabState,
    pending_focused_exit: &Option<Option<i32>>,
) -> bool {
    focus.request_id.is_none() && !create_tab.blocks_input() && pending_focused_exit.is_none()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the dispatcher explicitly borrows the small client states it coordinates"
)]
async fn dispatch_client_action(
    action: ClientAction,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    view: &ViewState,
    resources: &ResourceState,
    surface: &mut Option<ClientSurface>,
    workspace_history: &WorkspaceHistory,
    create_tab: &mut CreateTabState,
    focus: &mut FocusState,
) -> anyhow::Result<Option<String>> {
    match action {
        ClientAction::OpenCommandBar => {
            *surface = Some(ClientSurface::CommandBar(CommandBarState::open()));
        }
        ClientAction::OpenNavigator => {
            let mut navigator = NavigatorState::open(view.focused());
            if let Some(snapshot) = resources.snapshot() {
                navigator.accept_resources(snapshot, view.focused());
            }
            *surface = Some(ClientSurface::Navigator(navigator));
        }
        ClientAction::OpenWorkspaceSidebar => {
            let Some(snapshot) = resources.snapshot() else {
                return Ok(Some("workspaces are still loading".into()));
            };
            let Some(sidebar) =
                WorkspaceSidebarState::open(snapshot, view.focused(), workspace_history)
            else {
                return Ok(Some("no workspace available".into()));
            };
            *surface = Some(ClientSurface::WorkspaceSidebar(sidebar));
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
                    },
                )
                .await?;
            }
        }
        ClientAction::Detach => send(framed, ClientMessage::Detach).await?,
    }
    Ok(None)
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
enum CreateTabState {
    #[default]
    Idle,
    AwaitingCreated {
        request_id: Uuid,
    },
    AwaitingSelected {
        request_id: Uuid,
        terminal_id: TerminalId,
    },
}

impl CreateTabState {
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

    fn selected(&mut self, request_id: Option<Uuid>, selected: &SelectedTarget) -> bool {
        let Self::AwaitingSelected {
            request_id: expected,
            terminal_id,
        } = self
        else {
            return false;
        };
        if request_id != Some(*expected) || *terminal_id != selected.terminal_id {
            return false;
        }
        *self = Self::Idle;
        true
    }

    fn fail(&mut self, request_id: Option<Uuid>) -> bool {
        let expected = match self {
            Self::Idle => return false,
            Self::AwaitingCreated { request_id } | Self::AwaitingSelected { request_id, .. } => {
                *request_id
            }
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
    Workspace,
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
    pending: Option<ScreenSnapshot>,
    last_size: Option<TerminalSize>,
}

impl PaneState {
    fn new(target: SelectedTarget) -> Self {
        Self {
            target,
            newest_revision: None,
            drawn_revision: None,
            pending: None,
            last_size: None,
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
        true
    }
}

struct ViewState {
    focused: TerminalId,
    selected_revision: u64,
    panes: Vec<PaneState>,
}

impl ViewState {
    fn new(selected: SelectedView) -> anyhow::Result<Self> {
        let focused = selected.focused.terminal_id;
        let mut view = Self {
            focused,
            selected_revision: 0,
            panes: Vec::new(),
        };
        view.replace(selected)?;
        Ok(view)
    }

    fn replace(&mut self, selected: SelectedView) -> anyhow::Result<bool> {
        if selected.resource_revision < self.selected_revision {
            return Ok(false);
        }
        let previous_focus = self.focused;
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
        for (index, pane) in selected.panes.iter().enumerate() {
            if selected.panes[..index].iter().any(|previous| {
                previous.terminal_id == pane.terminal_id || previous.pane_id == pane.pane_id
            }) {
                bail!("daemon pane view contains a duplicate pane or terminal");
            }
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
        self.selected_revision = selected.resource_revision;
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

    fn len(&self) -> usize {
        self.panes.len()
    }

    fn accept(&mut self, terminal_id: TerminalId, screen: ScreenSnapshot) -> bool {
        self.panes
            .iter_mut()
            .find(|pane| pane.target.terminal_id == terminal_id)
            .is_some_and(|pane| pane.accept(screen))
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

    fn remove(&mut self, terminal_id: TerminalId) {
        let Some(index) = self
            .panes
            .iter()
            .position(|pane| pane.target.terminal_id == terminal_id)
        else {
            return;
        };
        self.panes.remove(index);
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

    fn resize_requests(&mut self, area: Rect) -> Vec<(TerminalId, TerminalSize)> {
        let layouts = pane_layouts(area, &self.terminal_ids(), self.focused);
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
    ui: UiConfig,
) -> anyhow::Result<()> {
    let terminal = client_layout(area, ui).terminal;
    for (terminal_id, size) in view.resize_requests(terminal) {
        send(framed, ClientMessage::Resize { terminal_id, size }).await?;
    }
    Ok(())
}

fn render_view(view: &ViewState, area: Rect, buffer: &mut Buffer) -> Option<(u16, u16)> {
    let layouts = pane_layouts(area, &view.terminal_ids(), view.focused);
    let mut cursor = None;
    for pane in &view.panes {
        let Some(PaneLayout { rail, content }) = layouts.get(&pane.target.terminal_id) else {
            continue;
        };
        if let Some(rail) = rail {
            let focused = pane.target.terminal_id == view.focused;
            let style = if focused {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::DIM)
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
        if pane.target.terminal_id == view.focused
            && screen.cursor.visible
            && screen.cursor.column < content.width
            && screen.cursor.row < content.height
        {
            cursor = Some((
                content.x + screen.cursor.column,
                content.y + screen.cursor.row,
            ));
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

struct Screen<'a>(&'a ScreenSnapshot);

impl Widget for Screen<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let screen = self.0;
        for row in 0..screen.size.rows.min(area.height) {
            for column in 0..screen.size.columns.min(area.width) {
                let index =
                    usize::from(row) * usize::from(screen.size.columns) + usize::from(column);
                let Some(cell) = screen.cells.get(index) else {
                    continue;
                };
                if let Some(target) = buffer.cell_mut((area.x + column, area.y + row)) {
                    target
                        .set_symbol(&cell.contents)
                        .set_style(style(cell.style));
                }
            }
        }
    }
}

fn style(source: CellStyle) -> Style {
    let mut target = Style::default();
    if let Some(color) = source.foreground {
        target = target.fg(Color::Rgb(color.red, color.green, color.blue));
    }
    if let Some(color) = source.background {
        target = target.bg(Color::Rgb(color.red, color.green, color.blue));
    }
    for (enabled, modifier) in [
        (source.bold, Modifier::BOLD),
        (source.italic, Modifier::ITALIC),
        (source.underline, Modifier::UNDERLINED),
        (source.inverse, Modifier::REVERSED),
    ] {
        if enabled {
            target = target.add_modifier(modifier);
        }
    }
    target
}

struct TerminalGuard {
    raw: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
    cursor_hidden: bool,
    line_wrap_disabled: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut guard = Self {
            raw: false,
            alternate_screen: false,
            bracketed_paste: false,
            cursor_hidden: false,
            line_wrap_disabled: false,
        };
        enable_raw_mode()?;
        guard.raw = true;
        execute!(io::stdout(), EnterAlternateScreen)?;
        guard.alternate_screen = true;
        execute!(io::stdout(), EnableBracketedPaste)?;
        guard.bracketed_paste = true;
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
        if self.bracketed_paste {
            let _ = execute!(stdout, DisableBracketedPaste);
        }
        if self.line_wrap_disabled {
            let _ = execute!(stdout, EnableLineWrap);
        }
        if self.cursor_hidden {
            let _ = execute!(stdout, Show);
        }
        if self.alternate_screen {
            let _ = execute!(stdout, LeaveAlternateScreen);
        }
        if self.raw {
            let _ = disable_raw_mode();
        }
    }
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

    #[test]
    fn create_tab_state_blocks_input_through_correlated_ack_and_selection() {
        let mut state = CreateTabState::default();
        let focus = FocusState::default();
        let no_exit = None;
        let request = state.begin().expect("first request starts");
        assert!(state.begin().is_none());
        assert!(state.blocks_input());
        assert!(!accepts_client_input(&focus, &state, &no_exit));
        let target = targets(1).remove(0);
        assert!(!state.created(None, target.terminal_id));
        assert!(!state.created(Some(Uuid::new_v4()), target.terminal_id));
        assert!(state.created(Some(request), target.terminal_id));
        assert!(state.blocks_input());
        assert!(!accepts_client_input(&focus, &state, &no_exit));
        assert!(!state.selected(Some(Uuid::new_v4()), &target));
        assert!(state.begin().is_none());
        assert!(state.selected(Some(request), &target));
        assert!(!state.blocks_input());
        assert!(accepts_client_input(&focus, &state, &no_exit));
        assert!(state.begin().is_some());

        let mut failed = CreateTabState::default();
        let request = failed.begin().unwrap();
        assert!(!failed.fail(Some(Uuid::new_v4())));
        assert!(failed.fail(Some(request)));
        assert!(!failed.blocks_input());
        assert!(accepts_client_input(&focus, &failed, &no_exit));
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
                },
            )
            .unwrap()
        };
        let panes = targets(2);
        let first = panes[0].terminal_id;
        let second = panes[1].terminal_id;
        let mut state = ViewState::new(SelectedView {
            resource_revision: 1,
            focused: panes[0].clone(),
            panes: panes.clone(),
        })
        .unwrap();
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
            .replace(SelectedView {
                resource_revision: 1,
                focused: panes[1].clone(),
                panes,
            })
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
    fn selected_view_revisions_only_reject_older_selected_views() {
        let panes = targets(3);
        let mut state = ViewState::new(SelectedView {
            resource_revision: 2,
            focused: panes[0].clone(),
            panes: panes[..2].to_vec(),
        })
        .unwrap();

        assert!(
            state
                .replace(SelectedView {
                    resource_revision: 3,
                    focused: panes[1].clone(),
                    panes: panes[1..].to_vec(),
                })
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
                .replace(SelectedView {
                    resource_revision: 2,
                    focused: panes[0].clone(),
                    panes: panes[..2].to_vec(),
                })
                .unwrap()
        );
        assert_eq!(state.focused().terminal_id, panes[1].terminal_id);
    }

    #[test]
    fn view_rejects_inconsistent_focus_and_duplicate_identity() {
        let panes = targets(2);
        let mut inconsistent = panes[0].clone();
        inconsistent.child_pid += 100;
        assert!(
            ViewState::new(SelectedView {
                resource_revision: 1,
                focused: inconsistent,
                panes: panes.clone(),
            })
            .is_err()
        );

        let mut duplicate = panes[1].clone();
        duplicate.pane_id = panes[0].pane_id;
        assert!(
            ViewState::new(SelectedView {
                resource_revision: 1,
                focused: panes[0].clone(),
                panes: vec![panes[0].clone(), duplicate],
            })
            .is_err()
        );
    }

    #[test]
    fn view_resize_targets_only_focus_and_tracks_each_pane_independently() {
        let panes = targets(2);
        let first = panes[0].terminal_id;
        let second = panes[1].terminal_id;
        let mut state = ViewState::new(SelectedView {
            resource_revision: 1,
            focused: panes[0].clone(),
            panes: panes.clone(),
        })
        .unwrap();
        let area = Rect::new(0, 0, 38, 4);
        assert_eq!(
            state.resize_requests(area),
            [(
                first,
                TerminalSize {
                    columns: 24,
                    rows: 4
                }
            )]
        );
        assert!(state.resize_requests(area).is_empty());
        assert_eq!(
            state.resize_requests(Rect::new(0, 0, 37, 4)),
            [(
                first,
                TerminalSize {
                    columns: 37,
                    rows: 4
                }
            )]
        );

        state
            .replace(SelectedView {
                resource_revision: 1,
                focused: panes[1].clone(),
                panes,
            })
            .unwrap();
        assert_eq!(
            state.resize_requests(area),
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
                    };
                    usize::from(columns) * 2
                ],
                Cursor {
                    column: 0,
                    row: 0,
                    visible: true,
                },
            )
            .unwrap()
        };
        let mut state = ViewState::new(SelectedView {
            resource_revision: 1,
            focused: panes[0].clone(),
            panes: panes.clone(),
        })
        .unwrap();
        assert!(state.accept(first, snapshot("A", 13)));
        assert!(state.accept(second, snapshot("B", 12)));

        let area = Rect::new(0, 0, 38, 2);
        let mut buffer = Buffer::empty(area);
        assert_eq!(render_view(&state, area, &mut buffer), Some((1, 0)));
        assert_eq!(buffer[(0, 0)].symbol(), "┃");
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 0)].fg, Color::Reset);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert_eq!(buffer[(1, 0)].symbol(), "A");
        assert_eq!(buffer[(25, 0)].symbol(), "│");
        assert!(buffer[(25, 0)].modifier.contains(Modifier::DIM));
        assert_eq!(buffer[(26, 0)].symbol(), "B");

        state
            .replace(SelectedView {
                resource_revision: 1,
                focused: panes[1].clone(),
                panes,
            })
            .unwrap();
        let mut moved = Buffer::empty(area);
        assert_eq!(render_view(&state, area, &mut moved), Some((14, 0)));
        assert!(moved[(0, 0)].modifier.contains(Modifier::DIM));
        assert!(moved[(13, 0)].modifier.contains(Modifier::BOLD));

        let tiny = Rect::new(0, 0, 20, 2);
        let mut tiny_buffer = Buffer::empty(tiny);
        assert_eq!(render_view(&state, tiny, &mut tiny_buffer), Some((0, 0)));
        assert_eq!(tiny_buffer[(0, 0)].symbol(), "B");
        assert!((0..tiny.width).all(|column| tiny_buffer[(column, 0)].symbol() != "┃"));
    }

    #[test]
    fn style_conversion_preserves_rgb_and_modifiers() {
        let converted = style(CellStyle {
            foreground: Some(Rgb {
                red: 1,
                green: 2,
                blue: 3,
            }),
            background: Some(Rgb {
                red: 4,
                green: 5,
                blue: 6,
            }),
            bold: true,
            italic: true,
            underline: true,
            inverse: true,
        });
        assert_eq!(converted.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(converted.bg, Some(Color::Rgb(4, 5, 6)));
        assert!(converted.add_modifier.contains(
            Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::REVERSED
        ));
    }
}
