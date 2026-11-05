use super::*;

// Opt-in: FUT_TEST_INCUS_{INSTANCE,PROJECT}, FUT_TEST_SANDBOXSH_{BIN,CONFIG}.
// The sandbox must already be running; probes use no network, extensions, or saved sessions.

const GUEST_DEADLINE: Duration = Duration::from_secs(60);

struct Guest {
    instance: String,
    project: String,
    marker: String,
}

impl Guest {
    fn new() -> Self {
        Self {
            instance: std::env::var("FUT_TEST_INCUS_INSTANCE")
                .expect("set FUT_TEST_INCUS_INSTANCE to an already-running test sandbox"),
            project: std::env::var("FUT_TEST_INCUS_PROJECT")
                .expect("set FUT_TEST_INCUS_PROJECT to the restricted Incus project"),
            marker: format!("fut-restore-test-{}", uuid::Uuid::new_v4()),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("/usr/bin/incus");
        command.args([
            "--force-local",
            "--project",
            &self.project,
            "exec",
            &self.instance,
            "--",
        ]);
        command
    }

    fn pi_args(&self) -> Vec<String> {
        [
            "/home/dev/.local/bin/pi",
            "--offline",
            "--verbose",
            "--no-session",
            "--no-extensions",
            "--no-mcp",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-approve",
            "--name",
            &self.marker,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn pi_pids(&self) -> std::io::Result<Vec<u32>> {
        let output = self
            .command()
            .args(["/usr/bin/pgrep", "-f", &self.marker])
            .output()?;
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(std::io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(std::io::Error::other)?
            .split_whitespace()
            .map(|pid| pid.parse().map_err(std::io::Error::other))
            .collect()
    }

    async fn wait_for_pi(&self) -> u32 {
        time::timeout(GUEST_DEADLINE, async {
            loop {
                let pids = self.pi_pids().expect("inspect this test's guest Pi");
                if let [pid] = pids.as_slice() {
                    return *pid;
                }
                time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("a fresh guest Pi process did not start")
    }

    fn stop_pi(&self) {
        let Ok(pids) = self.pi_pids() else {
            return;
        };
        for pid in pids {
            let _ = self
                .command()
                .args(["/bin/kill", "-TERM", "--", &pid.to_string()])
                .output();
        }
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.stop_pi();
    }
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn wait_for_pi_ui(harness: &mut Harness, marker: &str) {
    let (mut connection, terminal, _) = harness.interactive().await;
    let mut last_screen = String::new();
    let ready = time::timeout(GUEST_DEADLINE, async {
        loop {
            let frame = connection
                .next()
                .await
                .expect("Pi terminal disconnected")
                .unwrap();
            let Some(envelope) = connection.assemble(&frame) else {
                continue;
            };
            if let ServerMessage::Snapshot {
                terminal_id,
                screen,
            } = envelope.message
                && terminal_id == terminal
            {
                last_screen = snapshot_text(&screen);
                // Pi truncates long session names in an 80-column footer.
                if last_screen.contains(&marker[..24]) && last_screen.contains("/ for commands") {
                    return;
                }
            }
        }
    })
    .await;
    assert!(
        ready.is_ok(),
        "Pi's interactive UI did not start: {last_screen}"
    );
    harness.detach(&mut connection).await;
}

#[derive(Clone, Copy)]
enum Launch {
    Incus,
    SandboxshExec,
    SandboxshShell,
}

async fn verify_restore(launch: Launch) {
    let guest = Guest::new();
    let mut command = match launch {
        Launch::Incus => {
            let command = guest.command();
            let mut args = vec!["/usr/bin/incus".to_owned()];
            args.extend(
                command
                    .get_args()
                    .map(|arg| arg.to_str().unwrap().to_owned()),
            );
            assert_eq!(args.pop().as_deref(), Some("--"));
            args.extend(
                [
                    "--cwd",
                    "/workspaces/fut",
                    "--",
                    "runuser",
                    "-u",
                    "dev",
                    "--",
                ]
                .map(str::to_owned),
            );
            args.extend(guest.pi_args());
            args
        }
        Launch::SandboxshExec | Launch::SandboxshShell => vec![
            std::env::var("FUT_TEST_SANDBOXSH_BIN")
                .expect("set FUT_TEST_SANDBOXSH_BIN to the installed sandboxsh executable"),
            "--config".into(),
            std::env::var("FUT_TEST_SANDBOXSH_CONFIG")
                .expect("set FUT_TEST_SANDBOXSH_CONFIG to an approved existing sandbox config"),
        ],
    };
    if !matches!(launch, Launch::Incus) {
        let output = Command::new(&command[0])
            .args(&command[1..])
            .arg("plan")
            .output()
            .expect("resolve the sandboxsh test configuration");
        assert!(output.status.success(), "sandboxsh plan failed");
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["instance"].as_str(), Some(guest.instance.as_str()));
    }
    match launch {
        Launch::SandboxshExec => {
            command.extend(["exec", "--"].map(str::to_owned));
            command.extend(guest.pi_args());
        }
        Launch::SandboxshShell => command.push("shell".into()),
        Launch::Incus => {}
    }
    let mut harness = Harness::start(&format!("exec {}", shell_join(&command))).await;
    if matches!(launch, Launch::SandboxshShell) {
        let (mut connection, _, _) = harness.interactive().await;
        send(
            &mut connection,
            ClientMessage::Input {
                bytes: format!("{}\r", shell_join(&guest.pi_args())).into_bytes(),
            },
        )
        .await;
        harness.detach(&mut connection).await;
    }
    let original_pid = guest.wait_for_pi().await;
    wait_for_pi_ui(&mut harness, &guest.marker).await;
    save(&harness).await;
    let review = action(&harness, LayoutAction::Review).await;
    approve(&harness).await;
    assert_eq!(review.commands.len(), 1, "{review:?}");
    let saved = &review.commands[0].command;
    assert_eq!(saved.program.file_name().unwrap(), "incus");
    let starts_pi = !matches!(launch, Launch::SandboxshShell);
    assert_eq!(saved.args.contains(&guest.marker), starts_pi);
    assert_eq!(saved.args.contains(&"--no-session".to_owned()), starts_pi);
    stop(&mut harness).await;
    // Incus can leave a disconnected guest command alive; terminate only our UUID-tagged Pi.
    guest.stop_pi();
    restart_empty(&mut harness).await;
    let response = harness
        .control_command(ClientMessage::RestoreLayout { project: None })
        .await;
    let ServerMessage::LayoutRestored { report } = response else {
        panic!("{response:?}")
    };
    assert!(report.warnings.is_empty(), "{report:?}");
    assert_eq!(report.restored.len(), 1, "{report:?}");
    if starts_pi {
        let restarted_pid = guest.wait_for_pi().await;
        wait_for_pi_ui(&mut harness, &guest.marker).await;
        assert_ne!(original_pid, restarted_pid);
        eprintln!(
            "fresh Pi restarted in {}: {original_pid} -> {restarted_pid}",
            guest.instance
        );
    } else {
        time::sleep(Duration::from_secs(2)).await;
        assert!(
            guest.pi_pids().unwrap().is_empty(),
            "shell-only restart unexpectedly launched Pi"
        );
        eprintln!("sandboxsh shell restored without restarting the Pi launched inside it");
    }
    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires an explicitly selected running Incus sandbox with Pi installed"]
async fn layout_restore_incus_direct_pi() {
    verify_restore(Launch::Incus).await;
}

#[tokio::test]
#[ignore = "requires an explicitly selected running sandboxsh project with Pi installed"]
async fn layout_restore_sandboxsh_exec_pi() {
    verify_restore(Launch::SandboxshExec).await;
}

#[tokio::test]
#[ignore = "requires an explicitly selected running sandboxsh project with Pi installed"]
async fn layout_restore_sandboxsh_shell_does_not_replay_guest_shell_history() {
    verify_restore(Launch::SandboxshShell).await;
}
