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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NavigatorStatus {
    Loading,
    Ready,
    Empty,
    Error { message: String, can_retry: bool },
    Switching,
}

pub(super) struct NavigatorState {
    pub rows: Vec<NavigatorRow>,
    pub selected: usize,
    pub scroll: usize,
    pub status: NavigatorStatus,
    pub resource_revision: Option<u64>,
    pub list_request: Option<Uuid>,
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
            list_request: None,
            switch_request: None,
        }
    }

    pub fn set_list_request(&mut self, id: Uuid) {
        self.list_request = Some(id);
        self.status = NavigatorStatus::Loading;
    }

    pub fn accept_resources(
        &mut self,
        request_id: Option<Uuid>,
        snapshot: &ResourceSnapshot,
        current: &SelectedTarget,
    ) -> bool {
        if !matches_request(self.list_request, request_id)
            || self
                .resource_revision
                .is_some_and(|revision| snapshot.revision <= revision)
        {
            return false;
        }
        let old_key = self.rows.get(self.selected).map(|row| row.key);
        let old_index = self.selected;
        self.rows = flatten(snapshot, current);
        self.resource_revision = Some(snapshot.revision);
        self.list_request = None;
        self.status = if self.rows.is_empty() {
            NavigatorStatus::Empty
        } else {
            NavigatorStatus::Ready
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
            (KeyCode::Enter, _)
                if matches!(
                    self.status,
                    NavigatorStatus::Ready
                        | NavigatorStatus::Error {
                            can_retry: true,
                            ..
                        }
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
        self.status = NavigatorStatus::Error {
            message,
            can_retry: true,
        };
        true
    }

    pub fn list_error(&mut self, request: Option<Uuid>, message: String) -> bool {
        if !matches_request(self.list_request, request) {
            return false;
        }
        self.list_request = None;
        self.status = NavigatorStatus::Error {
            message,
            can_retry: false,
        };
        true
    }

    pub fn switch_selected(&mut self, request: Option<Uuid>) -> bool {
        if !matches_request(self.switch_request, request) {
            return false;
        }
        self.switch_request = None;
        true
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

    pub fn render(&mut self, area: Rect, buffer: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.reset();
                }
            }
        }
        let (header, footer) = match area.height {
            0..=2 => (0, 0),
            3..=4 => (0, 1),
            _ => (1, 1),
        };
        if header == 1 {
            put(
                buffer,
                area.x,
                area.y,
                area.width,
                "fut · navigator",
                Style::default().add_modifier(Modifier::BOLD),
            );
        }
        if footer == 1 {
            let footer = match &self.status {
                NavigatorStatus::Loading => "Loading…".to_owned(),
                NavigatorStatus::Empty => "No resources".to_owned(),
                NavigatorStatus::Switching => "Switching…".to_owned(),
                NavigatorStatus::Error {
                    message,
                    can_retry: true,
                } => format!("Error: {message}  enter retry  esc cancel"),
                NavigatorStatus::Error {
                    message,
                    can_retry: false,
                } => format!("Error: {message}  esc cancel"),
                NavigatorStatus::Ready => match self.rows.get(self.selected) {
                    Some(row) if row.closing => "Closing…  ↑↓/jk move  esc cancel".to_owned(),
                    _ => "↑↓/jk move  enter switch  esc cancel".to_owned(),
                },
            };
            put(
                buffer,
                area.x,
                area.bottom() - 1,
                area.width,
                &footer,
                Style::default().add_modifier(Modifier::DIM),
            );
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
                    put(
                        buffer,
                        area.x,
                        body_y + line as u16,
                        area.width,
                        &text,
                        style,
                    );
                }
            }
        }
    }
}

fn matches_request(pending: Option<Uuid>, response: Option<Uuid>) -> bool {
    pending.is_some() && pending == response
}

