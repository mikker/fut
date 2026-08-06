use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use crate::{
    domain::{AgentState, AttentionKind, PaneId, SessionId, TerminalId},
    resources::{PaneSnapshot, ResourceSnapshot},
};

use super::chrome::truncate;

const BRAILLE_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActivityIndicator {
    Working,
    Blocked,
    Completed,
}

impl ActivityIndicator {
    pub(super) fn marker(self, frame: usize) -> &'static str {
        match self {
            Self::Working => BRAILLE_SPINNER[frame % BRAILLE_SPINNER.len()],
            Self::Blocked => "!",
            Self::Completed => "●",
        }
    }
}

#[derive(Default)]
pub(super) struct NotificationState {
    seen: HashMap<TerminalId, u64>,
}

impl NotificationState {
    pub(super) fn observe(&mut self, terminal_id: TerminalId, revision: u64) -> bool {
        let seen = self.seen.entry(terminal_id).or_default();
        if *seen >= revision {
            return false;
        }
        *seen = revision;
        true
    }

    pub(super) fn is_unseen(&self, pane: &PaneSnapshot) -> bool {
        pane.activity.attention.is_some_and(|attention| {
            self.seen.get(&pane.terminal_id).copied().unwrap_or(0) < attention.revision
        })
    }

    pub(super) fn indicator(&self, panes: &[PaneSnapshot]) -> Option<ActivityIndicator> {
        if panes.iter().any(|pane| {
            self.is_unseen(pane)
                && pane
                    .activity
                    .attention
                    .is_some_and(|attention| attention.kind == AttentionKind::Blocked)
        }) {
            return Some(ActivityIndicator::Blocked);
        }
        if panes.iter().any(|pane| {
            self.is_unseen(pane)
                && pane
                    .activity
                    .attention
                    .is_some_and(|attention| attention.kind == AttentionKind::Completed)
        }) {
            return Some(ActivityIndicator::Completed);
        }
        if panes
            .iter()
            .any(|pane| pane.activity.state == AgentState::Blocked)
        {
            return Some(ActivityIndicator::Blocked);
        }
        panes
            .iter()
            .any(|pane| pane.activity.state == AgentState::Working)
            .then_some(ActivityIndicator::Working)
    }

    pub(super) fn waiting(&self, snapshot: &ResourceSnapshot) -> Vec<WaitingTerminal> {
        let mut waiting = Vec::new();
        for session in &snapshot.sessions {
            for workspace in &session.workspaces {
                for tab in &workspace.tabs {
                    for pane in &tab.panes {
                        let Some(attention) = pane.activity.attention else {
                            continue;
                        };
                        if !self.is_unseen(pane) || pane.closing {
                            continue;
                        }
                        waiting.push(WaitingTerminal {
                            session_id: session.id,
                            pane_id: pane.id,
                            terminal_id: pane.terminal_id,
                            session: session.name.clone(),
                            workspace: workspace.name.clone(),
                            tab: tab.name.clone(),
                            kind: attention.kind,
                            occurred_at_ms: attention.occurred_at_ms,
                        });
                    }
                }
            }
        }
        waiting
    }

    pub(super) fn waiting_count(&self, snapshot: &ResourceSnapshot) -> usize {
        self.waiting(snapshot).len()
    }

    pub(super) fn session_waiting_count(
        &self,
        snapshot: &ResourceSnapshot,
        session_id: SessionId,
    ) -> usize {
        self.waiting(snapshot)
            .into_iter()
            .filter(|item| item.session_id == session_id)
            .count()
    }

    pub(super) fn next(&self, snapshot: &ResourceSnapshot, current: TerminalId) -> Option<PaneId> {
        let panes = snapshot
            .sessions
            .iter()
            .flat_map(|session| &session.workspaces)
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| &tab.panes)
            .collect::<Vec<_>>();
        let start = panes
            .iter()
            .position(|pane| pane.terminal_id == current)
            .map_or(0, |index| index + 1);
        (0..panes.len())
            .map(|offset| panes[(start + offset) % panes.len()])
            .find(|pane| !pane.closing && self.is_unseen(pane))
            .map(|pane| pane.id)
    }

    pub(super) fn has_working(&self, snapshot: &ResourceSnapshot) -> bool {
        snapshot
            .sessions
            .iter()
            .flat_map(|session| &session.workspaces)
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| &tab.panes)
            .any(|pane| pane.activity.state == AgentState::Working)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WaitingTerminal {
    pub session_id: SessionId,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub session: String,
    pub workspace: String,
    pub tab: String,
    pub kind: AttentionKind,
    pub occurred_at_ms: u64,
}

pub(super) struct NotificationsDialog {
    rows: Vec<WaitingTerminal>,
    selected: usize,
    scroll: usize,
}

pub(super) enum NotificationsAction {
    Stay,
    Close,
    Select(PaneId),
}

