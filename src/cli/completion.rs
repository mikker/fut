use std::{ffi::OsStr, path::PathBuf, time::Duration};

use clap::CommandFactory;
use clap_complete::{CompleteEnv, engine::CompletionCandidate};

use crate::{
    daemon::path::socket_path,
    protocol::{ClientMessage, ServerMessage},
    resources::ResourceSnapshot,
};

use super::Cli;

const COMPLETION_TIMEOUT: Duration = Duration::from_millis(200);

pub(super) fn complete_env() {
    CompleteEnv::with_factory(Cli::command).complete();
}

macro_rules! completer {
    ($name:ident, $operation:ident) => {
        pub(super) fn $name(_: &OsStr) -> Vec<CompletionCandidate> {
            dynamic_candidates(Operation::$operation)
        }
    };
}

completer!(session_attach, SessionAttach);
completer!(session_rename, SessionRename);
completer!(session_close, SessionClose);
completer!(workspace_attach, WorkspaceAttach);
completer!(workspace_rename, WorkspaceRename);
completer!(workspace_close, WorkspaceClose);
completer!(tab_new, TabNew);
completer!(tab_attach, TabAttach);
completer!(tab_rename, TabRename);
completer!(tab_close, TabClose);
completer!(pane_attach, PaneAttach);
completer!(pane_close, PaneClose);
completer!(terminal_attach, TerminalAttach);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operation {
    SessionAttach,
    SessionRename,
    SessionClose,
    WorkspaceAttach,
    WorkspaceRename,
    WorkspaceClose,
    TabNew,
    TabAttach,
    TabRename,
    TabClose,
    PaneAttach,
    PaneClose,
    TerminalAttach,
}

fn dynamic_candidates(operation: Operation) -> Vec<CompletionCandidate> {
    let Some(snapshot) = fetch_snapshot() else {
        return vec![];
    };
    candidates(&snapshot, operation)
        .into_iter()
        .enumerate()
        .map(|(order, candidate)| {
            CompletionCandidate::new(candidate.value)
                .help(Some(candidate.help.into()))
                .display_order(Some(order))
        })
        .collect()
}

fn fetch_snapshot() -> Option<ResourceSnapshot> {
    let explicit = explicit_socket(std::env::args_os());
    let socket = socket_path(explicit.as_deref()).ok()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .ok()?;
    runtime.block_on(async {
        match tokio::time::timeout(
            COMPLETION_TIMEOUT,
            super::control(&socket, ClientMessage::ListResources),
        )
        .await
        {
            Ok(Ok(ServerMessage::Resources { snapshot })) => Some(snapshot),
            _ => None,
        }
    })
}

fn explicit_socket(
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
) -> Option<PathBuf> {
    let mut args = args.into_iter().map(Into::into);
    args.next()?;
    if args.next()?.as_os_str() != "--" {
        return None;
    }
    args.next()?;

    while let Some(argument) = args.next() {
        if argument == "--" {
            break;
        }
        if argument == "--socket" {
            return args.next().map(PathBuf::from);
        }
        if let Some(value) = argument
            .to_str()
            .and_then(|value| value.strip_prefix("--socket="))
        {
            return Some(value.into());
        }
    }
    None
}

#[derive(Debug, Eq, PartialEq)]
struct Candidate {
    value: String,
    help: String,
}

