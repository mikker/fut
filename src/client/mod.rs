//! Interactive terminal client for a running Fut daemon.

mod input;

use std::{io, path::Path, time::Duration};

use anyhow::{Context, bail};
use bytes::Bytes;
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableMouseCapture, Event, EventStream},
    execute,
    terminal::{
        DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use futures_util::{SinkExt, StreamExt};
use input::{PrefixAction, PrefixState, encode_key};
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

use crate::{
    domain::{CellStyle, ScreenSnapshot, TerminalId, TerminalSize},
    protocol::{
        ClientKind, ClientMessage, Envelope, PROTOCOL_VERSION, ServerMessage, codec,
        decode_payload, encode_payload,
    },
    resources::SessionSelector,
};

/// Attach an interactive full-screen client to an already-running daemon.
pub async fn attach(socket_path: &Path, selector: Option<SessionSelector>) -> anyhow::Result<()> {
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
            kind: ClientKind::Interactive,
            size: TerminalSize { columns, rows },
            selector,
        },
    )
    .await?;

    let terminal_id = match receive(&mut framed).await? {
        ServerMessage::Welcome {
            version,
            selected: Some(selected),
            ..
        } if version == PROTOCOL_VERSION => selected.terminal_id,
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
    let result = run(&mut terminal, &mut framed, terminal_id).await;
    drop(terminal);
    drop(guard);
    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    terminal_id: TerminalId,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut prefix = PrefixState::default();
    let mut snapshots = SnapshotState::default();
    let mut redraw = time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            frame = framed.next() => {
                let Some(frame) = frame else { break };
                let envelope: Envelope<ServerMessage> = decode_payload(&frame?)?;
                match envelope.message {
                    ServerMessage::Snapshot { terminal_id: id, screen } if id == terminal_id => {
                        snapshots.accept(screen);
                    }
                    ServerMessage::Snapshot { .. } | ServerMessage::Pong { .. } | ServerMessage::CommandCompleted { .. } | ServerMessage::Resources { .. } | ServerMessage::SessionCreated { .. } => {}
                    ServerMessage::TerminalExited { terminal_id: id, exit_code } if id == terminal_id => {
                        if let Some(code) = exit_code { bail!("terminal exited with status {code}") }
                        break;
                    }
                    ServerMessage::TerminalExited { .. } | ServerMessage::Detached => break,
                    ServerMessage::Error { code, message } => bail!("daemon error ({code}): {message}"),
                    ServerMessage::IncompatibleProtocol { client, server } => {
                        bail!("protocol became incompatible: client {client}, server {server}")
                    }
                    ServerMessage::Welcome { .. } => bail!("unexpected second welcome from daemon"),
                }
            }
            event = events.next() => {
                let Some(event) = event else { break };
                match event? {
                    Event::Key(key) => if let Some(bytes) = encode_key(key) {
                        match prefix.feed(bytes) {
                            PrefixAction::Wait => {}
                            PrefixAction::Detach => {
                                send(framed, ClientMessage::Detach).await?;
                            }
                            PrefixAction::Send(bytes) => send(framed, ClientMessage::Input { bytes }).await?,
                        }
                    },
                    Event::Paste(text) => send(framed, ClientMessage::Input { bytes: text.into_bytes() }).await?,
                    Event::Resize(columns, rows) if columns > 0 && rows > 0 => {
                        send(framed, ClientMessage::Resize { size: TerminalSize { columns, rows } }).await?;
                    }
                    _ => {}
                }
            }
            _ = redraw.tick(), if snapshots.needs_draw() => {
                let screen = snapshots.pending.as_ref().expect("draw requires a pending snapshot");
                terminal.draw(|frame| {
                    let area = frame.area();
                    frame.render_widget(Screen(screen), area);
                    if screen.cursor.visible
                        && screen.cursor.column < area.width
                        && screen.cursor.row < area.height
                    {
                        frame.set_cursor_position((screen.cursor.column, screen.cursor.row));
                    }
                })?;
                snapshots.mark_drawn();
            }
        }
    }
    Ok(())
}

async fn send(
    framed: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    message: ClientMessage,
) -> anyhow::Result<()> {
    framed
        .send(Bytes::from(encode_payload(&Envelope {
            request_id: None,
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
struct SnapshotState {
    newest_revision: Option<u64>,
    drawn_revision: Option<u64>,
    pending: Option<ScreenSnapshot>,
}

impl SnapshotState {
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

    fn needs_draw(&self) -> bool {
        self.newest_revision != self.drawn_revision
    }

    fn mark_drawn(&mut self) {
        self.drawn_revision = self.newest_revision;
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
    mouse_capture: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut guard = Self {
            raw: false,
            alternate_screen: false,
            cursor_hidden: false,
            line_wrap_disabled: false,
            mouse_capture: false,
        };
        enable_raw_mode()?;
        guard.raw = true;
        execute!(io::stdout(), EnterAlternateScreen)?;
        guard.alternate_screen = true;
        execute!(io::stdout(), Hide)?;
        guard.cursor_hidden = true;
        execute!(io::stdout(), DisableLineWrap)?;
        guard.line_wrap_disabled = true;
        // Mouse input is not translated in this spike, so it remains disabled.
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.mouse_capture {
            let _ = execute!(stdout, DisableMouseCapture);
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
    use crate::domain::Rgb;

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
        assert!(state.accept(snapshot(2)));
        assert!(!state.accept(snapshot(1)));
        assert!(!state.accept(snapshot(2)));
        assert!(state.needs_draw());
        state.mark_drawn();
        assert!(!state.needs_draw());
        assert!(state.accept(snapshot(3)));
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
