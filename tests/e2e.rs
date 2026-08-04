#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use bytes::Bytes;
use fut::{
    domain::{PaneId, ScreenSnapshot, TabId, TerminalId, TerminalSize},
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, RenameSelector, SelectedTarget,
        ServerMessage, codec, decode_payload, encode_payload,
    },
    resources::{SessionSelector, TargetSelector},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
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
                        self.terminal_pid = Some(selected.focused.child_pid);
                        return (
                            connection,
                            selected.focused.terminal_id,
                            selected.focused.child_pid,
                        );
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
        .arg("run")
        .arg("--cwd")
        .arg(cwd)
        .arg("--")
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

        let shutdown = harness.cli().args(["daemon", "shutdown"]).output().unwrap();
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
        .args(["open"])
        .arg(&cwd)
        .args(["--name", "second", "--", "/bin/sh", "-c"])
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
    assert!(created.contains("disposition=SessionCreated"));
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
        .args(["workspace", "close", workspace])
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
async fn public_tab_new_is_isolated_and_public_tab_close_preserves_its_sibling() {
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
        .args(["tab", "new", &workspace_id.to_string(), "--name", "開発 λ", "--cwd", "relative-dir", "--", "/bin/sh", "-c"])
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
    let close = harness
        .cli()
        .args(["tab", "close", &tab_id.to_string()])
        .output()
        .unwrap();
    assert!(
        close.status.success(),
        "{}",
        String::from_utf8_lossy(&close.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&close.stdout).trim(), "closed=true");
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
async fn public_pane_new_preserves_attachment_isolates_input_and_cascades_on_last_close() {
    let mut harness = Harness::start(
        "printf 'PANE_A_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'PANE_A_INPUT\\r\\n'; done",
    )
    .await;
    let before = harness.resources().await;
    let session = &before.sessions[0];
    let workspace = &session.workspaces[0];
    let tab = &workspace.tabs[0];
    let pane_a = &tab.panes[0];
    let (mut attached_a, selected_a) =
        attach_once(&harness, TargetSelector::Terminal(pane_a.terminal_id)).await;
    snapshot_containing(&mut attached_a, pane_a.terminal_id, "PANE_A_READY").await;

    fs::create_dir(harness.root.path().join("cwd/pane-relative")).unwrap();
    let pwd = harness.root.path().join("pane-b.pwd");
    let output = harness
        .cli()
        .args(["pane", "new", &tab.id.to_string(), "--cwd", "pane-relative", "--", "/bin/sh", "-c"])
        .arg(format!(
            "pwd > {}; printf 'PANE_B_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = b ] && printf 'PANE_B_INPUT\\r\\n'; done",
            pwd.display()
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let field = |name: &str| {
        text.split_whitespace()
            .find_map(|field| field.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("missing {name} in {text:?}"))
    };
    assert_eq!(field("session"), session.id.to_string());
    assert_eq!(field("workspace"), workspace.id.to_string());
    assert_eq!(field("tab"), tab.id.to_string());
    let pane_b = field("pane").parse().unwrap();
    let terminal_b = field("terminal").parse().unwrap();
    let pid_b: u32 = field("pid").parse().unwrap();
    assert_ne!(pane_b, pane_a.id);
    assert_ne!(terminal_b, pane_a.terminal_id);
    assert_ne!(pid_b, selected_a.child_pid);
    wait_for(DEADLINE, || pwd.exists()).await;
    assert_eq!(
        fs::read_to_string(&pwd).unwrap().trim(),
        harness
            .root
            .path()
            .join("cwd/pane-relative")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );

    let after = harness.resources().await;
    let panes = &after.sessions[0].workspaces[0].tabs[0].panes;
    assert_eq!(
        panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
        [pane_a.id, pane_b]
    );
    assert_eq!(
        attach_error_once(&harness, TargetSelector::Terminal(pane_a.terminal_id)).await,
        "already_attached"
    );
    for target in [
        TargetSelector::Session(SessionSelector::Id(session.id)),
        TargetSelector::Workspace(workspace.id),
        TargetSelector::Tab(tab.id),
    ] {
        assert_eq!(attach_error_once(&harness, target).await, "target_required");
    }

    let (mut attached_b, selected_b) = attach_once(&harness, TargetSelector::Pane(pane_b)).await;
    assert_eq!(selected_b.pane_id, pane_b);
    assert_eq!(selected_b.terminal_id, terminal_b);
    snapshot_containing(&mut attached_b, terminal_b, "PANE_B_READY").await;
    send(
        &mut attached_b,
        ClientMessage::Input {
            bytes: b"b\n".to_vec(),
        },
    )
    .await;
    let b_screen = snapshot_containing(&mut attached_b, terminal_b, "PANE_B_INPUT").await;
    assert!(!snapshot_text(&b_screen).contains("PANE_A_INPUT"));
    send(
        &mut attached_a,
        ClientMessage::Input {
            bytes: b"a\n".to_vec(),
        },
    )
    .await;
    let a_screen = snapshot_containing(&mut attached_a, pane_a.terminal_id, "PANE_A_INPUT").await;
    assert!(!snapshot_text(&a_screen).contains("PANE_B_INPUT"));

    harness.detach(&mut attached_b).await;
    drop(attached_b);
    assert!(
        harness
            .cli()
            .args(["pane", "close", &pane_b.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    );
    wait_for(DEADLINE, || !process_alive(pid_b)).await;
    let surviving = resources_when(&harness, |snapshot| {
        snapshot
            .sessions
            .first()
            .and_then(|session| session.workspaces.first())
            .and_then(|workspace| workspace.tabs.first())
            .is_some_and(|tab| tab.panes.len() == 1)
    })
    .await;
    assert_eq!(surviving.sessions[0].workspaces[0].tabs[0].panes.len(), 1);
    assert!(process_alive(selected_a.child_pid));
    assert!(harness.socket.exists());
    harness.detach(&mut attached_a).await;
    drop(attached_a);
    assert!(
        harness
            .cli()
            .args(["pane", "close", &pane_a.id.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    );
    wait_for(DEADLINE, || !process_alive(selected_a.child_pid)).await;
    harness.wait_until_exited().await;
}

#[tokio::test]
async fn public_pane_move_preserves_attachment_identity_order_and_cascades_empty_tab() {
    let harness = Harness::start(
        "printf 'MOVE_A_READY\r\n'; while IFS= read -r line; do [ \"$line\" = moved ] && printf 'MOVE_A_INPUT\r\n'; done",
    )
    .await;
    let initial = harness.resources().await;
    let workspace = &initial.sessions[0].workspaces[0];
    let source_tab = workspace.tabs[0].id;
    let pane_a = workspace.tabs[0].panes[0].id;
    let terminal_a = workspace.tabs[0].panes[0].terminal_id;
    let (mut attached_a, selected_a) = attach_once(&harness, TargetSelector::Pane(pane_a)).await;
    snapshot_containing(&mut attached_a, terminal_a, "MOVE_A_READY").await;

    let sibling_output = harness
        .cli()
        .args([
            "--json",
            "pane",
            "new",
            &source_tab.to_string(),
            "--",
            "/bin/sh",
            "-c",
            "while IFS= read -r line; do :; done",
        ])
        .output()
        .unwrap();
    assert!(
        sibling_output.status.success(),
        "{}",
        String::from_utf8_lossy(&sibling_output.stderr)
    );
    let sibling_json: Value = serde_json::from_slice(&sibling_output.stdout).unwrap();
    let pane_c: PaneId = sibling_json["result"]["selected"]["pane_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let terminal_c: TerminalId = sibling_json["result"]["selected"]["terminal_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let pid_c = sibling_json["result"]["selected"]["child_pid"]
        .as_u64()
        .unwrap() as u32;

    let destination_output = harness
        .cli()
        .args([
            "--json",
            "tab",
            "new",
            &workspace.id.to_string(),
            "--name",
            "destination",
            "--",
            "/bin/sh",
            "-c",
            "while IFS= read -r line; do :; done",
        ])
        .output()
        .unwrap();
    assert!(
        destination_output.status.success(),
        "{}",
        String::from_utf8_lossy(&destination_output.stderr)
    );
    let destination_json: Value = serde_json::from_slice(&destination_output.stdout).unwrap();
    let destination_tab: TabId = destination_json["result"]["selected"]["tab_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let destination_pane: PaneId = destination_json["result"]["selected"]["pane_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let destination_pid = destination_json["result"]["selected"]["child_pid"]
        .as_u64()
        .unwrap() as u32;

    let rejected_request = Uuid::new_v4();
    send_envelope(
        &mut attached_a,
        Envelope {
            request_id: Some(rejected_request),
            message: ClientMessage::MovePane {
                pane_id: pane_a,
                destination_tab_id: destination_tab,
            },
        },
    )
    .await;
    loop {
        let response = receive_envelope(&mut attached_a)
            .await
            .expect("interactive connection closed during rejected move");
        if response.request_id == Some(rejected_request) {
            assert!(matches!(
                response.message,
                ServerMessage::Error { ref code, .. } if code == "control_only"
            ));
            break;
        }
    }

    let moved_output = harness
        .cli()
        .args([
            "--json",
            "pane",
            "move",
            &pane_a.to_string(),
            &destination_tab.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        moved_output.status.success(),
        "{}",
        String::from_utf8_lossy(&moved_output.stderr)
    );
    let moved_json: Value = serde_json::from_slice(&moved_output.stdout).unwrap();
    assert_eq!(moved_json["version"], 1);
    assert_eq!(moved_json["command"], "pane.move");
    assert_eq!(
        moved_json["result"]["source_tab_id"],
        source_tab.to_string()
    );
    assert_eq!(moved_json["result"]["moved"], true);
    assert_eq!(moved_json["result"]["source_tab_closed"], false);
    assert_eq!(
        moved_json["result"]["selected"]["tab_id"],
        destination_tab.to_string()
    );
    assert_eq!(
        moved_json["result"]["selected"]["pane_id"],
        pane_a.to_string()
    );
    assert_eq!(
        moved_json["result"]["selected"]["terminal_id"],
        terminal_a.to_string()
    );
    assert_eq!(
        moved_json["result"]["selected"]["child_pid"],
        selected_a.child_pid
    );

    let moved = harness.resources().await;
    let tabs = &moved.sessions[0].workspaces[0].tabs;
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0].id, source_tab);
    assert_eq!(
        tabs[0].panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
        [pane_c]
    );
    assert_eq!(tabs[1].id, destination_tab);
    assert_eq!(
        tabs[1].panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
        [destination_pane, pane_a]
    );
    assert!(process_alive(selected_a.child_pid));
    assert!(process_alive(pid_c));
    assert!(process_alive(destination_pid));
    assert_eq!(
        attach_error_once(&harness, TargetSelector::Terminal(terminal_a)).await,
        "already_attached"
    );

    let ServerMessage::TargetSelected { selected: refreshed } = receive_matching(
        &mut attached_a,
        |message| matches!(message, ServerMessage::TargetSelected { selected } if selected.focused.tab_id == destination_tab),
    )
    .await
    else {
        unreachable!()
    };
    assert_eq!(refreshed.focused.tab_id, destination_tab);
    assert_eq!(refreshed.focused.pane_id, pane_a);
    assert_eq!(refreshed.focused.terminal_id, terminal_a);
    assert_eq!(refreshed.focused.child_pid, selected_a.child_pid);
    assert_eq!(
        refreshed
            .panes
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<_>>(),
        [destination_pane, pane_a]
    );
    send(
        &mut attached_a,
        ClientMessage::Input {
            bytes: b"moved\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut attached_a, terminal_a, "MOVE_A_INPUT").await;

    let revision = moved.revision;
    let retry = harness
        .cli()
        .args([
            "--json",
            "pane",
            "move",
            &pane_a.to_string(),
            &destination_tab.to_string(),
        ])
        .output()
        .unwrap();
    assert!(retry.status.success());
    let retry_json: Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(
        retry_json["result"]["source_tab_id"],
        destination_tab.to_string()
    );
    assert_eq!(retry_json["result"]["moved"], false);
    assert_eq!(retry_json["result"]["source_tab_closed"], false);
    assert_eq!(harness.resources().await.revision, revision);

    let final_move = harness
        .cli()
        .args([
            "--json",
            "pane",
            "move",
            &pane_c.to_string(),
            &destination_tab.to_string(),
        ])
        .output()
        .unwrap();
    assert!(final_move.status.success());
    let final_json: Value = serde_json::from_slice(&final_move.stdout).unwrap();
    assert_eq!(
        final_json["result"]["source_tab_id"],
        source_tab.to_string()
    );
    assert_eq!(final_json["result"]["moved"], true);
    assert_eq!(final_json["result"]["source_tab_closed"], true);
    assert_eq!(
        final_json["result"]["selected"]["terminal_id"],
        terminal_c.to_string()
    );
    let after = harness.resources().await;
    let original_session = after
        .sessions
        .iter()
        .find(|session| session.id == initial.sessions[0].id)
        .unwrap();
    let tabs = &original_session.workspaces[0].tabs;
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].id, destination_tab);
    assert_eq!(
        tabs[0].panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
        [destination_pane, pane_a, pane_c]
    );
    assert!(process_alive(selected_a.child_pid));
    assert!(process_alive(pid_c));

    harness.detach(&mut attached_a).await;
    drop(attached_a);
    harness.shutdown().await;
}

#[tokio::test]
async fn closing_a_multi_pane_tab_preserves_and_routes_its_sibling_tab() {
    let mut harness = Harness::start("while IFS= read -r line; do :; done").await;
    let initial = harness.resources().await;
    let workspace = &initial.sessions[0].workspaces[0];
    let tab_a = workspace.tabs[0].id;
    let terminal_a = workspace.tabs[0].panes[0].terminal_id;
    let (mut original, selected_a) =
        attach_once(&harness, TargetSelector::Terminal(terminal_a)).await;
    let pid_a = selected_a.child_pid;
    harness.detach(&mut original).await;
    drop(original);
    let pane_b = harness
        .cli()
        .args([
            "pane",
            "new",
            &tab_a.to_string(),
            "--",
            "/bin/sh",
            "-c",
            "while IFS= read -r line; do :; done",
        ])
        .output()
        .unwrap();
    assert!(pane_b.status.success());
    let pid_b: u32 = String::from_utf8(pane_b.stdout)
        .unwrap()
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))
        .unwrap()
        .parse()
        .unwrap();
    let tab_c = harness.cli().args(["tab", "new", &workspace.id.to_string(), "--", "/bin/sh", "-c", "printf 'C_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = c ] && printf 'C_INPUT\\r\\n'; done"]).output().unwrap();
    assert!(tab_c.status.success());
    let tab_c_text = String::from_utf8(tab_c.stdout).unwrap();
    let terminal_c: TerminalId = tab_c_text
        .split_whitespace()
        .find_map(|field| field.strip_prefix("terminal="))
        .unwrap()
        .parse()
        .unwrap();
    let pid_c: u32 = tab_c_text
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))
        .unwrap()
        .parse()
        .unwrap();
    let (mut sibling, _) = attach_once(&harness, TargetSelector::Terminal(terminal_c)).await;
    snapshot_containing(&mut sibling, terminal_c, "C_READY").await;

    assert!(
        harness
            .cli()
            .args(["tab", "close", &tab_a.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    );
    wait_for(DEADLINE, || !process_alive(pid_a) && !process_alive(pid_b)).await;
    let after = resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs.len() == 1
            && snapshot.sessions[0].workspaces[0].tabs[0].id != tab_a
    })
    .await;
    let tabs = &after.sessions[0].workspaces[0].tabs;
    assert_eq!(tabs.len(), 1);
    assert_ne!(tabs[0].id, tab_a);
    assert!(process_alive(pid_c));
    assert!(harness.daemon.try_wait().unwrap().is_none());
    send(
        &mut sibling,
        ClientMessage::Input {
            bytes: b"c\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut sibling, terminal_c, "C_INPUT").await;
    harness.detach(&mut sibling).await;
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
            "could not resolve",
        ),
        (&duplicate, ".", "duplicate.marker", "duplicate"),
        (" \t ", ".", "blank.marker", "must not be blank"),
    ] {
        let marker = harness.root.path().join(marker_name);
        let output = harness
            .cli()
            .args([
                "tab",
                "new",
                &workspace_id.to_string(),
                "--name",
                name,
                "--cwd",
                cwd,
                "--",
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
        let output = harness.cli().args(["tab", "new", parent]).output().unwrap();
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
        .expect("selected new-tab view");
    assert_eq!(next.request_id, Some(request_id));
    let ServerMessage::TargetSelected { selected } = next.message else {
        panic!(
            "expected TargetSelected after TabCreated, got {:?}",
            next.message
        )
    };
    assert_eq!(selected.focused, created);
    assert_eq!(selected.panes.as_slice(), std::slice::from_ref(&created));
    snapshot_containing(&mut connection, created.terminal_id, "RAW_B_READY").await;

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
async fn interactive_create_pane_correlates_ack_switches_atomically_and_holds_lease() {
    let mut harness = Harness::start(
        "printf 'RAW_PANE_A_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = old ] && printf 'RAW_PANE_A_INPUT\\r\\n'; done",
    )
    .await;
    let resources = harness.resources().await;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let (mut connection, old_terminal, old_pid) = harness.interactive().await;
    snapshot_containing(&mut connection, old_terminal, "RAW_PANE_A_READY").await;

    let request_id = Uuid::new_v4();
    send_envelope(
        &mut connection,
        Envelope {
            request_id: Some(request_id),
            message: ClientMessage::CreatePane {
                tab_id,
                cwd: None,
                program: Some("/bin/sh".into()),
                argv: vec![
                    "-c".into(),
                    "printf 'RAW_PANE_B_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = new ] && printf 'RAW_PANE_B_INPUT\\r\\n'; done".into(),
                ],
            },
        },
    )
    .await;
    let created = loop {
        let response = receive_envelope(&mut connection)
            .await
            .expect("create-pane response");
        if response.request_id == Some(request_id) {
            let ServerMessage::PaneCreated { selected } = response.message else {
                panic!(
                    "expected correlated PaneCreated, got {:?}",
                    response.message
                )
            };
            break selected;
        }
        match response.message {
            ServerMessage::Snapshot { terminal_id, .. } => assert_eq!(terminal_id, old_terminal),
            other => panic!("unexpected frame before PaneCreated: {other:?}"),
        }
    };
    assert_eq!(created.tab_id, tab_id);
    assert_ne!(created.terminal_id, old_terminal);
    let selected = receive_envelope(&mut connection)
        .await
        .expect("selected new-pane view");
    assert_eq!(selected.request_id, Some(request_id));
    let ServerMessage::TargetSelected { selected } = selected.message else {
        panic!(
            "expected TargetSelected after PaneCreated, got {:?}",
            selected.message
        )
    };
    assert_eq!(selected.focused, created);
    assert_eq!(selected.panes.len(), 2);
    assert_eq!(selected.panes[0].terminal_id, old_terminal);
    assert_eq!(selected.panes[1], created);
    snapshot_containing(&mut connection, created.terminal_id, "RAW_PANE_B_READY").await;
    assert_eq!(
        attach_error_once(&harness, TargetSelector::Terminal(created.terminal_id)).await,
        "already_attached"
    );

    let (mut old, selected_old) =
        attach_once(&harness, TargetSelector::Terminal(old_terminal)).await;
    assert_eq!(selected_old.child_pid, old_pid);
    send(
        &mut connection,
        ClientMessage::Input {
            bytes: b"new\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut connection, created.terminal_id, "RAW_PANE_B_INPUT").await;
    send(
        &mut old,
        ClientMessage::Input {
            bytes: b"old\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut old, old_terminal, "RAW_PANE_A_INPUT").await;
    harness.detach(&mut old).await;
    harness.detach(&mut connection).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn interactive_tab_view_streams_all_panes_with_per_client_focus() {
    let harness = Harness::start(
        "printf 'VIEW_A_READY\r\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'VIEW_A_INPUT\r\n'; done",
    )
    .await;
    let initial = harness.resources().await;
    let tab_id = initial.sessions[0].workspaces[0].tabs[0].id;
    let pane_a = initial.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let terminal_a = initial.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'VIEW_B_READY\r\n'; while IFS= read -r line; do [ \"$line\" = b ] && printf 'VIEW_B_INPUT\r\n'; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create second view pane")
    };

    let mut first = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        selected: Some(first_view),
        ..
    } = hello(
        &mut first,
        interactive_mode(Some(TargetSelector::Pane(pane_a))),
        PROTOCOL_VERSION,
    )
    .await
    .unwrap()
    else {
        panic!("first view did not attach")
    };
    assert_eq!(first_view.focused.terminal_id, terminal_a);
    assert_eq!(
        first_view
            .panes
            .iter()
            .map(|pane| pane.terminal_id)
            .collect::<Vec<_>>(),
        [terminal_a, pane_b.terminal_id]
    );
    snapshot_containing(&mut first, terminal_a, "VIEW_A_READY").await;
    snapshot_containing(&mut first, pane_b.terminal_id, "VIEW_B_READY").await;

    let ServerMessage::PaneCreated { selected: pane_c } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'VIEW_C_READY\r\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create externally added view pane")
    };
    let ServerMessage::TargetSelected { selected: refreshed } = receive_matching(
        &mut first,
        |message| matches!(message, ServerMessage::TargetSelected { selected } if selected.panes.len() == 3),
    )
    .await
    else {
        unreachable!()
    };
    let changed_revision = match receive_matching(&mut first, |message| {
        matches!(
            message,
            ServerMessage::ResourcesChanged { snapshot }
                if snapshot.sessions[0].workspaces[0].tabs[0].panes.len() == 3
        )
    })
    .await
    {
        ServerMessage::ResourcesChanged { snapshot } => snapshot.revision,
        _ => unreachable!(),
    };
    assert_eq!(refreshed.resource_revision, changed_revision);
    assert_eq!(refreshed.focused.terminal_id, terminal_a);
    assert_eq!(
        refreshed
            .panes
            .iter()
            .map(|pane| pane.terminal_id)
            .collect::<Vec<_>>(),
        [terminal_a, pane_b.terminal_id, pane_c.terminal_id]
    );
    snapshot_containing(&mut first, pane_c.terminal_id, "VIEW_C_READY").await;

    send(
        &mut first,
        ClientMessage::Resize {
            terminal_id: pane_b.terminal_id,
            size: TerminalSize {
                columns: 31,
                rows: 9,
            },
        },
    )
    .await;
    assert!(matches!(
        receive_matching(&mut first, |message| matches!(
            message,
            ServerMessage::Error { code, .. } if code == "not_focused"
        ))
        .await,
        ServerMessage::Error { ref code, .. } if code == "not_focused"
    ));

    let (mut second, selected_b) =
        attach_once(&harness, TargetSelector::Terminal(pane_b.terminal_id)).await;
    assert_eq!(selected_b, pane_b);
    snapshot_containing(&mut second, pane_b.terminal_id, "VIEW_B_READY").await;
    assert_eq!(
        select_error(&mut first, TargetSelector::Terminal(pane_b.terminal_id)).await,
        "already_attached"
    );
    send(
        &mut first,
        ClientMessage::Input {
            bytes: b"a\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut first, terminal_a, "VIEW_A_INPUT").await;
    send(
        &mut second,
        ClientMessage::Input {
            bytes: b"b\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut second, pane_b.terminal_id, "VIEW_B_INPUT").await;

    harness.detach(&mut second).await;
    drop(second);
    let selected = select_response(&mut first, TargetSelector::Terminal(pane_b.terminal_id)).await;
    assert_eq!(selected, pane_b);
    send(
        &mut first,
        ClientMessage::Resize {
            terminal_id: pane_b.terminal_id,
            size: TerminalSize {
                columns: 31,
                rows: 9,
            },
        },
    )
    .await;
    receive_matching(&mut first, |message| {
        matches!(
            message,
            ServerMessage::CommandCompleted {
                command: fut::protocol::AcknowledgedCommand::Resize
            }
        )
    })
    .await;
    let resized = receive_matching(&mut first, |message| {
        matches!(
            message,
            ServerMessage::Snapshot { terminal_id, screen }
                if *terminal_id == pane_b.terminal_id
                    && screen.size == TerminalSize { columns: 31, rows: 9 }
        )
    })
    .await;
    assert!(matches!(resized, ServerMessage::Snapshot { .. }));

    harness.detach(&mut first).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn external_pane_moves_reconcile_both_tabs_and_preserve_per_client_focus() {
    let harness = Harness::start(
        "printf 'LIVE_MOVE_A_READY\r\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'LIVE_MOVE_A_INPUT\r\n'; done",
    )
    .await;
    let initial = harness.resources().await;
    let workspace = &initial.sessions[0].workspaces[0];
    let workspace_id = workspace.id;
    let source_tab = workspace.tabs[0].id;
    let pane_a = workspace.tabs[0].panes[0].id;
    let terminal_a = workspace.tabs[0].panes[0].terminal_id;

    let ServerMessage::PaneCreated { selected: pane_c } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id: source_tab,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'LIVE_MOVE_C_READY\r\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create source sibling")
    };
    let ServerMessage::TabCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreateTab {
            workspace_id,
            name: Some("destination".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'LIVE_MOVE_B_READY\r\n'; while IFS= read -r line; do [ \"$line\" = b ] && printf 'LIVE_MOVE_B_INPUT\r\n'; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create destination tab")
    };

    let mut first = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        selected: Some(first_view),
        ..
    } = hello(
        &mut first,
        interactive_mode(Some(TargetSelector::Pane(pane_a))),
        PROTOCOL_VERSION,
    )
    .await
    .unwrap()
    else {
        panic!("first client did not attach")
    };
    assert_eq!(
        first_view
            .panes
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<_>>(),
        [pane_a, pane_c.pane_id]
    );

    let mut second = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        selected: Some(second_view),
        ..
    } = hello(
        &mut second,
        interactive_mode(Some(TargetSelector::Pane(pane_b.pane_id))),
        PROTOCOL_VERSION,
    )
    .await
    .unwrap()
    else {
        panic!("second client did not attach")
    };
    assert_eq!(second_view.focused.terminal_id, pane_b.terminal_id);

    assert!(matches!(
        harness
            .control_command(ClientMessage::MovePane {
                pane_id: pane_c.pane_id,
                destination_tab_id: pane_b.tab_id,
            })
            .await,
        ServerMessage::PaneMoved { moved: true, .. }
    ));
    let ServerMessage::TargetSelected {
        selected: source_view,
    } = receive_matching(&mut first, |message| {
        matches!(message, ServerMessage::TargetSelected { selected } if selected.panes.len() == 1)
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(source_view.focused.terminal_id, terminal_a);
    let ServerMessage::TargetSelected {
        selected: destination_view,
    } = receive_matching(&mut second, |message| {
        matches!(message, ServerMessage::TargetSelected { selected } if selected.panes.len() == 2)
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(destination_view.focused.terminal_id, pane_b.terminal_id);
    assert_eq!(
        destination_view
            .panes
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<_>>(),
        [pane_b.pane_id, pane_c.pane_id]
    );

    assert!(matches!(
        harness
            .control_command(ClientMessage::MovePane {
                pane_id: pane_a,
                destination_tab_id: pane_b.tab_id,
            })
            .await,
        ServerMessage::PaneMoved {
            moved: true,
            source_tab_closed: true,
            ..
        }
    ));
    let ServerMessage::TargetSelected { selected: followed } = receive_matching(
        &mut first,
        |message| matches!(message, ServerMessage::TargetSelected { selected } if selected.focused.tab_id == pane_b.tab_id),
    )
    .await
    else {
        unreachable!()
    };
    assert_eq!(followed.focused.terminal_id, terminal_a);
    assert_eq!(
        followed
            .panes
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<_>>(),
        [pane_b.pane_id, pane_c.pane_id, pane_a]
    );
    let ServerMessage::TargetSelected { selected: expanded } = receive_matching(
        &mut second,
        |message| matches!(message, ServerMessage::TargetSelected { selected } if selected.panes.len() == 3),
    )
    .await
    else {
        unreachable!()
    };
    assert_eq!(expanded.focused.terminal_id, pane_b.terminal_id);

    send(
        &mut first,
        ClientMessage::Input {
            bytes: b"a\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut first, terminal_a, "LIVE_MOVE_A_INPUT").await;
    send(
        &mut second,
        ClientMessage::Input {
            bytes: b"b\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut second, pane_b.terminal_id, "LIVE_MOVE_B_INPUT").await;

    harness.detach(&mut first).await;
    harness.detach(&mut second).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn external_pane_closes_reconcile_background_then_transfer_focused_input() {
    let harness = Harness::start(
        "printf 'LIVE_CLOSE_A_READY\r\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'LIVE_CLOSE_A_INPUT\r\n'; done",
    )
    .await;
    let initial = harness.resources().await;
    let tab_id = initial.sessions[0].workspaces[0].tabs[0].id;
    let pane_a = initial.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let terminal_a = initial.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "while IFS= read -r line; do :; done".into()],
        })
        .await
    else {
        panic!("failed to create closeable background pane")
    };
    let mut attached = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        selected: Some(view),
        ..
    } = hello(
        &mut attached,
        interactive_mode(Some(TargetSelector::Pane(pane_a))),
        PROTOCOL_VERSION,
    )
    .await
    .unwrap()
    else {
        panic!("close reconciliation client did not attach")
    };
    assert_eq!(view.panes.len(), 2);

    assert_eq!(
        harness
            .control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Pane(pane_b.pane_id),
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::CloseTarget,
        }
    );
    let ServerMessage::TargetSelected {
        selected: background_removed,
    } = receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TargetSelected { selected } if selected.panes.len() == 1)
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(background_removed.focused.terminal_id, terminal_a);
    send(
        &mut attached,
        ClientMessage::Input {
            bytes: b"a\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut attached, terminal_a, "LIVE_CLOSE_A_INPUT").await;

    let ServerMessage::PaneCreated { selected: pane_c } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'LIVE_CLOSE_C_READY\r\n'; while IFS= read -r line; do [ \"$line\" = c ] && printf 'LIVE_CLOSE_C_INPUT\r\n'; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create focus replacement")
    };
    receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TargetSelected { selected } if selected.panes.iter().any(|pane| pane.pane_id == pane_c.pane_id))
    })
    .await;

    assert_eq!(
        harness
            .control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Pane(pane_a),
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::CloseTarget,
        }
    );
    receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TerminalExited { terminal_id, .. } if *terminal_id == terminal_a)
    })
    .await;
    let ServerMessage::TargetSelected {
        selected: transferred,
    } = receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TargetSelected { selected } if selected.focused.terminal_id == pane_c.terminal_id)
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(transferred.panes.len(), 1);
    send(
        &mut attached,
        ClientMessage::Input {
            bytes: b"c\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut attached, pane_c.terminal_id, "LIVE_CLOSE_C_INPUT").await;

    harness.detach(&mut attached).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_pane_new_rejections_are_pre_spawn_and_atomic() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let before = harness.resources().await;
    let tab_id = before.sessions[0].workspaces[0].tabs[0].id;
    let marker = harness.root.path().join("rejected-pane.marker");
    let missing_tab = Uuid::new_v4().to_string();
    for (parent, cwd, program, expected) in [
        (missing_tab.as_str(), ".", "/bin/sh", "not found"),
        (
            &tab_id.to_string(),
            "missing-directory",
            "/bin/sh",
            "could not resolve",
        ),
        (
            &tab_id.to_string(),
            ".",
            "/definitely/missing/fut-pane-command",
            "spawn",
        ),
    ] {
        let output = harness
            .cli()
            .args(["pane", "new", parent, "--cwd", cwd, "--", program, "-c"])
            .arg(format!("touch {}", marker.display()))
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted rejected pane creation");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(harness.resources().await, before);
        assert!(!marker.exists());
    }
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
                    ServerMessage::TargetSelected { selected } => {
                        assert_eq!(selected.focused.terminal_id, terminal_id);
                        assert_eq!(
                            selected.panes.as_slice(),
                            std::slice::from_ref(&selected.focused)
                        );
                    }
                    ServerMessage::ResourcesChanged { snapshot } => {
                        assert!(snapshot.revision >= 2);
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
async fn immediate_exit_interactive_create_pane_preserves_its_original_sibling() {
    let mut harness = Harness::start(
        "printf 'PANE_A_READY\\r\\n'; while IFS= read -r line; do printf 'PANE_A_%s\\r\\n' \"$line\"; done",
    )
    .await;
    let before = harness.resources().await;
    let session_id = before.sessions[0].id;
    let workspace_id = before.sessions[0].workspaces[0].id;
    let tab_id = before.sessions[0].workspaces[0].tabs[0].id;
    let pane_id = before.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let (mut connection, terminal_a, pid_a) = harness.interactive().await;
    snapshot_containing(&mut connection, terminal_a, "PANE_A_READY").await;

    let request_id = Uuid::new_v4();
    send_envelope(
        &mut connection,
        Envelope {
            request_id: Some(request_id),
            message: ClientMessage::CreatePane {
                tab_id,
                cwd: None,
                program: Some("/bin/sh".into()),
                argv: vec!["-c".into(), "printf 'PANE_B_FINAL\\r\\n'; exit 23".into()],
            },
        },
    )
    .await;

    let response = time::timeout(DEADLINE, async {
        loop {
            let response = receive_envelope(&mut connection)
                .await
                .expect("connection ended before create-pane response");
            if response.request_id == Some(request_id) {
                break response.message;
            }
            assert!(matches!(
                response.message,
                ServerMessage::Snapshot { terminal_id, .. } if terminal_id == terminal_a
            ));
        }
    })
    .await
    .expect("immediate-exit CreatePane hung");

    match response {
        ServerMessage::Error { code, .. } => {
            assert_eq!(code, "terminal_exited");
            send(
                &mut connection,
                ClientMessage::Input {
                    bytes: b"BEFORE_REATTACH\n".to_vec(),
                },
            )
            .await;
            snapshot_containing(&mut connection, terminal_a, "PANE_A_BEFORE_REATTACH").await;
            harness.detach(&mut connection).await;
        }
        ServerMessage::PaneCreated { selected } => {
            assert_eq!(selected.tab_id, tab_id);
            let mut saw_final_snapshot = false;
            loop {
                let message = time::timeout(DEADLINE, receive(&mut connection))
                    .await
                    .expect("committed immediate-exit pane hung")
                    .expect("connection ended before TerminalExited");
                match message {
                    ServerMessage::Snapshot {
                        terminal_id,
                        screen,
                    } => {
                        assert_eq!(terminal_id, selected.terminal_id);
                        saw_final_snapshot |= snapshot_text(&screen).contains("PANE_B_FINAL");
                    }
                    ServerMessage::TerminalExited {
                        terminal_id,
                        exit_code,
                    } => {
                        assert_eq!(terminal_id, selected.terminal_id);
                        assert_eq!(exit_code, Some(23));
                        assert!(saw_final_snapshot, "TerminalExited preceded final snapshot");
                        break;
                    }
                    ServerMessage::TargetSelected { selected: view } => {
                        assert_eq!(view.focused, selected);
                        assert_eq!(view.panes.len(), 2);
                    }
                    ServerMessage::ResourcesChanged { snapshot } => {
                        assert!(snapshot.revision >= 2);
                    }
                    other => panic!("unexpected frame after PaneCreated: {other:?}"),
                }
            }
            let transferred = receive_matching(&mut connection, |message| {
                matches!(
                    message,
                    ServerMessage::TargetSelected { selected }
                        if selected.focused.terminal_id == terminal_a
                )
            })
            .await;
            assert!(matches!(transferred, ServerMessage::TargetSelected { .. }));
            send(
                &mut connection,
                ClientMessage::Input {
                    bytes: b"TRANSFERRED\n".to_vec(),
                },
            )
            .await;
            snapshot_containing(&mut connection, terminal_a, "PANE_A_TRANSFERRED").await;
            harness.detach(&mut connection).await;
        }
        other => panic!("expected terminal_exited or PaneCreated, got {other:?}"),
    }
    drop(connection);

    let after = resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs[0].panes.len() == 1
            && snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id == pane_id
    })
    .await;
    assert_eq!(after.sessions[0].id, session_id);
    assert_eq!(after.sessions[0].workspaces[0].id, workspace_id);
    assert_eq!(after.sessions[0].workspaces[0].tabs[0].id, tab_id);
    assert_eq!(after.sessions[0].workspaces[0].tabs[0].panes.len(), 1);
    assert_eq!(after.sessions[0].workspaces[0].tabs[0].panes[0].id, pane_id);
    assert!(process_alive(pid_a));
    assert!(harness.socket.exists());
    assert!(harness.daemon.try_wait().unwrap().is_none());

    let (mut original, selected) =
        attach_once(&harness, TargetSelector::Terminal(terminal_a)).await;
    assert_eq!(selected.child_pid, pid_a);
    send(
        &mut original,
        ClientMessage::Input {
            bytes: b"AFTER_EXIT\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut original, terminal_a, "PANE_A_AFTER_EXIT").await;
    harness.detach(&mut original).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn naturally_exited_detached_sibling_pane_is_reaped_without_collateral_damage() {
    let mut harness = Harness::start(
        "printf 'NATURAL_A_READY\\r\\n'; while IFS= read -r line; do printf 'NATURAL_A_%s\\r\\n' \"$line\"; done",
    )
    .await;
    let before = harness.resources().await;
    let session_id = before.sessions[0].id;
    let workspace_id = before.sessions[0].workspaces[0].id;
    let tab_id = before.sessions[0].workspaces[0].tabs[0].id;
    let terminal_a = before.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let gate = harness.root.path().join("natural-pane-gate");
    let output = harness
        .cli()
        .args(["pane", "new", &tab_id.to_string(), "--", "/bin/sh", "-c"])
        .arg(format!(
            "printf 'NATURAL_B_READY\\r\\n'; while [ ! -e {} ]; do sleep 0.02; done; printf 'NATURAL_B_FINAL\\r\\n'; exit 17",
            gate.display()
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
    let pane_b = field("pane").parse().unwrap();
    let terminal_b = field("terminal").parse().unwrap();
    let pid_b: u32 = field("pid").parse().unwrap();

    let (mut sibling, _) = attach_once(&harness, TargetSelector::Terminal(terminal_b)).await;
    snapshot_containing(&mut sibling, terminal_b, "NATURAL_B_READY").await;
    fs::write(&gate, "go").unwrap();
    snapshot_containing(&mut sibling, terminal_b, "NATURAL_B_FINAL").await;
    assert_eq!(
        receive_matching(&mut sibling, |message| matches!(
            message,
            ServerMessage::TerminalExited { terminal_id, exit_code: Some(17) }
                if *terminal_id == terminal_b
        ))
        .await,
        ServerMessage::TerminalExited {
            terminal_id: terminal_b,
            exit_code: Some(17),
        }
    );
    let transferred = receive_matching(&mut sibling, |message| {
        matches!(
            message,
            ServerMessage::TargetSelected { selected }
                if selected.focused.terminal_id == terminal_a
        )
    })
    .await;
    assert!(matches!(transferred, ServerMessage::TargetSelected { .. }));
    harness.detach(&mut sibling).await;
    wait_for(DEADLINE, || !process_alive(pid_b)).await;

    let after = resources_when(&harness, |snapshot| {
        !snapshot.sessions[0].workspaces[0].tabs[0]
            .panes
            .iter()
            .any(|pane| pane.id == pane_b)
    })
    .await;
    assert_eq!(after.sessions[0].id, session_id);
    assert_eq!(after.sessions[0].workspaces[0].id, workspace_id);
    assert_eq!(after.sessions[0].workspaces[0].tabs[0].id, tab_id);
    assert!(
        !after.sessions[0].workspaces[0].tabs[0]
            .panes
            .iter()
            .any(|pane| pane.id == pane_b)
    );
    assert!(harness.socket.exists());
    assert!(harness.daemon.try_wait().unwrap().is_none());

    let (mut original, _) = attach_once(&harness, TargetSelector::Terminal(terminal_a)).await;
    snapshot_containing(&mut original, terminal_a, "NATURAL_A_READY").await;
    send(
        &mut original,
        ClientMessage::Input {
            bytes: b"ROUTES\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut original, terminal_a, "NATURAL_A_ROUTES").await;
    harness.detach(&mut original).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_cli_rejects_removed_forms_and_malformed_raw_ids_locally() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    for arguments in [
        vec!["new"],
        vec!["new-tab"],
        vec!["attach"],
        vec!["rename"],
        vec!["close"],
        vec!["ping"],
        vec!["shutdown"],
        vec!["open", "--attach"],
        vec!["tab", "new", "--attach"],
        vec!["pane", "new", "--attach"],
        vec!["pane", "new", "not-a-uuid"],
        vec!["pane", "new", "tab:00000000-0000-0000-0000-000000000000"],
        vec!["pane", "move", "not-a-uuid", "not-a-tab"],
        vec!["pane", "move", "00000000-0000-0000-0000-000000000000"],
        vec!["workspace", "attach", "workspace:not-a-uuid"],
        vec!["tab", "close", "not-a-uuid"],
        vec![
            "pane",
            "attach",
            "pane:00000000-0000-0000-0000-000000000000",
        ],
        vec!["open", "/definitely/missing/fut-e2e-path"],
    ] {
        let output = harness.cli().args(&arguments).output().unwrap();
        assert!(!output.status.success(), "accepted {arguments:?}");
        assert!(!output.stderr.is_empty(), "no diagnostic for {arguments:?}");
    }

    let resources = harness.resources().await;
    let session = &resources.sessions[0];
    let workspace = &session.workspaces[0];
    let tab = &workspace.tabs[0];
    let pane = &tab.panes[0];
    for arguments in [
        vec![
            "session".into(),
            "attach".into(),
            format!("session:{}", session.id),
        ],
        vec![
            "session".into(),
            "close".into(),
            format!("session:{}", session.id),
        ],
        vec![
            "workspace".into(),
            "attach".into(),
            format!("workspace:{}", workspace.id),
        ],
        vec![
            "workspace".into(),
            "rename".into(),
            format!("workspace:{}", workspace.id),
            "x".into(),
        ],
        vec!["tab".into(), "close".into(), format!("tab:{}", tab.id)],
        vec!["pane".into(), "attach".into(), format!("pane:{}", pane.id)],
        vec![
            "pane".into(),
            "move".into(),
            format!("pane:{}", pane.id),
            tab.id.to_string(),
        ],
        vec![
            "terminal".into(),
            "attach".into(),
            format!("terminal:{}", pane.terminal_id),
        ],
    ] {
        let output = harness.cli().args(&arguments).output().unwrap();
        assert!(
            !output.status.success(),
            "accepted removed selector syntax {arguments:?}"
        );
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn public_noun_first_attach_accepts_names_and_raw_typed_ids() {
    let harness =
        Harness::start("printf 'ATTACH_READY\\r\\n'; while IFS= read -r line; do :; done").await;
    let snapshot = harness.resources().await;
    let session = &snapshot.sessions[0];
    let workspace = &session.workspaces[0];
    let tab = &workspace.tabs[0];
    let pane = &tab.panes[0];

    for arguments in [
        vec!["session".into(), "attach".into(), session.name.clone()],
        vec!["session".into(), "attach".into(), session.id.to_string()],
        vec![
            "workspace".into(),
            "attach".into(),
            workspace.id.to_string(),
        ],
        vec!["tab".into(), "attach".into(), tab.id.to_string()],
        vec!["pane".into(), "attach".into(), pane.id.to_string()],
        vec![
            "terminal".into(),
            "attach".into(),
            pane.terminal_id.to_string(),
        ],
    ] {
        let invocation = arguments
            .iter()
            .map(|argument| format!("'{}'", argument.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
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
                "stty rows 24 cols 80; exec '{}' --socket '{}' {invocation}",
                env!("CARGO_BIN_EXE_fut"),
                harness.socket.display()
            ));
        let mut client = PtyChild::spawn(command);
        client.wait_for("ATTACH_READY").await;
        client.send(b"\x02d");
        client.wait_success().await;
    }

    let second_tab = harness
        .cli()
        .args(["tab", "new", &workspace.id.to_string()])
        .output()
        .unwrap();
    assert!(second_tab.status.success());
    let ambiguous = harness
        .cli()
        .args(["session", "attach", &session.id.to_string()])
        .output()
        .unwrap();
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("target_required"));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_json_commands_emit_compact_canonical_envelopes() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let json = |output: std::process::Output, command: &str| {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), 1, "JSON was not compact: {text:?}");
        let value: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["command"], command);
        value["result"].clone()
    };
    let json_error = |output: std::process::Output, code: &str| {
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let text = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            text.lines().count(),
            1,
            "JSON error was not compact: {text:?}"
        );
        let value: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["error"]["code"], code);
        assert!(value["error"]["message"].is_string());
        value
    };

    json_error(
        harness
            .cli()
            .args(["tab", "close", "not-a-uuid", "--json"])
            .output()
            .unwrap(),
        "invalid_arguments",
    );

    let listed = json(
        harness
            .cli()
            .args(["daemon", "--json", "ping"])
            .output()
            .unwrap(),
        "daemon.ping",
    );
    assert!(listed["daemon_pid"].is_number());
    let listed = json(
        harness.cli().args(["list", "--json"]).output().unwrap(),
        "list",
    );
    assert!(listed["sessions"].is_array());

    let cwd = harness.root.path().join("json-open");
    fs::create_dir(&cwd).unwrap();
    let marker = harness.root.path().join("delimiter.marker");
    let opened = json(
        harness
            .cli()
            .args(["--json", "open"])
            .arg(&cwd)
            .args(["--name", "json-open", "--", "/bin/sh", "-c"])
            .arg(format!(
                "test x\"$1\" = xkept && touch {}; while IFS= read -r line; do :; done",
                marker.display()
            ))
            .args(["fut-test", "kept"])
            .output()
            .unwrap(),
        "open",
    );
    assert_eq!(opened["disposition"], "session_created");
    let workspace_id = opened["selected"]["workspace_id"].as_str().unwrap();
    assert!(opened["selected"]["session_id"].is_string());
    wait_for(DEADLINE, || marker.exists()).await;

    let child_json_marker = harness.root.path().join("child-json.marker");
    let child_json_cwd = harness.root.path().join("child-json");
    fs::create_dir(&child_json_cwd).unwrap();
    let child_json = harness
        .cli()
        .args(["open"])
        .arg(&child_json_cwd)
        .args(["--", "/bin/sh", "-c"])
        .arg(format!(
            "test x\"$1\" = x--json && touch {}",
            child_json_marker.display()
        ))
        .args(["fut-test", "--json"])
        .output()
        .unwrap();
    assert!(child_json.status.success());
    assert!(serde_json::from_slice::<Value>(&child_json.stdout).is_err());
    wait_for(DEADLINE, || child_json_marker.exists()).await;

    let created = json(
        harness
            .cli()
            .args([
                "--json",
                "tab",
                "new",
                workspace_id,
                "--name",
                "json-tab",
                "--",
                "/bin/sh",
                "-c",
                "while IFS= read -r line; do :; done",
            ])
            .output()
            .unwrap(),
        "tab.new",
    );
    let tab_id = created["selected"]["tab_id"].as_str().unwrap();
    assert!(created["selected"]["pane_id"].is_string());
    assert!(created["selected"]["terminal_id"].is_string());
    let pane = json(
        harness
            .cli()
            .args(["--json", "pane", "new", tab_id])
            .output()
            .unwrap(),
        "pane.new",
    );
    assert_eq!(
        pane["selected"]["session_id"],
        created["selected"]["session_id"]
    );
    assert_eq!(
        pane["selected"]["workspace_id"],
        created["selected"]["workspace_id"]
    );
    assert_eq!(pane["selected"]["tab_id"], created["selected"]["tab_id"]);
    assert_ne!(pane["selected"]["pane_id"], created["selected"]["pane_id"]);
    assert_ne!(
        pane["selected"]["terminal_id"],
        created["selected"]["terminal_id"]
    );
    assert!(pane["selected"]["child_pid"].is_number());
    let renamed = json(
        harness
            .cli()
            .args(["--json", "tab", "rename", tab_id, "renamed"])
            .output()
            .unwrap(),
        "tab.rename",
    );
    assert_eq!(renamed["tab_id"], tab_id);
    let closed = json(
        harness
            .cli()
            .args(["--json", "tab", "close", tab_id])
            .output()
            .unwrap(),
        "tab.close",
    );
    assert_eq!(closed["tab_id"], tab_id);

    json_error(
        harness
            .cli()
            .args([
                "--json",
                "session",
                "attach",
                opened["selected"]["session_id"].as_str().unwrap(),
            ])
            .output()
            .unwrap(),
        "invalid_arguments",
    );
    json_error(
        harness.cli().args(["--json"]).output().unwrap(),
        "invalid_arguments",
    );

    let missing_id = Uuid::new_v4().to_string();
    json_error(
        harness
            .cli()
            .args(["--json", "tab", "close", &missing_id])
            .output()
            .unwrap(),
        "not_found",
    );
    let original_session = harness.resources().await.sessions[0].id.to_string();
    json_error(
        harness
            .cli()
            .args([
                "--json",
                "session",
                "rename",
                &original_session,
                "json-open",
            ])
            .output()
            .unwrap(),
        "duplicate",
    );

    let missing_socket = harness.root.path().join("runtime/missing.sock");
    json_error(
        harness
            .cli()
            .args(["daemon", "ping", "--json", "--socket"])
            .arg(missing_socket)
            .output()
            .unwrap(),
        "command_failed",
    );

    let shutdown = json(
        harness
            .cli()
            .args(["--json", "daemon", "shutdown"])
            .output()
            .unwrap(),
        "daemon.shutdown",
    );
    assert_eq!(shutdown["shutdown"], true);
    wait_for(DEADLINE, || !harness.socket.exists()).await;
}

#[tokio::test]
async fn public_child_commands_require_the_delimiter_before_the_child_argv() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let resources = harness.resources().await;
    let workspace_id = resources.sessions[0].workspaces[0].id.to_string();
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id.to_string();

    for (arguments, marker_name) in [
        (
            vec!["open", ".", "/bin/sh", "-c", "touch"],
            "open-no-delimiter",
        ),
        (
            vec!["tab", "new", &workspace_id, "/bin/sh", "-c", "touch"],
            "tab-no-delimiter",
        ),
        (
            vec!["pane", "new", &tab_id, "/bin/sh", "-c", "touch"],
            "pane-no-delimiter",
        ),
        (
            vec!["daemon", "run", "/bin/sh", "-c", "touch"],
            "daemon-no-delimiter",
        ),
    ] {
        let marker = harness.root.path().join(marker_name);
        let mut arguments = arguments.into_iter().map(str::to_owned).collect::<Vec<_>>();
        *arguments.last_mut().unwrap() = format!("touch {}", marker.display());
        let output = harness.cli().args(&arguments).output().unwrap();
        assert!(
            !output.status.success(),
            "accepted child argv without --: {arguments:?}"
        );
        assert!(
            !marker.exists(),
            "rejected child command executed: {arguments:?}"
        );
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn public_json_control_mutations_use_dotted_commands_and_raw_ids() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let json = |arguments: &[&str], command: &str| {
        let output = harness.cli().args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), 1);
        let value: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["command"], command);
        value["result"].clone()
    };

    let initial = harness.resources().await;
    let session_id = initial.sessions[0].id.to_string();
    let workspace_id = initial.sessions[0].workspaces[0].id.to_string();
    let tab_id = initial.sessions[0].workspaces[0].tabs[0].id.to_string();
    assert_eq!(
        json(
            &["session", "rename", &session_id, "json-session", "--json"],
            "session.rename"
        )["session_id"],
        session_id
    );
    assert_eq!(
        json(
            &[
                "workspace",
                "rename",
                &workspace_id,
                "json-workspace",
                "--json"
            ],
            "workspace.rename"
        )["workspace_id"],
        workspace_id
    );
    assert_eq!(
        json(
            &["tab", "rename", &tab_id, "json-tab", "--json"],
            "tab.rename"
        )["tab_id"],
        tab_id
    );

    let created = json(&["tab", "new", &workspace_id, "--json"], "tab.new");
    let pane_id = created["selected"]["pane_id"].as_str().unwrap();
    assert_eq!(
        json(&["pane", "close", pane_id, "--json"], "pane.close")["pane_id"],
        pane_id
    );

    let created = json(&["tab", "new", &workspace_id, "--json"], "tab.new");
    let close_tab_id = created["selected"]["tab_id"].as_str().unwrap();
    assert_eq!(
        json(&["tab", "close", close_tab_id, "--json"], "tab.close")["tab_id"],
        close_tab_id
    );

    let workspace_dir = harness.root.path().join("json-workspace-close");
    let session_dir = harness.root.path().join("json-session-close");
    fs::create_dir(&workspace_dir).unwrap();
    fs::create_dir(&session_dir).unwrap();
    json(&["open", workspace_dir.to_str().unwrap(), "--json"], "open");
    json(&["open", session_dir.to_str().unwrap(), "--json"], "open");
    let resources = harness.resources().await;
    let workspace_to_close = resources
        .sessions
        .iter()
        .find(|session| session.name == "json-workspace-close")
        .unwrap()
        .workspaces[0]
        .id
        .to_string();
    let session_to_close = resources
        .sessions
        .iter()
        .find(|session| session.name == "json-session-close")
        .unwrap()
        .id
        .to_string();
    assert_eq!(
        json(
            &["workspace", "close", &workspace_to_close, "--json"],
            "workspace.close"
        )["workspace_id"],
        workspace_to_close
    );
    assert_eq!(
        json(
            &["session", "close", &session_to_close, "--json"],
            "session.close"
        )["session_id"],
        session_to_close
    );

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
            "stty rows 24 cols 80; exec '{}' --socket '{}' terminal attach {}",
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
async fn public_client_renders_simultaneous_panes_and_cycles_focus() {
    let harness = Harness::start(
        "printf 'SPLIT_A_READY\r\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'SPLIT_A_INPUT\r\n'; done",
    )
    .await;
    let resources = harness.resources().await;
    let workspace_id = resources.sessions[0].workspaces[0].id;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let pane_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let terminal_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;

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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
            pane_a
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("SPLIT_A_READY").await;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'SPLIT_B_READY\r\n'; while IFS= read -r line; do [ \"$line\" = b ] && printf 'SPLIT_B_INPUT\r\n'; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create public split pane")
    };
    client.wait_for("SPLIT_B_READY").await;
    client.send(b"\x02l");
    client.send(b"b\n");
    client.wait_for("SPLIT_B_INPUT").await;

    let ServerMessage::TabCreated {
        selected: destination,
    } = harness
        .control_command(ClientMessage::CreateTab {
            workspace_id,
            name: Some("live-destination".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'ZZZZZZZZ_READY\r\n'; while IFS= read -r line; do [ \"$line\" = d ] && printf 'DESTINATION_INPUT\r\n'; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create live destination")
    };
    assert!(matches!(
        harness
            .control_command(ClientMessage::MovePane {
                pane_id: pane_b.pane_id,
                destination_tab_id: destination.tab_id,
            })
            .await,
        ServerMessage::PaneMoved { moved: true, .. }
    ));
    client.wait_for("ZZZZZZZZ").await;
    client.send(b"b\n");
    client.wait_for_count("SPLIT_B_INPUT", 2).await;
    assert_eq!(
        harness
            .control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Pane(pane_b.pane_id),
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::CloseTarget,
        }
    );
    client.send(b"d\n");
    client.wait_for("DESTINATION_INPUT").await;
    client.send(b"\x02d");
    client.wait_success().await;

    assert!(!process_alive(pane_b.child_pid));
    assert!(process_alive(destination.child_pid));
    let (mut a, selected_a) = attach_once(&harness, TargetSelector::Terminal(terminal_a)).await;
    assert_eq!(selected_a.terminal_id, terminal_a);
    let a_screen = snapshot_containing(&mut a, terminal_a, "SPLIT_A_READY").await;
    assert!(!snapshot_text(&a_screen).contains("SPLIT_B_INPUT"));
    harness.detach(&mut a).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_client_accordion_resizes_focus_and_falls_back_narrowly() {
    let harness = Harness::start(
        "printf 'AAAAAAAA\\r\\n'; while IFS= read -r line; do if [ \"$line\" = size-a ]; then set -- $(stty size); printf 'A_SIZE_%s_%s\\r\\n' \"$1\" \"$2\"; fi; done",
    )
    .await;
    let resources = harness.resources().await;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let pane_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'BBBBBBBB\\r\\n'; while IFS= read -r line; do if [ \"$line\" = size-b ]; then set -- $(stty size); printf 'B_SIZE_%s_%s\\r\\n' \"$1\" \"$2\"; fi; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create accordion sibling")
    };

    let spawn_client = |columns: u16| {
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
                "stty rows 24 cols {columns}; exec '{}' --socket '{}' pane attach {}",
                env!("CARGO_BIN_EXE_fut"),
                harness.socket.display(),
                pane_a
            ));
        PtyChild::spawn(command)
    };

    let mut narrow = spawn_client(37);
    narrow.wait_for("AAAAAAAA").await;
    narrow.send(b"size-a\n");
    narrow.wait_for("A_SIZE_23_37").await;
    narrow.send(b"\x02l");
    narrow.send(b"size-b\n");
    narrow.wait_for("B_SIZE_23_37").await;
    narrow.send(b"\x02d");
    narrow.wait_success().await;

    let mut accordion = spawn_client(38);
    accordion.wait_for("AAAAAAAA").await;
    accordion.wait_for("BBBBBBBB").await;
    accordion.send(b"size-a\n");
    accordion.wait_for("A_SIZE_23_24").await;
    accordion.send(b"\x02l");
    accordion.send(b"size-b\n");
    accordion.wait_for("B_SIZE_23_24").await;
    accordion.send(b"\x02d");
    accordion.wait_success().await;

    assert!(process_alive(pane_b.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_tab_bar_tracks_live_tabs_resizes_content_and_honors_global_position() {
    let harness = Harness::start(
        "printf 'TAB_BAR_READY\\r\\n'; while IFS= read -r line; do case \"$line\" in top-probe) set -- $(stty size); printf 'TOP_SIZE_%s_%s\\r\\n' \"$1\" \"$2\";; bottom-probe) set -- $(stty size); printf 'BOTTOM_SIZE_%s_%s\\r\\n' \"$1\" \"$2\";; esac; done",
    )
    .await;
    let resources = harness.resources().await;
    let workspace_id = resources.sessions[0].workspaces[0].id;
    let current_tab = resources.sessions[0].workspaces[0].tabs[0].id;
    let current_pane = resources.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let ServerMessage::TabCreated { selected: sibling } = harness
        .control_command(ClientMessage::CreateTab {
            workspace_id,
            name: Some("tests".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "while :; do sleep 1; done".into()],
        })
        .await
    else {
        panic!("failed to create tab-bar sibling")
    };

    let spawn_client = || {
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
                "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {}",
                env!("CARGO_BIN_EXE_fut"),
                harness.socket.display(),
                current_pane
            ));
        PtyChild::spawn(command)
    };

    let mut top = spawn_client();
    top.wait_for("TAB_BAR_READY").await;
    top.wait_for("tests").await;
    top.send(b"top-probe\n");
    top.wait_for("TOP_SIZE_23_80").await;

    assert!(matches!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Tab(sibling.tab_id),
                name: "checks".into(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    top.wait_for("checks").await;
    assert!(matches!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Tab(current_tab),
                name: "work".into(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    top.wait_for("work").await;
    top.send(b"\x02d");
    top.wait_success().await;

    let config_directory = harness.root.path().join("home/.config/fut");
    fs::create_dir_all(&config_directory).unwrap();
    fs::write(
        config_directory.join("config.toml"),
        "[ui]\ntab_bar_position = \"bottom\"\nworkspace_sidebar_position = \"right\"\n",
    )
    .unwrap();

    let mut bottom = spawn_client();
    bottom.wait_for("work").await;
    bottom.wait_for("checks").await;
    bottom.wait_for("\x1b[24;1H").await;
    let bar_positioned_on_last_row = bottom.text().split("\x1b[24;1H").any(|suffix| {
        let nearby = &suffix[..suffix.len().min(200)];
        nearby.contains("work") || nearby.contains("checks")
    });
    assert!(
        bar_positioned_on_last_row,
        "bottom bar labels were not rendered on row 24: {:?}",
        bottom.text()
    );
    bottom.send(b"bottom-probe\n");
    bottom.wait_for("BOTTOM_SIZE_23_80").await;
    bottom.send(b"\x02d");
    bottom.wait_success().await;

    fs::write(
        config_directory.join("config.toml"),
        "[ui]\ntab_bar_position = \"sideways\"\n",
    )
    .unwrap();
    let invalid = harness
        .cli()
        .args(["pane", "attach", &current_pane.to_string()])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("parse Fut config"),
        "{}",
        String::from_utf8_lossy(&invalid.stderr)
    );
    let control = harness.cli().arg("list").output().unwrap();
    assert!(
        control.status.success(),
        "UI config affected a control command: {}",
        String::from_utf8_lossy(&control.stderr)
    );

    assert!(process_alive(sibling.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_workspace_sidebar_docks_navigates_and_collapses_responsively() {
    let main_script = r#"
printf 'ALPHA_READY\r\n'
while IFS= read -r line; do
  case "$line" in
    main) printf 'BRAVO_ACK\r\n' ;;
    size-main) set -- $(stty size); printf 'ALPHA_SIZE_%s_%s\r\n' "$1" "$2" ;;
    size-narrow) set -- $(stty size); printf 'NARROW_SIZE_%s_%s\r\n' "$1" "$2" ;;
  esac
done
"#;
    let harness = Harness::start_with(main_script, |root| {
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
    })
    .await;
    let linked = harness.root.path().join("linked");
    let ServerMessage::LocationOpened {
        selected: linked_target,
        disposition: fut::protocol::OpenDisposition::WorkspaceCreated,
    } = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some("feature".into()),
            cwd: linked,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'ZETA_READY\\r\\n'; while IFS= read -r line; do case \"$line\" in linked) printf 'ZETA_INPUT\\r\\n';; after-close) printf 'OMEGA_ACK\\r\\n';; size-linked) set -- $(stty size); printf 'ZETA_SIZE_%s_%s\\r\\n' \"$1\" \"$2\";; esac; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create peer workspace")
    };
    let snapshot = harness.resources().await;
    let main_workspace = &snapshot.sessions[0].workspaces[0];
    let main_pane = main_workspace.tabs[0].panes[0].id;

    let spawn_client = |columns: u16, pane: PaneId| {
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
                "stty rows 24 cols {columns}; exec '{}' --socket '{}' pane attach {pane}",
                env!("CARGO_BIN_EXE_fut"),
                harness.socket.display(),
            ));
        PtyChild::spawn(command)
    };

    let mut left = spawn_client(120, main_pane);
    left.wait_for("ALPHA_READY").await;
    left.wait_for("feature").await;
    left.send(b"size-main\n");
    left.wait_for("ALPHA_SIZE_23_96").await;
    left.send(b"\x02w");
    left.wait_for("↑↓ enter · esc").await;
    left.send(b"j\r");
    left.wait_for("ZETA_READY").await;
    left.send(b"linked\nsize-linked\n");
    left.wait_for("ZETA_INPUT").await;
    left.wait_for("ZETA_SIZE_23_96").await;
    left.send(b"\x02w");
    left.wait_for_count("↑↓ enter · esc", 2).await;
    left.send(b"k\r");
    left.send(b"main\n");
    left.wait_for("BRAVO_ACK").await;
    left.send(b"\x02d");
    left.wait_success().await;

    let config_directory = harness.root.path().join("home/.config/fut");
    fs::create_dir_all(&config_directory).unwrap();
    fs::write(
        config_directory.join("config.toml"),
        "[ui]\nworkspace_sidebar_position = \"right\"\n",
    )
    .unwrap();
    let mut right = spawn_client(120, linked_target.pane_id);
    right.wait_for("feature").await;
    right.wait_for("\x1b[2;97H").await;
    assert!(matches!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Workspace(linked_target.workspace_id),
                name: "feature λ".into(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    right.wait_for(" λ").await;
    right.send(b"size-linked\n");
    right.wait_for("ZETA_SIZE_23_96").await;
    right.send(b"\x02d");
    right.wait_success().await;

    let mut narrow = spawn_client(119, main_pane);
    narrow.wait_for("ALPHA_READY").await;
    narrow.send(b"size-narrow\n");
    narrow.wait_for("NARROW_SIZE_23_119").await;
    narrow.send(b"\x02w");
    narrow.wait_for("feature").await;
    narrow.wait_for("λ").await;
    narrow.send(b"q");
    narrow.send(b"size-narrow\n");
    narrow.wait_for_count("NARROW_SIZE_23_119", 2).await;
    narrow.send(b"\x02d");
    narrow.wait_success().await;

    let mut live_close = spawn_client(120, linked_target.pane_id);
    live_close.wait_for("feature").await;
    assert!(matches!(
        harness
            .control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Workspace(main_workspace.id),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    assert!(matches!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Workspace(linked_target.workspace_id),
                name: "feature done".into(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    live_close.wait_for("done").await;
    live_close.send(b"\x02w");
    live_close.wait_for("↑↓ enter · esc").await;
    live_close.send(b"k\r");
    live_close.send(b"after-close\n");
    live_close.wait_for("OMEGA_ACK").await;
    live_close.send(b"\x02d");
    live_close.wait_success().await;

    let linked_pid = linked_target.child_pid;
    assert!(process_alive(linked_pid));
    harness.shutdown().await;
    wait_for(DEADLINE, || !process_alive(linked_pid)).await;
}

#[tokio::test]
async fn malformed_ui_config_prevents_bare_fut_from_starting_or_opening_resources() {
    let root = tempfile::Builder::new()
        .prefix("fut-e2e-config-")
        .tempdir()
        .unwrap();
    let home = root.path().join("home");
    let runtime = root.path().join("runtime");
    let cwd = root.path().join("cwd");
    fs::create_dir_all(home.join(".config/fut")).unwrap();
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    fs::write(
        home.join(".config/fut/config.toml"),
        "[ui]\ntab_bar_position = \"sideways\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", &runtime)
        .env("FUT_RUNTIME_DIR", &runtime)
        .current_dir(&cwd)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("parse Fut config"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!runtime.join("fut.sock").exists());
    assert!(fs::read_dir(&cwd).unwrap().next().is_none());
}

#[tokio::test]
async fn public_client_transfers_focus_when_the_focused_pane_exits() {
    let harness = Harness::start(
        "printf 'EXIT_A_READY\r\n'; while IFS= read -r line; do [ \"$line\" = exit ] && exit 23; done",
    )
    .await;
    let resources = harness.resources().await;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let pane_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'EXIT_B_READY\r\n'; while IFS= read -r line; do [ \"$line\" = b ] && printf 'EXIT_B_INPUT\r\n'; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create focus replacement pane")
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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
            pane_a
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("EXIT_A_READY").await;
    client.wait_for("EXIT_B_READY").await;
    client.send(b"exit\n");
    client.wait_for_count("EXIT_B_READY", 2).await;
    client.send(b"b\n");
    client.wait_for("EXIT_B_INPUT").await;
    client.send(b"\x02d");
    client.wait_success().await;

    assert!(process_alive(pane_b.child_pid));
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
            "stty rows 24 cols 80; exec '{}' --socket '{}' terminal attach {}",
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
    assert_eq!(selected.focused, c);
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

#[tokio::test]
async fn public_rename_preserves_a_live_process_and_rejects_invalid_changes_atomically() {
    let harness = Harness::start(
        "printf 'RENAME_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = routed ] && printf 'RENAME_ROUTED\\r\\n'; done",
    ).await;
    let initial = harness.resources().await;
    let session_id = initial.sessions[0].id;
    let workspace_id = initial.sessions[0].workspaces[0].id;
    let tab_id = initial.sessions[0].workspaces[0].tabs[0].id;
    let pane_id = initial.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let terminal_id = initial.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let (mut attached, selected) =
        attach_once(&harness, TargetSelector::Terminal(terminal_id)).await;
    let child_pid = selected.child_pid;
    assert_eq!(selected.terminal_id, terminal_id);
    snapshot_containing(&mut attached, terminal_id, "RENAME_READY").await;

    let sibling_cwd = harness.root.path().join("rename-sibling");
    fs::create_dir(&sibling_cwd).unwrap();
    let ServerMessage::LocationOpened { .. } = harness
        .control_command(ClientMessage::OpenLocation {
            name: Some("rename-sibling".into()),
            cwd: sibling_cwd,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "while IFS= read -r line; do :; done".into()],
        })
        .await
    else {
        panic!("failed to create rename sibling")
    };
    let before = harness.resources().await;

    let run = |arguments: &[&str]| harness.cli().args(arguments).output().expect("run fut CLI");
    for arguments in [
        vec![
            "session".into(),
            "rename".into(),
            session_id.to_string(),
            "セッション 六".into(),
        ],
        vec![
            "workspace".into(),
            "rename".into(),
            workspace_id.to_string(),
            "作業 空間 λ".into(),
        ],
        vec![
            "tab".into(),
            "rename".into(),
            tab_id.to_string(),
            "タブ 雪 v6".into(),
        ],
    ] {
        let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        let output = run(&borrowed);
        assert!(
            output.status.success(),
            "rename failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "renamed=true"
        );
    }

    let after = harness.resources().await;
    assert_eq!(after.revision, before.revision + 3);
    let mut expected = before.clone();
    expected.revision += 3;
    expected.sessions[0].name = "セッション 六".into();
    expected.sessions[0].workspaces[0].name = "作業 空間 λ".into();
    expected.sessions[0].workspaces[0].tabs[0].name = "タブ 雪 v6".into();
    assert_eq!(
        after, expected,
        "rename changed identity, order, roots, or process structure"
    );
    assert!(process_alive(child_pid));

    send(
        &mut attached,
        ClientMessage::Input {
            bytes: b"routed\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut attached, terminal_id, "RENAME_ROUTED").await;
    assert_eq!(
        attach_error_once(
            &harness,
            TargetSelector::Session(SessionSelector::Name("cwd".into()))
        )
        .await,
        "not_found"
    );
    assert_eq!(
        attach_error_once(
            &harness,
            TargetSelector::Session(SessionSelector::Name("セッション 六".into()))
        )
        .await,
        "already_attached"
    );

    let listed = run(&["list"]);
    assert!(listed.status.success());
    let listed = String::from_utf8_lossy(&listed.stdout);
    for label in ["セッション 六", "作業 空間 λ", "タブ 雪 v6"] {
        assert!(
            listed.contains(label),
            "fresh list omitted {label:?}: {listed}"
        );
    }

    let no_op_target = tab_id.to_string();
    let no_op = run(&["tab", "rename", &no_op_target, "タブ 雪 v6"]);
    assert!(
        no_op.status.success(),
        "same-name rename failed: {}",
        String::from_utf8_lossy(&no_op.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&no_op.stdout).trim(),
        "renamed=true"
    );
    assert_eq!(harness.resources().await, after);

    let mut control = harness.connect().await.unwrap();
    assert!(matches!(
        hello(&mut control, ClientMode::Control, PROTOCOL_VERSION)
            .await
            .unwrap(),
        ServerMessage::Welcome { .. }
    ));
    let request_id = Uuid::new_v4();
    send_envelope(
        &mut control,
        Envelope {
            request_id: Some(request_id),
            message: ClientMessage::RenameTarget {
                selector: RenameSelector::Tab(tab_id),
                name: "タブ 雪 v6".into(),
            },
        },
    )
    .await;
    assert_eq!(
        receive_envelope(&mut control).await.unwrap(),
        Envelope {
            request_id: Some(request_id),
            message: ServerMessage::CommandCompleted {
                command: fut::protocol::AcknowledgedCommand::RenameTarget,
            },
        }
    );
    send(
        &mut attached,
        ClientMessage::RenameTarget {
            selector: RenameSelector::Tab(tab_id),
            name: "forbidden".into(),
        },
    )
    .await;
    assert!(
        matches!(receive_matching(&mut attached, |message| matches!(message, ServerMessage::Error { code, .. } if code == "control_only")).await,
        ServerMessage::Error { ref code, .. } if code == "control_only")
    );
    assert_eq!(harness.resources().await, after);

    let invalid = [
        vec![
            "pane".into(),
            "rename".into(),
            pane_id.to_string(),
            "no".into(),
        ],
        vec![
            "terminal".into(),
            "rename".into(),
            terminal_id.to_string(),
            "no".into(),
        ],
        vec!["rename".into(), "bogus:target".into(), "no".into()],
        vec![
            "session".into(),
            "rename".into(),
            Uuid::new_v4().to_string(),
            "no".into(),
        ],
        vec![
            "session".into(),
            "rename".into(),
            session_id.to_string(),
            " \t ".into(),
        ],
        vec![
            "session".into(),
            "rename".into(),
            session_id.to_string(),
            "rename-sibling".into(),
        ],
    ];
    for arguments in invalid {
        let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        let output = run(&borrowed);
        assert!(
            !output.status.success(),
            "invalid rename unexpectedly succeeded: {arguments:?}"
        );
        assert_eq!(
            harness.resources().await,
            after,
            "invalid rename mutated resources: {arguments:?}"
        );
    }

    harness.detach(&mut attached).await;
    drop(attached);
    let (mut renamed, resolved) = attach_once(
        &harness,
        TargetSelector::Session(SessionSelector::Name("セッション 六".into())),
    )
    .await;
    assert_eq!(
        (resolved.terminal_id, resolved.child_pid),
        (terminal_id, child_pid)
    );
    harness.detach(&mut renamed).await;
    drop(renamed);
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
    select_view_response(connection, selector).await.focused
}

async fn select_view_response(
    connection: &mut Connection,
    selector: TargetSelector,
) -> fut::protocol::SelectedView {
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
    receive_matching(connection, |message| {
        matches!(message, ServerMessage::Snapshot { terminal_id, .. } if *terminal_id == expected.terminal_id)
    })
    .await;
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
    (connection, selected.focused)
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

async fn resources_when(
    harness: &Harness,
    condition: impl Fn(&fut::resources::ResourceSnapshot) -> bool,
) -> fut::resources::ResourceSnapshot {
    time::timeout(DEADLINE, async {
        loop {
            let snapshot = harness.resources().await;
            if condition(&snapshot) {
                return snapshot;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("resource condition did not become true before timeout")
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: signal zero does not modify the target process.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

struct CompletionEnv(TempDir);

impl CompletionEnv {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("fut-completion-e2e-")
            .tempdir()
            .unwrap();
        fs::create_dir(root.path().join("home")).unwrap();
        Self(root)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
        command
            .env_clear()
            .env("HOME", self.0.path().join("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", self.0.path())
            .env("FUT_RUNTIME_DIR", self.0.path().join("runtime"))
            .env("TERM", "xterm-256color")
            .env("COMPLETE", "zsh");
        command
    }

    fn complete(&self, socket: &std::path::Path, words: &[&str]) -> std::process::Output {
        self.complete_at(socket, words, words.len() + 2)
    }

    fn complete_at(
        &self,
        socket: &std::path::Path,
        words: &[&str],
        index: usize,
    ) -> std::process::Output {
        self.command()
            .env("FUT_SOCKET", self.0.path().join("missing-env.sock"))
            .env("_CLAP_COMPLETE_INDEX", index.to_string())
            .args(["--", "fut", "--socket"])
            .arg(socket)
            .args(words)
            .output()
            .unwrap()
    }
}

fn zsh_dynamic(output: &std::process::Output) -> Vec<(String, String)> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .filter_map(|line| {
            let (value, help) = line.split_once(':')?;
            Uuid::parse_str(value).ok()?;
            Some((value.into(), help.into()))
        })
        .collect()
}

fn completion_values(output: &std::process::Output) -> Vec<String> {
    zsh_dynamic(output)
        .into_iter()
        .map(|candidate| candidate.0)
        .collect()
}

fn completion_help(output: &std::process::Output, id: impl ToString) -> String {
    let id = id.to_string();
    zsh_dynamic(output)
        .into_iter()
        .find_map(|(value, help)| (value == id).then_some(help))
        .unwrap_or_else(|| panic!("missing completion {id}"))
}

#[test]
fn zsh_completion_registration_is_compact_and_side_effect_free() {
    let env = CompletionEnv::new();
    let before = fs::read_dir(env.0.path()).unwrap().count();
    let output = env.command().output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let script = String::from_utf8(output.stdout).unwrap();
    assert!(script.starts_with("#compdef fut\n"));
    assert!(script.contains("COMPLETE=\"zsh\""));
    assert!(script.contains("_CLAP_COMPLETE_INDEX"));
    assert!(script.len() < 4_096);
    assert_eq!(fs::read_dir(env.0.path()).unwrap().count(), before);
    assert!(!env.0.path().join("runtime").exists());
}

#[tokio::test]
async fn process_completion_covers_live_resource_operations_and_refresh() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let env = CompletionEnv::new();
    let snapshot = harness.resources().await;
    let session = &snapshot.sessions[0];
    let workspace = &session.workspaces[0];
    let tab = &workspace.tabs[0];
    let pane = &tab.panes[0];
    let root = workspace.root.to_string_lossy();
    let session_help = format!("session {}", session.name);
    let workspace_help = format!("{session_help} › workspace {} — {root}", workspace.name);
    let tab_help = format!(
        "{session_help} › workspace {} › tab {} — {root}",
        workspace.name, tab.name
    );
    let pane_help = format!(
        "{session_help} › workspace {} › tab {} › pane 1 — {root}",
        workspace.name, tab.name
    );
    let cases = [
        (
            vec!["session", "attach", ""],
            session.id.to_string(),
            session_help.clone(),
        ),
        (
            vec!["session", "rename", ""],
            session.id.to_string(),
            session_help.clone(),
        ),
        (
            vec!["session", "close", ""],
            session.id.to_string(),
            session_help.clone(),
        ),
        (
            vec!["workspace", "attach", ""],
            workspace.id.to_string(),
            workspace_help.clone(),
        ),
        (
            vec!["workspace", "rename", ""],
            workspace.id.to_string(),
            workspace_help.clone(),
        ),
        (
            vec!["workspace", "close", ""],
            workspace.id.to_string(),
            workspace_help.clone(),
        ),
        (
            vec!["tab", "new", ""],
            workspace.id.to_string(),
            workspace_help,
        ),
        (
            vec!["tab", "attach", ""],
            tab.id.to_string(),
            tab_help.clone(),
        ),
        (
            vec!["tab", "rename", ""],
            tab.id.to_string(),
            tab_help.clone(),
        ),
        (
            vec!["tab", "close", ""],
            tab.id.to_string(),
            tab_help.clone(),
        ),
        (
            vec!["pane", "new", ""],
            tab.id.to_string(),
            tab_help.clone(),
        ),
        (
            vec!["pane", "attach", ""],
            pane.id.to_string(),
            pane_help.clone(),
        ),
        (
            vec!["pane", "close", ""],
            pane.id.to_string(),
            pane_help.clone(),
        ),
        (
            vec!["terminal", "attach", ""],
            pane.terminal_id.to_string(),
            format!(
                "{session_help} › workspace {} › tab {} › pane 1 › terminal process — {root}",
                workspace.name, tab.name
            ),
        ),
    ];
    for (words, id, help) in cases {
        let output = env.complete(&harness.socket, &words);
        assert_eq!(
            completion_values(&output),
            std::slice::from_ref(&id),
            "{words:?}"
        );
        assert_eq!(completion_help(&output, &id), help, "{words:?}");
        assert!(!id.contains(':'));
    }

    let created = harness
        .cli()
        .args(["tab", "new", &workspace.id.to_string(), "--name", "second"])
        .output()
        .unwrap();
    assert!(created.status.success());
    let expanded = harness.resources().await;
    let workspace = &expanded.sessions[0].workspaces[0];
    assert_eq!(workspace.tabs.len(), 2);
    assert!(
        completion_values(&env.complete(&harness.socket, &["session", "attach", ""])).is_empty()
    );
    assert!(
        completion_values(&env.complete(&harness.socket, &["workspace", "attach", ""])).is_empty()
    );
    assert_eq!(
        completion_values(&env.complete(&harness.socket, &["tab", "attach", ""])),
        workspace
            .tabs
            .iter()
            .map(|tab| tab.id.to_string())
            .collect::<Vec<_>>()
    );
    let first_tab = workspace.tabs[0].id;
    let first_pane = workspace.tabs[0].panes[0].id;
    let second_tab = workspace.tabs[1].id;
    let second_tab_pane = workspace.tabs[1].panes[0].id;
    let completion_revision = expanded.revision;
    assert_eq!(
        completion_values(&env.complete(&harness.socket, &["pane", "move", ""])),
        [first_pane.to_string(), second_tab_pane.to_string()]
    );
    assert_eq!(
        completion_values(&env.complete(
            &harness.socket,
            &["pane", "move", &first_pane.to_string(), ""],
        )),
        [second_tab.to_string()]
    );
    assert_eq!(
        completion_values(&env.complete(
            &harness.socket,
            &["pane", "move", &second_tab_pane.to_string(), ""],
        )),
        [first_tab.to_string()]
    );
    assert!(
        completion_values(&env.complete(
            &harness.socket,
            &["pane", "move", "00000000-0000-0000-0000-000000000000", "",],
        ))
        .is_empty()
    );
    assert_eq!(harness.resources().await.revision, completion_revision);

    let ambiguous_tab = workspace.tabs[0].id;
    let second_pane = harness
        .cli()
        .args(["pane", "new", &ambiguous_tab.to_string()])
        .output()
        .unwrap();
    assert!(second_pane.status.success());
    assert!(
        !completion_values(&env.complete(&harness.socket, &["tab", "attach", ""]))
            .contains(&ambiguous_tab.to_string())
    );
    assert!(
        completion_values(&env.complete(&harness.socket, &["pane", "new", ""]))
            .contains(&ambiguous_tab.to_string())
    );

    let stable_id = workspace.tabs[0].id;
    assert!(
        harness
            .cli()
            .args(["tab", "rename", &stable_id.to_string(), "fresh-name"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let refreshed = env.complete(&harness.socket, &["tab", "rename", ""]);
    assert!(completion_help(&refreshed, stable_id).contains("tab fresh-name"));
    assert!(completion_values(&refreshed).contains(&stable_id.to_string()));

    #[cfg(target_os = "macos")]
    {
        let registration = env.command().output().unwrap();
        assert!(registration.status.success());
        let registration_path = env.0.path().join("_fut");
        fs::write(&registration_path, registration.stdout).unwrap();
        let smoke_path = env.0.path().join("completion-smoke.zsh");
        fs::write(
            &smoke_path,
            r#"autoload -Uz compinit
compinit -D
source "$COMPLETION_SCRIPT"
function _describe() {
  local array_name="${@[-1]}"
  print -rl -- "${(@P)array_name}"
}
words=(fut --socket "$FUT_TEST_SOCKET" tab rename '')
CURRENT=${#words}
_clap_dynamic_completer_fut
"#,
        )
        .unwrap();
        let smoke = Command::new("/bin/zsh")
            .args(["-f", smoke_path.to_str().unwrap()])
            .env_clear()
            .env("HOME", env.0.path().join("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", env.0.path())
            .env("FUT_RUNTIME_DIR", env.0.path().join("missing-runtime"))
            .env("COMPLETION_SCRIPT", registration_path)
            .env("FUT_TEST_SOCKET", &harness.socket)
            .output()
            .unwrap();
        assert!(
            smoke.status.success(),
            "{}",
            String::from_utf8_lossy(&smoke.stderr)
        );
        let smoke = String::from_utf8(smoke.stdout).unwrap();
        assert!(smoke.contains(&stable_id.to_string()));
        assert!(smoke.contains("tab fresh-name"));
    }

    let missing = env.0.path().join("child.sock");
    let words = [
        "tab",
        "new",
        "",
        "--",
        "--socket",
        missing.to_str().unwrap(),
    ];
    assert_eq!(
        completion_values(&env.complete_at(&harness.socket, &words, 5)),
        [workspace.id.to_string()]
    );
    harness.shutdown().await;
}

#[test]
fn unavailable_completion_sockets_are_fast_silent_and_non_destructive() {
    for kind in ["missing", "stale", "stalled"] {
        let env = CompletionEnv::new();
        let runtime = env.0.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        let socket = runtime.join(format!("{kind}.sock"));
        let (mut release, mut thread) = (None, None);
        if kind == "stale" {
            drop(UnixListener::bind(&socket).unwrap());
        } else if kind == "stalled" {
            let listener = UnixListener::bind(&socket).unwrap();
            let (accepted_tx, accepted_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            thread = Some(std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                accepted_tx.send(()).unwrap();
                let _stream = stream;
                let _ = release_rx.recv();
            }));
            release = Some((release_tx, accepted_rx));
        }
        let existed = socket.exists();
        let start = Instant::now();
        let output = env.complete(&socket, &["session", "attach", "--"]);
        if let Some((_, accepted)) = &release {
            accepted.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{kind} completion hung"
        );
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert!(completion_values(&output).is_empty());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("--socket:")
        );
        assert_eq!(socket.exists(), existed);
        if let Some((release, _)) = release {
            release.send(()).unwrap();
        }
        if let Some(thread) = thread.take() {
            thread.join().unwrap();
        }
        assert_eq!(
            fs::read_dir(&runtime).unwrap().count(),
            usize::from(existed)
        );
    }
}
