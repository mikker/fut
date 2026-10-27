use crate::{
    domain::{BlockedKind, PaneId, SessionId, StatusState, TabId, TerminalId, WorkspaceId},
    protocol::SelectedTarget,
    resources::{PanePathRef, ResourceSnapshot},
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

use super::{
    chrome::sanitize,
    config::{AgentScope, SemanticStyle, SpinnerConfig},
    federation::{Generation, MachineId},
    notifications::{ActivityIndicator, NotificationState},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AgentItem {
    pub machine: MachineId,
    pub generation: Generation,
    pub machine_label: Option<String>,
    pub selectable: bool,
    pub terminal_id: TerminalId,
    pub pane_id: PaneId,
    pub session: String,
    pub workspace: String,
    pub tab: String,
    pub app: String,
    pub title: Option<String>,
    pub message: Option<String>,
    pub blocked_kind: Option<BlockedKind>,
    pub state: StatusState,
    pub progress: Option<u8>,
    pub current: bool,
    pub indicator: Option<ActivityIndicator>,
}

#[derive(Default)]
pub(super) struct LineStyles {
    pub title: Style,
    pub detail: Style,
    pub activity: Style,
    pub status: Style,
}

impl AgentItem {
    pub(super) fn status(&self) -> &'static str {
        match self.state {
            StatusState::Idle => "idle",
            StatusState::Working => "working",
            StatusState::Done => "done",
            StatusState::Blocked => "blocked",
            StatusState::Error => "error",
        }
    }

    pub(super) fn search_text(&self) -> String {
        format!(
            "{} {} {} {} {} {} {} {} {}",
            self.machine_label.as_deref().unwrap_or(""),
            self.app,
            self.status(),
            self.title.as_deref().unwrap_or(""),
            self.message.as_deref().unwrap_or(""),
            self.blocked_kind.map(blocked_kind_label).unwrap_or(""),
            self.session,
            self.workspace,
            self.tab
        )
    }

    pub(super) fn status_style(&self) -> SemanticStyle {
        match self.state {
            StatusState::Idle => SemanticStyle::Muted,
            StatusState::Working => SemanticStyle::Activity,
            StatusState::Done => SemanticStyle::Added,
            StatusState::Blocked | StatusState::Error => SemanticStyle::Error,
        }
    }

    pub(super) fn activity_style(&self) -> SemanticStyle {
        match self.indicator {
            Some(ActivityIndicator::Working) => SemanticStyle::Activity,
            Some(ActivityIndicator::Completed) => SemanticStyle::Added,
            Some(
                ActivityIndicator::UnreadBlocked
                | ActivityIndicator::UnreadError
                | ActivityIndicator::Blocked
                | ActivityIndicator::Error,
            ) => SemanticStyle::Error,
            Some(ActivityIndicator::Bell) => SemanticStyle::Attention,
            None => SemanticStyle::Muted,
        }
    }

    pub(super) fn marker<'a>(&self, elapsed_ms: usize, spinner: &'a SpinnerConfig) -> &'a str {
        self.indicator
            .map_or(if self.current { "•" } else { " " }, |indicator| {
                indicator.marker(elapsed_ms, spinner)
            })
    }

    pub(super) fn line(
        &self,
        elapsed_ms: usize,
        spinner: &SpinnerConfig,
        path_separator: &str,
        styles: LineStyles,
    ) -> Line<'static> {
        let marker = self.marker(elapsed_ms, spinner);
        let mut details = [
            self.title.as_deref(),
            self.message.as_deref(),
            self.blocked_kind.map(blocked_kind_label),
        ]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
        details.push(
            [
                self.session.as_str(),
                self.workspace.as_str(),
                self.tab.as_str(),
            ]
            .join(path_separator),
        );
        if let Some(machine) = &self.machine_label {
            details.push(machine.clone());
        }
        let mut spans = vec![Span::styled(format!(" {marker}"), styles.activity)];
        if let Some(progress) = self.progress {
            spans.push(Span::styled(format!(" {progress}%"), styles.status));
        }
        spans.extend([
            Span::styled(format!(" {} ", self.app), styles.title),
            Span::styled(self.status(), styles.status),
            Span::styled(format!(" · {}", details.join(" · ")), styles.detail),
        ]);
        Line::from(spans)
    }
}

