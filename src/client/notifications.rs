use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};

use crate::{
    alerts::{ClientAlertSnapshot, TerminalAlertSnapshot},
    domain::{PaneId, ProgramStatus, SessionId, StatusRecord, StatusState, TerminalId},
    resources::{PaneSnapshot, ResourceSnapshot},
};

use super::chrome::truncate;
use super::config::SpinnerConfig;
use super::dialog::{
    dialog_area, fill_row, frame_inner, render_footer, render_frame, render_list_scrollbar,
    render_title, row_style,
};
use super::federation::{Generation, MachineId, State as FederationState};
use super::navigator::NavigatorSelection;

const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 16;

pub(super) fn spinner_marker(spinner: &SpinnerConfig, elapsed_ms: usize) -> &str {
    spinner.frame(elapsed_ms)
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
    UnreadBlocked,
    Bell,
    UnreadError,
    Working,
    Blocked,
    Error,
    Completed,
}

impl ActivityIndicator {
    pub(super) fn marker(self, elapsed_ms: usize, spinner: &SpinnerConfig) -> &str {
        match self {
            Self::Bell => "!",
            Self::Working => spinner_marker(spinner, elapsed_ms),
            Self::UnreadBlocked | Self::Blocked => "!",
            Self::UnreadError | Self::Error => "×",
            Self::Completed => "•",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ActivityRollup {
    pub indicator: Option<ActivityIndicator>,
    pub progress: Option<u8>,
}

pub(super) fn activity_priority(activity: ActivityIndicator) -> u8 {
    match activity {
        ActivityIndicator::UnreadBlocked => 7,
        ActivityIndicator::Bell => 6,
        ActivityIndicator::UnreadError => 5,
        ActivityIndicator::Completed => 4,
        ActivityIndicator::Blocked => 3,
        ActivityIndicator::Error => 2,
        ActivityIndicator::Working => 1,
    }
}

fn unread_attention(activity: &ProgramStatus) -> Option<(AgentNotificationKind, &StatusRecord)> {
    activity
        .attention_candidates()
        .filter(|record| record.attention_revision > activity.read_revision)
        .filter_map(|record| {
            let kind = match record.state {
                StatusState::Blocked => AgentNotificationKind::Blocked,
                StatusState::Error => AgentNotificationKind::Error,
                StatusState::Done => AgentNotificationKind::Done,
                StatusState::Idle | StatusState::Working => return None,
            };
            Some((kind, record))
        })
        .max_by_key(|(kind, record)| {
            (
                notification_priority(NotificationKind::Agent(*kind)),
                record.attention_revision,
            )
        })
}

fn notification_priority(kind: NotificationKind) -> u8 {
    match kind {
        NotificationKind::Agent(AgentNotificationKind::Blocked) => 3,
        NotificationKind::Bell => 2,
        NotificationKind::Agent(AgentNotificationKind::Error) => 1,
        NotificationKind::Agent(AgentNotificationKind::Done) => 0,
    }
}

#[derive(Clone, Default)]
pub(super) struct NotificationState {
    alerts: ClientAlertSnapshot,
}

impl NotificationState {
    pub(super) fn accept_alerts(&mut self, alerts: ClientAlertSnapshot) -> bool {
        if alerts.revision < self.alerts.revision {
            return false;
        }
        self.alerts = alerts;
        true
    }

    pub(super) fn terminal_alert(&self, terminal_id: TerminalId) -> Option<TerminalAlertSnapshot> {
        self.alerts
            .terminals
            .iter()
            .find(|alert| alert.terminal_id == terminal_id)
            .copied()
    }

    fn agent_acknowledgement(&self, pane: &PaneSnapshot) -> Option<AttentionAcknowledgement> {
        unread_attention(&pane.activity)?;
        pane.activity
            .attention()
            .map(|attention| AttentionAcknowledgement::Agent {
                terminal_id: pane.terminal_id,
                event_revision: attention.attention_revision,
            })
    }

    fn alert_acknowledgement(&self, pane: &PaneSnapshot) -> Option<AttentionAcknowledgement> {
        self.terminal_alert(pane.terminal_id)
            .filter(|alert| alert.unseen())
            .map(|alert| AttentionAcknowledgement::Alert {
                terminal_id: pane.terminal_id,
                observed: alert.state.cursor(),
            })
    }

    pub(super) fn is_unseen(&self, pane: &PaneSnapshot) -> bool {
        self.agent_acknowledgement(pane).is_some() || self.alert_acknowledgement(pane).is_some()
    }

    pub(super) fn indicator(&self, panes: &[PaneSnapshot]) -> Option<ActivityIndicator> {
        self.rollup(panes.iter()).indicator
    }

    pub(super) fn agent_indicator(&self, panes: &[PaneSnapshot]) -> Option<ActivityIndicator> {
        self.agent_rollup(panes.iter()).indicator
    }

    pub(super) fn rollup<'a>(
        &self,
        panes: impl IntoIterator<Item = &'a PaneSnapshot>,
    ) -> ActivityRollup {
        self.rollup_with_bells(panes, true)
    }

    pub(super) fn agent_rollup<'a>(
        &self,
        panes: impl IntoIterator<Item = &'a PaneSnapshot>,
    ) -> ActivityRollup {
        self.rollup_with_bells(panes, false)
    }

    fn rollup_with_bells<'a>(
        &self,
        panes: impl IntoIterator<Item = &'a PaneSnapshot>,
        include_bells: bool,
    ) -> ActivityRollup {
        let mut indicator = None;
        let mut best_priority = 0;
        let mut active_panes = 0;
        let mut progress = None;
        let mut consider = |candidate| {
            let priority = activity_priority(candidate);
            if priority > best_priority {
                best_priority = priority;
                indicator = Some(candidate);
            }
        };

        for pane in panes {
            if let Some((kind, _)) = unread_attention(&pane.activity) {
                consider(match kind {
                    AgentNotificationKind::Blocked => ActivityIndicator::UnreadBlocked,
                    AgentNotificationKind::Done => ActivityIndicator::Completed,
                    AgentNotificationKind::Error => ActivityIndicator::UnreadError,
                });
            }
            if include_bells
                && self
                    .terminal_alert(pane.terminal_id)
                    .is_some_and(TerminalAlertSnapshot::unseen)
            {
                consider(ActivityIndicator::Bell);
            }
            let state = pane.activity.summary_state();
            match state {
                StatusState::Working => consider(ActivityIndicator::Working),
                StatusState::Blocked => consider(ActivityIndicator::Blocked),
                StatusState::Error => consider(ActivityIndicator::Error),
                StatusState::Idle | StatusState::Done => {}
            }
            if pane
                .activity
                .records
                .iter()
                .any(|record| matches!(record.state, StatusState::Working | StatusState::Blocked))
            {
                active_panes += 1;
                progress = (active_panes == 1)
                    .then(|| pane.activity.progress())
                    .flatten();
            }
        }
        ActivityRollup {
            indicator,
            progress,
        }
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
                        if pane.closing {
                            continue;
                        }
                        // One row per terminal: a program that rings the bell
                        // and reports attention is still one terminal waiting.
                        // The row carries the stronger kind and clears both.
                        let bell = self
                            .terminal_alert(pane.terminal_id)
                            .filter(|alert| alert.unseen())
                            .map(|alert| (NotificationKind::Bell, alert.state.last_bell_at_ms));
                        let agent = unread_attention(&pane.activity).map(|(kind, attention)| {
                            (NotificationKind::Agent(kind), attention.updated_at_ms)
                        });
                        let Some((kind, occurred_at_ms)) = [agent, bell]
                            .into_iter()
                            .flatten()
                            .max_by_key(|(kind, occurred_at_ms)| {
                                (notification_priority(*kind), *occurred_at_ms)
                            })
                        else {
                            continue;
                        };
                        waiting.push(WaitingTerminal {
                            machine: MachineId::Local,
                            generation: Generation::default(),
                            machine_label: None,
                            selectable: true,
                            session_id: session.id,
                            pane_id: pane.id,
                            terminal_id: pane.terminal_id,
                            session: session.name.clone(),
                            workspace: workspace.name.clone(),
                            tab: tab.name.clone(),
                            kind,
                            occurred_at_ms,
                            acknowledgements: [
                                self.agent_acknowledgement(pane),
                                self.alert_acknowledgement(pane),
                            ],
                        });
                    }
                }
            }
        }
        waiting.sort_by_key(|row| {
            std::cmp::Reverse((notification_priority(row.kind), row.occurred_at_ms))
        });
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

    fn next_target(
        &self,
        snapshot: &ResourceSnapshot,
        current: TerminalId,
    ) -> Option<AttentionTarget> {
        let panes = open_panes(snapshot).collect::<Vec<_>>();
        let start = panes
            .iter()
            .position(|pane| pane.terminal_id == current)
            .map_or(0, |index| index + 1);
        (0..panes.len())
            .map(|offset| panes[(start + offset) % panes.len()])
            .find(|pane| self.is_unseen(pane))
            .map(|pane| AttentionTarget {
                pane_id: pane.id,
                terminal_id: pane.terminal_id,
                acknowledgements: [
                    self.agent_acknowledgement(pane),
                    self.alert_acknowledgement(pane),
                ],
            })
    }

    pub(super) fn next_action(
        &self,
        snapshot: &ResourceSnapshot,
        current: TerminalId,
        current_was_covered: bool,
    ) -> Option<DirectAttentionAction> {
        let target = self.next_target(snapshot, current)?;
        if target.terminal_id != current {
            return Some(DirectAttentionAction::Navigate(target.pane_id));
        }
        if current_was_covered {
            Some(DirectAttentionAction::RevealCurrent)
        } else {
            Some(DirectAttentionAction::AcknowledgeCurrent(
                target.acknowledgements,
            ))
        }
    }

    pub(super) fn has_working(&self, snapshot: &ResourceSnapshot) -> bool {
        open_panes(snapshot).any(|pane| pane.activity.is_working())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WaitingTerminal {
    pub machine: MachineId,
    pub generation: Generation,
    pub machine_label: Option<String>,
    pub selectable: bool,
    pub session_id: SessionId,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub session: String,
    pub workspace: String,
    pub tab: String,
    pub kind: NotificationKind,
    pub occurred_at_ms: u64,
    pub acknowledgements: [Option<AttentionAcknowledgement>; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttentionAcknowledgement {
    Agent {
        terminal_id: TerminalId,
        event_revision: u64,
    },
    Alert {
        terminal_id: TerminalId,
        observed: crate::alerts::AlertCursor,
    },
}

struct AttentionTarget {
    pane_id: PaneId,
    terminal_id: TerminalId,
    acknowledgements: [Option<AttentionAcknowledgement>; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DirectAttentionAction {
    Navigate(PaneId),
    RevealCurrent,
    AcknowledgeCurrent([Option<AttentionAcknowledgement>; 2]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NotificationKind {
    Agent(AgentNotificationKind),
    Bell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentNotificationKind {
    Blocked,
    Done,
    Error,
}

pub(super) struct NotificationsDialog {
    rows: Vec<WaitingTerminal>,
    selected: usize,
    scroll: usize,
    federated: bool,
}

pub(super) enum NotificationsAction {
    Stay,
    Close,
    Select(NavigatorSelection),
    Acknowledge(MachineId, Generation, [Option<AttentionAcknowledgement>; 2]),
}

impl NotificationsDialog {
    pub(super) fn open(snapshot: &ResourceSnapshot, notifications: &NotificationState) -> Self {
        Self {
            rows: notifications.waiting(snapshot),
            selected: 0,
            scroll: 0,
            federated: false,
        }
    }

    pub(super) fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        notifications: &NotificationState,
    ) {
        if self.federated {
            // Federated rows carry machine labels and remote terminals; the
            // federation state delivers local updates with them intact.
            return;
        }
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

    pub(super) fn accept_federation(
        &mut self,
        state: &FederationState,
        active: MachineId,
        active_notifications: &NotificationState,
    ) {
        if state.registry.iter().count() <= 1 {
            return;
        }
        self.federated = true;
        let selected = self
            .rows
            .get(self.selected)
            .map(|row| (row.machine, row.terminal_id));
        let mut rows = Vec::new();
        for (machine, endpoint) in state.registry.iter() {
            let Some(resources) = endpoint.metadata.resources.as_ref() else {
                continue;
            };
            let current = endpoint.is_current(resources);
            let mut notifications = NotificationState::default();
            if machine == active {
                notifications = active_notifications.clone();
            } else if let Some(alerts) = endpoint
                .metadata
                .alerts
                .as_ref()
                .filter(|alerts| alerts.generation == resources.generation)
            {
                notifications.accept_alerts(alerts.value.clone());
            }
            let label = endpoint.spec.label();
            let mut waiting = notifications.waiting(&resources.value);
            for row in &mut waiting {
                row.machine = machine;
                row.generation = endpoint.generation;
                row.machine_label = Some(if current {
                    label.to_owned()
                } else {
                    format!("{label} · stale")
                });
                row.selectable = current;
            }
            rows.extend(waiting);
        }
        rows.sort_by_key(|row| {
            std::cmp::Reverse((notification_priority(row.kind), row.occurred_at_ms))
        });
        self.rows = rows;
        self.selected = selected
            .and_then(|selected| {
                self.rows
                    .iter()
                    .position(|row| (row.machine, row.terminal_id) == selected)
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
                    .filter(|row| row.selectable)
                    .map_or(NotificationsAction::Stay, |row| {
                        NotificationsAction::Select(NavigatorSelection {
                            machine: row.machine,
                            generation: row.generation,
                            selector: crate::resources::TargetSelector::Pane(row.pane_id),
                        })
                    });
            }
            KeyCode::Char('c') => {
                return self
                    .rows
                    .get(self.selected)
                    .filter(|row| row.selectable)
                    .map_or(NotificationsAction::Stay, |row| {
                        NotificationsAction::Acknowledge(
                            row.machine,
                            row.generation,
                            row.acknowledgements,
                        )
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
            render_title(area, " terminals waiting · alerts", buffer);
        }
        if self.rows.is_empty() {
            if body_height > 0 {
                buffer.set_string(
                    area.x,
                    area.y + header,
                    " No alerts or notifications",
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
                    NotificationKind::Agent(AgentNotificationKind::Blocked)
                    | NotificationKind::Bell => "!",
                    NotificationKind::Agent(AgentNotificationKind::Done) => "•",
                    NotificationKind::Agent(AgentNotificationKind::Error) => "×",
                };
                let kind = match row.kind {
                    NotificationKind::Agent(AgentNotificationKind::Blocked) => "agent blocked",
                    NotificationKind::Agent(AgentNotificationKind::Done) => "agent done",
                    NotificationKind::Agent(AgentNotificationKind::Error) => "agent error",
                    NotificationKind::Bell => "bell",
                };
                let age = age(row.occurred_at_ms);
                let text = format!(
                    " {marker} {kind}  {} › {} › {}{}  {age}",
                    row.session,
                    row.workspace,
                    row.tab,
                    row.machine_label
                        .as_deref()
                        .map_or_else(String::new, |machine| format!(" · {machine}")),
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
            render_footer(
                area,
                " ↑↓/jk move  enter switch  c clear  esc cancel",
                buffer,
            );
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
        domain::{ProgramStatus, StatusRecord, TabId, WorkspaceId},
        resources::{Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot},
        splits::SplitTree,
    };

    fn program_status(
        state: StatusState,
        revision: u64,
        attention_revision: u64,
        read_revision: u64,
        updated_at_ms: u64,
    ) -> ProgramStatus {
        let mut status = ProgramStatus::default();
        status.records.push(StatusRecord {
            id: String::new(),
            state,
            kind: None,
            progress: None,
            app: Some("agent".into()),
            title: None,
            message: None,
            revision,
            updated_at_ms,
            attention_revision,
        });
        status.revision = revision;
        status.attention_revision = attention_revision;
        status.read_revision = read_revision;
        status
    }

    fn completed_pane(closing: bool) -> PaneSnapshot {
        PaneSnapshot {
            name: None,
            tokens: Default::default(),
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing,
            activity: program_status(StatusState::Done, 9, 1, 0, 10),
            cwd: None,
            worktree: None,
        }
    }

    fn status_pane(
        state: StatusState,
        attention_revision: u64,
        read_revision: u64,
    ) -> PaneSnapshot {
        let mut pane = completed_pane(false);
        pane.activity = program_status(
            state,
            100 + attention_revision,
            attention_revision,
            read_revision,
            10 + attention_revision,
        );
        pane
    }

    fn snapshot_with_panes(panes: Vec<PaneSnapshot>) -> ResourceSnapshot {
        ResourceSnapshot {
            revision: 1,
            sessions: vec![SessionSnapshot {
                tokens: Default::default(),
                id: SessionId::new(),
                name: "project".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/project")),
                },
                trusted_project_config: None,
                closing: false,
                workspaces: vec![WorkspaceSnapshot {
                    tokens: Default::default(),
                    id: WorkspaceId::new(),
                    parent_workspace_id: None,
                    name: "main".into(),
                    root: PathBuf::from("/project"),
                    closing: false,
                    tabs: vec![TabSnapshot {
                        tokens: Default::default(),
                        id: TabId::new(),
                        name: "agents".into(),
                        closing: false,
                        layout: SplitTree::leaf(panes[0].id),
                        panes,
                    }],
                }],
            }],
        }
    }

    #[test]
    fn spinner_uses_braille_frames() {
        let spinner = SpinnerConfig::default();
        assert_eq!(ActivityIndicator::Working.marker(0, &spinner), "⠋");
        assert_eq!(ActivityIndicator::Working.marker(800, &spinner), "⠋");
    }

    #[test]
    fn unseen_comes_from_daemon_activity_revision() {
        let terminal_id = TerminalId::new();
        let pane = PaneSnapshot {
            tokens: Default::default(),
            id: PaneId::new(),
            terminal_id,
            closing: false,
            name: None,
            activity: program_status(StatusState::Done, 9, 4, 0, 10),
            cwd: None,
            worktree: None,
        };
        let notifications = NotificationState::default();
        assert!(notifications.is_unseen(&pane));
        let mut read = pane.clone();
        read.activity.read_revision = 4;
        assert!(!notifications.is_unseen(&read));
    }

    #[test]
    fn observing_an_older_render_does_not_hide_a_newer_completion() {
        let terminal_id = TerminalId::new();
        let pane = PaneSnapshot {
            tokens: Default::default(),
            id: PaneId::new(),
            terminal_id,
            closing: false,
            name: None,
            activity: program_status(StatusState::Done, 11, 5, 4, 20),
            cwd: None,
            worktree: None,
        };
        let notifications = NotificationState::default();
        assert!(notifications.is_unseen(&pane));
    }

    #[test]
    fn terminal_bells_roll_up_ahead_of_agent_completion() {
        let pane = completed_pane(false);
        let mut notifications = NotificationState::default();
        notifications.accept_alerts(ClientAlertSnapshot {
            revision: 9,
            terminals: vec![TerminalAlertSnapshot {
                terminal_id: pane.terminal_id,
                state: crate::alerts::TerminalAlertState {
                    bell_count: 2,
                    last_bell_at_ms: 11,
                },
                seen: crate::alerts::AlertCursor::default(),
            }],
        });

        // Agent completion and BEL fold into one waiting terminal; BEL wins
        // the row kind and the single ancestry marker.
        let waiting = notifications.waiting(&snapshot_with_panes(vec![pane.clone()]));
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].kind, NotificationKind::Bell);
        assert!(waiting[0].acknowledgements.iter().all(Option::is_some));
        assert_eq!(
            notifications.indicator(&[pane]),
            Some(ActivityIndicator::Bell)
        );
    }

    #[test]
    fn status_rollup_uses_the_exact_attention_bell_error_and_working_priority() {
        let working = status_pane(StatusState::Working, 0, 0);
        let error = status_pane(StatusState::Error, 0, 0);
        let blocked = status_pane(StatusState::Blocked, 0, 0);
        let done = status_pane(StatusState::Done, 4, 0);
        let unread_error = status_pane(StatusState::Error, 5, 0);
        let unread_blocked = status_pane(StatusState::Blocked, 6, 0);
        let mut notifications = NotificationState::default();
        notifications.accept_alerts(ClientAlertSnapshot {
            revision: 1,
            terminals: vec![TerminalAlertSnapshot {
                terminal_id: working.terminal_id,
                state: crate::alerts::TerminalAlertState {
                    bell_count: 1,
                    last_bell_at_ms: 20,
                },
                seen: crate::alerts::AlertCursor::default(),
            }],
        });

        assert_eq!(
            NotificationState::default().indicator(&[working.clone(), error.clone()]),
            Some(ActivityIndicator::Error)
        );
        assert_eq!(
            NotificationState::default().indicator(&[error, blocked.clone()]),
            Some(ActivityIndicator::Blocked)
        );
        assert_eq!(
            NotificationState::default().indicator(&[blocked, done.clone()]),
            Some(ActivityIndicator::Completed)
        );
        assert_eq!(
            NotificationState::default().indicator(&[done, unread_error.clone()]),
            Some(ActivityIndicator::UnreadError)
        );
        assert_eq!(
            notifications.indicator(&[working.clone(), unread_error]),
            Some(ActivityIndicator::Bell)
        );
        assert_eq!(
            notifications.indicator(&[working, unread_blocked]),
            Some(ActivityIndicator::UnreadBlocked)
        );
        assert_eq!(
            ActivityIndicator::Error.marker(0, &SpinnerConfig::default()),
            "×"
        );
    }

    #[test]
    fn one_terminal_scans_every_unread_record_and_clears_agent_then_bell() {
        let mut pane = status_pane(StatusState::Done, 5, 0);
        pane.activity.records[0].id = "new-result".into();
        pane.activity.records.push(StatusRecord {
            id: "old-request".into(),
            state: StatusState::Blocked,
            kind: None,
            progress: None,
            app: Some("agent".into()),
            title: None,
            message: None,
            revision: 4,
            updated_at_ms: 14,
            attention_revision: 4,
        });
        let mut notifications = NotificationState::default();
        notifications.accept_alerts(ClientAlertSnapshot {
            revision: 1,
            terminals: vec![TerminalAlertSnapshot {
                terminal_id: pane.terminal_id,
                state: crate::alerts::TerminalAlertState {
                    bell_count: 1,
                    last_bell_at_ms: 30,
                },
                seen: crate::alerts::AlertCursor::default(),
            }],
        });

        assert_eq!(
            notifications.indicator(std::slice::from_ref(&pane)),
            Some(ActivityIndicator::UnreadBlocked),
            "an older unread blocker beats a newer completion and a bell"
        );
        let snapshot = snapshot_with_panes(vec![pane.clone()]);
        let waiting = notifications.waiting(&snapshot);
        assert!(
            matches!(
                waiting.as_slice(),
                [WaitingTerminal {
                    kind: NotificationKind::Agent(AgentNotificationKind::Blocked),
                    acknowledgements: [
                        Some(AttentionAcknowledgement::Agent {
                            event_revision: 5,
                            ..
                        }),
                        Some(AttentionAcknowledgement::Alert { .. }),
                    ],
                    ..
                }]
            ),
            "a terminal with attention and a bell is one waiting terminal"
        );
        assert_eq!(notifications.waiting_count(&snapshot), 1);
        let mut dialog = NotificationsDialog::open(&snapshot, &notifications);
        assert!(matches!(
            dialog.key(
                KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::NONE),
                10
            ),
            NotificationsAction::Acknowledge(
                _,
                _,
                [
                    Some(AttentionAcknowledgement::Agent {
                        event_revision: 5,
                        ..
                    }),
                    Some(AttentionAcknowledgement::Alert { .. }),
                ]
            )
        ));

        pane.activity.read_revision = 5;
        let snapshot = snapshot_with_panes(vec![pane.clone()]);
        assert_eq!(
            notifications.indicator(std::slice::from_ref(&pane)),
            Some(ActivityIndicator::Bell)
        );
        assert!(matches!(
            notifications.waiting(&snapshot).as_slice(),
            [WaitingTerminal {
                kind: NotificationKind::Bell,
                acknowledgements: [None, Some(AttentionAcknowledgement::Alert { .. })],
                ..
            }]
        ));
        let mut dialog = NotificationsDialog::open(&snapshot, &notifications);
        assert!(matches!(
            dialog.key(
                KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::NONE),
                10
            ),
            NotificationsAction::Acknowledge(
                _,
                _,
                [None, Some(AttentionAcknowledgement::Alert { .. })]
            )
        ));

        notifications.accept_alerts(ClientAlertSnapshot {
            revision: 2,
            terminals: vec![TerminalAlertSnapshot {
                terminal_id: pane.terminal_id,
                state: crate::alerts::TerminalAlertState {
                    bell_count: 1,
                    last_bell_at_ms: 30,
                },
                seen: crate::alerts::AlertCursor { bell_count: 1 },
            }],
        });
        assert_eq!(
            notifications.indicator(std::slice::from_ref(&pane)),
            Some(ActivityIndicator::Blocked),
            "reading attention does not delete the stored blocker"
        );
        assert!(notifications.waiting(&snapshot).is_empty());
    }

    #[test]
    fn progress_aggregates_independently_from_the_winning_indicator() {
        let mut working = status_pane(StatusState::Working, 0, 0);
        working.activity.records[0].progress = Some(40);
        let done = status_pane(StatusState::Done, 5, 0);
        let mut notifications = NotificationState::default();
        notifications.accept_alerts(ClientAlertSnapshot {
            revision: 1,
            terminals: vec![TerminalAlertSnapshot {
                terminal_id: working.terminal_id,
                state: crate::alerts::TerminalAlertState {
                    bell_count: 1,
                    last_bell_at_ms: 20,
                },
                seen: crate::alerts::AlertCursor::default(),
            }],
        });

        assert_eq!(
            notifications.rollup([&working]),
            ActivityRollup {
                indicator: Some(ActivityIndicator::Bell),
                progress: Some(40),
            }
        );
        assert_eq!(
            NotificationState::default().rollup([&working, &done]),
            ActivityRollup {
                indicator: Some(ActivityIndicator::Completed),
                progress: Some(40),
            }
        );

        let mut with_old_error = working.clone();
        let mut old_error = status_pane(StatusState::Error, 0, 0)
            .activity
            .records
            .remove(0);
        old_error.id = "previous".into();
        old_error.revision = 99;
        with_old_error.activity.records.push(old_error);
        assert_eq!(
            NotificationState::default().rollup([&with_old_error]),
            ActivityRollup {
                indicator: Some(ActivityIndicator::Error),
                progress: Some(40),
            }
        );

        let mut other = status_pane(StatusState::Working, 0, 0);
        other.activity.records[0].progress = Some(80);
        assert_eq!(
            NotificationState::default()
                .rollup([&with_old_error, &other])
                .progress,
            None
        );
        assert_eq!(
            NotificationState::default()
                .rollup([&working, &other])
                .progress,
            None
        );
        other.activity.records[0].progress = None;
        assert_eq!(
            NotificationState::default()
                .rollup([&working, &other])
                .progress,
            None
        );
    }

    #[test]
    fn working_animation_follows_any_working_record_not_only_the_summary() {
        let mut pane = status_pane(StatusState::Working, 0, 0);
        pane.activity.records.push(StatusRecord {
            id: "request".into(),
            state: StatusState::Blocked,
            kind: None,
            progress: None,
            app: Some("agent".into()),
            title: None,
            message: None,
            revision: 2,
            updated_at_ms: 2,
            attention_revision: 0,
        });
        assert_eq!(pane.activity.summary_state(), StatusState::Blocked);

        let snapshot = snapshot_with_panes(vec![pane]);
        assert!(NotificationState::default().has_working(&snapshot));
    }

    #[test]
    fn read_done_keeps_its_stored_state_but_quiets_the_completion_bullet() {
        let pane = status_pane(StatusState::Done, 7, 7);
        let notifications = NotificationState::default();

        assert_eq!(pane.activity.summary_state(), StatusState::Done);
        assert_eq!(notifications.agent_indicator(&[pane]), None);
    }

    #[test]
    fn federated_notifications_pair_alerts_with_their_resource_generation() {
        use crate::client::federation::{EndpointSpec, Snapshot, Status};

        let pane = status_pane(StatusState::Done, 5, 0);
        let terminal_id = pane.terminal_id;
        let snapshot = snapshot_with_panes(vec![pane]);
        let machine = crate::machines::Machine {
            id: uuid::Uuid::new_v4(),
            label: "remote".into(),
            target: "remote".into(),
            enabled: true,
        };
        let remote = MachineId::Ssh(machine.id);
        let generation = Generation {
            supervisor: 1,
            connection: 2,
        };
        let previous = Generation {
            connection: 1,
            ..generation
        };
        let mut state = FederationState::fixture(vec![
            (
                EndpointSpec::Local,
                Status::Ready,
                Generation::default(),
                None,
            ),
            (
                EndpointSpec::Ssh(machine),
                Status::Ready,
                generation,
                Some(snapshot.clone()),
            ),
        ]);
        state.fixture_metadata_mut(remote).alerts = Some(Snapshot {
            generation: previous,
            value: ClientAlertSnapshot {
                revision: 1,
                terminals: vec![TerminalAlertSnapshot {
                    terminal_id,
                    state: crate::alerts::TerminalAlertState {
                        bell_count: 1,
                        last_bell_at_ms: 20,
                    },
                    seen: crate::alerts::AlertCursor::default(),
                }],
            },
        });
        let notifications = NotificationState::default();
        let mut dialog = NotificationsDialog::open(&snapshot, &notifications);
        dialog.accept_federation(&state, MachineId::Local, &notifications);
        assert_eq!(dialog.rows.len(), 1);
        assert_eq!(
            dialog.rows[0].kind,
            NotificationKind::Agent(AgentNotificationKind::Done)
        );
        assert!(dialog.rows[0].selectable);

        state
            .fixture_metadata_mut(remote)
            .alerts
            .as_mut()
            .unwrap()
            .generation = generation;
        dialog.accept_federation(&state, MachineId::Local, &notifications);
        assert_eq!(dialog.rows.len(), 1, "the bell joins the terminal's row");
        assert_eq!(dialog.rows[0].kind, NotificationKind::Bell);
        assert!(dialog.rows[0].acknowledgements.iter().all(Option::is_some));
        assert!(dialog.rows.iter().all(|row| row.selectable));

        let metadata = state.fixture_metadata_mut(remote);
        metadata.resources.as_mut().unwrap().generation = previous;
        metadata.alerts.as_mut().unwrap().generation = previous;
        dialog.accept_federation(&state, MachineId::Local, &notifications);
        assert_eq!(dialog.rows.len(), 1);
        assert!(
            dialog.rows.iter().all(
                |row| !row.selectable && row.machine_label.as_deref() == Some("remote · stale")
            )
        );
    }

    #[test]
    fn error_notifications_acknowledge_attention_revision_not_record_revision() {
        let pane = status_pane(StatusState::Error, 7, 6);
        assert_eq!(pane.activity.records[0].revision, 107);
        let notifications = NotificationState::default();
        let waiting = notifications.waiting(&snapshot_with_panes(vec![pane.clone()]));

        assert!(matches!(
            waiting.as_slice(),
            [WaitingTerminal {
                kind: NotificationKind::Agent(AgentNotificationKind::Error),
                acknowledgements: [
                    Some(AttentionAcknowledgement::Agent {
                        terminal_id,
                        event_revision: 7,
                    }),
                    None,
                ],
                ..
            }] if *terminal_id == pane.terminal_id
        ));
        assert!(notifications.is_unseen(&pane));
        let mut stale = pane;
        stale.activity.read_revision = 6;
        assert!(notifications.is_unseen(&stale));
    }

    #[test]
    fn next_wraps_in_resource_order_with_current_as_the_final_candidate() {
        let candidate = completed_pane(false);
        let seen = completed_pane(false);
        let current = completed_pane(false);
        let closing = completed_pane(true);
        let mut seen = seen;
        seen.activity.read_revision = 1;
        let mut snapshot = snapshot_with_panes(vec![
            candidate.clone(),
            seen.clone(),
            current.clone(),
            closing,
        ]);
        let notifications = NotificationState::default();

        assert_eq!(
            notifications.next_action(&snapshot, current.terminal_id, false),
            Some(DirectAttentionAction::Navigate(candidate.id)),
        );
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .read_revision = 1;
        assert_eq!(
            notifications.next_action(&snapshot, current.terminal_id, false),
            Some(DirectAttentionAction::AcknowledgeCurrent([
                Some(AttentionAcknowledgement::Agent {
                    terminal_id: current.terminal_id,
                    event_revision: 1,
                }),
                None,
            ])),
            "current attention follows every other waiting terminal"
        );
        assert_eq!(
            notifications.next_action(&snapshot, current.terminal_id, true),
            Some(DirectAttentionAction::RevealCurrent),
            "covered current attention is revealed before rendering acknowledges it"
        );
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
            tokens: Default::default(),
            id: TabId::new(),
            name: name.into(),
            closing,
            layout: SplitTree::leaf(panes[0].id),
            panes,
        };
        let workspace = |name: &str, closing: bool, tabs: Vec<TabSnapshot>| WorkspaceSnapshot {
            tokens: Default::default(),
            id: WorkspaceId::new(),
            parent_workspace_id: None,
            name: name.into(),
            root: PathBuf::from("/project"),
            closing,
            tabs,
        };
        let mut current = current;
        current.activity.read_revision = 1;
        let mut snapshot = ResourceSnapshot {
            revision: 1,
            sessions: vec![
                SessionSnapshot {
                    tokens: Default::default(),
                    id: SessionId::new(),
                    name: "closing-session".into(),
                    project: project(),
                    trusted_project_config: None,
                    closing: true,
                    workspaces: vec![workspace(
                        "main",
                        false,
                        vec![tab("tab", false, vec![closing_session.clone()])],
                    )],
                },
                SessionSnapshot {
                    tokens: Default::default(),
                    id: SessionId::new(),
                    name: "closing-workspace".into(),
                    project: project(),
                    trusted_project_config: None,
                    closing: false,
                    workspaces: vec![workspace(
                        "main",
                        true,
                        vec![tab("tab", false, vec![closing_workspace.clone()])],
                    )],
                },
                SessionSnapshot {
                    tokens: Default::default(),
                    id: open_session_id,
                    name: "open".into(),
                    project: project(),
                    trusted_project_config: None,
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
        let notifications = NotificationState::default();

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
            notifications.next_action(&snapshot, current.terminal_id, false),
            Some(DirectAttentionAction::Navigate(candidate.id)),
        );
        snapshot.sessions[2].workspaces[0].tabs[1].panes[1]
            .activity
            .read_revision = 1;
        assert_eq!(
            notifications.next_action(&snapshot, current.terminal_id, false),
            None
        );
    }

    #[test]
    fn clear_acknowledges_the_selected_agent_event_without_changing_its_state() {
        let pane = completed_pane(false);
        let snapshot = snapshot_with_panes(vec![pane.clone()]);
        let mut dialog = NotificationsDialog::open(&snapshot, &NotificationState::default());

        assert!(matches!(
            dialog.key(KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::NONE), 10),
            NotificationsAction::Acknowledge(
                _,
                _,
                [
                    Some(AttentionAcknowledgement::Agent {
                        terminal_id,
                        event_revision: 1,
                    }),
                    None,
                ]
            ) if terminal_id == pane.terminal_id
        ));
        assert_eq!(pane.activity.summary_state(), StatusState::Done);
    }

    #[test]
    fn dialog_keeps_its_selection_across_refreshes_of_a_terminal_with_a_bell() {
        let first = status_pane(StatusState::Blocked, 3, 0);
        let second = status_pane(StatusState::Done, 2, 0);
        let mut notifications = NotificationState::default();
        notifications.accept_alerts(ClientAlertSnapshot {
            revision: 1,
            terminals: vec![TerminalAlertSnapshot {
                terminal_id: second.terminal_id,
                state: crate::alerts::TerminalAlertState {
                    bell_count: 1,
                    last_bell_at_ms: 30,
                },
                seen: crate::alerts::AlertCursor::default(),
            }],
        });
        let snapshot = snapshot_with_panes(vec![first.clone(), second.clone()]);
        let mut dialog = NotificationsDialog::open(&snapshot, &notifications);
        assert_eq!(dialog.rows.len(), 2);
        dialog.key(
            KeyEvent::new(KeyCode::Char('j'), crossterm::event::KeyModifiers::NONE),
            10,
        );
        assert_eq!(dialog.rows[dialog.selected].terminal_id, second.terminal_id);

        dialog.accept_resources(&snapshot, &notifications);
        assert_eq!(dialog.selected, 1, "a refresh keeps the selected terminal");
    }

    #[test]
    fn completion_waits_for_working_sub_records_before_notifying() {
        let mut pane = status_pane(StatusState::Done, 5, 0);
        pane.activity.records.push(StatusRecord {
            id: "task-1".into(),
            state: StatusState::Working,
            kind: None,
            progress: Some(40),
            app: None,
            title: None,
            message: None,
            revision: 106,
            updated_at_ms: 16,
            attention_revision: 0,
        });
        let notifications = NotificationState::default();
        let snapshot = snapshot_with_panes(vec![pane.clone()]);

        assert_eq!(
            notifications.rollup(std::slice::from_ref(&pane)),
            ActivityRollup {
                indicator: Some(ActivityIndicator::Working),
                progress: Some(40),
            },
            "a finished root with a working sub-task is still working"
        );
        assert!(notifications.waiting(&snapshot).is_empty());
        assert!(!notifications.is_unseen(&pane));

        pane.activity.records.pop();
        let snapshot = snapshot_with_panes(vec![pane.clone()]);
        assert_eq!(
            notifications.indicator(std::slice::from_ref(&pane)),
            Some(ActivityIndicator::Completed)
        );
        assert!(matches!(
            notifications.waiting(&snapshot).as_slice(),
            [WaitingTerminal {
                kind: NotificationKind::Agent(AgentNotificationKind::Done),
                ..
            }]
        ));

        let mut blocked = status_pane(StatusState::Blocked, 5, 0);
        blocked
            .activity
            .records
            .push(pane.activity.records[0].clone());
        blocked.activity.records[1].id = "task-1".into();
        blocked.activity.records[1].state = StatusState::Working;
        assert_eq!(
            notifications.indicator(std::slice::from_ref(&blocked)),
            Some(ActivityIndicator::UnreadBlocked),
            "blockers still interrupt working sub-tasks"
        );
    }
}
