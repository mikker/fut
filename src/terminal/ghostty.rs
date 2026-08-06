use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use libghostty_vt::{
    RenderState, Terminal, TerminalOptions, key, mouse, paste,
    render::{CellIterator, RowIterator},
    screen::CellContentTag,
    style::{StyleColor, Underline},
    terminal::{Mode, ScrollViewport},
};

use crate::domain::{
    Cell, CellColor, CellStyle, Cursor, MouseModifiers, MouseWheelDirection, MouseWheelEvent, Rgb,
    ScreenSnapshot, TerminalSize,
};

const SYNCHRONIZED_OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);
const MOUSE_WHEEL_LINES: isize = 3;

pub(crate) enum MouseWheelOutcome {
    Forwarded,
    Scrolled(ViewportSnapshot),
}

pub(crate) struct ViewportSnapshot {
    pub(crate) offset: Option<usize>,
    pub(crate) screen: ScreenSnapshot,
}

/// The complete libghostty boundary. This value must never leave its runtime thread.
pub(super) struct GhosttyTerminal {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    mouse_encoder: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    size: TerminalSize,
    revision: u64,
    synchronized_output_started: Option<Instant>,
}

impl GhosttyTerminal {
    pub(super) fn new(
        size: TerminalSize,
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
    ) -> Result<Self> {
        validate_size(size)?;
        let mut terminal = Terminal::new(TerminalOptions {
            cols: size.columns,
            rows: size.rows,
            max_scrollback: 10_000,
        })?;
        let callback_writer = Arc::clone(&writer);
        terminal.on_pty_write(move |_, bytes| {
            if let Ok(mut writer) = callback_writer.lock() {
                let _ = writer.write_all(bytes);
                let _ = writer.flush();
            }
        })?;

        Ok(Self {
            terminal,
            render_state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            writer,
            size,
            revision: 0,
            synchronized_output_started: None,
        })
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<Option<ScreenSnapshot>> {
        self.terminal.vt_write(bytes);
        self.present_synchronized_output()
    }

    pub(super) fn flush_synchronized_output(&mut self) -> Result<Option<ScreenSnapshot>> {
        self.present_synchronized_output()
    }

    pub(super) fn finish_synchronized_output(&mut self) -> Result<Option<ScreenSnapshot>> {
        if self.synchronized_output_started.is_none() && !self.terminal.mode(Mode::SYNC_OUTPUT)? {
            return Ok(None);
        }
        self.terminal.set_mode(Mode::SYNC_OUTPUT, false)?;
        self.synchronized_output_started = None;
        self.snapshot().map(Some)
    }

    pub(super) fn resize(&mut self, size: TerminalSize) -> Result<ScreenSnapshot> {
        validate_size(size)?;
        if self.terminal.mode(Mode::SYNC_OUTPUT)? {
            self.terminal.set_mode(Mode::SYNC_OUTPUT, false)?;
        }
        self.terminal
            .resize(size.columns, size.rows, 0, 0)
            .context("resizing Ghostty terminal")?;
        self.size = size;
        self.synchronized_output_started = None;
        self.snapshot()
    }

    /// Render one client-owned historical viewport, restoring the shared
    /// terminal to its canonical bottom before returning.
    pub(super) fn viewport_snapshot(&mut self, offset: Option<usize>) -> Result<ViewportSnapshot> {
        self.terminal.scroll_viewport(match offset {
            Some(offset) => ScrollViewport::Row(offset),
            None => ScrollViewport::Bottom,
        });
        self.snapshot_viewport_and_restore_bottom()
    }

    pub(super) fn mouse_wheel(
        &mut self,
        event: MouseWheelEvent,
        offset: Option<usize>,
        pty_input_allowed: bool,
    ) -> Result<MouseWheelOutcome> {
        if self.terminal.is_mouse_tracking()? {
            if pty_input_allowed {
                self.forward_mouse_wheel(event)?;
            }
            return Ok(MouseWheelOutcome::Forwarded);
        }

        self.terminal.scroll_viewport(match offset {
            Some(offset) => ScrollViewport::Row(offset),
            None => ScrollViewport::Bottom,
        });
        let delta = match event.direction {
            MouseWheelDirection::Up => -MOUSE_WHEEL_LINES,
            MouseWheelDirection::Down => MOUSE_WHEEL_LINES,
        };
        self.terminal.scroll_viewport(ScrollViewport::Delta(delta));
        self.snapshot_viewport_and_restore_bottom()
            .map(MouseWheelOutcome::Scrolled)
    }

    pub(super) fn paste(&mut self, text: String) -> Result<()> {
        let mut data = text.into_bytes();
        let capacity = data
            .len()
            .checked_add(12)
            .context("sizing encoded terminal paste")?;
        let mut response = vec![0; capacity];
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE)?;
        let written = paste::encode(&mut data, bracketed, &mut response)
            .context("encoding terminal paste with Ghostty")?;

        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY writer lock poisoned"))?;
        writer
            .write_all(&response[..written])
            .context("writing encoded paste to PTY")?;
        writer.flush().context("flushing encoded paste to PTY")
    }