fn blocked_kind_label(kind: BlockedKind) -> &'static str {
    match kind {
        BlockedKind::Permission => "permission",
        BlockedKind::Question => "question",
        BlockedKind::Auth => "auth",
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
        .filter_map(|path| {
            let record = path.pane.activity.summary_record()?;
            Some(AgentItem {
                machine: MachineId::Local,
                generation: Generation::default(),
                machine_label: None,
                selectable: true,
                terminal_id: path.pane.terminal_id,
                pane_id: path.pane.id,
                session: sanitize(&path.session.name),
                workspace: sanitize(&path.workspace.name),
                tab: sanitize(&path.tab.name),
                app: sanitize(path.pane.activity.label().unwrap_or(&path.tab.name)),
                title: record.title.as_deref().map(sanitize),
                message: record.message.as_deref().map(sanitize),
                blocked_kind: record.kind,
                state: record.state,
                progress: path.pane.activity.progress(),
                current: path.pane.id == focused.pane_id,
                indicator: notifications.agent_indicator(std::slice::from_ref(path.pane)),
            })
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
        && path.pane.activity.is_reporting()
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        domain::{ProgramStatus, StatusRecord, StatusSource},
        resources::{
            PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
        },
        splits::SplitTree,
    };

    fn fixture() -> (ResourceSnapshot, SelectedTarget) {
        let session_id = SessionId::new();
        let workspace_id = WorkspaceId::new();
        let tab_id = TabId::new();
        let pane_id = PaneId::new();
        let terminal_id = TerminalId::new();
        let activity = ProgramStatus {
            records: vec![
                StatusRecord {
                    id: String::new(),
                    state: StatusState::Idle,
                    kind: None,
                    progress: None,
                    app: Some("codex".into()),
                    title: None,
                    message: None,
                    revision: 1,
                    updated_at_ms: 1,
                    attention_revision: 0,
                },
                StatusRecord {
                    id: "tool".into(),
                    state: StatusState::Blocked,
                    kind: Some(BlockedKind::Permission),
                    progress: Some(37),
                    app: None,
                    title: Some("Approve command".into()),
                    message: Some("literal {tab.activity}\ntext\u{202e}".into()),
                    revision: 2,
                    updated_at_ms: 2,
                    attention_revision: 2,
                },
            ],
            source: Some(StatusSource::Report {
                source: Some("pi".into()),
                agent_session_id: None,
            }),
            revision: 2,
            attention_revision: 2,
            read_revision: 0,
            working_revision: 0,
        };
        let snapshot = ResourceSnapshot {
            revision: 1,
            sessions: vec![SessionSnapshot {
                tokens: Default::default(),
                id: session_id,
                name: "project".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/project")),
                },
                trusted_project_config: None,
                closing: false,
                workspaces: vec![WorkspaceSnapshot {
                    tokens: Default::default(),
                    id: workspace_id,
                    parent_workspace_id: None,
                    name: "main".into(),
                    root: PathBuf::from("/project"),
                    closing: false,
                    tabs: vec![TabSnapshot {
                        tokens: Default::default(),
                        id: tab_id,
                        name: "node".into(),
                        closing: false,
                        layout: SplitTree::leaf(pane_id),
                        panes: vec![PaneSnapshot {
                            name: None,
                            tokens: Default::default(),
                            id: pane_id,
                            terminal_id,
                            closing: false,
                            activity,
                            cwd: None,
                            worktree: None,
                        }],
                    }],
                }],
            }],
        };
        let selected = SelectedTarget {
            session_id,
            workspace_id,
            tab_id,
            pane_id,
            terminal_id,
            child_pid: 1,
        };
        (snapshot, selected)
    }

    #[test]
    fn rows_render_inherited_app_title_literal_message_blocked_kind_and_progress() {
        let (snapshot, focused) = fixture();
        let rows = items(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Global,
        );
        let row = &rows[0];

        assert_eq!(row.app, "codex", "the child inherits the current root app");
        assert_eq!(row.title.as_deref(), Some("Approve command"));
        assert_eq!(row.message.as_deref(), Some("literal {tab.activity}�text�"));
        assert_eq!(row.blocked_kind, Some(BlockedKind::Permission));
        assert_eq!(row.progress, Some(37));
        assert_eq!(row.state, StatusState::Blocked);
        let line = row
            .line(0, &SpinnerConfig::default(), "/", LineStyles::default())
            .to_string();
        assert!(line.contains("! 37% codex blocked"), "{line:?}");
        assert!(line.contains("Approve command"), "{line:?}");
        assert!(line.contains("literal {tab.activity}�text�"), "{line:?}");
        assert!(line.contains("permission"), "{line:?}");
    }

    #[test]
    fn app_uses_source_only_program_label_then_falls_back_to_tab_name() {
        let (mut snapshot, focused) = fixture();
        let status = &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0].activity;
        status.records[0].app = None;
        status.records.remove(1);

        let reported = items(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Global,
        );
        assert_eq!(reported[0].app, "pi");

        let status = &mut snapshot.sessions[0].workspaces[0].tabs[0].panes[0].activity;
        status.source = Some(StatusSource::Report {
            source: Some(String::new()),
            agent_session_id: None,
        });
        let process = items(
            &snapshot,
            &focused,
            &NotificationState::default(),
            AgentScope::Global,
        );
        assert_eq!(process[0].app, "node");
    }
}
