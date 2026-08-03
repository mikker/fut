#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use fut::{
    domain::{ScreenSnapshot, TerminalId, TerminalSize},
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, SelectedTarget, ServerMessage,
        codec, decode_payload, encode_payload,
    },
    resources::{SessionSelector, TargetSelector},
};
use futures_util::{SinkExt, StreamExt};
use tempfile::TempDir;
use tokio::{net::UnixStream, time};
use tokio_util::codec::Framed;
use uuid::Uuid;

const DEADLINE: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const SIZE: TerminalSize = TerminalSize {
    columns: 80,
    rows: 24,
};

type Connection = Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>;

struct Harness {
    root: TempDir,
    socket: PathBuf,
    daemon: Child,
    terminal_pid: Option<u32>,
}

struct PtyChild {
    child: Child,
    input: Option<std::process::ChildStdin>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl PtyChild {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn PTY child");
        let input = child.stdin.take();
        let output = Arc::new(Mutex::new(Vec::new()));
        let readers: Vec<Box<dyn Read + Send>> = vec![
            Box::new(child.stdout.take().unwrap()),
            Box::new(child.stderr.take().unwrap()),
        ];
        for mut reader in readers {
            let output = Arc::clone(&output);
            std::thread::spawn(move || {
                let mut bytes = [0; 4096];
                while let Ok(count) = reader.read(&mut bytes) {
                    if count == 0 {
                        break;
                    }
                    output.lock().unwrap().extend_from_slice(&bytes[..count]);
                }
            });
        }
        Self {
            child,
            input,
            output,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.input
            .as_mut()
            .expect("PTY stdin is open")
            .write_all(bytes)
            .expect("write PTY input");
    }

    async fn wait_for(&mut self, needle: &str) {
        time::timeout(DEADLINE, async {
            loop {
                let text = self.text();
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "PTY child exited before {needle:?}; output={text:?}"
                );
                if text.contains(needle) {
                    return;
                }
                time::sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("PTY output never contained {needle:?}: {:?}", self.text()));
    }

    async fn wait_for_count(&mut self, needle: &str, count: usize) {
        time::timeout(DEADLINE, async {
            loop {
                let text = self.text();
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "PTY child exited before {count} occurrences of {needle:?}; output={text:?}"
                );
                if text.matches(needle).count() >= count {
                    return;
                }
                time::sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "PTY output never contained {count} occurrences of {needle:?}: {:?}",
                self.text()
            )
        });
    }

    async fn wait_success(&mut self) {
        let status = time::timeout(DEADLINE, async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                time::sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .expect("PTY child did not exit");
        assert!(status.success(), "PTY child failed: {:?}", self.text());
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Harness {
    async fn start(script: &str) -> Self {
        Self::start_with(script, |_| {}).await
    }

    async fn start_with(script: &str, setup: impl FnOnce(&std::path::Path)) -> Self {
        let root = tempfile::Builder::new()
            .prefix("fut-e2e-")
            .tempdir()
            .expect("create isolated e2e directory");
        let runtime = root.path().join("runtime");
        let home = root.path().join("home");
        let cwd = root.path().join("cwd");
        fs::create_dir_all(&runtime).expect("create runtime directory");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))
            .expect("secure existing runtime directory");
        fs::create_dir_all(&home).expect("create home directory");
        fs::create_dir_all(&cwd).expect("create working directory");
        setup(root.path());
        let socket = runtime.join("fut.sock");
        let daemon = spawn_daemon(&root, &socket, &home, &runtime, &cwd, script);

        let mut harness = Self {
            root,
            socket,
            daemon,
            terminal_pid: None,
        };
        harness.wait_until_ready().await;
        harness
    }

    async fn restart(&mut self, script: &str) {
        assert!(
            self.daemon
                .try_wait()
                .expect("query daemon status")
                .is_some(),
            "cannot restart while daemon is running"
        );
        self.daemon = spawn_daemon(
            &self.root,
            &self.socket,
            &self.root.path().join("home"),
            &self.root.path().join("runtime"),
            &self.root.path().join("cwd"),
            script,
        );
        self.wait_until_ready().await;
    }

    async fn wait_until_exited(&mut self) {
        wait_for(DEADLINE, || {
            self.daemon
                .try_wait()
                .expect("query daemon status")
                .is_some()
        })
        .await;
        assert!(
            !self.socket.exists(),
            "socket pathname remained after daemon exit: {}",
            self.logs()
        );
    }

    async fn wait_until_ready(&mut self) {
        let result = time::timeout(DEADLINE, async {
            loop {
                assert!(
                    self.daemon
                        .try_wait()
                        .expect("query daemon status")
                        .is_none(),
                    "daemon exited during startup: {}",
                    self.logs()
                );
                if let Ok(mut connection) = self.connect().await
                    && hello(&mut connection, ClientMode::Control, PROTOCOL_VERSION)
                        .await
                        .is_ok_and(|message| matches!(message, ServerMessage::Welcome { .. }))
                {
                    return;
                }
                time::sleep(POLL_INTERVAL).await;
            }
        })
        .await;
        assert!(result.is_ok(), "daemon startup timed out: {}", self.logs());
    }

    async fn connect(&self) -> std::io::Result<Connection> {
        UnixStream::connect(&self.socket)
            .await
            .map(|stream| Framed::new(stream, codec()))
    }

    async fn interactive(&mut self) -> (Connection, TerminalId, u32) {
        self.interactive_for(None).await
    }

    async fn interactive_for(
        &mut self,
        selector: Option<TargetSelector>,
    ) -> (Connection, TerminalId, u32) {
        time::timeout(DEADLINE, async {
            loop {
                let mut connection = self.connect().await.expect("connect interactive client");
                let welcome = hello(
                    &mut connection,
                    interactive_mode(selector.clone()),
                    PROTOCOL_VERSION,
                )
                .await
                .expect("receive interactive welcome");
                match welcome {
                    ServerMessage::Welcome {
                        version,
                        selected: Some(selected),
                        ..
                    } => {
                        assert_eq!(version, PROTOCOL_VERSION);
                        self.terminal_pid = Some(selected.child_pid);
                        return (connection, selected.terminal_id, selected.child_pid);
                    }
                    ServerMessage::Error { ref code, .. } if code == "already_attached" => {
                        drop(connection);
                        time::sleep(POLL_INTERVAL).await;
                    }
                    other => panic!("expected welcome, received {other:?}: {}", self.logs()),
                }
            }
        })
        .await
        .expect("attachment lease was not released before deadline")
    }

    async fn control_command(&self, message: ClientMessage) -> ServerMessage {
        let mut connection = self.connect().await.expect("connect control client");
        assert!(matches!(
            hello(&mut connection, ClientMode::Control, PROTOCOL_VERSION)
                .await
                .expect("receive control welcome"),
            ServerMessage::Welcome { .. }
        ));
        send(&mut connection, message).await;
        receive(&mut connection)
            .await
            .expect("receive control response")
    }

    async fn resources(&self) -> fut::resources::ResourceSnapshot {
        let ServerMessage::Resources { snapshot } =
            self.control_command(ClientMessage::ListResources).await
        else {
            panic!("expected resources response")
        };
        snapshot
    }

    async fn close_session(&self, selector: SessionSelector) {
        assert_eq!(
            self.control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Session(selector)
            })
            .await,
            ServerMessage::CommandCompleted {
                command: fut::protocol::AcknowledgedCommand::CloseTarget,
            }
        );
    }

    async fn detach(&self, connection: &mut Connection) {
        send(connection, ClientMessage::Detach).await;
        assert_eq!(
            receive_matching(connection, |message| matches!(
                message,
                ServerMessage::Detached
            ))
            .await,
            ServerMessage::Detached
        );
    }

