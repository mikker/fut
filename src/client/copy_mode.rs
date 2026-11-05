use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

use crate::domain::{
    CopyModeAction, CopyModeMovement, MAX_SEARCH_QUERY_BYTES, SearchDirection, TerminalId,
};

use super::chrome::{sanitize, truncate};

const ACTION_QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CopyModeReply {
    Snapshot,
    Prepared,
    Finalized,
    Cancelled,
}

impl CopyModeReply {
    fn for_action(action: &CopyModeAction) -> Self {
        match action {
            CopyModeAction::Begin
            | CopyModeAction::BeginSelection { .. }
            | CopyModeAction::SetSelectionEnd { .. }
            | CopyModeAction::Move { .. }
            | CopyModeAction::ToggleSelection
            | CopyModeAction::Search { .. }
            | CopyModeAction::RepeatSearch { .. } => Self::Snapshot,
            CopyModeAction::Copy => Self::Prepared,
            CopyModeAction::FinalizeCopy { .. } => Self::Finalized,
            CopyModeAction::Cancel => Self::Cancelled,
        }
    }

    fn is_terminal_barrier(self) -> bool {
        matches!(self, Self::Prepared | Self::Finalized | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CopyModeInput {
    Stay,
    Pump,
    Notice(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CopyModeErrorDisposition {
    Ignored,
    Continue,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CopyModePaste {
    Accepted,
    Ignored,
    TooLarge,
}

pub(super) struct CopyModeSubmission {
    pub request_id: Uuid,
    pub action: CopyModeAction,
}

pub(super) struct CopyModeState {
    terminal_id: TerminalId,
    search_prompt: Option<String>,
    active: bool,
    pending: Option<(Uuid, CopyModeReply)>,
    actions: VecDeque<CopyModeAction>,
    clipboard: Option<(Uuid, Uuid)>,
    copied_bytes: Option<usize>,
    mouse_position: Option<(u16, u16)>,
}

impl CopyModeState {
    pub fn enter(terminal_id: TerminalId) -> Self {
        let mut state = Self::new(terminal_id);
        state.actions.push_back(CopyModeAction::Begin);
        state
    }

    fn new(terminal_id: TerminalId) -> Self {
        Self {
            terminal_id,
            search_prompt: None,
            active: false,
            pending: None,
            actions: VecDeque::new(),
            clipboard: None,
            copied_bytes: None,
            mouse_position: None,
        }
    }

    pub fn enter_mouse(terminal_id: TerminalId, anchor: (u16, u16), position: (u16, u16)) -> Self {
        let mut state = Self::new(terminal_id);
        state.actions.push_back(CopyModeAction::BeginSelection {
            column: anchor.0,
            row: anchor.1,
        });
        state.actions.push_back(CopyModeAction::SetSelectionEnd {
            column: position.0,
            row: position.1,
        });
        state.mouse_position = Some(position);
        state
    }

    pub fn terminal_id(&self) -> TerminalId {
        self.terminal_id
    }

    pub fn is_mouse_dragging(&self) -> bool {
        self.mouse_position.is_some()
    }

    pub fn mouse_drag(&mut self, column: u16, row: u16) -> CopyModeInput {
        let Some(position) = self.mouse_position else {
            return CopyModeInput::Stay;
        };
        if position == (column, row) {
            return CopyModeInput::Stay;
        }
        self.mouse_position = Some((column, row));
        let action = CopyModeAction::SetSelectionEnd { column, row };
        if matches!(
            self.actions.back(),
            Some(CopyModeAction::SetSelectionEnd { .. })
        ) {
            self.actions.pop_back();
        }
        self.enqueue(action)
    }

    pub fn mouse_release(&mut self, column: u16, row: u16) -> CopyModeInput {
        if self.mouse_position.is_none() {
            return CopyModeInput::Stay;
        }
        let moved = self.mouse_drag(column, row);
        self.mouse_position = None;
        if matches!(moved, CopyModeInput::Notice(_)) {
            moved
        } else {
            self.enqueue(CopyModeAction::Copy)
        }
    }

    pub fn mouse_release_current(&mut self) -> CopyModeInput {
        self.mouse_position
            .map_or(CopyModeInput::Stay, |(column, row)| {
                self.mouse_release(column, row)
            })
    }

    pub fn key(&mut self, key: KeyEvent) -> CopyModeInput {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return CopyModeInput::Stay;
        }
        if self.search_prompt.is_some() {
            return self.search_key(key);
        }

        let action = match key.code {
            KeyCode::Esc | KeyCode::Char('q') => CopyModeAction::Cancel,
            KeyCode::Char('y') | KeyCode::Enter => CopyModeAction::Copy,
            KeyCode::Char(' ') => CopyModeAction::ToggleSelection,
            KeyCode::Char('/') => {
                if self.terminal_action_pending() {
                    return CopyModeInput::Notice(
                        "copy or cancel is already pending; search was not opened",
                    );
                }
                self.search_prompt = Some(String::new());
                return CopyModeInput::Stay;
            }
            KeyCode::Char('n') => CopyModeAction::RepeatSearch {
                direction: SearchDirection::Forward,
            },
            KeyCode::Char('N') => CopyModeAction::RepeatSearch {
                direction: SearchDirection::Backward,
            },
            KeyCode::Left | KeyCode::Char('h') => move_action(CopyModeMovement::Left),
            KeyCode::Down | KeyCode::Char('j') => move_action(CopyModeMovement::Down),
            KeyCode::Up | KeyCode::Char('k') => move_action(CopyModeMovement::Up),
            KeyCode::Right | KeyCode::Char('l') => move_action(CopyModeMovement::Right),
            KeyCode::Home => move_action(CopyModeMovement::BeginningOfLine),
            KeyCode::End => move_action(CopyModeMovement::EndOfLine),
            KeyCode::PageUp => move_action(CopyModeMovement::PageUp),
            KeyCode::PageDown => move_action(CopyModeMovement::PageDown),
            _ => return CopyModeInput::Stay,
        };
        self.enqueue(action)
    }

    pub fn paste(&mut self, value: &str) -> CopyModePaste {
        let Some(query) = self.search_prompt.as_mut() else {
            return CopyModePaste::Ignored;
        };
        if query.len().saturating_add(value.len()) > MAX_SEARCH_QUERY_BYTES {
            return CopyModePaste::TooLarge;
        }
        query.push_str(value);
        CopyModePaste::Accepted
    }

    pub fn start_next(&mut self) -> Option<CopyModeSubmission> {
        if self.pending.is_some() || self.clipboard.is_some() {
            return None;
        }
        let action = self.actions.pop_front()?;
        let reply = CopyModeReply::for_action(&action);
        let request_id = Uuid::new_v4();
        self.pending = Some((request_id, reply));
        Some(CopyModeSubmission { request_id, action })
    }

    pub fn complete(
        &mut self,
        terminal_id: TerminalId,
        request_id: Option<Uuid>,
        expected: CopyModeReply,
    ) -> bool {
        let Some((pending_id, reply)) = self.pending else {
            return false;
        };
        if terminal_id == self.terminal_id && request_id == Some(pending_id) && reply == expected {
            self.pending = None;
            if expected == CopyModeReply::Snapshot {
                self.active = true;
            }
            true
        } else {
            false
        }
    }

    pub fn copy_mode_error(
        &mut self,
        terminal_id: TerminalId,
        request_id: Option<Uuid>,
        error: &crate::domain::CopyModeError,
    ) -> CopyModeErrorDisposition {
        if terminal_id != self.terminal_id {
            return CopyModeErrorDisposition::Ignored;
        }
        let exits = matches!(
            error,
            crate::domain::CopyModeError::NotActive | crate::domain::CopyModeError::CursorLost
        );
        if request_id.is_none() {
            if exits {
                self.clear_work();
                return CopyModeErrorDisposition::Exit;
            }
            return CopyModeErrorDisposition::Ignored;
        }

        let Some((pending_id, reply)) = self.pending else {
            return CopyModeErrorDisposition::Ignored;
        };
        if request_id != Some(pending_id) {
            return CopyModeErrorDisposition::Ignored;
        }
        self.pending = None;
        if reply == CopyModeReply::Finalized {
            self.copied_bytes = None;
        }
        if !self.active || exits {
            self.clear_work();
            CopyModeErrorDisposition::Exit
        } else {
            CopyModeErrorDisposition::Continue
        }
    }

    pub fn fail(&mut self, request_id: Option<Uuid>) -> CopyModeErrorDisposition {
        let Some((pending_id, reply)) = self.pending else {
            return CopyModeErrorDisposition::Ignored;
        };
        if request_id != Some(pending_id) {
            return CopyModeErrorDisposition::Ignored;
        }
        self.pending = None;
        if reply == CopyModeReply::Finalized {
            self.copied_bytes = None;
        }
        if !self.active {
            self.clear_work();
            CopyModeErrorDisposition::Exit
        } else {
            CopyModeErrorDisposition::Continue
        }
    }

    pub fn begin_clipboard(&mut self, request_id: Uuid, copy_id: Uuid) -> bool {
        if self.pending.is_some() || self.clipboard.is_some() {
            return false;
        }
        self.clipboard = Some((request_id, copy_id));
        true
    }

    pub fn finish_clipboard(&mut self, request_id: Uuid) -> Option<Uuid> {
        let (pending_id, copy_id) = self.clipboard?;
        if pending_id != request_id {
            return None;
        }
        self.clipboard = None;
        Some(copy_id)
    }

    pub fn finalize_copy(&mut self, copy_id: Uuid, bytes: usize) {
        debug_assert!(self.pending.is_none() && self.clipboard.is_none());
        self.copied_bytes = Some(bytes);
        self.actions
            .push_front(CopyModeAction::FinalizeCopy { copy_id });
    }

    pub fn take_copied_bytes(&mut self) -> Option<usize> {
        self.copied_bytes.take()
    }

    fn enqueue(&mut self, action: CopyModeAction) -> CopyModeInput {
        if self.terminal_action_pending() {
            return CopyModeInput::Notice("action was not queued; copy or cancel is pending");
        }
        if self.actions.len() >= ACTION_QUEUE_CAPACITY {
            return CopyModeInput::Notice("copy action queue is full; action was not queued");
        }
        self.actions.push_back(action);
        CopyModeInput::Pump
    }

    fn terminal_action_pending(&self) -> bool {
        self.clipboard.is_some()
            || self
                .pending
                .is_some_and(|(_, reply)| reply.is_terminal_barrier())
            || self
                .actions
                .iter()
                .any(|action| CopyModeReply::for_action(action).is_terminal_barrier())
    }

    fn clear_work(&mut self) {
        self.active = false;
        self.pending = None;
        self.actions.clear();
        self.clipboard = None;
        self.copied_bytes = None;
        self.mouse_position = None;
    }

    pub fn render(&self, area: Rect, buffer: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let style = Style::default().add_modifier(Modifier::REVERSED);
        let width = area.width.min(48);
        let column = area.x + area.width - width;
        let row = area.y;
        for column in column..column + width {
            if let Some(cell) = buffer.cell_mut((column, row)) {
                cell.set_symbol(" ").set_style(style);
            }
        }
        let text = if self.clipboard.is_some() {
            " COPY · clipboard pending… ".to_owned()
        } else if self.copied_bytes.is_some()
            || self
                .pending
                .is_some_and(|(_, reply)| reply == CopyModeReply::Finalized)
        {
            " COPY · finalizing clipboard… ".to_owned()
        } else if let Some(query) = self.search_prompt.as_ref() {
            format!(" /{} · Enter find · Esc closes prompt ", sanitize(query))
        } else if self.pending.is_some() || !self.actions.is_empty() {
            format!(" COPY · processing · {} queued ", self.actions.len())
        } else {
            " COPY · y copy · / find · Esc/q cancel ".to_owned()
        };
        buffer.set_stringn(
            column,
            row,
            truncate(&text, usize::from(width)),
            usize::from(width),
            style,
        );
    }

    fn search_key(&mut self, key: KeyEvent) -> CopyModeInput {
        match key.code {
            KeyCode::Esc => {
                self.search_prompt = None;
                CopyModeInput::Stay
            }
            KeyCode::Enter => {
                let query = self.search_prompt.take().unwrap_or_default();
                if query.is_empty() {
                    CopyModeInput::Stay
                } else {
                    self.enqueue(CopyModeAction::Search { query })
                }
            }
            KeyCode::Backspace | KeyCode::Delete => {
                let query = self.search_prompt.as_mut().expect("search prompt exists");
                if let Some((index, _)) = query.grapheme_indices(true).next_back() {
                    query.truncate(index);
                }
                CopyModeInput::Stay
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_prompt
                    .as_mut()
                    .expect("search prompt exists")
                    .clear();
                CopyModeInput::Stay
            }
            KeyCode::Char(character)
                if !character.is_control()
                    && !key.modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
            {
                let query = self.search_prompt.as_mut().expect("search prompt exists");
                if query.len() + character.len_utf8() <= MAX_SEARCH_QUERY_BYTES {
                    query.push(character);
                    CopyModeInput::Stay
                } else {
                    CopyModeInput::Notice("search query limit reached; character was not added")
                }
            }
            _ => CopyModeInput::Stay,
        }
    }
}

fn move_action(movement: CopyModeMovement) -> CopyModeAction {
    CopyModeAction::Move { movement }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn take_and_complete(state: &mut CopyModeState) -> CopyModeAction {
        let submission = state.start_next().expect("queued copy-mode action");
        let reply = CopyModeReply::for_action(&submission.action);
        assert!(state.complete(state.terminal_id(), Some(submission.request_id), reply,));
        submission.action
    }

    #[test]
    fn navigation_selection_copy_and_cancel_are_queued_in_order() {
        let mut state = CopyModeState::enter(TerminalId::new());
        assert_eq!(take_and_complete(&mut state), CopyModeAction::Begin);
        for (code, expected) in [
            (KeyCode::Left, CopyModeMovement::Left),
            (KeyCode::Char('j'), CopyModeMovement::Down),
            (KeyCode::Home, CopyModeMovement::BeginningOfLine),
            (KeyCode::PageDown, CopyModeMovement::PageDown),
        ] {
            assert_eq!(
                state.key(key(code, KeyModifiers::NONE)),
                CopyModeInput::Pump
            );
            assert_eq!(take_and_complete(&mut state), move_action(expected));
        }
        assert_eq!(
            state.key(key(KeyCode::Char(' '), KeyModifiers::NONE)),
            CopyModeInput::Pump
        );
        assert_eq!(
            take_and_complete(&mut state),
            CopyModeAction::ToggleSelection
        );

        let mut copy = CopyModeState::enter(TerminalId::new());
        take_and_complete(&mut copy);
        assert_eq!(
            copy.key(key(KeyCode::Enter, KeyModifiers::NONE)),
            CopyModeInput::Pump
        );
        assert_eq!(take_and_complete(&mut copy), CopyModeAction::Copy);

        let mut cancel = CopyModeState::enter(TerminalId::new());
        take_and_complete(&mut cancel);
        assert_eq!(
            cancel.key(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            CopyModeInput::Pump
        );
        assert_eq!(take_and_complete(&mut cancel), CopyModeAction::Cancel);
    }

    #[test]
    fn mouse_drag_begins_at_the_anchor_coalesces_motion_and_copies_on_release() {
        let terminal_id = TerminalId::new();
        let mut state = CopyModeState::enter_mouse(terminal_id, (2, 3), (5, 3));
        assert!(state.is_mouse_dragging());
        assert_eq!(
            take_and_complete(&mut state),
            CopyModeAction::BeginSelection { column: 2, row: 3 }
        );

        assert_eq!(state.mouse_drag(6, 3), CopyModeInput::Pump);
        assert_eq!(state.mouse_drag(8, 4), CopyModeInput::Pump);
        assert_eq!(
            take_and_complete(&mut state),
            CopyModeAction::SetSelectionEnd { column: 8, row: 4 }
        );
        assert_eq!(state.mouse_release(9, 4), CopyModeInput::Pump);
        assert!(!state.is_mouse_dragging());
        assert_eq!(
            take_and_complete(&mut state),
            CopyModeAction::SetSelectionEnd { column: 9, row: 4 }
        );
        assert_eq!(take_and_complete(&mut state), CopyModeAction::Copy);
    }

    #[test]
    fn search_prompt_owns_editing_and_paste_until_submit() {
        let mut state = CopyModeState::enter(TerminalId::new());
        take_and_complete(&mut state);
        assert_eq!(state.paste("outside"), CopyModePaste::Ignored);
        assert_eq!(
            state.key(key(KeyCode::Char('/'), KeyModifiers::NONE)),
            CopyModeInput::Stay
        );
        assert_eq!(state.paste("héllo 雪"), CopyModePaste::Accepted);
        state.key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(
            state.key(key(KeyCode::Enter, KeyModifiers::NONE)),
            CopyModeInput::Pump
        );
        assert_eq!(
            take_and_complete(&mut state),
            CopyModeAction::Search {
                query: "héllo ".into(),
            }
        );
        assert_eq!(
            state.key(key(KeyCode::Char('N'), KeyModifiers::SHIFT)),
            CopyModeInput::Pump
        );
        assert_eq!(
            take_and_complete(&mut state),
            CopyModeAction::RepeatSearch {
                direction: SearchDirection::Backward,
            }
        );
    }

    #[test]
    fn search_paste_rejects_the_whole_oversized_value() {
        let mut state = CopyModeState::enter(TerminalId::new());
        take_and_complete(&mut state);
        state.key(key(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(
            state.paste(&"x".repeat(MAX_SEARCH_QUERY_BYTES + 1)),
            CopyModePaste::TooLarge
        );
        assert_eq!(
            state.key(key(KeyCode::Enter, KeyModifiers::NONE)),
            CopyModeInput::Stay
        );
    }

    #[test]
    fn request_correlation_requires_the_expected_typed_reply() {
        let mut state = CopyModeState::enter(TerminalId::new());
        take_and_complete(&mut state);
        assert_eq!(
            state.key(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            CopyModeInput::Pump
        );
        let submission = state.start_next().unwrap();
        let request_id = submission.request_id;
        assert_eq!(submission.action, CopyModeAction::Copy);
        assert!(!state.complete(
            state.terminal_id(),
            Some(request_id),
            CopyModeReply::Snapshot,
        ));
        assert!(!state.complete(state.terminal_id(), None, CopyModeReply::Prepared,));
        assert!(!state.complete(
            state.terminal_id(),
            Some(Uuid::new_v4()),
            CopyModeReply::Prepared,
        ));
        assert!(!state.complete(TerminalId::new(), Some(request_id), CopyModeReply::Prepared,));
        assert!(state.complete(
            state.terminal_id(),
            Some(request_id),
            CopyModeReply::Prepared,
        ));
    }

    #[test]
    fn pending_requests_queue_navigation_and_nested_escape_closes_only_search() {
        let mut state = CopyModeState::enter(TerminalId::new());
        let begin = state.start_next().unwrap();
        assert_eq!(
            state.key(key(KeyCode::PageUp, KeyModifiers::NONE)),
            CopyModeInput::Pump
        );
        assert!(state.complete(
            state.terminal_id(),
            Some(begin.request_id),
            CopyModeReply::Snapshot,
        ));
        assert_eq!(
            take_and_complete(&mut state),
            move_action(CopyModeMovement::PageUp)
        );

        state.key(key(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(
            state.key(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            CopyModeInput::Stay
        );
        assert_eq!(
            state.key(key(KeyCode::Esc, KeyModifiers::NONE)),
            CopyModeInput::Stay
        );
        assert_eq!(
            state.key(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            CopyModeInput::Pump
        );
        assert_eq!(take_and_complete(&mut state), CopyModeAction::Cancel);
    }

    #[test]
    fn bounded_queue_preserves_rapid_actions_and_reports_overflow() {
        let mut state = CopyModeState::enter(TerminalId::new());
        let begin = state.start_next().unwrap();
        let expected = [
            move_action(CopyModeMovement::Left),
            move_action(CopyModeMovement::Down),
            CopyModeAction::ToggleSelection,
            CopyModeAction::Cancel,
        ];
        for code in [
            KeyCode::Char('h'),
            KeyCode::Char('j'),
            KeyCode::Char(' '),
            KeyCode::Char('q'),
        ] {
            assert_eq!(
                state.key(key(code, KeyModifiers::NONE)),
                CopyModeInput::Pump
            );
        }
        assert!(state.complete(
            state.terminal_id(),
            Some(begin.request_id),
            CopyModeReply::Snapshot,
        ));
        for expected in expected {
            assert_eq!(take_and_complete(&mut state), expected);
        }

        let mut full = CopyModeState::enter(TerminalId::new());
        full.start_next().unwrap();
        for _ in 0..ACTION_QUEUE_CAPACITY {
            assert_eq!(
                full.key(key(KeyCode::Left, KeyModifiers::NONE)),
                CopyModeInput::Pump
            );
        }
        assert!(matches!(
            full.key(key(KeyCode::Left, KeyModifiers::NONE)),
            CopyModeInput::Notice(message) if message.contains("queue is full")
        ));
    }

    #[test]
    fn clipboard_is_a_visible_barrier_and_finalize_follows_the_prepared_copy() {
        let mut state = CopyModeState::enter(TerminalId::new());
        take_and_complete(&mut state);
        assert_eq!(
            state.key(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            CopyModeInput::Pump
        );
        let prepared = state.start_next().unwrap();
        assert_eq!(prepared.action, CopyModeAction::Copy);
        assert!(state.complete(
            state.terminal_id(),
            Some(prepared.request_id),
            CopyModeReply::Prepared,
        ));
        let copy_id = Uuid::new_v4();
        assert!(state.begin_clipboard(prepared.request_id, copy_id));
        let CopyModeInput::Notice(notice) = state.key(key(KeyCode::Left, KeyModifiers::NONE))
        else {
            panic!("clipboard action was not rejected with feedback")
        };
        assert!(notice.contains("action was not queued"));

        let area = Rect::new(0, 0, 60, 1);
        let mut buffer = Buffer::empty(area);
        state.render(area, &mut buffer);
        let status = (12..60)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(status.contains("clipboard pending"));

        assert_eq!(state.finish_clipboard(prepared.request_id), Some(copy_id));
        state.finalize_copy(copy_id, 42);
        let finalized = state.start_next().unwrap();
        assert_eq!(finalized.action, CopyModeAction::FinalizeCopy { copy_id });
        assert!(state.complete(
            state.terminal_id(),
            Some(finalized.request_id),
            CopyModeReply::Finalized,
        ));
        assert_eq!(state.take_copied_bytes(), Some(42));
    }

    #[test]
    fn unsolicited_cursor_loss_exits_and_search_limit_reports_locally() {
        let mut state = CopyModeState::enter(TerminalId::new());
        take_and_complete(&mut state);
        state.key(key(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(
            state.paste(&"x".repeat(MAX_SEARCH_QUERY_BYTES)),
            CopyModePaste::Accepted
        );
        assert!(matches!(
            state.key(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            CopyModeInput::Notice(message) if message.contains("limit reached")
        ));
        assert_eq!(
            state.copy_mode_error(
                state.terminal_id(),
                None,
                &crate::domain::CopyModeError::CursorLost,
            ),
            CopyModeErrorDisposition::Exit
        );
        assert!(state.start_next().is_none());
    }

    #[test]
    fn compact_status_clears_a_deterministic_top_right_cue_without_hiding_bottom_content() {
        let area = Rect::new(0, 0, 60, 3);
        let mut buffer = Buffer::filled(area, ratatui::buffer::Cell::new("x"));
        let state = CopyModeState::enter(TerminalId::new());
        state.render(area, &mut buffer);

        assert_eq!(buffer[(0, 0)].symbol(), "x");
        assert_eq!(buffer[(0, 2)].symbol(), "x");
        let cue = (12..60)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();
        assert!(cue.contains("COPY"));

        let tiny = Rect::new(0, 0, 4, 1);
        let mut tiny_buffer = Buffer::filled(tiny, ratatui::buffer::Cell::new("x"));
        state.render(tiny, &mut tiny_buffer);
        assert_eq!(
            (0..4)
                .map(|column| tiny_buffer[(column, 0)].symbol())
                .collect::<String>(),
            " CO…"
        );
    }
}
