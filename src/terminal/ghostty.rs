use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use libghostty_vt::{
    RenderState, Terminal, TerminalOptions,
    render::{CellIterator, RowIterator},
    style::Underline,
};

use crate::domain::{Cell, CellStyle, Cursor, Rgb, ScreenSnapshot, TerminalSize};

/// The complete libghostty boundary. This value must never leave its runtime thread.
pub(super) struct GhosttyTerminal {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    size: TerminalSize,
    revision: u64,
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
        terminal.on_pty_write(move |_, bytes| {
            if let Ok(mut writer) = writer.lock() {
                let _ = writer.write_all(bytes);
                let _ = writer.flush();
            }
        })?;

        Ok(Self {
            terminal,
            render_state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            size,
            revision: 0,
        })
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<ScreenSnapshot> {
        self.terminal.vt_write(bytes);
        self.snapshot()
    }

    pub(super) fn resize(&mut self, size: TerminalSize) -> Result<ScreenSnapshot> {
        validate_size(size)?;
        self.terminal
            .resize(size.columns, size.rows, 0, 0)
            .context("resizing Ghostty terminal")?;
        self.size = size;
        self.snapshot()
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
                result.push(Cell {
                    contents,
                    style: CellStyle {
                        foreground: cell.fg_color()?.map(rgb),
                        background: cell.bg_color()?.map(rgb),
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

fn validate_size(size: TerminalSize) -> Result<()> {
    ensure!(
        size.columns > 0 && size.rows > 0,
        "terminal dimensions must be non-zero"
    );
    Ok(())
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

    fn terminal(columns: u16, rows: u16) -> GhosttyTerminal {
        GhosttyTerminal::new(
            TerminalSize { columns, rows },
            Arc::new(Mutex::new(Box::new(io::sink()))),
        )
        .unwrap()
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
        let snapshot = terminal(5, 2).feed(b"hi").unwrap();
        assert_eq!(&text(&snapshot)[..5], "hi   ");
        assert_eq!(snapshot.cells.len(), 10);
        assert!(snapshot.revision > 0);
    }

    #[test]
    fn split_utf8_and_escape_input_matches_one_write() {
        let bytes = "aé\x1b[2;2HZ".as_bytes();
        let mut whole = terminal(6, 2);
        let expected = whole.feed(bytes).unwrap();
        let mut split = terminal(6, 2);
        for chunk in bytes.chunks(1) {
            split.feed(chunk).unwrap();
        }
        let actual = split.snapshot().unwrap();
        assert_eq!(actual.cells, expected.cells);
        assert_eq!(actual.cursor, expected.cursor);
    }

    #[test]
    fn maps_truecolor_and_text_styles() {
        let snapshot = terminal(4, 1)
            .feed(b"\x1b[1;3;4;38;2;1;2;3;48;2;4;5;6mX")
            .unwrap();
        let style = snapshot.cells[0].style;
        assert_eq!(
            style.foreground,
            Some(Rgb {
                red: 1,
                green: 2,
                blue: 3
            })
        );
        assert_eq!(
            style.background,
            Some(Rgb {
                red: 4,
                green: 5,
                blue: 6
            })
        );
        assert!(style.bold && style.italic && style.underline);
    }

    #[test]
    fn tracks_cursor_movement() {
        let snapshot = terminal(8, 4).feed(b"\x1b[3;5H").unwrap();
        assert_eq!((snapshot.cursor.column, snapshot.cursor.row), (4, 2));
    }

    #[test]
    fn restores_primary_screen_after_alternate_screen() {
        let mut terminal = terminal(8, 2);
        terminal.feed(b"primary").unwrap();
        terminal.feed(b"\x1b[?1049hother").unwrap();
        let snapshot = terminal.feed(b"\x1b[?1049l").unwrap();
        assert!(text(&snapshot).starts_with("primary"));
    }

    #[test]
    fn resize_updates_grid_and_revision() {
        let mut terminal = terminal(4, 2);
        let before = terminal.feed(b"abcdef").unwrap();
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
    fn incomplete_sequence_does_not_panic() {
        let mut terminal = terminal(4, 1);
        terminal.feed(b"\xf0\x9f\x1b[").unwrap();
        terminal.snapshot().unwrap();
    }
}
