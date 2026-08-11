use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

use crate::{
    domain::{PaneId, SessionId, TabId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, TargetSelector},
};

use super::dialog::{
    dialog_area, fill_row, frame_inner, render_footer, render_frame, render_list_scrollbar,
    render_title,
};
use super::fuzzy;
use super::notifications::{ActivityIndicator, NotificationState};

const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 20;
const MAX_QUERY_BYTES: usize = 512;

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
    pub search_path: String,
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
    filtered: Vec<usize>,
    query: String,
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
        Self::open_global()
    }

    pub fn open_global() -> Self {
        Self {
            rows: Vec::new(),
            filtered: Vec::new(),
            query: String::new(),
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
        self.accept_optional_resources(snapshot, Some(current), notifications)
    }

    pub fn accept_global_resources(&mut self, snapshot: &ResourceSnapshot) -> bool {
        self.accept_optional_resources(snapshot, None, &NotificationState::default())
    }

    fn accept_optional_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        current: Option<&SelectedTarget>,
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
        self.rows = flatten_optional(snapshot, current, notifications);
        self.refilter(old_key);
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
            && let Some(current) = current
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| row.key == ResourceKey::Pane(current.pane_id))
        {
            self.selected = index;
        }
        self.ensure_selected_match();
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
        if matches!(key.code, KeyCode::Esc) {
            return NavigatorAction::Close;
        }
        let last = self.filtered.len().saturating_sub(1);
        let page = visible_rows.max(1);
        match (key.code, key.modifiers) {
            (KeyCode::Up, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                self.jump_back(1)
            }
            (KeyCode::Down, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                self.jump_forward(1)
            }
            (KeyCode::Char('k'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-1)
            }
            (KeyCode::Char('j'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(1)
            }
            (KeyCode::Up, _) => self.move_selection(-1),
            (KeyCode::Down, _) => self.move_selection(1),
            (KeyCode::Home, _) => self.select_filtered(0),
            (KeyCode::End, _) => self.select_filtered(last),
            (KeyCode::PageUp, _) => self.move_selection(-(page as isize)),
            (KeyCode::Char('u'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-(page as isize))
            }
            (KeyCode::PageDown, _) => self.move_selection(page as isize),
            (KeyCode::Char('d'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(page as isize)
            }
            (KeyCode::Char('s'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_depth(0)
            }
            (KeyCode::Char('w'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_depth(1)
            }
            (KeyCode::Char('t'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_depth(2)
            }
            (KeyCode::Char('p'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_depth(3)
            }
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
            (KeyCode::Backspace | KeyCode::Delete, _) => self.remove_last_grapheme(),
            (KeyCode::Char(character), modifiers)
                if !character.is_control()
                    && !modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
            {
                self.append(character)
            }
            _ => {}
        }
        self.keep_visible(visible_rows);
        NavigatorAction::Stay
    }

    pub fn paste(&mut self, value: &str) {
        for character in value.chars().filter(|character| !character.is_control()) {
            if self.query.len() + character.len_utf8() > MAX_QUERY_BYTES {
                break;
            }
            self.query.push(character);
        }
        self.refilter(self.selected_key());
        self.ensure_selected_match();
    }

    fn append(&mut self, character: char) {
        if self.query.len() + character.len_utf8() <= MAX_QUERY_BYTES {
            let selected = self.selected_key();
            self.query.push(character);
            self.refilter(selected);
            self.ensure_selected_match();
        }
    }

    fn remove_last_grapheme(&mut self) {
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            let selected = self.selected_key();
            self.query.truncate(index);
            self.refilter(selected);
            self.ensure_selected_match();
        }
    }

    fn refilter(&mut self, _preserve: Option<ResourceKey>) {
        self.filtered = fuzzy::ranked(
            &self.query,
            self.rows.iter().map(|row| row.search_path.clone()),
        );
        self.scroll = 0;
    }

    fn selected_key(&self) -> Option<ResourceKey> {
        self.rows.get(self.selected).map(|row| row.key)
    }

    fn ensure_selected_match(&mut self) {
        if !self.filtered.contains(&self.selected) {
            self.selected = self.filtered.first().copied().unwrap_or(0);
        }
    }

    fn selected_filtered_position(&self) -> usize {
        self.filtered
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0)
    }

    fn select_filtered(&mut self, position: usize) {
        if let Some(index) = self.filtered.get(position) {
            self.selected = *index;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let position = self
            .selected_filtered_position()
            .saturating_add_signed(delta)
            .min(self.filtered.len().saturating_sub(1));
        self.select_filtered(position);
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
        let selected = self.selected_filtered_position();
        if selected < self.scroll {
            self.scroll = selected;
        }
        if selected >= self.scroll + height {
            self.scroll = selected + 1 - height;
        }
    }

    pub fn render(&mut self, host: Rect, spinner_frame: usize, buffer: &mut Buffer) {
        let area = render_frame(dialog_area(host, MAX_WIDTH, MAX_HEIGHT), buffer);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (header, footer) = chrome_rows(area.height);
        if header == 1 {
            render_title(area, &format!(" navigator › {}", self.query), buffer);
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
                    Some(row) if row.closing => "Closing…  ↑↓/C-jk move  esc cancel".to_owned(),
                    _ => "type search  ↑↓/C-jk move  C-s/w/t/p cycle  enter switch  esc close"
                        .to_owned(),
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
                if self.filtered.is_empty() {
                    put(
                        buffer,
                        area.x,
                        body_y,
                        area.width,
                        "No matching resources",
                        Style::default(),
                    );
                } else {
                    for (line, index) in self
                        .filtered
                        .iter()
                        .skip(self.scroll)
                        .take(body_height)
                        .enumerate()
                    {
                        let index = *index;
                        let row = &self.rows[index];
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
                }
                let body = Rect::new(
                    area.x,
                    body_y,
                    area.width,
                    u16::try_from(body_height).expect("body height fits u16"),
                );
                render_list_scrollbar(self.scroll, self.filtered.len(), body, buffer);
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

#[cfg(test)]
fn flatten_with_notifications(
    snapshot: &ResourceSnapshot,
    current: &SelectedTarget,
    notifications: &NotificationState,
) -> Vec<NavigatorRow> {
    flatten_optional(snapshot, Some(current), notifications)
}

fn flatten_optional(
    snapshot: &ResourceSnapshot,
    current: Option<&SelectedTarget>,
    notifications: &NotificationState,
) -> Vec<NavigatorRow> {
    let current_ancestry = current.and_then(|current| {
        snapshot.sessions.iter().find_map(|session| {
            session.workspaces.iter().find_map(|workspace| {
                workspace.tabs.iter().find_map(|tab| {
                    tab.panes
                        .iter()
                        .find(|pane| pane.id == current.pane_id)
                        .map(|_| (session.id, workspace.id, tab.id))
                })
            })
        })
    });
    let (current_session_id, current_workspace_id, current_tab_id) = current_ancestry
        .map(|(session, workspace, tab)| (Some(session), Some(workspace), Some(tab)))
        .unwrap_or_else(|| {
            current.map_or((None, None, None), |current| {
                (
                    Some(current.session_id),
                    Some(current.workspace_id),
                    Some(current.tab_id),
                )
            })
        });
    let current_pane_id = current.map(|current| current.pane_id);

    let mut rows = Vec::new();
    for session in &snapshot.sessions {
        let session_path = session.name.clone();
        let session_current = Some(session.id) == current_session_id;
        let session_target = if session_current
            && current_pane_id.is_some_and(|pane| pane_available_session(session, pane, false))
        {
            current_pane_id
        } else {
            first_pane_session(session, false)
        };
        rows.push(NavigatorRow {
            key: ResourceKey::Session(session.id),
            depth: 0,
            label: session.name.clone(),
            search_path: session_path.clone(),
            current: session_current,
            closing: session.closing,
            destination: (!session.closing).then_some(session_target).flatten(),
            activity: notifications.indicator(
                &session
                    .workspaces
                    .iter()
                    .flat_map(|workspace| &workspace.tabs)
                    .flat_map(|tab| &tab.panes)
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
        });
        for workspace in &session.workspaces {
            let workspace_path = format!("{session_path} › {}", workspace.name);
            let closing = session.closing || workspace.closing;
            let workspace_current = session_current && Some(workspace.id) == current_workspace_id;
            let target = if workspace_current
                && current_pane_id
                    .is_some_and(|pane| pane_available_workspace(workspace, pane, session.closing))
            {
                current_pane_id
            } else {
                first_pane_workspace(workspace, closing)
            };
            rows.push(NavigatorRow {
                key: ResourceKey::Workspace(workspace.id),
                depth: 1,
                label: workspace.name.clone(),
                search_path: workspace_path.clone(),
                current: workspace_current,
                closing,
                destination: (!closing).then_some(target).flatten(),
                activity: notifications.indicator(
                    &workspace
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
            });
            for (tab_index, tab) in workspace.tabs.iter().enumerate() {
                let tab_label = if tab.name.is_empty() {
                    format!("tab {}", tab_index + 1)
                } else {
                    tab.name.clone()
                };
                let tab_path = format!("{workspace_path} › {tab_label}");
                let tab_current = workspace_current && Some(tab.id) == current_tab_id;
                let tab_closing = closing || tab.closing;
                let target = if tab_current
                    && tab
                        .panes
                        .iter()
                        .any(|pane| Some(pane.id) == current_pane_id && !pane.closing)
                    && !tab_closing
                {
                    current_pane_id
                } else {
                    tab.panes
                        .iter()
                        .find(|pane| !tab_closing && !pane.closing)
                        .map(|pane| pane.id)
                };
                rows.push(NavigatorRow {
                    key: ResourceKey::Tab(tab.id),
                    depth: 2,
                    label: tab_label,
                    search_path: tab_path.clone(),
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
                        search_path: format!("{tab_path} › pane {}", index + 1),
                        current: Some(pane.id) == current_pane_id,
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
    fn global_navigator_has_no_current_row_and_selects_a_live_destination() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open_global();

        assert!(nav.accept_global_resources(&snapshot));
        assert!(nav.rows.iter().all(|row| !row.current));
        assert_eq!(nav.selected, 0);
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 10),
            NavigatorAction::Select(TargetSelector::Pane(pane_id)) if pane_id == current.pane_id
        ));
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
    fn fuzzy_search_matches_hidden_paths_keeps_rows_and_names_unnamed_tabs() {
        let (mut snapshot, current, _) = fixture();
        snapshot.sessions[0].workspaces[0].tabs[0].name.clear();
        let mut nav = NavigatorState::open(&current);
        nav.accept_resources(&snapshot, &current);

        assert_eq!(
            nav.rows
                .iter()
                .find(|row| matches!(row.key, ResourceKey::Tab(_)))
                .unwrap()
                .label,
            "tab 1"
        );
        nav.paste("sesn pane 1");
        assert!(!nav.filtered.is_empty());
        assert!(nav.filtered.iter().all(|index| nav.rows[*index].depth == 3));
        assert!(
            nav.filtered
                .iter()
                .all(|index| nav.rows[*index].search_path.contains("sessión"))
        );
        assert!(
            nav.rows.len() > nav.filtered.len(),
            "filtering preserves the live tree model"
        );

        nav.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 10);
        assert!(nav.query.ends_with('q'), "plain q is search text");
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 10),
            NavigatorAction::Close
        ));
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
            search_path: String::new(),
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
        nav.filtered = (0..nav.rows.len()).collect();
        nav.status = NavigatorStatus::Ready;
        nav
    }

    fn press(nav: &mut NavigatorState, code: KeyCode) {
        nav.key(KeyEvent::new(code, KeyModifiers::NONE), 10);
    }

    fn press_ctrl(nav: &mut NavigatorState, character: char) {
        nav.key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
            10,
        );
    }

    #[test]
    fn kind_keys_cycle_within_the_enclosing_scope_and_descend_from_above() {
        let mut nav = tree();
        // From P1 (3): panes wrap within T1, tabs and workspaces and sessions
        // cycle within their own parents.
        nav.selected = 3;
        press_ctrl(&mut nav, 'p');
        assert_eq!(nav.selected, 4);
        press_ctrl(&mut nav, 'p');
        assert_eq!(nav.selected, 3, "pane cycle wraps inside T1");
        press_ctrl(&mut nav, 't');
        assert_eq!(nav.selected, 5, "next tab in W1");
        press_ctrl(&mut nav, 't');
        assert_eq!(nav.selected, 2, "tab cycle wraps inside W1");
        press_ctrl(&mut nav, 'w');
        assert_eq!(nav.selected, 7, "next workspace in S1");
        press_ctrl(&mut nav, 'w');
        assert_eq!(nav.selected, 1, "workspace cycle wraps inside S1");
        press_ctrl(&mut nav, 's');
        assert_eq!(nav.selected, 10);
        press_ctrl(&mut nav, 's');
        assert_eq!(nav.selected, 0, "session cycle wraps globally");
        // From a session, kind keys descend into its subtree.
        press_ctrl(&mut nav, 'p');
        assert_eq!(nav.selected, 3, "first pane inside S1");
    }

    #[test]
    fn shift_arrows_jump_workspaces_without_wrapping() {
        let mut nav = tree();
        nav.selected = 0;
        nav.key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT), 10);
        assert_eq!(nav.selected, 1);
        nav.key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT), 10);
        assert_eq!(nav.selected, 7);
        nav.key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT), 10);
        assert_eq!(nav.selected, 11, "workspace jump crosses into S2");
        nav.key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT), 10);
        assert_eq!(nav.selected, 11);
        nav.key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT), 10);
        assert_eq!(nav.selected, 7);
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
