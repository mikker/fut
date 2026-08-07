use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use uuid::Uuid;

use crate::{
    domain::{PaneId, SessionId, TabId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, TargetSelector},
};

use super::dialog::{
    dialog_area, fill_row, frame_inner, render_footer, render_frame, render_list_scrollbar,
    title_style,
};
use super::notifications::{ActivityIndicator, NotificationState};

const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResourceKey {
    Session(SessionId),
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NavigatorRow {
    pub key: ResourceKey,
    pub depth: u16,
    pub label: String,
    pub current: bool,
    pub closing: bool,
    pub destination: Option<PaneId>,
    pub activity: Option<ActivityIndicator>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NavigatorStatus {
    Loading,
    Ready,
    Empty,
    Error { message: String },
    Switching,
}

pub(super) struct NavigatorState {
    pub rows: Vec<NavigatorRow>,
    pub selected: usize,
    pub scroll: usize,
    pub status: NavigatorStatus,
    pub resource_revision: Option<u64>,
    pub switch_request: Option<Uuid>,
}

pub(super) enum NavigatorAction {
    Stay,
    Close,
    Select(TargetSelector),
}

impl NavigatorState {
    pub fn open(_current: &SelectedTarget) -> Self {
        Self {
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            status: NavigatorStatus::Loading,
            resource_revision: None,
            switch_request: None,
        }
    }

    #[cfg(test)]
    pub fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        current: &SelectedTarget,
    ) -> bool {
        self.accept_resources_with_notifications(snapshot, current, &NotificationState::default())
    }

    pub fn accept_resources_with_notifications(
        &mut self,
        snapshot: &ResourceSnapshot,
        current: &SelectedTarget,
        notifications: &NotificationState,
    ) -> bool {
        if self
            .resource_revision
            .is_some_and(|revision| snapshot.revision <= revision)
        {
            return false;
        }
        let old_key = self.rows.get(self.selected).map(|row| row.key);
        let old_index = self.selected;
        let previous_status = self.status.clone();
        self.rows = flatten_with_notifications(snapshot, current, notifications);
        self.resource_revision = Some(snapshot.revision);
        self.status = match previous_status {
            status @ (NavigatorStatus::Switching | NavigatorStatus::Error { .. }) => status,
            _ if self.rows.is_empty() => NavigatorStatus::Empty,
            _ => NavigatorStatus::Ready,
        };
        self.selected = old_key
            .and_then(|key| self.rows.iter().position(|row| row.key == key))
            .unwrap_or_else(|| old_index.min(self.rows.len().saturating_sub(1)));
        if old_key.is_none()
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| row.key == ResourceKey::Pane(current.pane_id))
        {
            self.selected = index;
        }
        true
    }

    pub fn key(&mut self, key: KeyEvent, visible_rows: usize) -> NavigatorAction {
        if !matches!(
            key.kind,
            crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
        ) {
            return NavigatorAction::Stay;
        }
        if matches!(self.status, NavigatorStatus::Switching) {
            return NavigatorAction::Stay;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            return NavigatorAction::Close;
        }
        let last = self.rows.len().saturating_sub(1);
        let page = visible_rows.max(1);
        match (key.code, key.modifiers) {
            (KeyCode::Up | KeyCode::Char('k'), _) => {
                self.selected = self.selected.saturating_sub(1)
            }
            (KeyCode::Down | KeyCode::Char('j'), _) => {
                self.selected = (self.selected + 1).min(last)
            }
            (KeyCode::Home | KeyCode::Char('g'), _) => self.selected = 0,
            (KeyCode::End | KeyCode::Char('G'), _) => self.selected = last,
            (KeyCode::PageUp, _) => self.selected = self.selected.saturating_sub(page),
            (KeyCode::Char('u'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected = self.selected.saturating_sub(page)
            }
            (KeyCode::PageDown, _) => self.selected = (self.selected + page).min(last),
            (KeyCode::Char('d'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected = (self.selected + page).min(last)
            }
            (KeyCode::Char('s'), _) => self.cycle_depth(0),
            (KeyCode::Char('w'), _) => self.cycle_depth(1),
            (KeyCode::Char('t'), _) => self.cycle_depth(2),
            (KeyCode::Char('p'), _) => self.cycle_depth(3),
            (KeyCode::Char('K'), _) => self.jump_back(1),
            (KeyCode::Char('J'), _) => self.jump_forward(1),
            (KeyCode::Char('['), _) => self.jump_back(2),
            (KeyCode::Char(']'), _) => self.jump_forward(2),
            (KeyCode::Left, _) => self.select_parent(),
            (KeyCode::Right, _) => self.select_first_child(),
            (KeyCode::Enter, _)
                if matches!(
                    self.status,
                    NavigatorStatus::Ready | NavigatorStatus::Error { .. }
                ) =>
            {
                if let Some(row) = self.rows.get(self.selected)
                    && !row.closing
                    && let Some(pane) = row.destination
                {
                    return NavigatorAction::Select(TargetSelector::Pane(pane));
                }
            }
            _ => {}
        }
        self.keep_visible(visible_rows);
        NavigatorAction::Stay
    }

    pub fn begin_switch(&mut self, request: Uuid) {
        self.switch_request = Some(request);
        self.status = NavigatorStatus::Switching;
    }

    pub fn switch_error(&mut self, request: Option<Uuid>, message: String) -> bool {
        if !matches_request(self.switch_request, request) {
            return false;
        }
        self.switch_request = None;
        self.status = NavigatorStatus::Error { message };
        true
    }

    pub fn switch_selected(&mut self, request: Option<Uuid>) -> bool {
        if !matches_request(self.switch_request, request) {
            return false;
        }
        self.switch_request = None;
        true
    }

    /// Cycle through rows of one kind within the selection's enclosing scope:
    /// `s` through sessions, `w` through workspaces of the current session,
    /// `t` through tabs of the current workspace, `p` through panes of the
    /// current tab. From a shallower row, descend to the first such row inside
    /// its subtree.
    fn cycle_depth(&mut self, depth: u16) {
        let Some(current) = self.rows.get(self.selected) else {
            return;
        };
        if current.depth < depth {
            let end = self.subtree_end(self.selected);
            if let Some(index) =
                (self.selected + 1..end).find(|&index| self.rows[index].depth == depth)
            {
                self.selected = index;
            }
            return;
        }
        // Siblings of this kind live between the enclosing ancestor and the
        // next row that is at least as shallow as that ancestor.
        let start = (0..self.selected)
            .rev()
            .find(|&index| self.rows[index].depth < depth)
            .map_or(0, |index| index + 1);
        let end = (start..self.rows.len())
            .find(|&index| self.rows[index].depth < depth)
            .unwrap_or(self.rows.len());
        let mut candidates = (start..end).filter(|&index| self.rows[index].depth == depth);
        let next = candidates
            .clone()
            .find(|&index| index > self.selected)
            .or_else(|| candidates.next());
        if let Some(index) = next {
            self.selected = index;
        }
    }

    fn jump_forward(&mut self, depth: u16) {
        if let Some(index) =
            (self.selected + 1..self.rows.len()).find(|&index| self.rows[index].depth == depth)
        {
            self.selected = index;
        }
    }

    fn jump_back(&mut self, depth: u16) {
        if let Some(index) = (0..self.selected)
            .rev()
            .find(|&index| self.rows[index].depth == depth)
        {
            self.selected = index;
        }
    }

    fn select_parent(&mut self) {
        let Some(current) = self.rows.get(self.selected) else {
            return;
        };
        let depth = current.depth;
        if let Some(index) = (0..self.selected)
            .rev()
            .find(|&index| self.rows[index].depth < depth)
        {
            self.selected = index;
        }
    }

    fn select_first_child(&mut self) {
        // Pre-order: a row's first child is the row right after it, one level
        // deeper.
        let Some(current) = self.rows.get(self.selected) else {
            return;
        };
        if self
            .rows
            .get(self.selected + 1)
            .is_some_and(|next| next.depth == current.depth + 1)
        {
            self.selected += 1;
        }
    }

    /// One past the last row of the subtree rooted at `index`.
    fn subtree_end(&self, index: usize) -> usize {
        let depth = self.rows[index].depth;
        (index + 1..self.rows.len())
            .find(|&next| self.rows[next].depth <= depth)
            .unwrap_or(self.rows.len())
    }

    fn keep_visible(&mut self, height: usize) {
        let height = height.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
    }

    pub fn render(&mut self, host: Rect, spinner_frame: usize, buffer: &mut Buffer) {
        let area = render_frame(dialog_area(host, MAX_WIDTH, MAX_HEIGHT), buffer);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (header, footer) = chrome_rows(area.height);
        if header == 1 {
            let title_row = Rect::new(area.x, area.y, area.width, 1);
            fill_row(title_row, title_style(), buffer);
            put(buffer, area.x, area.y, area.width, " navigator", title_style());
        }
        if footer == 1 {
            let footer = match &self.status {
                NavigatorStatus::Loading => "Loading…".to_owned(),
                NavigatorStatus::Empty => "No resources".to_owned(),
                NavigatorStatus::Switching => "Switching…".to_owned(),
                NavigatorStatus::Error { message } => {
                    format!("Error: {message}  enter retry  esc cancel")
                }
                NavigatorStatus::Ready => match self.rows.get(self.selected) {
                    Some(row) if row.closing => "Closing…  ↑↓/jk move  esc cancel".to_owned(),
                    _ => "↑↓/jk move  s/w/t/p cycle  ←→ tree  enter switch  esc cancel".to_owned(),
                },
            };
            render_footer(area, &format!(" {footer}"), buffer);
        }
        let body_y = area.y + header;
        let body_height = usize::from(area.height - header - footer);
        self.keep_visible(body_height);
        match &self.status {
            NavigatorStatus::Loading => put(
                buffer,
                area.x,
                body_y,
                area.width,
                "Loading…",
                Style::default(),
            ),
            NavigatorStatus::Empty => put(
                buffer,
                area.x,
                body_y,
                area.width,
                "No resources",
                Style::default(),
            ),
            NavigatorStatus::Error { message, .. } if self.rows.is_empty() => put(
                buffer,
                area.x,
                body_y,
                area.width,
                &format!("Error: {message}"),
                Style::default(),
            ),
            NavigatorStatus::Ready | NavigatorStatus::Switching | NavigatorStatus::Error { .. } => {
                for (line, (index, row)) in self
                    .rows
                    .iter()
                    .enumerate()
                    .skip(self.scroll)
                    .take(body_height)
                    .enumerate()
                {
                    let marker = if row.closing {
                        "×"
                    } else if let Some(activity) = row.activity {
                        activity.marker(spinner_frame)
                    } else if row.current && matches!(row.key, ResourceKey::Pane(_)) {
                        "•"
                    } else {
                        " "
                    };
                    let text = format!(
                        "{}{} {}",
                        "  ".repeat(usize::from(row.depth)),
                        marker,
                        row.label
                    );
                    let mut style = Style::default();
                    if row.closing {
                        style = style.add_modifier(Modifier::DIM);
                    }
                    if row.current && !matches!(row.key, ResourceKey::Pane(_)) {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if index == self.selected {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    let y = body_y + line as u16;
                    fill_row(Rect::new(area.x, y, area.width, 1), style, buffer);
                    put(buffer, area.x, y, area.width, &text, style);
                }
                let body = Rect::new(
                    area.x,
                    body_y,
                    area.width,
                    u16::try_from(body_height).expect("body height fits u16"),
                );
                render_list_scrollbar(self.scroll, self.rows.len(), body, buffer);
            }
        }
    }
}

fn chrome_rows(height: u16) -> (u16, u16) {
    match height {
        0..=2 => (0, 0),
        3..=4 => (0, 1),
        _ => (1, 1),
    }
}

/// Rows the dialog body can show inside `host`, so key handling scrolls in
/// step with rendering.
pub(super) fn dialog_body_rows(host: Rect) -> usize {
    let area = frame_inner(dialog_area(host, MAX_WIDTH, MAX_HEIGHT));
    let (header, footer) = chrome_rows(area.height);
    usize::from(area.height.saturating_sub(header + footer))
}

fn matches_request(pending: Option<Uuid>, response: Option<Uuid>) -> bool {
    pending.is_some() && pending == response
}

#[cfg(test)]
pub(super) fn flatten(snapshot: &ResourceSnapshot, current: &SelectedTarget) -> Vec<NavigatorRow> {
    flatten_with_notifications(snapshot, current, &NotificationState::default())
}

fn flatten_with_notifications(
    snapshot: &ResourceSnapshot,
    current: &SelectedTarget,
    notifications: &NotificationState,
) -> Vec<NavigatorRow> {
    let current_ancestry = snapshot.sessions.iter().find_map(|session| {
        session.workspaces.iter().find_map(|workspace| {
            workspace.tabs.iter().find_map(|tab| {
                tab.panes
                    .iter()
                    .find(|pane| pane.id == current.pane_id)
                    .map(|_| (session.id, workspace.id, tab.id))
            })
        })
    });
    let (current_session_id, current_workspace_id, current_tab_id) =
        current_ancestry.unwrap_or((current.session_id, current.workspace_id, current.tab_id));

    let mut rows = Vec::new();
    for session in &snapshot.sessions {
        let session_current = session.id == current_session_id;
        let session_target =
            if session_current && pane_available_session(session, current.pane_id, false) {
                Some(current.pane_id)
            } else {
                first_pane_session(session, false)
            };
        rows.push(NavigatorRow {
            key: ResourceKey::Session(session.id),
            depth: 0,
            label: session.name.clone(),
            current: session_current,
            closing: session.closing,
            destination: (!session.closing).then_some(session_target).flatten(),
            activity: notifications.indicator(
                &session
                    .workspaces
                    .iter()
                    .flat_map(|workspace| &workspace.tabs)
                    .flat_map(|tab| &tab.panes)
                    .copied()
                    .collect::<Vec<_>>(),
            ),
        });
        for workspace in &session.workspaces {
            let closing = session.closing || workspace.closing;
            let workspace_current = session_current && workspace.id == current_workspace_id;
            let target = if workspace_current
                && pane_available_workspace(workspace, current.pane_id, session.closing)
            {
                Some(current.pane_id)
            } else {
                first_pane_workspace(workspace, closing)
            };
            rows.push(NavigatorRow {
                key: ResourceKey::Workspace(workspace.id),
                depth: 1,
                label: workspace.name.clone(),
                current: workspace_current,
                closing,
                destination: (!closing).then_some(target).flatten(),
                activity: notifications.indicator(
                    &workspace
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .copied()
                        .collect::<Vec<_>>(),
                ),
            });
            for tab in &workspace.tabs {
                let tab_current = workspace_current && tab.id == current_tab_id;
                let tab_closing = closing || tab.closing;
                let target = if tab_current
                    && tab
                        .panes
                        .iter()
                        .any(|pane| pane.id == current.pane_id && !pane.closing)
                    && !tab_closing
                {
                    Some(current.pane_id)
                } else {
                    tab.panes
                        .iter()
                        .find(|pane| !tab_closing && !pane.closing)
                        .map(|pane| pane.id)
                };
                rows.push(NavigatorRow {
                    key: ResourceKey::Tab(tab.id),
                    depth: 2,
                    label: tab.name.clone(),
                    current: tab_current,
                    closing: tab_closing,
                    destination: (!tab_closing).then_some(target).flatten(),
                    activity: notifications.indicator(&tab.panes),
                });
                for (index, pane) in tab.panes.iter().enumerate() {
                    let pane_closing = tab_closing || pane.closing;
                    rows.push(NavigatorRow {
                        key: ResourceKey::Pane(pane.id),
                        depth: 3,
                        label: format!("pane {}", index + 1),
                        current: pane.id == current.pane_id,
                        closing: pane_closing,
                        destination: (!pane_closing).then_some(pane.id),
                        activity: notifications.indicator(std::slice::from_ref(pane)),
                    });
                }
            }
        }
    }
    rows
}

fn first_pane_session(
    session: &crate::resources::SessionSnapshot,
    inherited: bool,
) -> Option<PaneId> {
    session
        .workspaces
        .iter()
        .find_map(|workspace| first_pane_workspace(workspace, inherited || session.closing))
}

fn first_pane_workspace(
    workspace: &crate::resources::WorkspaceSnapshot,
    inherited: bool,
) -> Option<PaneId> {
    workspace
        .tabs
        .iter()
        .filter(|tab| !tab.closing)
        .flat_map(|tab| &tab.panes)
        .find(|pane| !inherited && !workspace.closing && !pane.closing)
        .map(|pane| pane.id)
}

fn pane_available_session(
    session: &crate::resources::SessionSnapshot,
    pane_id: PaneId,
    inherited: bool,
) -> bool {
    !inherited
        && !session.closing
        && session
            .workspaces
            .iter()
            .any(|workspace| pane_available_workspace(workspace, pane_id, session.closing))
}

fn pane_available_workspace(
    workspace: &crate::resources::WorkspaceSnapshot,
    pane_id: PaneId,
    inherited: bool,
) -> bool {
    !inherited
        && !workspace.closing
        && workspace.tabs.iter().any(|tab| {
            !tab.closing
                && tab
                    .panes
                    .iter()
                    .any(|pane| pane.id == pane_id && !pane.closing)
        })
}

fn put(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    if width == 0 {
        return;
    }
    let clipped: String = text.chars().take(usize::from(width)).collect();
    buffer.set_stringn(x, y, clipped, usize::from(width), style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::TerminalId,
        resources::{
            PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
        },
    };
    use std::path::PathBuf;

    fn fixture() -> (ResourceSnapshot, SelectedTarget, PaneId) {
        let current_pane = PaneId::new();
        let other_pane = PaneId::new();
        let session_id = SessionId::new();
        let workspace_id = WorkspaceId::new();
        let tab_id = TabId::new();
        let current_terminal = TerminalId::new();
        let mut layout = crate::splits::SplitTree::leaf(current_pane);
        assert!(layout.split(
            current_pane,
            crate::splits::SplitDirection::Right,
            other_pane,
        ));
        (
            ResourceSnapshot {
                revision: 1,
                sessions: vec![SessionSnapshot {
                    id: session_id,
                    name: "sessión 🛰".into(),
                    project: Project {
                        identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/tmp")),
                    },
                    closing: false,
                    workspaces: vec![WorkspaceSnapshot {
                        id: workspace_id,
                        name: "workspace".into(),
                        root: PathBuf::from("/tmp"),
                        closing: false,
                        tabs: vec![TabSnapshot {
                            id: tab_id,
                            name: "tab".into(),
                            closing: false,
                            layout,
                            panes: vec![
                                PaneSnapshot {
                                    id: current_pane,
                                    terminal_id: current_terminal,
                                    closing: false,
                                    activity: Default::default(),
                                },
                                PaneSnapshot {
                                    id: other_pane,
                                    terminal_id: TerminalId::new(),
                                    closing: true,
                                    activity: Default::default(),
                                },
                            ],
                        }],
                    }],
                }],
            },
            SelectedTarget {
                session_id,
                workspace_id,
                tab_id,
                pane_id: current_pane,
                terminal_id: current_terminal,
                child_pid: 1,
            },
            other_pane,
        )
    }

    fn rendered(nav: &mut NavigatorState, width: u16, height: u16) -> (String, Buffer) {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        nav.render(area, 0, &mut buffer);
        let text = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, buffer)
    }

    #[test]
    fn flatten_preserves_hierarchy_current_ancestry_and_closing() {
        let (snapshot, current, closing) = fixture();
        let rows = flatten(&snapshot, &current);
        assert_eq!(
            rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
            [0, 1, 2, 3, 3]
        );
        assert!(rows[..4].iter().all(|row| row.current));
        assert!(
            rows[..3]
                .iter()
                .all(|row| row.destination == Some(current.pane_id))
        );
        let closing = rows
            .iter()
            .find(|row| row.key == ResourceKey::Pane(closing))
            .unwrap();
        assert!(closing.closing);
        assert_eq!(closing.destination, None);
    }

    #[test]
    fn flatten_uses_fresh_ancestry_after_current_pane_moves() {
        let (mut snapshot, mut current, _) = fixture();
        let old_tab_id = current.tab_id;
        let destination_tab_id = TabId::new();
        let moved_pane = snapshot.sessions[0].workspaces[0].tabs[0].panes.remove(0);
        snapshot.sessions[0].workspaces[0].tabs.push(TabSnapshot {
            id: destination_tab_id,
            name: "destination".into(),
            closing: false,
            layout: crate::splits::SplitTree::leaf(moved_pane.id),
            panes: vec![moved_pane],
        });

        // Simulate an attachment whose selected target has not observed the external move yet.
        current.tab_id = old_tab_id;
        let rows = flatten(&snapshot, &current);
        let current_rows = rows.iter().filter(|row| row.current).collect::<Vec<_>>();

        assert_eq!(current_rows.len(), 4);
        assert!(
            current_rows
                .iter()
                .any(|row| row.key == ResourceKey::Tab(destination_tab_id))
        );
        assert!(
            current_rows
                .iter()
                .any(|row| row.key == ResourceKey::Pane(current.pane_id))
        );
        assert!(
            !rows
                .iter()
                .find(|row| row.key == ResourceKey::Tab(old_tab_id))
                .unwrap()
                .current
        );
        for key in [
            ResourceKey::Session(current.session_id),
            ResourceKey::Workspace(current.workspace_id),
            ResourceKey::Tab(destination_tab_id),
        ] {
            assert_eq!(
                rows.iter().find(|row| row.key == key).unwrap().destination,
                Some(current.pane_id)
            );
        }
    }

    #[test]
    fn closing_tab_disables_its_rows_and_ancestor_targets_avoid_it() {
        let (mut snapshot, current, _) = fixture();
        snapshot.sessions[0].workspaces[0].tabs[0].closing = true;

        let rows = flatten(&snapshot, &current);

        assert!(rows[2..].iter().all(|row| row.closing));
        assert!(rows[2..].iter().all(|row| row.destination.is_none()));
        assert_eq!(rows[0].destination, None);
        assert_eq!(rows[1].destination, None);
    }

    #[test]
    fn navigation_clamps_pages_and_snapshot_refreshes_preserve_selection() {
        let (mut snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open(&current);
        assert!(nav.accept_resources(&snapshot, &current));
        assert!(!nav.accept_resources(&snapshot, &current));
        assert_eq!(nav.selected, 3);
        nav.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), 2);
        assert_eq!(nav.selected, 4);
        nav.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), 2);
        assert_eq!(nav.selected, 4);
        nav.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), 2);
        assert_eq!(nav.selected, 2);
        let selected_key = nav.rows[nav.selected].key;
        snapshot.revision += 1;
        assert!(nav.accept_resources(&snapshot, &current));
        assert_eq!(nav.rows[nav.selected].key, selected_key);
    }

    #[test]
    fn only_matching_request_ids_complete_pending_switches() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open(&current);
        assert!(nav.accept_resources(&snapshot, &current));
        assert!(!nav.switch_selected(None));
        assert!(!nav.switch_error(None, "unsolicited".into()));

        let switch = Uuid::new_v4();
        nav.begin_switch(switch);
        assert!(!nav.switch_selected(None));
        assert!(!nav.switch_error(Some(Uuid::new_v4()), "wrong".into()));
        assert!(matches!(nav.status, NavigatorStatus::Switching));
        assert!(nav.switch_error(Some(switch), "busy".into()));
        assert!(matches!(
            nav.status,
            NavigatorStatus::Error { ref message } if message == "busy"
        ));
    }

    #[test]
    fn switching_keeps_rows_and_disables_actions_while_error_allows_retry() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open(&current);
        nav.accept_resources(&snapshot, &current);
        let selected = nav.selected;
        let rows = nav.rows.clone();
        let switch = Uuid::new_v4();
        nav.begin_switch(switch);
        nav.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 2);
        assert_eq!(nav.selected, selected);
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 2),
            NavigatorAction::Stay
        ));
        assert_eq!(nav.rows, rows);
        assert!(nav.switch_error(Some(switch), "held".into()));
        assert_eq!(nav.rows, rows);
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 2),
            NavigatorAction::Select(_)
        ));
    }

    #[test]
    fn escape_and_q_cannot_cancel_a_pending_switch() {
        let (_, current, _) = fixture();
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let request = Uuid::new_v4();
            let mut nav = NavigatorState::open(&current);
            nav.begin_switch(request);

            assert!(matches!(
                nav.key(KeyEvent::new(code, KeyModifiers::NONE), 3),
                NavigatorAction::Stay
            ));
            assert_eq!(nav.switch_request, Some(request));
            assert!(matches!(nav.status, NavigatorStatus::Switching));
        }
    }

    #[test]
    fn render_keeps_hierarchy_visible_and_puts_progress_or_error_in_footer() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open(&current);
        nav.accept_resources(&snapshot, &current);
        nav.selected = 0;
        let (ready, buffer) = rendered(&mut nav, 50, 9);
        assert!(ready.contains("navigator"));
        assert!(ready.contains("sessión 🛰"));
        assert!(ready.contains("pane 1"));
        assert_eq!(ready.matches('•').count(), 1);
        // First body row sits inside the border, below the title.
        assert!(
            buffer
                .cell((1, 2))
                .unwrap()
                .modifier
                .contains(Modifier::BOLD)
        );

        let switch = Uuid::new_v4();
        nav.begin_switch(switch);
        let (switching, _) = rendered(&mut nav, 30, 5);
        assert!(switching.contains("sessión 🛰"));
        assert!(switching.contains("Switching…"));
        nav.switch_error(Some(switch), "destination busy".into());
        let (error, _) = rendered(&mut nav, 30, 5);
        assert!(error.contains("sessión 🛰"));
        assert!(error.contains("Error: destination busy"));
    }

    #[test]
    fn small_hosts_drop_the_title_before_the_footer() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open(&current);
        nav.accept_resources(&snapshot, &current);
        // Height 6 leaves a 4-row interior: footer but no title.
        let (text, _) = rendered(&mut nav, 40, 6);
        assert!(!text.contains("navigator"));
        assert_eq!(text.lines().filter(|line| line.contains("move")).count(), 1);
    }

    /// Two sessions: S1(W1(T1(P1 P2) T2(P3)) W2(T3(P4))) S2(W3(T4(P5))).
    fn tree() -> NavigatorState {
        let row = |depth: u16| NavigatorRow {
            key: ResourceKey::Pane(PaneId::new()),
            depth,
            label: String::new(),
            current: false,
            closing: false,
            destination: None,
            activity: None,
        };
        let mut nav = NavigatorState::open(&fixture().1);
        nav.rows = [0, 1, 2, 3, 3, 2, 3, 1, 2, 3, 0, 1, 2, 3]
            .into_iter()
            .map(row)
            .collect();
        nav.status = NavigatorStatus::Ready;
        nav
    }

    fn press(nav: &mut NavigatorState, code: KeyCode) {
        nav.key(KeyEvent::new(code, KeyModifiers::NONE), 10);
    }

    #[test]
    fn kind_keys_cycle_within_the_enclosing_scope_and_descend_from_above() {
        let mut nav = tree();
        // From P1 (3): panes wrap within T1, tabs and workspaces and sessions
        // cycle within their own parents.
        nav.selected = 3;
        press(&mut nav, KeyCode::Char('p'));
        assert_eq!(nav.selected, 4);
        press(&mut nav, KeyCode::Char('p'));
        assert_eq!(nav.selected, 3, "pane cycle wraps inside T1");
        press(&mut nav, KeyCode::Char('t'));
        assert_eq!(nav.selected, 5, "next tab in W1");
        press(&mut nav, KeyCode::Char('t'));
        assert_eq!(nav.selected, 2, "tab cycle wraps inside W1");
        press(&mut nav, KeyCode::Char('w'));
        assert_eq!(nav.selected, 7, "next workspace in S1");
        press(&mut nav, KeyCode::Char('w'));
        assert_eq!(nav.selected, 1, "workspace cycle wraps inside S1");
        press(&mut nav, KeyCode::Char('s'));
        assert_eq!(nav.selected, 10);
        press(&mut nav, KeyCode::Char('s'));
        assert_eq!(nav.selected, 0, "session cycle wraps globally");
        // From a session, kind keys descend into its subtree.
        press(&mut nav, KeyCode::Char('p'));
        assert_eq!(nav.selected, 3, "first pane inside S1");
    }

    #[test]
    fn workspace_and_tab_jumps_cross_scopes_without_wrapping() {
        let mut nav = tree();
        nav.selected = 0;
        press(&mut nav, KeyCode::Char('J'));
        assert_eq!(nav.selected, 1);
        press(&mut nav, KeyCode::Char('J'));
        assert_eq!(nav.selected, 7);
        press(&mut nav, KeyCode::Char('J'));
        assert_eq!(nav.selected, 11, "workspace jump crosses into S2");
        press(&mut nav, KeyCode::Char('J'));
        assert_eq!(nav.selected, 11, "no wrap at the end");
        press(&mut nav, KeyCode::Char('K'));
        assert_eq!(nav.selected, 7);
        nav.selected = 5;
        press(&mut nav, KeyCode::Char(']'));
        assert_eq!(nav.selected, 8, "tab jump crosses into W2");
        press(&mut nav, KeyCode::Char('['));
        assert_eq!(nav.selected, 5);
    }

    #[test]
    fn arrows_walk_to_parent_and_first_child() {
        let mut nav = tree();
        nav.selected = 4;
        press(&mut nav, KeyCode::Left);
        assert_eq!(nav.selected, 2, "P2 up to T1");
        press(&mut nav, KeyCode::Left);
        assert_eq!(nav.selected, 1, "T1 up to W1");
        press(&mut nav, KeyCode::Right);
        assert_eq!(nav.selected, 2, "W1 down to T1");
        press(&mut nav, KeyCode::Right);
        assert_eq!(nav.selected, 3, "T1 down to P1");
        press(&mut nav, KeyCode::Right);
        assert_eq!(nav.selected, 3, "panes have no children");
        nav.selected = 0;
        press(&mut nav, KeyCode::Left);
        assert_eq!(nav.selected, 0, "sessions have no parent");
    }

    #[test]
    fn enter_is_disabled_while_loading_and_for_closing_rows_and_escape_closes() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open(&current);
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 3),
            NavigatorAction::Stay
        ));
        nav.accept_resources(&snapshot, &current);
        nav.selected = 4;
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 3),
            NavigatorAction::Stay
        ));
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 3),
            NavigatorAction::Close
        ));
    }

    #[test]
    fn rendering_tiny_and_narrow_unicode_buffers_never_panics_and_keeps_selection_visible() {
        let (snapshot, current, _) = fixture();
        for (width, height) in [(1, 1), (2, 2), (5, 3), (12, 4)] {
            let mut nav = NavigatorState::open(&current);
            nav.accept_resources(&snapshot, &current);
            nav.selected = nav.rows.len() - 1;
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            nav.render(area, 0, &mut buffer);
            assert!(nav.scroll <= nav.selected);
        }
    }
}