    async fn shutdown(mut self) {
        assert_eq!(
            self.control_command(ClientMessage::Shutdown).await,
            ServerMessage::CommandCompleted {
                command: fut::protocol::AcknowledgedCommand::Shutdown,
            }
        );
        self.wait_until_exited().await;
    }

    fn logs(&self) -> String {
        let read = |name| {
            fs::read_to_string(self.root.path().join(name))
                .unwrap_or_else(|error| format!("<could not read {name}: {error}>"))
        };
        format!(
            "daemon stdout:\n{}\ndaemon stderr:\n{}",
            read("daemon.stdout"),
            read("daemon.stderr")
        )
    }

    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
        command
            .env_clear()
            .env("HOME", self.root.path().join("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", self.root.path().join("runtime"))
            .env("FUT_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("TERM", "xterm-256color")
            .arg("--socket")
            .arg(&self.socket);
        command
    }
}

fn git(cwd: &std::path::Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=Fut Test",
            "-c",
            "user.email=fut@example.invalid",
        ])
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success());
}

fn spawn_daemon(
    root: &TempDir,
    socket: &PathBuf,
    home: &PathBuf,
    runtime: &PathBuf,
    cwd: &PathBuf,
    script: &str,
) -> Child {
    let stdout =
        fs::File::create(root.path().join("daemon.stdout")).expect("create daemon stdout capture");
    let stderr =
        fs::File::create(root.path().join("daemon.stderr")).expect("create daemon stderr capture");
    Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", runtime)
        .env("FUT_RUNTIME_DIR", runtime)
        .env("TERM", "xterm-256color")
        .arg("--socket")
        .arg(socket)
        .arg("daemon")
        .arg("--foreground")
        .arg("--cwd")
        .arg(cwd)
        .arg("/bin/sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("start real fut daemon binary")
}

impl Drop for Harness {
    fn drop(&mut self) {
        if self.daemon.try_wait().ok().flatten().is_none() {
            let _ = self.daemon.kill();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if self.daemon.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

#[tokio::test]
async fn terminal_survives_detach_and_preserves_output_continuity() {
    let script = r#"
printf 'READY\r\n'
( while [ ! -e gate ]; do sleep 0.02; done; printf 'DURING\r\n'; : > done ) &
while IFS= read -r line; do
  case "$line" in
    before) printf 'BEFORE\r\n' ;;
    after) printf 'AFTER\r\n' ;;
    exit) exit 0 ;;
  esac
done
"#;
    let mut harness = Harness::start(script).await;
    let (mut client_a, terminal_id, child_pid) = harness.interactive().await;

    let ready = snapshot_containing(&mut client_a, terminal_id, "READY").await;
    assert!(snapshot_text(&ready).contains("READY"));
    send(
        &mut client_a,
        ClientMessage::Input {
            bytes: b"before\n".to_vec(),
        },
    )
    .await;
    let before = snapshot_containing(&mut client_a, terminal_id, "BEFORE").await;
    assert_ordered(&snapshot_text(&before), &["READY", "BEFORE"]);

    harness.detach(&mut client_a).await;
    drop(client_a);

    let cwd = harness.root.path().join("cwd");
    fs::write(cwd.join("gate"), b"").expect("open detached output gate");
    wait_for(DEADLINE, || cwd.join("done").exists()).await;

    assert!(process_alive(child_pid), "terminal child died on detach");
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    assert!(
        process_alive(child_pid),
        "terminal child died after control ping"
    );
    let (mut client_b, reattached_id, reattached_pid) = harness.interactive().await;
    assert_eq!(reattached_id, terminal_id, "terminal identity changed");
    assert_eq!(reattached_pid, child_pid, "terminal child PID changed");
    let during = snapshot_containing(&mut client_b, terminal_id, "DURING").await;
    assert_ordered(&snapshot_text(&during), &["READY", "BEFORE", "DURING"]);

    send(
        &mut client_b,
        ClientMessage::Input {
            bytes: b"after\n".to_vec(),
        },
    )
    .await;
    let after = snapshot_containing(&mut client_b, terminal_id, "AFTER").await;
    assert_ordered(
        &snapshot_text(&after),
        &["READY", "BEFORE", "DURING", "AFTER"],
    );
    // Abrupt EOF, without a Detach message, must release the interactive lease.
    drop(client_b);
    let (mut client_c, abrupt_id, abrupt_pid) = harness.interactive().await;
    assert_eq!((abrupt_id, abrupt_pid), (terminal_id, child_pid));
    harness.detach(&mut client_c).await;
    drop(client_c);

    harness
        .close_session(SessionSelector::Name("cwd".into()))
        .await;
    wait_for(DEADLINE, || !process_alive(child_pid)).await;
    harness.wait_until_exited().await;

    // Closing the empty multiplexer releases both the inode and lock so a new
    // daemon can immediately own the canonical socket.
    harness.restart("while IFS= read -r line; do :; done").await;
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    harness.shutdown().await;
}

#[tokio::test]
async fn natural_terminal_exit_stops_daemon_and_removes_socket() {
    let mut harness = Harness::start(
        "while [ ! -e exit-now ]; do sleep 0.02; done; printf 'FINAL_OUTPUT\\r\\n'; exit 7",
    )
    .await;
    let (mut interactive, terminal_id, _) = harness.interactive().await;
    fs::write(harness.root.path().join("cwd/exit-now"), b"").expect("release child exit gate");
    let final_snapshot = snapshot_containing(&mut interactive, terminal_id, "FINAL_OUTPUT").await;
    assert!(snapshot_text(&final_snapshot).contains("FINAL_OUTPUT"));
    assert_eq!(
        receive_matching(&mut interactive, |message| matches!(
            message,
            ServerMessage::TerminalExited {
                terminal_id: id,
                exit_code: Some(7),
            } if *id == terminal_id
        ))
        .await,
        ServerMessage::TerminalExited {
            terminal_id,
            exit_code: Some(7),
        }
    );
    harness.wait_until_exited().await;
}

#[tokio::test]
async fn current_location_open_recovers_from_last_terminal_exit_race() {
    for iteration in 0..10 {
        let mut harness =
            Harness::start("while [ ! -e exit-now ]; do sleep 0.01; done; exit 0").await;
        fs::write(harness.root.path().join("cwd/exit-now"), b"").unwrap();
        // Deterministically exercise the disappeared-socket edge of the race;
        // the same retry path handles a connected `shutting_down` response.
        wait_for(DEADLINE, || harness.daemon.try_wait().unwrap().is_some()).await;

        let shell = harness.root.path().join("ready-shell");
        fs::write(
            &shell,
            "#!/bin/sh\nprintf 'RACE_READY\\r\\n'\nwhile IFS= read -r line; do :; done\n",
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("HOME", harness.root.path().join("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", &shell)
            .env("TMPDIR", harness.root.path().join("runtime"))
            .env("FUT_RUNTIME_DIR", harness.root.path().join("runtime"))
            .env("TERM", "xterm-256color")
            .current_dir(harness.root.path().join("cwd"))
            .args(["-q", "/dev/null", "/bin/sh", "-c"])
            .arg(format!(
                "stty rows 24 cols 80; exec '{}' --socket '{}'",
                env!("CARGO_BIN_EXE_fut"),
                harness.socket.display()
            ));
        let mut open = PtyChild::spawn(command);
        open.wait_for("RACE_READY").await;
        open.send(b"\x02d");
        open.wait_success().await;

        let output = harness.cli().arg("list").output().expect("run public list");
        assert!(
            output.status.success(),
            "iteration {iteration}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let snapshot = String::from_utf8_lossy(&output.stdout);
        assert!(
            snapshot.contains("terminal="),
            "iteration {iteration}: {snapshot}"
        );

        let shutdown = harness.cli().arg("shutdown").output().unwrap();
        assert!(
            shutdown.status.success(),
            "{}",
            String::from_utf8_lossy(&shutdown.stderr)
        );
        wait_for(DEADLINE, || !harness.socket.exists()).await;
    }
}

#[tokio::test]
async fn public_cli_lists_creates_and_closes_resources() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let cwd = harness.root.path().join("second");
    fs::create_dir(&cwd).unwrap();
    let marker = harness.root.path().join("second.started");
    let created = harness
        .cli()
        .args(["new", "second", "--cwd"])
        .arg(&cwd)
        .args(["/bin/sh", "-c"])
        .arg(format!(
            "touch {}; while IFS= read -r line; do :; done",
            marker.display()
        ))
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created = String::from_utf8(created.stdout).unwrap();
    assert!(created.contains("disposition=session_created"));
    let workspace = created
        .split_whitespace()
        .find_map(|field| field.strip_prefix("workspace="))
        .unwrap();
    let terminal_pid: u32 = created
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))
        .unwrap()
        .parse()
        .unwrap();
    wait_for(DEADLINE, || marker.exists()).await;

    let listed = harness.cli().arg("list").output().unwrap();
    assert!(listed.status.success());
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(listed.contains(workspace));
    assert!(listed.contains(cwd.to_str().unwrap()));
    assert!(listed.contains("session "));

    let closed = harness
        .cli()
        .args(["close", &format!("workspace:{workspace}")])
        .output()
        .unwrap();
    assert!(
        closed.status.success(),
        "{}",
        String::from_utf8_lossy(&closed.stderr)
    );
    wait_for(DEADLINE, || !process_alive(terminal_pid)).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_new_tab_creates_an_isolated_ordered_tab_and_close_cascades() {
    let mut harness = Harness::start(
        "printf 'ORIGINAL_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = original ] && printf 'ORIGINAL_INPUT\\r\\n'; done",
    )
    .await;
    let before = harness.resources().await;
    let session_id = before.sessions[0].id;
    let workspace = &before.sessions[0].workspaces[0];
    let workspace_id = workspace.id;
    let original_tab = workspace.tabs[0].id;
    let original_pane = workspace.tabs[0].panes[0].id;
    let original_terminal = workspace.tabs[0].panes[0].terminal_id;
    let (mut original, _, original_pid) = harness
        .interactive_for(Some(TargetSelector::Terminal(original_terminal)))
        .await;
    snapshot_containing(&mut original, original_terminal, "ORIGINAL_READY").await;

    fs::create_dir(harness.root.path().join("cwd/relative-dir")).unwrap();
    let marker = harness.root.path().join("new-tab.started");
    let pwd_file = harness.root.path().join("new-tab.pwd");
    let output = harness
        .cli()
        .args(["new-tab", &workspace_id.to_string(), "--name", "開発 λ", "--cwd", "relative-dir", "/bin/sh", "-c"])
        .arg(format!(
            "touch {}; pwd > {}; printf 'NEW_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = new ] && printf 'NEW_INPUT\\r\\n'; done",
            marker.display(), pwd_file.display()
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    let field = |name: &str| {
        output
            .split_whitespace()
            .find_map(|field| field.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("missing {name} in {output:?}"))
    };
    assert_eq!(field("session"), session_id.to_string());
    assert_eq!(field("workspace"), workspace_id.to_string());
    let tab_id = field("tab").parse().unwrap();
    let pane_id = field("pane").parse().unwrap();
    let terminal_id = field("terminal").parse().unwrap();
    let child_pid: u32 = field("pid").parse().unwrap();
    assert_ne!(tab_id, original_tab);
    assert_ne!(pane_id, original_pane);
    assert_ne!(terminal_id, original_terminal);
    assert_ne!(child_pid, original_pid);
    wait_for(DEADLINE, || marker.exists()).await;
    wait_for(DEADLINE, || pwd_file.exists()).await;
    assert_eq!(
        fs::read_to_string(&pwd_file).unwrap().trim(),
        harness
            .root
            .path()
            .join("cwd/relative-dir")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );

    let after = harness.resources().await;
    let workspace = &after.sessions[0].workspaces[0];
    assert_eq!(workspace.tabs.len(), 2);
    assert_eq!(workspace.tabs[0].id, original_tab);
    assert_eq!(workspace.tabs[1].id, tab_id);
    assert_eq!(workspace.tabs[1].name, "開発 λ");
    assert_eq!(workspace.tabs[1].panes[0].id, pane_id);
    assert_eq!(workspace.tabs[1].panes[0].terminal_id, terminal_id);

    let (mut created, _, _) = harness
        .interactive_for(Some(TargetSelector::Terminal(terminal_id)))
        .await;
    let new_screen = snapshot_containing(&mut created, terminal_id, "NEW_READY").await;
    assert!(!snapshot_text(&new_screen).contains("ORIGINAL_READY"));
    send(
        &mut created,
        ClientMessage::Input {
            bytes: b"new\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut created, terminal_id, "NEW_INPUT").await;
    send(
        &mut original,
        ClientMessage::Input {
            bytes: b"original\n".to_vec(),
        },
    )
    .await;
    let original_screen =
        snapshot_containing(&mut original, original_terminal, "ORIGINAL_INPUT").await;
    assert!(!snapshot_text(&original_screen).contains("NEW_INPUT"));
    assert!(process_alive(original_pid));

    harness.detach(&mut created).await;
    drop(created);
    assert_eq!(
        harness
            .control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Pane(pane_id)
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::CloseTarget
        }
    );
    wait_for(DEADLINE, || !process_alive(child_pid)).await;
    let closed = harness.resources().await;
    assert_eq!(closed.sessions[0].workspaces[0].tabs.len(), 1);
    assert_eq!(closed.sessions[0].workspaces[0].tabs[0].id, original_tab);
    assert!(process_alive(original_pid));
    assert!(harness.daemon.try_wait().unwrap().is_none());
    harness.detach(&mut original).await;
    drop(original);
    harness.shutdown().await;
}

#[tokio::test]
async fn public_new_tab_rejections_are_pre_spawn_and_atomic() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let before = harness.resources().await;
    let workspace_id = before.sessions[0].workspaces[0].id;
    let duplicate = before.sessions[0].workspaces[0].tabs[0].name.clone();

    for (name, cwd, marker_name, expected) in [
        (
            "valid",
            "missing-directory",
            "bad-cwd.marker",
            "invalid_cwd",
        ),
        (&duplicate, ".", "duplicate.marker", "duplicate"),
        (" \t ", ".", "blank.marker", "invalid_name"),
    ] {
        let marker = harness.root.path().join(marker_name);
        let output = harness
            .cli()
            .args([
                "new-tab",
                &workspace_id.to_string(),
                "--name",
                name,
                "--cwd",
                cwd,
                "/bin/sh",
                "-c",
            ])
            .arg(format!("touch {}", marker.display()))
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted rejected tab {name:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(harness.resources().await, before);
        assert!(!marker.exists(), "rejected command for {name:?} ran");
    }

    for parent in ["not-a-uuid", "tab:00000000-0000-0000-0000-000000000000"] {
        let output = harness.cli().args(["new-tab", parent]).output().unwrap();
        assert!(
            !output.status.success(),
            "accepted malformed parent {parent:?}"
        );
        assert!(
            !output.stderr.is_empty(),
            "malformed parent had no parser diagnostic"
        );
        assert_eq!(harness.resources().await, before);
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn interactive_create_tab_correlates_ack_switches_atomically_and_routes_input() {
    let mut harness = Harness::start(
        "printf 'RAW_A_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = old ] && printf 'RAW_A_INPUT\\r\\n'; done",
    )
    .await;
    let resources = harness.resources().await;
    let workspace_id = resources.sessions[0].workspaces[0].id;
    let (mut connection, old_terminal, old_pid) = harness.interactive().await;
    snapshot_containing(&mut connection, old_terminal, "RAW_A_READY").await;

    let request_id = Uuid::new_v4();
    send_envelope(&mut connection, Envelope {
        request_id: Some(request_id),
        message: ClientMessage::CreateTab {
            workspace_id,
            name: Some("raw-v5".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "printf 'RAW_B_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = new ] && printf 'RAW_B_INPUT\\r\\n'; done".into()],
        },
    }).await;
    let created = loop {
        let response = receive_envelope(&mut connection)
            .await
            .expect("create-tab response");
        if response.request_id == Some(request_id) {
            let ServerMessage::TabCreated { selected } = response.message else {
                panic!("expected correlated TabCreated, got {:?}", response.message)
            };
            break selected;
        }
        match response.message {
            ServerMessage::Snapshot { terminal_id, .. } => assert_eq!(terminal_id, old_terminal),
            other => panic!("unexpected frame before TabCreated: {other:?}"),
        }
    };
    assert_eq!(created.workspace_id, workspace_id);
    assert_ne!(created.terminal_id, old_terminal);
    let next = receive_envelope(&mut connection)
        .await
        .expect("initial new-tab snapshot");
    assert_eq!(next.request_id, None);
    match next.message {
        ServerMessage::Snapshot { terminal_id, .. } => assert_eq!(terminal_id, created.terminal_id),
        other => {
            panic!("expected new terminal Snapshot immediately after TabCreated, got {other:?}")
        }
    }

    assert_eq!(
        attach_error_once(&harness, TargetSelector::Terminal(created.terminal_id)).await,
        "already_attached"
    );
    let (mut old, selected_old) =
        attach_once(&harness, TargetSelector::Terminal(old_terminal)).await;
    assert_eq!(selected_old.child_pid, old_pid);
    assert!(process_alive(old_pid));
    send(
        &mut connection,
        ClientMessage::Input {
            bytes: b"new\n".to_vec(),
        },
    )
    .await;
    let new_screen = snapshot_containing(&mut connection, created.terminal_id, "RAW_B_INPUT").await;
    assert!(!snapshot_text(&new_screen).contains("RAW_A_INPUT"));
    send(
        &mut old,
        ClientMessage::Input {
            bytes: b"old\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut old, old_terminal, "RAW_A_INPUT").await;

    harness.detach(&mut old).await;
    harness.detach(&mut connection).await;
    drop(old);
    drop(connection);
    harness.shutdown().await;
}

#[tokio::test]
async fn immediate_exit_interactive_create_tab_never_loses_exit_or_old_attachment() {
    let mut harness = Harness::start(
        "printf 'OLD_READY\\r\\n'; while IFS= read -r line; do printf 'OLD_%s\\r\\n' \"$line\"; done",
    )
    .await;
    let workspace_id = harness.resources().await.sessions[0].workspaces[0].id;
    let (mut connection, old_terminal, _) = harness.interactive().await;
    snapshot_containing(&mut connection, old_terminal, "OLD_READY").await;

    let request_id = Uuid::new_v4();
    send_envelope(
        &mut connection,
        Envelope {
            request_id: Some(request_id),
            message: ClientMessage::CreateTab {
                workspace_id,
                name: Some("immediate-exit".into()),
                cwd: None,
                program: Some("/bin/sh".into()),
                argv: vec!["-c".into(), "printf 'FINAL_SNAPSHOT\\r\\n'; exit 23".into()],
            },
        },
    )
    .await;

    let response = time::timeout(DEADLINE, async {
        loop {
            let response = receive_envelope(&mut connection)
                .await
                .expect("connection ended before create-tab response");
            if response.request_id == Some(request_id) {
                break response.message;
            }
            assert!(matches!(
                response.message,
                ServerMessage::Snapshot { terminal_id, .. } if terminal_id == old_terminal
            ));
        }
    })
    .await
    .expect("immediate-exit CreateTab hung");

    match response {
        ServerMessage::Error { code, .. } => {
            assert_eq!(code, "terminal_exited");
            send(
                &mut connection,
                ClientMessage::Input {
                    bytes: b"STILL_USABLE\n".to_vec(),
                },
            )
            .await;
            snapshot_containing(&mut connection, old_terminal, "OLD_STILL_USABLE").await;
            harness.detach(&mut connection).await;
        }
        ServerMessage::TabCreated { selected } => {
            let terminal_id = selected.terminal_id;
            let mut saw_final_snapshot = false;
            loop {
                let message = time::timeout(DEADLINE, receive(&mut connection))
                    .await
                    .expect("committed immediate-exit tab hung")
                    .expect("connection ended before TerminalExited");
                match message {
                    ServerMessage::Snapshot {
                        terminal_id: id,
                        screen,
                    } => {
                        assert_eq!(id, terminal_id, "stale old snapshot after TabCreated");
                        saw_final_snapshot |= snapshot_text(&screen).contains("FINAL_SNAPSHOT");
                    }
                    ServerMessage::TerminalExited {
                        terminal_id: id,
                        exit_code,
                    } => {
                        assert_eq!(id, terminal_id);
                        assert_eq!(exit_code, Some(23));
                        assert!(saw_final_snapshot, "TerminalExited preceded final snapshot");
                        break;
                    }
                    other => panic!("unexpected frame after TabCreated: {other:?}"),
                }
            }
            assert!(
                time::timeout(DEADLINE, receive(&mut connection))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        other => panic!("expected terminal_exited or TabCreated, got {other:?}"),
    }

    drop(connection);
    harness.shutdown().await;
}

#[tokio::test]
async fn public_cli_rejects_malformed_selectors_before_attachment() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    for (selector, expected) in [
        ("", "target must not be empty"),
        ("workspace:not-a-uuid", "invalid workspace id"),
        ("unknown:value", "unknown target selector prefix"),
        ("session:", "session selector must not be empty"),
    ] {
        let output = harness.cli().args(["attach", selector]).output().unwrap();
        assert!(!output.status.success(), "accepted {selector:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
    // Both documented legacy and typed session forms reach daemon selection.
    for selector in ["missing", "name:missing"] {
        let output = harness.cli().args(["attach", selector]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("not_found"));
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn two_project_sessions_coexist_select_and_close_independently() {
    let script_a = r#"
printf 'A_READY\r\n'
while IFS= read -r line; do
  case "$line" in a) printf 'A_INPUT\r\n' ;; esac
done
"#;
    let mut harness = Harness::start(script_a).await;
    let cwd_b = harness.root.path().join("project-b");
    fs::create_dir(&cwd_b).unwrap();
    let created = harness.control_command(ClientMessage::OpenLocation {
        name: Some("project-b".into()),
        cwd: cwd_b.clone(),
        program: Some("/bin/sh".into()),
        argv: vec!["-c".into(), "printf 'B_READY\\r\\n'; while IFS= read -r line; do case \"$line\" in b) printf 'B_INPUT\\r\\n' ;; esac; done".into()],
    }).await;
    let ServerMessage::LocationOpened {
        selected: selected_b,
        ..
    } = created
    else {
        panic!("unexpected create response: {created:?}")
    };

    let snapshot = harness.resources().await;
    assert_eq!(snapshot.sessions.len(), 2);
    assert_ne!(snapshot.sessions[0].id, snapshot.sessions[1].id);
    assert_ne!(
        snapshot.sessions[0].workspaces[0].root,
        snapshot.sessions[1].workspaces[0].root
    );

    let mut ambiguous = harness.connect().await.unwrap();
    assert!(matches!(
        hello(&mut ambiguous, interactive_mode(None), PROTOCOL_VERSION).await.unwrap(),
        ServerMessage::Error { ref code, .. } if code == "target_required"
    ));

    let (mut client_a, terminal_a, pid_a) = harness
        .interactive_for(Some(TargetSelector::Session(SessionSelector::Name(
            "cwd".into(),
        ))))
        .await;
    let (mut client_b, terminal_b, pid_b) = harness
        .interactive_for(Some(TargetSelector::Session(SessionSelector::Id(
            selected_b.session_id,
        ))))
        .await;
    assert_ne!((terminal_a, pid_a), (terminal_b, pid_b));
    let a_ready = snapshot_containing(&mut client_a, terminal_a, "A_READY").await;
    let b_ready = snapshot_containing(&mut client_b, terminal_b, "B_READY").await;
    assert!(!snapshot_text(&a_ready).contains("B_READY"));
    assert!(!snapshot_text(&b_ready).contains("A_READY"));
    send(
        &mut client_a,
        ClientMessage::Input {
            bytes: b"a\n".to_vec(),
        },
    )
    .await;
    send(
        &mut client_b,
        ClientMessage::Input {
            bytes: b"b\n".to_vec(),
        },
    )
    .await;
    assert!(
        snapshot_text(&snapshot_containing(&mut client_a, terminal_a, "A_INPUT").await)
            .contains("A_INPUT")
    );
    assert!(
        snapshot_text(&snapshot_containing(&mut client_b, terminal_b, "B_INPUT").await)
            .contains("B_INPUT")
    );

    drop(client_a);
    assert!(process_alive(pid_b));
    harness
        .close_session(SessionSelector::Name("cwd".into()))
        .await;
    wait_for(DEADLINE, || !process_alive(pid_a)).await;
    assert!(process_alive(pid_b));
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    let snapshot = harness.resources().await;
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].id, selected_b.session_id);

    harness.detach(&mut client_b).await;
    drop(client_b);
    harness
        .close_session(SessionSelector::Id(selected_b.session_id))
        .await;
    wait_for(DEADLINE, || !process_alive(pid_b)).await;
    harness.wait_until_exited().await;
}

#[tokio::test]
async fn linked_git_worktree_is_a_peer_workspace_and_reopens_idempotently() {
    let main_script = r#"printf 'MAIN_READY\r\n'; while IFS= read -r line; do case "$line" in main) printf 'MAIN_INPUT\r\n';; esac; done"#;
    let mut harness = Harness::start_with(main_script, |root| {
        let main = root.join("cwd");
        git(&main, &["init", "-b", "main"]);
        fs::write(main.join("tracked"), "x").unwrap();
        git(&main, &["add", "tracked"]);
        git(&main, &["commit", "-m", "initial"]);
        let linked = root.join("linked");
        git(
            &main,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        fs::create_dir(linked.join("nested")).unwrap();
    })
    .await;

    let linked = harness.root.path().join("linked");
    let opened = harness.control_command(ClientMessage::OpenLocation {
        name: Some("linked".into()),
        cwd: linked.join("nested"),
        program: Some("/bin/sh".into()),
        argv: vec!["-c".into(), "printf 'LINKED_READY\r\n'; while IFS= read -r line; do case \"$line\" in linked) printf 'LINKED_INPUT\r\n';; esac; done".into()],
    }).await;
    let ServerMessage::LocationOpened {
        selected: linked_target,
        disposition: fut::protocol::OpenDisposition::WorkspaceCreated,
    } = opened
    else {
        panic!("unexpected linked open: {opened:?}")
    };
    let snapshot = harness.resources().await;
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].workspaces.len(), 2);
    assert!(matches!(
        snapshot.sessions[0].project.identity,
        fut::resources::ProjectIdentity::GitCommonDir(_)
    ));
    assert_eq!(
        snapshot.sessions[0].workspaces[1].root,
        linked.canonicalize().unwrap()
    );
    let main_workspace = snapshot.sessions[0].workspaces[0].id;
    let main_pane = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let main_terminal = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    assert_ne!(main_workspace, linked_target.workspace_id);
    assert_ne!(main_pane, linked_target.pane_id);
    assert_ne!(main_terminal, linked_target.terminal_id);

    let mut ambiguous = harness.connect().await.unwrap();
    assert!(matches!(
        hello(
            &mut ambiguous,
            interactive_mode(Some(TargetSelector::Session(SessionSelector::Id(
                snapshot.sessions[0].id,
            )))),
            PROTOCOL_VERSION,
        )
        .await
        .unwrap(),
        ServerMessage::Error { ref code, .. } if code == "target_required"
    ));

    let marker = harness.root.path().join("must-not-run");
    let revision = snapshot.revision;
    let reopened = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some("ignored".into()),
            cwd: linked.clone(),
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), format!("touch {}", marker.display())],
        })
        .await;
    let ServerMessage::LocationOpened {
        selected: same,
        disposition: fut::protocol::OpenDisposition::Existing,
    } = reopened
    else {
        panic!("unexpected reopen: {reopened:?}")
    };
    assert_eq!(same, linked_target);
    assert_eq!(harness.resources().await.revision, revision);
    assert!(!marker.exists());

    let (mut main_client, _, main_pid) = harness
        .interactive_for(Some(TargetSelector::Workspace(main_workspace)))
        .await;
    let (mut linked_client, _, linked_pid) = harness
        .interactive_for(Some(TargetSelector::Terminal(linked_target.terminal_id)))
        .await;
    snapshot_containing(&mut main_client, main_terminal, "MAIN_READY").await;
    snapshot_containing(
        &mut linked_client,
        linked_target.terminal_id,
        "LINKED_READY",
    )
    .await;
    send(
        &mut linked_client,
        ClientMessage::Input {
            bytes: b"linked\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(
        &mut linked_client,
        linked_target.terminal_id,
        "LINKED_INPUT",
    )
    .await;
    drop(linked_client);
    assert_eq!(
        harness
            .control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Workspace(linked_target.workspace_id)
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::CloseTarget
        }
    );
    wait_for(DEADLINE, || !process_alive(linked_pid)).await;
    assert!(process_alive(main_pid));
    send(
        &mut main_client,
        ClientMessage::Input {
            bytes: b"main\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut main_client, main_terminal, "MAIN_INPUT").await;
    drop(main_client);
    harness
        .close_session(SessionSelector::Id(snapshot.sessions[0].id))
        .await;
    harness.wait_until_exited().await;
}

#[tokio::test]
async fn existing_reopen_is_idempotent_and_invalid_name_never_spawns() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let before = harness.resources().await;
    let pid_file = harness.root.path().join("duplicate.pid");
    let marker_file = harness.root.path().join("duplicate.marker");
    let response = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some("cwd".into()),
            cwd: harness.root.path().join("cwd"),
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                format!(
                    "echo $$ > {}; touch {}; sleep 60",
                    pid_file.display(),
                    marker_file.display()
                ),
            ],
        })
        .await;
    assert!(matches!(
        response,
        ServerMessage::LocationOpened {
            disposition: fut::protocol::OpenDisposition::Existing,
            ..
        }
    ));
    let response = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some("different-name".into()),
            cwd: harness.root.path().join("cwd"),
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                format!("touch {}; sleep 60", marker_file.display()),
            ],
        })
        .await;
    assert!(matches!(
        response,
        ServerMessage::LocationOpened {
            disposition: fut::protocol::OpenDisposition::Existing,
            ..
        }
    ));
    let after = harness.resources().await;
    assert_eq!(after, before);
    time::sleep(Duration::from_millis(100)).await;
    assert!(!pid_file.exists(), "rejected command wrote its PID file");
    assert!(
        !marker_file.exists(),
        "rejected command wrote its marker file"
    );

    let blank_marker = harness.root.path().join("blank.marker");
    let blank_cwd = harness.root.path().join("blank-project");
    fs::create_dir(&blank_cwd).unwrap();
    let response = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some(" \t ".into()),
            cwd: blank_cwd,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), format!("touch {}", blank_marker.display())],
        })
        .await;
    assert!(matches!(response, ServerMessage::Error { ref code, .. } if code == "invalid_name"));
    time::sleep(Duration::from_millis(100)).await;
    assert!(!blank_marker.exists(), "invalid-name command executed");
    let after_blank = harness.resources().await;
    assert_eq!(after_blank, before);
    harness.shutdown().await;
}