impl NotificationsDialog {
    pub(super) fn open(snapshot: &ResourceSnapshot, notifications: &NotificationState) -> Self {
        Self {
            rows: notifications.waiting(snapshot),
            selected: 0,
            scroll: 0,
        }
    }

    pub(super) fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        notifications: &NotificationState,
    ) {
        let selected = self.rows.get(self.selected).map(|row| row.terminal_id);
        self.rows = notifications.waiting(snapshot);
        self.selected = selected
            .and_then(|terminal_id| {
                self.rows
                    .iter()
                    .position(|row| row.terminal_id == terminal_id)
            })
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.selected);
    }

    pub(super) fn key(&mut self, key: KeyEvent, visible_rows: usize) -> NotificationsAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return NotificationsAction::Stay;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return NotificationsAction::Close,
            KeyCode::Enter => {
                return self
                    .rows
                    .get(self.selected)
                    .map_or(NotificationsAction::Stay, |row| {
                        NotificationsAction::Select(row.pane_id)
                    });
            }
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.rows.len().saturating_sub(1),
            _ => {}
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if visible_rows > 0 && self.selected >= self.scroll + visible_rows {
            self.scroll = self.selected + 1 - visible_rows;
        }
        NotificationsAction::Stay
    }

    pub(super) fn render(&mut self, area: Rect, buffer: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                buffer[(x, y)].reset();
            }
        }
        let header = usize::from(area.height >= 2);
        let footer = usize::from(area.height >= 3);
        let body_height = usize::from(area.height).saturating_sub(header + footer);
        if header == 1 {
            buffer.set_string(
                area.x,
                area.y,
                "fut · terminals waiting",
                Style::default().add_modifier(Modifier::BOLD),
            );
        }
        if self.rows.is_empty() {
            if body_height > 0 {
                buffer.set_string(
                    area.x,
                    area.y + header as u16,
                    "No terminals waiting",
                    Style::default(),
                );
            }
        } else {
            let max_scroll = self.rows.len().saturating_sub(body_height.max(1));
            self.scroll = self.scroll.min(max_scroll);
            for (line, (index, row)) in self
                .rows
                .iter()
                .enumerate()
                .skip(self.scroll)
                .take(body_height)
                .enumerate()
            {
                let selected = index == self.selected;
                let style = if selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let marker = match row.kind {
                    AttentionKind::Blocked => "!",
                    AttentionKind::Completed => "●",
                };
                let kind = match row.kind {
                    AttentionKind::Blocked => "blocked",
                    AttentionKind::Completed => "completed",
                };
                let age = age(row.occurred_at_ms);
                let text = format!(
                    " {marker} {kind}  {} › {} › {}  {age}",
                    row.session, row.workspace, row.tab
                );
                let text = truncate(&text, usize::from(area.width));
                let y = area.y + header as u16 + line as u16;
                for x in area.x..area.x.saturating_add(area.width) {
                    buffer[(x, y)].set_style(style);
                }
                buffer.set_stringn(area.x, y, text, usize::from(area.width), style);
            }
        }
        if footer == 1 {
            buffer.set_stringn(
                area.x,
                area.y + area.height - 1,
                "↑↓/jk move  enter switch  esc cancel",
                usize::from(area.width),
                Style::default().add_modifier(Modifier::DIM),
            );
        }
    }
}

fn age(timestamp_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now.saturating_sub(timestamp_ms) / 1_000;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        _ => format!("{}h", seconds / 3_600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AgentActivity, AgentAttention, AttentionKind};

    #[test]
    fn spinner_uses_braille_frames() {
        assert_eq!(ActivityIndicator::Working.marker(0), "⠋");
        assert_eq!(ActivityIndicator::Working.marker(10), "⠋");
    }

    #[test]
    fn unseen_is_per_client_revision() {
        let terminal_id = TerminalId::new();
        let pane = PaneSnapshot {
            id: PaneId::new(),
            terminal_id,
            closing: false,
            activity: AgentActivity {
                state: AgentState::Idle,
                revision: 4,
                updated_at_ms: 10,
                attention: Some(AgentAttention {
                    revision: 4,
                    kind: AttentionKind::Completed,
                    occurred_at_ms: 10,
                }),
            },
        };
        let mut notifications = NotificationState::default();
        assert!(notifications.is_unseen(&pane));
        assert!(notifications.observe(terminal_id, 4));
        assert!(!notifications.is_unseen(&pane));
    }

    #[test]
    fn observing_an_older_render_does_not_hide_a_newer_completion() {
        let terminal_id = TerminalId::new();
        let pane = PaneSnapshot {
            id: PaneId::new(),
            terminal_id,
            closing: false,
            activity: AgentActivity {
                state: AgentState::Idle,
                revision: 5,
                updated_at_ms: 20,
                attention: Some(AgentAttention {
                    revision: 5,
                    kind: AttentionKind::Completed,
                    occurred_at_ms: 20,
                }),
            },
        };
        let mut notifications = NotificationState::default();

        assert!(notifications.observe(terminal_id, 4));
        assert!(notifications.is_unseen(&pane));
    }
}
