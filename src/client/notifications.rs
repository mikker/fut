use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};

use crate::{
    domain::{
        AgentActivity, AgentAttention, AgentReport, AgentState, AttentionKind, PaneId, SessionId,
        TerminalId,
    },
    resources::{PaneSnapshot, ResourceSnapshot},
};

use super::chrome::truncate;
use super::dialog::{
    dialog_area, fill_row, frame_inner, render_footer, render_frame, render_list_scrollbar,
    render_title, row_style,
};

const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 16;

const BRAILLE_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn attention(activity: &AgentActivity) -> Option<AgentAttention> {
    let event = activity.last_event.as_ref()?;
    let kind = match event.kind {
        AgentReport::Blocked => AttentionKind::Blocked,
        AgentReport::Completed => AttentionKind::Completed,
        AgentReport::Idle | AgentReport::Working => return None,
    };
    Some(AgentAttention {
        revision: event.revision,
        kind,
        occurred_at_ms: event.occurred_at_ms,
    })
}

fn open_panes(snapshot: &ResourceSnapshot) -> impl Iterator<Item = &PaneSnapshot> {
    snapshot
        .sessions
        .iter()
        .filter(|session| !session.closing)
        .flat_map(|session| {
            session
                .workspaces
                .iter()
                .filter(|workspace| !workspace.closing)
        })
        .flat_map(|workspace| workspace.tabs.iter().filter(|tab| !tab.closing))
        .flat_map(|tab| tab.panes.iter().filter(|pane| !pane.closing))
}

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
        attention(&pane.activity).is_some_and(|attention| {
            self.seen.get(&pane.terminal_id).copied().unwrap_or(0) < attention.revision
        })
    }

    pub(super) fn indicator(&self, panes: &[PaneSnapshot]) -> Option<ActivityIndicator> {
        if panes.iter().any(|pane| {
            self.is_unseen(pane)
                && attention(&pane.activity)
                    .is_some_and(|attention| attention.kind == AttentionKind::Blocked)
        }) {
            return Some(ActivityIndicator::Blocked);
        }
        if panes.iter().any(|pane| {
            self.is_unseen(pane)
                && attention(&pane.activity)
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
            if session.closing {
                continue;
            }
            for workspace in &session.workspaces {
                if workspace.closing {
                    continue;
                }
                for tab in &workspace.tabs {
                    if tab.closing {
                        continue;
                    }
                    for pane in &tab.panes {
                        let Some(attention) = attention(&pane.activity) else {
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
        let panes = open_panes(snapshot).collect::<Vec<_>>();
        let start = panes
            .iter()
            .position(|pane| pane.terminal_id == current)
            .map_or(0, |index| index + 1);
        (0..panes.len())
            .map(|offset| panes[(start + offset) % panes.len()])
            .find(|pane| pane.terminal_id != current && self.is_unseen(pane))
            .map(|pane| pane.id)
    }

    pub(super) fn has_working(&self, snapshot: &ResourceSnapshot) -> bool {
        open_panes(snapshot).any(|pane| pane.activity.state == AgentState::Working)
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

    pub(super) fn render(&mut self, host: Rect, buffer: &mut Buffer) {
        let area = render_frame(dialog_area(host, MAX_WIDTH, MAX_HEIGHT), buffer);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (header, footer) = chrome_rows(area.height);
        let body_height = usize::from(area.height.saturating_sub(header + footer));
        if header == 1 {
            render_title(area, " terminals waiting", buffer);
        }
        if self.rows.is_empty() {
            if body_height > 0 {
                buffer.set_string(
                    area.x,
                    area.y + header,
                    " No terminals waiting",
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
                let style = row_style(index == self.selected);
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
                let y = area.y + header + line as u16;
                let row_area = Rect::new(area.x, y, area.width, 1);
                fill_row(row_area, style, buffer);
                buffer.set_stringn(area.x, y, text, usize::from(area.width), style);
            }
            let body = Rect::new(
                area.x,
                area.y + header,
                area.width,
                u16::try_from(body_height).expect("body height fits u16"),
            );
            render_list_scrollbar(self.scroll, self.rows.len(), body, buffer);
        }
        if footer == 1 {
            render_footer(area, " ↑↓/jk move  enter switch  esc cancel", buffer);
        }
    }
}

fn chrome_rows(height: u16) -> (u16, u16) {
    (u16::from(height >= 2), u16::from(height >= 3))
}

/// Rows the dialog body can show inside `host`, so key handling scrolls in
/// step with rendering.
pub(super) fn dialog_body_rows(host: Rect) -> usize {
    let area = frame_inner(dialog_area(host, MAX_WIDTH, MAX_HEIGHT));
    let (header, footer) = chrome_rows(area.height);
    usize::from(area.height.saturating_sub(header + footer))
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
    use std::path::PathBuf;

    use super::*;
    use crate::{
        domain::{AgentEvent, AgentIntegration, TabId, WorkspaceId},
        resources::{Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot},
        splits::SplitTree,
    };

    fn completed_pane(closing: bool) -> PaneSnapshot {
        PaneSnapshot {
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing,
            activity: AgentActivity {
                integration: Some(AgentIntegration::default()),
                state: AgentState::Idle,
                revision: 1,
                updated_at_ms: 10,
                last_event: Some(AgentEvent {
                    revision: 1,
                    kind: AgentReport::Completed,
                    occurred_at_ms: 10,
                    turn_id: None,
                }),
            },
        }
    }

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
                integration: Some(AgentIntegration::default()),
                state: AgentState::Idle,
                revision: 4,
                updated_at_ms: 10,
                last_event: Some(AgentEvent {
                    revision: 4,
                    kind: AgentReport::Completed,
                    occurred_at_ms: 10,
                    turn_id: None,
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
                integration: Some(AgentIntegration::default()),
                state: AgentState::Idle,
                revision: 5,
                updated_at_ms: 20,
                last_event: Some(AgentEvent {
                    revision: 5,
                    kind: AgentReport::Completed,
                    occurred_at_ms: 20,
                    turn_id: None,
                }),
            },
        };
        let mut notifications = NotificationState::default();

        assert!(notifications.observe(terminal_id, 4));
        assert!(notifications.is_unseen(&pane));
    }

    #[test]
    fn next_wraps_in_resource_order_and_skips_current_seen_and_closing_panes() {
        let candidate = completed_pane(false);
        let seen = completed_pane(false);
        let current = completed_pane(false);
        let closing = completed_pane(true);
        let snapshot = ResourceSnapshot {
            revision: 1,
            sessions: vec![SessionSnapshot {
                id: SessionId::new(),
                name: "project".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/project")),
                },
                closing: false,
                workspaces: vec![WorkspaceSnapshot {
                    id: WorkspaceId::new(),
                    name: "main".into(),
                    root: PathBuf::from("/project"),
                    closing: false,
                    tabs: vec![TabSnapshot {
                        id: TabId::new(),
                        name: "agents".into(),
                        closing: false,
                        layout: SplitTree::leaf(candidate.id),
                        panes: vec![candidate.clone(), seen.clone(), current.clone(), closing],
                    }],
                }],
            }],
        };
        let mut notifications = NotificationState::default();
        assert!(notifications.observe(seen.terminal_id, 1));

        assert_eq!(
            notifications.next(&snapshot, current.terminal_id),
            Some(candidate.id)
        );
        assert!(notifications.observe(candidate.terminal_id, 1));
        assert_eq!(notifications.next(&snapshot, current.terminal_id), None);
    }

    #[test]
    fn notification_lists_and_next_skip_every_closing_ancestor() {
        let closing_session = completed_pane(false);
        let closing_workspace = completed_pane(false);
        let closing_tab = completed_pane(false);
        let closing_pane = completed_pane(true);
        let candidate = completed_pane(false);
        let current = completed_pane(false);
        let open_session_id = SessionId::new();
        let project = || Project {
            identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/project")),
        };
        let tab = |name: &str, closing: bool, panes: Vec<PaneSnapshot>| TabSnapshot {
            id: TabId::new(),
            name: name.into(),
            closing,
            layout: SplitTree::leaf(panes[0].id),
            panes,
        };
        let workspace = |name: &str, closing: bool, tabs: Vec<TabSnapshot>| WorkspaceSnapshot {
            id: WorkspaceId::new(),
            name: name.into(),
            root: PathBuf::from("/project"),
            closing,
            tabs,
        };
        let snapshot = ResourceSnapshot {
            revision: 1,
            sessions: vec![
                SessionSnapshot {
                    id: SessionId::new(),
                    name: "closing-session".into(),
                    project: project(),
                    closing: true,
                    workspaces: vec![workspace(
                        "main",
                        false,
                        vec![tab("tab", false, vec![closing_session.clone()])],
                    )],
                },
                SessionSnapshot {
                    id: SessionId::new(),
                    name: "closing-workspace".into(),
                    project: project(),
                    closing: false,
                    workspaces: vec![workspace(
                        "main",
                        true,
                        vec![tab("tab", false, vec![closing_workspace.clone()])],
                    )],
                },
                SessionSnapshot {
                    id: open_session_id,
                    name: "open".into(),
                    project: project(),
                    closing: false,
                    workspaces: vec![workspace(
                        "main",
                        false,
                        vec![
                            tab("closing-tab", true, vec![closing_tab.clone()]),
                            tab(
                                "open-tab",
                                false,
                                vec![closing_pane, candidate.clone(), current.clone()],
                            ),
                        ],
                    )],
                },
            ],
        };
        let mut notifications = NotificationState::default();
        assert!(notifications.observe(current.terminal_id, 1));

        let waiting = notifications.waiting(&snapshot);
        assert_eq!(
            waiting
                .iter()
                .map(|row| row.terminal_id)
                .collect::<Vec<_>>(),
            [candidate.terminal_id]
        );
        assert_eq!(notifications.waiting_count(&snapshot), 1);
        assert_eq!(
            notifications.session_waiting_count(&snapshot, open_session_id),
            1
        );
        assert_eq!(
            notifications.next(&snapshot, current.terminal_id),
            Some(candidate.id)
        );
        assert!(notifications.observe(candidate.terminal_id, 1));
        assert_eq!(notifications.next(&snapshot, current.terminal_id), None);
    }
}
