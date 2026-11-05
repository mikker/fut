use super::*;
use fut::{
    layout::{LayoutAction, LayoutReply},
    splits::{SplitDirection, SplitRatio, SplitTree},
};
use serde_json::json;

#[cfg(target_os = "linux")]
mod incus;

async fn save(harness: &Harness) -> LayoutReply {
    time::timeout(DEADLINE, async {
        loop {
            match harness.control_command(ClientMessage::SaveLayout).await {
                ServerMessage::LayoutSaved { reply } => return reply,
                ServerMessage::Error { message, .. }
                    if message.contains("changed while capturing") =>
                {
                    time::sleep(POLL_INTERVAL).await
                }
                other => panic!("save failed: {other:?}"),
            }
        }
    })
    .await
    .unwrap()
}

pub(super) async fn action(harness: &Harness, action: LayoutAction) -> LayoutReply {
    match harness
        .control_command(ClientMessage::Layout { action })
        .await
    {
        ServerMessage::LayoutSaved { reply } => reply,
        other => panic!("layout action failed: {other:?}"),
    }
}

pub(super) async fn approve(harness: &Harness) -> LayoutReply {
    let review = action(harness, LayoutAction::Review).await;
    let fingerprints = review
        .commands
        .iter()
        .map(|c| c.fingerprint.clone())
        .collect();
    action(harness, LayoutAction::Approve { fingerprints }).await
}

pub(super) async fn stop(harness: &mut Harness) {
    assert!(matches!(
        harness.control_command(ClientMessage::Shutdown).await,
        ServerMessage::CommandCompleted { .. }
    ));
    harness.wait_until_exited().await;
}

pub(super) async fn restart_empty(harness: &mut Harness) {
    assert!(harness.daemon.try_wait().unwrap().is_some());
    let mut command = harness.cli();
    command
        .args(["daemon", "run", "--restore-only", "--cwd"])
        .arg(harness.root.path().join("cwd"))
        .stdout(fs::File::create(harness.root.path().join("daemon.stdout")).unwrap())
        .stderr(fs::File::create(harness.root.path().join("daemon.stderr")).unwrap());
    harness.daemon = command.spawn().unwrap();
    harness.wait_until_ready().await;
    assert!(harness.resources().await.sessions.is_empty());
}

fn review_client(harness: &Harness, json: bool) -> PtyChild {
    let template = harness.cli();
    let mut command = Command::new("/usr/bin/script");
    command.env_clear().envs(
        template
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key, value))),
    );
    command.args(script_command_args()).arg(format!(
        "stty rows 24 cols 100; exec '{}' --socket '{}' {} layout review",
        env!("CARGO_BIN_EXE_fut"),
        harness.socket.display(),
        if json { "--json" } else { "" },
    ));
    PtyChild::spawn(command)
}

