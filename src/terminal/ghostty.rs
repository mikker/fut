use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use compact_str::CompactString;
use libghostty_vt::{
    RenderState, Terminal, TerminalOptions,
    error::Error as GhosttyError,
    fmt::Format,
    key, mouse, paste,
    render::{CellIterator, RowIterator},
    screen::{CellContentTag, CellWide, GridRef, Screen, TrackedGridRef},
    selection::{FormatOptions, Selection},
    style::{StyleColor, Underline},
    terminal::{Mode, Point, PointCoordinate, PointSpace, ScrollViewport},
};
use uuid::Uuid;

use crate::domain::{
    Cell, CellColor, CellStyle, ClientId, CopyModeAction, CopyModeError, CopyModeMovement, Cursor,
    MAX_COPY_BYTES, MAX_COPY_CELLS, MAX_SEARCH_CELL_CODEPOINTS, MAX_SEARCH_CELLS,
    MAX_SEARCH_QUERY_BYTES, MAX_SEARCH_TEXT_BYTES, MAX_TERMINAL_OUTPUT_BYTES,
    MAX_TERMINAL_OUTPUT_CELLS, MAX_TERMINAL_OUTPUT_ROWS, MouseButton, MouseEvent, MouseEventKind,
    MouseModifiers, MouseWheelDirection, Rgb, ScreenSnapshot, SearchDirection,
    TerminalOutputSource, TerminalSize,
};

const SYNCHRONIZED_OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);
const MOUSE_WHEEL_LINES: isize = 3;

pub(crate) enum MouseInputOutcome {
    Handled,
    ReturnedToBottom(ViewportSnapshot),
    Scrolled(ViewportSnapshot),
}

pub(crate) struct ViewportSnapshot {
    pub(crate) offset: Option<usize>,
    pub(crate) screen: ScreenSnapshot,
}