#[tokio::test]
async fn terminal_attachment_lease_rejects_contention_and_releases_on_detach_and_eof() {
    let mut harness = Harness::start("while IFS= read -r line; do :; done").await;
    let (mut first, terminal_id, child_pid) = harness.interactive().await;

    let mut contender = harness.connect().await.unwrap();
    assert!(matches!(
        hello(&mut contender, interactive_mode(Some(TargetSelector::Session(SessionSelector::Name("cwd".into())))), PROTOCOL_VERSION).await.unwrap(),
        ServerMessage::Error { ref code, .. } if code == "already_attached"
    ));
    harness.detach(&mut first).await;
    drop(first);

    let (second, second_id, second_pid) = harness.interactive().await;
    assert_eq!((second_id, second_pid), (terminal_id, child_pid));
    drop(second);
    time::sleep(POLL_INTERVAL).await;
    let (mut third, third_id, third_pid) = harness.interactive().await;
    assert_eq!((third_id, third_pid), (terminal_id, child_pid));
    harness.detach(&mut third).await;
    drop(third);
    harness.shutdown().await;
}

#[tokio::test]
async fn public_client_navigator_switches_live_pty_and_preserves_terminal_isolation() {
    let harness = Harness::start(
        "printf 'PUBLIC_A_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'PUBLIC_A_INPUT\\r\\n'; done",
    )
    .await;
    let resources = harness.resources().await;
    let a_terminal = resources.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let (mut probe_a, a) = attach_once(&harness, TargetSelector::Terminal(a_terminal)).await;
    harness.detach(&mut probe_a).await;
    drop(probe_a);

    let cwd_b = harness.root.path().join("public-b");
    fs::create_dir(&cwd_b).unwrap();
    let ServerMessage::LocationOpened { selected: b, .. } = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some("public-b".into()),
            cwd: cwd_b,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'PUBLIC_B_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = b ] && printf 'PUBLIC_B_INPUT\\r\\n'; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create public B target")
    };

    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("HOME", harness.root.path().join("home"))
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", harness.root.path().join("runtime"))
        .env("FUT_RUNTIME_DIR", harness.root.path().join("runtime"))
        .env("TERM", "xterm-256color")
        .args(["-q", "/dev/null", "/bin/sh", "-c"])
        .arg(format!(
            "stty rows 24 cols 80; exec '{}' --socket '{}' attach terminal:{}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
            a.terminal_id
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("PUBLIC_A_READY").await;
    client.send(b"\x02g");
    client.wait_for("fut · navigator").await;
    client.wait_for("public-b").await;
    client.send(b"\x1b[F\r");
    client.wait_for("PUBLIC_B_READY").await;
    client.send(b"b\n");
    client.wait_for("PUBLIC_B_INPUT").await;
    client.send(b"\x02d");
    client.wait_success().await;

    assert!(process_alive(a.child_pid));
    assert!(process_alive(b.child_pid));
    let (mut raw_a, selected_a) =
        attach_once(&harness, TargetSelector::Terminal(a.terminal_id)).await;
    assert_eq!(
        (selected_a.terminal_id, selected_a.child_pid),
        (a.terminal_id, a.child_pid)
    );
    let screen_a = snapshot_containing(&mut raw_a, a.terminal_id, "PUBLIC_A_READY").await;
    assert!(!snapshot_text(&screen_a).contains("PUBLIC_B_INPUT"));
    harness.detach(&mut raw_a).await;
    drop(raw_a);
    let (mut raw_b, selected_b) =
        attach_once(&harness, TargetSelector::Terminal(b.terminal_id)).await;
    assert_eq!(
        (selected_b.terminal_id, selected_b.child_pid),
        (b.terminal_id, b.child_pid)
    );
    let screen_b = snapshot_containing(&mut raw_b, b.terminal_id, "PUBLIC_B_INPUT").await;
    assert!(!snapshot_text(&screen_b).contains("PUBLIC_A_READY"));
    harness.detach(&mut raw_b).await;
    drop(raw_b);
    harness.shutdown().await;
}

#[tokio::test]
async fn public_client_ctrl_b_c_creates_routes_and_navigates_back() {
    let harness = Harness::start(
        "printf 'CTRL_C_A_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'CTRL_C_A_INPUT\\r\\n'; done",
    )
    .await;
    let before = harness.resources().await;
    let workspace = &before.sessions[0].workspaces[0];
    let workspace_id = workspace.id;
    let a_terminal = workspace.tabs[0].panes[0].terminal_id;
    let (mut probe, a) = attach_once(&harness, TargetSelector::Terminal(a_terminal)).await;
    harness.detach(&mut probe).await;
    drop(probe);

    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("HOME", harness.root.path().join("home"))
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", harness.root.path().join("runtime"))
        .env("FUT_RUNTIME_DIR", harness.root.path().join("runtime"))
        .env("TERM", "xterm-256color")
        .args(["-q", "/dev/null", "/bin/sh", "-c"])
        .arg(format!(
            "stty rows 24 cols 80; exec '{}' --socket '{}' attach terminal:{}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
            a.terminal_id
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("CTRL_C_A_READY").await;
    client.send(b"\x02c");

    let created = time::timeout(DEADLINE, async {
        loop {
            let snapshot = harness.resources().await;
            let workspace = snapshot.sessions[0]
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .expect("original workspace remains present");
            if workspace.tabs.len() == 2 {
                break workspace.tabs[1].panes[0].terminal_id;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("Ctrl-b c did not create a second tab");

    client.send(b"printf 'CTRL_C_NEW_INPUT\\r\\n'\n");
    client.wait_for("CTRL_C_NEW_INPUT").await;
    client.send(b"\x02g");
    client.wait_for("fut · navigator").await;
    client.send(b"gjjj\r");
    client.wait_for_count("CTRL_C_A_READY", 2).await;
    client.send(b"a\n");
    client.wait_for("CTRL_C_A_INPUT").await;
    client.send(b"\x02d");
    client.wait_success().await;

    assert!(process_alive(a.child_pid));
    let (mut raw_a, selected_a) = attach_once(&harness, TargetSelector::Terminal(a_terminal)).await;
    assert_eq!(selected_a.child_pid, a.child_pid);
    assert!(process_alive(selected_a.child_pid));
    snapshot_containing(&mut raw_a, a_terminal, "CTRL_C_A_INPUT").await;
    harness.detach(&mut raw_a).await;
    drop(raw_a);
    let (mut raw_created, selected_created) =
        attach_once(&harness, TargetSelector::Terminal(created)).await;
    assert!(process_alive(selected_created.child_pid));
    snapshot_containing(&mut raw_created, created, "CTRL_C_NEW_INPUT").await;
    harness.detach(&mut raw_created).await;
    drop(raw_created);
    harness.shutdown().await;
}

#[tokio::test]
async fn switching_is_atomic_reversible_and_keeps_protocol_routing_isolated() {
    let mut harness = Harness::start(
        "printf 'A_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'A_INPUT\\r\\n'; done",
    )
    .await;
    let cwd_b = harness.root.path().join("switch-b");
    fs::create_dir(&cwd_b).unwrap();
    let opened = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some("switch-b".into()),
            cwd: cwd_b,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'B_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = b ] && printf 'B_INPUT\\r\\n'; done".into(),
            ],
        })
        .await;
    let ServerMessage::LocationOpened { selected: b, .. } = opened else {
        panic!("expected second target, got {opened:?}")
    };
    let resources = harness.resources().await;
    let a_session = resources.sessions[0].id;
    let (mut connection, a_id, a_pid) = harness
        .interactive_for(Some(TargetSelector::Session(SessionSelector::Id(
            a_session,
        ))))
        .await;
    snapshot_containing(&mut connection, a_id, "A_READY").await;

    let a = SelectedTarget {
        session_id: resources.sessions[0].id,
        workspace_id: resources.sessions[0].workspaces[0].id,
        tab_id: resources.sessions[0].workspaces[0].tabs[0].id,
        pane_id: resources.sessions[0].workspaces[0].tabs[0].panes[0].id,
        terminal_id: a_id,
        child_pid: a_pid,
    };

    select_and_require_next_snapshot(&mut connection, TargetSelector::Terminal(b.terminal_id), &b)
        .await;

    // TargetSelected alone is the gate: no delay or unrelated round trip is needed
    // before a one-shot client can acquire the exact old lease.
    let (mut old_target, reattached) = attach_once(&harness, TargetSelector::Terminal(a_id)).await;
    assert_eq!(reattached, a);
    harness.detach(&mut old_target).await;
    drop(old_target);

    send(
        &mut connection,
        ClientMessage::Input {
            bytes: b"b\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut connection, b.terminal_id, "B_INPUT").await;

    select_and_require_next_snapshot(&mut connection, TargetSelector::Terminal(a_id), &a).await;
    send(
        &mut connection,
        ClientMessage::Input {
            bytes: b"a\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut connection, a_id, "A_INPUT").await;

    select_and_require_next_snapshot(&mut connection, TargetSelector::Pane(b.pane_id), &b).await;
    select_and_require_next_snapshot(&mut connection, TargetSelector::Terminal(a_id), &a).await;
    harness.detach(&mut connection).await;
    drop(connection);
    harness.shutdown().await;
}

#[tokio::test]
async fn same_target_list_and_failed_switches_preserve_the_attachment() {
    let mut harness = Harness::start(
        "printf 'A_READY\\r\\n'; while IFS= read -r line; do printf 'A_%s\\r\\n' \"$line\"; done",
    )
    .await;
    let cwd_b = harness.root.path().join("held-b");
    fs::create_dir(&cwd_b).unwrap();
    let ServerMessage::LocationOpened { selected: b, .. } = harness.control_command(ClientMessage::OpenLocation {
        name: Some("held-b".into()), cwd: cwd_b, program: Some("/bin/sh".into()),
        argv: vec!["-c".into(), "printf 'B_READY\\r\\n'; while IFS= read -r line; do printf 'B_%s\\r\\n' \"$line\"; done".into()],
    }).await else { panic!("expected B") };
    let resources = harness.resources().await;
    let pane_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let (mut a, a_id, a_pid) = harness
        .interactive_for(Some(TargetSelector::Pane(pane_a)))
        .await;
    let (mut held_b, _, _) = harness
        .interactive_for(Some(TargetSelector::Terminal(b.terminal_id)))
        .await;

    for selector in [TargetSelector::Terminal(a_id), TargetSelector::Pane(pane_a)] {
        let selected = select_response(&mut a, selector).await;
        assert_eq!((selected.terminal_id, selected.child_pid), (a_id, a_pid));
        let contender = attach_error_once(&harness, TargetSelector::Terminal(a_id)).await;
        assert_eq!(contender, "already_attached");
    }
    let request = Uuid::new_v4();
    send_envelope(
        &mut a,
        Envelope {
            request_id: Some(request),
            message: ClientMessage::ListResources,
        },
    )
    .await;
    let response = loop {
        let response = receive_envelope(&mut a).await.unwrap();
        if response.request_id == Some(request) {
            break response;
        }
        match response.message {
            ServerMessage::Snapshot { terminal_id, .. } => assert_eq!(terminal_id, a_id),
            other => panic!("unexpected frame before Resources: {other:?}"),
        }
    };
    assert!(matches!(response.message, ServerMessage::Resources { .. }));

    assert_eq!(
        select_error(&mut a, TargetSelector::Terminal(b.terminal_id)).await,
        "already_attached"
    );
    send(
        &mut a,
        ClientMessage::Input {
            bytes: b"still-a\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut a, a_id, "A_still-a").await;
    send(
        &mut held_b,
        ClientMessage::Input {
            bytes: b"still-b\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut held_b, b.terminal_id, "B_still-b").await;

    harness.detach(&mut held_b).await;
    drop(held_b);
    select_and_require_next_snapshot(&mut a, TargetSelector::Terminal(b.terminal_id), &b).await;
    harness.detach(&mut a).await;
    drop(a);
    let (mut released, selected) =
        attach_once(&harness, TargetSelector::Terminal(b.terminal_id)).await;
    assert_eq!(selected, b);
    harness.detach(&mut released).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn missing_destination_and_post_switch_disconnects_preserve_exact_leases() {
    let mut harness = Harness::start(
        "printf 'A_READY\\r\\n'; while IFS= read -r line; do printf 'A_%s\\r\\n' \"$line\"; done",
    )
    .await;
    let cwd_b = harness.root.path().join("closing-b");
    fs::create_dir(&cwd_b).unwrap();
    let ServerMessage::LocationOpened { selected: b, .. } = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some("closing-b".into()),
            cwd: cwd_b,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "while IFS= read -r line; do :; done".into()],
        })
        .await
    else {
        panic!("expected B")
    };
    let resources = harness.resources().await;
    let a_id = resources
        .sessions
        .iter()
        .find(|session| session.id != b.session_id)
        .unwrap()
        .workspaces[0]
        .tabs[0]
        .panes[0]
        .terminal_id;
    let (mut a, _, _) = harness
        .interactive_for(Some(TargetSelector::Terminal(a_id)))
        .await;
    harness
        .close_session(SessionSelector::Id(b.session_id))
        .await;
    loop {
        let resources = harness.resources().await;
        if resources
            .sessions
            .iter()
            .all(|session| session.id != b.session_id)
        {
            break;
        }
    }
    assert_eq!(
        select_error(&mut a, TargetSelector::Terminal(b.terminal_id)).await,
        "not_found"
    );
    send(
        &mut a,
        ClientMessage::Input {
            bytes: b"after-missing\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut a, a_id, "A_after-missing").await;

    let cwd_c = harness.root.path().join("disconnect-c");
    fs::create_dir(&cwd_c).unwrap();
    let ServerMessage::LocationOpened { selected: c, .. } = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some("disconnect-c".into()),
            cwd: cwd_c,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "while IFS= read -r line; do :; done".into()],
        })
        .await
    else {
        panic!("expected C")
    };
    select_and_require_next_snapshot(&mut a, TargetSelector::Terminal(c.terminal_id), &c).await;
    harness.detach(&mut a).await;
    drop(a);
    let (mut after_detach, selected) =
        attach_once(&harness, TargetSelector::Terminal(c.terminal_id)).await;
    assert_eq!(selected, c);
    harness.detach(&mut after_detach).await;
    drop(after_detach);

    let (mut abrupt, _, _) = harness
        .interactive_for(Some(TargetSelector::Terminal(a_id)))
        .await;
    select_and_require_next_snapshot(&mut abrupt, TargetSelector::Terminal(c.terminal_id), &c)
        .await;
    drop(abrupt);
    let (mut after_eof, selected) = time::timeout(DEADLINE, async {
        loop {
            let mut contender = harness.connect().await.expect("connect EOF contender");
            match hello(
                &mut contender,
                interactive_mode(Some(TargetSelector::Terminal(c.terminal_id))),
                PROTOCOL_VERSION,
            )
            .await
            .unwrap()
            {
                ServerMessage::Welcome {
                    selected: Some(selected),
                    ..
                } => break (contender, selected),
                ServerMessage::Error { ref code, .. } if code == "already_attached" => continue,
                other => panic!("unexpected EOF contender response: {other:?}"),
            }
        }
    })
    .await
    .expect("EOF did not release destination lease");
    assert_eq!(selected, c);
    harness.detach(&mut after_eof).await;
    drop(after_eof);
    harness.shutdown().await;
}

#[tokio::test]
async fn unsupported_protocol_is_rejected_without_harming_daemon() {
    let mut harness = Harness::start("while IFS= read -r line; do :; done").await;
    let mut incompatible = harness.connect().await.expect("connect mismatched client");
    let unsupported = PROTOCOL_VERSION + 1;
    assert_eq!(
        hello(&mut incompatible, interactive_mode(None), unsupported)
            .await
            .expect("receive protocol rejection"),
        ServerMessage::IncompatibleProtocol {
            client: unsupported,
            server: PROTOCOL_VERSION,
        }
    );
    let closed = time::timeout(DEADLINE, incompatible.next())
        .await
        .expect("mismatched connection did not close");
    assert!(
        closed.is_none(),
        "daemon sent another frame after rejection"
    );

    let (mut interactive, terminal_id, child_pid) = harness.interactive().await;
    harness.detach(&mut interactive).await;
    drop(interactive);

    let (mut follow_up, follow_up_id, follow_up_pid) = harness.interactive().await;
    assert_eq!((follow_up_id, follow_up_pid), (terminal_id, child_pid));
    harness.detach(&mut follow_up).await;
    drop(follow_up);
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    harness.shutdown().await;
}

async fn hello(
    connection: &mut Connection,
    mode: ClientMode,
    version: u16,
) -> Result<ServerMessage, &'static str> {
    send_envelope(
        connection,
        Envelope {
            request_id: Some(Uuid::new_v4()),
            message: ClientMessage::Hello {
                version,
                client_version: "fut-e2e".into(),
                mode,
            },
        },
    )
    .await;
    receive(connection).await.ok_or("connection closed")
}

fn interactive_mode(selector: Option<TargetSelector>) -> ClientMode {
    ClientMode::Interactive {
        size: SIZE,
        selector,
    }
}

async fn send(connection: &mut Connection, message: ClientMessage) {
    send_envelope(
        connection,
        Envelope {
            request_id: Some(Uuid::new_v4()),
            message,
        },
    )
    .await;
}

async fn send_envelope(connection: &mut Connection, envelope: Envelope<ClientMessage>) {
    connection
        .send(Bytes::from(
            encode_payload(&envelope).expect("encode client frame"),
        ))
        .await
        .expect("send client frame");
}

async fn receive(connection: &mut Connection) -> Option<ServerMessage> {
    receive_envelope(connection)
        .await
        .map(|envelope| envelope.message)
}

async fn receive_envelope(connection: &mut Connection) -> Option<Envelope<ServerMessage>> {
    let frame = time::timeout(DEADLINE, connection.next())
        .await
        .expect("protocol receive timed out")?
        .expect("read server frame")
        .to_vec();
    Some(decode_payload(&frame).expect("decode server frame"))
}

async fn select_response(connection: &mut Connection, selector: TargetSelector) -> SelectedTarget {
    let request_id = Uuid::new_v4();
    send_envelope(
        connection,
        Envelope {
            request_id: Some(request_id),
            message: ClientMessage::SelectTarget { selector },
        },
    )
    .await;
    loop {
        let response = receive_envelope(connection)
            .await
            .expect("connection closed during selection");
        if response.request_id == Some(request_id) {
            let ServerMessage::TargetSelected { selected } = response.message else {
                panic!("expected TargetSelected, got {:?}", response.message)
            };
            return selected;
        }
    }
}

async fn select_error(connection: &mut Connection, selector: TargetSelector) -> String {
    let request_id = Uuid::new_v4();
    send_envelope(
        connection,
        Envelope {
            request_id: Some(request_id),
            message: ClientMessage::SelectTarget { selector },
        },
    )
    .await;
    loop {
        let response = receive_envelope(connection)
            .await
            .expect("connection closed during selection");
        if response.request_id == Some(request_id) {
            let ServerMessage::Error { code, .. } = response.message else {
                panic!("expected selection error, got {:?}", response.message)
            };
            return code;
        }
    }
}

async fn select_and_require_next_snapshot(
    connection: &mut Connection,
    selector: TargetSelector,
    expected: &SelectedTarget,
) {
    assert_eq!(select_response(connection, selector).await, *expected);
    let next = receive_envelope(connection)
        .await
        .expect("missing destination snapshot");
    match next.message {
        ServerMessage::Snapshot { terminal_id, .. } => {
            assert_eq!(terminal_id, expected.terminal_id)
        }
        other => {
            panic!("expected destination Snapshot immediately after TargetSelected, got {other:?}")
        }
    }
}

async fn attach_once(harness: &Harness, selector: TargetSelector) -> (Connection, SelectedTarget) {
    let mut connection = harness.connect().await.expect("connect one-shot contender");
    let ServerMessage::Welcome {
        selected: Some(selected),
        ..
    } = hello(
        &mut connection,
        interactive_mode(Some(selector)),
        PROTOCOL_VERSION,
    )
    .await
    .expect("one-shot contender disconnected")
    else {
        panic!("one-shot contender did not acquire lease")
    };
    (connection, selected)
}

async fn attach_error_once(harness: &Harness, selector: TargetSelector) -> String {
    let mut connection = harness.connect().await.expect("connect one-shot contender");
    let ServerMessage::Error { code, .. } = hello(
        &mut connection,
        interactive_mode(Some(selector)),
        PROTOCOL_VERSION,
    )
    .await
    .expect("one-shot contender disconnected") else {
        panic!("one-shot contender unexpectedly acquired lease")
    };
    code
}

async fn receive_matching(
    connection: &mut Connection,
    predicate: impl Fn(&ServerMessage) -> bool,
) -> ServerMessage {
    time::timeout(DEADLINE, async {
        loop {
            let message = receive(connection)
                .await
                .expect("daemon disconnected while waiting for message");
            if predicate(&message) {
                return message;
            }
        }
    })
    .await
    .expect("matching protocol message timed out")
}

async fn snapshot_containing(
    connection: &mut Connection,
    terminal_id: TerminalId,
    needle: &str,
) -> ScreenSnapshot {
    let message = receive_matching(connection, |message| match message {
        ServerMessage::Snapshot {
            terminal_id: id,
            screen,
        } => *id == terminal_id && snapshot_text(screen).contains(needle),
        _ => false,
    })
    .await;
    match message {
        ServerMessage::Snapshot { screen, .. } => screen,
        _ => unreachable!(),
    }
}

fn snapshot_text(snapshot: &ScreenSnapshot) -> String {
    snapshot
        .cells
        .chunks(usize::from(snapshot.size.columns))
        .map(|row| {
            row.iter()
                .map(|cell| cell.contents.as_str())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_ordered(text: &str, needles: &[&str]) {
    let mut offset = 0;
    for needle in needles {
        let relative = text[offset..]
            .find(needle)
            .unwrap_or_else(|| panic!("snapshot lacks {needle:?} after byte {offset}:\n{text}"));
        offset += relative + needle.len();
    }
}

async fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) {
    time::timeout(timeout, async {
        while !condition() {
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("condition did not become true before timeout");
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: signal zero does not modify the target process.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