    fn snapshot_viewport_and_restore_bottom(&mut self) -> Result<ViewportSnapshot> {
        let result = (|| {
            let scrollbar = self.terminal.scrollbar()?;
            let bottom = scrollbar.total.saturating_sub(scrollbar.len);
            let offset = (scrollbar.offset < bottom)
                .then(|| usize::try_from(scrollbar.offset))
                .transpose()
                .context("converting Ghostty viewport offset")?;
            Ok(ViewportSnapshot {
                offset,
                screen: self.snapshot()?,
            })
        })();
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        result
    }

    fn forward_mouse_wheel(&mut self, event: MouseWheelEvent) -> Result<()> {
        let column = event.column.min(self.size.columns - 1);
        let row = event.row.min(self.size.rows - 1);
        self.mouse_event
            .set_mods(mouse_modifiers(event.modifiers))
            .set_position(mouse::Position {
                x: f32::from(column) + 0.5,
                y: f32::from(row) + 0.5,
            })
            .set_button(Some(match event.direction {
                MouseWheelDirection::Up => mouse::Button::Four,
                MouseWheelDirection::Down => mouse::Button::Five,
            }));
        self.mouse_encoder
            .set_options_from_terminal(&self.terminal)
            .set_size(mouse::EncoderSize {
                screen_width: u32::from(self.size.columns),
                screen_height: u32::from(self.size.rows),
                cell_width: 1,
                cell_height: 1,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            })
            .set_any_button_pressed(false);

        let mut response = Vec::new();
        self.mouse_event.set_action(mouse::Action::Press);
        self.mouse_encoder
            .encode_to_vec(&self.mouse_event, &mut response)?;
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY writer lock poisoned"))?;
        writer
            .write_all(&response)
            .context("writing encoded mouse wheel to PTY")?;
        writer
            .flush()
            .context("flushing encoded mouse wheel to PTY")
    }

    fn present_synchronized_output(&mut self) -> Result<Option<ScreenSnapshot>> {
        if self.terminal.mode(Mode::SYNC_OUTPUT)? {
            let started = self
                .synchronized_output_started
                .get_or_insert_with(Instant::now);
            if started.elapsed() < SYNCHRONIZED_OUTPUT_TIMEOUT {
                return Ok(None);
            }
            self.terminal.set_mode(Mode::SYNC_OUTPUT, false)?;
        }
        self.synchronized_output_started = None;
        self.snapshot().map(Some)
    }