pub(crate) enum CopyModeOutcome {
    Active(ViewportSnapshot),
    Prepared { copy_id: Uuid, text: String },
    Finalized { screen: ScreenSnapshot },
    Cancelled { screen: ScreenSnapshot },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutputCapture {
    pub(crate) revision: u64,
    pub(crate) source: TerminalOutputSource,
    pub(crate) requested_rows: usize,
    pub(crate) returned_rows: usize,
    pub(crate) truncated: bool,
    pub(crate) starts_mid_logical_line: bool,
    pub(crate) ansi: bool,
    pub(crate) text: String,
}

#[derive(Clone, Debug, thiserror::Error, Eq, PartialEq)]
pub enum OutputCaptureError {
    #[error("terminal output rows must be between 1 and {MAX_TERMINAL_OUTPUT_ROWS}")]
    InvalidRows,
    #[error("terminal output inspection requires {actual} cells; maximum is {maximum}")]
    TooManyCells { actual: usize, maximum: usize },
    #[error("terminal output is {actual} bytes; maximum is {maximum}")]
    TooManyBytes { actual: usize, maximum: usize },
    #[error("historical terminal output is unavailable while the alternate screen is active")]
    AlternateScreen,
    #[error("terminal emulator operation failed: {0}")]
    Emulator(String),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CopyModeFailure {
    #[error(transparent)]
    Semantic(#[from] CopyModeError),
    #[error("copy-mode cursor was discarded by the terminal")]
    CursorLost {
        canonical: Option<ScreenSnapshot>,
        cleanup_error: Option<anyhow::Error>,
    },
    #[error(transparent)]
    Emulator(#[from] anyhow::Error),
}

impl From<GhosttyError> for CopyModeFailure {
    fn from(error: GhosttyError) -> Self {
        Self::Emulator(error.into())
    }
}

struct ClientCopyState {
    screen: Screen,
    cursor: TrackedGridRef,
    anchor: Option<TrackedGridRef>,
    search: Option<ClientSearchState>,
    prepared_copy: Option<Uuid>,
}

struct ClientSearchState {
    query: String,
    start: TrackedGridRef,
    end: TrackedGridRef,
}

struct SearchText {
    value: String,
    segments: Vec<SearchSegment>,
    columns: u16,
}

#[derive(Clone, Copy)]
struct SearchSegment {
    start: u32,
    end: u32,
    cell: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SearchMatch {
    start: PointCoordinate,
    end: PointCoordinate,
}

#[derive(Clone, Copy)]
enum SearchBoundary {
    At(PointCoordinate),
    After(PointCoordinate),
    Before(PointCoordinate),
}

/// The complete libghostty boundary. This value must never leave its runtime thread.
pub(super) struct GhosttyTerminal {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    key_encoder: key::Encoder<'static>,
    key_event: key::Event<'static>,
    mouse_encoder: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    size: TerminalSize,
    revision: u64,
    synchronized_output_started: Option<Instant>,
    copy_modes: HashMap<ClientId, ClientCopyState>,
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
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            writer,
            size,
            revision: 0,
            synchronized_output_started: None,
            copy_modes: HashMap::new(),
        })
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<Option<ScreenSnapshot>> {
        self.vt_write(bytes);
        self.snapshot_after_feed()
    }

    /// Parse-only half of `feed`, so a caller can push several PTY chunks
    /// through the parser and pay for one snapshot at the end of the batch.
    pub(super) fn vt_write(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    /// Snapshot half of `feed`. Respects synchronized-output suppression the
    /// same way `feed` always has, whether called once per chunk or once per
    /// drained batch of chunks.
    pub(super) fn snapshot_after_feed(&mut self) -> Result<Option<ScreenSnapshot>> {
        if self.holds_synchronized_output()? {
            return Ok(None);
        }
        self.snapshot().map(Some)
    }

    /// Publish an idle terminal only when an unterminated synchronized-output
    /// frame has expired. A quiet terminal must not manufacture revisions:
    /// every published snapshot wakes every attached client.
    pub(super) fn flush_synchronized_output(&mut self) -> Result<Option<ScreenSnapshot>> {
        if self.synchronized_output_started.is_none() || self.holds_synchronized_output()? {
            return Ok(None);
        }
        self.snapshot().map(Some)
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

    pub(super) fn output(
        &self,
        source: TerminalOutputSource,
        requested_rows: usize,
        ansi: bool,
    ) -> std::result::Result<OutputCapture, OutputCaptureError> {
        if requested_rows == 0 || requested_rows > MAX_TERMINAL_OUTPUT_ROWS {
            return Err(OutputCaptureError::InvalidRows);
        }
        if source != TerminalOutputSource::Visible
            && self
                .terminal
                .active_screen()
                .map_err(|error| OutputCaptureError::Emulator(error.to_string()))?
                == Screen::Alternate
        {
            return Err(OutputCaptureError::AlternateScreen);
        }

        let total_rows = self
            .terminal
            .total_rows()
            .map_err(|error| OutputCaptureError::Emulator(error.to_string()))?;
        let requested_rows = match source {
            TerminalOutputSource::Visible => usize::from(self.size.rows),
            TerminalOutputSource::Recent | TerminalOutputSource::RecentUnwrapped => requested_rows,
        };
        let returned_rows = requested_rows.min(total_rows);
        let start_row = total_rows.saturating_sub(returned_rows);
        let cells = returned_rows.saturating_mul(usize::from(self.size.columns));
        if cells > MAX_TERMINAL_OUTPUT_CELLS {
            return Err(OutputCaptureError::TooManyCells {
                actual: cells,
                maximum: MAX_TERMINAL_OUTPUT_CELLS,
            });
        }

        let starts_mid_logical_line = if source == TerminalOutputSource::Visible || start_row == 0 {
            false
        } else {
            self.terminal
                .grid_ref(Point::Screen(PointCoordinate {
                    x: 0,
                    y: u32::try_from(start_row - 1)
                        .map_err(|error| OutputCaptureError::Emulator(error.to_string()))?,
                }))
                .and_then(|reference| reference.row())
                .and_then(|row| row.is_wrapped())
                .map_err(|error| OutputCaptureError::Emulator(error.to_string()))?
        };

        let text = if returned_rows == 0 {
            String::new()
        } else {
            let (start, end) = match source {
                TerminalOutputSource::Visible => (
                    Point::Active(PointCoordinate { x: 0, y: 0 }),
                    Point::Active(PointCoordinate {
                        x: self.size.columns - 1,
                        y: u32::from(self.size.rows - 1),
                    }),
                ),
                TerminalOutputSource::Recent | TerminalOutputSource::RecentUnwrapped => (
                    Point::Screen(PointCoordinate {
                        x: 0,
                        y: u32::try_from(start_row)
                            .map_err(|error| OutputCaptureError::Emulator(error.to_string()))?,
                    }),
                    Point::Screen(PointCoordinate {
                        x: self.size.columns - 1,
                        y: u32::try_from(total_rows - 1)
                            .map_err(|error| OutputCaptureError::Emulator(error.to_string()))?,
                    }),
                ),
            };
            let start = self
                .terminal
                .grid_ref(start)
                .map_err(|error| OutputCaptureError::Emulator(error.to_string()))?;
            let end = self
                .terminal
                .grid_ref(end)
                .map_err(|error| OutputCaptureError::Emulator(error.to_string()))?;
            let selection = Selection::new(start, end, false);
            let options = FormatOptions::new()
                .with_emit_format(if ansi { Format::Vt } else { Format::Plain })
                .with_unwrap(source == TerminalOutputSource::RecentUnwrapped)
                .with_trim(true)
                .with_selection(&selection);
            let mut output = vec![0; MAX_TERMINAL_OUTPUT_BYTES];
            let written = match self.terminal.format_selection_buf(options, &mut output) {
                Ok(Some(written)) => written,
                Ok(None) => 0,
                Err(GhosttyError::OutOfSpace { required }) => {
                    return Err(OutputCaptureError::TooManyBytes {
                        actual: required,
                        maximum: MAX_TERMINAL_OUTPUT_BYTES,
                    });
                }
                Err(error) => return Err(OutputCaptureError::Emulator(error.to_string())),
            };
            output.truncate(written);
            String::from_utf8(output)
                .map_err(|error| OutputCaptureError::Emulator(error.to_string()))?
        };

        Ok(OutputCapture {
            revision: self.revision,
            source,
            requested_rows,
            returned_rows,
            truncated: start_row > 0,
            starts_mid_logical_line,
            ansi,
            text,
        })
    }

    /// Render one client-owned historical viewport, restoring the shared
    /// terminal to its canonical bottom before returning.
    pub(super) fn viewport_snapshot(&mut self, offset: Option<usize>) -> Result<ViewportSnapshot> {
        self.terminal.set_selection(None)?;
        self.terminal.scroll_viewport(match offset {
            Some(offset) => ScrollViewport::Row(offset),
            None => ScrollViewport::Bottom,
        });
        self.snapshot_viewport_and_restore_bottom()
    }

    pub(super) fn mouse_input(
        &mut self,
        event: MouseEvent,
        offset: Option<usize>,
        pty_input_allowed: bool,
    ) -> Result<MouseInputOutcome> {
        let MouseEventKind::Wheel { direction } = event.kind else {
            if !pty_input_allowed || !self.terminal.is_mouse_tracking()? {
                return Ok(MouseInputOutcome::Handled);
            }
            self.forward_mouse(event)?;
            return self.finish_application_mouse_input(offset);
        };

        if self.terminal.is_mouse_tracking()? {
            if !pty_input_allowed {
                return Ok(MouseInputOutcome::Handled);
            }
            self.forward_mouse(event)?;
            return self.finish_application_mouse_input(offset);
        }

        if self.terminal.active_screen()? == Screen::Alternate
            && self.terminal.mode(Mode::ALT_SCROLL)?
        {
            if !pty_input_allowed {
                return Ok(MouseInputOutcome::Handled);
            }
            self.forward_alternate_scroll(direction)?;
            return self.finish_application_mouse_input(offset);
        }

        // Overscroll cannot move the viewport; drop it before it costs a
        // snapshot build so a fling that hits either end queues no work.
        let scrollbar = self.terminal.scrollbar()?;
        let bottom_row = usize::try_from(scrollbar.total.saturating_sub(scrollbar.len))
            .context("converting Ghostty scrollback size")?;
        let position = offset.unwrap_or(bottom_row);
        let overscroll = match direction {
            MouseWheelDirection::Up => position == 0,
            MouseWheelDirection::Down => position >= bottom_row,
        };
        if overscroll {
            return Ok(MouseInputOutcome::Handled);
        }

        self.terminal.set_selection(None)?;
        self.terminal.scroll_viewport(match offset {
            Some(offset) => ScrollViewport::Row(offset),
            None => ScrollViewport::Bottom,
        });
        let delta = match direction {
            MouseWheelDirection::Up => -MOUSE_WHEEL_LINES,
            MouseWheelDirection::Down => MOUSE_WHEEL_LINES,
        };
        self.terminal.scroll_viewport(ScrollViewport::Delta(delta));
        self.snapshot_viewport_and_restore_bottom()
            .map(MouseInputOutcome::Scrolled)
    }

    fn finish_application_mouse_input(
        &mut self,
        previous_offset: Option<usize>,
    ) -> Result<MouseInputOutcome> {
        if previous_offset.is_none() {
            return Ok(MouseInputOutcome::Handled);
        }
        self.viewport_snapshot(None)
            .map(MouseInputOutcome::ReturnedToBottom)
    }

    pub(super) fn paste(&mut self, text: String) -> Result<()> {
        let response = self.encode_paste(text)?;
        self.write_encoded_input(&response, "paste")
    }

    pub(super) fn paste_and_input(&mut self, text: String, input: &[u8]) -> Result<()> {
        let mut response = self.encode_paste(text)?;
        response.extend_from_slice(input);
        self.write_encoded_input(&response, "paste and input")
    }

    fn encode_paste(&mut self, text: String) -> Result<Vec<u8>> {
        let mut data = text.into_bytes();
        let capacity = data
            .len()
            .checked_add(12)
            .context("sizing encoded terminal paste")?;
        let mut response = vec![0; capacity];
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE)?;
        let written = paste::encode(&mut data, bracketed, &mut response)
            .context("encoding terminal paste with Ghostty")?;
        response.truncate(written);
        Ok(response)
    }

    pub(super) fn copy_mode(
        &mut self,
        owner: ClientId,
        action: CopyModeAction,
        viewport_offset: Option<usize>,
    ) -> std::result::Result<CopyModeOutcome, CopyModeFailure> {
        let beginning = matches!(&action, CopyModeAction::Begin);
        let result = self
            .check_copy_state(owner, beginning)
            .and_then(|()| self.copy_mode_inner(owner, action, viewport_offset));
        match result {
            Err(CopyModeFailure::Semantic(CopyModeError::CursorLost)) => {
                Err(self.invalidate_cursor_lost(owner))
            }
            result => result,
        }
    }

    fn copy_mode_inner(
        &mut self,
        owner: ClientId,
        action: CopyModeAction,
        viewport_offset: Option<usize>,
    ) -> std::result::Result<CopyModeOutcome, CopyModeFailure> {
        match action {
            CopyModeAction::Begin => self
                .begin_copy_mode(owner, viewport_offset)
                .map(CopyModeOutcome::Active),
            CopyModeAction::Move { movement } => {
                self.move_copy_cursor(owner, movement)?;
                self.copy_mode_snapshot_inner(owner, viewport_offset)
                    .map(CopyModeOutcome::Active)
            }
            CopyModeAction::ToggleSelection => {
                self.toggle_copy_selection(owner)?;
                self.copy_mode_snapshot_inner(owner, viewport_offset)
                    .map(CopyModeOutcome::Active)
            }
            CopyModeAction::Search { query } => {
                self.search_copy_mode(owner, query, SearchDirection::Forward, false)?;
                self.copy_mode_snapshot_inner(owner, viewport_offset)
                    .map(CopyModeOutcome::Active)
            }
            CopyModeAction::RepeatSearch { direction } => {
                let query = self
                    .copy_modes
                    .get(&owner)
                    .ok_or(CopyModeError::NotActive)?
                    .search
                    .as_ref()
                    .ok_or(CopyModeError::NoSearch)?
                    .query
                    .clone();
                self.search_copy_mode(owner, query, direction, true)?;
                self.copy_mode_snapshot_inner(owner, viewport_offset)
                    .map(CopyModeOutcome::Active)
            }
            CopyModeAction::Copy => {
                let text = self.format_copy_selection(owner)?;
                let copy_id = Uuid::new_v4();
                self.copy_modes
                    .get_mut(&owner)
                    .ok_or(CopyModeError::NotActive)?
                    .prepared_copy = Some(copy_id);
                Ok(CopyModeOutcome::Prepared { copy_id, text })
            }
            CopyModeAction::FinalizeCopy { copy_id } => self
                .finalize_copy_mode(owner, copy_id)
                .map(|screen| CopyModeOutcome::Finalized { screen }),
            CopyModeAction::Cancel => self
                .cancel_copy_mode(owner)
                .map(|screen| CopyModeOutcome::Cancelled { screen }),
        }
    }

    pub(super) fn copy_mode_snapshot(
        &mut self,
        owner: ClientId,
        viewport_offset: Option<usize>,
    ) -> std::result::Result<ViewportSnapshot, CopyModeFailure> {
        let result = self
            .check_copy_state(owner, false)
            .and_then(|()| self.copy_mode_snapshot_inner(owner, viewport_offset));
        match result {
            Err(CopyModeFailure::Semantic(CopyModeError::CursorLost)) => {
                Err(self.invalidate_cursor_lost(owner))
            }
            result => result,
        }
    }

    fn copy_mode_snapshot_inner(
        &mut self,
        owner: ClientId,
        viewport_offset: Option<usize>,
    ) -> std::result::Result<ViewportSnapshot, CopyModeFailure> {
        let offset = self.copy_cursor_viewport(owner, viewport_offset)?;
        self.terminal.scroll_viewport(match offset {
            Some(offset) => ScrollViewport::Row(offset),
            None => ScrollViewport::Bottom,
        });

        let result = (|| {
            self.install_copy_selection(owner)?;
            let scrollbar = self.terminal.scrollbar()?;
            let bottom = scrollbar.total.saturating_sub(scrollbar.len);
            let offset = (scrollbar.offset < bottom)
                .then(|| usize::try_from(scrollbar.offset))
                .transpose()
                .context("converting Ghostty copy-mode viewport offset")
                .map_err(CopyModeFailure::Emulator)?;
            let mut screen = self.snapshot_current()?;
            screen.cursor.visible = false;
            Ok::<_, CopyModeFailure>(ViewportSnapshot { offset, screen })
        })();
        let restore = self.restore_canonical();
        match (result, restore) {
            (Err(error @ CopyModeFailure::Semantic(CopyModeError::CursorLost)), _) => Err(error),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(restore_error)) => Err(CopyModeFailure::Emulator(anyhow::anyhow!(
                "{error:#}; also failed to restore canonical viewport: {restore_error:#}"
            ))),
            (Ok(_), Err(error)) => Err(CopyModeFailure::Emulator(error)),
            (Ok(snapshot), Ok(())) => Ok(snapshot),
        }
    }

    pub(super) fn clear_copy_mode(&mut self, owner: ClientId) -> Result<Option<ScreenSnapshot>> {
        self.finish_copy_mode(owner).map(Some)
    }

    fn begin_copy_mode(
        &mut self,
        owner: ClientId,
        viewport_offset: Option<usize>,
    ) -> std::result::Result<ViewportSnapshot, CopyModeFailure> {
        self.restore_canonical()?;
        let screen = self.terminal.active_screen()?;
        let total_rows = self.terminal.total_rows()?;
        let visible_rows = usize::from(self.size.rows);
        let bottom = total_rows.saturating_sub(visible_rows);
        let viewport_top = viewport_offset.unwrap_or(bottom).min(bottom);
        let relative_y = usize::from(self.terminal.cursor_y()?.min(self.size.rows - 1));
        let y = viewport_top
            .saturating_add(relative_y)
            .min(total_rows.saturating_sub(1));
        let cursor = PointCoordinate {
            x: self.terminal.cursor_x()?.min(self.size.columns - 1),
            y: u32::try_from(y).context("converting initial copy-mode cursor row")?,
        };
        let cursor = self.terminal.track_grid_ref(Point::Screen(cursor))?;
        self.copy_modes.insert(
            owner,
            ClientCopyState {
                screen,
                cursor,
                anchor: None,
                search: None,
                prepared_copy: None,
            },
        );
        match self.copy_mode_snapshot_inner(owner, viewport_offset) {
            Ok(viewport) => Ok(viewport),
            Err(error) => {
                match self.finish_copy_mode(owner) {
                    Ok(_) => Err(error),
                    Err(_)
                        if matches!(
                            &error,
                            CopyModeFailure::Semantic(CopyModeError::CursorLost)
                        ) =>
                    {
                        // The outer operation wrapper retries canonical cleanup
                        // while preserving the semantic invalidation.
                        Err(error)
                    }
                    Err(cleanup_error) => Err(CopyModeFailure::Emulator(anyhow::anyhow!(
                        "{error:#}; also failed to roll back copy-mode begin: {cleanup_error:#}"
                    ))),
                }
            }
        }
    }

    fn finalize_copy_mode(
        &mut self,
        owner: ClientId,
        copy_id: Uuid,
    ) -> std::result::Result<ScreenSnapshot, CopyModeFailure> {
        let state = self
            .copy_modes
            .get(&owner)
            .ok_or(CopyModeError::NotActive)?;
        if state.prepared_copy != Some(copy_id) {
            return Err(CopyModeError::CopyConfirmationMismatch.into());
        }

        self.finish_copy_mode(owner).map_err(Into::into)
    }

    fn cancel_copy_mode(
        &mut self,
        owner: ClientId,
    ) -> std::result::Result<ScreenSnapshot, CopyModeFailure> {
        if !self.copy_modes.contains_key(&owner) {
            return Err(CopyModeError::NotActive.into());
        }
        self.finish_copy_mode(owner).map_err(Into::into)
    }

    fn check_copy_state(
        &self,
        owner: ClientId,
        beginning: bool,
    ) -> std::result::Result<(), CopyModeFailure> {
        let Some(state) = self.copy_modes.get(&owner) else {
            return if beginning {
                Ok(())
            } else {
                Err(CopyModeError::NotActive.into())
            };
        };
        let active_screen = self.terminal.active_screen()?;
        let refs_valid = state.cursor.has_value()
            && state.anchor.as_ref().is_none_or(TrackedGridRef::has_value)
            && state
                .search
                .as_ref()
                .is_none_or(|search| search.start.has_value() && search.end.has_value());
        if active_screen != state.screen || !refs_valid {
            return Err(CopyModeError::CursorLost.into());
        }
        if beginning {
            Err(CopyModeError::AlreadyActive.into())
        } else {
            Ok(())
        }
    }

    fn invalidate_cursor_lost(&mut self, owner: ClientId) -> CopyModeFailure {
        match self.finish_copy_mode(owner) {
            Ok(canonical) => CopyModeFailure::CursorLost {
                canonical: Some(canonical),
                cleanup_error: None,
            },
            Err(error) => CopyModeFailure::CursorLost {
                canonical: None,
                cleanup_error: Some(error),
            },
        }
    }

    /// Restore the shared emulator and materialize its selection-free bottom
    /// snapshot before releasing an owner. Keeping removal as the commit step
    /// makes every cleanup path retryable if any emulator operation fails.
    fn finish_copy_mode(&mut self, owner: ClientId) -> Result<ScreenSnapshot> {
        let screen = self.canonical_snapshot()?;
        self.copy_modes.remove(&owner);
        Ok(screen)
    }

    fn move_copy_cursor(
        &mut self,
        owner: ClientId,
        movement: CopyModeMovement,
    ) -> std::result::Result<(), CopyModeFailure> {
        // These are physical-cell movements, intentionally unlike Ghostty's
        // semantic `Selection::adjust` left/right/down operations. Blank cells
        // remain reachable so users can choose precise whitespace endpoints.
        let cursor = self.copy_cursor_point(owner)?;
        let total_rows = self.terminal.total_rows()?;
        let last_y = u32::try_from(total_rows.saturating_sub(1))
            .context("converting final copy-mode row")?;
        let last_x = self.size.columns - 1;
        let page = u32::from(self.size.rows);
        let point = match movement {
            CopyModeMovement::Left if cursor.x > 0 => PointCoordinate {
                x: cursor.x - 1,
                ..cursor
            },
            CopyModeMovement::Left if cursor.y > 0 => PointCoordinate {
                x: last_x,
                y: cursor.y - 1,
            },
            CopyModeMovement::Right if cursor.x < last_x => PointCoordinate {
                x: cursor.x + 1,
                ..cursor
            },
            CopyModeMovement::Right if cursor.y < last_y => PointCoordinate {
                x: 0,
                y: cursor.y + 1,
            },
            CopyModeMovement::Up => PointCoordinate {
                y: cursor.y.saturating_sub(1),
                ..cursor
            },
            CopyModeMovement::Down => PointCoordinate {
                y: cursor.y.saturating_add(1).min(last_y),
                ..cursor
            },
            CopyModeMovement::BeginningOfLine => PointCoordinate { x: 0, ..cursor },
            CopyModeMovement::EndOfLine => PointCoordinate {
                x: last_x,
                ..cursor
            },
            CopyModeMovement::PageUp => PointCoordinate {
                y: cursor.y.saturating_sub(page),
                ..cursor
            },
            CopyModeMovement::PageDown => PointCoordinate {
                y: cursor.y.saturating_add(page).min(last_y),
                ..cursor
            },
            CopyModeMovement::Left | CopyModeMovement::Right => cursor,
        };
        let state = self
            .copy_modes
            .get_mut(&owner)
            .ok_or(CopyModeError::NotActive)?;
        state.cursor.set(&mut self.terminal, Point::Screen(point))?;
        state.prepared_copy = None;
        Ok(())
    }

    fn toggle_copy_selection(
        &mut self,
        owner: ClientId,
    ) -> std::result::Result<(), CopyModeFailure> {
        let point = self.copy_cursor_point(owner)?;
        let state = self
            .copy_modes
            .get_mut(&owner)
            .ok_or(CopyModeError::NotActive)?;
        if state.anchor.is_some() {
            state.anchor = None;
        } else {
            state.anchor = Some(self.terminal.track_grid_ref(Point::Screen(point))?);
        }
        state.prepared_copy = None;
        Ok(())
    }

    fn install_copy_selection(&self, owner: ClientId) -> std::result::Result<(), CopyModeFailure> {
        let state = self
            .copy_modes
            .get(&owner)
            .ok_or(CopyModeError::NotActive)?;
        validate_copy_cells(self.copy_selection_cell_span(state)?)?;
        let cursor = state
            .cursor
            .snapshot(&self.terminal)?
            .ok_or(CopyModeError::CursorLost)?;
        let start = match state.anchor.as_ref() {
            Some(anchor) => anchor
                .snapshot(&self.terminal)?
                .ok_or(CopyModeError::CursorLost)?,
            None => cursor.clone(),
        };
        let selection = Selection::new(start, cursor, false);
        self.terminal.set_selection(Some(&selection))?;
        Ok(())
    }

    fn copy_selection_cell_span(
        &self,
        state: &ClientCopyState,
    ) -> std::result::Result<usize, CopyModeFailure> {
        let start = state
            .anchor
            .as_ref()
            .unwrap_or(&state.cursor)
            .point(PointSpace::Screen)?
            .ok_or(CopyModeError::CursorLost)?;
        let end = state
            .cursor
            .point(PointSpace::Screen)?
            .ok_or(CopyModeError::CursorLost)?;
        selection_cell_span(start, end, self.size.columns)
    }

    fn format_copy_selection(
        &self,
        owner: ClientId,
    ) -> std::result::Result<String, CopyModeFailure> {
        let state = self
            .copy_modes
            .get(&owner)
            .ok_or(CopyModeError::NotActive)?;
        let cursor = state
            .cursor
            .snapshot(&self.terminal)?
            .ok_or(CopyModeError::CursorLost)?;
        let start = match state.anchor.as_ref() {
            Some(anchor) => anchor
                .snapshot(&self.terminal)?
                .ok_or(CopyModeError::CursorLost)?,
            None => cursor.clone(),
        };
        let selection = Selection::new(start, cursor, false);
        let options = FormatOptions::new()
            .with_emit_format(Format::Plain)
            .with_unwrap(true)
            .with_trim(true)
            .with_selection(&selection);
        let mut output = vec![0; MAX_COPY_BYTES];
        validate_copy_cells(self.copy_selection_cell_span(state)?)?;
        let written = match self.terminal.format_selection_buf(options, &mut output) {
            Ok(Some(written)) => {
                validate_copy_size(written)?;
                written
            }
            Ok(None) => 0,
            Err(GhosttyError::OutOfSpace { required }) => {
                validate_copy_size(required)?;
                unreachable!("Ghostty reported an in-capacity selection as out of space");
            }
            Err(error) => return Err(anyhow::Error::from(error).into()),
        };
        output.truncate(written);
        String::from_utf8(output)
            .context("Ghostty returned non-UTF-8 plain selection text")
            .map_err(CopyModeFailure::Emulator)
    }

    fn copy_cursor_point(
        &self,
        owner: ClientId,
    ) -> std::result::Result<PointCoordinate, CopyModeFailure> {
        self.copy_modes
            .get(&owner)
            .ok_or(CopyModeError::NotActive)?
            .cursor
            .point(PointSpace::Screen)?
            .ok_or_else(|| CopyModeError::CursorLost.into())
    }

    fn copy_cursor_viewport(
        &self,
        owner: ClientId,
        viewport_offset: Option<usize>,
    ) -> std::result::Result<Option<usize>, CopyModeFailure> {
        let cursor = self.copy_cursor_point(owner)?;
        let total = self.terminal.total_rows()?;
        let rows = usize::from(self.size.rows);
        let bottom = total.saturating_sub(rows);
        let mut offset = viewport_offset.unwrap_or(bottom).min(bottom);
        let cursor_row = usize::try_from(cursor.y).context("converting copy-mode cursor row")?;
        if cursor_row < offset {
            offset = cursor_row;
        } else if cursor_row >= offset.saturating_add(rows) {
            offset = cursor_row.saturating_add(1).saturating_sub(rows);
        }
        Ok((offset < bottom).then_some(offset))
    }

    fn search_copy_mode(
        &mut self,
        owner: ClientId,
        query: String,
        direction: SearchDirection,
        repeat: bool,
    ) -> std::result::Result<(), CopyModeFailure> {
        validate_search_query(&query)?;
        let boundary = {
            let state = self
                .copy_modes
                .get(&owner)
                .ok_or(CopyModeError::NotActive)?;
            if repeat {
                let search = state.search.as_ref().ok_or(CopyModeError::NoSearch)?;
                match direction {
                    SearchDirection::Forward => SearchBoundary::After(
                        search
                            .end
                            .point(PointSpace::Screen)?
                            .ok_or(CopyModeError::CursorLost)?,
                    ),
                    SearchDirection::Backward => SearchBoundary::Before(
                        search
                            .start
                            .point(PointSpace::Screen)?
                            .ok_or(CopyModeError::CursorLost)?,
                    ),
                }
            } else {
                SearchBoundary::At(
                    state
                        .cursor
                        .point(PointSpace::Screen)?
                        .ok_or(CopyModeError::CursorLost)?,
                )
            }
        };
        let text = self.search_text()?;
        let found = text
            .find(&query, boundary, direction)
            .ok_or(CopyModeError::NoMatch)?;

        let search_start = self.terminal.track_grid_ref(Point::Screen(found.start))?;
        let search_end = self.terminal.track_grid_ref(Point::Screen(found.end))?;
        let state = self
            .copy_modes
            .get_mut(&owner)
            .ok_or(CopyModeError::NotActive)?;
        state
            .cursor
            .set(&mut self.terminal, Point::Screen(found.start))?;
        state.search = Some(ClientSearchState {
            query,
            start: search_start,
            end: search_end,
        });
        state.prepared_copy = None;
        Ok(())
    }

    fn search_text(&self) -> std::result::Result<SearchText, CopyModeFailure> {
        let total_rows = self.terminal.total_rows()?;
        let columns = usize::from(self.size.columns);
        let cells = total_rows
            .checked_mul(columns)
            .ok_or(CopyModeError::SearchSpaceTooLarge {
                actual: usize::MAX,
                maximum: MAX_SEARCH_CELLS,
            })?;
        validate_search_cells(cells)?;

        let mut value = String::new();
        let mut mapped = Vec::with_capacity(cells);
        for y in 0..total_rows {
            let point_y = u32::try_from(y).context("converting Ghostty search row")?;
            let first = self
                .terminal
                .grid_ref(Point::Screen(PointCoordinate { x: 0, y: point_y }))?;
            let wrapped = first.row()?.is_wrapped()?;
            let row_value_start = value.len();
            let row_map_start = mapped.len();
            let mut meaningful_value_end = row_value_start;
            let mut meaningful_map_end = row_map_start;
            for x in 0..self.size.columns {
                let point = PointCoordinate { x, y: point_y };
                let reference = self.terminal.grid_ref(Point::Screen(point))?;
                if matches!(
                    reference.cell()?.wide()?,
                    CellWide::SpacerTail | CellWide::SpacerHead
                ) {
                    continue;
                }
                let text = grid_ref_text(&reference)?;
                let text = if text.is_empty() { " " } else { text.as_str() };
                let actual = value.len().saturating_add(text.len());
                validate_search_text_size(actual)?;
                let start =
                    u32::try_from(value.len()).context("converting search text start offset")?;
                value.push_str(text);
                mapped.push(SearchSegment {
                    start,
                    end: u32::try_from(value.len()).context("converting search text end offset")?,
                    cell: u32::try_from(y * columns + usize::from(x))
                        .context("converting search cell index")?,
                });
                if text.chars().any(|character| !character.is_whitespace()) {
                    meaningful_value_end = value.len();
                    meaningful_map_end = mapped.len();
                }
            }
            if !wrapped {
                value.truncate(meaningful_value_end);
                mapped.truncate(meaningful_map_end);
            }
            if !wrapped && y + 1 < total_rows {
                let actual = value.len().saturating_add(1);
                validate_search_text_size(actual)?;
                let start =
                    u32::try_from(value.len()).context("converting hard-newline search offset")?;
                value.push('\n');
                mapped.push(SearchSegment {
                    start,
                    end: start + 1,
                    // A hard newline leads to the first physical cell of the
                    // following row, which is the useful cursor destination
                    // for a query beginning with `\n`.
                    cell: u32::try_from((y + 1) * columns)
                        .context("converting hard-newline search cell")?,
                });
            }
        }
        Ok(SearchText {
            value,
            segments: mapped,
            columns: self.size.columns,
        })
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
                screen: self.snapshot_current()?,
            })
        })();
        let restore = self.restore_canonical();
        match (result, restore) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(snapshot), Ok(())) => Ok(snapshot),
        }
    }

    fn forward_mouse(&mut self, event: MouseEvent) -> Result<()> {
        let column = event.column.min(self.size.columns - 1);
        let row = event.row.min(self.size.rows - 1);
        let (action, button) = match event.kind {
            MouseEventKind::Press { button } => (mouse::Action::Press, Some(mouse_button(button))),
            MouseEventKind::Release { button } => {
                (mouse::Action::Release, Some(mouse_button(button)))
            }
            MouseEventKind::Motion { button } => (
                mouse::Action::Motion,
                button
                    .filter(|button| event.buttons.contains(*button))
                    .map(mouse_button),
            ),
            MouseEventKind::Wheel { direction } => (
                mouse::Action::Press,
                Some(match direction {
                    MouseWheelDirection::Up => mouse::Button::Four,
                    MouseWheelDirection::Down => mouse::Button::Five,
                }),
            ),
        };
        self.mouse_event
            .set_mods(mouse_modifiers(event.modifiers))
            .set_position(mouse::Position {
                x: f32::from(column) + 0.5,
                y: f32::from(row) + 0.5,
            })
            .set_action(action)
            .set_button(button);
        let sgr_pixels = self.terminal.mode(Mode::SGR_PIXELS_MOUSE)?;
        self.mouse_encoder.set_options_from_terminal(&self.terminal);
        if sgr_pixels {
            // Fut receives terminal-cell coordinates, never surface pixels.
            // Preserve SGR framing while explicitly declining mode 1016.
            self.mouse_encoder.set_format(mouse::Format::Sgr);
        }
        self.mouse_encoder
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
            .set_any_button_pressed(event.buttons.any())
            .set_track_last_cell(true);

        let mut response = Vec::new();
        self.mouse_encoder
            .encode_to_vec(&self.mouse_event, &mut response)?;
        self.write_encoded_input(&response, "mouse input")
    }

    fn forward_alternate_scroll(&mut self, direction: MouseWheelDirection) -> Result<()> {
        self.key_event
            .set_action(key::Action::Press)
            .set_key(match direction {
                MouseWheelDirection::Up => key::Key::ArrowUp,
                MouseWheelDirection::Down => key::Key::ArrowDown,
            })
            .set_mods(key::Mods::empty())
            .set_consumed_mods(key::Mods::empty())
            .set_composing(false)
            .set_utf8(None::<String>)
            .set_unshifted_codepoint('\0');
        self.key_encoder.set_options_from_terminal(&self.terminal);
        let mut response = Vec::with_capacity(64);
        for _ in 0..MOUSE_WHEEL_LINES {
            self.key_encoder
                .encode_to_vec(&self.key_event, &mut response)?;
        }
        self.write_encoded_input(&response, "alternate scroll")
    }

    fn write_encoded_input(&self, response: &[u8], operation: &str) -> Result<()> {
        if response.is_empty() {
            return Ok(());
        }
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY writer lock poisoned"))?;
        writer
            .write_all(response)
            .with_context(|| format!("writing encoded {operation} to PTY"))?;
        writer
            .flush()
            .with_context(|| format!("flushing encoded {operation} to PTY"))
    }

    /// Whether the screen must stay hidden inside an open synchronized-output
    /// frame. Ends an expired frame so the terminal fails open.
    fn holds_synchronized_output(&mut self) -> Result<bool> {
        if self.terminal.mode(Mode::SYNC_OUTPUT)? {
            let started = self
                .synchronized_output_started
                .get_or_insert_with(Instant::now);
            if started.elapsed() < SYNCHRONIZED_OUTPUT_TIMEOUT {
                return Ok(true);
            }
            self.terminal.set_mode(Mode::SYNC_OUTPUT, false)?;
        }
        self.synchronized_output_started = None;
        Ok(false)
    }

    pub(super) fn snapshot(&mut self) -> Result<ScreenSnapshot> {
        self.canonical_snapshot()
    }

    fn canonical_snapshot(&mut self) -> Result<ScreenSnapshot> {
        self.restore_canonical()?;
        self.snapshot_current()
    }

    fn snapshot_current(&mut self) -> Result<ScreenSnapshot> {
        let snapshot = self.render_state.update(&self.terminal)?;
        let visible = snapshot.cursor_visible()?;
        let position = snapshot.cursor_viewport()?;
        let cursor = Cursor {
            column: position.map_or(0, |cursor| cursor.x.min(self.size.columns - 1)),
            row: position.map_or(0, |cursor| cursor.y.min(self.size.rows - 1)),
            visible: visible && position.is_some(),
        };

        let expected = usize::from(self.size.columns) * usize::from(self.size.rows);
        let has_selection = self.terminal.selection()?.is_some();
        let mut result = Vec::with_capacity(expected);
        let mut widths = has_selection.then(|| Vec::with_capacity(expected));
        let mut contents_buffer = String::new();
        let mut rows = self.rows.update(&snapshot)?;
        while let Some(row) = rows.next() {
            // Row-local selection range, fetched once per row instead of
            // once per cell (see `CellIteration::is_selected`'s docs).
            let selection = if has_selection {
                row.selection()?
            } else {
                None
            };
            let mut cells = self.cells.update(row)?;
            let mut column: u16 = 0;
            while let Some(cell) = cells.next() {
                let raw_cell = cell.raw_cell()?;
                let content_tag = raw_cell.content_tag()?;
                let contents = match content_tag {
                    CellContentTag::Codepoint => {
                        let codepoint = raw_cell.codepoint()?;
                        let character = if codepoint == 0 {
                            ' '
                        } else {
                            char::from_u32(codepoint).context("invalid Ghostty cell codepoint")?
                        };
                        let mut encoded = [0; 4];
                        CompactString::new(character.encode_utf8(&mut encoded))
                    }
                    CellContentTag::CodepointGrapheme => {
                        contents_buffer.clear();
                        cell.graphemes_utf8(&mut contents_buffer)?;
                        if contents_buffer.is_empty() {
                            contents_buffer.push(' ');
                        }
                        CompactString::new(&contents_buffer)
                    }
                    CellContentTag::BgColorPalette | CellContentTag::BgColorRgb => " ".into(),
                };
                // `has_styling` is false exactly when the cell's style is
                // default. Build Fut's zero-cost default directly instead of
                // asking Ghostty to materialize its larger default style.
                let mut style = if raw_cell.has_styling()? {
                    let ghostty_style = cell.style()?;
                    CellStyle {
                        foreground: color(ghostty_style.fg_color),
                        background: color(ghostty_style.bg_color),
                        bold: ghostty_style.bold,
                        italic: ghostty_style.italic,
                        underline: ghostty_style.underline != Underline::None,
                        inverse: ghostty_style.inverse,
                    }
                } else {
                    CellStyle::default()
                };
                style.background = match content_tag {
                    CellContentTag::BgColorPalette => {
                        Some(CellColor::Indexed(raw_cell.bg_color_palette()?.0))
                    }
                    CellContentTag::BgColorRgb => {
                        Some(CellColor::Rgb(rgb(raw_cell.bg_color_rgb()?)))
                    }
                    _ => style.background,
                };
                if let Some(widths) = &mut widths {
                    widths.push(raw_cell.wide()?);
                }
                let selected =
                    selection.is_some_and(|range| column >= range.start_x && column <= range.end_x);
                result.push(Cell {
                    contents,
                    style,
                    selected,
                });
                column += 1;
            }
        }
        if let Some(widths) = widths {
            normalize_wide_selection(&mut result, &widths, self.size.columns);
        }
        result.resize(expected, Cell::default());
        result.truncate(expected);
        let revision = self
            .revision
            .checked_add(1)
            .context("terminal snapshot revision exhausted")?;
        // Cells above come straight from libghostty state: each is at most
        // one grapheme cluster (or a single space fallback), with no
        // control characters, so the untrusted-input revalidation that
        // `ScreenSnapshot::new` does is unnecessary here.
        let mut snapshot = ScreenSnapshot::from_terminal(revision, self.size, result, cursor)?;
        snapshot.scroll = self.scroll_position()?;
        self.revision = revision;
        Ok(snapshot)
    }

    fn scroll_position(&mut self) -> Result<crate::domain::ScrollPosition> {
        let scrollbar = self.terminal.scrollbar()?;
        let bottom = scrollbar.total.saturating_sub(scrollbar.len);
        Ok(crate::domain::ScrollPosition {
            offset_from_bottom: usize::try_from(bottom.saturating_sub(scrollbar.offset))
                .context("converting Ghostty scroll offset")?,
            max_offset_from_bottom: usize::try_from(bottom)
                .context("converting Ghostty scrollback size")?,
        })
    }

    fn restore_canonical(&mut self) -> Result<()> {
        self.terminal.set_selection(None)?;
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        Ok(())
    }
}