pub(super) fn flatten(snapshot: &ResourceSnapshot, current: &SelectedTarget) -> Vec<NavigatorRow> {
    let mut rows = Vec::new();
    for session in &snapshot.sessions {
        let session_current = session.id == current.session_id;
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
        });
        for workspace in &session.workspaces {
            let closing = session.closing || workspace.closing;
            let workspace_current = session_current && workspace.id == current.workspace_id;
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
            });
            for tab in &workspace.tabs {
                let tab_current = workspace_current && tab.id == current.tab_id;
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
                            panes: vec![
                                PaneSnapshot {
                                    id: current_pane,
                                    terminal_id: current_terminal,
                                    closing: false,
                                },
                                PaneSnapshot {
                                    id: other_pane,
                                    terminal_id: TerminalId::new(),
                                    closing: true,
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
        nav.render(area, &mut buffer);
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
    fn navigation_clamps_pages_and_matching_requests_preserve_selection() {
        let (mut snapshot, current, _) = fixture();
        let request = Uuid::new_v4();
        let mut nav = NavigatorState::open(&current);
        nav.set_list_request(request);
        assert!(!nav.accept_resources(Some(Uuid::new_v4()), &snapshot, &current));
        assert!(nav.accept_resources(Some(request), &snapshot, &current));
        assert_eq!(nav.selected, 3);
        nav.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), 2);
        assert_eq!(nav.selected, 4);
        nav.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), 2);
        assert_eq!(nav.selected, 4);
        nav.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), 2);
        assert_eq!(nav.selected, 2);
        let selected_key = nav.rows[nav.selected].key;
        snapshot.revision += 1;
        let refresh = Uuid::new_v4();
        nav.set_list_request(refresh);
        assert!(nav.accept_resources(Some(refresh), &snapshot, &current));
        assert_eq!(nav.rows[nav.selected].key, selected_key);
    }

    #[test]
    fn only_some_matching_request_ids_complete_pending_operations() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open(&current);
        assert!(!nav.accept_resources(None, &snapshot, &current));
        assert!(!nav.switch_selected(None));
        assert!(!nav.switch_error(None, "unsolicited".into()));
        assert!(!nav.list_error(None, "unsolicited".into()));

        let list = Uuid::new_v4();
        nav.set_list_request(list);
        assert!(!nav.accept_resources(None, &snapshot, &current));
        assert!(!nav.accept_resources(Some(Uuid::new_v4()), &snapshot, &current));
        assert!(nav.accept_resources(Some(list), &snapshot, &current));

        let switch = Uuid::new_v4();
        nav.begin_switch(switch);
        assert!(!nav.switch_selected(None));
        assert!(!nav.switch_error(Some(Uuid::new_v4()), "wrong".into()));
        assert!(matches!(nav.status, NavigatorStatus::Switching));
        assert!(nav.switch_error(Some(switch), "busy".into()));
        assert!(matches!(
            nav.status,
            NavigatorStatus::Error {
                ref message,
                can_retry: true
            } if message == "busy"
        ));

        let mut loading = NavigatorState::open(&current);
        let request = Uuid::new_v4();
        loading.set_list_request(request);
        assert!(loading.list_error(Some(request), "unavailable".into()));
        assert!(matches!(
            loading.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 3),
            NavigatorAction::Stay
        ));
        let (rendered, _) = rendered(&mut loading, 50, 5);
        assert!(rendered.contains("Error: unavailable  esc cancel"));
        assert!(!rendered.contains("enter retry"));
    }

    #[test]
    fn switching_keeps_rows_and_disables_actions_while_error_allows_retry() {
        let (snapshot, current, _) = fixture();
        let list = Uuid::new_v4();
        let mut nav = NavigatorState::open(&current);
        nav.set_list_request(list);
        nav.accept_resources(Some(list), &snapshot, &current);
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
        let list = Uuid::new_v4();
        let mut nav = NavigatorState::open(&current);
        nav.set_list_request(list);
        nav.accept_resources(Some(list), &snapshot, &current);
        nav.selected = 0;
        let (ready, buffer) = rendered(&mut nav, 50, 7);
        assert!(ready.contains("fut · navigator"));
        assert!(ready.contains("sessión 🛰"));
        assert!(ready.contains("pane 1"));
        assert_eq!(ready.matches('•').count(), 1);
        assert!(
            buffer
                .cell((1, 1))
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
    fn three_lines_reserve_only_one_line_for_chrome() {
        let (snapshot, current, _) = fixture();
        let list = Uuid::new_v4();
        let mut nav = NavigatorState::open(&current);
        nav.set_list_request(list);
        nav.accept_resources(Some(list), &snapshot, &current);
        let (text, _) = rendered(&mut nav, 40, 3);
        assert!(!text.contains("fut · navigator"));
        assert_eq!(text.lines().filter(|line| line.contains("move")).count(), 1);
        assert!(text.lines().take(2).all(|line| !line.is_empty()));
    }

    #[test]
    fn enter_is_disabled_while_loading_and_for_closing_rows_and_escape_closes() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open(&current);
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 3),
            NavigatorAction::Stay
        ));
        let request = Uuid::new_v4();
        nav.set_list_request(request);
        nav.accept_resources(Some(request), &snapshot, &current);
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
            let request = Uuid::new_v4();
            let mut nav = NavigatorState::open(&current);
            nav.set_list_request(request);
            nav.accept_resources(Some(request), &snapshot, &current);
            nav.selected = nav.rows.len() - 1;
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            nav.render(area, &mut buffer);
            assert!(nav.scroll <= nav.selected);
        }
    }
}
