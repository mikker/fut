use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use crate::{
    domain::{PaneId, SessionId, TabId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{ResourceSnapshot, TargetSelector},
};

use super::config::{SemanticStyle, StylesConfig};
use super::dialog::{
    dialog_area, fill_row, frame_inner, render_footer, render_frame, render_list_scrollbar,
    render_title,
};
use super::fuzzy;
use super::navigation::NavigationHistory;
use super::notifications::{ActivityIndicator, NotificationState};

const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 20;
const MAX_QUERY_BYTES: usize = 512;
const BREADCRUMB_WIDTH: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResourceKey {
    Session(SessionId),
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
}

impl ResourceKey {
    fn style(self) -> SemanticStyle {
        match self {
            Self::Session(_) => SemanticStyle::Session,
            Self::Workspace(_) => SemanticStyle::Workspace,
            Self::Tab(_) => SemanticStyle::Tab,
            Self::Pane(_) => SemanticStyle::Pane,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResourceFilter {
    depth: u16,
    scope: Option<ResourceKey>,
}

impl ResourceFilter {
    fn label(self) -> &'static str {
        match self.depth {
            0 => "sessions",
            1 => "workspaces",
            2 => "tabs",
            _ => "panes",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NavigatorRow {
    pub key: ResourceKey,
    pub depth: u16,
    pub label: String,
    pub inline_pane: Option<PaneId>,
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
    filter: Option<ResourceFilter>,
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
    pub fn open() -> Self {
        Self {
            rows: Vec::new(),
            filtered: Vec::new(),
            filter: None,
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
        let mut history = NavigationHistory::default();
        history.record(current);
        self.accept_resources_with_notifications(
            snapshot,
            current,
            &history,
            &NotificationState::default(),
        )
    }

    pub fn accept_resources_with_notifications(
        &mut self,
        snapshot: &ResourceSnapshot,
        current: &SelectedTarget,
        history: &NavigationHistory,
        notifications: &NotificationState,
    ) -> bool {
        self.accept_optional_resources(snapshot, Some(current), history, notifications)
    }

    pub fn accept_global_resources(&mut self, snapshot: &ResourceSnapshot) -> bool {
        self.accept_optional_resources(
            snapshot,
            None,
            &NavigationHistory::default(),
            &NotificationState::default(),
        )
    }

    fn accept_optional_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        current: Option<&SelectedTarget>,
        history: &NavigationHistory,
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
        self.rows = flatten_optional(snapshot, current, history, notifications);
        self.refilter();
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
            && let Some(index) = self.rows.iter().position(|row| {
                row.key == ResourceKey::Pane(current.pane_id)
                    || row.inline_pane == Some(current.pane_id)
            })
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
        let searching = !self.query.is_empty();
        let navigating_tree = !searching && self.filter.is_none();
        match (key.code, key.modifiers) {
            (KeyCode::Up, modifiers)
                if navigating_tree && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.jump_back(1)
            }
            (KeyCode::Down, modifiers)
                if navigating_tree && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.jump_forward(1)
            }
            (KeyCode::Up | KeyCode::Down, modifiers)
                if !navigating_tree && modifiers.contains(KeyModifiers::SHIFT) => {}
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
            (KeyCode::Char('a'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.show_all()
            }
            (KeyCode::Char('s'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter_depth(0)
            }
            (KeyCode::Char('w'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter_depth(1)
            }
            (KeyCode::Char('t'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter_depth(2)
            }
            (KeyCode::Char('p'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter_depth(3)
            }
            (KeyCode::Left, _) if navigating_tree => self.select_parent(),
            (KeyCode::Right, _) if navigating_tree => self.select_first_child(),
            (KeyCode::Enter, _)
                if matches!(
                    self.status,
                    NavigatorStatus::Ready | NavigatorStatus::Error { .. }
                ) && self.filtered.contains(&self.selected) =>
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
        if matches!(self.status, NavigatorStatus::Switching) {
            return;
        }
        for character in value.chars().filter(|character| !character.is_control()) {
            if self.query.len() + character.len_utf8() > MAX_QUERY_BYTES {
                break;
            }
            self.query.push(character);
        }
        self.refilter();
        self.ensure_selected_match();
    }

    fn append(&mut self, character: char) {
        if self.query.len() + character.len_utf8() <= MAX_QUERY_BYTES {
            self.query.push(character);
            self.refilter();
            self.ensure_selected_match();
        }
    }

    fn remove_last_grapheme(&mut self) {
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(index);
            self.refilter();
            self.ensure_selected_match();
        }
    }

    fn show_all(&mut self) {
        self.filter = None;
        self.query.clear();
        self.refilter();
        self.ensure_selected_match();
    }

    fn refilter(&mut self) {
        self.filtered = minimal_fuzzy_matches(&self.query, &self.rows);
        if let Some(filter) = self.filter {
            let scope = filter
                .scope
                .and_then(|key| self.rows.iter().position(|row| row.key == key));
            let range = scope.map_or(0..self.rows.len(), |start| {
                start + 1..self.subtree_end(start)
            });
            self.filtered
                .retain(|index| self.rows[*index].depth == filter.depth && range.contains(index));
        }
        self.scroll = 0;
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

    /// Limit rows to one resource kind inside the selected row's enclosing
    /// scope. Repeating the active filter restores the complete tree.
    fn filter_depth(&mut self, depth: u16) {
        let Some(current) = self.rows.get(self.selected) else {
            return;
        };
        let target = if current.depth < depth {
            let end = self.subtree_end(self.selected);
            (self.selected + 1..end).find(|&index| self.rows[index].depth == depth)
        } else {
            (0..=self.selected)
                .rev()
                .find(|&index| self.rows[index].depth == depth)
        };
        let Some(target) = target else { return };
        let scope = if depth > 0 {
            let Some(index) = (0..=target)
                .rev()
                .find(|&index| self.rows[index].depth == depth - 1)
            else {
                return;
            };
            Some(self.rows[index].key)
        } else {
            None
        };
        let filter = ResourceFilter { depth, scope };
        self.filter = (self.filter != Some(filter)).then_some(filter);
        self.refilter();
        if self.filter.is_some() && self.rows[target].depth == depth {
            self.selected = target;
        }
        self.ensure_selected_match();
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

    fn title(&self) -> String {
        let mut breadcrumbs = vec!["navigator".to_owned()];
        if let Some(filter) = self.filter {
            if let Some(scope) = filter.scope
                && let Some(index) = self.rows.iter().position(|row| row.key == scope)
            {
                let scope_depth = self.rows[index].depth;
                for depth in 0..=scope_depth {
                    if let Some(row) = self.rows[..=index]
                        .iter()
                        .rev()
                        .find(|row| row.depth == depth)
                    {
                        breadcrumbs.push(breadcrumb_label(&row.label));
                    }
                }
            }
            breadcrumbs.push(filter.label().to_owned());
        }
        if !self.query.is_empty() {
            breadcrumbs.push(self.query.clone());
        }
        format!(" {}", breadcrumbs.join(" › "))
    }

    pub fn render(
        &mut self,
        host: Rect,
        spinner_frame: usize,
        styles: &StylesConfig,
        buffer: &mut Buffer,
    ) {
        let area = render_frame(dialog_area(host, MAX_WIDTH, MAX_HEIGHT), buffer);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (header, footer) = chrome_rows(area.height);
        if header == 1 {
            render_title(area, &self.title(), buffer);
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
                    _ => "type search  ↑↓/C-jk move  C-s/w/t/p filter  C-a all  enter switch  esc close"
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
                        let mut style = styles.apply(row.key.style(), Style::default());
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
                        if self.query.is_empty() {
                            let text = format!(
                                "{}{} {}",
                                "  ".repeat(usize::from(row.depth)),
                                marker,
                                row.label
                            );
                            let mut spans = vec![Span::styled(text, style)];
                            if row.inline_pane.is_some() {
                                spans.push(Span::styled(
                                    " · pane",
                                    styles.apply(SemanticStyle::Muted, style),
                                ));
                            }
                            buffer.set_line(area.x, y, &Line::from(spans), area.width);
                        } else {
                            render_fuzzy_path(
                                buffer,
                                Rect::new(area.x, y, area.width, 1),
                                marker,
                                &row.search_path,
                                &self.query,
                                style,
                                styles.apply(SemanticStyle::Muted, style),
                            );
                        }
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

fn breadcrumb_label(label: &str) -> String {
    if label.width() <= BREADCRUMB_WIDTH {
        return label.to_owned();
    }

    let mut clipped = String::new();
    let mut width = 0;
    for grapheme in label.graphemes(true) {
        let grapheme_width = grapheme.width();
        if width + grapheme_width > BREADCRUMB_WIDTH - 1 {
            break;
        }
        clipped.push_str(grapheme);
        width += grapheme_width;
    }
    clipped.push('…');
    clipped
}

fn render_fuzzy_path(
    buffer: &mut Buffer,
    area: Rect,
    marker: &str,
    path: &str,
    query: &str,
    style: Style,
    muted: Style,
) {
    let matched = fuzzy::matched_char_indices(query, path).unwrap_or_default();
    let highlighted = style
        .remove_modifier(Modifier::DIM)
        .add_modifier(Modifier::BOLD);
    let prefix = format!(" {marker} ");
    let available = usize::from(area.width).saturating_sub(prefix.width());
    let mut spans = vec![Span::styled(prefix, muted)];
    let mut character_index = 0;
    let path = path
        .graphemes(true)
        .map(|grapheme| {
            let start = character_index;
            character_index += grapheme.chars().count();
            let is_matched = matched
                .iter()
                .any(|index| (start..character_index).contains(index));
            (grapheme, is_matched)
        })
        .collect::<Vec<_>>();
    let path_len = path.len();
    let total_width = path
        .iter()
        .map(|(grapheme, _)| grapheme.width())
        .sum::<usize>();
    let (start, end) = if total_width > available {
        let end = path
            .iter()
            .rposition(|(_, is_matched)| *is_matched)
            .map_or(path_len, |last| (last + 2).min(path_len));
        let suffix_width = usize::from(end < path_len);
        let mut remaining = available.saturating_sub(1 + suffix_width);
        let mut start = end;
        while start > 0 {
            let width = path[start - 1].0.width();
            if width > remaining {
                break;
            }
            remaining -= width;
            start -= 1;
        }
        (start, end)
    } else {
        (0, path_len)
    };
    if start > 0 {
        spans.push(Span::styled("…", muted));
    }
    let mut run = String::new();
    let mut run_matched = None;
    for (grapheme, is_matched) in path.into_iter().skip(start).take(end.saturating_sub(start)) {
        if run_matched.is_some_and(|current| current != is_matched) {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if run_matched == Some(true) {
                    highlighted
                } else {
                    muted
                },
            ));
        }
        run_matched = Some(is_matched);
        run.push_str(grapheme);
    }
    if !run.is_empty() {
        spans.push(Span::styled(
            run,
            if run_matched == Some(true) {
                highlighted
            } else {
                muted
            },
        ));
    }
    if end < path_len {
        spans.push(Span::styled("…", muted));
    }
    buffer.set_line(area.x, area.y, &Line::from(spans), area.width);
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

fn minimal_fuzzy_matches(query: &str, rows: &[NavigatorRow]) -> Vec<usize> {
    let mut ranked = fuzzy::ranked(query, rows.iter().map(|row| row.search_path.clone()));
    if query.is_empty() {
        return ranked;
    }

    let mut path_matches = vec![false; rows.len()];
    for &index in &ranked {
        path_matches[index] = true;
    }

    let mut ancestor_matches = Vec::new();
    let mut visible = vec![false; rows.len()];
    for (index, row) in rows.iter().enumerate() {
        ancestor_matches.truncate(usize::from(row.depth));
        let direct_match = fuzzy::matched_char_indices(query, &row.label).is_some();
        visible[index] = path_matches[index]
            && (direct_match || !ancestor_matches.iter().any(|matched| *matched));
        ancestor_matches.push(path_matches[index]);
    }
    ranked.retain(|index| visible[*index]);
    ranked
}

#[cfg(test)]
pub(super) fn flatten(snapshot: &ResourceSnapshot, current: &SelectedTarget) -> Vec<NavigatorRow> {
    let mut history = NavigationHistory::default();
    history.record(current);
    flatten_with_notifications(snapshot, current, &history, &NotificationState::default())
}

#[cfg(test)]
fn flatten_with_notifications(
    snapshot: &ResourceSnapshot,
    current: &SelectedTarget,
    history: &NavigationHistory,
    notifications: &NotificationState,
) -> Vec<NavigatorRow> {
    flatten_optional(snapshot, Some(current), history, notifications)
}

fn flatten_optional(
    snapshot: &ResourceSnapshot,
    current: Option<&SelectedTarget>,
    history: &NavigationHistory,
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
        rows.push(NavigatorRow {
            key: ResourceKey::Session(session.id),
            depth: 0,
            label: session.name.clone(),
            inline_pane: None,
            search_path: session_path.clone(),
            current: session_current,
            closing: session.closing,
            destination: (!session.closing)
                .then(|| history.session_destination(session))
                .flatten(),
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
            rows.push(NavigatorRow {
                key: ResourceKey::Workspace(workspace.id),
                depth: 1,
                label: workspace.name.clone(),
                inline_pane: None,
                search_path: workspace_path.clone(),
                current: workspace_current,
                closing,
                destination: (!closing)
                    .then(|| history.workspace_destination(workspace))
                    .flatten(),
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
                let single_pane = match tab.panes.as_slice() {
                    [pane] => Some(pane),
                    _ => None,
                };
                let tab_row_closing = tab_closing || single_pane.is_some_and(|pane| pane.closing);
                rows.push(NavigatorRow {
                    key: ResourceKey::Tab(tab.id),
                    depth: 2,
                    label: tab_label,
                    inline_pane: single_pane.map(|pane| pane.id),
                    search_path: single_pane
                        .map_or_else(|| tab_path.clone(), |_| format!("{tab_path} › pane")),
                    current: tab_current,
                    closing: tab_row_closing,
                    destination: (!tab_row_closing)
                        .then(|| history.tab_destination(tab))
                        .flatten(),
                    activity: notifications.indicator(&tab.panes),
                });
                if single_pane.is_some() {
                    continue;
                }
                for (index, pane) in tab.panes.iter().enumerate() {
                    let pane_closing = tab_closing || pane.closing;
                    rows.push(NavigatorRow {
                        key: ResourceKey::Pane(pane.id),
                        depth: 3,
                        label: format!("pane {}", index + 1),
                        inline_pane: None,
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
                    tokens: Default::default(),
                    id: session_id,
                    name: "sessión 🛰".into(),
                    project: Project {
                        identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/tmp")),
                    },
                    closing: false,
                    workspaces: vec![WorkspaceSnapshot {
                        tokens: Default::default(),
                        id: workspace_id,
                        name: "workspace".into(),
                        root: PathBuf::from("/tmp"),
                        closing: false,
                        tabs: vec![TabSnapshot {
                            tokens: Default::default(),
                            id: tab_id,
                            name: "tab".into(),
                            closing: false,
                            layout,
                            panes: vec![
                                PaneSnapshot {
                                    tokens: Default::default(),
                                    id: current_pane,
                                    terminal_id: current_terminal,
                                    closing: false,
                                    activity: Default::default(),
                                    cwd: None,
                                    worktree: None,
                                },
                                PaneSnapshot {
                                    tokens: Default::default(),
                                    id: other_pane,
                                    terminal_id: TerminalId::new(),
                                    closing: true,
                                    activity: Default::default(),
                                    cwd: None,
                                    worktree: None,
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
        nav.render(area, 0, &StylesConfig::default(), &mut buffer);
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
    fn single_pane_tabs_render_the_pane_inline_in_muted_text() {
        let (mut snapshot, current, _) = fixture();
        snapshot.sessions[0].workspaces[0].tabs[0].panes.truncate(1);
        let mut nav = NavigatorState::open();

        nav.accept_resources(&snapshot, &current);

        assert_eq!(
            nav.rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(nav.selected, 2);
        assert_eq!(nav.rows[2].inline_pane, Some(current.pane_id));
        let (rendered, buffer) = rendered(&mut nav, 50, 8);
        assert!(rendered.contains("tab · pane"));
        assert!(buffer[(12, 4)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn parent_destinations_use_the_most_recently_focused_pane() {
        let (mut snapshot, current, remembered_pane_id) = fixture();
        let remembered_pane = &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[1];
        remembered_pane.closing = false;
        let mut remembered = current.clone();
        remembered.pane_id = remembered_pane_id;
        remembered.terminal_id = remembered_pane.terminal_id;
        let mut history = NavigationHistory::default();
        history.record(&current);
        history.record(&remembered);

        let rows = flatten_with_notifications(
            &snapshot,
            &current,
            &history,
            &NotificationState::default(),
        );

        assert!(
            rows[..3]
                .iter()
                .all(|row| row.destination == Some(remembered_pane_id))
        );
    }

    #[test]
    fn global_navigator_has_no_current_row_and_selects_a_live_destination() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open();

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
            tokens: Default::default(),
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

        assert_eq!(current_rows.len(), 3);
        assert!(
            current_rows
                .iter()
                .any(|row| row.key == ResourceKey::Tab(destination_tab_id))
        );
        assert!(
            current_rows
                .iter()
                .any(|row| row.destination == Some(current.pane_id)
                    && row.inline_pane == Some(current.pane_id))
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
        let mut nav = NavigatorState::open();
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
        let mut nav = NavigatorState::open();
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
    fn fuzzy_search_collapses_descendants_matched_only_through_their_ancestor() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open();
        nav.accept_resources(&snapshot, &current);

        nav.paste("sessión");
        assert_eq!(nav.filtered, [0]);

        nav.query.clear();
        nav.refilter();
        nav.paste("workspace tab");
        assert_eq!(
            nav.filtered
                .iter()
                .map(|index| nav.rows[*index].key)
                .collect::<Vec<_>>(),
            [ResourceKey::Tab(current.tab_id)]
        );

        nav.query.clear();
        nav.refilter();
        nav.paste("pane 1");
        assert_eq!(
            nav.filtered
                .iter()
                .map(|index| nav.rows[*index].key)
                .collect::<Vec<_>>(),
            [ResourceKey::Pane(current.pane_id)]
        );
    }

    #[test]
    fn fuzzy_render_shows_the_complete_path_and_only_emphasizes_matches() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buffer = Buffer::empty(area);
        render_fuzzy_path(
            &mut buffer,
            area,
            " ",
            "fut › work › nvim",
            "fuvim",
            Style::default(),
            Style::default().add_modifier(Modifier::DIM),
        );
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();

        assert!(text.contains("fut › work › nvim"));
        assert!(buffer[(3, 0)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(3, 0)].modifier.contains(Modifier::DIM));
        assert!(buffer[(5, 0)].modifier.contains(Modifier::DIM));
        assert!(buffer[(17, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn fuzzy_render_keeps_a_late_match_visible_when_the_path_is_too_wide() {
        let area = Rect::new(0, 0, 24, 1);
        let mut buffer = Buffer::empty(area);
        render_fuzzy_path(
            &mut buffer,
            area,
            " ",
            "界界界界 wide session › workspace › matching-pane",
            "matching",
            Style::default(),
            Style::default().add_modifier(Modifier::DIM),
        );
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();

        assert!(text.contains("…"));
        assert!(text.contains("matching"));
    }

    #[test]
    fn fuzzy_render_never_clips_through_a_grapheme() {
        let area = Rect::new(0, 0, 15, 1);
        let mut buffer = Buffer::empty(area);
        render_fuzzy_path(
            &mut buffer,
            area,
            " ",
            "prefixprefix 👩‍💻 matching",
            "matching",
            Style::default(),
            Style::default().add_modifier(Modifier::DIM),
        );
        let text = (0..area.width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>();

        assert!(text.contains("…👩‍💻"), "{text:?}");
        assert!(text.contains("matching"), "{text:?}");
    }

    #[test]
    fn only_matching_request_ids_complete_pending_switches() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open();
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
        let mut nav = NavigatorState::open();
        nav.accept_resources(&snapshot, &current);
        let selected = nav.selected;
        let rows = nav.rows.clone();
        let switch = Uuid::new_v4();
        nav.begin_switch(switch);
        nav.paste("ignored");
        nav.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 2);
        assert_eq!(nav.selected, selected);
        assert!(nav.query.is_empty());
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
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let request = Uuid::new_v4();
            let mut nav = NavigatorState::open();
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
        let mut nav = NavigatorState::open();
        nav.accept_resources(&snapshot, &current);
        nav.selected = 0;
        let (ready, buffer) = rendered(&mut nav, 50, 9);
        assert!(ready.contains("navigator"));
        assert!(ready.contains("sessión 🛰"));
        assert!(ready.contains("pane 1"));
        assert_eq!(ready.matches('•').count(), 1);
        assert_eq!(buffer[(3, 2)].fg, ratatui::style::Color::Red);
        assert_eq!(buffer[(5, 3)].fg, ratatui::style::Color::Blue);
        assert_eq!(buffer[(7, 4)].fg, ratatui::style::Color::Green);
        assert_eq!(buffer[(9, 5)].fg, ratatui::style::Color::Magenta);
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
        let mut nav = NavigatorState::open();
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
            inline_pane: None,
            search_path: String::new(),
            current: false,
            closing: false,
            destination: None,
            activity: None,
        };
        let mut nav = NavigatorState::open();
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
    fn kind_keys_filter_within_the_enclosing_scope_and_repeat_to_clear() {
        let mut nav = tree();
        nav.selected = 3;
        press_ctrl(&mut nav, 'p');
        assert_eq!(nav.filtered, [3, 4], "panes in T1");
        press_ctrl(&mut nav, 'p');
        assert_eq!(nav.filtered, (0..14).collect::<Vec<_>>(), "repeat clears");

        press_ctrl(&mut nav, 't');
        assert_eq!(nav.filtered, [2, 5], "tabs in W1");
        press_ctrl(&mut nav, 'w');
        assert_eq!(nav.filtered, [1, 7], "workspaces in S1");
        press_ctrl(&mut nav, 's');
        assert_eq!(nav.filtered, [0, 10], "all sessions");

        nav.show_all();
        nav.selected = 0;
        press_ctrl(&mut nav, 'p');
        assert_eq!(nav.filtered, [3, 4], "first tab's panes inside S1");
        assert_eq!(nav.selected, 3);

        nav.show_all();
        nav.selected = 13;
        press_ctrl(&mut nav, 's');
        assert_eq!(nav.selected, 10, "keeps the enclosing session selected");
    }

    #[test]
    fn filtered_title_breadcrumbs_name_the_truncated_scope() {
        let (mut snapshot, current, _) = fixture();
        snapshot.sessions[0].name = "long-session-name".into();
        snapshot.sessions[0].workspaces[0].name = "long-workspace-name".into();
        snapshot.sessions[0].workspaces[0].tabs[0].name = "long-tab-name".into();
        let mut nav = NavigatorState::open();
        nav.accept_resources(&snapshot, &current);

        press_ctrl(&mut nav, 't');
        assert_eq!(nav.title(), " navigator › long-sess… › long-work… › tabs");

        press_ctrl(&mut nav, 'p');
        assert_eq!(
            nav.title(),
            " navigator › long-sess… › long-work… › long-tab-… › panes"
        );
    }

    #[test]
    fn control_a_clears_resource_and_text_filters() {
        let mut nav = tree();
        nav.selected = 3;
        press_ctrl(&mut nav, 'p');
        nav.query = "match".into();
        nav.refilter();

        press_ctrl(&mut nav, 'a');

        assert!(nav.filter.is_none());
        assert!(nav.query.is_empty());
        assert_eq!(nav.filtered, (0..14).collect::<Vec<_>>());
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
    fn text_or_resource_filtering_disables_tree_navigation() {
        let mut nav = tree();
        nav.query = "match".into();
        nav.filtered = vec![3, 4];

        for key in [
            KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        ] {
            nav.selected = 4;
            nav.key(key, 10);
            assert_eq!(nav.selected, 4, "{key:?} selected a hidden row");
        }

        nav.query.clear();
        nav.refilter();
        nav.selected = 3;
        press_ctrl(&mut nav, 'p');
        for code in [KeyCode::Left, KeyCode::Right] {
            nav.key(KeyEvent::new(code, KeyModifiers::NONE), 10);
            assert_eq!(nav.selected, 3);
        }
    }

    #[test]
    fn enter_acts_on_the_visibly_selected_filtered_result() {
        let (mut snapshot, current, other_pane) = fixture();
        snapshot.sessions[0].workspaces[0].tabs[0].panes[1].closing = false;
        let mut nav = NavigatorState::open();
        nav.accept_resources(&snapshot, &current);

        nav.paste("pane 2");

        assert_eq!(nav.filtered, vec![4]);
        assert_eq!(nav.selected, 4);
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 10),
            NavigatorAction::Select(TargetSelector::Pane(pane)) if pane == other_pane
        ));
    }

    #[test]
    fn enter_does_nothing_when_filter_has_no_results() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open();
        nav.accept_resources(&snapshot, &current);

        nav.paste("definitely absent");

        assert!(nav.filtered.is_empty());
        assert!(matches!(
            nav.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 10),
            NavigatorAction::Stay
        ));
    }

    #[test]
    fn enter_is_disabled_while_loading_and_for_closing_rows_and_escape_closes() {
        let (snapshot, current, _) = fixture();
        let mut nav = NavigatorState::open();
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
            let mut nav = NavigatorState::open();
            nav.accept_resources(&snapshot, &current);
            nav.selected = nav.rows.len() - 1;
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            nav.render(area, 0, &StylesConfig::default(), &mut buffer);
            assert!(nav.scroll <= nav.selected);
        }
    }
}
