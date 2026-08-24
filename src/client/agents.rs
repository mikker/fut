use crate::{
    domain::{PaneId, SessionId, TabId, TerminalId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{PanePathRef, ResourceSnapshot},
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

use super::{
    chrome::sanitize,
    config::{AgentScope, SemanticStyle},
    notifications::{ActivityIndicator, NotificationState},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AgentItem {
    pub terminal_id: TerminalId,
    pub pane_id: PaneId,
    pub session: String,
    pub workspace: String,
    pub tab: String,
    pub source: String,
    pub current: bool,
    pub indicator: Option<ActivityIndicator>,
}

impl AgentItem {
    pub(super) fn status(&self) -> &'static str {
        match self.indicator {
            Some(ActivityIndicator::Working) => "working",
            Some(ActivityIndicator::Blocked) => "blocked",
            Some(ActivityIndicator::Completed) => "completed",
            None => "idle",
        }
    }

    pub(super) fn search_text(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.source,
            self.status(),
            self.session,
            self.workspace,
            self.tab
        )
    }

    pub(super) fn status_style(&self) -> SemanticStyle {
        match self.indicator {
            Some(ActivityIndicator::Working) => SemanticStyle::Activity,
            Some(ActivityIndicator::Blocked) => SemanticStyle::Error,
            Some(ActivityIndicator::Completed) => SemanticStyle::Added,
            None => SemanticStyle::Muted,
        }
    }

    pub(super) fn marker(&self, spinner_frame: usize) -> &'static str {
        self.indicator
            .map_or(if self.current { "•" } else { " " }, |indicator| {
                indicator.marker(spinner_frame)
            })
    }

    pub(super) fn line(
        &self,
        spinner_frame: usize,
        path_separator: &str,
        title_style: Style,
        detail_style: Style,
        status_style: Style,
    ) -> Line<'static> {
        let marker = self.marker(spinner_frame);
        Line::from(vec![
            Span::styled(format!(" {marker}"), status_style),
            Span::styled(format!(" {} ", self.source), title_style),
            Span::styled(self.status(), status_style),
            Span::styled(
                format!(
                    " · {}",
                    [
                        self.session.as_str(),
                        self.workspace.as_str(),
                        self.tab.as_str(),
                    ]
                    .join(path_separator)
                ),
                detail_style,
            ),
        ])
    }
}

#[derive(Clone, Copy)]
struct FocusedAncestry {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    tab_id: TabId,
}

pub(super) fn items(
    snapshot: &ResourceSnapshot,
    focused: &SelectedTarget,
    notifications: &NotificationState,
    scope: AgentScope,
) -> Vec<AgentItem> {
    let focused_ancestry = focused_ancestry(snapshot, focused);
    snapshot
        .pane_paths()
        .filter(|path| in_scope(*path, focused_ancestry, scope))
        .map(|path| AgentItem {
            terminal_id: path.pane.terminal_id,
            pane_id: path.pane.id,
            session: sanitize(&path.session.name),
            workspace: sanitize(&path.workspace.name),
            tab: sanitize(&path.tab.name),
            source: sanitize(
                path.pane
                    .activity
                    .integration
                    .as_ref()
                    .and_then(|integration| integration.source.as_deref())
                    .or_else(|| {
                        path.pane
                            .activity
                            .detection
                            .as_ref()
                            .map(|detection| detection.agent.as_str())
                    })
                    .unwrap_or("agent"),
            ),
            current: path.pane.id == focused.pane_id,
            indicator: notifications.indicator(std::slice::from_ref(path.pane)),
        })
        .collect()
}

pub(super) fn has_items(
    snapshot: &ResourceSnapshot,
    focused: &SelectedTarget,
    scope: AgentScope,
) -> bool {
    let focused_ancestry = focused_ancestry(snapshot, focused);
    snapshot
        .pane_paths()
        .any(|path| in_scope(path, focused_ancestry, scope))
}

fn focused_ancestry(snapshot: &ResourceSnapshot, focused: &SelectedTarget) -> FocusedAncestry {
    snapshot
        .pane_paths()
        .find(|path| path.pane.id == focused.pane_id && path_is_live(*path))
        .map_or(
            FocusedAncestry {
                session_id: focused.session_id,
                workspace_id: focused.workspace_id,
                tab_id: focused.tab_id,
            },
            |path| FocusedAncestry {
                session_id: path.session.id,
                workspace_id: path.workspace.id,
                tab_id: path.tab.id,
            },
        )
}

fn in_scope(path: PanePathRef<'_>, focused: FocusedAncestry, scope: AgentScope) -> bool {
    path_is_live(path)
        && (path.pane.activity.has_active_integration() || path.pane.activity.detection.is_some())
        && match scope {
            AgentScope::Tab => path.tab.id == focused.tab_id,
            AgentScope::Workspace => path.workspace.id == focused.workspace_id,
            AgentScope::Session => path.session.id == focused.session_id,
            AgentScope::Global => true,
        }
}

fn path_is_live(path: PanePathRef<'_>) -> bool {
    !path.session.closing && !path.workspace.closing && !path.tab.closing && !path.pane.closing
}