    pub(super) fn snapshot(&mut self) -> Result<ScreenSnapshot> {
        let snapshot = self.render_state.update(&self.terminal)?;
        let visible = snapshot.cursor_visible()?;
        let position = snapshot.cursor_viewport()?;
        let cursor = Cursor {
            column: position.map_or(0, |cursor| cursor.x.min(self.size.columns - 1)),
            row: position.map_or(0, |cursor| cursor.y.min(self.size.rows - 1)),
            visible: visible && position.is_some(),
        };

        let expected = usize::from(self.size.columns) * usize::from(self.size.rows);
        let mut result = Vec::with_capacity(expected);
        let mut rows = self.rows.update(&snapshot)?;
        while let Some(row) = rows.next() {
            let mut cells = self.cells.update(row)?;
            while let Some(cell) = cells.next() {
                let mut contents = String::new();
                cell.graphemes_utf8(&mut contents)?;
                if contents.is_empty() {
                    contents.push(' ');
                }
                let ghostty_style = cell.style()?;
                let raw_cell = cell.raw_cell()?;
                let background = match raw_cell.content_tag()? {
                    CellContentTag::BgColorPalette => {
                        Some(CellColor::Indexed(raw_cell.bg_color_palette()?.0))
                    }
                    CellContentTag::BgColorRgb => {
                        Some(CellColor::Rgb(rgb(raw_cell.bg_color_rgb()?)))
                    }
                    _ => color(ghostty_style.bg_color),
                };
                result.push(Cell {
                    contents,
                    style: CellStyle {
                        foreground: color(ghostty_style.fg_color),
                        background,
                        bold: ghostty_style.bold,
                        italic: ghostty_style.italic,
                        underline: ghostty_style.underline != Underline::None,
                        inverse: ghostty_style.inverse,
                    },
                });
            }
        }
        result.resize(expected, Cell::default());
        result.truncate(expected);
        self.revision = self.revision.saturating_add(1);
        Ok(ScreenSnapshot::new(
            self.revision,
            self.size,
            result,
            cursor,
        )?)
    }
}

fn mouse_modifiers(modifiers: MouseModifiers) -> key::Mods {
    let mut result = key::Mods::empty();
    result.set(key::Mods::SHIFT, modifiers.shift);
    result.set(key::Mods::CTRL, modifiers.control);
    result.set(key::Mods::ALT, modifiers.alt);
    result
}

fn validate_size(size: TerminalSize) -> Result<()> {
    ensure!(
        size.columns > 0 && size.rows > 0,
        "terminal dimensions must be non-zero"
    );
    Ok(())
}

fn color(color: StyleColor) -> Option<CellColor> {
    match color {
        StyleColor::None => None,
        StyleColor::Palette(index) => Some(CellColor::Indexed(index.0)),
        StyleColor::Rgb(color) => Some(CellColor::Rgb(rgb(color))),
    }
}

