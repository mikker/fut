//! Interactive terminal client for a running Fut daemon.

mod input;
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
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, ServerMessage, codec,
        decode_payload, encode_payload,
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
    mut selected: crate::protocol::SelectedTarget,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut prefix = PrefixState::default();
    let mut snapshots = SnapshotState::default();
    snapshots.select(selected.terminal_id);
    let mut navigator: Option<NavigatorState> = None;
    let mut create_tab = CreateTabState::default();
    let mut force_draw = false;
    let mut redraw = time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            frame = framed.next() => {
                let Some(frame) = frame else { break };
                let envelope: Envelope<ServerMessage> = decode_payload(&frame?)?;
                let request_id = envelope.request_id;
                match envelope.message {
                    ServerMessage::Snapshot { terminal_id: id, screen } if id == selected.terminal_id => {
                        if snapshots.accept(id, screen) && navigator.as_ref().is_some_and(|nav| nav.switch_request.is_none() && matches!(nav.status, navigator::NavigatorStatus::Switching)) {
                            navigator = None;
                        }
                    }
                    ServerMessage::Resources { snapshot } => {
                        if let Some(nav) = navigator.as_mut() {
                            force_draw |= nav.accept_resources(request_id, &snapshot, &selected);
                        }
                    }
                    ServerMessage::TargetSelected { selected: target } => {
                        if !navigator.as_mut().is_some_and(|nav| nav.switch_selected(request_id)) {
                            continue;
                        }
                        let old_terminal = selected.terminal_id;
                        selected = target;
                        if selected.terminal_id == old_terminal {
                            navigator = None;
                            snapshots.invalidate_drawn();
                        } else {
                            snapshots.commit_selection(selected.terminal_id);
                            if let Some(nav) = navigator.as_mut() {
                                nav.status = navigator::NavigatorStatus::Switching;
                            }
                        }
                        force_draw = true;
                    }
                    ServerMessage::TabCreated { selected: target } => {
                        if !create_tab.complete(request_id) {
                            continue;
                        }
                        selected = target;
                        snapshots.commit_selection(selected.terminal_id);
                        force_draw = true;
                    }
                    ServerMessage::Snapshot { .. } | ServerMessage::Pong { .. } | ServerMessage::CommandCompleted { .. } | ServerMessage::LocationOpened { .. } => {}
                    ServerMessage::TerminalExited { terminal_id: id, exit_code } if id == selected.terminal_id => {
                        if let Some(code) = exit_code { bail!("terminal exited with status {code}") }
                        break;
                    }
                    ServerMessage::TerminalExited { .. } => {}
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
                        } else { bail!("daemon error ({code}): {message}") }
                    }
                    ServerMessage::IncompatibleProtocol { client, server } => {
                        bail!("protocol became incompatible: client {client}, server {server}")
                    }
                    ServerMessage::Welcome { .. } => bail!("unexpected second welcome from daemon"),
                }
            }
            event = events.next() => {
                let Some(event) = event else { break };
                match event? {
                    Event::Key(key) if navigator.is_some() => {
                        let visible = terminal.size()?.height.saturating_sub(2) as usize;
                        match navigator.as_mut().expect("navigator exists").key(key, visible) {
                            NavigatorAction::Stay => force_draw = true,
                            NavigatorAction::Close => {
                                navigator = None;
                                snapshots.invalidate_drawn();
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
                        match prefix.feed(bytes) {
                            PrefixAction::Wait => {}
                            PrefixAction::Detach => {
                                send(framed, ClientMessage::Detach).await?;
                            }
                            PrefixAction::Navigator => {
                                let request = Uuid::new_v4();
                                let mut state = NavigatorState::open(&selected);
                                state.set_list_request(request);
                                navigator = Some(state);
                                send_request(framed, Some(request), ClientMessage::ListResources).await?;
                                force_draw = true;
                            }
                            PrefixAction::CreateTab => if let Some(request) = create_tab.begin() {
                                send_request(framed, Some(request), ClientMessage::CreateTab {
                                    workspace_id: selected.workspace_id,
                                    name: None,
                                    cwd: None,
                                    program: None,
                                    argv: Vec::new(),
                                }).await?;
                            },
                            PrefixAction::Send(bytes) => send(framed, ClientMessage::Input { bytes }).await?,
                        }
                    },
                    Event::Paste(text) if navigator.is_none() => send(framed, ClientMessage::Input { bytes: text.into_bytes() }).await?,
                    Event::Resize(columns, rows) if columns > 0 && rows > 0 => {
                        send(framed, ClientMessage::Resize { size: TerminalSize { columns, rows } }).await?;
                    }
                    _ => {}
                }
            }
            _ = redraw.tick(), if force_draw || snapshots.needs_draw() => {
                let screen = snapshots.pending.as_ref();
                terminal.draw(|frame| {
                    let area = frame.area();
                    if let Some(screen) = screen { frame.render_widget(Screen(screen), area); }
                    if let Some(nav) = navigator.as_mut() {
                        nav.render(area, frame.buffer_mut());
                    } else if let Some(screen) = screen && screen.cursor.visible
                        && screen.cursor.column < area.width
                        && screen.cursor.row < area.height
                    {
                        frame.set_cursor_position((screen.cursor.column, screen.cursor.row));
                    }
                })?;
                snapshots.mark_drawn();
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
struct SnapshotState {
    terminal_id: Option<TerminalId>,
    newest_revision: Option<u64>,
    drawn_revision: Option<u64>,
    pending: Option<ScreenSnapshot>,
}

impl SnapshotState {
    fn select(&mut self, terminal_id: TerminalId) {
        if self.terminal_id != Some(terminal_id) {
            self.commit_selection(terminal_id);
        }
    }

    fn commit_selection(&mut self, terminal_id: TerminalId) {
        self.terminal_id = Some(terminal_id);
        self.newest_revision = None;
        self.drawn_revision = None;
        self.pending = None;
    }

    fn accept(&mut self, terminal_id: TerminalId, screen: ScreenSnapshot) -> bool {
        if self.terminal_id != Some(terminal_id) {
            return false;
        }
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

    fn needs_draw(&self) -> bool {
        self.newest_revision != self.drawn_revision
    }

    fn mark_drawn(&mut self) {
        self.drawn_revision = self.newest_revision;
    }

    fn invalidate_drawn(&mut self) {
        self.drawn_revision = None;
    }
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
    use crate::domain::Rgb;

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
    fn stale_snapshots_are_rejected_and_newest_is_drawn_once() {
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
        let mut state = SnapshotState::default();
        let terminal = TerminalId::new();
        state.select(terminal);
        assert!(state.accept(terminal, snapshot(2)));
        assert!(!state.accept(terminal, snapshot(1)));
        assert!(!state.accept(terminal, snapshot(2)));
        assert!(state.needs_draw());
        state.mark_drawn();
        assert!(!state.needs_draw());
        assert!(state.accept(terminal, snapshot(3)));
        state.mark_drawn();
        state.select(terminal);
        assert_eq!(
            state.pending.as_ref().map(|screen| screen.revision),
            Some(3)
        );
        assert!(!state.needs_draw());
        state.invalidate_drawn();
        assert!(state.needs_draw());
        let other = TerminalId::new();
        state.select(other);
        assert!(state.pending.is_none());
        assert!(state.accept(other, snapshot(1)));
        assert!(!state.accept(terminal, snapshot(100)));
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