impl SearchText {
    fn find(
        &self,
        query: &str,
        boundary: SearchBoundary,
        direction: SearchDirection,
    ) -> Option<SearchMatch> {
        let mut first = None;
        let mut last = None;
        let mut candidate = None;
        let mut offset = 0;
        while offset <= self.value.len() {
            let Some(relative) = self.value[offset..].find(query) else {
                break;
            };
            let start = offset + relative;
            let end = start + query.len();
            if let (Some(start_cell), Some(end_cell)) = (
                self.segment_containing(start),
                end.checked_sub(1).and_then(|end| self.cell_containing(end)),
            ) {
                let found = SearchMatch {
                    start: start_cell.point(self.columns),
                    end: end_cell.point(self.columns),
                };
                first.get_or_insert(found);
                last = Some(found);
                let eligible = match (direction, boundary) {
                    (SearchDirection::Forward, SearchBoundary::At(point)) => {
                        !point_before(found.start, point)
                    }
                    (SearchDirection::Forward, SearchBoundary::After(point)) => {
                        point_before(point, found.start)
                    }
                    (SearchDirection::Backward, SearchBoundary::At(point)) => {
                        point_before(found.end, point)
                    }
                    (SearchDirection::Backward, SearchBoundary::Before(point)) => {
                        point_before(found.end, point)
                    }
                    (SearchDirection::Forward, SearchBoundary::Before(_))
                    | (SearchDirection::Backward, SearchBoundary::After(_)) => false,
                };
                if eligible && (direction == SearchDirection::Backward || candidate.is_none()) {
                    candidate = Some(found);
                }
            }
            let advance = self.value[start..].chars().next().map_or(1, char::len_utf8);
            offset = start.saturating_add(advance);
        }
        candidate.or(match direction {
            SearchDirection::Forward => first,
            SearchDirection::Backward => last,
        })
    }

