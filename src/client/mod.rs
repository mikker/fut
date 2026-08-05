//! Interactive terminal client for a running Fut daemon.

mod actions;
mod chrome;
mod command_bar;
pub(crate) mod config;
mod input;
mod layout;
mod navigation;
mod navigator;
mod presentation;
mod rename;
mod sidebar;
mod tab_bar;

use std::{io, path::Path, time::Duration};

use actions::{ClientAction, FocusDirection, NavigationScope};
use anyhow::{Context, bail};
use bytes::Bytes;
use chrome::{ResourceState, client_layout, render_tab_bar};
use command_bar::{CommandBarAction, CommandBarState};
use config::{PaneLayoutPolicy, UiConfig};
use crossterm::{
    SynchronizedUpdate,
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
use layout::{
    PaneLayout, authored_layout, authored_navigation_layout, directional_neighbor,
    navigation_pane_layouts, pane_layouts,
};
use navigation::NavigationHistory;
use navigator::{NavigatorAction, NavigatorState};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};
use rename::{RenameAction, RenameState};
use tokio::{net::UnixStream, time};
use tokio_util::codec::Framed;
use uuid::Uuid;

use sidebar::{WorkspaceSidebarAction, WorkspaceSidebarState, render_workspace_sidebar};
use tab_bar::{TabBarAction, TabBarState};

use crate::{
    domain::{CellColor, CellStyle, ScreenSnapshot, TerminalId, TerminalSize},
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, SelectedTarget, SelectedView,
        SelectionExpectation, ServerMessage, codec, decode_payload, encode_payload,
    },
    resources::TargetSelector,
    splits::SplitTree,
};