#[tokio::test]
async fn layout_review_checklist_confirms_multiple_commands_and_reopens_persisted_status() {
    let harness = Harness::start_with("exec /bin/sleep 600", recipe_setup).await;
    wait_for_cwds(&harness).await;
    save(&harness).await;
    let review = action(&harness, LayoutAction::Review).await;
    assert_eq!(review.commands.len(), 3);
    action(
        &harness,
        LayoutAction::Approve {
            fingerprints: vec![review.commands[0].fingerprint.clone()],
        },
    )
    .await;
    let before = fs::read(&review.path).unwrap();
    let mut client = review_client(&harness, false);
    client.wait_for_screen("[x] approved | keep approval").await;
    client
        .wait_for_screen("[ ] pending | restore as shell")
        .await;
    client.wait_for_screen("executable: \"/bin/sleep\"").await;
    client.send(b" \x1b[B \x1b[B ");
    client.wait_for_screen("revoke on confirm").await;
    client.wait_for_screen("approve on confirm").await;
    assert_eq!(fs::read(&review.path).unwrap(), before);
    client.send(b"\r");
    client.wait_for_screen("Confirm approval changes?").await;
    assert_eq!(fs::read(&review.path).unwrap(), before);
    client.send(b"y");
    client.wait_success().await;
    let updated = action(&harness, LayoutAction::Review).await;
    assert!(!updated.commands[0].approved);
    assert!(updated.commands[1..].iter().all(|c| c.approved));
    let mut reopened = review_client(&harness, false);
    reopened
        .wait_for_screen("[ ] pending | restore as shell")
        .await;
    reopened
        .wait_for_screen("[x] approved | keep approval")
        .await;
    reopened.send(b"\x03");
    reopened.wait_success().await;
    assert_eq!(
        action(&harness, LayoutAction::Review).await.commands,
        updated.commands
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_review_cancellation_and_stale_confirmation_leave_approvals_unchanged() {
    let mut harness = Harness::start_with("exec /bin/sleep 600", recipe_setup).await;
    wait_for_cwds(&harness).await;
    save(&harness).await;
    approve(&harness).await;
    let review = action(&harness, LayoutAction::Review).await;
    stop(&mut harness).await;
    for confirmation in [false, true] {
        let before = fs::read(&review.path).unwrap();
        let mut client = review_client(&harness, false);
        client.wait_for_screen("[x] approved").await;
        client.send(b" ");
        client.wait_for_screen("revoke on confirm").await;
        if confirmation {
            client.send(b"\r");
            client.wait_for_screen("Confirm approval changes?").await;
        }
        client.send(b"\x1b");
        client.wait_success().await;
        assert_eq!(fs::read(&review.path).unwrap(), before);
    }
    let mut client = review_client(&harness, false);
    client.wait_for_screen("[x] approved").await;
    client.send(b" ");
    let mut changed: Value = serde_json::from_slice(&fs::read(&review.path).unwrap()).unwrap();
    changed["projects"][0]["workspaces"][0]["tabs"][0]["panes"][0]["command"]["args"] =
        json!(["999"]);
    fs::write(&review.path, serde_json::to_vec_pretty(&changed).unwrap()).unwrap();
    let before = fs::read(&review.path).unwrap();
    client.send(b"\r");
    client.wait_for_screen("Confirm approval changes?").await;
    client.send(b"y");
    client.wait_success().await;
    assert!(
        client
            .text()
            .contains("saved commands or approvals changed"),
        "{}",
        client.text()
    );
    assert_eq!(fs::read(&review.path).unwrap(), before);
    let mut reopened = review_client(&harness, false);
    reopened
        .wait_for_screen("[ ] pending | restore as shell")
        .await;
    reopened.wait_for_screen("argv: [\"999\"]").await;
    reopened.send(b"\x1b");
    reopened.wait_success().await;
    assert_eq!(fs::read(&review.path).unwrap(), before);
}

#[tokio::test]
async fn layout_review_noninteractive_and_json_never_change_approvals_or_open_a_menu() {
    let mut harness = Harness::start_with("exec /bin/sleep 600", recipe_setup).await;
    wait_for_cwds(&harness).await;
    save(&harness).await;
    let review = action(&harness, LayoutAction::Review).await;
    action(
        &harness,
        LayoutAction::Approve {
            fingerprints: vec![review.commands[0].fingerprint.clone()],
        },
    )
    .await;
    for offline in [false, true] {
        if offline {
            stop(&mut harness).await;
        }
        let before = fs::read(&review.path).unwrap();
        let plain = harness.cli().args(["layout", "review"]).output().unwrap();
        assert!(
            plain.status.success(),
            "{}",
            String::from_utf8_lossy(&plain.stderr)
        );
        let text = String::from_utf8(plain.stdout).unwrap();
        assert!(text.contains("approved"));
        assert!(text.contains("pending"));
        assert!(text.contains("executable:"));
        assert!(!text.contains('\x1b'));
        let json = harness
            .cli()
            .args(["--json", "layout", "review"])
            .output()
            .unwrap();
        assert!(json.status.success());
        let value: Value = serde_json::from_slice(&json.stdout).unwrap();
        assert_eq!(value["result"]["commands"].as_array().unwrap().len(), 3);
        assert_eq!(value["result"]["commands"][0]["approved"], true);
        assert_eq!(value["result"]["commands"][1]["approved"], false);
        let mut tty_json = review_client(&harness, true);
        tty_json.wait_success().await;
        assert!(tty_json.text().contains("\"commands\""));
        assert!(!tty_json.text().contains("?1049h"));
        assert_eq!(fs::read(&review.path).unwrap(), before);
    }
}

#[tokio::test]
async fn layout_review_empty_explains_shell_fallback_and_available_capture_warnings() {
    let mut harness = Harness::start_with("cd sub; /bin/sleep 600 | /bin/sleep 601", |root| {
        fs::create_dir(root.join("cwd/sub")).unwrap();
    })
    .await;
    wait_for_cwds(&harness).await;
    let saved = save(&harness).await;
    assert!(
        action(&harness, LayoutAction::Review)
            .await
            .commands
            .is_empty()
    );
    stop(&mut harness).await;
    let before = fs::read(&saved.path).unwrap();
    let plain = harness.cli().args(["layout", "review"]).output().unwrap();
    assert!(plain.status.success());
    let text = String::from_utf8(plain.stdout).unwrap();
    assert!(text.contains("No restart commands were captured"));
    assert!(text.contains("restore as shells"));
    assert!(text.contains("Capture warning"));
    assert!(text.contains("could not be identified reliably"));
    let mut client = review_client(&harness, false);
    client.wait_success().await;
    assert!(client.text().contains("No restart commands were captured"));
    assert!(!client.text().contains("?1049h"));
    let json = harness
        .cli()
        .args(["--json", "layout", "review"])
        .output()
        .unwrap();
    assert!(json.status.success());
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["result"]["commands"], json!([]));
    assert_eq!(fs::read(&saved.path).unwrap(), before);
}

fn recipe_setup(root: &std::path::Path) {
    fs::create_dir_all(root.join("cwd/.fut")).unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    fs::create_dir(root.join("other")).unwrap();
    fs::create_dir(root.join("cwd/sub")).unwrap();
    let nested = root.join("nested");
    fs::write(
        root.join("cwd/.fut/project.toml"),
        format!(
            r#"
environment = {{ RESTORE_ROOT = "current" }}
focus = "main.code.first"
[[workspaces]]
id = "main"
title = "Primary"
[[workspaces.tabs]]
id = "code"
title = "Code"
environment = {{ RESTORE_TAB = "current" }}
[[workspaces.tabs.panes]]
id = "first"
title = "Editor"
command = ["/bin/sleep", "600"]
exec = true
cwd = "sub"
[[workspaces.tabs.panes]]
id = "second"
title = "Agent"
command = ["/bin/sleep", "601"]
exec = true
split = {{ target = "first", direction = "right", ratio = {{ numerator = 2, denominator = 3 }} }}
[[workspaces]]
id = "nested"
parent = "main"
cwd = {:?}
title = "Unrelated repository"
[[workspaces.tabs]]
id = "tools"
title = "Tools"
[[workspaces.tabs.panes]]
id = "shell"
command = ["/bin/sleep", "602"]
exec = true
"#,
            nested.to_str().unwrap()
        ),
    )
    .unwrap();
    project_trust_cli(root, "trust", &root.join("cwd"));
}

fn topology(snapshot: &fut::resources::ResourceSnapshot) -> Value {
    Value::Array(snapshot.sessions.iter().map(|s| json!({
        "name": s.name,
        "project": s.project,
        "workspaces": s.workspaces.iter().map(|w| json!({
            "name": w.name,
            "root": w.root,
            "parent": w.parent_workspace_id.map(|id| s.workspaces.iter().position(|candidate| candidate.id == id).unwrap()),
            "tabs": w.tabs.iter().map(|t| json!({
                "name": t.name,
                "layout": tree_shape(&t.layout),
                "panes": t.panes.iter().map(|p| json!({ "name": p.name, "cwd": p.cwd })).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        })).collect::<Vec<_>>()
    })).collect())
}

fn tree_shape(tree: &SplitTree) -> Value {
    match tree {
        SplitTree::Leaf { .. } => json!("pane"),
        SplitTree::Branch {
            axis,
            ratio,
            first,
            second,
            ..
        } => {
            json!({ "axis": axis, "ratio": ratio, "first": tree_shape(first), "second": tree_shape(second) })
        }
    }
}

async fn wait_for_cwds(harness: &Harness) -> fut::resources::ResourceSnapshot {
    time::timeout(DEADLINE, async {
        loop {
            let snapshot = harness.resources().await;
            if snapshot.pane_paths().all(|path| path.pane.cwd.is_some()) {
                return snapshot;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn layout_restore_recreates_whole_host_order_geometry_names_and_fresh_processes() {
    let mut harness = Harness::start_with("exec /bin/sleep 600", recipe_setup).await;
    let before = wait_for_cwds(&harness).await;
    let owner = &before.sessions[0];
    let first = owner.workspaces[0].tabs[0].panes[0].id;
    let split = harness
        .control_command(ClientMessage::SplitPane {
            pane_id: first,
            direction: SplitDirection::Down,
            cwd: Some(harness.root.path().join("cwd/sub")),
            program: Some("/bin/sleep".into()),
            argv: vec!["603".into()],
        })
        .await;
    let ServerMessage::PaneCreated { selected: split } = split else {
        panic!("{split:?}")
    };
    harness
        .control_command(ClientMessage::RenameTarget {
            selector: RenameSelector::Pane(split.pane_id),
            name: "Tests".into(),
        })
        .await;
    let other = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
            name: Some("Other project".into()),
            parent_workspace_id: None,
            cwd: harness.root.path().join("other"),
            program: Some("/bin/sleep".into()),
            argv: vec!["604".into()],
        })
        .await;
    assert!(matches!(other, ServerMessage::LocationOpened { .. }));
    let (mut client, terminal, old_pid) = harness
        .interactive_for(Some(TargetSelector::Pane(split.pane_id)))
        .await;
    let layout = harness.resources().await.sessions[0].workspaces[0].tabs[0]
        .layout
        .clone();
    let SplitTree::Branch { split_id, .. } = layout else {
        unreachable!()
    };
    send(
        &mut client,
        ClientMessage::ResizeSplit {
            tab_id: split.tab_id,
            split_id,
            ratio: SplitRatio::from_cells(7, 11).unwrap(),
        },
    )
    .await;
    send(&mut client, ClientMessage::Ping).await;
    receive_matching(&mut client, |message| {
        matches!(message, ServerMessage::Pong { .. })
    })
    .await;
    harness.detach(&mut client).await;
    drop(client);
    let before = wait_for_cwds(&harness).await;
    let saved = save(&harness).await;
    assert_eq!(saved.projects.len(), 2);
    assert_eq!(saved.projects[0].panes, 4);
    assert!(saved.projects.iter().all(|p| p.pending_commands > 0));
    approve(&harness).await;
    action(&harness, LayoutAction::Enable).await;
    let response = harness
        .control_command(ClientMessage::RestoreLayout { project: None })
        .await;
    let ServerMessage::LayoutRestored { report } = response else {
        panic!("{response:?}")
    };
    assert!(report.restored.is_empty());
    assert_eq!(report.already_live.len(), 2);
    assert!(process_alive(old_pid));
    assert_eq!(topology(&harness.resources().await), topology(&before));
    stop(&mut harness).await;
    let disk: Value = serde_json::from_str(&fs::read_to_string(saved.path).unwrap()).unwrap();
    assert_eq!(disk["projects"].as_array().unwrap().len(), 2);
    assert_eq!(
        disk["selected_project"],
        serde_json::to_value(&before.sessions[0].project).unwrap()
    );
    harness.restart("exit 77").await;
    let after = wait_for_cwds(&harness).await;
    assert_eq!(topology(&after), topology(&before));
    assert_ne!(
        after.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id,
        terminal
    );
    let focused = after
        .pane_paths()
        .find(|path| path.pane.name.as_deref() == Some("Tests"))
        .unwrap()
        .pane
        .terminal_id;
    let mut client = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        selected: Some(selected),
        ..
    } = local_hello(&mut client, interactive_mode(None))
        .await
        .unwrap()
    else {
        unreachable!()
    };
    let selected = selected.focused;
    assert_eq!(
        selected.terminal_id, focused,
        "startup must select the most recently active pane"
    );
    assert_ne!(selected.child_pid, old_pid);
    harness.detach(&mut client).await;
    drop(client);
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_restore_missing_directory_and_program_keep_intended_command_for_next_restart() {
    let mut harness = Harness::start_with("exec /bin/sleep 600", |root| {
        fs::create_dir(root.join("cwd/vanishing")).unwrap();
        fs::create_dir(root.join("copy-bin")).unwrap();
        fs::copy("/bin/sleep", root.join("copy-bin/sleep")).unwrap();
        fs::set_permissions(
            root.join("copy-bin/sleep"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    })
    .await;
    let workspace = harness.resources().await.sessions[0].workspaces[0].id;
    let executable = harness.root.path().join("copy-bin/sleep");
    let cwd = harness.root.path().join("cwd/vanishing");
    let result = harness
        .control_command(ClientMessage::CreateTab {
            workspace_id: workspace,
            name: Some("Recover".into()),
            cwd: Some(cwd.clone()),
            program: Some(executable.clone()),
            argv: vec!["601".into()],
        })
        .await;
    assert!(matches!(result, ServerMessage::TabCreated { .. }));
    wait_for_cwds(&harness).await;
    save(&harness).await;
    approve(&harness).await;
    action(&harness, LayoutAction::Enable).await;
    stop(&mut harness).await;
    fs::rename(&cwd, cwd.with_file_name("returned-later")).unwrap();
    fs::rename(&executable, executable.with_file_name("returned-program")).unwrap();
    harness.restart("exit 77").await;
    let restored = wait_for_cwds(&harness).await;
    assert_eq!(
        restored.sessions[0].workspaces[0].tabs.len(),
        2,
        "{}\n{restored:#?}",
        harness.logs()
    );
    assert_eq!(
        restored.sessions[0].workspaces[0].tabs[1].panes[0].cwd,
        Some(harness.root.path().join("home"))
    );
    assert!(harness.logs().contains("missing directory"));
    save(&harness).await;
    let review = action(&harness, LayoutAction::Review).await;
    let missing = review
        .commands
        .iter()
        .find(|c| c.command.program == executable)
        .unwrap();
    assert_eq!(missing.cwd, cwd);
    assert!(missing.approved);
    stop(&mut harness).await;
    fs::rename(cwd.with_file_name("returned-later"), &cwd).unwrap();
    harness.restart("exit 77").await;
    let shell = wait_for_cwds(&harness).await;
    assert_eq!(
        shell.sessions[0].workspaces[0].tabs[1].panes[0].cwd,
        Some(cwd.clone())
    );
    assert!(harness.logs().contains("executable is missing"));
    save(&harness).await;
    assert!(
        action(&harness, LayoutAction::Review)
            .await
            .commands
            .iter()
            .any(|command| command.command.program == executable && command.approved)
    );
    stop(&mut harness).await;
    fs::rename(executable.with_file_name("returned-program"), &executable).unwrap();
    harness.restart("exit 77").await;
    let recovered = wait_for_cwds(&harness).await;
    assert_eq!(
        recovered.sessions[0].workspaces[0].tabs[1].panes[0].cwd,
        Some(cwd)
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_restore_returning_programs_keep_their_shell_after_the_program_exits() {
    let mut harness = Harness::start_with("exec /bin/sleep 600", |root| {
        fs::create_dir(root.join("cwd/.fut")).unwrap();
        fs::write(root.join("cwd/.fut/project.toml"), r#"
[[workspaces]]
id = "main"
[[workspaces.tabs]]
id = "code"
panes = [
  { id = "shell" },
  { id = "program", title = "Returning", command = ["/bin/sleep", "601"], split = { target = "shell", direction = "right" } },
]
"#).unwrap();
        project_trust_cli(root, "trust", &root.join("cwd"));
    }).await;
    wait_for_cwds(&harness).await;
    action(&harness, LayoutAction::Enable).await;
    let approved = approve(&harness).await;
    let checkpoint: Value =
        serde_json::from_str(&fs::read_to_string(&approved.path).unwrap()).unwrap();
    assert_eq!(
        checkpoint["projects"][0]["workspaces"][0]["tabs"][0]["panes"][1]["return_to_shell"],
        true
    );
    stop(&mut harness).await;
    harness.restart("exec /bin/sleep 600").await;
    let snapshot = wait_for_cwds(&harness).await;
    let terminal = snapshot.sessions[0].workspaces[0].tabs[0].panes[1].terminal_id;
    let (_connection, _, parent) = harness
        .interactive_for(Some(TargetSelector::Terminal(terminal)))
        .await;
    let child = time::timeout(DEADLINE, async {
        loop {
            let output = Command::new("/bin/ps")
                .args(["-axo", "pid=,ppid="])
                .output()
                .unwrap();
            let child = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .find_map(|line| {
                    let mut fields = line.split_whitespace();
                    let pid: u32 = fields.next()?.parse().ok()?;
                    let ppid: u32 = fields.next()?.parse().ok()?;
                    (ppid == parent).then_some(pid)
                });
            if let Some(child) = child {
                break child;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap();
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &child.to_string()])
            .status()
            .unwrap()
            .success()
    );
    time::timeout(DEADLINE, async {
        loop {
            let saved = save(&harness).await;
            if !saved
                .commands
                .iter()
                .any(|item| item.command.args == ["601"])
            {
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        harness.resources().await.sessions[0].workspaces[0].tabs[0]
            .panes
            .len(),
        2
    );
    let (_connection, _, shell) = harness
        .interactive_for(Some(TargetSelector::Terminal(terminal)))
        .await;
    assert_eq!(
        shell, parent,
        "the returning wrapper should exec the shell in place"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_restore_unapproved_commands_are_shells_and_intentional_closure_is_not_restored() {
    let mut harness = Harness::start("exec /bin/sleep 600").await;
    let before = harness.resources().await;
    let pane = before.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let response = harness
        .control_command(ClientMessage::SplitPane {
            pane_id: pane,
            direction: SplitDirection::Right,
            cwd: None,
            program: Some("/bin/sleep".into()),
            argv: vec!["601".into()],
        })
        .await;
    let ServerMessage::PaneCreated { selected } = response else {
        panic!("{response:?}")
    };
    wait_for_cwds(&harness).await;
    save(&harness).await;
    action(&harness, LayoutAction::Enable).await;
    assert!(matches!(
        harness
            .control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Pane(selected.pane_id)
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    stop(&mut harness).await;
    harness.restart("exit 77").await;
    let restored = wait_for_cwds(&harness).await;
    assert_eq!(restored.pane_paths().count(), 1);
    assert!(harness.logs().contains("needs `fut layout review`"));
    save(&harness).await;
    let review = action(&harness, LayoutAction::Review).await;
    assert_eq!(review.commands.len(), 1);
    assert!(!review.commands[0].approved);
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_restore_explicit_target_does_not_autorestore_other_projects_at_startup() {
    let mut harness = Harness::start_with("exec /bin/sleep 600", recipe_setup).await;
    let root = harness.resources().await.sessions[0].name.clone();
    let other = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
            name: Some("Other project".into()),
            parent_workspace_id: None,
            cwd: harness.root.path().join("other"),
            program: Some("/bin/sleep".into()),
            argv: vec!["604".into()],
        })
        .await;
    assert!(matches!(other, ServerMessage::LocationOpened { .. }));
    wait_for_cwds(&harness).await;
    action(&harness, LayoutAction::Enable).await;
    approve(&harness).await;
    stop(&mut harness).await;
    restart_empty(&mut harness).await;
    let output = tokio::process::Command::from({
        let mut cmd = harness.cli();
        cmd.args(["--json", "layout", "restore", &root, "--background"]);
        cmd
    })
    .output()
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot = harness.resources().await;
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].name, root);
    assert_eq!(
        save(&harness).await.projects.len(),
        2,
        "a dormant project's checkpoint must survive a partial restore"
    );
    let response = harness
        .control_command(ClientMessage::RestoreLayout { project: None })
        .await;
    let ServerMessage::LayoutRestored { report } = response else {
        panic!("{response:?}")
    };
    assert_eq!(report.restored, ["Other project"]);
    assert_eq!(report.already_live, [root]);
    assert_eq!(harness.resources().await.sessions.len(), 2);
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_restore_uses_current_trusted_settings_and_original_recipe_root_without_bootstrap() {
    let mut harness = Harness::start_with("exec /bin/sleep 600", |root| {
        recipe_setup(root);
        fs::create_dir(root.join("nested/sub")).unwrap();
        let path = root.join("cwd/.fut/project.toml");
        let source = fs::read_to_string(&path)
            .unwrap()
            .replace(
                "title = \"Primary\"",
                &format!(
                    "title = \"Primary\"\ncwd = {:?}",
                    root.join("nested").to_str().unwrap()
                ),
            )
            .replace(
                "command = [\"/bin/sleep\", \"601\"]\nexec = true",
                "environment = { RESTORE_PANE = \"original\" }",
            );
        fs::write(path, source).unwrap();
        project_trust_cli(root, "trust", &root.join("cwd"));
    })
    .await;
    wait_for_cwds(&harness).await;
    let saved = save(&harness).await;
    let checkpoint: Value =
        serde_json::from_str(&fs::read_to_string(&saved.path).unwrap()).unwrap();
    assert_eq!(
        checkpoint["projects"][0]["source_root"],
        json!(harness.root.path().join("cwd"))
    );
    approve(&harness).await;
    stop(&mut harness).await;
    let source = harness.root.path().join("cwd/.fut/project.toml");
    let marker = harness.root.path().join("bootstrap-must-not-run");
    let updated = fs::read_to_string(&source)
        .unwrap()
        .replace("RESTORE_ROOT = \"current\"", "RESTORE_ROOT = \"new\"")
        .replace("RESTORE_TAB = \"current\"", "RESTORE_TAB = \"new\"")
        .replace("RESTORE_PANE = \"original\"", "RESTORE_PANE = \"new\"")
        .replace(
            "command = [\"/bin/sleep\", \"600\"]",
            &format!(
                "command = [\"/usr/bin/touch\", {:?}]",
                marker.to_str().unwrap()
            ),
        );
    fs::write(source, updated).unwrap();
    for (trusted, expected) in [
        (true, "SETTINGS=new/new/new"),
        (false, "SETTINGS=unset/unset/unset"),
    ] {
        project_trust_cli(
            harness.root.path(),
            if trusted { "trust" } else { "untrust" },
            &harness.root.path().join("cwd"),
        );
        restart_empty(&mut harness).await;
        let response = harness
            .control_command(ClientMessage::RestoreLayout { project: None })
            .await;
        let ServerMessage::LayoutRestored { report } = response else {
            panic!("{response:?}")
        };
        assert_eq!(report.restored, ["cwd"]);
        if trusted {
            assert!(report.warnings.is_empty(), "{report:?}");
        } else {
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("shell-only recovery")),
                "{report:?}"
            );
        }
        let snapshot = harness.resources().await;
        let terminal = snapshot.sessions[0].workspaces[0].tabs[0].panes[1].terminal_id;
        let (mut connection, _, _) = harness
            .interactive_for(Some(TargetSelector::Terminal(terminal)))
            .await;
        send(&mut connection, ClientMessage::Input { bytes: b"printf 'SETTINGS=%s/%s/%s\\nIDENTITY=%s\\n' \"${RESTORE_ROOT-unset}\" \"${RESTORE_TAB-unset}\" \"${RESTORE_PANE-unset}\" \"$FUT_TERMINAL_ID\"\n".to_vec() }).await;
        let screen =
            snapshot_containing(&mut connection, terminal, &format!("IDENTITY={terminal}")).await;
        assert!(
            snapshot_text(&screen).contains(expected),
            "{}",
            snapshot_text(&screen)
        );
        assert!(
            !marker.exists(),
            "restoration must not execute new recipe commands"
        );
        harness.detach(&mut connection).await;
        drop(connection);
        stop(&mut harness).await;
    }
}

#[tokio::test]
async fn layout_restore_does_not_resurrect_a_dormant_project_reopened_and_closed_before_checkpoint()
{
    let mut harness = Harness::start("exec /bin/sleep 600").await;
    save(&harness).await;
    stop(&mut harness).await;
    restart_empty(&mut harness).await;
    let enabled = action(&harness, LayoutAction::Enable).await;
    assert_eq!(enabled.projects.len(), 1);
    let response = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
            name: None,
            parent_workspace_id: None,
            cwd: harness.root.path().join("cwd"),
            program: Some("/bin/sleep".into()),
            argv: vec!["600".into()],
        })
        .await;
    let ServerMessage::LocationOpened { selected, .. } = response else {
        panic!("{response:?}")
    };
    harness
        .close_session(SessionSelector::Id(selected.session_id))
        .await;
    harness.wait_until_exited().await;
    let saved: Value = serde_json::from_str(&fs::read_to_string(&enabled.path).unwrap()).unwrap();
    assert_eq!(saved["projects"], json!([]));
}

#[tokio::test]
async fn layout_restore_periodic_capture_is_opt_in_and_disable_keeps_the_last_checkpoint() {
    let mut harness = Harness::start("exec /bin/sleep 600").await;
    let enabled = action(&harness, LayoutAction::Enable).await;
    let pane = harness.resources().await.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let response = harness
        .control_command(ClientMessage::SplitPane {
            pane_id: pane,
            direction: SplitDirection::Right,
            cwd: None,
            program: Some("/bin/sleep".into()),
            argv: vec!["601".into()],
        })
        .await;
    assert!(matches!(response, ServerMessage::PaneCreated { .. }));
    time::timeout(DEADLINE, async {
        loop {
            let saved: Value =
                serde_json::from_str(&fs::read_to_string(&enabled.path).unwrap()).unwrap();
            if saved["projects"][0]["workspaces"][0]["tabs"][0]["panes"]
                .as_array()
                .unwrap()
                .len()
                == 2
            {
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap();
    action(&harness, LayoutAction::Disable).await;
    let saved = fs::read(&enabled.path).unwrap();
    harness
        .control_command(ClientMessage::RenameTarget {
            selector: RenameSelector::Pane(pane),
            name: "Not automatically saved".into(),
        })
        .await;
    stop(&mut harness).await;
    assert_eq!(fs::read(&enabled.path).unwrap(), saved);
}

#[tokio::test]
async fn layout_restore_recovers_the_last_periodic_checkpoint_after_an_abrupt_daemon_exit() {
    let mut harness = Harness::start("exec /bin/sleep 600").await;
    let enabled = action(&harness, LayoutAction::Enable).await;
    let pane = harness.resources().await.sessions[0].workspaces[0].tabs[0].panes[0].id;
    harness
        .control_command(ClientMessage::SplitPane {
            pane_id: pane,
            direction: SplitDirection::Down,
            cwd: None,
            program: Some("/bin/sleep".into()),
            argv: vec!["601".into()],
        })
        .await;
    time::timeout(DEADLINE, async {
        loop {
            let saved: Value =
                serde_json::from_str(&fs::read_to_string(&enabled.path).unwrap()).unwrap();
            if saved["projects"][0]["workspaces"][0]["tabs"][0]["panes"]
                .as_array()
                .unwrap()
                .len()
                == 2
            {
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap();
    approve(&harness).await;
    let before = wait_for_cwds(&harness).await;
    let saved = fs::read(&enabled.path).unwrap();
    harness.daemon.kill().unwrap();
    harness.daemon.wait().unwrap();
    assert_eq!(fs::read(&enabled.path).unwrap(), saved);
    harness.restart("exec /bin/sleep 600").await;
    let after = wait_for_cwds(&harness).await;
    assert_eq!(topology(&after), topology(&before));
    assert_ne!(after.sessions[0].id, before.sessions[0].id);
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_restore_export_is_reviewable_and_never_overwrites_files() {
    let mut harness = Harness::start_with("exec /bin/sleep 600", recipe_setup).await;
    let pane = harness.resources().await.sessions[0].workspaces[0].tabs[0].panes[0].id;
    harness
        .control_command(ClientMessage::SplitPane {
            pane_id: pane,
            direction: SplitDirection::Down,
            cwd: Some(harness.root.path().join("cwd/sub")),
            program: Some("/bin/sleep".into()),
            argv: vec!["603".into()],
        })
        .await;
    let before = wait_for_cwds(&harness).await;
    let saved = save(&harness).await;
    let name = &saved.projects[0].name;
    let destination = harness.root.path().join("export.toml");
    let exported = harness
        .cli()
        .args(["layout", "export", name, "--output"])
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let content = fs::read_to_string(&destination).unwrap();
    assert!(content.contains("numerator = 2"));
    assert!(content.contains("parent = \"w0\""));
    assert!(content.contains("title = \"Editor\""));
    let again = harness
        .cli()
        .args(["layout", "export", name, "--output"])
        .arg(&destination)
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert_eq!(fs::read_to_string(&destination).unwrap(), content);
    stop(&mut harness).await;
    fs::write(harness.root.path().join("cwd/.fut/project.toml"), content).unwrap();
    project_trust_cli(
        harness.root.path(),
        "trust",
        &harness.root.path().join("cwd"),
    );
    harness.restart("exec /bin/sleep 600").await;
    let after = wait_for_cwds(&harness).await;
    assert_eq!(topology(&after), topology(&before));
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_restore_old_daemon_save_adapter_uses_only_read_only_frozen_methods() {
    let harness = Harness::start("exec /bin/sleep 600").await;
    let snapshot = wait_for_cwds(&harness).await;
    let proxy = harness.root.path().join("baseline.sock");
    let listener = tokio::net::UnixListener::bind(&proxy).unwrap();
    let upstream = harness.socket.clone();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut client = Framed::new(stream, codec());
        let mut daemon = Framed::new(UnixStream::connect(upstream).await.unwrap(), codec());
        let frame = client.next().await.unwrap().unwrap();
        let mut envelope: Envelope<ClientMessage> = decode_payload(&frame).unwrap();
        let ClientMessage::LocalHello(hello) = &mut envelope.message else {
            unreachable!()
        };
        hello
            .optional
            .retain(|name| name != "layout-restoration.v1");
        daemon
            .send(encode_payload(&envelope).unwrap().into())
            .await
            .unwrap();
        client
            .send(daemon.next().await.unwrap().unwrap().freeze())
            .await
            .unwrap();
        let mut methods = Vec::new();
        while let Some(frame) = client.next().await {
            let frame = frame.unwrap();
            let envelope: Envelope<ClientMessage> = decode_payload(&frame).unwrap();
            assert!(matches!(
                envelope.message,
                ClientMessage::ListResources | ClientMessage::Ping
            ));
            methods.push(envelope.message);
            daemon.send(frame.freeze()).await.unwrap();
            client
                .send(daemon.next().await.unwrap().unwrap().freeze())
                .await
                .unwrap();
        }
        methods
    });
    let template = harness.cli();
    let mut command = Command::new(template.get_program());
    command.env_clear().envs(
        template
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key, value))),
    );
    command
        .arg("--socket")
        .arg(&proxy)
        .args(["--json", "layout", "save"]);
    let result = tokio::process::Command::from(command)
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result: Value = serde_json::from_slice(&result.stdout).unwrap();
    let disk: Value = serde_json::from_str(
        &fs::read_to_string(result["result"]["path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let pane = &disk["projects"][0]["workspaces"][0]["tabs"][0]["panes"][0];
    assert_eq!(pane["command"]["program"], "/bin/sleep");
    assert_eq!(pane["command"]["args"], json!(["600"]));
    assert_eq!(pane["cwd"], json!(harness.root.path().join("cwd")));
    assert_eq!(task.await.unwrap().len(), 3);
    assert!(process_alive(harness.daemon.id()));
    assert_eq!(topology(&harness.resources().await), topology(&snapshot));
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_restore_unwraps_only_its_own_recipe_shell_not_arbitrary_pipelines() {
    let harness = Harness::start_with("cd sub; /bin/sleep 600 | /bin/sleep 601", |root| {
        fs::create_dir(root.join("cwd/.fut")).unwrap();
        fs::create_dir(root.join("cwd/sub")).unwrap();
        fs::write(
            root.join("cwd/.fut/project.toml"),
            r#"
[[workspaces]]
[[workspaces.tabs]]
[[workspaces.tabs.panes]]
id = "first"
command = ["/bin/sleep", "600"]
[[workspaces.tabs.panes]]
command = ["/bin/sleep", "602"]
split = { target = "first", direction = "right" }
"#,
        )
        .unwrap();
        project_trust_cli(root, "trust", &root.join("cwd"));
    })
    .await;
    wait_for_cwds(&harness).await;
    let reply = save(&harness).await;
    let disk: Value = serde_json::from_str(&fs::read_to_string(reply.path).unwrap()).unwrap();
    let panes = disk["projects"][0]["workspaces"][0]["tabs"][0]["panes"]
        .as_array()
        .unwrap();
    assert!(panes[0]["command"].is_null());
    assert_eq!(panes[0]["cwd"], json!(harness.root.path().join("cwd/sub")));
    assert!(panes[0]["warning"].is_string());
    assert_eq!(panes[1]["command"]["program"], "/bin/sleep");
    assert_eq!(panes[1]["command"]["args"], json!(["602"]));
    harness.shutdown().await;
}

#[tokio::test]
async fn layout_restore_capability_does_not_expand_frozen_or_remote_private_methods() {
    let harness = Harness::start("exec /bin/sleep 600").await;
    for remote in [false, true] {
        let mut connection = harness.connect().await.unwrap();
        let message = if remote {
            let mut hello =
                fut::protocol::remote::RemoteHello::new(ClientMode::Control, "old-client");
            hello
                .optional
                .retain(|name| name != "layout-restoration.v1");
            ClientMessage::RemoteHello(hello)
        } else {
            let mut hello =
                fut::protocol::local::LocalHello::new(ClientMode::Control, "old-client");
            hello
                .optional
                .retain(|name| name != "layout-restoration.v1");
            ClientMessage::LocalHello(hello)
        };
        send(&mut connection, message).await;
        receive(&mut connection).await.unwrap();
        for method in [
            ClientMessage::SaveLayout,
            ClientMessage::RestoreLayout { project: None },
        ] {
            send(&mut connection, method).await;
            assert!(matches!(
                receive(&mut connection).await.unwrap(),
                ServerMessage::EndpointError {
                    error: fut::protocol::remote::EndpointError::MethodNotNegotiated
                }
            ));
        }
    }
    let mut connection = harness.connect().await.unwrap();
    send(
        &mut connection,
        ClientMessage::RemoteHello(fut::protocol::remote::RemoteHello::new(
            ClientMode::Control,
            "new-client",
        )),
    )
    .await;
    receive(&mut connection).await.unwrap();
    send(&mut connection, ClientMessage::ListLayouts).await;
    assert!(matches!(
        receive(&mut connection).await.unwrap(),
        ServerMessage::SavedLayouts { .. }
    ));
    for method in [
        ClientMessage::Layout {
            action: LayoutAction::Enable,
        },
        ClientMessage::Layout {
            action: LayoutAction::Approve {
                fingerprints: Vec::new(),
            },
        },
        ClientMessage::Shutdown,
    ] {
        send(&mut connection, method).await;
        assert!(matches!(
            receive(&mut connection).await.unwrap(),
            ServerMessage::EndpointError {
                error: fut::protocol::remote::EndpointError::MethodNotNegotiated
            }
        ));
    }
    drop(connection);
    harness.shutdown().await;
}