    fn segment_containing(&self, byte: usize) -> Option<&SearchSegment> {
        let byte = u32::try_from(byte).ok()?;
        let index = self.segments.partition_point(|segment| segment.end <= byte);
        self.segments
            .get(index)
            .filter(|segment| byte >= segment.start && byte < segment.end)
    }

    fn cell_containing(&self, byte: usize) -> Option<&SearchSegment> {
        self.segment_containing(byte)
    }
}

impl SearchSegment {
    fn point(self, columns: u16) -> PointCoordinate {
        let columns = u32::from(columns);
        PointCoordinate {
            x: (self.cell % columns) as u16,
            y: self.cell / columns,
        }
    }
}

fn point_before(left: PointCoordinate, right: PointCoordinate) -> bool {
    (left.y, left.x) < (right.y, right.x)
}

fn validate_copy_size(actual: usize) -> std::result::Result<(), CopyModeError> {
    if actual > MAX_COPY_BYTES {
        Err(CopyModeError::CopyTooLarge {
            actual,
            maximum: MAX_COPY_BYTES,
        })
    } else {
        Ok(())
    }
}

fn validate_copy_cells(actual: usize) -> std::result::Result<(), CopyModeError> {
    if actual > MAX_COPY_CELLS {
        Err(CopyModeError::CopySpanTooLarge {
            actual,
            maximum: MAX_COPY_CELLS,
        })
    } else {
        Ok(())
    }
}