fn candidates(snapshot: &ResourceSnapshot, operation: Operation) -> Vec<Candidate> {
    let mut result = Vec::new();
    for session in &snapshot.sessions {
        let session_live = !session.closing;
        let session_panes = session
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| &tab.panes);
        let live_workspace_panes = session
            .workspaces
            .iter()
            .filter(|workspace| !workspace.closing)
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| &tab.panes);
        let descendants_live = session
            .workspaces
            .iter()
            .all(|workspace| !workspace.closing);
        let session_attachable = session_live
            && live_workspace_panes.clone().count() == 1
            && live_workspace_panes.clone().all(|pane| !pane.closing);
        let session_closable =
            session_live && descendants_live && session_panes.clone().all(|pane| !pane.closing);
        let session_label = clean(&session.name);

        if matches!(operation, Operation::SessionAttach) && session_attachable
            || matches!(operation, Operation::SessionRename) && session_live
            || matches!(operation, Operation::SessionClose) && session_closable
        {
            push(
                &mut result,
                session.id.to_string(),
                format!("session {session_label}"),
            );
        }

        for workspace in &session.workspaces {
            let workspace_live = session_live && !workspace.closing;
            let workspace_panes = workspace.tabs.iter().flat_map(|tab| &tab.panes);
            let workspace_attachable = workspace_live
                && workspace_panes.clone().count() == 1
                && workspace_panes.clone().all(|pane| !pane.closing);
            let workspace_closable =
                workspace_live && workspace_panes.clone().all(|pane| !pane.closing);
            let hierarchy = format!(
                "session {session_label} › workspace {}",
                clean(&workspace.name)
            );
            let root_suffix = format!(" — {}", clean(&workspace.root.to_string_lossy()));
            let workspace_label = format!("{hierarchy}{root_suffix}");

            if matches!(operation, Operation::WorkspaceAttach) && workspace_attachable
                || matches!(operation, Operation::WorkspaceRename | Operation::TabNew)
                    && workspace_live
                || matches!(operation, Operation::WorkspaceClose) && workspace_closable
            {
                push(
                    &mut result,
                    workspace.id.to_string(),
                    workspace_label.clone(),
                );
            }

            for tab in &workspace.tabs {
                let tab_panes_open = tab.panes.iter().all(|pane| !pane.closing);
                let tab_attachable = workspace_live
                    && tab.panes.len() == 1
                    && tab.panes.iter().all(|pane| !pane.closing);
                let tab_hierarchy = format!("{hierarchy} › tab {}", clean(&tab.name));
                let tab_label = format!("{tab_hierarchy}{root_suffix}");

                if matches!(operation, Operation::TabAttach) && tab_attachable
                    || matches!(operation, Operation::TabRename) && workspace_live
                    || matches!(operation, Operation::TabClose) && workspace_live && tab_panes_open
                {
                    push(&mut result, tab.id.to_string(), tab_label.clone());
                }

                for (index, pane) in tab.panes.iter().enumerate() {
                    let pane_live = workspace_live && !pane.closing;
                    let pane_hierarchy = format!("{tab_hierarchy} › pane {}", index + 1);
                    let pane_label = format!("{pane_hierarchy}{root_suffix}");
                    if matches!(operation, Operation::PaneAttach | Operation::PaneClose)
                        && pane_live
                    {
                        push(&mut result, pane.id.to_string(), pane_label.clone());
                    }
                    if matches!(operation, Operation::TerminalAttach) && pane_live {
                        push(
                            &mut result,
                            pane.terminal_id.to_string(),
                            format!("{pane_hierarchy} › terminal process{root_suffix}"),
                        );
                    }
                }
            }
        }
    }
    result
}

fn push(result: &mut Vec<Candidate>, value: String, help: String) {
    result.push(Candidate { value, help });
}