fn rgb(color: libghostty_vt::style::RgbColor) -> Rgb {
    Rgb {
        red: color.r,
        green: color.g,
        blue: color.b,
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[derive(Clone)]
    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn terminal(columns: u16, rows: u16) -> GhosttyTerminal {
        GhosttyTerminal::new(
            TerminalSize { columns, rows },
            Arc::new(Mutex::new(Box::new(io::sink()))),
        )
        .unwrap()
    }

    fn recording_terminal(columns: u16, rows: u16) -> (GhosttyTerminal, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let terminal = GhosttyTerminal::new(
            TerminalSize { columns, rows },
            Arc::new(Mutex::new(Box::new(RecordingWriter(Arc::clone(&output))))),
        )
        .unwrap();
        (terminal, output)
    }

    fn text(snapshot: &ScreenSnapshot) -> String {
        snapshot
            .cells
            .iter()
            .map(|cell| cell.contents.as_str())
            .collect()
    }

    #[test]
    fn renders_plain_text_and_blanks() {
        let snapshot = terminal(5, 2).feed(b"hi").unwrap().unwrap();
        assert_eq!(&text(&snapshot)[..5], "hi   ");
        assert_eq!(snapshot.cells.len(), 10);
        assert!(snapshot.revision > 0);
    }

    #[test]
    fn split_utf8_and_escape_input_matches_one_write() {
        let bytes = "aé\x1b[2;2HZ".as_bytes();
        let mut whole = terminal(6, 2);
        let expected = whole.feed(bytes).unwrap().unwrap();
        let mut split = terminal(6, 2);
        for chunk in bytes.chunks(1) {
            split.feed(chunk).unwrap().unwrap();
        }
        let actual = split.snapshot().unwrap();
        assert_eq!(actual.cells, expected.cells);
        assert_eq!(actual.cursor, expected.cursor);
    }

    #[test]
    fn preserves_indexed_and_truecolor_styles() {
        let snapshot = terminal(4, 1)
            .feed(b"\x1b[1;3;4;31;48;5;120mX\x1b[38;2;1;2;3;48;2;4;5;6mY")
            .unwrap();
        let snapshot = snapshot.unwrap();
        let indexed = snapshot.cells[0].style;
        assert_eq!(indexed.foreground, Some(CellColor::Indexed(1)));
        assert_eq!(indexed.background, Some(CellColor::Indexed(120)));
        assert!(indexed.bold && indexed.italic && indexed.underline);

        let truecolor = snapshot.cells[1].style;
        assert_eq!(
            truecolor.foreground,
            Some(CellColor::Rgb(Rgb {
                red: 1,
                green: 2,
                blue: 3
            }))
        );
        assert_eq!(
            truecolor.background,
            Some(CellColor::Rgb(Rgb {
                red: 4,
                green: 5,
                blue: 6
            }))
        );
    }

    #[test]
    fn preserves_indexed_background_on_erased_cells() {
        let snapshot = terminal(4, 1).feed(b"\x1b[41m\x1b[2K").unwrap().unwrap();
        assert!(snapshot.cells.iter().all(|cell| {
            cell.style.background == Some(CellColor::Indexed(1)) && cell.style.foreground.is_none()
        }));
    }

    #[test]
    fn tracks_cursor_movement() {
        let snapshot = terminal(8, 4).feed(b"\x1b[3;5H").unwrap().unwrap();
        assert_eq!((snapshot.cursor.column, snapshot.cursor.row), (4, 2));
    }

    #[test]
    fn restores_primary_screen_after_alternate_screen() {
        let mut terminal = terminal(8, 2);
        terminal.feed(b"primary").unwrap().unwrap();
        terminal.feed(b"\x1b[?1049hother").unwrap().unwrap();
        let snapshot = terminal.feed(b"\x1b[?1049l").unwrap().unwrap();
        assert!(text(&snapshot).starts_with("primary"));
    }

    #[test]
    fn resize_updates_grid_and_revision() {
        let mut terminal = terminal(4, 2);
        let before = terminal.feed(b"abcdef").unwrap().unwrap();
        let after = terminal
            .resize(TerminalSize {
                columns: 3,
                rows: 3,
            })
            .unwrap();
        assert_eq!(after.cells.len(), 9);
        assert_eq!(after.size.columns, 3);
        assert!(after.revision > before.revision);
    }

    #[test]
    fn temporary_history_view_restores_bottom_and_keeps_revisions_monotonic() {
        let mut terminal = terminal(8, 3);
        let bottom = terminal
            .feed(b"00\r\n01\r\n02\r\n03\r\n04\r\n05")
            .unwrap()
            .unwrap();
        assert!(text(&bottom).contains("05"));

        let MouseWheelOutcome::Scrolled(history) = terminal
            .mouse_wheel(
                MouseWheelEvent {
                    direction: MouseWheelDirection::Up,
                    column: 0,
                    row: 0,
                    modifiers: MouseModifiers::default(),
                },
                None,
                true,
            )
            .unwrap()
        else {
            panic!("wheel was unexpectedly forwarded");
        };
        assert!(history.offset.is_some());
        assert!(!text(&history.screen).contains("05"));
        assert!(terminal.terminal.viewport_active().unwrap());

        let after = terminal.feed(b"\r\n06").unwrap().unwrap();
        assert!(text(&after).contains("06"));
        assert!(after.revision > history.screen.revision);
        assert!(history.screen.revision > bottom.revision);
        assert!(terminal.terminal.viewport_active().unwrap());
    }

    #[test]
    fn tracked_wheel_uses_ghostty_modes_and_is_forwarded_to_the_pty() {
        let (mut terminal, output) = recording_terminal(10, 4);
        terminal.feed(b"\x1b[?1000h\x1b[?1006h").unwrap().unwrap();
        output.lock().unwrap().clear();

        let outcome = terminal
            .mouse_wheel(
                MouseWheelEvent {
                    direction: MouseWheelDirection::Up,
                    column: 2,
                    row: 1,
                    modifiers: MouseModifiers {
                        shift: true,
                        control: false,
                        alt: false,
                    },
                },
                None,
                true,
            )
            .unwrap();
        assert!(matches!(outcome, MouseWheelOutcome::Forwarded));
        assert_eq!(
            String::from_utf8(output.lock().unwrap().clone()).unwrap(),
            "\x1b[<68;3;2M"
        );
        output.lock().unwrap().clear();
        let outcome = terminal
            .mouse_wheel(
                MouseWheelEvent {
                    direction: MouseWheelDirection::Down,
                    column: 2,
                    row: 1,
                    modifiers: MouseModifiers::default(),
                },
                None,
                false,
            )
            .unwrap();
        assert!(matches!(outcome, MouseWheelOutcome::Forwarded));
        assert!(output.lock().unwrap().is_empty());
        assert!(terminal.terminal.viewport_active().unwrap());
    }

    #[test]
    fn paste_without_bracketed_mode_uses_ghostty_encoding_exactly() {
        let (mut terminal, output) = recording_terminal(10, 4);

        terminal
            .paste("héllo 雪\nnext\r\t\0\x1b[201~\x07\x02\x7f".into())
            .unwrap();

        assert_eq!(
            *output.lock().unwrap(),
            b"h\xc3\xa9llo \xe9\x9b\xaa\rnext\r\t  [201~\x07\x02 ".to_vec()
        );
    }

    #[test]
    fn paste_with_bracketed_mode_uses_ghostty_encoding_exactly() {
        let (mut terminal, output) = recording_terminal(10, 4);
        terminal.feed(b"\x1b[?2004h").unwrap().unwrap();

        terminal
            .paste("héllo 雪\nnext\r\t\0\x1b[201~\x07\x02\x7f".into())
            .unwrap();

        assert_eq!(
            *output.lock().unwrap(),
            b"\x1b[200~h\xc3\xa9llo \xe9\x9b\xaa\nnext\r\t  [201~\x07\x02 \x1b[201~".to_vec()
        );
    }

    #[test]
    fn incomplete_sequence_does_not_panic() {
        let mut terminal = terminal(4, 1);
        terminal.feed(b"\xf0\x9f\x1b[").unwrap().unwrap();
        terminal.snapshot().unwrap();
    }

    #[test]
    fn synchronized_output_hides_partial_frames() {
        let mut terminal = terminal(30, 1);
        let before = terminal.feed(b"\x1b[15GOLD_AT_RIGHT").unwrap().unwrap();
        assert!(text(&before).contains("OLD_AT_RIGHT"));

        terminal.feed(b"\x1b[?20").unwrap().unwrap();
        assert!(terminal.feed(b"26h\r\x1b[2KNEW_PARTIAL").unwrap().is_none());
        assert!(terminal.feed(b"_COMPLETE").unwrap().is_none());
        assert!(terminal.feed(b"\x1b[?20").unwrap().is_none());
        let complete = terminal.feed(b"26l").unwrap().unwrap();
        assert!(text(&complete).starts_with("NEW_PARTIAL_COMPLETE"));
        assert!(!text(&complete).contains("OLD_AT_RIGHT"));
    }

    #[test]
    fn synchronized_output_fails_open_after_timeout() {
        let mut terminal = terminal(20, 1);
        assert!(terminal.feed(b"\x1b[?2026hPARTIAL").unwrap().is_none());
        terminal.synchronized_output_started = Some(Instant::now() - SYNCHRONIZED_OUTPUT_TIMEOUT);

        let snapshot = terminal.flush_synchronized_output().unwrap().unwrap();
        assert!(text(&snapshot).starts_with("PARTIAL"));
        assert!(!terminal.terminal.mode(Mode::SYNC_OUTPUT).unwrap());
    }

    #[test]
    fn resize_finishes_synchronized_output_before_publishing() {
        let mut terminal = terminal(20, 1);
        assert!(terminal.feed(b"\x1b[?2026hPARTIAL").unwrap().is_none());

        let snapshot = terminal
            .resize(TerminalSize {
                columns: 10,
                rows: 2,
            })
            .unwrap();
        assert_eq!(snapshot.size.columns, 10);
        assert!(!terminal.terminal.mode(Mode::SYNC_OUTPUT).unwrap());
        assert!(terminal.synchronized_output_started.is_none());
    }
}