fn selection_cell_span(
    left: PointCoordinate,
    right: PointCoordinate,
    columns: u16,
) -> std::result::Result<usize, CopyModeFailure> {
    let (start, end) = if point_before(right, left) {
        (right, left)
    } else {
        (left, right)
    };
    let columns = usize::from(columns);
    let index = |point: PointCoordinate| {
        usize::try_from(point.y)
            .ok()
            .and_then(|row| row.checked_mul(columns))
            .and_then(|row| row.checked_add(usize::from(point.x)))
    };
    let Some(start) = index(start) else {
        return Err(CopyModeError::CopySpanTooLarge {
            actual: usize::MAX,
            maximum: MAX_COPY_CELLS,
        }
        .into());
    };
    let Some(end) = index(end) else {
        return Err(CopyModeError::CopySpanTooLarge {
            actual: usize::MAX,
            maximum: MAX_COPY_CELLS,
        }
        .into());
    };
    end.checked_sub(start)
        .and_then(|span| span.checked_add(1))
        .ok_or_else(|| {
            CopyModeError::CopySpanTooLarge {
                actual: usize::MAX,
                maximum: MAX_COPY_CELLS,
            }
            .into()
        })
}

fn validate_search_query(query: &str) -> std::result::Result<(), CopyModeError> {
    if query.is_empty() {
        Err(CopyModeError::EmptySearchQuery)
    } else if query.len() > MAX_SEARCH_QUERY_BYTES {
        Err(CopyModeError::SearchQueryTooLarge {
            actual: query.len(),
            maximum: MAX_SEARCH_QUERY_BYTES,
        })
    } else {
        Ok(())
    }
}

fn validate_search_cells(actual: usize) -> std::result::Result<(), CopyModeError> {
    if actual > MAX_SEARCH_CELLS {
        Err(CopyModeError::SearchSpaceTooLarge {
            actual,
            maximum: MAX_SEARCH_CELLS,
        })
    } else {
        Ok(())
    }
}

fn validate_search_text_size(actual: usize) -> std::result::Result<(), CopyModeError> {
    if actual > MAX_SEARCH_TEXT_BYTES {
        Err(CopyModeError::SearchTextTooLarge {
            actual,
            maximum: MAX_SEARCH_TEXT_BYTES,
        })
    } else {
        Ok(())
    }
}

fn validate_search_cell_size(actual: usize) -> std::result::Result<(), CopyModeError> {
    if actual > MAX_SEARCH_CELL_CODEPOINTS {
        Err(CopyModeError::SearchCellTooLarge {
            actual,
            maximum: MAX_SEARCH_CELL_CODEPOINTS,
        })
    } else {
        Ok(())
    }
}

fn grid_ref_text(reference: &GridRef<'_>) -> std::result::Result<String, CopyModeFailure> {
    let mut graphemes = vec!['\0'; 4];
    let length = match reference.graphemes(&mut graphemes) {
        Ok(length) => length,
        Err(GhosttyError::OutOfSpace { required }) => {
            validate_search_cell_size(required)?;
            graphemes.resize(required, '\0');
            reference.graphemes(&mut graphemes)?
        }
        Err(error) => return Err(error.into()),
    };
    graphemes.truncate(length);
    Ok(graphemes.into_iter().collect())
}

fn normalize_wide_selection(cells: &mut [Cell], widths: &[CellWide], columns: u16) {
    let len = cells.len().min(widths.len());
    for index in 0..len.saturating_sub(1) {
        if widths[index] == CellWide::Wide && widths[index + 1] == CellWide::SpacerTail {
            let selected = cells[index].selected || cells[index + 1].selected;
            cells[index].selected = selected;
            cells[index + 1].selected = selected;
        }
        if widths[index] == CellWide::SpacerHead
            && (index + 1).is_multiple_of(usize::from(columns))
            && index + 2 < len
            && widths[index + 1] == CellWide::Wide
            && widths[index + 2] == CellWide::SpacerTail
        {
            let selected = cells[index..=index + 2].iter().any(|cell| cell.selected);
            for cell in &mut cells[index..=index + 2] {
                cell.selected = selected;
            }
        }
    }
}

fn mouse_modifiers(modifiers: MouseModifiers) -> key::Mods {
    let mut result = key::Mods::empty();
    result.set(key::Mods::SHIFT, modifiers.shift);
    result.set(key::Mods::CTRL, modifiers.control);
    result.set(key::Mods::ALT, modifiers.alt);
    result
}

