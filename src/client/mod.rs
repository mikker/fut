//! Interactive terminal client for a running Fut daemon.

mod input;
mod layout;
mod navigator;

use std::{io, path::Path, time::Duration};

use anyhow::{Context, bail};
use bytes::Bytes;
use crossterm::{
    cursor::{Hide, Show},
    event::{Event, EventStream},
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

use crate::{
    domain::{CellStyle, ScreenSnapshot, TerminalId, TerminalSize},
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, SelectedTarget, SelectedView,
        ServerMessage, codec, decode_payload, encode_payload,
    },
    resources::TargetSelector,
};

/// Attach an interactive full-screen client to an already-running daemon.
pub async fn attach(socket_path: &Path, selector: Option<TargetSelector>) -> anyhow::Result<()> {
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
    let result = run(&mut terminal, &mut framed, selected).await;
    drop(terminal);
    drop(guard);
    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    selected: SelectedView,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut prefix = PrefixState::default();
    let mut view = ViewState::new(selected)?;
    let mut navigator: Option<NavigatorState> = None;
    let mut create_tab = CreateTabState::default();
    let mut focus = FocusState::default();
    let mut notice: Option<String> = None;
    let mut force_draw = false;
    let mut redraw = time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    resize_view(framed, terminal.size()?.into(), &mut view).await?;

    loop {
        tokio::select! {
            frame = framed.next() => {
                let Some(frame) = frame else { break };
                let envelope: Envelope<ServerMessage> = decode_payload(&frame?)?;
                let request_id = envelope.request_id;
                match envelope.message {
                    ServerMessage::Snapshot { terminal_id, screen } => {
                        if view.accept(terminal_id, screen)
                            && terminal_id == view.focused().terminal_id
                            && navigator.as_ref().is_some_and(|nav| nav.switch_request.is_none() && matches!(nav.status, navigator::NavigatorStatus::Switching))
                        {
                            navigator = None;
                        }
                    }
                    ServerMessage::Resources { snapshot } => {
                        if let Some(nav) = navigator.as_mut() {
                            force_draw |= nav.accept_resources(request_id, &snapshot, view.focused());
                        }
                    }
                    ServerMessage::TargetSelected { selected: target } => {
                        let navigator_selected = navigator.as_mut().is_some_and(|nav| nav.switch_selected(request_id));
                        focus.complete(request_id);
                        let old_terminal = view.focused().terminal_id;
                        view.replace(target)?;
                        resize_view(framed, terminal.size()?.into(), &mut view).await?;
                        if navigator_selected && view.focused().terminal_id == old_terminal {
                            navigator = None;
                            view.invalidate_drawn();
                        } else if navigator_selected
                            && let Some(nav) = navigator.as_mut()
                        {
                            nav.status = navigator::NavigatorStatus::Switching;
                        }
                        force_draw = true;
                    }
                    ServerMessage::TabCreated { selected: target } => {
                        if !create_tab.complete(request_id) {
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
                        view.remove(terminal_id);
                        if view.is_empty() {
                            if let Some(code) = exit_code { bail!("terminal exited with status {code}") }
                            break;
                        }
                        force_draw = true;
                    }
                    ServerMessage::Detached => break,
                    ServerMessage::Error { code, message } => {
                        if create_tab.complete(request_id) {
                            bail!("create tab failed: daemon error ({code}): {message}");
                        }
                        let handled = navigator.as_mut().is_some_and(|nav| {
                            nav.switch_error(request_id, message.clone())
                                || nav.list_error(request_id, message.clone())
                        });
                        if handled {
                            force_draw = true;
                        } else if focus.complete(request_id) {
                            notice = Some(format!("pane unavailable · {message}"));
                            force_draw = true;
                        } else { bail!("daemon error ({code}): {message}") }
                    }
                    ServerMessage::IncompatibleProtocol { client, server } => {
                        bail!("protocol became incompatible: client {client}, server {server}")
                    }
                    ServerMessage::Welcome { .. } => bail!("unexpected second welcome from daemon"),
                }
            }
            event = events.next(), if focus.request_id.is_none() => {
                let Some(event) = event else { break };
                match event? {
                    Event::Key(key) if navigator.is_some() => {
                        notice = None;
                        let visible = terminal.size()?.height.saturating_sub(2) as usize;
                        match navigator.as_mut().expect("navigator exists").key(key, visible) {
                            NavigatorAction::Stay => force_draw = true,
                            NavigatorAction::Close => {
                                navigator = None;
                                view.invalidate_drawn();
                                force_draw = true;
                            }
                            NavigatorAction::Select(selector) => {
                                let request = Uuid::new_v4();
                                navigator.as_mut().expect("navigator exists").begin_switch(request);
                                send_request(framed, Some(request), ClientMessage::SelectTarget { selector }).await?;
                                force_draw = true;
                            }
                        }
                    }
                    Event::Key(key) => if let Some(bytes) = encode_key(key) {
                        notice = None;
                        match prefix.feed(bytes) {
                            PrefixAction::Wait => {}
                            PrefixAction::Detach => {
                                send(framed, ClientMessage::Detach).await?;
                            }
                            PrefixAction::Navigator => {
                                let request = Uuid::new_v4();
                                let mut state = NavigatorState::open(view.focused());
                                state.set_list_request(request);
                                navigator = Some(state);
                                send_request(framed, Some(request), ClientMessage::ListResources).await?;
                                force_draw = true;
                            }
                            PrefixAction::CreateTab => if let Some(request) = create_tab.begin() {
                                send_request(framed, Some(request), ClientMessage::CreateTab {
                                    workspace_id: view.focused().workspace_id,
                                    name: None,
                                    cwd: None,
                                    program: None,
                                    argv: Vec::new(),
                                }).await?;
                            },
                            action @ (PrefixAction::FocusNext | PrefixAction::FocusPrevious) => {
                                let forward = matches!(action, PrefixAction::FocusNext);
                                if let Some(target) = view.cycle(forward)
                                    && let Some(request) = focus.begin()
                                {
                                    send_request(
                                        framed,
                                        Some(request),
                                        ClientMessage::SelectTarget {
                                            selector: TargetSelector::Pane(target.pane_id),
                                        },
                                    ).await?;
                                }
                            }
                            PrefixAction::Send(bytes) => send(framed, ClientMessage::Input { bytes }).await?,
                        }
                    },
                    Event::Paste(text) if navigator.is_none() => send(framed, ClientMessage::Input { bytes: text.into_bytes() }).await?,
                    Event::Resize(columns, rows) if columns > 0 && rows > 0 => {
                        resize_view(framed, Rect::new(0, 0, columns, rows), &mut view).await?;
                        force_draw = true;
                    }
                    _ => {}
                }
            }
            _ = redraw.tick(), if force_draw || view.needs_draw() => {
                terminal.draw(|frame| {
                    let area = frame.area();
                    let cursor = render_view(&view, area, frame.buffer_mut());
                    if let Some(nav) = navigator.as_mut() {
                        nav.render(area, frame.buffer_mut());
                    } else if notice.is_none()
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
struct CreateTabState {
    request_id: Option<Uuid>,
}

impl CreateTabState {
    fn begin(&mut self) -> Option<Uuid> {
        if self.request_id.is_some() {
            return None;
        }
        let request_id = Uuid::new_v4();
        self.request_id = Some(request_id);
        Some(request_id)
    }

    fn complete(&mut self, request_id: Option<Uuid>) -> bool {
        if request_id.is_none() || request_id != self.request_id {
            return false;
        }
        self.request_id = None;
        true
    }
}

#[derive(Default)]
struct FocusState {
    request_id: Option<Uuid>,
}

impl FocusState {
    fn begin(&mut self) -> Option<Uuid> {
        if self.request_id.is_some() {
            return None;
        }
        let request_id = Uuid::new_v4();
        self.request_id = Some(request_id);
        Some(request_id)
    }

    fn complete(&mut self, request_id: Option<Uuid>) -> bool {
        if request_id.is_none() || request_id != self.request_id {
            return false;
        }
        self.request_id = None;
        true
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
    panes: Vec<PaneState>,
}

impl ViewState {
    fn new(selected: SelectedView) -> anyhow::Result<Self> {
        let focused = selected.focused.terminal_id;
        let mut view = Self {
            focused,
            panes: Vec::new(),
        };
        view.replace(selected)?;
        Ok(view)
    }

    fn replace(&mut self, selected: SelectedView) -> anyhow::Result<()> {
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
        if previous_focus != focused
            && let Some(pane) = self
                .panes
                .iter_mut()
                .find(|pane| pane.target.terminal_id == focused)
        {
            pane.last_size = None;
        }
        Ok(())
    }

    fn focused(&self) -> &SelectedTarget {
        &self
            .panes
            .iter()
            .find(|pane| pane.target.terminal_id == self.focused)
            .expect("focused terminal belongs to the client view")
            .target
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

    fn is_empty(&self) -> bool {
        self.panes.is_empty()
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
) -> anyhow::Result<()> {
    for (terminal_id, size) in view.resize_requests(area) {
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
    cursor_hidden: bool,
    line_wrap_disabled: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut guard = Self {
            raw: false,
            alternate_screen: false,
            cursor_hidden: false,
            line_wrap_disabled: false,
        };
        enable_raw_mode()?;
        guard.raw = true;
        execute!(io::stdout(), EnterAlternateScreen)?;
        guard.alternate_screen = true;
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
    fn create_tab_state_allows_one_request_and_only_completes_its_correlation() {
        let mut state = CreateTabState::default();
        let request = state.begin().expect("first request starts");
        assert!(state.begin().is_none());
        assert!(!state.complete(None));
        assert!(!state.complete(Some(Uuid::new_v4())));
        assert!(state.begin().is_none());
        assert!(state.complete(Some(request)));
        assert!(state.begin().is_some());
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
    fn view_rejects_inconsistent_focus_and_duplicate_identity() {
        let panes = targets(2);
        let mut inconsistent = panes[0].clone();
        inconsistent.child_pid += 100;
        assert!(
            ViewState::new(SelectedView {
                focused: inconsistent,
                panes: panes.clone(),
            })
            .is_err()
        );

        let mut duplicate = panes[1].clone();
        duplicate.pane_id = panes[0].pane_id;
        assert!(
            ViewState::new(SelectedView {
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
            focused: panes[0].clone(),
            panes: panes.clone(),
        })
        .unwrap();
        let area = Rect::new(0, 0, 27, 4);
        assert_eq!(
            state.resize_requests(area),
            [(
                first,
                TerminalSize {
                    columns: 13,
                    rows: 4
                }
            )]
        );
        assert!(state.resize_requests(area).is_empty());

        state
            .replace(SelectedView {
                focused: panes[1].clone(),
                panes,
            })
            .unwrap();
        assert_eq!(
            state.resize_requests(area),
            [(
                second,
                TerminalSize {
                    columns: 12,
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
            focused: panes[0].clone(),
            panes: panes.clone(),
        })
        .unwrap();
        assert!(state.accept(first, snapshot("A", 13)));
        assert!(state.accept(second, snapshot("B", 12)));

        let area = Rect::new(0, 0, 27, 2);
        let mut buffer = Buffer::empty(area);
        assert_eq!(render_view(&state, area, &mut buffer), Some((1, 0)));
        assert_eq!(buffer[(0, 0)].symbol(), "┃");
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 0)].fg, Color::Reset);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert_eq!(buffer[(1, 0)].symbol(), "A");
        assert_eq!(buffer[(14, 0)].symbol(), "│");
        assert!(buffer[(14, 0)].modifier.contains(Modifier::DIM));
        assert_eq!(buffer[(15, 0)].symbol(), "B");

        state
            .replace(SelectedView {
                focused: panes[1].clone(),
                panes,
            })
            .unwrap();
        let mut moved = Buffer::empty(area);
        assert_eq!(render_view(&state, area, &mut moved), Some((15, 0)));
        assert!(moved[(0, 0)].modifier.contains(Modifier::DIM));
        assert!(moved[(14, 0)].modifier.contains(Modifier::BOLD));

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
