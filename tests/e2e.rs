#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

use bytes::Bytes;
use fut::{
    domain::{ScreenSnapshot, TerminalId, TerminalSize},
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, ServerMessage, codec,
        decode_payload, encode_payload,
    },
    resources::SessionSelector,
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

impl Harness {
    async fn start(script: &str) -> Self {
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
        selector: Option<SessionSelector>,
    ) -> (Connection, TerminalId, u32) {
        let mut connection = self.connect().await.expect("connect interactive client");
        let welcome = hello(
            &mut connection,
            interactive_mode(selector),
            PROTOCOL_VERSION,
        )
        .await
        .expect("receive interactive welcome");
        let ServerMessage::Welcome {
            version,
            selected: Some(selected),
            ..
        } = welcome
        else {
            panic!("expected welcome, received {welcome:?}: {}", self.logs());
        };
        assert_eq!(version, PROTOCOL_VERSION);
        self.terminal_pid = Some(selected.child_pid);
        (connection, selected.terminal_id, selected.child_pid)
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
            self.control_command(ClientMessage::CloseSession { selector })
                .await,
            ServerMessage::CommandCompleted {
                command: fut::protocol::AcknowledgedCommand::CloseSession,
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
    let created = harness.control_command(ClientMessage::CreateSession {
        name: "project-b".into(),
        cwd: cwd_b.clone(),
        program: Some("/bin/sh".into()),
        argv: vec!["-c".into(), "printf 'B_READY\\r\\n'; while IFS= read -r line; do case \"$line\" in b) printf 'B_INPUT\\r\\n' ;; esac; done".into()],
    }).await;
    let ServerMessage::SessionCreated {
        selected: selected_b,
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
        .interactive_for(Some(SessionSelector::Name("cwd".into())))
        .await;
    let (mut client_b, terminal_b, pid_b) = harness
        .interactive_for(Some(SessionSelector::Id(selected_b.session_id)))
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
async fn rejected_session_creation_never_spawns_and_preserves_tree() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let before = harness.resources().await;
    let pid_file = harness.root.path().join("duplicate.pid");
    let marker_file = harness.root.path().join("duplicate.marker");
    let response = harness
        .control_command(ClientMessage::CreateSession {
            name: "cwd".into(),
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
    assert!(matches!(response, ServerMessage::Error { ref code, .. } if code == "duplicate"));
    let response = harness
        .control_command(ClientMessage::CreateSession {
            name: "different-name".into(),
            cwd: harness.root.path().join("cwd"),
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                format!("touch {}; sleep 60", marker_file.display()),
            ],
        })
        .await;
    assert!(matches!(response, ServerMessage::Error { ref code, .. } if code == "duplicate"));
    let after = harness.resources().await;
    assert_eq!(after, before);
    time::sleep(Duration::from_millis(100)).await;
    assert!(!pid_file.exists(), "rejected command wrote its PID file");
    assert!(
        !marker_file.exists(),
        "rejected command wrote its marker file"
    );

    let blank_marker = harness.root.path().join("blank.marker");
    let response = harness
        .control_command(ClientMessage::CreateSession {
            name: " \t ".into(),
            cwd: harness.root.path().join("cwd"),
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
        hello(&mut contender, interactive_mode(Some(SessionSelector::Name("cwd".into()))), PROTOCOL_VERSION).await.unwrap(),
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

fn interactive_mode(selector: Option<SessionSelector>) -> ClientMode {
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
    let frame = time::timeout(DEADLINE, connection.next())
        .await
        .expect("protocol receive timed out")?
        .expect("read server frame")
        .to_vec();
    Some(
        decode_payload::<Envelope<ServerMessage>>(&frame)
            .expect("decode server frame")
            .message,
    )
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