enum ClientSurface {
    Navigator(NavigatorState),
    WorkspaceSidebar(WorkspaceSidebarState),
    TabBar(TabBarState),
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
    let mut redraw = time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    send_request(framed, Some(Uuid::new_v4()), ClientMessage::ListResources).await?;
    resize_view(framed, terminal.size()?.into(), &mut view, &ui).await?;

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
                            let snapshot = resources.snapshot().expect("accepted resources exist");
                            refresh_surface_resources(
                                &mut surface,
                                snapshot,
                                view.focused(),
                                &workspace_history,
                            );
                            reconcile_resource_barriers(
                                snapshot,
                                &mut create_workspace,
                                &mut create_tab,
                                &mut split_pane,
                                &mut rename,
                            );
                            force_draw = true;
                        }
                    }
                    ServerMessage::ResourcesChanged { snapshot } => {
                        if resources.accept(snapshot) {
                            let snapshot = resources.snapshot().expect("accepted resources exist");
                            refresh_surface_resources(
                                &mut surface,
                                snapshot,
                                view.focused(),
                                &workspace_history,
                            );
                            reconcile_resource_barriers(
                                snapshot,
                                &mut create_workspace,
                                &mut create_tab,
                                &mut split_pane,
                                &mut rename,
                            );
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
                        workspace_history.record_transition(&previous_target, view.focused());
                        if !resources
                            .snapshot()
                            .is_some_and(|snapshot| view.resources_are_current(snapshot))
                        {
                            send(framed, ClientMessage::ListResources).await?;
                        }
                        pending_focused_exit = None;
                        resize_view(framed, terminal.size()?.into(), &mut view, &ui).await?;
                        if let Some(snapshot) = resources.snapshot() {
                            refresh_surface_resources(
                                &mut surface,
                                snapshot,
                                view.focused(),
                                &workspace_history,
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
                        if terminal_id == view.focused().terminal_id {
                            pending_focused_exit = Some(exit_code);
                            force_draw = true;
                            continue;
                        }
                        view.remove(terminal_id);
                        force_draw = true;
                    }
                    ServerMessage::Detached => break,
                    ServerMessage::Error { code, message } => {
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
            event = events.next(), if accepts_client_input(&focus, &create_workspace, &create_tab, &split_pane, &pending_focused_exit) => {
                let Some(event) = event else { break };
                match event? {
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
                            WorkspaceSidebarAction::Rename(workspace_id, name) => {
                                rename = Some(RenameState::open(
                                    crate::protocol::RenameSelector::Workspace(workspace_id),
                                    "workspace",
                                    name,
                                ));
                                force_draw = true;
                            }
                            WorkspaceSidebarAction::Select(pane_id) => {
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
                                    terminal.size()?.into(),
                                    &ui,
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
                                    &mut view,
                                    &resources,
                                    &mut surface,
                                    &workspace_history,
                                    &mut create_tab,
                                    &mut split_pane,
                                    &mut focus,
                                    terminal.size()?.into(),
                                    &ui,
                                ).await?;
                                force_draw = true;
                            }
                            PrefixAction::Send(bytes) => send(framed, ClientMessage::Input { bytes }).await?,
                        }
                    },
                    Event::Paste(text) if surface.is_none() => send(framed, ClientMessage::Input { bytes: text.into_bytes() }).await?,
                    Event::Resize(columns, rows) if columns > 0 && rows > 0 => {
                        resize_view(framed, Rect::new(0, 0, columns, rows), &mut view, &ui).await?;
                        force_draw = true;
                    }
                    _ => {}
                }
            }
            _ = redraw.tick(), if force_draw || view.needs_draw() => {
                io::stdout().sync_update(|_| {
                    terminal.draw(|frame| {
                        let area = frame.area();
                        let layout = client_layout(area, &ui);
                        let cursor = render_view(
                            &view,
                            layout.terminal,
                            ui.pane_layout,
                            frame.buffer_mut(),
                        );
                        if let Some(tab_bar) = layout.tab_bar {
                            if let Some(ClientSurface::TabBar(state)) = surface.as_ref() {
                                state.render(
                                    resources.snapshot(),
                                    view.focused(),
                                    view.is_zoomed(),
                                    &ui,
                                    tab_bar,
                                    frame.buffer_mut(),
                                );
                            } else {
                                render_tab_bar(
                                    resources.snapshot(),
                                    view.focused(),
                                    view.is_zoomed(),
                                    None,
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
                                ui.workspace_sidebar.position,
                                &ui,
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
                            Some(ClientSurface::WorkspaceSidebar(_))
                            | Some(ClientSurface::TabBar(_))
                            | None => {}
                        }
                        if let Some(rename) = rename.as_ref() {
                            rename.render(layout.terminal, frame.buffer_mut());
                        }
                        if surface.is_none()
                            && rename.is_none()
                            && notice.is_none()
                            && let Some((column, row)) = cursor
                        {
                            frame.set_cursor_position((column, row));
                        }
                        if let Some(message) = notice.as_deref() {
                            render_notice(area, frame.buffer_mut(), message);
                        }
                    })
                })??;
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
    workspace_history: &NavigationHistory,
) {
    match surface.as_mut() {
        Some(ClientSurface::Navigator(nav)) => {
            nav.accept_resources(snapshot, focused);
        }
        Some(ClientSurface::WorkspaceSidebar(sidebar)) => {
            sidebar.accept_resources(snapshot, focused, workspace_history);
        }
        Some(ClientSurface::TabBar(tab_bar)) => {
            tab_bar.accept_resources(snapshot, focused, workspace_history);
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
    host: Rect,
    ui: &UiConfig,
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
            if !view.resources_are_current(snapshot) {
                return Ok(Some("navigation is syncing".into()));
            }
            let Some(sidebar) =
                WorkspaceSidebarState::open(snapshot, view.focused(), workspace_history)
            else {
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
            if client_layout(host, ui).tab_bar.is_none() {
                return Ok(Some("tab bar is unavailable at this size".into()));
            }
            let Some(tab_bar) = TabBarState::open(snapshot, view.focused(), workspace_history)
            else {
                return Ok(Some("no tab available".into()));
            };
            *surface = Some(ClientSurface::TabBar(tab_bar));
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
            let terminal = client_layout(host, ui).terminal;
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
            resize_view(framed, host, view, ui).await?;
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
        Vec<Rect>,
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
    ui: &UiConfig,
) -> anyhow::Result<()> {
    let terminal = client_layout(area, ui).terminal;
    for (terminal_id, size) in view.resize_requests(terminal, ui.pane_layout) {
        send(framed, ClientMessage::Resize { terminal_id, size }).await?;
    }
    Ok(())
}

fn render_view(
    view: &ViewState,
    area: Rect,
    policy: PaneLayoutPolicy,
    buffer: &mut Buffer,
) -> Option<(u16, u16)> {
    let (layouts, dividers) = view.pane_layouts(area, policy);
    for divider in dividers {
        let symbol = if divider.width == 1 { "│" } else { "─" };
        for row in divider.y..divider.y + divider.height {
            for column in divider.x..divider.x + divider.width {
                if let Some(cell) = buffer.cell_mut((column, row)) {
                    cell.set_symbol(symbol)
                        .set_style(Style::default().add_modifier(Modifier::DIM));
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
        target = target.fg(color.into());
    }
    if let Some(color) = source.background {
        target = target.bg(color.into());
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

impl From<CellColor> for Color {
    fn from(color: CellColor) -> Self {
        match color {
            CellColor::Indexed(index) => Self::Indexed(index),
            CellColor::Rgb(color) => Self::Rgb(color.red, color.green, color.blue),
        }
    }
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
        let mut state = ViewState::new(selected_view(1, panes[0].clone(), panes.clone())).unwrap();
        assert!(state.accept(first, snapshot("A", 13)));
        assert!(state.accept(second, snapshot("B", 12)));

        let area = Rect::new(0, 0, 38, 2);
        let mut buffer = Buffer::empty(area);
        assert_eq!(
            render_view(&state, area, PaneLayoutPolicy::Accordion, &mut buffer),
            Some((1, 0))
        );
        assert_eq!(buffer[(0, 0)].symbol(), "┃");
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 0)].fg, Color::Reset);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert_eq!(buffer[(1, 0)].symbol(), "A");
        assert_eq!(buffer[(25, 0)].symbol(), "│");
        assert!(buffer[(25, 0)].modifier.contains(Modifier::DIM));
        assert_eq!(buffer[(26, 0)].symbol(), "B");

        state
            .replace(selected_view(1, panes[1].clone(), panes))
            .unwrap();
        let mut moved = Buffer::empty(area);
        assert_eq!(
            render_view(&state, area, PaneLayoutPolicy::Accordion, &mut moved),
            Some((14, 0))
        );
        assert!(moved[(0, 0)].modifier.contains(Modifier::DIM));
        assert!(moved[(13, 0)].modifier.contains(Modifier::BOLD));

        let tiny = Rect::new(0, 0, 20, 2);
        let mut tiny_buffer = Buffer::empty(tiny);
        assert_eq!(
            render_view(&state, tiny, PaneLayoutPolicy::Accordion, &mut tiny_buffer,),
            Some((0, 0))
        );
        assert_eq!(tiny_buffer[(0, 0)].symbol(), "B");
        assert!((0..tiny.width).all(|column| tiny_buffer[(column, 0)].symbol() != "┃"));
    }

    #[test]
    fn style_conversion_preserves_indexed_rgb_and_modifiers() {
        let converted = style(CellStyle {
            foreground: Some(CellColor::Indexed(1)),
            background: Some(CellColor::Rgb(Rgb {
                red: 4,
                green: 5,
                blue: 6,
            })),
            bold: true,
            italic: true,
            underline: true,
            inverse: true,
        });
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
    }
}