fn clean(value: &str) -> String {
    value
        .split(|character: char| character.is_control() || character.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{PaneId, SessionId, TabId, TerminalId, WorkspaceId},
        resources::{
            InitialPath, PaneSnapshot, Project, ProjectIdentity, ResourceTree, SessionSelector,
            SessionSnapshot, TabPath, TabSnapshot, TargetSelector, WorkspacePath,
            WorkspaceSnapshot,
        },
    };

    fn initial(name: &str, root: &str) -> InitialPath {
        InitialPath {
            session_id: SessionId::new(),
            session_name: name.into(),
            project: Project {
                identity: ProjectIdentity::CanonicalDirectory(root.into()),
            },
            workspace_id: WorkspaceId::new(),
            workspace_name: format!("{name}-workspace"),
            root: root.into(),
            tab_id: TabId::new(),
            tab_name: format!("{name}-tab"),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        }
    }

    fn workspace(name: &str, root: &str) -> WorkspacePath {
        WorkspacePath {
            workspace_id: WorkspaceId::new(),
            workspace_name: name.into(),
            root: root.into(),
            tab_id: TabId::new(),
            tab_name: format!("{name}-tab"),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        }
    }

    fn tab(name: &str) -> TabPath {
        TabPath {
            tab_id: TabId::new(),
            tab_name: name.into(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        }
    }

    fn fixture() -> ResourceSnapshot {
        let pane = |closing| PaneSnapshot {
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing,
        };
        ResourceSnapshot {
            revision: 1,
            sessions: vec![SessionSnapshot {
                id: SessionId::new(),
                name: "Sés\nsion".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/root")),
                },
                closing: false,
                workspaces: vec![WorkspaceSnapshot {
                    id: WorkspaceId::new(),
                    name: "W\tork".into(),
                    root: "/root\nwork".into(),
                    closing: false,
                    tabs: vec![
                        TabSnapshot {
                            id: TabId::new(),
                            name: "One".into(),
                            panes: vec![pane(false)],
                        },
                        TabSnapshot {
                            id: TabId::new(),
                            name: "Two".into(),
                            panes: vec![pane(true)],
                        },
                    ],
                }],
            }],
        }
    }

    fn representative_trees() -> Vec<ResourceTree> {
        let mut rich = ResourceTree::default();
        let first = initial("first", "/first");
        let (first_session, first_workspace, first_tab) =
            (first.session_id, first.workspace_id, first.tab_id);
        rich.create_session(first).unwrap();
        rich.add_pane(first_tab, PaneId::new(), TerminalId::new())
            .unwrap();
        rich.add_tab(first_workspace, tab("second-tab")).unwrap();
        rich.add_workspace(first_session, workspace("second-workspace", "/second"))
            .unwrap();
        rich.create_session(initial("second", "/third")).unwrap();

        let pending = |scope: Operation| {
            let mut tree = ResourceTree::default();
            let path = initial("pending", "/pending");
            let (session, workspace, tab, pane) = (
                path.session_id,
                path.workspace_id,
                path.tab_id,
                path.pane_id,
            );
            tree.create_session(path).unwrap();
            match scope {
                Operation::PaneClose => tree.close_pane(pane).unwrap(),
                Operation::TabClose => tree.close_tab(tab).unwrap(),
                Operation::WorkspaceClose => tree.close_workspace(workspace).unwrap(),
                Operation::SessionClose => tree.close_session(session).unwrap(),
                _ => unreachable!(),
            };
            tree
        };

        let mut closing_workspace = ResourceTree::default();
        let path = initial("attach", "/attach-live");
        let session = path.session_id;
        closing_workspace.create_session(path).unwrap();
        let closing = workspace("closing", "/attach-closing");
        let closing_id = closing.workspace_id;
        closing_workspace.add_workspace(session, closing).unwrap();
        closing_workspace.close_workspace(closing_id).unwrap();

        vec![
            rich,
            pending(Operation::PaneClose),
            pending(Operation::TabClose),
            pending(Operation::WorkspaceClose),
            pending(Operation::SessionClose),
            closing_workspace,
        ]
    }

    fn authority_accepts(tree: &ResourceTree, operation: Operation, value: &str) -> bool {
        let snapshot = tree.snapshot();
        let mut clone = tree.clone();

        for session in snapshot.sessions {
            if session.id.to_string() == value {
                return match operation {
                    Operation::SessionAttach => clone
                        .resolve_terminal_target(Some(TargetSelector::Session(
                            SessionSelector::Id(session.id),
                        )))
                        .is_ok(),
                    Operation::SessionRename => clone
                        .rename_session(session.id, "completion-fresh-session".into())
                        .is_ok(),
                    Operation::SessionClose => clone.close_session(session.id).is_ok(),
                    _ => false,
                };
            }
            for workspace in session.workspaces {
                if workspace.id.to_string() == value {
                    return match operation {
                        Operation::WorkspaceAttach => clone
                            .resolve_terminal_target(Some(TargetSelector::Workspace(workspace.id)))
                            .is_ok(),
                        Operation::WorkspaceRename => clone
                            .rename_workspace(workspace.id, "completion-fresh-workspace".into())
                            .is_ok(),
                        Operation::WorkspaceClose => clone.close_workspace(workspace.id).is_ok(),
                        Operation::TabNew => clone
                            .add_tab(workspace.id, tab("completion-fresh-tab"))
                            .is_ok(),
                        _ => false,
                    };
                }
                for tab in workspace.tabs {
                    if tab.id.to_string() == value {
                        return match operation {
                            Operation::TabAttach => clone
                                .resolve_terminal_target(Some(TargetSelector::Tab(tab.id)))
                                .is_ok(),
                            Operation::TabRename => clone
                                .rename_tab(tab.id, "completion-fresh-tab".into())
                                .is_ok(),
                            Operation::TabClose => clone.close_tab(tab.id).is_ok(),
                            _ => false,
                        };
                    }
                    for pane in tab.panes {
                        if pane.id.to_string() == value {
                            return match operation {
                                Operation::PaneAttach => clone
                                    .resolve_terminal_target(Some(TargetSelector::Pane(pane.id)))
                                    .is_ok(),
                                Operation::PaneClose => clone.close_pane(pane.id).is_ok(),
                                _ => false,
                            };
                        }
                        if pane.terminal_id.to_string() == value {
                            return matches!(operation, Operation::TerminalAttach)
                                && clone
                                    .resolve_terminal_target(Some(TargetSelector::Terminal(
                                        pane.terminal_id,
                                    )))
                                    .is_ok();
                        }
                    }
                }
            }
        }
        false
    }

    #[test]
    fn candidates_match_resource_tree_authority_for_every_operation_and_target() {
        let operations = [
            Operation::SessionAttach,
            Operation::SessionRename,
            Operation::SessionClose,
            Operation::WorkspaceAttach,
            Operation::WorkspaceRename,
            Operation::WorkspaceClose,
            Operation::TabNew,
            Operation::TabAttach,
            Operation::TabRename,
            Operation::TabClose,
            Operation::PaneAttach,
            Operation::PaneClose,
            Operation::TerminalAttach,
        ];

        for tree in representative_trees() {
            let snapshot = tree.snapshot();
            let all_values = snapshot.sessions.iter().flat_map(|session| {
                std::iter::once(session.id.to_string()).chain(session.workspaces.iter().flat_map(
                    |workspace| {
                        std::iter::once(workspace.id.to_string()).chain(
                            workspace.tabs.iter().flat_map(|tab| {
                                std::iter::once(tab.id.to_string()).chain(
                                    tab.panes.iter().flat_map(|pane| {
                                        [pane.id.to_string(), pane.terminal_id.to_string()]
                                    }),
                                )
                            }),
                        )
                    },
                ))
            });
            let all_values: Vec<_> = all_values.collect();

            for operation in operations {
                let offered: Vec<_> = candidates(&snapshot, operation)
                    .into_iter()
                    .map(|candidate| candidate.value)
                    .collect();
                for value in &all_values {
                    assert_eq!(
                        offered.contains(value),
                        authority_accepts(&tree, operation, value),
                        "{operation:?} target {value}"
                    );
                }
                assert!(
                    offered.iter().all(|value| all_values.contains(value)),
                    "{operation:?} offered an unknown target"
                );
            }
        }
    }

    #[test]
    fn values_are_full_raw_ids_labels_are_sanitized_and_order_is_snapshot_order() {
        let snapshot = fixture();
        let workspace = &snapshot.sessions[0].workspaces[0];
        let panes = candidates(&snapshot, Operation::PaneAttach);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].value, workspace.tabs[0].panes[0].id.to_string());
        assert_eq!(
            panes[0].help,
            "session Sés sion › workspace W ork › tab One › pane 1 — /root work"
        );
        assert!(!panes[0].help.chars().any(char::is_control));
        assert!(
            panes[0]
                .help
                .contains("workspace W ork › tab One › pane 1 — /root work")
        );
        let terminals = candidates(&snapshot, Operation::TerminalAttach);
        assert_eq!(
            terminals[0].value,
            workspace.tabs[0].panes[0].terminal_id.to_string()
        );
        assert!(
            terminals[0]
                .help
                .contains("pane 1 › terminal process — /root work")
        );
    }

    #[test]
    fn attach_rejects_mixed_open_and_closing_panes() {
        let snapshot = fixture();
        assert!(candidates(&snapshot, Operation::SessionAttach).is_empty());
        assert!(candidates(&snapshot, Operation::WorkspaceAttach).is_empty());
        assert_eq!(candidates(&snapshot, Operation::TabAttach).len(), 1);
    }

    #[test]
    fn session_attach_ignores_wholly_closing_workspaces() {
        let mut snapshot = fixture();
        let closing_workspace = snapshot.sessions[0].workspaces[0].clone();
        snapshot.sessions[0].workspaces[0].tabs.truncate(1);
        snapshot.sessions[0].workspaces.push(WorkspaceSnapshot {
            closing: true,
            ..closing_workspace
        });

        assert_eq!(candidates(&snapshot, Operation::SessionAttach).len(), 1);
        assert!(candidates(&snapshot, Operation::WorkspaceAttach).len() == 1);
    }

    #[test]
    fn sibling_panes_have_distinct_hierarchical_descriptions() {
        let mut snapshot = fixture();
        let second = PaneSnapshot {
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing: false,
        };
        snapshot.sessions[0].workspaces[0].tabs[0]
            .panes
            .push(second);

        let panes = candidates(&snapshot, Operation::PaneAttach);
        assert!(panes[0].help.contains("pane 1 — /root work"));
        assert!(panes[1].help.contains("pane 2 — /root work"));
        let terminals = candidates(&snapshot, Operation::TerminalAttach);
        assert!(
            terminals[1]
                .help
                .contains("pane 2 › terminal process — /root work")
        );
    }

    #[test]
    fn pending_descendant_close_blocks_tree_closes_but_not_exact_renames() {
        let snapshot = fixture();
        assert!(candidates(&snapshot, Operation::SessionClose).is_empty());
        assert!(candidates(&snapshot, Operation::WorkspaceClose).is_empty());
        assert_eq!(candidates(&snapshot, Operation::TabClose).len(), 1);
        assert_eq!(candidates(&snapshot, Operation::SessionRename).len(), 1);
        assert_eq!(candidates(&snapshot, Operation::WorkspaceRename).len(), 1);
        assert_eq!(candidates(&snapshot, Operation::TabRename).len(), 2);
        assert_eq!(candidates(&snapshot, Operation::TabNew).len(), 1);
    }

    #[test]
    fn closing_ancestry_excludes_its_whole_subtree() {
        let mut snapshot = fixture();
        snapshot.sessions[0].closing = true;
        for operation in [
            Operation::SessionAttach,
            Operation::SessionRename,
            Operation::SessionClose,
            Operation::WorkspaceAttach,
            Operation::WorkspaceRename,
            Operation::WorkspaceClose,
            Operation::TabNew,
            Operation::TabAttach,
            Operation::TabRename,
            Operation::TabClose,
            Operation::PaneAttach,
            Operation::PaneClose,
            Operation::TerminalAttach,
        ] {
            assert!(candidates(&snapshot, operation).is_empty(), "{operation:?}");
        }
    }

    #[test]
    fn extracts_global_socket_from_completion_process_argv() {
        assert_eq!(
            explicit_socket(["fut", "--", "fut", "--socket", "/one", "list"]),
            Some("/one".into())
        );
        assert_eq!(
            explicit_socket(["fut", "--", "fut", "tab", "--socket=/two", "new"]),
            Some("/two".into())
        );
        assert_eq!(
            explicit_socket(["fut", "--", "fut", "open", "--", "--socket", "/child"]),
            None
        );
    }

    #[test]
    fn socket_extraction_requires_a_well_formed_transport_shape() {
        assert_eq!(explicit_socket(["fut", "--socket", "/one"]), None);
        assert_eq!(explicit_socket(["fut", "--"]), None);
        assert_eq!(explicit_socket(["fut", "--", "fut", "--socket"]), None);
    }
}