fn mouse_button(button: MouseButton) -> mouse::Button {
    match button {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Middle => mouse::Button::Middle,
        MouseButton::Right => mouse::Button::Right,
    }
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
    use crate::domain::MouseButtons;

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

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: MouseModifiers::default(),
            buttons: Default::default(),
        }
    }

    #[test]
    fn output_sources_are_bounded_unicode_exact_and_optionally_ansi() {
        let mut terminal = terminal(6, 2);
        terminal.feed("first\r\n雪red\r\nlast".as_bytes()).unwrap();

        let visible = terminal
            .output(TerminalOutputSource::Visible, 1, false)
            .unwrap();
        assert_eq!(visible.requested_rows, 2);
        assert_eq!(visible.returned_rows, 2);
        assert!(visible.truncated);
        assert!(visible.text.contains("雪red"));
        assert!(visible.text.contains("last"));

        let recent = terminal
            .output(TerminalOutputSource::Recent, 1, false)
            .unwrap();
        assert_eq!(recent.returned_rows, 1);
        assert!(recent.truncated);
        assert_eq!(recent.text.trim(), "last");

        terminal.feed(b"\r\n\x1b[31mstyled\x1b[0m").unwrap();
        let ansi = terminal
            .output(TerminalOutputSource::Recent, 1, true)
            .unwrap();
        assert!(ansi.ansi);
        assert!(ansi.text.contains("styled"));
        assert!(ansi.text.contains("\x1b["));
    }

    #[test]
    fn unwrapped_output_marks_a_window_starting_inside_a_soft_wrap() {
        let mut terminal = terminal(4, 2);
        terminal.feed(b"abcdefgh").unwrap();
        let output = terminal
            .output(TerminalOutputSource::RecentUnwrapped, 1, false)
            .unwrap();
        assert!(output.truncated);
        assert!(output.starts_mid_logical_line);
        assert_eq!(output.text.trim(), "efgh");
    }

    #[test]
    fn historical_output_rejects_alternate_screen_and_invalid_bounds() {
        let mut alternate = terminal(8, 2);
        assert_eq!(
            alternate.output(TerminalOutputSource::Recent, 0, false),
            Err(OutputCaptureError::InvalidRows)
        );
        alternate.feed(b"\x1b[?1049halternate").unwrap();
        assert_eq!(
            alternate.output(TerminalOutputSource::RecentUnwrapped, 2, false),
            Err(OutputCaptureError::AlternateScreen)
        );
        let visible = alternate
            .output(TerminalOutputSource::Visible, 2, false)
            .unwrap();
        assert_eq!(visible.text.replace('\n', ""), "alternate");
    }

    fn wheel(direction: MouseWheelDirection, column: u16, row: u16) -> MouseEvent {
        mouse_event(MouseEventKind::Wheel { direction }, column, row)
    }

    fn take_output(output: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
        std::mem::take(&mut *output.lock().unwrap())
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

        let MouseInputOutcome::Scrolled(history) = terminal
            .mouse_input(wheel(MouseWheelDirection::Up, 0, 0), None, true)
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
    fn overscroll_wheels_are_dropped_without_building_snapshots() {
        let mut terminal = terminal(8, 3);
        let bottom = terminal
            .feed(b"00\r\n01\r\n02\r\n03\r\n04\r\n05")
            .unwrap()
            .unwrap();

        // Wheel down at the bottom has nowhere to go.
        assert!(matches!(
            terminal
                .mouse_input(wheel(MouseWheelDirection::Down, 0, 0), None, true)
                .unwrap(),
            MouseInputOutcome::Handled
        ));

        // One wheel up from the bottom reaches the top of six lines.
        let MouseInputOutcome::Scrolled(top) = terminal
            .mouse_input(wheel(MouseWheelDirection::Up, 0, 0), None, true)
            .unwrap()
        else {
            panic!("wheel up from the bottom must scroll");
        };
        assert_eq!(top.offset, Some(0));

        // Wheel up at the top has nowhere to go either.
        assert!(matches!(
            terminal
                .mouse_input(wheel(MouseWheelDirection::Up, 0, 0), Some(0), true)
                .unwrap(),
            MouseInputOutcome::Handled
        ));

        // Scrolling back down from the top still works.
        assert!(matches!(
            terminal
                .mouse_input(wheel(MouseWheelDirection::Down, 0, 0), Some(0), true)
                .unwrap(),
            MouseInputOutcome::Scrolled(_)
        ));

        // Dropped wheels build no snapshots: only the two real scrolls and
        // this canonical snapshot advance the revision.
        assert_eq!(terminal.snapshot().unwrap().revision, bottom.revision + 3);
    }

    #[test]
    fn wheel_without_scrollback_is_dropped_in_both_directions() {
        let mut terminal = terminal(8, 3);
        terminal.feed(b"hi").unwrap();
        for direction in [MouseWheelDirection::Up, MouseWheelDirection::Down] {
            assert!(matches!(
                terminal
                    .mouse_input(wheel(direction, 0, 0), None, true)
                    .unwrap(),
                MouseInputOutcome::Handled
            ));
        }
    }

    #[test]
    fn tracked_wheel_uses_ghostty_modes_and_is_forwarded_to_the_pty() {
        let (mut terminal, output) = recording_terminal(10, 4);
        terminal.feed(b"\x1b[?1000h\x1b[?1006h").unwrap().unwrap();
        output.lock().unwrap().clear();

        let outcome = terminal
            .mouse_input(
                MouseEvent {
                    modifiers: MouseModifiers {
                        shift: true,
                        control: false,
                        alt: false,
                    },
                    ..wheel(MouseWheelDirection::Up, 2, 1)
                },
                None,
                true,
            )
            .unwrap();
        assert!(matches!(outcome, MouseInputOutcome::Handled));
        assert_eq!(
            String::from_utf8(output.lock().unwrap().clone()).unwrap(),
            "\x1b[<68;3;2M"
        );
        output.lock().unwrap().clear();
        let outcome = terminal
            .mouse_input(wheel(MouseWheelDirection::Down, 2, 1), None, false)
            .unwrap();
        assert!(matches!(outcome, MouseInputOutcome::Handled));
        assert!(output.lock().unwrap().is_empty());
        assert!(terminal.terminal.viewport_active().unwrap());
    }

    #[test]
    fn sgr_mouse_encodes_all_buttons_releases_coordinates_and_modifiers_exactly() {
        let (mut terminal, output) = recording_terminal(10, 4);
        terminal.feed(b"\x1b[?1000h\x1b[?1006h").unwrap().unwrap();
        take_output(&output);

        let cases = [
            (
                MouseEvent {
                    modifiers: MouseModifiers {
                        shift: true,
                        control: true,
                        alt: true,
                    },
                    buttons: MouseButtons {
                        left: true,
                        ..Default::default()
                    },
                    ..mouse_event(
                        MouseEventKind::Press {
                            button: MouseButton::Left,
                        },
                        2,
                        1,
                    )
                },
                "\x1b[<28;3;2M",
            ),
            (
                mouse_event(
                    MouseEventKind::Press {
                        button: MouseButton::Middle,
                    },
                    3,
                    2,
                ),
                "\x1b[<1;4;3M",
            ),
            (
                mouse_event(
                    MouseEventKind::Release {
                        button: MouseButton::Middle,
                    },
                    3,
                    2,
                ),
                "\x1b[<1;4;3m",
            ),
            (
                mouse_event(
                    MouseEventKind::Press {
                        button: MouseButton::Right,
                    },
                    u16::MAX,
                    u16::MAX,
                ),
                "\x1b[<2;10;4M",
            ),
            (
                mouse_event(
                    MouseEventKind::Release {
                        button: MouseButton::Right,
                    },
                    u16::MAX,
                    u16::MAX,
                ),
                "\x1b[<2;10;4m",
            ),
        ];
        for (event, expected) in cases {
            assert!(matches!(
                terminal.mouse_input(event, None, true).unwrap(),
                MouseInputOutcome::Handled
            ));
            assert_eq!(String::from_utf8(take_output(&output)).unwrap(), expected);
        }
    }

    #[test]
    fn sgr_pixels_mode_is_explicitly_downgraded_to_cell_sgr_bytes() {
        let (mut terminal, output) = recording_terminal(10, 4);
        terminal.feed(b"\x1b[?1000h\x1b[?1016h").unwrap().unwrap();
        take_output(&output);
        assert!(terminal.terminal.mode(Mode::SGR_PIXELS_MOUSE).unwrap());

        for (event, expected) in [
            (
                MouseEvent {
                    buttons: MouseButtons {
                        left: true,
                        ..Default::default()
                    },
                    ..mouse_event(
                        MouseEventKind::Press {
                            button: MouseButton::Left,
                        },
                        7,
                        2,
                    )
                },
                b"\x1b[<0;8;3M".as_slice(),
            ),
            (
                mouse_event(
                    MouseEventKind::Release {
                        button: MouseButton::Left,
                    },
                    7,
                    2,
                ),
                b"\x1b[<0;8;3m".as_slice(),
            ),
        ] {
            assert!(matches!(
                terminal.mouse_input(event, None, true).unwrap(),
                MouseInputOutcome::Handled
            ));
            assert_eq!(take_output(&output), expected);
        }
    }

    #[test]
    fn ghostty_tracking_modes_filter_drag_and_requested_motion_exactly() {
        let drag = MouseEvent {
            buttons: MouseButtons {
                left: true,
                ..Default::default()
            },
            ..mouse_event(
                MouseEventKind::Motion {
                    button: Some(MouseButton::Left),
                },
                2,
                1,
            )
        };
        let hover = mouse_event(MouseEventKind::Motion { button: None }, 4, 2);

        let (mut x10, x10_output) = recording_terminal(10, 4);
        x10.feed(b"\x1b[?9h").unwrap().unwrap();
        take_output(&x10_output);
        x10.mouse_input(
            mouse_event(
                MouseEventKind::Press {
                    button: MouseButton::Left,
                },
                0,
                0,
            ),
            None,
            true,
        )
        .unwrap();
        assert_eq!(take_output(&x10_output), b"\x1b[M !!");
        x10.mouse_input(
            mouse_event(
                MouseEventKind::Release {
                    button: MouseButton::Left,
                },
                0,
                0,
            ),
            None,
            true,
        )
        .unwrap();
        assert!(take_output(&x10_output).is_empty());

        let (mut normal, normal_output) = recording_terminal(10, 4);
        normal.feed(b"\x1b[?1000h\x1b[?1006h").unwrap().unwrap();
        take_output(&normal_output);
        normal.mouse_input(drag, None, true).unwrap();
        normal.mouse_input(hover, None, true).unwrap();
        assert!(take_output(&normal_output).is_empty());

        let (mut button, button_output) = recording_terminal(10, 4);
        button.feed(b"\x1b[?1002h\x1b[?1006h").unwrap().unwrap();
        take_output(&button_output);
        button.mouse_input(hover, None, true).unwrap();
        assert!(take_output(&button_output).is_empty());
        button
            .mouse_input(
                MouseEvent {
                    buttons: MouseButtons::default(),
                    ..drag
                },
                None,
                true,
            )
            .unwrap();
        assert!(take_output(&button_output).is_empty());
        button.mouse_input(drag, None, true).unwrap();
        assert_eq!(take_output(&button_output), b"\x1b[<32;3;2M");
        for (button_id, column, expected) in [
            (MouseButton::Middle, 3, b"\x1b[<33;4;2M".as_slice()),
            (MouseButton::Right, 4, b"\x1b[<34;5;2M".as_slice()),
        ] {
            let mut buttons = MouseButtons::default();
            buttons.set(button_id, true);
            button
                .mouse_input(
                    MouseEvent {
                        buttons,
                        ..mouse_event(
                            MouseEventKind::Motion {
                                button: Some(button_id),
                            },
                            column,
                            1,
                        )
                    },
                    None,
                    true,
                )
                .unwrap();
            assert_eq!(take_output(&button_output), expected);
        }

        let (mut any, any_output) = recording_terminal(10, 4);
        any.feed(b"\x1b[?1003h\x1b[?1006h").unwrap().unwrap();
        take_output(&any_output);
        any.mouse_input(hover, None, true).unwrap();
        assert_eq!(take_output(&any_output), b"\x1b[<35;5;3M");
    }

    #[test]
    fn alternate_scroll_precedence_uses_repeated_terminal_key_encoding() {
        let (mut terminal, output) = recording_terminal(10, 4);
        terminal.feed(b"\x1b[?1049h").unwrap().unwrap();
        take_output(&output);

        assert!(matches!(
            terminal
                .mouse_input(wheel(MouseWheelDirection::Up, 2, 1), None, true)
                .unwrap(),
            MouseInputOutcome::Handled
        ));
        assert_eq!(take_output(&output), b"\x1b[A\x1b[A\x1b[A");

        terminal.feed(b"\x1b[?1h").unwrap().unwrap();
        take_output(&output);
        terminal
            .mouse_input(wheel(MouseWheelDirection::Down, 2, 1), None, true)
            .unwrap();
        assert_eq!(take_output(&output), b"\x1bOB\x1bOB\x1bOB");

        terminal.feed(b"\x1b[?1000h\x1b[?1006h").unwrap().unwrap();
        take_output(&output);
        terminal
            .mouse_input(wheel(MouseWheelDirection::Up, 2, 1), None, true)
            .unwrap();
        assert_eq!(take_output(&output), b"\x1b[<64;3;2M");

        terminal.feed(b"\x1b[?1000l\x1b[?1007l").unwrap().unwrap();
        take_output(&output);
        // Without alternate scroll the wheel is consumed locally; at the
        // bottom of the alternate screen that means dropping it outright.
        assert!(matches!(
            terminal
                .mouse_input(wheel(MouseWheelDirection::Down, 2, 1), None, true)
                .unwrap(),
            MouseInputOutcome::Handled
        ));
        assert!(take_output(&output).is_empty());
    }

    #[test]
    fn pty_mouse_paths_are_silent_when_input_is_not_allowed() {
        let (mut terminal, output) = recording_terminal(10, 4);
        terminal
            .feed(b"\x1b[?1049h\x1b[?1003h\x1b[?1006h")
            .unwrap()
            .unwrap();
        take_output(&output);

        for event in [
            mouse_event(
                MouseEventKind::Press {
                    button: MouseButton::Left,
                },
                1,
                1,
            ),
            mouse_event(MouseEventKind::Motion { button: None }, 2, 1),
            wheel(MouseWheelDirection::Up, 2, 1),
        ] {
            assert!(matches!(
                terminal.mouse_input(event, None, false).unwrap(),
                MouseInputOutcome::Handled
            ));
        }
        assert!(take_output(&output).is_empty());

        terminal.feed(b"\x1b[?1003l").unwrap().unwrap();
        take_output(&output);
        terminal
            .mouse_input(wheel(MouseWheelDirection::Up, 2, 1), None, false)
            .unwrap();
        assert!(take_output(&output).is_empty());
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
    fn paste_and_input_appends_key_bytes_after_the_complete_bracketed_paste() {
        let (mut terminal, output) = recording_terminal(10, 4);
        terminal.feed(b"\x1b[?2004h").unwrap().unwrap();

        terminal.paste_and_input("echo 雪".into(), b"\r").unwrap();

        assert_eq!(
            *output.lock().unwrap(),
            b"\x1b[200~echo \xe9\x9b\xaa\x1b[201~\r".to_vec()
        );
    }

    fn copy_action(
        terminal: &mut GhosttyTerminal,
        owner: ClientId,
        action: CopyModeAction,
    ) -> CopyModeOutcome {
        terminal.copy_mode(owner, action, None).unwrap()
    }

    fn active_screen(outcome: CopyModeOutcome) -> ScreenSnapshot {
        let CopyModeOutcome::Active(viewport) = outcome else {
            panic!("expected active copy-mode outcome")
        };
        viewport.screen
    }

    fn copy_and_finalize(terminal: &mut GhosttyTerminal, owner: ClientId) -> String {
        let CopyModeOutcome::Prepared { copy_id, text } =
            copy_action(terminal, owner, CopyModeAction::Copy)
        else {
            panic!("expected prepared copy-mode outcome")
        };
        assert!(matches!(
            copy_action(terminal, owner, CopyModeAction::FinalizeCopy { copy_id },),
            CopyModeOutcome::Finalized { .. }
        ));
        text
    }

    #[test]
    fn two_copy_owners_select_render_and_copy_independently() {
        let mut terminal = terminal(12, 3);
        terminal.feed(b"alpha\r\nbeta").unwrap().unwrap();
        let owner_a = ClientId::new();
        let owner_b = ClientId::new();

        copy_action(&mut terminal, owner_a, CopyModeAction::Begin);
        copy_action(
            &mut terminal,
            owner_a,
            CopyModeAction::Move {
                movement: CopyModeMovement::BeginningOfLine,
            },
        );
        copy_action(&mut terminal, owner_a, CopyModeAction::ToggleSelection);
        let selected_a = active_screen(copy_action(
            &mut terminal,
            owner_a,
            CopyModeAction::Move {
                movement: CopyModeMovement::EndOfLine,
            },
        ));

        copy_action(&mut terminal, owner_b, CopyModeAction::Begin);
        copy_action(
            &mut terminal,
            owner_b,
            CopyModeAction::Move {
                movement: CopyModeMovement::Up,
            },
        );
        copy_action(
            &mut terminal,
            owner_b,
            CopyModeAction::Move {
                movement: CopyModeMovement::BeginningOfLine,
            },
        );
        copy_action(&mut terminal, owner_b, CopyModeAction::ToggleSelection);
        let selected_b = active_screen(copy_action(
            &mut terminal,
            owner_b,
            CopyModeAction::Move {
                movement: CopyModeMovement::EndOfLine,
            },
        ));

        assert!(selected_a.cells[12..24].iter().any(|cell| cell.selected));
        assert!(selected_a.cells[..12].iter().all(|cell| !cell.selected));
        assert!(selected_b.cells[..12].iter().any(|cell| cell.selected));
        assert!(selected_b.cells[12..24].iter().all(|cell| !cell.selected));
        assert_eq!(copy_and_finalize(&mut terminal, owner_a), "beta");
        assert!(terminal.copy_modes.contains_key(&owner_b));
        assert_eq!(copy_and_finalize(&mut terminal, owner_b), "alpha");
        assert!(terminal.copy_modes.is_empty());
    }

    #[test]
    fn canonical_snapshots_never_retain_a_client_selection_or_viewport() {
        let mut terminal = terminal(8, 2);
        terminal.feed(b"zero\r\none\r\ntwo").unwrap().unwrap();
        let owner = ClientId::new();
        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        copy_action(&mut terminal, owner, CopyModeAction::ToggleSelection);
        let selected = active_screen(copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Move {
                movement: CopyModeMovement::PageUp,
            },
        ));
        assert!(selected.cells.iter().any(|cell| cell.selected));

        let canonical = terminal.snapshot().unwrap();
        assert!(canonical.cells.iter().all(|cell| !cell.selected));
        assert!(terminal.terminal.selection().unwrap().is_none());
        assert!(terminal.terminal.viewport_active().unwrap());
    }

    #[test]
    fn literal_search_handles_unicode_soft_wrap_and_bidirectional_repeat() {
        let mut terminal = terminal(5, 4);
        terminal
            .feed("first\r\nab雪cd\r\nneedle\r\nneedle".as_bytes())
            .unwrap()
            .unwrap();
        let owner = ClientId::new();
        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Search {
                query: "雪cd".into(),
            },
        );
        copy_action(&mut terminal, owner, CopyModeAction::ToggleSelection);
        // Physical-cell movement crosses the wide glyph's spacer tail before
        // reaching both following narrow cells.
        for _ in 0..3 {
            copy_action(
                &mut terminal,
                owner,
                CopyModeAction::Move {
                    movement: CopyModeMovement::Right,
                },
            );
        }
        assert_eq!(copy_and_finalize(&mut terminal, owner), "雪cd");

        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Search {
                query: "needle".into(),
            },
        );
        let first = terminal.copy_modes[&owner]
            .search
            .as_ref()
            .unwrap()
            .start
            .point(PointSpace::Screen)
            .unwrap()
            .unwrap();
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::RepeatSearch {
                direction: SearchDirection::Forward,
            },
        );
        let second = terminal.copy_modes[&owner]
            .search
            .as_ref()
            .unwrap()
            .start
            .point(PointSpace::Screen)
            .unwrap()
            .unwrap();
        assert_ne!(first, second);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::RepeatSearch {
                direction: SearchDirection::Backward,
            },
        );
        assert_eq!(
            terminal.copy_modes[&owner]
                .search
                .as_ref()
                .unwrap()
                .start
                .point(PointSpace::Screen)
                .unwrap(),
            Some(first)
        );
    }

    #[test]
    fn tracked_copy_refs_survive_output_scroll_and_reflow() {
        let mut terminal = terminal(10, 3);
        terminal.feed(b"keep").unwrap().unwrap();
        let owner = ClientId::new();
        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Move {
                movement: CopyModeMovement::BeginningOfLine,
            },
        );
        copy_action(&mut terminal, owner, CopyModeAction::ToggleSelection);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Move {
                movement: CopyModeMovement::EndOfLine,
            },
        );

        terminal
            .feed(b"\r\none\r\ntwo\r\nthree\r\nfour")
            .unwrap()
            .unwrap();
        terminal
            .resize(TerminalSize {
                columns: 5,
                rows: 4,
            })
            .unwrap();
        let selected = terminal.copy_mode_snapshot(owner, None).unwrap();
        assert!(selected.screen.cells.iter().any(|cell| cell.selected));
        assert_eq!(copy_and_finalize(&mut terminal, owner), "keep");
    }

    #[test]
    fn copy_revalidates_a_selection_expanded_by_reflow() {
        const HARD_LINES: usize = 1_005;

        let mut terminal = terminal(249, 2);
        let mut contents = String::with_capacity(HARD_LINES * 3);
        for line in 0..HARD_LINES {
            if line > 0 {
                contents.push_str("\r\n");
            }
            contents.push('x');
        }
        terminal.feed(contents.as_bytes()).unwrap().unwrap();

        let owner = ClientId::new();
        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Move {
                movement: CopyModeMovement::BeginningOfLine,
            },
        );
        copy_action(&mut terminal, owner, CopyModeAction::ToggleSelection);
        terminal
            .copy_modes
            .get_mut(&owner)
            .unwrap()
            .cursor
            .set(
                &mut terminal.terminal,
                Point::Screen(PointCoordinate { x: 0, y: 0 }),
            )
            .unwrap();

        let selected = terminal.copy_mode_snapshot(owner, None).unwrap();
        assert!(selected.screen.cells.iter().any(|cell| cell.selected));
        let before = terminal
            .copy_selection_cell_span(&terminal.copy_modes[&owner])
            .unwrap();
        assert!(before <= MAX_COPY_CELLS);

        terminal
            .resize(TerminalSize {
                columns: 1_100,
                rows: 2,
            })
            .unwrap();
        let expanded = terminal
            .copy_selection_cell_span(&terminal.copy_modes[&owner])
            .unwrap();
        assert!(
            expanded > MAX_COPY_CELLS,
            "selection span changed from {before} to {expanded} cells"
        );
        assert!(matches!(
            terminal.copy_mode(owner, CopyModeAction::Copy, None),
            Err(CopyModeFailure::Semantic(CopyModeError::CopySpanTooLarge {
                actual,
                maximum: MAX_COPY_CELLS,
            })) if actual == expanded
        ));
    }

    #[test]
    fn client_cleanup_releases_only_that_owners_tracked_state() {
        let mut terminal = terminal(8, 2);
        terminal.feed(b"content").unwrap().unwrap();
        let owner_a = ClientId::new();
        let owner_b = ClientId::new();
        copy_action(&mut terminal, owner_a, CopyModeAction::Begin);
        copy_action(&mut terminal, owner_b, CopyModeAction::Begin);

        terminal.clear_copy_mode(owner_a).unwrap();
        assert!(matches!(
            terminal.copy_mode(
                owner_a,
                CopyModeAction::Move {
                    movement: CopyModeMovement::Left,
                },
                None,
            ),
            Err(CopyModeFailure::Semantic(CopyModeError::NotActive))
        ));
        assert!(terminal.copy_modes.contains_key(&owner_b));
        assert!(terminal.terminal.selection().unwrap().is_none());
    }

    #[test]
    fn failed_cleanup_keeps_copy_ownership_for_a_retry() {
        let mut terminal = terminal(8, 2);
        terminal.feed(b"content").unwrap().unwrap();
        let owner = ClientId::new();
        copy_action(&mut terminal, owner, CopyModeAction::Begin);

        terminal.revision = u64::MAX;
        assert!(terminal.clear_copy_mode(owner).is_err());
        assert!(terminal.copy_modes.contains_key(&owner));
        assert!(terminal.terminal.selection().unwrap().is_none());
        assert!(terminal.terminal.viewport_active().unwrap());

        terminal.revision = 10;
        let canonical = terminal.clear_copy_mode(owner).unwrap().unwrap();
        assert!(canonical.cells.iter().all(|cell| !cell.selected));
        assert!(!terminal.copy_modes.contains_key(&owner));
    }

    #[test]
    fn failed_begin_rollback_and_cursor_invalidation_remain_retryable() {
        let owner = ClientId::new();
        let mut begin = terminal(8, 2);
        begin.revision = u64::MAX;
        assert!(matches!(
            begin.copy_mode(owner, CopyModeAction::Begin, None),
            Err(CopyModeFailure::Emulator(_))
        ));
        assert!(begin.copy_modes.contains_key(&owner));
        begin.revision = 0;
        begin.clear_copy_mode(owner).unwrap();

        let owner = ClientId::new();
        let mut invalidated = terminal(8, 2);
        invalidated.feed(b"primary").unwrap().unwrap();
        copy_action(&mut invalidated, owner, CopyModeAction::Begin);
        invalidated.terminal.vt_write(b"\x1b[?1049halternate");
        invalidated.revision = u64::MAX;
        assert!(matches!(
            invalidated.copy_mode(
                owner,
                CopyModeAction::Move {
                    movement: CopyModeMovement::Left,
                },
                None,
            ),
            Err(CopyModeFailure::CursorLost {
                canonical: None,
                cleanup_error: Some(_),
            })
        ));
        assert!(invalidated.copy_modes.contains_key(&owner));

        invalidated.revision = 10;
        assert!(matches!(
            invalidated.copy_mode(
                owner,
                CopyModeAction::Move {
                    movement: CopyModeMovement::Left,
                },
                None,
            ),
            Err(CopyModeFailure::CursorLost {
                canonical: Some(_),
                cleanup_error: None,
            })
        ));
        assert!(!invalidated.copy_modes.contains_key(&owner));
    }

    #[test]
    fn copy_mode_begins_inside_the_clients_historical_viewport() {
        let mut terminal = terminal(8, 3);
        terminal
            .feed(b"00\r\n01\r\n02\r\n03\r\n04\r\n05")
            .unwrap()
            .unwrap();
        let MouseInputOutcome::Scrolled(history) = terminal
            .mouse_input(wheel(MouseWheelDirection::Up, 0, 0), None, true)
            .unwrap()
        else {
            panic!("history wheel was forwarded")
        };
        let owner = ClientId::new();
        let CopyModeOutcome::Active(copy) = terminal
            .copy_mode(owner, CopyModeAction::Begin, history.offset)
            .unwrap()
        else {
            panic!("copy mode did not become active")
        };

        assert_eq!(copy.offset, history.offset);
        assert!(!text(&copy.screen).contains("05"));
        assert!(copy.screen.cells.iter().any(|cell| cell.selected));
    }

    #[test]
    fn alternate_screen_change_invalidates_refs_and_returns_a_canonical_snapshot() {
        let mut terminal = terminal(10, 2);
        terminal.feed(b"primary").unwrap().unwrap();
        let owner = ClientId::new();
        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        terminal.feed(b"\x1b[?1049halternate").unwrap().unwrap();

        let error = match terminal.copy_mode(
            owner,
            CopyModeAction::Move {
                movement: CopyModeMovement::Left,
            },
            None,
        ) {
            Err(error) => error,
            Ok(_) => panic!("screen change did not invalidate copy mode"),
        };
        let CopyModeFailure::CursorLost {
            canonical: Some(canonical),
            cleanup_error: None,
        } = error
        else {
            panic!("screen change did not return semantic cursor loss: {error:?}")
        };
        assert!(canonical.cells.iter().all(|cell| !cell.selected));
        assert!(!terminal.copy_modes.contains_key(&owner));

        let primary = terminal.feed(b"\x1b[?1049l").unwrap().unwrap();
        assert!(primary.cells.iter().all(|cell| !cell.selected));
    }

    #[test]
    fn reset_and_scrollback_pruning_invalidate_tracked_copy_refs() {
        let owner = ClientId::new();
        let mut reset = terminal(8, 2);
        reset.feed(b"reset me").unwrap().unwrap();
        copy_action(&mut reset, owner, CopyModeAction::Begin);
        reset.terminal.reset();
        assert!(matches!(
            reset.copy_mode(
                owner,
                CopyModeAction::Move {
                    movement: CopyModeMovement::Right,
                },
                None,
            ),
            Err(CopyModeFailure::CursorLost { .. })
        ));
        assert!(!reset.copy_modes.contains_key(&owner));

        let owner = ClientId::new();
        let mut pruned = terminal(4, 2);
        pruned.feed(b"old").unwrap().unwrap();
        copy_action(&mut pruned, owner, CopyModeAction::Begin);
        let mut output = String::new();
        for index in 0..10_050 {
            use std::fmt::Write as _;
            writeln!(&mut output, "{index:04}").unwrap();
        }
        pruned.feed(output.as_bytes()).unwrap().unwrap();
        assert!(matches!(
            pruned.copy_mode(
                owner,
                CopyModeAction::Move {
                    movement: CopyModeMovement::Right,
                },
                None,
            ),
            Err(CopyModeFailure::CursorLost { .. })
        ));
        assert!(!pruned.copy_modes.contains_key(&owner));
    }

    #[test]
    fn search_scrollback_strips_padding_and_maps_hard_newlines() {
        let mut terminal = terminal(12, 3);
        terminal
            .feed(b"old-target\r\nrow-01\r\nrow-02\r\nrow-03\r\nrow-04")
            .unwrap()
            .unwrap();
        let owner = ClientId::new();
        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Search {
                query: "old-target\nrow-01".into(),
            },
        );

        let point = terminal.copy_modes[&owner]
            .search
            .as_ref()
            .unwrap()
            .start
            .point(PointSpace::Screen)
            .unwrap()
            .unwrap();
        assert_eq!(point.x, 0);
        assert!(point.y < terminal.terminal.total_rows().unwrap() as u32 - 3);

        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Search {
                query: "\nrow-02".into(),
            },
        );
        assert_eq!(
            terminal.copy_modes[&owner]
                .search
                .as_ref()
                .unwrap()
                .start
                .point(PointSpace::Screen)
                .unwrap()
                .unwrap()
                .x,
            0
        );
    }

    #[test]
    fn unicode_repeat_skips_the_whole_combining_or_zwj_cell_and_no_match_recovers() {
        let mut terminal = terminal(30, 2);
        terminal
            .feed("e\u{301}x e\u{301}y 👩\u{200d}💻a 👩\u{200d}💻b".as_bytes())
            .unwrap()
            .unwrap();
        let owner = ClientId::new();
        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Search {
                query: "\u{301}".into(),
            },
        );
        let first = terminal.copy_modes[&owner]
            .search
            .as_ref()
            .unwrap()
            .start
            .point(PointSpace::Screen)
            .unwrap()
            .unwrap();
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::RepeatSearch {
                direction: SearchDirection::Forward,
            },
        );
        let second = terminal.copy_modes[&owner]
            .search
            .as_ref()
            .unwrap()
            .start
            .point(PointSpace::Screen)
            .unwrap()
            .unwrap();
        assert_ne!(first, second);

        let before_no_match = terminal.copy_cursor_point(owner).unwrap();
        assert!(matches!(
            terminal.copy_mode(
                owner,
                CopyModeAction::Search {
                    query: "not present".into(),
                },
                None,
            ),
            Err(CopyModeFailure::Semantic(CopyModeError::NoMatch))
        ));
        assert_eq!(terminal.copy_cursor_point(owner).unwrap(), before_no_match);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Search {
                query: "\u{200d}".into(),
            },
        );
        let zwj_first = terminal.copy_cursor_point(owner).unwrap();
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::RepeatSearch {
                direction: SearchDirection::Forward,
            },
        );
        assert_ne!(terminal.copy_cursor_point(owner).unwrap(), zwj_first);
    }

    #[test]
    fn wide_glyph_halves_and_physical_whitespace_cells_render_selection_consistently() {
        let mut terminal = terminal(8, 2);
        terminal.feed("雪  b".as_bytes()).unwrap().unwrap();
        let owner = ClientId::new();
        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Move {
                movement: CopyModeMovement::BeginningOfLine,
            },
        );
        copy_action(&mut terminal, owner, CopyModeAction::ToggleSelection);
        let tail = active_screen(copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Move {
                movement: CopyModeMovement::Right,
            },
        ));
        assert!(tail.cells[0].selected && tail.cells[1].selected);

        let whitespace = active_screen(copy_action(
            &mut terminal,
            owner,
            CopyModeAction::Move {
                movement: CopyModeMovement::Right,
            },
        ));
        assert!(whitespace.cells[2].selected);
    }

    #[test]
    fn wrapped_wide_glyph_spacer_head_and_both_halves_share_selection() {
        let mut cells = vec![Cell::default(); 6];
        cells[4].selected = true;
        let widths = [
            CellWide::Narrow,
            CellWide::Narrow,
            CellWide::SpacerHead,
            CellWide::Wide,
            CellWide::SpacerTail,
            CellWide::Narrow,
        ];

        normalize_wide_selection(&mut cells, &widths, 3);

        assert!(cells[2..=4].iter().all(|cell| cell.selected));
        assert!(!cells[1].selected && !cells[5].selected);
    }

    #[test]
    fn prepared_copy_stays_active_until_matching_finalize_and_cancel_is_canonical() {
        let mut terminal = terminal(8, 2);
        terminal.feed(b"content").unwrap().unwrap();
        let owner = ClientId::new();
        let selected = active_screen(copy_action(&mut terminal, owner, CopyModeAction::Begin));
        let CopyModeOutcome::Prepared { copy_id, .. } =
            copy_action(&mut terminal, owner, CopyModeAction::Copy)
        else {
            panic!("copy was not prepared")
        };
        assert!(terminal.copy_modes.contains_key(&owner));
        assert!(matches!(
            terminal.copy_mode(
                owner,
                CopyModeAction::FinalizeCopy {
                    copy_id: Uuid::new_v4(),
                },
                None,
            ),
            Err(CopyModeFailure::Semantic(
                CopyModeError::CopyConfirmationMismatch
            ))
        ));
        assert!(terminal.copy_modes.contains_key(&owner));

        let CopyModeOutcome::Finalized { screen } = copy_action(
            &mut terminal,
            owner,
            CopyModeAction::FinalizeCopy { copy_id },
        ) else {
            panic!("copy did not finalize")
        };
        assert!(screen.revision > selected.revision);
        assert!(screen.cells.iter().all(|cell| !cell.selected));
        assert!(!terminal.copy_modes.contains_key(&owner));

        copy_action(&mut terminal, owner, CopyModeAction::Begin);
        let CopyModeOutcome::Cancelled { screen } =
            copy_action(&mut terminal, owner, CopyModeAction::Cancel)
        else {
            panic!("copy mode did not cancel")
        };
        assert!(screen.cells.iter().all(|cell| !cell.selected));
    }

    #[test]
    fn copy_payload_limit_is_explicit_and_never_truncates() {
        assert_eq!(validate_copy_cells(MAX_COPY_CELLS), Ok(()));
        assert_eq!(
            validate_copy_cells(MAX_COPY_CELLS + 1),
            Err(CopyModeError::CopySpanTooLarge {
                actual: MAX_COPY_CELLS + 1,
                maximum: MAX_COPY_CELLS,
            })
        );
        assert_eq!(validate_copy_size(MAX_COPY_BYTES), Ok(()));
        assert_eq!(
            validate_copy_size(MAX_COPY_BYTES + 1),
            Err(CopyModeError::CopyTooLarge {
                actual: MAX_COPY_BYTES + 1,
                maximum: MAX_COPY_BYTES,
            })
        );
    }

    #[test]
    fn search_limits_are_explicit_and_never_truncate() {
        assert_eq!(validate_search_query("literal 雪"), Ok(()));
        assert_eq!(
            validate_search_query(&"x".repeat(MAX_SEARCH_QUERY_BYTES + 1)),
            Err(CopyModeError::SearchQueryTooLarge {
                actual: MAX_SEARCH_QUERY_BYTES + 1,
                maximum: MAX_SEARCH_QUERY_BYTES,
            })
        );
        assert_eq!(
            validate_search_cells(MAX_SEARCH_CELLS + 1),
            Err(CopyModeError::SearchSpaceTooLarge {
                actual: MAX_SEARCH_CELLS + 1,
                maximum: MAX_SEARCH_CELLS,
            })
        );
        assert_eq!(
            validate_search_text_size(MAX_SEARCH_TEXT_BYTES + 1),
            Err(CopyModeError::SearchTextTooLarge {
                actual: MAX_SEARCH_TEXT_BYTES + 1,
                maximum: MAX_SEARCH_TEXT_BYTES,
            })
        );
        assert_eq!(
            validate_search_cell_size(MAX_SEARCH_CELL_CODEPOINTS + 1),
            Err(CopyModeError::SearchCellTooLarge {
                actual: MAX_SEARCH_CELL_CODEPOINTS + 1,
                maximum: MAX_SEARCH_CELL_CODEPOINTS,
            })
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
    fn idle_flushes_publish_nothing_for_a_quiet_terminal() {
        let mut terminal = terminal(20, 1);
        let fed = terminal.feed(b"QUIET").unwrap().unwrap();
        assert!(text(&fed).starts_with("QUIET"));

        for _ in 0..3 {
            assert!(terminal.flush_synchronized_output().unwrap().is_none());
        }
        assert_eq!(terminal.snapshot().unwrap().revision, fed.revision + 1);
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
