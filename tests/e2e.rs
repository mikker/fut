#![cfg(unix)]

use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
        net::UnixListener,
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use bytes::Bytes;
use fut::{
    domain::{
        AgentReport, AgentState, CopyModeAction, CopyModeError, CopyModeMovement,
        MAX_SEARCH_QUERY_BYTES, MouseButton, MouseButtons, MouseEvent, MouseEventKind,
        MouseModifiers, MouseWheelDirection, PaneId, ScreenSnapshot, SearchDirection, SessionId,
        TabId, TerminalId, TerminalSize, WorkspaceId,
    },
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, PROTOCOL_VERSION_0_1,
        RenameSelector, SelectedTarget, ServerMessage, codec, decode_payload, encode_payload,
    },
    resources::{PresentationTokenTarget, SessionSelector, TargetSelector},
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

struct ChaosRng(u64);

impl ChaosRng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn index(&mut self, upper: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 as usize) % upper
    }
}

type RawConnection = Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>;

/// A client connection plus the materialized per-terminal grids it has
/// received. [`receive_envelope`] folds `Snapshot`/`SnapshotDelta` frames
/// into `grids` and hands callers back a synthesized `Snapshot` either way,
/// so every existing assertion can keep matching on `ServerMessage::Snapshot`
/// without knowing which one arrived on the wire.
struct Connection {
    raw: RawConnection,
    grids: HashMap<TerminalId, ScreenSnapshot>,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        Self {
            raw: Framed::new(stream, codec()),
            grids: HashMap::new(),
        }
    }
}

impl std::ops::Deref for Connection {
    type Target = RawConnection;

    fn deref(&self) -> &Self::Target {
        &self.raw
    }
}

impl std::ops::DerefMut for Connection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.raw
    }
}

/// Applies a delta on top of `grids`, panicking if it doesn't apply cleanly —
/// the test harness talks to one well-behaved daemon with no competing
/// clients, so an inapplicable delta means the protocol implementation
/// itself is broken.
fn apply_delta(
    grids: &mut HashMap<TerminalId, ScreenSnapshot>,
    terminal_id: TerminalId,
    delta: fut::domain::ScreenDelta,
) -> ScreenSnapshot {
    let mut screen = grids.remove(&terminal_id).unwrap_or_else(|| {
        panic!("test harness received a delta with no matching base grid for {terminal_id:?}")
    });
    screen.apply_delta(delta).unwrap_or_else(|error| {
        panic!("test harness received an invalid delta for {terminal_id:?}: {error}")
    });
    screen
}

/// Folds `Snapshot`/`SnapshotDelta` messages into `connection.grids`,
/// rewriting a delta into the equivalent full `Snapshot` so callers never
/// need to distinguish the two on the wire.
fn track_screen(connection: &mut Connection, message: ServerMessage) -> ServerMessage {
    match message {
        ServerMessage::Snapshot {
            terminal_id,
            screen,
        } => {
            connection.grids.insert(terminal_id, screen.clone());
            ServerMessage::Snapshot {
                terminal_id,
                screen,
            }
        }
        ServerMessage::SnapshotDelta { terminal_id, delta } => {
            let screen = apply_delta(&mut connection.grids, terminal_id, delta);
            connection.grids.insert(terminal_id, screen.clone());
            ServerMessage::Snapshot {
                terminal_id,
                screen,
            }
        }
        other => other,
    }
}

struct Harness {
    root: TempDir,
    socket: PathBuf,
    daemon: Child,
    terminal_pid: Option<u32>,
}

struct PtyChild {
    child: Child,
    input: Option<std::process::ChildStdin>,
    output: Arc<Mutex<PtyOutput>>,
    renderer: Mutex<PtyRenderer>,
}

#[derive(Default)]
struct PtyOutput {
    bytes: Vec<u8>,
    checkpoint: usize,
}

struct PtyRenderer {
    terminal: fut::terminal::bench::VtBench,
    parsed: usize,
    screen: Option<ScreenSnapshot>,
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
        let output = Arc::new(Mutex::new(PtyOutput::default()));
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
                    output
                        .lock()
                        .unwrap()
                        .bytes
                        .extend_from_slice(&bytes[..count]);
                }
            });
        }
        Self {
            child,
            input,
            output,
            renderer: Mutex::new(PtyRenderer {
                terminal: fut::terminal::bench::VtBench::new(SIZE).expect("test terminal"),
                parsed: 0,
                screen: None,
            }),
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        if let Err(error) = self
            .input
            .as_mut()
            .expect("PTY stdin is open")
            .write_all(bytes)
        {
            panic!("write PTY input: {error}; output={:?}", self.text());
        }
    }

    async fn wait_for(&mut self, needle: &str) {
        time::timeout(DEADLINE, async {
            loop {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "PTY child exited before {needle:?}; output={:?}",
                    self.text()
                );
                if self.sees(needle) {
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
                if text
                    .matches(needle)
                    .count()
                    .max(self.screen_text().matches(needle).count())
                    >= count
                {
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
        let output = self.output.lock().unwrap();
        String::from_utf8_lossy(&output.bytes[output.checkpoint..]).into_owned()
    }

    /// Whether the raw output or the rendered screen contains `needle`.
    /// Diff-based repaints may reuse cells already on screen, so a marker is
    /// not guaranteed to appear contiguously in the byte stream; rendering the
    /// output through the terminal reconstructs it.
    fn sees(&self, needle: &str) -> bool {
        self.text().contains(needle) || self.screen_text().contains(needle)
    }

    fn screen_text(&self) -> String {
        let mut renderer = self.renderer.lock().unwrap();
        let bytes = {
            let output = self.output.lock().unwrap();
            output.bytes[renderer.parsed..].to_vec()
        };
        renderer.parsed += bytes.len();
        let snapshot = renderer
            .terminal
            .feed(&bytes)
            .ok()
            .flatten()
            .or_else(|| renderer.terminal.snapshot().ok().flatten());
        if let Some(snapshot) = snapshot {
            renderer.screen = Some(snapshot);
        }
        renderer
            .screen
            .as_ref()
            .map(snapshot_text)
            .unwrap_or_default()
    }

    fn clear_output(&mut self) {
        // Bring the retained VT state up to date before advancing the raw
        // observation boundary. Future diff repaints can then reuse cells
        // that were written before this checkpoint without losing context.
        self.screen_text();
        let mut output = self.output.lock().unwrap();
        output.checkpoint = output.bytes.len();
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
        Self::start_configured(script, setup, None, false).await
    }

    async fn start_public_with(script: &str, setup: impl FnOnce(&std::path::Path)) -> Self {
        Self::start_configured(script, setup, None, true).await
    }

    async fn start_with_shell(script: &str, shell: &std::path::Path) -> Self {
        Self::start_configured(script, |_| {}, Some(shell), false).await
    }

    async fn start_configured(
        script: &str,
        setup: impl FnOnce(&std::path::Path),
        shell: Option<&std::path::Path>,
        public_readiness: bool,
    ) -> Self {
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
        let daemon = spawn_daemon(&root, &socket, &home, &runtime, &cwd, script, shell);

        let mut harness = Self {
            root,
            socket,
            daemon,
            terminal_pid: None,
        };
        if public_readiness {
            harness.wait_until_publicly_ready().await;
        } else {
            harness.wait_until_ready().await;
        }
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
            None,
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

    async fn wait_until_publicly_ready(&mut self) {
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
                if self
                    .cli()
                    .args(["daemon", "ping"])
                    .output()
                    .is_ok_and(|output| output.status.success())
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
        UnixStream::connect(&self.socket).await.map(Connection::new)
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
        let ServerMessage::Resources { snapshot, .. } =
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
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
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

fn git_stdout(cwd: &std::path::Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn project_trust_cli(root: &std::path::Path, operation: &str, name: &str) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("PATH", "/usr/bin:/bin")
        .arg("--json")
        .arg("project")
        .arg(operation)
        .arg(name)
        .output()
        .expect("run daemonless project trust command");
    assert!(
        output.status.success(),
        "project {operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse project trust JSON")
}

fn spawn_daemon(
    root: &TempDir,
    socket: &PathBuf,
    home: &PathBuf,
    runtime: &PathBuf,
    cwd: &PathBuf,
    script: &str,
    shell: Option<&std::path::Path>,
) -> Child {
    let stdout =
        fs::File::create(root.path().join("daemon.stdout")).expect("create daemon stdout capture");
    let stderr =
        fs::File::create(root.path().join("daemon.stderr")).expect("create daemon stderr capture");
    let test_bin = root.path().join("bin");
    let path = if test_bin.is_dir() {
        format!("{}:/usr/bin:/bin", test_bin.display())
    } else {
        "/usr/bin:/bin".into()
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
    command
        .env_clear()
        .env("HOME", home)
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("PATH", path)
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
        .stderr(Stdio::from(stderr));
    if let Some(shell) = shell {
        command.env("SHELL", shell);
    }
    command.spawn().expect("start real fut daemon binary")
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
async fn agent_reports_flow_from_scoped_cli_into_authoritative_snapshots() {
    let harness = Harness::start("env > fut-env; while IFS= read -r line; do :; done").await;
    let initial = harness.resources().await;
    let session = &initial.sessions[0];
    let workspace = &session.workspaces[0];
    let tab = &workspace.tabs[0];
    let pane = tab.panes[0].clone();
    let terminal_id = pane.terminal_id;
    let env_path = harness.root.path().join("cwd/fut-env");
    wait_for(DEADLINE, || {
        fs::read_to_string(&env_path).is_ok_and(|contents| contents.contains("FUT_TERMINAL_ID="))
    })
    .await;
    let child_env = fs::read_to_string(env_path).unwrap();
    for expected in [
        format!("FUT_SESSION_ID={}", session.id),
        format!("FUT_WORKSPACE_ID={}", workspace.id),
        format!("FUT_TAB_ID={}", tab.id),
        format!("FUT_PANE_ID={}", pane.id),
        format!("FUT_TERMINAL_ID={terminal_id}"),
        format!("FUT_SOCKET={}", harness.socket.display()),
    ] {
        assert!(child_env.lines().any(|line| line == expected), "{expected}");
    }

    let working = harness
        .cli()
        .env("FUT_TERMINAL_ID", terminal_id.to_string())
        .args(["terminal", "report", "working"])
        .output()
        .unwrap();
    assert!(
        working.status.success(),
        "{}",
        String::from_utf8_lossy(&working.stderr)
    );
    let snapshot = harness.resources().await;
    assert_eq!(
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .state,
        AgentState::Working
    );

    let completed = harness
        .cli()
        .env("FUT_TERMINAL_ID", terminal_id.to_string())
        .args(["terminal", "report", "completed"])
        .output()
        .unwrap();
    assert!(completed.status.success());
    let snapshot = harness.resources().await;
    let activity = snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
        .activity
        .clone();
    assert_eq!(activity.state, AgentState::Idle);
    assert_eq!(activity.last_event.unwrap().kind, AgentReport::Completed);

    harness.shutdown().await;
}

#[tokio::test]
async fn codex_node_wrapper_screen_detection_tracks_working_then_idle() {
    let script = r#"
mkfifo codex
( exec > codex
  printf '› Build it\r\n\r\n◦ Working (3s • esc to interrupt)\r\n'
  sleep 2
  printf '\r\n• Implemented the change.\r\n\r\n› \r\n'
  sleep 5
) &
exec ./node ./codex
"#;
    let harness = Harness::start_with(script, |root| {
        let source = root.join("codex-fixture.c");
        fs::write(
            &source,
            "#include <fcntl.h>\n#include <unistd.h>\nint main(int c, char **v) { char b[1024]; int f = open(v[1], O_RDONLY); ssize_t n; while ((n = read(f, b, sizeof b)) > 0) write(1, b, n); sleep(30); }\n",
        )
        .unwrap();
        let status = Command::new("cc")
            .arg(&source)
            .arg("-o")
            .arg(root.join("cwd/node"))
            .status()
            .expect("compile fake Node runtime");
        assert!(status.success());
    })
    .await;

    fn activity(snapshot: &fut::resources::ResourceSnapshot) -> &fut::domain::AgentActivity {
        &snapshot.sessions[0].workspaces[0].tabs[0].panes[0].activity
    }
    time::timeout(DEADLINE, async {
        loop {
            let snapshot = harness.resources().await;
            if activity(&snapshot).state == AgentState::Working {
                assert_eq!(
                    activity(&snapshot).detection.as_ref().unwrap().agent,
                    "codex"
                );
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fake Codex was not detected working: {}", harness.logs()));

    time::timeout(DEADLINE, async {
        loop {
            let snapshot = harness.resources().await;
            if activity(&snapshot).state == AgentState::Idle
                && activity(&snapshot)
                    .detection
                    .as_ref()
                    .is_some_and(|detection| detection.rule == "idle_fallback")
            {
                assert_eq!(
                    activity(&snapshot).last_event.as_ref().unwrap().kind,
                    AgentReport::Completed
                );
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fake Codex did not return idle: {}", harness.logs()));

    harness.shutdown().await;
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
async fn terminal_output_read_and_wait_are_bounded_event_driven_and_typed() {
    let script = r#"
printf '\033[31mBOOT-雪\033[0m\r\n'
while IFS= read -r line; do
  case "$line" in
    future) printf 'future-λ-42\r\n' ;;
    alt) printf '\033[?1049hALT-SCREEN\r\n' ;;
    exit) printf 'FINAL-BEFORE-EXIT\r\n'; break ;;
  esac
done
"#;
    let mut harness = Harness::start(script).await;
    let resources = harness.resources().await;
    let terminal_id = resources.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let terminal = terminal_id.to_string();

    let boot = harness
        .cli()
        .args([
            "terminal",
            "wait-output",
            &terminal,
            "--literal",
            "BOOT-雪",
            "--timeout",
            "1s",
        ])
        .output()
        .unwrap();
    assert!(
        boot.status.success(),
        "{}",
        String::from_utf8_lossy(&boot.stderr)
    );

    let read = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "read",
            &terminal,
            "--source",
            "recent-unwrapped",
            "--lines",
            "30",
        ])
        .output()
        .unwrap();
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    let read: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(read["version"], 1);
    assert_eq!(read["command"], "terminal.read");
    assert_eq!(read["result"]["version"], 1);
    assert_eq!(read["result"]["source"], "recent_unwrapped");
    assert!(
        read["result"]["text"].as_str().unwrap().contains("BOOT-雪"),
        "{read}"
    );
    assert!(read["result"]["returned_rows"].as_u64().unwrap() <= 30);

    let waiter = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "wait-output",
            &terminal,
            "--regex",
            "future-λ-[0-9]+",
            "--timeout",
            "2s",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let run = harness
        .cli()
        .args(["terminal", "run", &terminal, "future"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let waited = waiter.wait_with_output().unwrap();
    assert!(
        waited.status.success(),
        "{}",
        String::from_utf8_lossy(&waited.stderr)
    );
    let waited: serde_json::Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited["command"], "terminal.wait-output");
    assert_eq!(waited["result"]["match"]["text"], "future-λ-42");
    let start = waited["result"]["match"]["start"].as_u64().unwrap() as usize;
    let end = waited["result"]["match"]["end"].as_u64().unwrap() as usize;
    let text = waited["result"]["output"]["text"].as_str().unwrap();
    assert_eq!(&text[start..end], "future-λ-42");

    for (arguments, code) in [
        (vec!["--regex", "(", "--timeout", "1s"], "invalid_regex"),
        (
            vec!["--literal", "never-produced", "--timeout", "20ms"],
            "output_timeout",
        ),
    ] {
        let failed = harness
            .cli()
            .args(["--json", "terminal", "wait-output", &terminal])
            .args(arguments)
            .output()
            .unwrap();
        assert!(!failed.status.success());
        let failed: serde_json::Value = serde_json::from_slice(&failed.stderr).unwrap();
        assert_eq!(failed["error"]["code"], code);
    }

    assert!(
        harness
            .cli()
            .args(["terminal", "run", &terminal, "alt"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let visible_alt = harness
        .cli()
        .args([
            "terminal",
            "wait-output",
            &terminal,
            "--literal",
            "ALT-SCREEN",
            "--timeout",
            "1s",
            "--source",
            "visible",
        ])
        .output()
        .unwrap();
    assert!(
        visible_alt.status.success(),
        "{}",
        String::from_utf8_lossy(&visible_alt.stderr)
    );
    let historical_alt = harness
        .cli()
        .args([
            "--json", "terminal", "read", &terminal, "--source", "recent",
        ])
        .output()
        .unwrap();
    assert!(!historical_alt.status.success());
    let historical_alt: serde_json::Value = serde_json::from_slice(&historical_alt.stderr).unwrap();
    assert_eq!(historical_alt["error"]["code"], "alternate_screen");

    let exiting = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "wait-output",
            &terminal,
            "--literal",
            "not-before-exit",
            "--timeout",
            "2s",
            "--source",
            "visible",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let final_output = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "wait-output",
            &terminal,
            "--literal",
            "FINAL-BEFORE-EXIT",
            "--timeout",
            "2s",
            "--source",
            "visible",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert!(
        harness
            .cli()
            .args(["terminal", "run", &terminal, "exit"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let exited = exiting.wait_with_output().unwrap();
    assert!(!exited.status.success());
    let exited: serde_json::Value = serde_json::from_slice(&exited.stderr).unwrap();
    assert_eq!(exited["error"]["code"], "terminal_exited");
    let final_output = final_output.wait_with_output().unwrap();
    assert!(
        final_output.status.success(),
        "{}",
        String::from_utf8_lossy(&final_output.stderr)
    );
    let final_output: serde_json::Value = serde_json::from_slice(&final_output.stdout).unwrap();
    assert_eq!(final_output["result"]["match"]["text"], "FINAL-BEFORE-EXIT");
    harness.wait_until_exited().await;
}

#[tokio::test]
async fn isolated_hollywood_overload_journey_stays_controllable_and_closes_cleanly() {
    let mut harness = Harness::start(
        "printf 'CONTROL_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = probe ] && printf 'CONTROL_ALIVE\\r\\n'; done",
    )
    .await;
    let initial = harness.resources().await;
    let anchor = initial.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let control_terminal = initial.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let (mut attached, _, _) = harness.interactive().await;

    let programs = [
        r#"i=0; while :; do i=$((i+1)); printf '\033[H\033[32mMATRIX GRID %06d\033[0m\r\n' "$i"; head -c 32768 /dev/zero; sleep 0.005; done"#,
        r#"printf '\033[?1049h'; i=0; while :; do i=$((i+1)); printf '\033[H\033[36mNEURAL SCAN %06d\033[0m\r\n\033[2K[||||||||||||||||||||||||||||||||]\r\n' "$i"; sleep 0.01; done"#,
        r#"i=0; while :; do i=$((i+1)); printf '\033[33mACCESS %06d node=zion status=BREACH vector=0xDEADBEEF\033[0m\r\n' "$i"; sleep 0.005; done"#,
    ];
    let mut overload_panes = Vec::new();
    let mut overload_terminals = Vec::new();
    for (index, program) in programs.iter().enumerate() {
        let direction = if index == 0 { "right" } else { "down" };
        let split = harness
            .cli()
            .args([
                "--json",
                "pane",
                "split",
                &anchor.to_string(),
                direction,
                "--",
                "/bin/sh",
                "-c",
                program,
            ])
            .output()
            .unwrap();
        assert!(
            split.status.success(),
            "overload pane {index} failed: {}",
            String::from_utf8_lossy(&split.stderr)
        );
        let split: Value = serde_json::from_slice(&split.stdout).unwrap();
        overload_panes.push(
            split["result"]["selected"]["pane_id"]
                .as_str()
                .unwrap()
                .parse::<PaneId>()
                .unwrap(),
        );
        overload_terminals.push(
            split["result"]["selected"]["terminal_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }

    time::sleep(Duration::from_millis(150)).await;
    let ping_started = Instant::now();
    assert!(matches!(
        harness.control_command(ClientMessage::Ping).await,
        ServerMessage::Pong { .. }
    ));
    assert!(
        ping_started.elapsed() < Duration::from_secs(2),
        "control ping stalled under overload"
    );
    for terminal in &overload_terminals {
        let read_started = Instant::now();
        let read = harness
            .cli()
            .args([
                "--json", "terminal", "read", terminal, "--source", "visible",
            ])
            .output()
            .unwrap();
        assert!(
            read.status.success(),
            "overload read failed: {}",
            String::from_utf8_lossy(&read.stderr)
        );
        let read_elapsed = read_started.elapsed();
        assert!(
            read_elapsed < Duration::from_secs(2),
            "terminal {terminal} read stalled under overload for {read_elapsed:?}"
        );
    }

    for pane in overload_panes.into_iter().rev() {
        let close_started = Instant::now();
        let closed = harness
            .cli()
            .args(["--json", "pane", "close", &pane.to_string()])
            .output()
            .unwrap();
        assert!(
            closed.status.success(),
            "overload close failed: {}",
            String::from_utf8_lossy(&closed.stderr)
        );
        assert!(
            close_started.elapsed() < Duration::from_secs(5),
            "overload pane close exceeded its deadline"
        );
    }
    let resources = harness.resources().await;
    assert_eq!(resources.sessions[0].workspaces[0].tabs[0].panes.len(), 1);

    assert!(
        harness
            .cli()
            .args(["terminal", "run", &control_terminal.to_string(), "probe"])
            .output()
            .unwrap()
            .status
            .success()
    );
    snapshot_containing(&mut attached, control_terminal, "CONTROL_ALIVE").await;
    drop(attached);
    let (mut reattached, terminal, _) = harness.interactive().await;
    assert_eq!(terminal, control_terminal);
    harness.detach(&mut reattached).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn agent_native_coordination_journey_uses_only_public_background_commands() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;

    let listed = harness.cli().args(["--json", "list"]).output().unwrap();
    assert!(
        listed.status.success(),
        "list failed: {}\n{}",
        String::from_utf8_lossy(&listed.stdout),
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let session = &listed["result"]["sessions"][0];
    let workspace = &session["workspaces"][0];
    let tab = &workspace["tabs"][0];
    let anchor = &tab["panes"][0];
    let session_id = session["id"].as_str().unwrap();
    let workspace_id = workspace["id"].as_str().unwrap();
    let tab_id = tab["id"].as_str().unwrap();
    let anchor_pane_id = anchor["id"].as_str().unwrap();
    let anchor_terminal_id = anchor["terminal_id"].as_str().unwrap();

    let context = harness
        .cli()
        .env("FUT_SESSION_ID", session_id)
        .env("FUT_WORKSPACE_ID", workspace_id)
        .env("FUT_TAB_ID", tab_id)
        .env("FUT_PANE_ID", anchor_pane_id)
        .env("FUT_TERMINAL_ID", anchor_terminal_id)
        .args(["--json", "context"])
        .output()
        .unwrap();
    assert!(
        context.status.success(),
        "context failed: {}\n{}",
        String::from_utf8_lossy(&context.stdout),
        String::from_utf8_lossy(&context.stderr)
    );
    let context: Value = serde_json::from_slice(&context.stdout).unwrap();
    assert_eq!(context["result"]["target"]["pane"]["id"], anchor_pane_id);
    assert_eq!(
        context["result"]["target"]["terminal"]["id"],
        anchor_terminal_id
    );

    let binary = env!("CARGO_BIN_EXE_fut");
    let fake_agent = format!(
        r#"'{binary}' agent report idle --source journey --agent-session-id journey-session --turn-id setup
printf 'JOURNEY_READY\r\n'
turn=0
while IFS= read -r prompt; do
  turn=$((turn + 1))
  case "$prompt" in
    raw-command)
      printf 'RAW_RESULT_λ\r\n'
      ;;
    complete*)
      '{binary}' agent report working --source journey --agent-session-id journey-session --turn-id "turn-$turn"
      printf 'COMPLETED_RESULT:%s\r\n' "$prompt"
      '{binary}' agent report completed --source journey --agent-session-id journey-session --turn-id "turn-$turn"
      ;;
    blocked*)
      '{binary}' agent report working --source journey --agent-session-id journey-session --turn-id "turn-$turn"
      printf 'BLOCKED_RESULT:%s\r\n' "$prompt"
      '{binary}' agent report blocked --source journey --agent-session-id journey-session --turn-id "turn-$turn"
      ;;
  esac
done"#
    );
    let split = harness
        .cli()
        .args([
            "--json",
            "pane",
            "split",
            anchor_pane_id,
            "right",
            "--",
            "/bin/sh",
            "-c",
        ])
        .arg(&fake_agent)
        .output()
        .unwrap();
    assert!(
        split.status.success(),
        "split failed: {}\n{}",
        String::from_utf8_lossy(&split.stdout),
        String::from_utf8_lossy(&split.stderr)
    );
    let split: Value = serde_json::from_slice(&split.stdout).unwrap();
    let pane_id = split["result"]["selected"]["pane_id"].as_str().unwrap();
    let terminal_id = split["result"]["selected"]["terminal_id"].as_str().unwrap();
    assert_eq!(split["result"]["anchor_pane_id"], anchor_pane_id);

    let ready = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "wait-output",
            terminal_id,
            "--literal",
            "JOURNEY_READY",
            "--timeout",
            "2s",
        ])
        .output()
        .unwrap();
    assert!(
        ready.status.success(),
        "agent readiness failed: {}\n{}",
        String::from_utf8_lossy(&ready.stdout),
        String::from_utf8_lossy(&ready.stderr)
    );

    let agents = harness
        .cli()
        .args(["--json", "agent", "list"])
        .output()
        .unwrap();
    assert!(agents.status.success());
    let agents: Value = serde_json::from_slice(&agents.stdout).unwrap();
    assert!(
        agents["result"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["terminal_id"] == terminal_id)
    );

    let raw = harness
        .cli()
        .args(["--json", "terminal", "run", terminal_id, "raw-command"])
        .output()
        .unwrap();
    assert!(
        raw.status.success(),
        "raw command failed: {}\n{}",
        String::from_utf8_lossy(&raw.stdout),
        String::from_utf8_lossy(&raw.stderr)
    );
    let raw_result = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "wait-output",
            terminal_id,
            "--regex",
            "RAW_RESULT_.",
            "--timeout",
            "2s",
        ])
        .output()
        .unwrap();
    assert!(
        raw_result.status.success(),
        "raw output wait failed: {}\n{}",
        String::from_utf8_lossy(&raw_result.stdout),
        String::from_utf8_lossy(&raw_result.stderr)
    );
    let raw_read = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "read",
            terminal_id,
            "--source",
            "recent-unwrapped",
            "--lines",
            "40",
        ])
        .output()
        .unwrap();
    assert!(raw_read.status.success());
    let raw_read: Value = serde_json::from_slice(&raw_read.stdout).unwrap();
    assert!(
        raw_read["result"]["text"]
            .as_str()
            .unwrap()
            .contains("RAW_RESULT_λ")
    );
    assert!(raw_read["result"]["returned_rows"].as_u64().unwrap() <= 40);

    let completed = harness
        .cli()
        .args([
            "--json",
            "agent",
            "prompt",
            terminal_id,
            "complete revised request",
            "--wait",
            "--timeout",
            "2s",
        ])
        .output()
        .unwrap();
    assert!(
        completed.status.success(),
        "completed prompt failed: {}\n{}",
        String::from_utf8_lossy(&completed.stdout),
        String::from_utf8_lossy(&completed.stderr)
    );
    let completed: Value = serde_json::from_slice(&completed.stdout).unwrap();
    assert_eq!(completed["result"]["activity"]["state"], "idle");
    assert_eq!(
        completed["result"]["activity"]["last_event"]["kind"],
        "completed"
    );
    assert!(
        completed["result"]["barrier_revision"].as_u64().unwrap()
            < completed["result"]["working_revision"].as_u64().unwrap()
    );
    assert!(
        completed["result"]["working_revision"].as_u64().unwrap()
            < completed["result"]["activity"]["revision"]
                .as_u64()
                .unwrap()
    );

    let blocked = harness
        .cli()
        .args([
            "--json",
            "agent",
            "prompt",
            terminal_id,
            "blocked revised request",
            "--wait",
            "--timeout",
            "2s",
        ])
        .output()
        .unwrap();
    assert!(
        blocked.status.success(),
        "blocked prompt failed: {}\n{}",
        String::from_utf8_lossy(&blocked.stdout),
        String::from_utf8_lossy(&blocked.stderr)
    );
    let blocked: Value = serde_json::from_slice(&blocked.stdout).unwrap();
    assert_eq!(blocked["result"]["activity"]["state"], "blocked");
    assert_eq!(
        blocked["result"]["activity"]["last_event"]["kind"],
        "blocked"
    );
    assert!(
        blocked["result"]["barrier_revision"].as_u64().unwrap()
            < blocked["result"]["working_revision"].as_u64().unwrap()
    );
    assert!(
        blocked["result"]["working_revision"].as_u64().unwrap()
            < blocked["result"]["activity"]["revision"].as_u64().unwrap()
    );

    let result = harness
        .cli()
        .args([
            "--json",
            "agent",
            "read",
            terminal_id,
            "--source",
            "recent-unwrapped",
            "--lines",
            "60",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "agent result read failed: {}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let result: Value = serde_json::from_slice(&result.stdout).unwrap();
    let output = result["result"]["output"]["text"].as_str().unwrap();
    assert!(output.contains("COMPLETED_RESULT:complete revised request"));
    assert!(output.contains("BLOCKED_RESULT:blocked revised request"));
    assert_eq!(result["result"]["agent"]["activity"]["state"], "blocked");

    let closed = harness
        .cli()
        .args(["--json", "pane", "close", pane_id])
        .output()
        .unwrap();
    assert!(
        closed.status.success(),
        "pane cleanup failed: {}\n{}",
        String::from_utf8_lossy(&closed.stdout),
        String::from_utf8_lossy(&closed.stderr)
    );
    let closed: Value = serde_json::from_slice(&closed.stdout).unwrap();
    assert_eq!(closed["result"]["pane_id"], pane_id);

    let exited = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "wait-output",
            terminal_id,
            "--literal",
            "output-after-close",
            "--timeout",
            "2s",
        ])
        .output()
        .unwrap();
    assert!(!exited.status.success());
    assert!(exited.stdout.is_empty());
    let exited: Value = serde_json::from_slice(&exited.stderr).unwrap();
    assert_eq!(exited["error"]["code"], "terminal_exited");

    harness.shutdown().await;
}

#[tokio::test]
async fn agent_cli_composes_lifecycle_input_and_bounded_output_without_stale_idle_races() {
    let binary = env!("CARGO_BIN_EXE_fut");
    let script = format!(
        "while IFS= read -r line; do '{binary}' agent report working --source e2e --turn-id turn-1; '{binary}' agent report completed --source e2e --turn-id turn-1; done"
    );
    let harness = Harness::start(&script).await;
    let resources = harness.resources().await;
    let terminal_id = resources.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let terminal = terminal_id.to_string();

    let empty = harness
        .cli()
        .args(["--json", "agent", "list"])
        .output()
        .unwrap();
    assert!(empty.status.success());
    let empty: Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert!(empty["result"]["agents"].as_array().unwrap().is_empty());

    let reported = harness
        .cli()
        .args([
            "--json",
            "agent",
            "report",
            "idle",
            "--terminal-id",
            &terminal,
            "--source",
            "e2e",
            "--agent-session-id",
            "session-1",
            "--turn-id",
            "setup",
        ])
        .output()
        .unwrap();
    assert!(
        reported.status.success(),
        "{}",
        String::from_utf8_lossy(&reported.stderr)
    );

    let listed = harness
        .cli()
        .args(["--json", "agent", "list"])
        .output()
        .unwrap();
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["result"]["agents"].as_array().unwrap().len(), 1);
    assert_eq!(listed["result"]["agents"][0]["terminal_id"], terminal);
    assert_eq!(listed["result"]["agents"][0]["available"], true);
    let got = harness
        .cli()
        .args(["--json", "agent", "get", &terminal])
        .output()
        .unwrap();
    assert!(got.status.success());
    let got: Value = serde_json::from_slice(&got.stdout).unwrap();
    assert_eq!(
        got["result"]["agent"]["activity"]["integration"]["source"],
        "e2e"
    );

    // The integration emits Working and Completed back-to-back. The prompt
    // waiter must consume both lifecycle events rather than coalescing to the
    // latest idle snapshot and incorrectly satisfying from the setup report.
    let prompted = harness
        .cli()
        .args([
            "--json",
            "agent",
            "prompt",
            &terminal,
            "hello λ",
            "--wait",
            "--timeout",
            "2s",
        ])
        .output()
        .unwrap();
    assert!(
        prompted.status.success(),
        "{}",
        String::from_utf8_lossy(&prompted.stderr)
    );
    let prompted: Value = serde_json::from_slice(&prompted.stdout).unwrap();
    assert_eq!(
        prompted["result"]["activity"]["last_event"]["kind"],
        "completed"
    );
    let barrier = prompted["result"]["barrier_revision"].as_u64().unwrap();
    let working = prompted["result"]["working_revision"].as_u64().unwrap();
    let completed = prompted["result"]["activity"]["revision"].as_u64().unwrap();
    assert!(barrier < working && working < completed);

    let read = harness
        .cli()
        .args([
            "--json",
            "agent",
            "read",
            &terminal,
            "--source",
            "recent-unwrapped",
            "--lines",
            "20",
        ])
        .output()
        .unwrap();
    assert!(read.status.success());
    let read: Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(read["result"]["agent"]["available"], true);
    assert_eq!(read["result"]["output"]["requested_rows"], 20);

    assert!(matches!(
        harness
            .control_command(ClientMessage::ReportAgent {
                terminal_id,
                report: AgentReport::Working,
                metadata: Default::default(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    let timed_out = harness
        .cli()
        .args(["--json", "agent", "wait", &terminal, "--timeout", "25ms"])
        .output()
        .unwrap();
    assert!(!timed_out.status.success());
    let timed_out: Value = serde_json::from_slice(&timed_out.stderr).unwrap();
    assert_eq!(timed_out["error"]["code"], "agent_timeout");
    let busy = harness
        .cli()
        .args(["--json", "agent", "prompt", &terminal, "do not steer"])
        .output()
        .unwrap();
    assert!(!busy.status.success());
    let busy: Value = serde_json::from_slice(&busy.stderr).unwrap();
    assert_eq!(busy["error"]["code"], "agent_busy");

    assert!(matches!(
        harness
            .control_command(ClientMessage::ReportAgent {
                terminal_id,
                report: AgentReport::Blocked,
                metadata: Default::default(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    let blocked = harness
        .cli()
        .args(["--json", "agent", "wait", &terminal, "--timeout", "1s"])
        .output()
        .unwrap();
    assert!(blocked.status.success());
    let blocked: Value = serde_json::from_slice(&blocked.stdout).unwrap();
    assert_eq!(blocked["result"]["activity"]["state"], "blocked");

    let exit_cwd = harness.root.path().join("exiting-agent");
    fs::create_dir(&exit_cwd).unwrap();
    let ServerMessage::LocationOpened {
        selected: exiting, ..
    } = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
            name: Some("exiting-agent".into()),
            cwd: exit_cwd,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "IFS= read -r line; exit 7".into()],
        })
        .await
    else {
        panic!("failed to create exiting agent terminal")
    };
    assert!(matches!(
        harness
            .control_command(ClientMessage::ReportAgent {
                terminal_id: exiting.terminal_id,
                report: AgentReport::Idle,
                metadata: Default::default(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    let exiting_id = exiting.terminal_id.to_string();
    let exited = harness
        .cli()
        .args([
            "--json",
            "agent",
            "prompt",
            &exiting_id,
            "bye",
            "--wait",
            "--timeout",
            "2s",
        ])
        .output()
        .unwrap();
    assert!(!exited.status.success());
    let exited: Value = serde_json::from_slice(&exited.stderr).unwrap();
    assert_eq!(exited["error"]["code"], "terminal_exited");

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
        .args(["open", "--background"])
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
    let tab_pane = created
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pane="))
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

    let tabs = harness
        .cli()
        .args(["tab", "list", workspace])
        .output()
        .unwrap();
    assert!(
        tabs.status.success(),
        "{}",
        String::from_utf8_lossy(&tabs.stderr)
    );
    let tabs = String::from_utf8(tabs.stdout).unwrap();
    assert!(tabs.contains("revision="));
    let tab = tabs
        .lines()
        .find_map(|line| line.strip_prefix("tab "))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    assert!(tabs.contains(&format!("layout={tab_pane}")), "{tabs}");

    let panes: Value = serde_json::from_slice(
        &harness
            .cli()
            .args(["pane", "list", &tab, "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(panes["command"], "pane.list");
    assert_eq!(panes["result"]["tab_id"], tab);
    assert_eq!(panes["result"]["layout"]["pane_id"], tab_pane);
    assert_eq!(panes["result"]["panes"][0]["id"], tab_pane);

    let missing = harness
        .cli()
        .args(["pane", "list", &Uuid::new_v4().to_string(), "--json"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    let missing: Value = serde_json::from_slice(&missing.stderr).unwrap();
    assert_eq!(missing["error"]["code"], "not_found");

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
async fn configured_workspace_hooks_observe_committed_lifecycle_in_order() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let extension = root.join("extension");
        fs::create_dir(&extension).unwrap();
        fs::write(
            extension.join("fut-extension.toml"),
r#"
api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
capabilities = ["hooks"]
id = "lifecycle"
[hooks]
"session.created" = ["./hook"]
"workspace.created" = ["./hook"]
"workspace.renamed" = ["./hook"]
"workspace.closed" = ["./hook"]
"#,
        )
        .unwrap();
        let hook = extension.join("hook");
        fs::write(
            &hook,
            "#!/bin/sh\nprintf '%s|%s|%s|%s|%s\\n' \"$FUT_EVENT\" \"$FUT_EVENT_VERSION\" \"$FUT_SESSION_ID\" \"$FUT_WORKSPACE_ID\" \"$FUT_SOCKET\" >> environment\ncat >> events.jsonl\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(root.join("home/.config/fut")).unwrap();
        fs::write(
            root.join("home/.config/fut/config.toml"),
            format!("extensions = [{:?}]\n", extension.display().to_string()),
        )
        .unwrap();
    })
    .await;
    let events_path = harness.root.path().join("extension/events.jsonl");
    wait_for(DEADLINE, || {
        fs::read_to_string(&events_path).is_ok_and(|events| events.lines().count() == 2)
    })
    .await;

    let second_root = harness.root.path().join("second");
    fs::create_dir(&second_root).unwrap();
    let opened = harness
        .cli()
        .args(["open", "--background"])
        .arg(&second_root)
        .args([
            "--name",
            "second",
            "--",
            "/bin/sh",
            "-c",
            "while :; do sleep 1; done",
        ])
        .output()
        .unwrap();
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    let second = harness
        .resources()
        .await
        .sessions
        .into_iter()
        .flat_map(|session| session.workspaces)
        .find(|workspace| workspace.name == "second")
        .unwrap();
    assert_eq!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Workspace(second.id),
                name: "renamed".into(),
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::RenameTarget,
        }
    );
    assert_eq!(
        harness
            .control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Workspace(second.id),
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::CloseTarget,
        }
    );
    wait_for(DEADLINE, || {
        fs::read_to_string(&events_path).is_ok_and(|events| events.lines().count() == 6)
    })
    .await;

    let events = fs::read_to_string(&events_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "session.created",
            "workspace.created",
            "session.created",
            "workspace.created",
            "workspace.renamed",
            "workspace.closed",
        ]
    );
    // The workspace was never explicitly named; it presented its root's
    // basename. Lifecycle events carry stored names, so the rename reports an
    // empty previous name.
    assert_eq!(events[4]["previous_name"], "");
    assert_eq!(events[4]["workspace"]["name"], "renamed");
    assert_eq!(
        events[4]["workspace"]["root"],
        fs::canonicalize(&second_root)
            .unwrap()
            .display()
            .to_string()
    );
    let environment =
        fs::read_to_string(harness.root.path().join("extension/environment")).unwrap();
    assert!(environment.lines().all(|line| {
        let fields = line.split('|').collect::<Vec<_>>();
        fields.len() == 5
            && fields[1] == "1"
            && !fields[2].is_empty()
            && !fields[3].is_empty()
            && fields[4] == harness.socket.to_str().unwrap()
    }));

    harness.shutdown().await;
}

#[tokio::test]
async fn checked_in_example_extension_smokes_hooks_cli_tokens_and_configuration() {
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples/extensions/example-workspace-status");
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        fs::create_dir_all(root.join("home/.config/fut")).unwrap();
        fs::write(
            root.join("home/.config/fut/config.toml"),
            format!(
                "extensions = [{:?}]\n[ui.sidebar.left]\ncomponents = [{{ component = 'workspaces', row = {{ right = [{{ token = 'workspace.extension.example-workspace-status.last_event' }}] }} }}]\n",
                example.display().to_string()
            ),
        )
        .unwrap();
    })
    .await;

    let created = resources_when_with_timeout(&harness, DEADLINE, |snapshot| {
        snapshot.sessions[0].workspaces[0]
            .tokens
            .get("workspace.extension.example-workspace-status.last_event")
            .is_some_and(|value| value == "workspace.created")
    })
    .await;
    let workspace_id = created.sessions[0].workspaces[0].id;

    let doctor = harness.cli().arg("doctor").output().unwrap();
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert_eq!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Workspace(workspace_id),
                name: "example-renamed".into(),
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::RenameTarget,
        }
    );
    resources_when_with_timeout(&harness, DEADLINE, |snapshot| {
        snapshot.sessions[0].workspaces[0]
            .tokens
            .get("workspace.extension.example-workspace-status.last_event")
            .is_some_and(|value| value == "workspace.renamed")
    })
    .await;

    harness.shutdown().await;
}

#[test]
fn checked_in_wt_extension_composes_project_open_and_retirement_commands() {
    let extension = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/extensions/wt");
    let temporary = tempfile::tempdir().unwrap();
    let capture = temporary.path().join("create-argv");
    let fake_wt = temporary.path().join("wt");
    fs::write(
        &fake_wt,
        "#!/bin/sh\n: > \"$CAPTURE\"\nfor arg do printf '%s\\n' \"$arg\" >> \"$CAPTURE\"; done\n",
    )
    .unwrap();
    fs::set_permissions(&fake_wt, fs::Permissions::from_mode(0o755)).unwrap();
    let mut create = Command::new(extension.join("bin/create"))
        .env("FUT_BIN", "/opt/fut")
        .env("FUT_EXTENSION_ROOT", &extension)
        .env("FUT_SOCKET", "/tmp/fut.sock")
        .env("FUT_WT_BIN", &fake_wt)
        .env("CAPTURE", &capture)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    create
        .stdin
        .take()
        .unwrap()
        .write_all(b"feature-name\n")
        .unwrap();
    assert!(create.wait().unwrap().success());
    let expected = vec![
        "create".to_owned(),
        "feature-name".to_owned(),
        "--".to_owned(),
        "/opt/fut".to_owned(),
        "--socket".to_owned(),
        "/tmp/fut.sock".to_owned(),
        "open".to_owned(),
        "--background".to_owned(),
        ".".to_owned(),
        "--name".to_owned(),
        "feature-name".to_owned(),
    ];
    assert_eq!(
        fs::read_to_string(&capture)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        expected
    );

    let mut obsolete = Command::new(extension.join("bin/create"))
        .arg("test-agent")
        .env("FUT_BIN", "/opt/fut")
        .env("FUT_EXTENSION_ROOT", &extension)
        .env("FUT_SOCKET", "/tmp/fut.sock")
        .env("FUT_WT_BIN", &fake_wt)
        .env("CAPTURE", &capture)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    obsolete
        .stdin
        .take()
        .unwrap()
        .write_all(b"obsolete\n")
        .unwrap();
    assert!(!obsolete.wait().unwrap().success());

    let retire_capture = temporary.path().join("retire-argv");
    let fake_fut = temporary.path().join("fut");
    fs::write(
        &fake_fut,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_fut, fs::Permissions::from_mode(0o755)).unwrap();
    let mut retire = Command::new(extension.join("bin/worktree-event"))
        .env("FUT_BIN", &fake_fut)
        .env("FUT_SOCKET", "/tmp/fut.sock")
        .env("CAPTURE", &retire_capture)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    retire
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"version\":1,\"event\":\"worktree.removed\"}\n")
        .unwrap();
    assert!(retire.wait().unwrap().success());
    assert_eq!(
        fs::read_to_string(&retire_capture)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["--socket", "/tmp/fut.sock", "workspace", "retire"]
    );

    fs::remove_file(&retire_capture).unwrap();
    for payload in [
        b"not json\n".as_slice(),
        br#"{"version":10,"event":"worktree.removed"}"#.as_slice(),
        br#"{"version":1,"event":{"name":"worktree.removed"}}"#.as_slice(),
        br#"{"version":1,"event":"workspace.renamed"}"#.as_slice(),
    ] {
        let mut invalid = Command::new(extension.join("bin/worktree-event"))
            .env("FUT_BIN", &fake_fut)
            .env("FUT_SOCKET", "/tmp/fut.sock")
            .env("CAPTURE", &retire_capture)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        invalid.stdin.take().unwrap().write_all(payload).unwrap();
        assert!(!invalid.wait().unwrap().success());
        assert!(
            !retire_capture.exists(),
            "retire command ran for {payload:?}"
        );
    }
}

#[tokio::test]
async fn declared_extension_tokens_publish_authoritatively_through_cli_and_protocol() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let extension = root.join("extension");
        fs::create_dir(&extension).unwrap();
        fs::write(
            extension.join("fut-extension.toml"),
r#"
api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
capabilities = ["presentation_tokens"]
id = "status"
[[presentation_tokens]]
name = "state"
scope = "workspace"
[[presentation_tokens]]
name = "badge"
scope = "tab"
[[presentation_tokens]]
name = "mark"
scope = "pane"
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("home/.config/fut")).unwrap();
        fs::write(
            root.join("home/.config/fut/config.toml"),
            format!(
                "extensions = [{:?}]\n[ui.sidebar.left]\ncomponents = [{{ component = 'workspaces', row = {{ right = [{{ token = 'workspace.extension.status.state' }}] }} }}]\n",
                extension.display().to_string()
            ),
        )
        .unwrap();
    })
    .await;
    let initial = harness.resources().await;
    let workspace = &initial.sessions[0].workspaces[0];
    let workspace_id = workspace.id;
    let tab_id = workspace.tabs[0].id;
    let pane_id = workspace.tabs[0].panes[0].id;

    let mut watcher = harness.connect().await.unwrap();
    assert!(matches!(
        hello(&mut watcher, ClientMode::Control, PROTOCOL_VERSION)
            .await
            .unwrap(),
        ServerMessage::Welcome { .. }
    ));
    send(&mut watcher, ClientMessage::WatchResources).await;
    assert!(matches!(
        receive(&mut watcher).await.unwrap(),
        ServerMessage::Resources { .. }
    ));

    let published = harness
        .cli()
        .args([
            "token",
            "publish",
            "status",
            "state",
            "ready",
            "--workspace-id",
            &workspace_id.to_string(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    let published: Value = serde_json::from_slice(&published.stdout).unwrap();
    assert_eq!(published["command"], "token.publish");
    assert_eq!(published["result"]["changed"], true);
    let published_revision = published["result"]["revision"].as_u64().unwrap();
    assert!(published_revision > initial.revision);

    let changed = receive_matching(&mut watcher, |message| {
        matches!(
            message,
            ServerMessage::ResourcesChanged { snapshot }
                if snapshot.sessions[0].workspaces[0]
                    .tokens
                    .get("workspace.extension.status.state")
                    .is_some_and(|value| value == "ready")
        )
    })
    .await;
    let ServerMessage::ResourcesChanged { snapshot } = changed else {
        unreachable!()
    };
    assert_eq!(
        snapshot.sessions[0].workspaces[0]
            .tokens
            .get("workspace.extension.status.state")
            .map(String::as_str),
        Some("ready")
    );
    assert!(snapshot.revision >= published_revision);

    let unchanged = harness
        .cli()
        .args([
            "--json",
            "token",
            "publish",
            "status",
            "state",
            "ready",
            "--workspace-id",
            &workspace_id.to_string(),
        ])
        .output()
        .unwrap();
    assert!(unchanged.status.success());
    let unchanged: Value = serde_json::from_slice(&unchanged.stdout).unwrap();
    assert_eq!(unchanged["result"]["changed"], false);
    assert!(unchanged["result"]["revision"].as_u64().unwrap() >= published_revision);

    assert!(matches!(
        harness
            .control_command(ClientMessage::PublishToken {
                extension_id: "status".into(),
                token: "badge".into(),
                value: "tab".into(),
                target: PresentationTokenTarget::Workspace(workspace_id),
            })
            .await,
        ServerMessage::Error { ref code, .. } if code == "invalid_token_scope"
    ));
    assert!(matches!(
        harness
            .control_command(ClientMessage::PublishToken {
                extension_id: "missing".into(),
                token: "state".into(),
                value: "value".into(),
                target: PresentationTokenTarget::Workspace(workspace_id),
            })
            .await,
        ServerMessage::Error { ref code, .. } if code == "unknown_extension"
    ));
    assert!(matches!(
        harness
            .control_command(ClientMessage::PublishToken {
                extension_id: "status".into(),
                token: "missing".into(),
                value: "value".into(),
                target: PresentationTokenTarget::Workspace(workspace_id),
            })
            .await,
        ServerMessage::Error { ref code, .. } if code == "undeclared_token"
    ));
    assert!(matches!(
        harness
            .control_command(ClientMessage::PublishToken {
                extension_id: "status".into(),
                token: "state".into(),
                value: "unsafe\nvalue".into(),
                target: PresentationTokenTarget::Workspace(workspace_id),
            })
            .await,
        ServerMessage::Error { ref code, .. } if code == "invalid_token_value"
    ));

    assert!(matches!(
        harness
            .control_command(ClientMessage::PublishToken {
                extension_id: "status".into(),
                token: "badge".into(),
                value: "tab".into(),
                target: PresentationTokenTarget::Tab(tab_id),
            })
            .await,
        ServerMessage::TokenPublished { changed: true, .. }
    ));
    assert!(matches!(
        harness
            .control_command(ClientMessage::PublishToken {
                extension_id: "status".into(),
                token: "mark".into(),
                value: "pane".into(),
                target: PresentationTokenTarget::Pane(pane_id),
            })
            .await,
        ServerMessage::TokenPublished { changed: true, .. }
    ));
    let snapshot = harness.resources().await;
    assert_eq!(
        snapshot.sessions[0].workspaces[0].tabs[0]
            .tokens
            .get("tab.extension.status.badge")
            .map(String::as_str),
        Some("tab")
    );
    assert_eq!(
        snapshot.sessions[0].workspaces[0].tabs[0].panes[0]
            .tokens
            .get("pane.extension.status.mark")
            .map(String::as_str),
        Some("pane")
    );

    harness.shutdown().await;
}

#[test]
fn public_extension_validate_is_daemonless_side_effect_free_and_versioned() {
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("candidate");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("fut-extension.toml"),
        r#"
api_version = 1
version = "2.3.4"
fut = ">=0.7.0, <1.0.0"
capabilities = ["hooks"]
id = "candidate"
[hooks]
"workspace.created" = ["./hook"]
"#,
    )
    .unwrap();
    fs::write(package.join("hook"), "#!/bin/sh\ntouch executed\n").unwrap();
    let runtime = root.path().join("runtime-must-remain-absent");

    let valid = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", "/usr/bin:/bin")
        .env("FUT_RUNTIME_DIR", &runtime)
        .current_dir(root.path())
        .args([
            "--socket",
            "relative-socket-is-ignored",
            "--json",
            "extension",
            "validate",
            "candidate",
        ])
        .output()
        .unwrap();
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    assert!(valid.stderr.is_empty());
    let valid: Value = serde_json::from_slice(&valid.stdout).unwrap();
    assert_eq!(valid["version"], 1);
    assert_eq!(valid["command"], "extension.validate");
    assert_eq!(valid["result"]["valid"], true);
    assert_eq!(valid["result"]["extension"]["id"], "candidate");
    assert_eq!(valid["result"]["extension"]["version"], "2.3.4");
    assert!(!package.join("executed").exists());
    assert!(!runtime.exists());

    let human = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", "/usr/bin:/bin")
        .env("FUT_RUNTIME_DIR", &runtime)
        .current_dir(root.path())
        .args(["extension", "validate", "candidate"])
        .output()
        .unwrap();
    assert!(human.status.success());
    assert!(
        String::from_utf8(human.stdout)
            .unwrap()
            .contains("valid extension candidate version=2.3.4")
    );
    assert!(!package.join("executed").exists());
    assert!(!runtime.exists());

    fs::write(package.join("fut-extension.toml"), "invalid manifest\n").unwrap();
    let invalid = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", "/usr/bin:/bin")
        .env("FUT_RUNTIME_DIR", &runtime)
        .current_dir(root.path())
        .args(["--json", "extension", "validate", "candidate"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    let invalid: Value = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(invalid["version"], 1);
    assert_eq!(invalid["error"]["code"], "invalid_extension");
    assert!(!runtime.exists());
}

#[tokio::test]
async fn managed_extension_store_commands_are_versioned_safe_and_merged_with_explicit_roots() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let explicit = root.join("explicit-extension");
        fs::create_dir(&explicit).unwrap();
        fs::write(
            explicit.join("fut-extension.toml"),
            r#"
api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
capabilities = []
id = "explicit"
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("home/.config/fut")).unwrap();
        fs::write(
            root.join("home/.config/fut/config.toml"),
            format!("extensions = [{:?}]\n", explicit.display().to_string()),
        )
        .unwrap();

        let managed = root.join("managed-source");
        fs::create_dir(&managed).unwrap();
        fs::write(
            managed.join("fut-extension.toml"),
            r#"
api_version = 1
version = "2.3.4"
fut = ">=0.7.0, <1.0.0"
capabilities = ["commands"]
id = "managed"
[commands.run]
title = "Run managed command"
argv = ["./run"]
"#,
        )
        .unwrap();
        fs::write(managed.join("run"), "#!/bin/sh\ntouch package-executed\n").unwrap();
        fs::set_permissions(managed.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
    })
    .await;

    let initial = harness
        .cli()
        .args(["--json", "extension", "list"])
        .output()
        .unwrap();
    let initial: Value = serde_json::from_slice(&initial.stdout).unwrap();
    assert_eq!(initial["result"]["count"], 1);
    assert_eq!(initial["result"]["extensions"][0]["id"], "explicit");

    let source = harness.root.path().join("managed-source");
    let installed = harness
        .cli()
        .args(["--json", "extension", "install"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(installed.stderr.is_empty());
    let installed: Value = serde_json::from_slice(&installed.stdout).unwrap();
    assert_eq!(installed["version"], 1);
    assert_eq!(installed["command"], "extension.install");
    assert_eq!(installed["result"]["extension"]["id"], "managed");
    assert_eq!(installed["result"]["extension"]["version"], "2.3.4");
    assert_eq!(installed["result"]["extension"]["enabled"], false);
    assert_eq!(installed["result"]["changed"], true);
    assert_eq!(installed["result"]["scripts_executed"], false);
    assert_eq!(installed["result"]["trust"], "trusted_local_code");
    assert_eq!(installed["result"]["reload_required"], false);
    let digest = installed["result"]["extension"]["content_sha256"]
        .as_str()
        .unwrap();
    assert_eq!(digest.len(), 64);
    let install_path = PathBuf::from(
        installed["result"]["extension"]["install_path"]
            .as_str()
            .unwrap(),
    );
    assert!(install_path.is_dir());
    assert!(install_path.ends_with(format!("managed/2.3.4/{digest}")));
    assert!(!source.join("package-executed").exists());

    let index_path = harness.root.path().join("data/fut/extensions/index.json");
    let index_source = fs::read_to_string(&index_path).unwrap();
    assert!(index_source.contains("\n  \"schema_version\": 1,"));
    assert!(index_source.ends_with('\n'));
    let index: Value = serde_json::from_str(&index_source).unwrap();
    assert_eq!(
        index["extensions"][0]["source"],
        fs::canonicalize(&source)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        index["extensions"][0]["install_path"],
        install_path.to_string_lossy().as_ref()
    );

    let human = harness
        .cli()
        .args(["extension", "install"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("changed=false"), "{human}");
    assert!(
        human.contains("No package scripts were executed"),
        "{human}"
    );
    assert!(human.contains("trusted local code"), "{human}");

    let enabled = harness
        .cli()
        .args(["--json", "extension", "enable", "managed"])
        .output()
        .unwrap();
    assert!(enabled.status.success());
    let enabled: Value = serde_json::from_slice(&enabled.stdout).unwrap();
    assert_eq!(enabled["command"], "extension.enable");
    assert_eq!(enabled["result"]["extension"]["enabled"], true);
    assert_eq!(enabled["result"]["reload_required"], true);
    assert!(!source.join("package-executed").exists());

    let refused = harness
        .cli()
        .args(["--json", "extension", "remove", "managed"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert!(refused.stdout.is_empty());
    let refused: Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(refused["error"]["code"], "extension_enabled");
    assert!(install_path.exists());

    let reloaded = harness
        .cli()
        .args(["--json", "extension", "reload"])
        .output()
        .unwrap();
    assert!(
        reloaded.status.success(),
        "{}",
        String::from_utf8_lossy(&reloaded.stderr)
    );
    let reloaded: Value = serde_json::from_slice(&reloaded.stdout).unwrap();
    let ids = reloaded["result"]["catalog"]["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|extension| extension["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["explicit", "managed"]);

    let disabled = harness
        .cli()
        .args(["--json", "extension", "disable", "managed"])
        .output()
        .unwrap();
    assert!(disabled.status.success());
    let disabled: Value = serde_json::from_slice(&disabled.stdout).unwrap();
    assert_eq!(disabled["command"], "extension.disable");
    assert_eq!(disabled["result"]["extension"]["enabled"], false);

    let reloaded = harness
        .cli()
        .args(["--json", "extension", "reload"])
        .output()
        .unwrap();
    assert!(reloaded.status.success());
    let reloaded: Value = serde_json::from_slice(&reloaded.stdout).unwrap();
    assert_eq!(
        reloaded["result"]["catalog"]["extensions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reloaded["result"]["catalog"]["extensions"][0]["id"],
        "explicit"
    );

    let removed = harness
        .cli()
        .args(["--json", "extension", "remove", "managed"])
        .output()
        .unwrap();
    assert!(removed.status.success());
    let removed: Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(removed["command"], "extension.remove");
    assert_eq!(removed["result"]["changed"], true);
    assert!(install_path.exists());
    assert!(
        serde_json::from_slice::<Value>(&fs::read(index_path).unwrap()).unwrap()["extensions"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    harness.shutdown().await;
}

#[test]
fn pinned_git_extension_install_update_and_rollback_are_daemonless() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let remote = root.path().join("remote.git");
    let data = root.path().join("data");
    let temporary = root.path().join("tmp");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&temporary).unwrap();
    git(&source, &["init", "-b", "main"]);
    fs::write(
        source.join("fut-extension.toml"),
        r#"
api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
capabilities = []
id = "remote-test"
"#,
    )
    .unwrap();
    fs::write(source.join("content.txt"), "first\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "first"]);
    let first_commit = git_stdout(&source, &["rev-parse", "HEAD"]);
    git(
        root.path(),
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    let remote_url = format!("file://{}", remote.display());

    let cli = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_fut"))
            .env_clear()
            .env("HOME", root.path())
            .env("XDG_DATA_HOME", &data)
            .env("TMPDIR", &temporary)
            .env("PATH", "/usr/bin:/bin")
            .args(arguments)
            .output()
            .unwrap()
    };

    let invalid = cli(&[
        "--json",
        "extension",
        "install-git",
        &remote_url,
        "--rev",
        "main",
    ]);
    assert_eq!(invalid.status.code(), Some(1));
    let invalid: Value = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(invalid["error"]["code"], "extension_git_install_failed");
    assert!(
        invalid["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exact full")
    );
    assert!(!data.join("fut/extensions/index.json").exists());

    let installed = cli(&[
        "--json",
        "extension",
        "install-git",
        &remote_url,
        "--rev",
        &first_commit,
    ]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed: Value = serde_json::from_slice(&installed.stdout).unwrap();
    assert_eq!(installed["command"], "extension.install-git");
    assert_eq!(installed["result"]["extension"]["id"], "remote-test");
    assert_eq!(
        installed["result"]["extension"]["provenance"]["kind"],
        "git"
    );
    assert_eq!(
        installed["result"]["extension"]["provenance"]["remote_url"],
        remote_url
    );
    assert_eq!(
        installed["result"]["extension"]["provenance"]["commit"],
        first_commit
    );
    assert!(installed["result"]["extension"].get("source").is_none());
    assert_eq!(installed["result"]["scripts_executed"], false);
    let first_path = PathBuf::from(
        installed["result"]["extension"]["install_path"]
            .as_str()
            .unwrap(),
    );
    let first_digest = installed["result"]["extension"]["content_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(first_path.is_dir());

    fs::write(
        source.join("fut-extension.toml"),
        r#"
api_version = 1
version = "2.0.0"
fut = ">=0.7.0, <1.0.0"
capabilities = []
id = "remote-test"
"#,
    )
    .unwrap();
    fs::write(source.join("content.txt"), "second\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "second"]);
    let second_commit = git_stdout(&source, &["rev-parse", "HEAD"]);
    git(&source, &["push", remote.to_str().unwrap(), "HEAD:main"]);

    let index_path = data.join("fut/extensions/index.json");
    let old_index = fs::read(&index_path).unwrap();
    let mismatch = cli(&[
        "--json",
        "extension",
        "update",
        "remote-test",
        "--rev",
        &second_commit,
        "--sha256",
        "0000000000000000000000000000000000000000000000000000000000000000",
    ]);
    assert_eq!(mismatch.status.code(), Some(1));
    let mismatch: Value = serde_json::from_slice(&mismatch.stderr).unwrap();
    assert_eq!(mismatch["error"]["code"], "extension_git_update_failed");
    assert!(
        mismatch["error"]["message"]
            .as_str()
            .unwrap()
            .contains("SHA-256 mismatch")
    );
    assert_eq!(fs::read(&index_path).unwrap(), old_index);
    assert!(first_path.is_dir());

    let updated = cli(&[
        "--json",
        "extension",
        "update",
        "remote-test",
        "--rev",
        &second_commit,
    ]);
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let updated: Value = serde_json::from_slice(&updated.stdout).unwrap();
    assert_eq!(updated["command"], "extension.update");
    assert_eq!(updated["result"]["previous"]["version"], "1.0.0");
    assert_eq!(updated["result"]["extension"]["version"], "2.0.0");
    assert_eq!(
        updated["result"]["previous"]["provenance"]["commit"],
        first_commit
    );
    assert_eq!(
        updated["result"]["extension"]["provenance"]["commit"],
        second_commit
    );
    assert_ne!(
        updated["result"]["extension"]["content_sha256"],
        first_digest
    );
    assert!(first_path.is_dir(), "superseded bytes remain immutable");

    fs::rename(&remote, root.path().join("remote-offline.git")).unwrap();
    let removed = cli(&["--json", "extension", "remove", "remote-test"]);
    assert!(removed.status.success());
    assert!(
        serde_json::from_slice::<Value>(&fs::read(&index_path).unwrap()).unwrap()["extensions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn public_extension_catalog_commands_use_active_daemon_state_and_atomic_reload() {
    let initial_manifest = r#"
api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
capabilities = ["commands"]
id = "inspect"
[commands.open]
title = "Open inspector"
argv = ["./open"]
"#;
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let extension = root.join("extension");
        fs::create_dir(&extension).unwrap();
        fs::write(extension.join("fut-extension.toml"), initial_manifest).unwrap();
        fs::create_dir_all(root.join("home/.config/fut")).unwrap();
        fs::write(
            root.join("home/.config/fut/config.toml"),
            format!(
                "extensions = [{:?}]\n[extension.inspect]\ncolor = 'blue'\n",
                extension.display().to_string()
            ),
        )
        .unwrap();
    })
    .await;

    let ServerMessage::ExtensionCatalog { catalog } = harness
        .control_command(ClientMessage::GetExtensionCatalog)
        .await
    else {
        panic!("expected active extension catalog")
    };
    assert_eq!(catalog.generation, 1);
    assert_eq!(catalog.extensions[0].id, "inspect");

    let human = harness.cli().args(["extension", "list"]).output().unwrap();
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("generation=1"), "{human}");
    assert!(human.contains("extension inspect version=1.0.0"), "{human}");

    let listed = harness
        .cli()
        .args(["--json", "extension", "list"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["version"], 1);
    assert_eq!(listed["command"], "extension.list");
    assert_eq!(listed["result"]["generation"], 1);
    assert_eq!(listed["result"]["count"], 1);
    assert_eq!(listed["result"]["extensions"][0]["id"], "inspect");

    let shown = harness
        .cli()
        .args(["--json", "extension", "show", "inspect"])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["command"], "extension.show");
    assert_eq!(
        shown["result"]["extension"]["commands"]["open"]["title"],
        "Open inspector"
    );
    assert_eq!(shown["result"]["config"]["defaults"]["color"], "blue");
    assert!(shown["result"]["config"]["source"].is_string());
    let shown = harness
        .cli()
        .args(["extension", "show", "inspect"])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(shown.contains("extension inspect"), "{shown}");
    assert!(shown.contains("commands: open (interactive)"), "{shown}");
    assert!(
        shown.contains("config_defaults: {\"color\":\"blue\"}"),
        "{shown}"
    );

    let missing = harness
        .cli()
        .args(["--json", "extension", "show", "missing"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&missing.stderr).unwrap()["error"]["code"],
        "not_found"
    );

    let doctor = harness.cli().args(["--json", "doctor"]).output().unwrap();
    assert!(doctor.status.success());
    let doctor: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let extensions = doctor["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "extensions")
        .unwrap();
    assert_eq!(extensions["details"]["generation"], 1);
    assert_eq!(extensions["details"]["count"], 1);
    assert_eq!(extensions["details"]["packages"][0]["id"], "inspect");
    assert_eq!(extensions["details"]["packages"][0]["version"], "1.0.0");
    assert_eq!(
        extensions["details"]["packages"][0]["provenance"],
        "active_daemon_catalog"
    );

    let manifest = harness.root.path().join("extension/fut-extension.toml");
    fs::write(&manifest, initial_manifest.replace("1.0.0", "1.1.0")).unwrap();
    let reloaded = harness
        .cli()
        .args(["--json", "extension", "reload"])
        .output()
        .unwrap();
    assert!(
        reloaded.status.success(),
        "{}",
        String::from_utf8_lossy(&reloaded.stderr)
    );
    let reloaded: Value = serde_json::from_slice(&reloaded.stdout).unwrap();
    assert_eq!(reloaded["command"], "extension.reload");
    assert_eq!(reloaded["result"]["changed"], true);
    assert_eq!(reloaded["result"]["catalog"]["generation"], 2);
    assert_eq!(
        reloaded["result"]["catalog"]["extensions"][0]["version"],
        "1.1.0"
    );
    let unchanged = harness
        .cli()
        .args(["extension", "reload"])
        .output()
        .unwrap();
    assert!(unchanged.status.success());
    let unchanged = String::from_utf8(unchanged.stdout).unwrap();
    assert!(unchanged.contains("generation=2"), "{unchanged}");
    assert!(unchanged.contains("changed=false"), "{unchanged}");

    fs::write(&manifest, "invalid manifest\n").unwrap();
    let failed = harness
        .cli()
        .args(["--json", "extension", "reload"])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&failed.stderr).unwrap()["error"]["code"],
        "extension_reload_failed"
    );
    let listed = harness
        .cli()
        .args(["--json", "extension", "list"])
        .output()
        .unwrap();
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["result"]["generation"], 2);
    assert_eq!(listed["result"]["extensions"][0]["version"], "1.1.0");

    let doctor = harness.cli().args(["--json", "doctor"]).output().unwrap();
    assert_eq!(doctor.status.code(), Some(1));
    let doctor: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let checks = doctor["result"]["checks"].as_array().unwrap();
    let config = checks.iter().find(|check| check["id"] == "config").unwrap();
    assert_eq!(config["status"], "error");
    assert!(config["summary"].as_str().unwrap().contains("manifest"));
    let active = checks
        .iter()
        .find(|check| check["id"] == "extensions")
        .unwrap();
    assert_eq!(active["status"], "ok");
    assert_eq!(active["details"]["generation"], 2);
    assert_eq!(active["details"]["packages"][0]["version"], "1.1.0");

    harness.shutdown().await;
}

#[tokio::test]
async fn daemon_extensions_reload_atomically_rolls_back_and_prunes_removed_tokens() {
    let mut harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let extension = root.join("extension");
        fs::create_dir(&extension).unwrap();
        fs::write(
            extension.join("fut-extension.toml"),
            r#"
api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
capabilities = ["presentation_tokens"]
id = "reloadable"
[[presentation_tokens]]
name = "state"
scope = "workspace"
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("home/.config/fut")).unwrap();
        fs::write(
            root.join("home/.config/fut/config.toml"),
            format!("extensions = [{:?}]\n", extension.display().to_string()),
        )
        .unwrap();
    })
    .await;
    let manifest = harness.root.path().join("extension/fut-extension.toml");
    let initial = harness.resources().await;
    let workspace_id = initial.sessions[0].workspaces[0].id;
    let ServerMessage::ExtensionsReloaded { catalog, changed } = harness
        .control_command(ClientMessage::ReloadExtensions)
        .await
    else {
        panic!("expected extension reload response")
    };
    let generation = catalog.generation;
    let fingerprint = catalog.fingerprint;
    assert_eq!(generation, 1);
    assert_eq!(fingerprint.len(), 64);
    assert!(!changed);

    let (mut interactive, _, _) = harness.interactive().await;
    send(&mut interactive, ClientMessage::ReloadExtensions).await;
    assert!(matches!(
        receive_matching(&mut interactive, |message| matches!(
            message,
            ServerMessage::Error { code, .. } if code == "control_only"
        ))
        .await,
        ServerMessage::Error { ref code, .. } if code == "control_only"
    ));
    harness.detach(&mut interactive).await;

    assert!(matches!(
        harness
            .control_command(ClientMessage::PublishToken {
                extension_id: "reloadable".into(),
                token: "state".into(),
                value: "ready".into(),
                target: PresentationTokenTarget::Workspace(workspace_id),
            })
            .await,
        ServerMessage::TokenPublished { changed: true, .. }
    ));
    let before_failure = harness.resources().await;
    fs::write(&manifest, "this is not a valid extension manifest\n").unwrap();

    let failed = harness
        .control_command(ClientMessage::ReloadExtensions)
        .await;
    assert!(matches!(
        failed,
        ServerMessage::Error { ref code, ref message }
            if code == "extension_reload_failed"
                && message.contains("fix the configuration or extension package")
                && message.contains("left unchanged")
    ));
    let after_failure = harness.resources().await;
    assert_eq!(
        after_failure.sessions[0].workspaces[0]
            .tokens
            .get("workspace.extension.reloadable.state"),
        before_failure.sessions[0].workspaces[0]
            .tokens
            .get("workspace.extension.reloadable.state")
    );
    let mut after_failure_connection = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        extension_catalog: after_failure_catalog,
        ..
    } = hello(
        &mut after_failure_connection,
        ClientMode::Control,
        PROTOCOL_VERSION,
    )
    .await
    .unwrap()
    else {
        panic!("expected welcome after failed reload")
    };
    assert_eq!(after_failure_catalog.generation, generation);
    assert_eq!(after_failure_catalog.fingerprint, fingerprint);

    fs::write(
        &manifest,
        r#"
api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
capabilities = []
id = "reloadable"
"#,
    )
    .unwrap();
    let reloaded = harness
        .control_command(ClientMessage::ReloadExtensions)
        .await;
    let ServerMessage::ExtensionsReloaded {
        catalog: changed_catalog,
        changed,
    } = reloaded
    else {
        panic!("expected changed extension reload response")
    };
    let generation = changed_catalog.generation;
    let changed_fingerprint = changed_catalog.fingerprint.clone();
    assert_eq!(generation, 2);
    assert!(changed);
    assert_ne!(changed_fingerprint, fingerprint);
    let pruned = harness.resources().await;
    assert!(pruned.revision > before_failure.revision);
    assert!(pruned.sessions[0].workspaces[0].tokens.is_empty());
    assert!(matches!(
        harness
            .control_command(ClientMessage::PublishToken {
                extension_id: "reloadable".into(),
                token: "state".into(),
                value: "again".into(),
                target: PresentationTokenTarget::Workspace(workspace_id),
            })
            .await,
        ServerMessage::Error { ref code, .. } if code == "undeclared_token"
    ));
    assert_eq!(
        harness
            .control_command(ClientMessage::ReloadExtensions)
            .await,
        ServerMessage::ExtensionsReloaded {
            catalog: changed_catalog,
            changed: false,
        }
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn extension_catalog_prepare_commit_converges_clients_reconnects_and_rejects_stale_cas() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let extension = root.join("extension");
        fs::create_dir(&extension).unwrap();
        fs::write(
            extension.join("fut-extension.toml"),
            r#"
api_version = 1
version = "1.0.0"
fut = ">=0.7.0, <1.0.0"
capabilities = ["commands", "hooks", "presentation_tokens"]
id = "catalog"
[hooks]
"client.attached" = ["./hook"]
[commands.launch]
title = "Initial command"
argv = ["./launch", "initial"]
[[presentation_tokens]]
name = "state"
scope = "workspace"
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("home/.config/fut")).unwrap();
        fs::write(
            root.join("home/.config/fut/config.toml"),
            format!(
                "extensions = [{:?}]\n[extension.catalog]\nenabled = true\n",
                extension.display().to_string()
            ),
        )
        .unwrap();
    })
    .await;
    let manifest = harness.root.path().join("extension/fut-extension.toml");

    let mut client_a = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        extension_catalog: initial_a,
        ..
    } = hello(&mut client_a, interactive_mode(None), PROTOCOL_VERSION)
        .await
        .unwrap()
    else {
        panic!("expected client A welcome")
    };
    let mut client_b = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        extension_catalog: initial_b,
        ..
    } = hello(&mut client_b, interactive_mode(None), PROTOCOL_VERSION)
        .await
        .unwrap()
    else {
        panic!("expected client B welcome")
    };
    assert_eq!(initial_a, initial_b);
    assert_eq!(initial_a.generation, 1);
    assert_eq!(
        initial_a.extensions[0].commands["launch"].title,
        "Initial command"
    );
    assert_eq!(initial_a.config.defaults["catalog"]["enabled"], true);

    fs::write(
        &manifest,
        r#"
api_version = 1
version = "1.1.0"
fut = ">=0.7.0, <1.0.0"
capabilities = ["commands", "hooks", "presentation_tokens"]
id = "catalog"
[hooks]
"client.session_changed" = ["./new-hook"]
[commands.launch]
title = "Reloaded command"
argv = ["./launch", "reloaded"]
mode = "background"
[[presentation_tokens]]
name = "state"
scope = "workspace"
presentation = "spinner"
"#,
    )
    .unwrap();
    send(&mut client_a, ClientMessage::PrepareExtensionReload).await;
    let ServerMessage::ExtensionReloadPrepared {
        base_generation,
        catalog: proposed,
    } = receive_matching(&mut client_a, |message| {
        matches!(message, ServerMessage::ExtensionReloadPrepared { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(base_generation, 1);
    assert_eq!(proposed.generation, 2);
    assert_eq!(
        proposed.extensions[0].commands["launch"].title,
        "Reloaded command"
    );

    let mut before_commit = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        extension_catalog: still_initial,
        ..
    } = hello(&mut before_commit, interactive_mode(None), PROTOCOL_VERSION)
        .await
        .unwrap()
    else {
        panic!("expected pre-commit welcome")
    };
    assert_eq!(still_initial, initial_a);
    harness.detach(&mut before_commit).await;

    send(
        &mut client_a,
        ClientMessage::CommitExtensionReload { base_generation },
    )
    .await;
    let ServerMessage::ExtensionsReloaded {
        catalog: committed,
        changed: true,
    } = receive_matching(&mut client_a, |message| {
        matches!(message, ServerMessage::ExtensionsReloaded { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(committed, proposed);
    for client in [&mut client_a, &mut client_b] {
        let ServerMessage::ExtensionCatalogChanged { catalog } =
            receive_matching(client, |message| {
                matches!(message, ServerMessage::ExtensionCatalogChanged { .. })
            })
            .await
        else {
            unreachable!()
        };
        assert_eq!(catalog, committed);
    }

    let mut reconnected = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        extension_catalog: reconnect_catalog,
        ..
    } = hello(&mut reconnected, interactive_mode(None), PROTOCOL_VERSION)
        .await
        .unwrap()
    else {
        panic!("expected reconnect welcome")
    };
    assert_eq!(reconnect_catalog, committed);
    harness.detach(&mut reconnected).await;

    fs::write(&manifest, "invalid manifest\n").unwrap();
    send(&mut client_a, ClientMessage::PrepareExtensionReload).await;
    assert!(matches!(
        receive_matching(&mut client_a, |message| matches!(
            message,
            ServerMessage::Error { code, .. } if code == "extension_reload_failed"
        ))
        .await,
        ServerMessage::Error { ref code, .. } if code == "extension_reload_failed"
    ));

    fs::write(
        &manifest,
        r#"
api_version = 1
version = "1.2.0"
fut = ">=0.7.0, <1.0.0"
capabilities = ["commands"]
id = "catalog"
[commands.final]
title = "Final command"
argv = ["./final"]
"#,
    )
    .unwrap();
    send(&mut client_a, ClientMessage::PrepareExtensionReload).await;
    let ServerMessage::ExtensionReloadPrepared {
        base_generation: base_a,
        catalog: candidate_a,
    } = receive_matching(&mut client_a, |message| {
        matches!(message, ServerMessage::ExtensionReloadPrepared { .. })
    })
    .await
    else {
        unreachable!()
    };
    send(&mut client_b, ClientMessage::PrepareExtensionReload).await;
    let ServerMessage::ExtensionReloadPrepared {
        base_generation: base_b,
        catalog: candidate_b,
    } = receive_matching(&mut client_b, |message| {
        matches!(message, ServerMessage::ExtensionReloadPrepared { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(base_a, 2);
    assert_eq!(base_a, base_b);
    assert_eq!(candidate_a, candidate_b);

    send(
        &mut client_a,
        ClientMessage::CommitExtensionReload {
            base_generation: base_a,
        },
    )
    .await;
    let ServerMessage::ExtensionsReloaded {
        catalog: winner, ..
    } = receive_matching(&mut client_a, |message| {
        matches!(message, ServerMessage::ExtensionsReloaded { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(winner.generation, 3);
    let ServerMessage::ExtensionCatalogChanged { catalog } =
        receive_matching(&mut client_b, |message| {
            matches!(message, ServerMessage::ExtensionCatalogChanged { .. })
        })
        .await
    else {
        unreachable!()
    };
    assert_eq!(catalog, winner);
    send(
        &mut client_b,
        ClientMessage::CommitExtensionReload {
            base_generation: base_b,
        },
    )
    .await;
    assert!(matches!(
        receive_matching(&mut client_b, |message| matches!(
            message,
            ServerMessage::Error { code, .. } if code == "stale_extension_generation"
        ))
        .await,
        ServerMessage::Error { ref code, .. } if code == "stale_extension_generation"
    ));

    let mut final_reconnect = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        extension_catalog: final_catalog,
        ..
    } = hello(
        &mut final_reconnect,
        interactive_mode(None),
        PROTOCOL_VERSION,
    )
    .await
    .unwrap()
    else {
        panic!("expected final reconnect welcome")
    };
    assert_eq!(final_catalog, winner);
    harness.detach(&mut final_reconnect).await;
    harness.detach(&mut client_a).await;
    harness.detach(&mut client_b).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn daemon_git_tokens_publish_atomic_shared_snapshots_and_clear_when_git_disappears() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let cwd = root.join("cwd");
        git(&cwd, &["init", "--initial-branch=main"]);
        fs::write(cwd.join("tracked"), "one\ntwo\n").unwrap();
        git(&cwd, &["add", "tracked"]);
        git(&cwd, &["commit", "-m", "fixture"]);
    })
    .await;

    let initial = resources_when_with_timeout(&harness, Duration::from_secs(20), |snapshot| {
        workspace_git_tokens(snapshot) == [Some("main"), None, None]
    })
    .await;
    let mut watchers = Vec::new();
    for _ in 0..2 {
        let mut watcher = harness.connect().await.unwrap();
        assert!(matches!(
            hello(&mut watcher, ClientMode::Control, PROTOCOL_VERSION)
                .await
                .unwrap(),
            ServerMessage::Welcome { .. }
        ));
        send(&mut watcher, ClientMessage::WatchResources).await;
        let ServerMessage::Resources { snapshot, .. } = receive(&mut watcher).await.unwrap() else {
            panic!("expected initial resources")
        };
        assert_eq!(workspace_git_tokens(&snapshot), [Some("main"), None, None]);
        watchers.push(watcher);
    }

    fs::write(
        harness.root.path().join("cwd/tracked"),
        "one\nthree\nfour\n",
    )
    .unwrap();
    resources_when_with_timeout(&harness, Duration::from_secs(20), |snapshot| {
        workspace_git_tokens(snapshot) == [Some("main"), Some("+2"), Some("-1")]
    })
    .await;
    let mut changed_revisions = Vec::new();
    for watcher in &mut watchers {
        let message = receive_matching(watcher, |message| {
            let ServerMessage::ResourcesChanged { snapshot } = message else {
                return false;
            };
            let tokens = workspace_git_tokens(snapshot);
            assert!(
                tokens == [Some("main"), None, None]
                    || tokens == [Some("main"), Some("+2"), Some("-1")],
                "Git values must never be published as a partial snapshot: {tokens:?}"
            );
            tokens == [Some("main"), Some("+2"), Some("-1")]
        })
        .await;
        let ServerMessage::ResourcesChanged { snapshot } = message else {
            unreachable!()
        };
        changed_revisions.push(snapshot.revision);
    }
    assert_eq!(changed_revisions[0], changed_revisions[1]);
    assert!(changed_revisions[0] > initial.revision);

    fs::rename(
        harness.root.path().join("cwd/.git"),
        harness.root.path().join("cwd/git-hidden"),
    )
    .unwrap();
    resources_when_with_timeout(&harness, Duration::from_secs(20), |snapshot| {
        workspace_git_tokens(snapshot) == [None, None, None]
    })
    .await;
    let cleared = receive_matching(&mut watchers[0], |message| {
        let ServerMessage::ResourcesChanged { snapshot } = message else {
            return false;
        };
        let tokens = workspace_git_tokens(snapshot);
        assert!(
            tokens == [Some("main"), Some("+2"), Some("-1")] || tokens == [None, None, None],
            "Git values must clear together: {tokens:?}"
        );
        tokens == [None, None, None]
    })
    .await;
    let ServerMessage::ResourcesChanged { snapshot } = cleared else {
        unreachable!()
    };
    assert_eq!(workspace_git_tokens(&snapshot), [None, None, None]);

    harness.shutdown().await;
}

#[tokio::test]
async fn daemon_shutdown_cancels_an_in_flight_git_refresh() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let cwd = root.join("cwd");
        git(&cwd, &["init", "--initial-branch=main"]);
        fs::write(cwd.join("tracked"), "ready\n").unwrap();
        git(&cwd, &["add", "tracked"]);
        git(&cwd, &["commit", "-m", "fixture"]);

        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let marker = root.join("git-refresh-started");
        let fake_git = bin.join("git");
        fs::write(
            &fake_git,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"--abbrev-ref HEAD\"*)\n    : > {:?}\n    exec sleep 60\n    ;;\nesac\nexec /usr/bin/git \"$@\"\n",
                marker.display().to_string()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    })
    .await;
    wait_for(DEADLINE, || {
        harness.root.path().join("git-refresh-started").exists()
    })
    .await;

    let started = Instant::now();
    harness.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "daemon shutdown waited for the hung Git process"
    );
}

#[test]
fn daemon_setup_rejects_unsupported_extension_hooks_before_creating_resources() {
    let root = tempfile::tempdir().unwrap();
    let extension = root.path().join("extension");
    fs::create_dir(&extension).unwrap();
    fs::write(
        extension.join("fut-extension.toml"),
        "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = ['hooks']\nid = 'invalid'\n[hooks]\n'workspace.changed' = ['helper']\n",
    )
    .unwrap();
    let config = root.path().join("config.toml");
    fs::write(
        &config,
        format!("extensions = [{:?}]\n", extension.display().to_string()),
    )
    .unwrap();
    let socket = root.path().join("fut.sock");

    let output = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("FUT_CONFIG", &config)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .args(["--socket"])
        .arg(&socket)
        .args(["daemon", "run", "--cwd"])
        .arg(root.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsupported hook"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!socket.exists());
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
    wait_for_nonempty_file(&pwd_file).await;
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
    wait_for_nonempty_file(&pwd).await;
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
    assert_eq!(
        without_observations(harness.resources().await),
        without_observations(moved.clone())
    );

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
    let named = harness.resources().await.sessions[0].workspaces[0].tabs[0].id;
    assert!(matches!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Tab(named),
                name: "shell".into(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
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
        assert_eq!(
            without_observations(harness.resources().await),
            without_observations(before.clone())
        );
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
        assert_eq!(
            without_observations(harness.resources().await),
            without_observations(before.clone())
        );
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
    let resized = receive_matching(&mut first, |message| {
        matches!(
            message,
            ServerMessage::TerminalResized { terminal_id, size }
                if *terminal_id == pane_b.terminal_id
                    && *size == TerminalSize { columns: 31, rows: 9 }
        )
    })
    .await;
    assert!(matches!(resized, ServerMessage::TerminalResized { .. }));

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
    send_uncorrelated(&mut first, mouse_wheel_message(pane_c.terminal_id)).await;
    assert_no_error_before_pong(&mut first).await;
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
    send_uncorrelated(&mut attached, mouse_wheel_message(pane_b.terminal_id)).await;
    assert_no_error_before_pong(&mut attached).await;
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
    let before = resources_when(&harness, |snapshot| {
        !snapshot.sessions[0].workspaces[0].tabs[0].name.is_empty()
    })
    .await;
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
        assert_eq!(
            without_observations(harness.resources().await),
            without_observations(before.clone())
        );
        assert!(!marker.exists());
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn scrollback_is_attachment_local_survives_output_and_paste_returns_only_focus_to_bottom() {
    let mut harness = Harness::start(
        "i=0; while [ $i -le 40 ]; do printf 'HIST_%02d\\r\\n' \"$i\"; i=$((i + 1)); done; while IFS= read -r line; do printf 'BOTTOM_%s\\r\\n' \"$line\"; done",
    )
    .await;
    let resources = harness.resources().await;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let terminal_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'PANE_B_READY\\r\\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create second pane")
    };

    let (mut client_a, selected_a, _) = harness
        .interactive_for(Some(TargetSelector::Terminal(terminal_a)))
        .await;
    assert_eq!(selected_a, terminal_a);
    let (mut client_b, selected_b) =
        attach_once(&harness, TargetSelector::Terminal(pane_b.terminal_id)).await;
    assert_eq!(selected_b, pane_b);
    let bottom = snapshot_containing(&mut client_a, terminal_a, "HIST_40").await;
    assert_eq!(bottom.scroll.offset_from_bottom, 0);
    assert!(
        bottom.scroll.max_offset_from_bottom > 0,
        "history beyond the viewport must report scrollback"
    );
    snapshot_containing(&mut client_b, terminal_a, "HIST_40").await;

    let wheel = ClientMessage::MouseInput {
        terminal_id: terminal_a,
        event: mouse_event(
            MouseEventKind::Wheel {
                direction: MouseWheelDirection::Up,
            },
            0,
            0,
        ),
    };
    assert_fire_and_forget_request_id_rejected(&mut client_b, wheel.clone()).await;
    send_uncorrelated(&mut client_b, wheel.clone()).await;
    let ServerMessage::Snapshot {
        screen: history_b, ..
    } = receive_matching(&mut client_b, |message| {
        matches!(message, ServerMessage::Snapshot { terminal_id, screen }
            if *terminal_id == terminal_a && !snapshot_text(screen).contains("HIST_40"))
    })
    .await
    else {
        unreachable!()
    };
    assert!(
        history_b.scroll.offset_from_bottom > 0,
        "scrolled viewport must report its offset for the scrollbar"
    );

    assert_fire_and_forget_request_id_rejected(
        &mut client_b,
        ClientMessage::ResetViewport {
            terminal_id: terminal_a,
        },
    )
    .await;

    send_uncorrelated(&mut client_a, wheel).await;
    let ServerMessage::Snapshot {
        screen: history_a, ..
    } = receive_matching(&mut client_a, |message| {
        matches!(message, ServerMessage::Snapshot { terminal_id, screen }
            if *terminal_id == terminal_a && !snapshot_text(screen).contains("HIST_40"))
    })
    .await
    else {
        unreachable!()
    };

    send(
        &mut client_a,
        ClientMessage::Paste {
            text: "NEW\n".into(),
        },
    )
    .await;
    let bottom_a = snapshot_containing(&mut client_a, terminal_a, "BOTTOM_NEW").await;
    assert!(bottom_a.revision > history_a.revision);

    let ServerMessage::Snapshot {
        screen: history_b_after_output,
        ..
    } = receive_matching(&mut client_b, |message| {
        matches!(message, ServerMessage::Snapshot { terminal_id, screen }
            if *terminal_id == terminal_a && screen.revision > history_b.revision)
    })
    .await
    else {
        unreachable!()
    };
    let history_text = snapshot_text(&history_b_after_output);
    assert!(!history_text.contains("BOTTOM_NEW"), "{history_text:?}");
    assert!(!history_text.contains("HIST_40"), "{history_text:?}");

    harness.detach(&mut client_a).await;
    harness.detach(&mut client_b).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn wheel_flood_from_a_slow_reading_client_does_not_stall_the_daemon() {
    // Reproduces the interactive scroll freeze: every wheel event is answered
    // with a full snapshot, so a scroll burst fills the daemon->client socket
    // buffer faster than a busy client drains it. The daemon's send must not
    // block its read path, or input processing stops and the session freezes.
    let mut harness = Harness::start(
        "i=0; while [ $i -le 200 ]; do printf 'HIST_%03d\\r\\n' \"$i\"; i=$((i + 1)); done; while IFS= read -r line; do printf '%s\\n' \"$line\" >> input.log; done",
    )
    .await;
    let (mut connection, terminal_id, _) = harness.interactive().await;
    let log = harness.root.path().join("cwd/input.log");

    // Sanity: input reaches the terminal while the connection is drained.
    snapshot_containing(&mut connection, terminal_id, "HIST_200").await;
    send(
        &mut connection,
        ClientMessage::Input {
            bytes: b"alive\n".to_vec(),
        },
    )
    .await;
    wait_for(DEADLINE, || {
        fs::read_to_string(&log).is_ok_and(|text| text.contains("alive"))
    })
    .await;

    // Scroll flood without reading anything back. A send that cannot complete
    // means both socket buffers are full and the daemon has stopped reading.
    let mut sent = 0;
    let mut jammed = false;
    for _ in 0..5000 {
        let wheel = time::timeout(
            Duration::from_millis(200),
            send_uncorrelated(&mut connection, mouse_wheel_message(terminal_id)),
        )
        .await;
        if wheel.is_err() {
            jammed = true;
            break;
        }
        sent += 1;
    }

    // The daemon must keep consuming client input regardless of how far
    // behind this client's reads are: wheel frames still buffered by the
    // timed-out sends above can only flush if the daemon is draining them.
    let deadline = Instant::now() + DEADLINE;
    loop {
        match time::timeout(POLL_INTERVAL, connection.flush()).await {
            Ok(result) => {
                result.expect("flush wheel backlog");
                break;
            }
            Err(_) => assert!(
                Instant::now() < deadline,
                "daemon never drained the wheel backlog \
                 (write-blocked read loop); wheels sent={sent}, socket jammed={jammed}\n{}",
                harness.logs()
            ),
        }
    }
    send(
        &mut connection,
        ClientMessage::Input {
            bytes: b"done\n".to_vec(),
        },
    )
    .await;
    let deadline = Instant::now() + DEADLINE;
    loop {
        if fs::read_to_string(&log).is_ok_and(|text| text.contains("done")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon stopped processing input after a wheel flood \
             (write-blocked read loop); wheels sent={sent}, socket jammed={jammed}\n{}",
            harness.logs()
        );
        time::sleep(POLL_INTERVAL).await;
    }

    // The stream must still be coherent once the client resumes reading.
    send_uncorrelated(
        &mut connection,
        ClientMessage::ResetViewport { terminal_id },
    )
    .await;
    snapshot_containing(&mut connection, terminal_id, "done").await;
    harness.detach(&mut connection).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn focused_application_mouse_returns_only_its_attachment_viewport_to_bottom() {
    let expected = b"\x1b[<0;2;2M";
    let script = format!(
        "i=0; while [ $i -le 40 ]; do printf 'APP_HIST_%02d\\r\\n' \"$i\"; i=$((i + 1)); done; while [ ! -e app-mouse-go ]; do sleep 0.01; done; stty raw -echo; printf '\\033[?1000h\\033[?1006hAPP_MOUSE_READY\\r\\n'; dd bs=1 count={} of=app-mouse.capture 2>/dev/null; printf 'APP_MOUSE_CAPTURED\\r\\n'; exec cat >/dev/null",
        expected.len(),
    );
    let mut harness = Harness::start(&script).await;
    let resources = harness.resources().await;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let terminal_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'APP_PANE_B_READY\\r\\n'; sleep 60".into(),
            ],
        })
        .await
    else {
        panic!("failed to create second application-mouse pane")
    };
    let (mut client_a, selected_a, _) = harness
        .interactive_for(Some(TargetSelector::Terminal(terminal_a)))
        .await;
    assert_eq!(selected_a, terminal_a);
    let (mut client_b, selected_b) =
        attach_once(&harness, TargetSelector::Terminal(pane_b.terminal_id)).await;
    assert_eq!(selected_b, pane_b);
    let (mut readiness_client, readiness_target) =
        attach_once(&harness, TargetSelector::Terminal(terminal_a)).await;
    assert_eq!(readiness_target.terminal_id, terminal_a);
    snapshot_containing(&mut client_a, terminal_a, "APP_HIST_40").await;
    snapshot_containing(&mut client_b, terminal_a, "APP_HIST_40").await;
    snapshot_containing(&mut readiness_client, terminal_a, "APP_HIST_40").await;

    let wheel = mouse_wheel_message(terminal_a);
    send_uncorrelated(&mut client_a, wheel.clone()).await;
    send_uncorrelated(&mut client_b, wheel).await;
    let ServerMessage::Snapshot {
        screen: history_a, ..
    } = receive_matching(&mut client_a, |message| {
        matches!(message, ServerMessage::Snapshot { terminal_id, screen }
            if *terminal_id == terminal_a && !snapshot_text(screen).contains("APP_HIST_40"))
    })
    .await
    else {
        unreachable!()
    };
    let ServerMessage::Snapshot { .. } = receive_matching(&mut client_b, |message| {
        matches!(message, ServerMessage::Snapshot { terminal_id, screen }
            if *terminal_id == terminal_a && !snapshot_text(screen).contains("APP_HIST_40"))
    })
    .await
    else {
        unreachable!()
    };

    fs::write(harness.root.path().join("cwd/app-mouse-go"), b"").unwrap();
    snapshot_containing(&mut readiness_client, terminal_a, "APP_MOUSE_READY").await;
    let mut buttons = MouseButtons::default();
    buttons.set(MouseButton::Left, true);
    send_uncorrelated(
        &mut client_a,
        ClientMessage::MouseInput {
            terminal_id: terminal_a,
            event: MouseEvent {
                buttons,
                ..mouse_event(
                    MouseEventKind::Press {
                        button: MouseButton::Left,
                    },
                    1,
                    1,
                )
            },
        },
    )
    .await;
    let bottom_a = snapshot_containing(&mut client_a, terminal_a, "APP_MOUSE_READY").await;
    assert!(bottom_a.revision > history_a.revision);
    assert!(snapshot_text(&bottom_a).contains("APP_HIST_40"));
    wait_for(DEADLINE, || {
        fs::read(harness.root.path().join("cwd/app-mouse.capture"))
            .is_ok_and(|bytes| bytes.len() == expected.len())
    })
    .await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/app-mouse.capture")).unwrap(),
        expected
    );

    send_uncorrelated(
        &mut client_b,
        ClientMessage::RefreshTerminal {
            terminal_id: terminal_a,
        },
    )
    .await;
    let ServerMessage::Snapshot {
        screen: history_b_after_mouse,
        ..
    } = receive_matching(&mut client_b, |message| {
        matches!(message, ServerMessage::Snapshot { terminal_id, .. }
            if *terminal_id == terminal_a)
    })
    .await
    else {
        unreachable!()
    };
    let history_text = snapshot_text(&history_b_after_mouse);
    assert!(
        !history_text.contains("APP_MOUSE_READY"),
        "{history_text:?}"
    );
    assert!(!history_text.contains("APP_HIST_40"), "{history_text:?}");

    harness.detach(&mut client_a).await;
    harness.detach(&mut client_b).await;
    harness.detach(&mut readiness_client).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn raw_mouse_input_reaches_only_the_attachments_focused_terminal() {
    let expected_a = b"\x1b[<0;2;2M";
    let expected_b = b"\x1b[<2;3;2M";
    let script = format!(
        "stty raw -echo; printf '\\033[?1000h\\033[?1006hRAW_MOUSE_A_READY\\r\\n'; dd bs=1 count={} of=raw-mouse-a.capture 2>/dev/null; printf 'RAW_MOUSE_A_CAPTURED\\r\\n'; exec cat >/dev/null",
        expected_a.len(),
    );
    let mut harness = Harness::start(&script).await;
    let resources = harness.resources().await;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let terminal_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                format!(
                    "stty raw -echo; printf '\\033[?1000h\\033[?1006hRAW_MOUSE_B_READY\\r\\n'; dd bs=1 count={} of=raw-mouse-b.capture 2>/dev/null; printf 'RAW_MOUSE_B_CAPTURED\\r\\n'; exec cat >/dev/null",
                    expected_b.len(),
                ),
            ],
        })
        .await
    else {
        panic!("failed to create raw mouse sibling")
    };
    let (mut connection, selected, _) = harness
        .interactive_for(Some(TargetSelector::Terminal(terminal_a)))
        .await;
    assert_eq!(selected, terminal_a);
    snapshot_containing(&mut connection, terminal_a, "RAW_MOUSE_A_READY").await;
    snapshot_containing(&mut connection, pane_b.terminal_id, "RAW_MOUSE_B_READY").await;

    for event in [
        mouse_event(
            MouseEventKind::Press {
                button: MouseButton::Left,
            },
            1,
            1,
        ),
        MouseEvent {
            buttons: MouseButtons {
                middle: true,
                ..Default::default()
            },
            ..mouse_event(
                MouseEventKind::Release {
                    button: MouseButton::Middle,
                },
                1,
                1,
            )
        },
        mouse_event(
            MouseEventKind::Motion {
                button: Some(MouseButton::Right),
            },
            1,
            1,
        ),
    ] {
        send_uncorrelated(
            &mut connection,
            ClientMessage::MouseInput {
                terminal_id: terminal_a,
                event,
            },
        )
        .await;
    }
    assert_no_error_before_pong(&mut connection).await;

    let press = |button, column, row| {
        let mut buttons = MouseButtons::default();
        buttons.set(button, true);
        MouseEvent {
            buttons,
            ..mouse_event(MouseEventKind::Press { button }, column, row)
        }
    };
    send_uncorrelated(
        &mut connection,
        ClientMessage::MouseInput {
            terminal_id: pane_b.terminal_id,
            event: press(MouseButton::Left, 0, 0),
        },
    )
    .await;
    send_uncorrelated(
        &mut connection,
        ClientMessage::MouseInput {
            terminal_id: terminal_a,
            event: press(MouseButton::Left, 1, 1),
        },
    )
    .await;
    snapshot_containing(&mut connection, terminal_a, "RAW_MOUSE_A_CAPTURED").await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/raw-mouse-a.capture")).unwrap(),
        expected_a
    );

    send(
        &mut connection,
        ClientMessage::SelectTarget {
            selector: TargetSelector::Pane(pane_b.pane_id),
            expected: Some(fut::protocol::SelectionExpectation::Tab(tab_id)),
        },
    )
    .await;
    receive_matching(&mut connection, |message| {
        matches!(
            message,
            ServerMessage::TargetSelected { selected }
                if selected.focused.terminal_id == pane_b.terminal_id
        )
    })
    .await;
    send_uncorrelated(
        &mut connection,
        ClientMessage::MouseInput {
            terminal_id: pane_b.terminal_id,
            event: press(MouseButton::Right, 2, 1),
        },
    )
    .await;
    snapshot_containing(&mut connection, pane_b.terminal_id, "RAW_MOUSE_B_CAPTURED").await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/raw-mouse-b.capture")).unwrap(),
        expected_b
    );

    harness.detach(&mut connection).await;
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
            match response.message {
                ServerMessage::Snapshot { terminal_id, .. } => {
                    assert_eq!(terminal_id, old_terminal);
                }
                ServerMessage::ResourcesChanged { .. } => {}
                other => panic!("unexpected frame before CreateTab response: {other:?}"),
            }
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
            receive_matching(&mut connection, |message| {
                matches!(message, ServerMessage::TargetSelected { selected } if selected.focused.terminal_id == old_terminal)
            })
            .await;
            send(
                &mut connection,
                ClientMessage::Input {
                    bytes: b"FELL_BACK\n".to_vec(),
                },
            )
            .await;
            snapshot_containing(&mut connection, old_terminal, "OLD_FELL_BACK").await;
            harness.detach(&mut connection).await;
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
            match response.message {
                ServerMessage::Snapshot { terminal_id, .. } => {
                    assert_eq!(terminal_id, terminal_a);
                }
                ServerMessage::ResourcesChanged { .. } => {}
                other => panic!("unexpected frame before CreatePane response: {other:?}"),
            }
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
                        assert!(
                            terminal_id == selected.terminal_id || terminal_id == terminal_a,
                            "snapshot came from an unrelated terminal"
                        );
                        if terminal_id == selected.terminal_id {
                            saw_final_snapshot |= snapshot_text(&screen).contains("PANE_B_FINAL");
                        }
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
async fn top_level_attach_is_navigator_only_until_selection_then_attaches_normally() {
    let mut harness =
        Harness::start("printf 'PRIMARY_READY\\r\\n'; while IFS= read -r line; do :; done").await;
    let primary = harness.resources().await.sessions[0].workspaces[0].tabs[0].panes[0].clone();
    let other = harness.root.path().join("other-session");
    fs::create_dir(&other).unwrap();
    let opened = harness
        .cli()
        .args([
            "open",
            "--background",
            other.to_str().unwrap(),
            "--name",
            "other-session",
            "--",
            "/bin/sh",
            "-c",
            "printf 'SECONDARY_READY\\r\\n'; while IFS= read -r line; do :; done",
        ])
        .output()
        .unwrap();
    assert!(opened.status.success());
    let before = harness.resources().await;
    assert_eq!(before.sessions.len(), 2);
    let attach_home = harness.root.path().join("home");
    let attach_runtime = harness.root.path().join("runtime");
    let attach_socket = harness.socket.clone();

    let spawn_attach = || {
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("HOME", &attach_home)
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", &attach_runtime)
            .env("FUT_RUNTIME_DIR", &attach_runtime)
            .env("TERM", "xterm-256color")
            .args(["-q", "/dev/null", "/bin/sh", "-c"])
            .arg(format!(
                "stty rows 24 cols 80; exec '{}' --socket '{}' attach",
                env!("CARGO_BIN_EXE_fut"),
                attach_socket.display()
            ));
        PtyChild::spawn(command)
    };

    let mut cancelled = spawn_attach();
    cancelled.wait_for("other-session").await;
    let (mut contender, _, _) = harness
        .interactive_for(Some(TargetSelector::Terminal(primary.terminal_id)))
        .await;
    harness.detach(&mut contender).await;
    drop(contender);
    cancelled.send(b"\x1b");
    cancelled.wait_success().await;
    assert_eq!(
        without_observations(harness.resources().await),
        without_observations(before.clone())
    );

    let mut selected = spawn_attach();
    selected.wait_for("other-session").await;
    selected.send(b"\x1b[200~other\x1b[201~\r");
    selected.wait_for("SECONDARY_READY").await;
    selected.send(b"\x02d");
    selected.wait_success().await;
    assert_eq!(
        without_observations(harness.resources().await),
        without_observations(before.clone())
    );

    harness.shutdown().await;
}

#[test]
fn top_level_attach_does_not_start_a_missing_daemon_or_create_cwd_resources() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("runtime");
    let cwd = root.path().join("cwd");
    fs::create_dir(&cwd).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", root.path())
        .env("FUT_RUNTIME_DIR", &runtime)
        .env("TERM", "xterm-256color")
        .current_dir(&cwd)
        .args([
            "--socket",
            root.path().join("missing.sock").to_str().unwrap(),
            "attach",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("connect to"));
    assert!(!runtime.exists());
    assert!(fs::read_dir(cwd).unwrap().next().is_none());
}

#[tokio::test]
async fn location_aware_layout_commands_follow_live_terminal_ancestry_and_guard_races() {
    let mut harness = Harness::start("while IFS= read -r line; do :; done").await;
    let initial = harness.resources().await;
    let session_id = initial.sessions[0].id;
    let workspace_id = initial.sessions[0].workspaces[0].id;
    let original_tab_id = initial.sessions[0].workspaces[0].tabs[0].id;
    let anchor = initial.sessions[0].workspaces[0].tabs[0].panes[0].clone();
    let original_context = fut::protocol::TerminalContext {
        session_id,
        workspace_id,
        tab_id: original_tab_id,
        pane_id: anchor.id,
        terminal_id: anchor.terminal_id,
    };
    let run = |arguments: &[&str], terminal_id: TerminalId| {
        harness
            .cli()
            .env("FUT_SESSION_ID", SessionId::new().to_string())
            .env("FUT_WORKSPACE_ID", WorkspaceId::new().to_string())
            .env("FUT_TAB_ID", TabId::new().to_string())
            .env("FUT_PANE_ID", PaneId::new().to_string())
            .env("FUT_TERMINAL_ID", terminal_id.to_string())
            .args(arguments)
            .output()
            .unwrap()
    };
    let success_json = |output: std::process::Output| {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };

    assert!(
        run(&["session", "rename", "self-session"], anchor.terminal_id)
            .status
            .success()
    );
    assert!(
        run(
            &["workspace", "rename", "self-workspace"],
            anchor.terminal_id
        )
        .status
        .success()
    );
    let tabs = success_json(run(&["--json", "tab", "list"], anchor.terminal_id));
    assert_eq!(tabs["result"]["workspace_id"], workspace_id.to_string());
    let panes = success_json(run(&["--json", "pane", "list"], anchor.terminal_id));
    assert_eq!(panes["result"]["tab_id"], original_tab_id.to_string());

    let second_tab = success_json(run(
        &["--json", "tab", "new", "--name", "destination"],
        anchor.terminal_id,
    ));
    let second_tab_id: TabId = second_tab["result"]["selected"]["tab_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let second_terminal: TerminalId = second_tab["result"]["selected"]["terminal_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let appended = success_json(run(&["--json", "pane", "new"], anchor.terminal_id));
    let appended_terminal: TerminalId = appended["result"]["selected"]["terminal_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let split = success_json(run(
        &["--json", "pane", "split", "down"],
        anchor.terminal_id,
    ));
    assert_eq!(split["result"]["direction"], "down");
    assert!(
        run(&["tab", "rename", "source-renamed"], anchor.terminal_id)
            .status
            .success()
    );

    let moved = success_json(run(
        &["--json", "pane", "move", &second_tab_id.to_string()],
        anchor.terminal_id,
    ));
    assert_eq!(
        moved["result"]["selected"]["pane_id"],
        anchor.id.to_string()
    );
    assert!(
        run(
            &["tab", "rename", "destination-renamed"],
            anchor.terminal_id
        )
        .status
        .success()
    );
    let moved_panes = success_json(run(&["--json", "pane", "list"], anchor.terminal_id));
    assert_eq!(moved_panes["result"]["tab_id"], second_tab_id.to_string());

    assert!(matches!(
        harness
            .control_command(ClientMessage::Contextual {
                context: original_context,
                command: fut::protocol::ContextualCommand::Rename {
                    scope: fut::protocol::ContextScope::Tab,
                    name: "wrong-old-tab".into(),
                },
            })
            .await,
        ServerMessage::Error { code, .. } if code == "context_changed"
    ));
    let snapshot = harness.resources().await;
    assert_eq!(snapshot.sessions[0].name, "self-session");
    assert_eq!(snapshot.sessions[0].workspaces[0].name, "self-workspace");
    assert_eq!(
        snapshot.sessions[0].workspaces[0].tabs[0].name,
        "source-renamed"
    );

    assert!(run(&["pane", "close"], anchor.terminal_id).status.success());
    assert!(run(&["tab", "close"], second_terminal).status.success());
    assert!(
        run(&["workspace", "close"], appended_terminal)
            .status
            .success()
    );
    harness.wait_until_exited().await;

    let mut session_harness = Harness::start("while IFS= read -r line; do :; done").await;
    let terminal_id =
        session_harness.resources().await.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let closed = session_harness
        .cli()
        .env("FUT_TERMINAL_ID", terminal_id.to_string())
        .args(["session", "close"])
        .output()
        .unwrap();
    assert!(closed.status.success());
    session_harness.wait_until_exited().await;
}

#[tokio::test]
async fn inferred_layout_commands_fail_typed_without_a_live_terminal_and_do_not_mutate() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let before = resources_when(&harness, |snapshot| {
        !snapshot.sessions[0].workspaces[0].tabs[0].name.is_empty()
    })
    .await;
    for (terminal, code) in [
        (None, "missing_context"),
        (Some("not-a-uuid".to_owned()), "invalid_context"),
        (Some(TerminalId::new().to_string()), "stale_context"),
    ] {
        let mut command = harness.cli();
        if let Some(terminal) = terminal {
            command.env("FUT_TERMINAL_ID", terminal);
        }
        let output = command
            .args(["--json", "tab", "rename", "must-not-apply"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["code"], code);
        assert_eq!(
            without_observations(harness.resources().await),
            without_observations(before.clone())
        );
    }
    let tab_id = before.sessions[0].workspaces[0].tabs[0].id;
    let explicit = harness
        .cli()
        .env("FUT_TERMINAL_ID", "malformed-but-ignored")
        .args(["tab", "rename", &tab_id.to_string(), "explicit-ok"])
        .output()
        .unwrap();
    assert!(explicit.status.success());
    assert_eq!(
        harness.resources().await.sessions[0].workspaces[0].tabs[0].name,
        "explicit-ok"
    );
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
            .args(["--json", "open", "--background"])
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
        .args(["open", "--background"])
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
    json_error(
        harness.cli().args(["--json", "attach"]).output().unwrap(),
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
    let pane_id = resources.sessions[0].workspaces[0].tabs[0].panes[0]
        .id
        .to_string();

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
            vec!["pane", "split", &pane_id, "right", "/bin/sh", "-c", "touch"],
            "pane-split-no-delimiter",
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
async fn public_pane_split_preserves_target_direction_cwd_argv_focus_and_atomic_failures() {
    let mut harness = Harness::start(
        "while IFS= read -r line; do [ \"$line\" = focus ] && touch ../focus-stayed; done",
    )
    .await;
    let initial_tab = harness.resources().await.sessions[0].workspaces[0].tabs[0].id;
    assert!(matches!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Tab(initial_tab),
                name: "stable".into(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    let before = harness.resources().await;
    let anchor = before.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let tab_id = before.sessions[0].workspaces[0].tabs[0].id;
    let split_cwd = harness.root.path().join("split-cwd");
    fs::create_dir(&split_cwd).unwrap();
    let argv_capture = harness.root.path().join("split-argv");

    let (mut attached, attached_terminal, _) = harness.interactive().await;
    let output = harness
        .cli()
        .args(["--json", "pane", "split", &anchor.to_string(), "right"])
        .arg("--cwd")
        .arg(&split_cwd)
        .args(["--", "/bin/sh", "-c"])
        .arg(format!(
            "printf '%s\\n' \"$PWD\" \"$0\" \"$1\" \"$2\" > '{}'; while IFS= read -r line; do :; done",
            argv_capture.display()
        ))
        .args(["sentinel", "argument with spaces", "--literal-flag"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let split: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(split["version"], 1);
    assert_eq!(split["command"], "pane.split");
    assert_eq!(split["result"]["anchor_pane_id"], anchor.to_string());
    assert_eq!(split["result"]["direction"], "right");
    let right_pane: PaneId = split["result"]["selected"]["pane_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(split["result"]["selected"]["terminal_id"].is_string());
    wait_for_nonempty_file(&argv_capture).await;
    assert_eq!(
        fs::read_to_string(&argv_capture).unwrap(),
        format!(
            "{}\nsentinel\nargument with spaces\n--literal-flag\n",
            split_cwd.canonicalize().unwrap().display()
        )
    );

    send(
        &mut attached,
        ClientMessage::Input {
            bytes: b"focus\n".to_vec(),
        },
    )
    .await;
    wait_for(DEADLINE, || {
        harness.root.path().join("focus-stayed").exists()
    })
    .await;
    assert_eq!(
        attached_terminal,
        before.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id
    );
    harness.detach(&mut attached).await;

    let down = harness
        .cli()
        .args(["--json", "pane", "split", &right_pane.to_string(), "down"])
        .output()
        .unwrap();
    assert!(
        down.status.success(),
        "{}",
        String::from_utf8_lossy(&down.stderr)
    );
    let down: Value = serde_json::from_slice(&down.stdout).unwrap();
    let down_pane: PaneId = down["result"]["selected"]["pane_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let resources = harness.resources().await;
    let tab = &resources.sessions[0].workspaces[0].tabs[0];
    assert_eq!(tab.id, tab_id);
    assert_eq!(tab.layout.leaf_ids(), vec![anchor, right_pane, down_pane]);
    assert!(matches!(
        &tab.layout,
        fut::splits::SplitTree::Branch {
            axis: fut::splits::SplitAxis::Horizontal,
            second,
            ..
        } if matches!(
            second.as_ref(),
            fut::splits::SplitTree::Branch {
                axis: fut::splits::SplitAxis::Vertical,
                ..
            }
        )
    ));

    let stable = harness.resources().await;
    for output in [
        harness
            .cli()
            .args([
                "--json",
                "pane",
                "split",
                &Uuid::new_v4().to_string(),
                "right",
            ])
            .output()
            .unwrap(),
        harness
            .cli()
            .args(["--json", "pane", "split", &anchor.to_string(), "down"])
            .args(["--", "/definitely/missing/fut-pane-split"])
            .output()
            .unwrap(),
        harness
            .cli()
            .args(["--json", "pane", "split", &anchor.to_string(), "down"])
            .arg("--cwd")
            .arg(harness.root.path().join("missing-split-cwd"))
            .output()
            .unwrap(),
    ] {
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(matches!(
            error["error"]["code"].as_str(),
            Some("not_found" | "spawn_failed" | "invalid_cwd")
        ));
        assert_eq!(
            without_observations(harness.resources().await),
            without_observations(stable.clone())
        );
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn public_terminal_input_is_literal_validated_bracket_aware_atomic_and_exit_typed() {
    let harness = Harness::start("while :; do sleep 1; done").await;
    let resources = harness.resources().await;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let plain_capture = harness.root.path().join("plain-input.bin");
    let plain_ready = harness.root.path().join("plain-input.ready");
    let bracket_capture = harness.root.path().join("bracket-input.bin");
    let bracket_ready = harness.root.path().join("bracket-input.ready");
    let plain_expected = ["hé 雪".as_bytes(), b"\x03\x1b[D\r", b"printf plain\r"].concat();
    let bracket_expected = [
        b"\x1b[200~".as_slice(),
        "雪\nnext".as_bytes(),
        b"\x1b[201~\x1bx\x1bOQ\x1b[200~echo bracket\x1b[201~\r",
    ]
    .concat();
    let capture_script = |ready: &std::path::Path,
                          capture: &std::path::Path,
                          bracketed: bool,
                          length: usize| {
        format!(
            "stty raw -echo; printf '{}INPUT_READY\\r\\n'; : > '{}'; dd bs=1 count={length} of='{}' 2>/dev/null; while :; do sleep 1; done",
            if bracketed { "\\033[?2004h" } else { "" },
            ready.display(),
            capture.display(),
        )
    };
    let create_capture = |script| ClientMessage::CreatePane {
        tab_id,
        cwd: None,
        program: Some("/bin/sh".into()),
        argv: vec!["-c".into(), script],
    };
    let ServerMessage::PaneCreated { selected: plain } = harness
        .control_command(create_capture(capture_script(
            &plain_ready,
            &plain_capture,
            false,
            plain_expected.len(),
        )))
        .await
    else {
        panic!("failed to create plain input capture terminal")
    };
    let ServerMessage::PaneCreated { selected: bracket } = harness
        .control_command(create_capture(capture_script(
            &bracket_ready,
            &bracket_capture,
            true,
            bracket_expected.len(),
        )))
        .await
    else {
        panic!("failed to create bracketed input capture terminal")
    };
    wait_for(DEADLINE, || plain_ready.exists() && bracket_ready.exists()).await;
    let (mut bracket_probe, _) =
        attach_once(&harness, TargetSelector::Terminal(bracket.terminal_id)).await;
    snapshot_containing(&mut bracket_probe, bracket.terminal_id, "INPUT_READY").await;
    harness.detach(&mut bracket_probe).await;

    let json = |output: std::process::Output, command: &str, terminal_id: TerminalId| {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["command"], command);
        assert_eq!(value["result"]["terminal_id"], terminal_id.to_string());
    };

    json(
        harness
            .cli()
            .args([
                "--json",
                "terminal",
                "send-text",
                &plain.terminal_id.to_string(),
                "hé 雪",
            ])
            .output()
            .unwrap(),
        "terminal.send-text",
        plain.terminal_id,
    );
    let invalid = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "send-keys",
            &plain.terminal_id.to_string(),
            "ctrl+c",
            "no-such-key",
        ])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(invalid.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&invalid.stderr).unwrap()["error"]["code"],
        "invalid_arguments"
    );
    json(
        harness
            .cli()
            .args([
                "--json",
                "terminal",
                "send-keys",
                &plain.terminal_id.to_string(),
                "ctrl+c",
                "left",
                "enter",
            ])
            .output()
            .unwrap(),
        "terminal.send-keys",
        plain.terminal_id,
    );
    json(
        harness
            .cli()
            .args([
                "--json",
                "terminal",
                "run",
                &plain.terminal_id.to_string(),
                "printf plain",
            ])
            .output()
            .unwrap(),
        "terminal.run",
        plain.terminal_id,
    );

    json(
        harness
            .cli()
            .args([
                "--json",
                "terminal",
                "send-text",
                &bracket.terminal_id.to_string(),
                "雪\nnext",
            ])
            .output()
            .unwrap(),
        "terminal.send-text",
        bracket.terminal_id,
    );
    json(
        harness
            .cli()
            .args([
                "--json",
                "terminal",
                "send-keys",
                &bracket.terminal_id.to_string(),
                "alt+x",
                "f2",
            ])
            .output()
            .unwrap(),
        "terminal.send-keys",
        bracket.terminal_id,
    );
    json(
        harness
            .cli()
            .args([
                "--json",
                "terminal",
                "run",
                &bracket.terminal_id.to_string(),
                "echo bracket",
            ])
            .output()
            .unwrap(),
        "terminal.run",
        bracket.terminal_id,
    );
    wait_for(DEADLINE, || {
        fs::read(&plain_capture).is_ok_and(|bytes| bytes.len() == plain_expected.len())
            && fs::read(&bracket_capture).is_ok_and(|bytes| bytes.len() == bracket_expected.len())
    })
    .await;
    assert_eq!(fs::read(&plain_capture).unwrap(), plain_expected);
    assert_eq!(fs::read(&bracket_capture).unwrap(), bracket_expected);

    let ServerMessage::PaneCreated { selected: exited } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "exit 17".into()],
        })
        .await
    else {
        panic!("failed to create exiting terminal")
    };
    time::timeout(DEADLINE, async {
        loop {
            let resources = harness.resources().await;
            if resources
                .sessions
                .iter()
                .flat_map(|session| &session.workspaces)
                .flat_map(|workspace| &workspace.tabs)
                .flat_map(|tab| &tab.panes)
                .all(|pane| pane.terminal_id != exited.terminal_id)
            {
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("exited terminal was not finalized");
    let after_exit = harness
        .cli()
        .args([
            "--json",
            "terminal",
            "run",
            &exited.terminal_id.to_string(),
            "echo too late",
        ])
        .output()
        .unwrap();
    assert!(!after_exit.status.success());
    assert!(after_exit.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&after_exit.stderr).unwrap()["error"]["code"],
        "terminal_exited"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn public_events_stream_emits_versioned_json_snapshots_on_change() {
    let mut harness = Harness::start("while IFS= read -r line; do :; done").await;
    let mut events = PtyChild::spawn({
        let mut command = harness.cli();
        command.arg("events");
        command
    });
    events.wait_for("\"command\":\"events\"").await;
    let first: Value = serde_json::from_str(events.text().lines().next().unwrap()).unwrap();
    assert_eq!(first["version"], 1);
    assert_eq!(first["command"], "events");
    let first_revision = first["result"]["revision"].as_u64().unwrap();
    let session_id = first["result"]["sessions"][0]["id"].as_str().unwrap();

    // A mutation through any other client streams a new snapshot line.
    assert_eq!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Session(SessionSelector::Id(session_id.parse().unwrap())),
                name: "watched λ".into(),
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::RenameTarget,
        }
    );
    events.wait_for("watched λ").await;
    let last: Value = serde_json::from_str(events.text().lines().last().unwrap()).unwrap();
    assert_eq!(last["version"], 1);
    assert_eq!(last["command"], "events");
    assert!(last["result"]["revision"].as_u64().unwrap() > first_revision);
    assert_eq!(last["result"]["sessions"][0]["name"], "watched λ");

    // Agent activity reports stream too.
    let pane = &last["result"]["sessions"][0]["workspaces"][0]["tabs"][0]["panes"][0];
    assert_eq!(pane["activity"]["state"], "idle");
    let terminal_id: TerminalId = pane["terminal_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        harness
            .control_command(ClientMessage::ReportAgent {
                terminal_id,
                report: AgentReport::Blocked,
                metadata: Default::default(),
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::ReportAgent,
        }
    );
    events.wait_for("\"state\":\"blocked\"").await;
    let last: Value = serde_json::from_str(events.text().lines().last().unwrap()).unwrap();
    assert_eq!(
        last["result"]["sessions"][0]["workspaces"][0]["tabs"][0]["panes"][0]["activity"]["state"],
        "blocked"
    );

    // Interactive connections cannot subscribe; the stream is control-only.
    let (mut interactive, _, _) = harness.interactive().await;
    let rejected_request = Uuid::new_v4();
    send_envelope(
        &mut interactive,
        Envelope {
            request_id: Some(rejected_request),
            message: ClientMessage::WatchResources,
        },
    )
    .await;
    loop {
        let response = receive_envelope(&mut interactive)
            .await
            .expect("interactive connection closed during rejected watch");
        if response.request_id == Some(rejected_request) {
            assert!(matches!(
                response.message,
                ServerMessage::Error { ref code, .. } if code == "control_only"
            ));
            break;
        }
    }
    harness.detach(&mut interactive).await;

    // Daemon shutdown ends the stream cleanly.
    harness.shutdown().await;
    events.wait_success().await;
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
    json(
        &[
            "open",
            "--background",
            workspace_dir.to_str().unwrap(),
            "--json",
        ],
        "open",
    );
    json(
        &[
            "open",
            "--background",
            session_dir.to_str().unwrap(),
            "--json",
        ],
        "open",
    );
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
        project: None,
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
async fn public_last_session_navigation_toggles_after_global_selection() {
    let harness = Harness::start(
        "printf 'LAST_SESSION_A_READY\r\n'; while IFS= read -r line; do [ \"$line\" = a ] && { printf x >> last-session-a-input; printf 'LAST_SESSION_A_INPUT\r\n'; }; done",
    )
    .await;
    let second_root = harness.root.path().join("second-project");
    fs::create_dir(&second_root).unwrap();
    let ServerMessage::LocationOpened { selected: second, .. } = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
            name: Some("second-project".into()),
            cwd: second_root,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'LAST_SESSION_B_READY\r\n'; while IFS= read -r line; do [ \"$line\" = b ] && { printf x >> last-session-b-input; printf 'LAST_SESSION_B_INPUT\r\n'; }; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create second session")
    };
    let snapshot = harness.resources().await;
    let first_pane = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let first_input = harness.root.path().join("cwd/last-session-a-input");
    let second_input = harness
        .root
        .path()
        .join("second-project/last-session-b-input");

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
            "stty rows 24 cols 90; exec '{}' --socket '{}' pane attach {first_pane}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("LAST_SESSION_A_READY").await;
    client.send(b"\x02s");
    client.wait_for("second-project").await;
    client.send(b"\x1b[F\r");
    // The diff renderer may reuse the stale "LAST_" cells from marker A on
    // the same row, so only the differing suffix reliably reaches the PTY.
    client.wait_for("SESSION_B_READY").await;
    client.send(b"b\n");
    wait_for(DEADLINE, || second_input.exists()).await;
    client.send(b"\x02\x13a\n");
    wait_for(DEADLINE, || first_input.exists()).await;
    client.send(b"\x02\x13b\n");
    wait_for(DEADLINE, || {
        fs::metadata(&second_input).is_ok_and(|metadata| metadata.len() >= 2)
    })
    .await;
    client.send(b"\x02d");
    client.wait_success().await;

    assert!(process_alive(second.child_pid));
    harness.shutdown().await;
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
        project: None,
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
    let reopened = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
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
    assert_eq!(
        without_observations(harness.resources().await),
        without_observations(snapshot.clone()),
        "reopening an existing worktree must not mutate resource structure"
    );
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
async fn trusted_catalog_recipe_atomically_creates_linked_and_bare_workspaces_once() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let main = root.join("main");
        fs::create_dir(&main).unwrap();
        git(&main, &["init", "-b", "main"]);
        fs::write(main.join("tracked"), "x").unwrap();
        git(&main, &["add", "tracked"]);
        git(&main, &["commit", "-m", "initial"]);
        let linked = root.join("linked-recipe");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "recipe-linked",
                linked.to_str().unwrap(),
            ],
        );
        for checkout in [&main, &linked] {
            fs::create_dir(checkout.join("frontend")).unwrap();
            fs::create_dir(checkout.join("agent")).unwrap();
        }
        let recipe = r#"version = 1
focus = "code.agent"
environment = { PROJECT_LEVEL = "workspace", CASCADE = "workspace" }

[[tabs]]
id = "code"
cwd = "frontend"
environment = { CASCADE = "tab" }
panes = [
  { id = "editor", command = ["/bin/sh", "-c", 'printf "%s|%s|%s" "$PWD" "$PROJECT_LEVEL" "$CASCADE" > editor.marker; while :; do sleep 1; done'] },
  { id = "agent", cwd = "agent", environment = { CASCADE = "pane" }, command = ["/bin/sh", "-c", 'printf "%s|%s|%s" "$PWD" "$PROJECT_LEVEL" "$CASCADE" > agent.marker; while :; do sleep 1; done'], split = { target = "editor", direction = "right" } },
]

[[tabs]]
id = "server"
name = "development server"
panes = [
  { id = "server", command = ["/bin/sh", "-c", 'printf "%s|%s" "$PROJECT_LEVEL" "$CASCADE" > server.marker; while :; do sleep 1; done'] },
]
"#;
        fs::create_dir(main.join(".fut")).unwrap();
        fs::write(main.join(".fut/project.toml"), recipe).unwrap();
        let config_dir = root.join("home/.config/fut");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            format!("[projects.fut]\npath = {:?}\n", main),
        )
        .unwrap();
        let trusted = project_trust_cli(root, "trust", "fut");
        assert_eq!(trusted["command"], "project.trust");
        assert_eq!(trusted["version"], 1);
        assert_eq!(trusted["result"]["name"], "fut");
        assert_eq!(trusted["result"]["trusted"], true);
        assert_eq!(trusted["result"]["changed"], true);
    })
    .await;
    let linked = harness.root.path().join("linked-recipe");
    let main = harness.root.path().join("main");

    let mut watcher = harness.connect().await.unwrap();
    assert!(matches!(
        hello(&mut watcher, ClientMode::Control, PROTOCOL_VERSION)
            .await
            .unwrap(),
        ServerMessage::Welcome { .. }
    ));
    send(&mut watcher, ClientMessage::WatchResources).await;
    assert!(matches!(
        receive(&mut watcher).await.unwrap(),
        ServerMessage::Resources { .. }
    ));

    let opened = harness
        .control_command(ClientMessage::OpenLocation {
            project: Some("fut".into()),
            name: Some("recipe-project".into()),
            cwd: linked.clone(),
            program: None,
            argv: Vec::new(),
        })
        .await;
    let ServerMessage::LocationOpened {
        selected: linked_focus,
        disposition: fut::protocol::OpenDisposition::SessionCreated,
    } = opened
    else {
        panic!("recipe project did not create a session: {opened:?}")
    };
    let published = receive_matching(&mut watcher, |message| {
        matches!(message, ServerMessage::ResourcesChanged { snapshot } if snapshot.sessions.iter().any(|session| session.id == linked_focus.session_id))
    })
    .await;
    let ServerMessage::ResourcesChanged { snapshot } = published else {
        unreachable!()
    };
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == linked_focus.session_id)
        .unwrap();
    assert_eq!(session.workspaces.len(), 1);
    let workspace = &session.workspaces[0];
    assert_eq!(workspace.tabs.len(), 2);
    assert_eq!(workspace.tabs[0].name, "code");
    assert_eq!(workspace.tabs[1].name, "development server");
    assert_eq!(workspace.tabs[0].panes.len(), 2);
    assert_eq!(workspace.tabs[1].panes.len(), 1);
    assert_eq!(workspace.tabs[0].layout.leaf_ids().len(), 2);
    assert_eq!(workspace.tabs[0].panes[1].id, linked_focus.pane_id);
    assert_eq!(
        workspace.tabs[0].panes[1].terminal_id,
        linked_focus.terminal_id
    );

    wait_for(DEADLINE, || {
        linked.join("frontend/editor.marker").exists()
            && linked.join("agent/agent.marker").exists()
            && linked.join("server.marker").exists()
    })
    .await;
    assert_eq!(
        fs::read_to_string(linked.join("frontend/editor.marker")).unwrap(),
        format!(
            "{}|workspace|tab",
            linked.join("frontend").canonicalize().unwrap().display()
        )
    );
    assert_eq!(
        fs::read_to_string(linked.join("agent/agent.marker")).unwrap(),
        format!(
            "{}|workspace|pane",
            linked.join("agent").canonicalize().unwrap().display()
        )
    );
    assert_eq!(
        fs::read_to_string(linked.join("server.marker")).unwrap(),
        "workspace|workspace"
    );

    let bare = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
            name: Some("main".into()),
            cwd: main.clone(),
            program: None,
            argv: Vec::new(),
        })
        .await;
    let ServerMessage::LocationOpened {
        selected: main_focus,
        disposition: fut::protocol::OpenDisposition::WorkspaceCreated,
    } = bare
    else {
        panic!("bare catalog identity did not create a recipe workspace: {bare:?}")
    };
    assert_eq!(main_focus.session_id, linked_focus.session_id);
    wait_for(DEADLINE, || {
        main.join("frontend/editor.marker").exists()
            && main.join("agent/agent.marker").exists()
            && main.join("server.marker").exists()
    })
    .await;
    let snapshot = harness.resources().await;
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == linked_focus.session_id)
        .unwrap();
    assert_eq!(session.workspaces.len(), 2);
    let main_workspace = session
        .workspaces
        .iter()
        .find(|workspace| workspace.id == main_focus.workspace_id)
        .unwrap();
    assert_eq!(main_workspace.tabs.len(), 2);
    assert_eq!(main_workspace.tabs[0].panes[1].id, main_focus.pane_id);

    fs::write(
        main.join(".fut/project.toml"),
        "this is no longer valid TOML",
    )
    .unwrap();
    let ignored_marker = linked.join("must-not-run");
    let reopened = harness
        .control_command(ClientMessage::OpenLocation {
            project: Some("fut".into()),
            name: Some("ignored".into()),
            cwd: linked,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "touch must-not-run".into()],
        })
        .await;
    assert!(
        matches!(
            &reopened,
            ServerMessage::LocationOpened {
                selected,
                disposition: fut::protocol::OpenDisposition::Existing,
            } if selected.workspace_id == linked_focus.workspace_id
        ),
        "unexpected existing recipe response: {reopened:?}"
    );
    assert!(!ignored_marker.exists());
    harness.shutdown().await;
}

#[tokio::test]
async fn repository_recipe_trust_change_and_untrust_apply_without_daemon_restart() {
    let harness = Harness::start_with("while :; do sleep 1; done", |root| {
        let project = root.join("untrusted-project");
        fs::create_dir_all(project.join(".fut")).unwrap();
        fs::write(
            project.join(".fut/project.toml"),
            r#"version = 1
tabs = [{ id = "unsafe", panes = [{ id = "unsafe", command = ["/bin/sh", "-c", "touch untrusted-marker; while :; do sleep 1; done"] }] }]
"#,
        )
        .unwrap();
        let config_dir = root.join("home/.config/fut");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            format!("[projects.unsafe]\npath = {:?}\n", project),
        )
        .unwrap();
    })
    .await;
    let project = harness.root.path().join("untrusted-project");
    let before = without_observations(harness.resources().await);
    let response = harness
        .control_command(ClientMessage::OpenLocation {
            project: Some("unsafe".into()),
            name: None,
            cwd: project.clone(),
            program: None,
            argv: Vec::new(),
        })
        .await;
    assert!(
        matches!(
            &response,
            ServerMessage::Error { code, message }
                if code == "untrusted_recipe"
                    && message.contains("fut project trust unsafe")
        ),
        "unexpected untrusted response: {response:?}"
    );
    assert_eq!(without_observations(harness.resources().await), before);
    assert!(!project.join("untrusted-marker").exists());

    let trusted = project_trust_cli(harness.root.path(), "trust", "unsafe");
    assert_eq!(trusted["command"], "project.trust");
    assert_eq!(trusted["version"], 1);
    assert_eq!(trusted["result"]["trusted"], true);
    assert_eq!(trusted["result"]["changed"], true);
    let store = harness.root.path().join("state/fut/trusted-recipes.toml");
    assert!(store.is_file());
    assert_eq!(
        fs::metadata(&store).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let opened = harness
        .control_command(ClientMessage::OpenLocation {
            project: Some("unsafe".into()),
            name: None,
            cwd: project.clone(),
            program: None,
            argv: Vec::new(),
        })
        .await;
    let ServerMessage::LocationOpened {
        selected: first,
        disposition: fut::protocol::OpenDisposition::SessionCreated,
    } = opened
    else {
        panic!("trusted recipe was not applied without restart: {opened:?}")
    };
    wait_for(DEADLINE, || project.join("untrusted-marker").exists()).await;
    harness
        .close_session(SessionSelector::Id(first.session_id))
        .await;

    fs::write(
        project.join(".fut/project.toml"),
        r#"version = 1
tabs = [{ id = "changed", panes = [{ id = "changed", command = ["/bin/sh", "-c", "touch changed-marker; while :; do sleep 1; done"] }] }]
"#,
    )
    .unwrap();
    let changed = harness
        .control_command(ClientMessage::OpenLocation {
            project: Some("unsafe".into()),
            name: None,
            cwd: project.clone(),
            program: None,
            argv: Vec::new(),
        })
        .await;
    assert!(matches!(
        changed,
        ServerMessage::Error { ref code, .. } if code == "untrusted_recipe"
    ));
    assert!(!project.join("changed-marker").exists());

    let retrusted = project_trust_cli(harness.root.path(), "trust", "unsafe");
    assert_eq!(retrusted["result"]["changed"], true);
    let reopened = harness
        .control_command(ClientMessage::OpenLocation {
            project: Some("unsafe".into()),
            name: None,
            cwd: project.clone(),
            program: None,
            argv: Vec::new(),
        })
        .await;
    let ServerMessage::LocationOpened {
        selected: second, ..
    } = reopened
    else {
        panic!("retrusted changed recipe was not applied: {reopened:?}")
    };
    wait_for(DEADLINE, || project.join("changed-marker").exists()).await;
    harness
        .close_session(SessionSelector::Id(second.session_id))
        .await;

    let untrusted = project_trust_cli(harness.root.path(), "untrust", "unsafe");
    assert_eq!(untrusted["command"], "project.untrust");
    assert_eq!(untrusted["version"], 1);
    assert_eq!(untrusted["result"]["trusted"], false);
    assert_eq!(untrusted["result"]["changed"], true);
    let revoked = harness
        .control_command(ClientMessage::OpenLocation {
            project: Some("unsafe".into()),
            name: None,
            cwd: project,
            program: None,
            argv: Vec::new(),
        })
        .await;
    assert!(matches!(
        revoked,
        ServerMessage::Error { ref code, .. } if code == "untrusted_recipe"
    ));
    harness.shutdown().await;
}

#[tokio::test]
async fn daemon_bootstrap_applies_the_matching_catalog_recipe_atomically() {
    let harness = Harness::start_with(
        "touch bootstrap-focus.marker; while :; do sleep 1; done",
        |root| {
            let recipe = root.join("bootstrap-recipe.toml");
            fs::write(
                &recipe,
                r#"version = 1
focus = "main.focus"
[[tabs]]
id = "main"
panes = [
  { id = "first", command = ["/bin/sh", "-c", "touch bootstrap-first.marker; while :; do sleep 1; done"] },
  { id = "focus", command = ["/bin/sh", "-c", "touch declared-focus.marker; while :; do sleep 1; done"], split = { target = "first", direction = "down" } },
]
"#,
            )
            .unwrap();
            let config_dir = root.join("home/.config/fut");
            fs::create_dir_all(&config_dir).unwrap();
            fs::write(
                config_dir.join("config.toml"),
                format!(
                    "[projects.bootstrap]\npath = {:?}\nrecipe = {:?}\n",
                    root.join("cwd"),
                    recipe
                ),
            )
            .unwrap();
        },
    )
    .await;
    let cwd = harness.root.path().join("cwd");
    wait_for(DEADLINE, || {
        cwd.join("bootstrap-first.marker").exists() && cwd.join("bootstrap-focus.marker").exists()
    })
    .await;
    let snapshot = harness.resources().await;
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].workspaces.len(), 1);
    let tabs = &snapshot.sessions[0].workspaces[0].tabs;
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].panes.len(), 2);
    assert_eq!(tabs[0].layout.leaf_ids().len(), 2);
    assert!(
        !cwd.join("declared-focus.marker").exists(),
        "the explicit daemon command should override only the recipe focus command"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn workspace_retirement_waits_for_its_acknowledgement_connection() {
    let mut harness = Harness::start_with("while :; do sleep 1; done", |root| {
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
        selected: target,
        disposition: fut::protocol::OpenDisposition::WorkspaceCreated,
    } = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
            name: Some("linked".into()),
            cwd: linked,
            program: Some("/bin/sh".into()),
            argv: vec!["-c".into(), "while :; do sleep 1; done".into()],
        })
        .await
    else {
        panic!("linked worktree did not open as a workspace")
    };

    let mut retirement = harness.connect().await.unwrap();
    assert!(matches!(
        hello(&mut retirement, ClientMode::Control, PROTOCOL_VERSION)
            .await
            .unwrap(),
        ServerMessage::Welcome { .. }
    ));
    send(
        &mut retirement,
        ClientMessage::RetireWorkspace {
            workspace_id: target.workspace_id,
            context: None,
        },
    )
    .await;
    assert_eq!(
        receive(&mut retirement).await.unwrap(),
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::RetireWorkspace,
        }
    );
    assert!(process_alive(target.child_pid));
    let closing = harness.resources().await;
    let workspace = closing.sessions[0]
        .workspaces
        .iter()
        .find(|workspace| workspace.id == target.workspace_id)
        .unwrap();
    assert!(workspace.closing);

    drop(retirement);
    wait_for(DEADLINE, || !process_alive(target.child_pid)).await;
    resources_when(&harness, |snapshot| {
        snapshot.sessions[0]
            .workspaces
            .iter()
            .all(|workspace| workspace.id != target.workspace_id)
    })
    .await;
    let session_id = harness.resources().await.sessions[0].id;
    harness.close_session(SessionSelector::Id(session_id)).await;
    harness.wait_until_exited().await;
}

#[tokio::test]
async fn public_open_resolves_relative_paths_from_its_calling_process() {
    let mut harness = Harness::start_with("while :; do sleep 1; done", |root| {
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
    let opened = harness
        .cli()
        .current_dir(linked.join("nested"))
        .args([
            "--json",
            "open",
            "--background",
            ".",
            "--",
            "/bin/sh",
            "-c",
            "while :; do sleep 1; done",
        ])
        .output()
        .unwrap();
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    let response: Value = serde_json::from_slice(&opened.stdout).unwrap();
    assert_eq!(response["result"]["disposition"], "workspace_created");
    let workspace_id = response["result"]["selected"]["workspace_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let snapshot = harness.resources().await;
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].workspaces.len(), 2);
    assert_eq!(
        snapshot.sessions[0].workspaces[1].root,
        linked.canonicalize().unwrap()
    );

    assert!(matches!(
        harness
            .control_command(ClientMessage::CloseTarget {
                selector: TargetSelector::Workspace(workspace_id),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    let session_id = snapshot.sessions[0].id;
    harness.close_session(SessionSelector::Id(session_id)).await;
    harness.wait_until_exited().await;
}

#[tokio::test]
async fn existing_reopen_is_idempotent_and_invalid_name_never_spawns() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let tab_id = harness.resources().await.sessions[0].workspaces[0].tabs[0].id;
    assert!(matches!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Tab(tab_id),
                name: "stable".into(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    let before = harness.resources().await;
    let pid_file = harness.root.path().join("duplicate.pid");
    let marker_file = harness.root.path().join("duplicate.marker");
    let response = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
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
            project: None,
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
    assert_eq!(
        without_observations(after),
        without_observations(before.clone())
    );
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
            project: None,
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
    assert_eq!(
        without_observations(after_blank),
        without_observations(before)
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn terminal_attachments_share_input_and_use_the_smallest_client_geometry() {
    let harness =
        Harness::start("while IFS= read -r line; do printf 'INPUT:%s\\r\\n' \"$line\"; done").await;
    let mut first = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        selected: Some(first_selected),
        ..
    } = hello(
        &mut first,
        interactive_mode_with_size(
            Some(TargetSelector::Session(SessionSelector::Name("cwd".into()))),
            TerminalSize {
                columns: 100,
                rows: 40,
            },
        ),
        PROTOCOL_VERSION,
    )
    .await
    .unwrap()
    else {
        panic!("first client was not attached")
    };
    let terminal_id = first_selected.focused.terminal_id;

    let mut second = harness.connect().await.unwrap();
    let ServerMessage::Welcome {
        selected: Some(second_selected),
        ..
    } = hello(
        &mut second,
        interactive_mode_with_size(
            Some(TargetSelector::Terminal(terminal_id)),
            TerminalSize {
                columns: 80,
                rows: 30,
            },
        ),
        PROTOCOL_VERSION,
    )
    .await
    .unwrap()
    else {
        panic!("second client was not attached")
    };
    assert_eq!(second_selected.focused.terminal_id, terminal_id);
    snapshot_with_size(
        &mut first,
        terminal_id,
        TerminalSize {
            columns: 80,
            rows: 30,
        },
    )
    .await;

    send(
        &mut second,
        ClientMessage::Input {
            bytes: b"second\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut first, terminal_id, "INPUT:second").await;
    send(
        &mut first,
        ClientMessage::Input {
            bytes: b"first\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut second, terminal_id, "INPUT:first").await;

    send(
        &mut second,
        ClientMessage::Resize {
            terminal_id,
            size: TerminalSize {
                columns: 70,
                rows: 35,
            },
        },
    )
    .await;
    snapshot_with_size(
        &mut first,
        terminal_id,
        TerminalSize {
            columns: 70,
            rows: 35,
        },
    )
    .await;

    harness.detach(&mut second).await;
    drop(second);
    snapshot_with_size(
        &mut first,
        terminal_id,
        TerminalSize {
            columns: 100,
            rows: 40,
        },
    )
    .await;
    harness.detach(&mut first).await;
    drop(first);
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
            project: None,
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
    client.send(b"\x02s");
    client.wait_for(" navigator").await;
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
async fn public_agent_activity_spins_lists_waiting_terminals_and_navigates_unread() {
    let harness =
        Harness::start("printf 'AGENT_A_READY\r\n'; while IFS= read -r line; do :; done").await;
    let resources = harness.resources().await;
    let a = resources.sessions[0].workspaces[0].tabs[0].panes[0].clone();
    let cwd_b = harness.root.path().join("agent-b");
    fs::create_dir(&cwd_b).unwrap();
    let ServerMessage::LocationOpened { selected: b, .. } = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
            name: Some("waiting-b".into()),
            cwd: cwd_b,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'WAITING_TERMINAL_B\\r\\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create agent B")
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
    client.wait_for("AGENT_A").await;

    assert_eq!(
        harness
            .control_command(ClientMessage::ReportAgent {
                terminal_id: a.terminal_id,
                report: AgentReport::Working,
                metadata: Default::default(),
            })
            .await,
        ServerMessage::CommandCompleted {
            command: fut::protocol::AcknowledgedCommand::ReportAgent,
        }
    );
    client.wait_for("⠋").await;

    assert!(matches!(
        harness
            .control_command(ClientMessage::ReportAgent {
                terminal_id: b.terminal_id,
                report: AgentReport::Completed,
                metadata: Default::default(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    let agent_list = || {
        let output = harness
            .cli()
            .args(["--json", "agent", "list"])
            .output()
            .unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let unread = agent_list();
    assert_eq!(unread["result"]["unread_count"], 1);
    assert_eq!(unread["result"]["agents"][1]["unread"], true);
    client.wait_for("• 1").await;
    client.send(b"\x02u");
    client.wait_for(" terminals waiting").await;
    client.wait_for("waiting-b").await;
    client.send(b"\r");
    client.wait_for("WAITING").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let read = agent_list();
    assert_eq!(read["result"]["unread_count"], 0);
    assert_eq!(read["result"]["agents"][1]["unread"], false);

    client.clear_output();
    assert!(matches!(
        harness
            .control_command(ClientMessage::ReportAgent {
                terminal_id: a.terminal_id,
                report: AgentReport::Completed,
                metadata: Default::default(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    client.wait_for("• 1").await;
    client.clear_output();
    client.send(b"\x02\x02");
    client.wait_for("AGENT_A").await;
    client.send(b"\x02d");
    client.wait_success().await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_client_preserves_host_palette_indices_and_truecolor() {
    let harness = Harness::start(
        r"printf '\033[31mPALETTE_RED\033[0m \033[38;2;1;2;3mEXACT_RGB\033[0m\r\n'; while IFS= read -r line; do :; done",
    )
    .await;
    let pane = harness.resources().await.sessions[0].workspaces[0].tabs[0].panes[0].id;

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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("PALETTE_RED").await;
    client.wait_for("EXACT_RGB").await;
    let output = client.text();
    let content = output
        .find("PALETTE_RED")
        .expect("rendered output contains palette fixture");
    assert!(
        output[..content].rfind("\x1b[?2026h").is_some()
            && output[content..].find("\x1b[?2026l").is_some(),
        "client frame was not emitted atomically: {output:?}"
    );
    assert!(
        output.contains("\x1b[38;5;1;49mPALETTE_RED"),
        "indexed ANSI color was not emitted as a palette reference: {output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;1;2;3;49mEXACT_RGB"),
        "truecolor was not emitted as exact RGB: {output:?}"
    );

    client.send(b"\x02d");
    client.wait_success().await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_held_mouse_releases_before_modal_focus_and_detach_transitions() {
    let modal_expected = b"\x1b[<0;8;5M\x1b[<0;8;5m";
    let copy_expected = b"\x1b[<0;8;6M\x1b[<0;8;6m";
    let focus_expected = b"\x1b[<0;8;7M\x1b[<0;8;7m";
    let detach_expected = b"\x1b[<0;8;5M\x1b[<0;8;5m";
    let script = format!(
        "stty raw -echo; printf '\\033[?1000h\\033[?1006hHELD_A_READY\\r\\n'; dd bs=1 count={} of=held-modal.capture 2>/dev/null; printf 'HELD_MODAL_CAPTURED\\r\\n'; dd bs=1 count={} of=held-copy.capture 2>/dev/null; printf 'HELD_COPY_CAPTURED\\r\\n'; dd bs=1 count={} of=held-focus.capture 2>/dev/null; printf 'HELD_FOCUS_CAPTURED\\r\\n'; exec cat >/dev/null",
        modal_expected.len(),
        copy_expected.len(),
        focus_expected.len(),
    );
    let harness = Harness::start(&script).await;
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
                format!(
                    "stty raw -echo; printf '\\033[?1000h\\033[?1006hHELD_B_READY\\r\\n'; dd bs=1 count={} of=held-detach.capture 2>/dev/null; printf 'HELD_DETACH_CAPTURED\\r\\n'; exec cat >/dev/null",
                    detach_expected.len(),
                ),
            ],
        })
        .await
    else {
        panic!("failed to create held-mouse target pane")
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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane_a}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("HELD_A_READY").await;
    client.wait_for("HELD_B_READY").await;

    client.send(&[sgr_mouse(0, 8, 6, false), b"\x02:".to_vec()].concat());
    client.wait_for("Search commands").await;
    wait_for(DEADLINE, || {
        fs::read(harness.root.path().join("cwd/held-modal.capture"))
            .is_ok_and(|bytes| bytes.len() == modal_expected.len())
    })
    .await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/held-modal.capture")).unwrap(),
        modal_expected
    );

    client.send(
        &[
            sgr_mouse(0, 8, 6, true),
            b"\x03".to_vec(),
            sgr_mouse(0, 8, 7, false),
            b"\x02[".to_vec(),
        ]
        .concat(),
    );
    client.wait_for("COPY ·").await;
    wait_for(DEADLINE, || {
        fs::read(harness.root.path().join("cwd/held-copy.capture"))
            .is_ok_and(|bytes| bytes.len() == copy_expected.len())
    })
    .await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/held-copy.capture")).unwrap(),
        copy_expected
    );
    client.send(&[sgr_mouse(0, 8, 7, true), b"q".to_vec()].concat());
    client.wait_for("copy mode cancelled").await;
    client.send(
        &[
            sgr_mouse(0, 8, 8, false),
            b"\x02o".to_vec(),
            sgr_mouse(0, 8, 8, true),
        ]
        .concat(),
    );
    wait_for(DEADLINE, || {
        fs::read(harness.root.path().join("cwd/held-focus.capture"))
            .is_ok_and(|bytes| bytes.len() == focus_expected.len())
    })
    .await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/held-focus.capture")).unwrap(),
        focus_expected
    );

    client.send(&[sgr_mouse(0, 48, 6, false), b"\x02d".to_vec()].concat());
    client.wait_success().await;
    wait_for(DEADLINE, || {
        fs::read(harness.root.path().join("cwd/held-detach.capture"))
            .is_ok_and(|bytes| bytes.len() == detach_expected.len())
    })
    .await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/held-detach.capture")).unwrap(),
        detach_expected
    );
    assert!(process_alive(pane_b.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_sgr_mouse_focuses_without_leaking_then_forwards_focused_app_input_and_cleans_up() {
    let expected_a = b"\x1b[<18;8;5M\x1b[<18;8;5m";
    let expected_b = concat!(
        "\x1b[<0;6;4M",
        "\x1b[<0;6;4m",
        "\x1b[<1;7;5M",
        "\x1b[<1;7;5m",
        "\x1b[<14;8;6M",
        "\x1b[<14;8;6m",
        "\x1b[<0;9;7M",
        "\x1b[<32;11;8M",
        "\x1b[<0;11;8m",
        "\x1b[<35;12;9M",
        "\x1b[<80;13;10M",
        "\x1b[<65;14;11M",
    )
    .as_bytes();
    let script = format!(
        "stty raw -echo; printf '\\033[?1003h\\033[?1006hMOUSE_A_READY\\r\\n'; dd bs=1 count={} of=mouse-a.tmp 2>/dev/null; mv mouse-a.tmp mouse-a.capture; printf 'MOUSE_A_CAPTURED\\r\\n'; exec cat >/dev/null",
        expected_a.len(),
    );
    let harness = Harness::start(&script).await;
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
                format!(
                    "stty raw -echo; printf '\\033[?1003h\\033[?1006hMOUSE_B_READY\\r\\n'; dd bs=1 count={} of=mouse-b.tmp 2>/dev/null; mv mouse-b.tmp mouse-b.capture; printf 'MOUSE_B_CAPTURED\\r\\n'; exec cat >/dev/null",
                    expected_b.len(),
                ),
            ],
        })
        .await
    else {
        panic!("failed to create mouse target pane")
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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane_a}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("MOUSE_A_READY").await;
    client.wait_for("MOUSE_B_READY").await;

    // Fut modal surfaces own mouse input. These gestures must not become the
    // first bytes captured by the focused application.
    client.send(b"\x02:");
    client.wait_for("Search commands").await;
    client.send(
        &[
            sgr_mouse(0, 8, 6, false),
            sgr_mouse(0, 8, 6, true),
            b"\x03".to_vec(),
            b"\x02[".to_vec(),
        ]
        .concat(),
    );
    client.wait_for("COPY ·").await;
    client.send(
        &[
            sgr_mouse(0, 8, 6, false),
            sgr_mouse(0, 8, 6, true),
            b"q".to_vec(),
        ]
        .concat(),
    );
    client.wait_for("copy mode cancelled").await;

    // At 80 columns the right pane begins at outer column 41 and terminal
    // content begins on outer row 2 below the tab bar. The first complete
    // left gesture only focuses B and is swallowed. Everything after it is
    // encoded in B-local cells by Ghostty.
    let b_input = [
        sgr_mouse(0, 45, 4, false),
        sgr_mouse(0, 45, 4, true),
        sgr_mouse(0, 46, 5, false),
        sgr_mouse(0, 46, 5, true),
        sgr_mouse(1, 47, 6, false),
        sgr_mouse(1, 47, 6, true),
        sgr_mouse(14, 48, 7, false),
        sgr_mouse(14, 48, 7, true),
        sgr_mouse(0, 49, 8, false),
        sgr_mouse(32, 51, 9, false),
        sgr_mouse(0, 51, 9, true),
        sgr_mouse(35, 52, 10, false),
        sgr_mouse(80, 53, 11, false),
        sgr_mouse(65, 54, 12, false),
    ]
    .concat();
    client.send(&b_input);
    client.wait_for("MOUSE_B_CAPTURED").await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/mouse-b.capture")).unwrap(),
        expected_b
    );

    // Focusing A is also a swallowed gesture. The following control-right
    // press/release proves subsequent events use A's local coordinates.
    client.send(
        &[
            sgr_mouse(0, 5, 4, false),
            sgr_mouse(0, 5, 4, true),
            sgr_mouse(18, 8, 6, false),
            sgr_mouse(18, 8, 6, true),
        ]
        .concat(),
    );
    client.wait_for("MOUSE_A_CAPTURED").await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/mouse-a.capture")).unwrap(),
        expected_a
    );

    client.send(b"\x02d");
    client.wait_success().await;
    let outer = client.text();
    assert!(
        outer.contains("\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l"),
        "outer mouse capture was not disabled: {outer:?}"
    );
    assert!(process_alive(pane_b.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_mouse_resizes_nested_shared_splits_without_stealing_application_drags() {
    let expected = b"\x1b[<0;6;4M\x1b[<32;35;4M\x1b[<0;35;4m";
    let harness = Harness::start(&format!(
        "stty raw -echo; printf '\\033[?1003h\\033[?1006hPANE_DRAG_A_READY\\r\\n'; dd bs=1 count={} of=pane-drag.capture 2>/dev/null; printf 'PANE_DRAG_APP_CAPTURED\\r\\n'; while IFS= read -r line; do if [ \"$line\" = size ]; then set -- $(stty size); printf 'PANE_A_SIZE_%s_%s\\r\\n' \"$1\" \"$2\"; fi; done",
        expected.len(),
    ))
    .await;
    let snapshot = harness.resources().await;
    let tab_id = snapshot.sessions[0].workspaces[0].tabs[0].id;
    let pane_a = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::SplitPane {
            pane_id: pane_a,
            direction: fut::splits::SplitDirection::Right,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'PANE_DRAG_B_READY\\r\\n'; while :; do sleep 1; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create horizontal drag pane")
    };
    let ServerMessage::PaneCreated { selected: pane_c } = harness
        .control_command(ClientMessage::SplitPane {
            pane_id: pane_b.pane_id,
            direction: fut::splits::SplitDirection::Down,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'PANE_DRAG_C_READY\\r\\n'; while :; do sleep 1; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create vertical drag pane")
    };
    let layout = harness.resources().await.sessions[0].workspaces[0].tabs[0]
        .layout
        .clone();
    let (horizontal, vertical) = match layout {
        fut::splits::SplitTree::Branch {
            split_id: horizontal,
            axis: fut::splits::SplitAxis::Horizontal,
            second,
            ..
        } => match *second {
            fut::splits::SplitTree::Branch {
                split_id: vertical,
                axis: fut::splits::SplitAxis::Vertical,
                ..
            } => (horizontal, vertical),
            other => panic!("expected nested vertical branch, got {other:?}"),
        },
        other => panic!("expected horizontal root branch, got {other:?}"),
    };

    let (mut observer, observed) =
        attach_once(&harness, TargetSelector::Pane(pane_b.pane_id)).await;
    assert_eq!(observed.tab_id, tab_id);
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
            "stty rows 30 cols 100; exec '{}' --socket '{}' pane attach {pane_a}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("PANE_DRAG_A_READY").await;
    client.wait_for("PANE_DRAG_B_READY").await;
    client.wait_for("PANE_DRAG_C_READY").await;

    client.send(
        &[
            sgr_mouse(0, 50, 6, false),
            sgr_mouse(32, 36, 6, false),
            sgr_mouse(0, 36, 6, true),
        ]
        .concat(),
    );
    resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs[0]
            .layout
            .ratio(horizontal)
            .is_some_and(|ratio| ratio.first_cells(99) == 35)
    })
    .await;
    receive_matching(&mut observer, |message| {
        matches!(
            message,
            ServerMessage::TargetSelected { selected }
                if selected.layout.ratio(horizontal)
                    .is_some_and(|ratio| ratio.first_cells(99) == 35)
        )
    })
    .await;

    client.send(
        &[
            sgr_mouse(0, 61, 16, false),
            sgr_mouse(32, 61, 12, false),
            sgr_mouse(0, 61, 12, true),
        ]
        .concat(),
    );
    resources_when(&harness, |snapshot| {
        let layout = &snapshot.sessions[0].workspaces[0].tabs[0].layout;
        layout
            .ratio(horizontal)
            .is_some_and(|ratio| ratio.first_cells(99) == 35)
            && layout
                .ratio(vertical)
                .is_some_and(|ratio| ratio.first_cells(28) == 10)
    })
    .await;
    receive_matching(&mut observer, |message| {
        matches!(
            message,
            ServerMessage::TargetSelected { selected }
                if selected.layout.ratio(vertical)
                    .is_some_and(|ratio| ratio.first_cells(28) == 10)
        )
    })
    .await;

    assert!(
        fs::metadata(harness.root.path().join("cwd/pane-drag.capture"))
            .is_ok_and(|metadata| metadata.len() == 0),
        "divider gesture leaked into the all-motion application"
    );
    client.send(
        &[
            sgr_mouse(0, 6, 5, false),
            sgr_mouse(32, 36, 5, false),
            sgr_mouse(0, 36, 5, true),
        ]
        .concat(),
    );
    client.wait_for("PANE_DRAG_APP_CAPTURED").await;
    assert_eq!(
        fs::read(harness.root.path().join("cwd/pane-drag.capture")).unwrap(),
        expected
    );
    client.send(b"size\n");
    client.wait_for("PANE_A_SIZE_29_35").await;
    let after_app_drag = harness.resources().await;
    assert_eq!(
        after_app_drag.sessions[0].workspaces[0].tabs[0]
            .layout
            .ratio(horizontal)
            .unwrap()
            .first_cells(99),
        35,
        "terminal-originated drag changed the Fut split"
    );

    client.send(
        &[
            sgr_mouse(0, 36, 5, false),
            sgr_mouse(32, 1, 5, false),
            sgr_mouse(0, 1, 5, true),
        ]
        .concat(),
    );
    resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs[0]
            .layout
            .ratio(horizontal)
            .is_some_and(|ratio| ratio.first_cells(99) == 24)
    })
    .await;
    client.send(b"size\n");
    client.wait_for("PANE_A_SIZE_29_24").await;

    send_uncorrelated(
        &mut observer,
        ClientMessage::ResizeSplit {
            tab_id,
            split_id: fut::domain::SplitId::new(),
            ratio: fut::splits::SplitRatio::from_cells(1, 2).unwrap(),
        },
    )
    .await;
    assert_no_error_before_pong(&mut observer).await;

    client.send(b"\x02d");
    client.wait_success().await;
    let (mut reattached, selected) = attach_once(&harness, TargetSelector::Pane(pane_a)).await;
    assert_eq!(selected.tab_id, tab_id);
    send(&mut reattached, ClientMessage::ListResources).await;
    let ServerMessage::Resources { snapshot, .. } = receive_matching(&mut reattached, |message| {
        matches!(message, ServerMessage::Resources { .. })
    })
    .await
    else {
        unreachable!()
    };
    let persisted = &snapshot.sessions[0].workspaces[0].tabs[0].layout;
    assert_eq!(persisted.ratio(horizontal).unwrap().first_cells(99), 24);
    assert_eq!(persisted.ratio(vertical).unwrap().first_cells(28), 10);

    harness.detach(&mut reattached).await;
    harness.detach(&mut observer).await;
    assert!(process_alive(pane_b.child_pid));
    assert!(process_alive(pane_c.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_wheel_uses_dec_alternate_scroll_and_terminal_cursor_key_mode() {
    let harness = Harness::start(
        "stty raw -echo; printf '\\033[?1049h\\033[?1007hALT_SCROLL_NORMAL_READY\\r\\n'; dd bs=1 count=9 of=alt-normal.capture 2>/dev/null; printf '\\033[?1hALT_SCROLL_APPLICATION_READY\\r\\n'; dd bs=1 count=9 of=alt-application.capture 2>/dev/null; printf 'ALT_SCROLL_CAPTURED\\r\\n'; exec cat >/dev/null",
    )
    .await;
    let pane = harness.resources().await.sessions[0].workspaces[0].tabs[0].panes[0].id;
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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("ALT_SCROLL_NORMAL_READY").await;
    client.send(&sgr_mouse(64, 8, 6, false));
    client.wait_for("ALT_SCROLL_APPLICATION_READY").await;
    client.send(&sgr_mouse(65, 8, 6, false));
    client.wait_for("ALT_SCROLL_CAPTURED").await;

    assert_eq!(
        fs::read(harness.root.path().join("cwd/alt-normal.capture")).unwrap(),
        b"\x1b[A\x1b[A\x1b[A"
    );
    assert_eq!(
        fs::read(harness.root.path().join("cwd/alt-application.capture")).unwrap(),
        b"\x1bOB\x1bOB\x1bOB"
    );
    client.send(b"\x02d");
    client.wait_success().await;
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
    client.send(b"\x02o");
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
                "printf 'ZZZZZZZZ_READY\r\n'; while IFS= read -r line; do [ \"$line\" = d ] && { : > destination-input; printf 'DESTINATION_INPUT\r\n'; }; done".into(),
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
    let destination_marker = harness.root.path().join("cwd/destination-input");
    time::timeout(DEADLINE, async {
        while !destination_marker.exists() {
            client.send(b"d\n");
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("pane-close fallback never reconciled: {:?}", client.text()));
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
async fn public_directional_focus_follows_authored_geometry_when_zoomed_and_tiny() {
    let harness = Harness::start(
        "printf 'DIRECTION_A_READY\r\n'; while IFS= read -r line; do [ \"$line\" = a ] && { printf x >> direction-a; printf 'DIRECTION_A_INPUT\r\n'; }; done",
    )
    .await;
    let resources = harness.resources().await;
    let pane_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::SplitPane {
            pane_id: pane_a,
            direction: fut::splits::SplitDirection::Right,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'DIRECTION_B_READY\r\n'; while IFS= read -r line; do case \"$line\" in b) printf x >> direction-b; printf 'DIRECTION_B_INPUT\r\n';; b2) printf x >> direction-b-up; printf 'DIRECTION_B_UP\r\n';; b3) printf x >> direction-b-zoom; printf 'DIRECTION_B_ZOOM\r\n';; esac; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create right directional pane")
    };
    let ServerMessage::PaneCreated { selected: pane_c } = harness
        .control_command(ClientMessage::SplitPane {
            pane_id: pane_b.pane_id,
            direction: fut::splits::SplitDirection::Down,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'DIRECTION_C_READY\r\n'; while IFS= read -r line; do case \"$line\" in c) printf x >> direction-c; printf 'DIRECTION_C_INPUT\r\n';; tiny) printf x >> direction-c-tiny; printf 'DIRECTION_C_TINY\r\n';; esac; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create down directional pane")
    };

    let spawn = |columns: u16, pane: PaneId| {
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
                "stty rows 30 cols {columns}; exec '{}' --socket '{}' pane attach {pane}",
                env!("CARGO_BIN_EXE_fut"),
                harness.socket.display(),
            ));
        PtyChild::spawn(command)
    };

    let cwd = harness.root.path().join("cwd");
    let marker_size = |name: &str| {
        fs::metadata(cwd.join(name))
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    };

    let mut client = spawn(100, pane_a);
    client.wait_for("DIRECTION_C_READY").await;
    client.send(b"\x02lb\n");
    wait_for(DEADLINE, || marker_size("direction-b") >= 1).await;
    client.send(b"\x02jc\n");
    wait_for(DEADLINE, || marker_size("direction-c") >= 1).await;
    client.send(b"\x02kb2\n");
    wait_for(DEADLINE, || marker_size("direction-b-up") >= 1).await;
    client.send(b"\x02Pc\n");
    wait_for(DEADLINE, || marker_size("direction-c") >= 2).await;
    client.send(b"\x02Pb2\n");
    wait_for(DEADLINE, || marker_size("direction-b-up") >= 2).await;
    client.send(b"\x02ha\n");
    wait_for(DEADLINE, || marker_size("direction-a") >= 1).await;
    client.send(b"\x02z\x02lb3\n");
    wait_for(DEADLINE, || marker_size("direction-b-zoom") >= 1).await;
    client.send(b"\x02d");
    client.wait_success().await;

    let mut tiny = spawn(20, pane_b.pane_id);
    tiny.wait_for("DIRECTION_B_READY").await;
    tiny.send(b"\x02jtiny\n");
    wait_for(DEADLINE, || marker_size("direction-c-tiny") >= 1).await;
    tiny.send(b"\x02d");
    tiny.wait_success().await;

    assert!(process_alive(pane_b.child_pid));
    assert!(process_alive(pane_c.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_client_accordion_resizes_focus_and_falls_back_narrowly() {
    let harness = Harness::start(
        "printf 'AAAAAAAA\\r\\n'; while IFS= read -r line; do if [ \"$line\" = size-a ]; then set -- $(stty size); printf 'A_SIZE_%s_%s\\r\\n' \"$1\" \"$2\"; fi; done",
    )
    .await;
    let config_dir = harness.root.path().join("home/.config/fut");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "[ui]\npane_layout = \"accordion\"\n",
    )
    .unwrap();
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
    narrow.send(b"\x02o");
    // The narrow fallback renders one pane, so the sibling's banner is the
    // switch itself. Typing before it lands lets the incremental renderer
    // coalesce the switch with the reply and emit only the changed cells.
    narrow.wait_for("BBBBBBBB").await;
    narrow.send(b"size-b\n");
    narrow.wait_for("B_SIZE_23_37").await;
    narrow.send(b"\x02d");
    narrow.wait_success().await;

    let mut accordion = spawn_client(38);
    accordion.wait_for("AAAAAAAA").await;
    accordion.wait_for("BBBBBBBB").await;
    accordion.send(b"size-a\n");
    accordion.wait_for("A_SIZE_23_24").await;
    accordion.send(b"\x02o");
    accordion.send(b"size-b\n");
    accordion.wait_for("B_SIZE_23_24").await;
    accordion.send(b"\x02d");
    accordion.wait_success().await;

    assert!(process_alive(pane_b.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_client_reloads_config_immediately_and_retains_it_after_an_error() {
    let harness = Harness::start(
        "printf 'RELOAD_A_READY\\r\\n'; while IFS= read -r line; do set -- $(stty size); case \"$line\" in before) printf 'RELOAD_BEFORE_%s_%s\\r\\n' \"$1\" \"$2\";; after) printf 'RELOAD_AFTER_%s_%s\\r\\n' \"$1\" \"$2\";; retained) printf 'RELOAD_RETAINED_%s_%s\\r\\n' \"$1\" \"$2\";; esac; done",
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
                "printf 'RELOAD_B_READY\\r\\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create reload sibling")
    };

    let config_directory = harness.root.path().join("home/.config/fut");
    fs::create_dir_all(&config_directory).unwrap();
    let config_path = config_directory.join("config.toml");
    fs::write(&config_path, "").unwrap();

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
    client.wait_for("RELOAD_A_READY").await;
    client.wait_for("RELOAD_B_READY").await;
    client.send(b"before\n");
    client.wait_for("RELOAD_BEFORE_23_39").await;

    fs::write(
        &config_path,
        "[ui]\npane_layout = 'accordion'\n\n[ui.bindings]\nreload_config = 'r'\ndetach = 'y'\n",
    )
    .unwrap();
    client.clear_output();
    client.send(b"\x02R");
    client.wait_for("config reloaded").await;
    client.send(b"after\n");
    client.wait_for("RELOAD_AFTER_23_").await;
    assert!(
        !client.text().contains("RELOAD_AFTER_23_39"),
        "reloaded layout did not resize panes: {:?}",
        client.text()
    );

    fs::write(
        &config_path,
        "[ui]\npane_layout = 'sideways'\n\n[ui.bindings]\ndetach = 'x'\n",
    )
    .unwrap();
    client.clear_output();
    client.send(b"\x02r");
    client.wait_for("config reload failed").await;
    client.wait_for("parse Fut config").await;
    client.send(b"retained\n");
    client.wait_for("RELOAD_RETAINED_23_").await;
    assert!(
        !client.text().contains("RELOAD_RETAINED_23_39"),
        "failed reload replaced the prior layout: {:?}",
        client.text()
    );
    client.send(b"\x02y");
    client.wait_success().await;

    assert!(process_alive(pane_b.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_pane_zoom_toggles_full_width_and_matches_command_dispatch() {
    let harness = Harness::start(
        "printf 'ZOOM_A_READY\r\n'; while IFS= read -r line; do set -- $(stty size); case \"$line\" in before) printf 'BEFORE_%s_%s\r\n' \"$1\" \"$2\";; zoomed) printf 'ZOOMED_%s_%s\r\n' \"$1\" \"$2\";; restored) printf 'RESTORED_%s_%s\r\n' \"$1\" \"$2\";; command) printf 'COMMAND_ZOOM_%s_%s\r\n' \"$1\" \"$2\";; esac; done",
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
                "printf 'ZOOM_B_READY\\r\\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create zoom sibling")
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
    client.wait_for("ZOOM_A_READY").await;
    client.wait_for("ZOOM_B_READY").await;
    client.send(b"before\n");
    client.wait_for("BEFORE_23_39").await;

    client.send(b"\x02z");
    client.wait_for("zoom").await;
    client.send(b"zoomed\n");
    client.wait_for("ZOOMED_23_80").await;

    client.send(b"\x02z");
    client.send(b"restored\n");
    client.wait_for("RESTORED_23_39").await;

    client.send(b"\x02:");
    client.send(b"pane zoom");
    client.send(b"\r");
    client.send(b"command\n");
    client.wait_for("COMMAND_ZOOM_23_80").await;
    client.send(b"\x02z");
    client.send(b"\x02d");
    client.wait_success().await;

    assert!(process_alive(pane_b.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_tab_navigation_and_right_down_splits_share_the_command_catalog() {
    let split_cwd = tempfile::tempdir().unwrap();
    let back_marker = split_cwd.path().join("back-marker");
    let script = format!(
        "printf 'ACTION_A_READY\\r\\n'; while IFS= read -r line; do case \"$line\" in back) printf x >> '{}'; printf 'ACTION_A_BACK\\r\\n';; cd-now) cd '{}'; printf 'ACTION_A_CHANGED_DIR\\r\\n';; esac; done",
        back_marker.display(),
        split_cwd.path().display(),
    );
    let harness = Harness::start(&script).await;
    let resources = harness.resources().await;
    let workspace_id = resources.sessions[0].workspaces[0].id;
    let pane_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let ServerMessage::TabCreated { selected: tab_b } = harness
        .control_command(ClientMessage::CreateTab {
            workspace_id,
            name: Some("second".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'ACTION_TAB_B_READY\\r\\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create tab-navigation target")
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
            "stty rows 30 cols 100; exec '{}' --socket '{}' pane attach {}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
            pane_a
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("ACTION_A_READY").await;
    time::timeout(DEADLINE, async {
        while !client.text().contains("TAB_B_READY") {
            client.send(b"\x02n");
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("tab navigation never reconciled: {:?}", client.text()));
    client.send(b"\x02T");
    client.send(b"back\n");
    wait_for(DEADLINE, || {
        fs::metadata(&back_marker).is_ok_and(|metadata| metadata.len() >= 1)
    })
    .await;
    client.send(b"cd-now\n");
    client.wait_for("ACTION_A_CHANGED_DIR").await;

    let inherited_marker = split_cwd.path().join("tab-cwd");
    client.send(b"\x02c");
    client.wait_for("sh-3.2$").await;
    client.send(b"pwd>tab-cwd\n");
    wait_for_nonempty_file(&inherited_marker).await;
    assert_eq!(
        fs::read_to_string(&inherited_marker).unwrap().trim(),
        split_cwd
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
    client.send(b"exit\n");
    resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs.len() == 2
    })
    .await;
    time::timeout(DEADLINE, async {
        while !fs::metadata(&back_marker).is_ok_and(|metadata| metadata.len() >= 2) {
            client.send(b"\x021back\n");
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "numbered tab navigation never reconciled: {:?}",
            client.text()
        )
    });

    client.send(b"\x02|");
    client.send(b": > right-split-marker\n");
    wait_for(DEADLINE, || {
        split_cwd.path().join("right-split-marker").exists()
    })
    .await;

    client.send(b"\x02:");
    client.send(b"split pane down\r");
    client.send(b": > down-split-marker\n");
    wait_for(DEADLINE, || {
        split_cwd.path().join("down-split-marker").exists()
    })
    .await;

    let snapshot = resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs[0].panes.len() == 3
    })
    .await;
    let layout = &snapshot.sessions[0].workspaces[0].tabs[0].layout;
    assert!(matches!(
        layout,
        fut::splits::SplitTree::Branch {
            axis: fut::splits::SplitAxis::Horizontal,
            second,
            ..
        } if matches!(
            second.as_ref(),
            fut::splits::SplitTree::Branch {
                axis: fut::splits::SplitAxis::Vertical,
                ..
            }
        )
    ));

    client.send(b"\x02d");
    client.wait_success().await;
    assert!(process_alive(tab_b.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn isolated_daily_driver_journey() {
    let harness = Harness::start(
        "printf 'JOURNEY_MAIN_READY\r\n'; while IFS= read -r line; do case \"$line\" in main) : > journey-main; printf 'JOURNEY_MAIN_INPUT\r\n';; fallback) : > journey-fallback; printf 'JOURNEY_EXIT_FALLBACK\r\n';; reattach) : > journey-reattach; printf 'JOURNEY_REATTACHED_INPUT\r\n';; esac; done",
    )
    .await;
    let resources = harness.resources().await;
    let workspace_id = resources.sessions[0].workspaces[0].id;
    let original_pane = resources.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let ServerMessage::TabCreated { selected: second } = harness
        .control_command(ClientMessage::CreateTab {
            workspace_id,
            name: Some("journey-second".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'TAB2_READY_729\r\n'; while IFS= read -r line; do case \"$line\" in second) : > journey-second; printf 'TAB2_INPUT_729\r\n';; exit-now) exit;; esac; done"
                    .into(),
            ],
        })
        .await
    else {
        panic!("failed to create journey tab")
    };

    let spawn_client = |pane: PaneId, columns: u16| {
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
                "stty rows 30 cols {columns}; exec '{}' --socket '{}' pane attach {pane}",
                env!("CARGO_BIN_EXE_fut"),
                harness.socket.display()
            ));
        PtyChild::spawn(command)
    };

    let journey_marker = |name: &str| harness.root.path().join("cwd").join(name);

    let mut client = spawn_client(original_pane, 100);
    client.wait_for("JOURNEY_MAIN_READY").await;
    client.send(b"main\n");
    wait_for_file(journey_marker("journey-main")).await;

    client.send(b"\x02|");
    client.send(b":>journey-right\n");
    wait_for_file(journey_marker("journey-right")).await;
    client.send(b"\x02:");
    client.send(b"split pane down\r");
    client.send(b":>journey-down\n");
    wait_for_file(journey_marker("journey-down")).await;

    let split_resources = resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs[0].panes.len() == 3
    })
    .await;
    let split_tab = &split_resources.sessions[0].workspaces[0].tabs[0];
    assert_eq!(
        split_tab.layout.leaf_ids(),
        split_tab
            .panes
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>()
    );

    client.send(b"\x02z");
    client.wait_for("zoom").await;
    client.send(b":>journey-zoomed\n");
    wait_for_file(journey_marker("journey-zoomed")).await;
    client.send(b"\x02z");

    client.send(b"\x02n");
    client.wait_for("TAB2_READY_729").await;
    client.send(b"second\n");
    wait_for_file(journey_marker("journey-second")).await;
    client.send(b"\x02p");
    client.send(b":>journey-back\n");
    wait_for_file(journey_marker("journey-back")).await;
    client.send(b"\x02n");
    client.send(b"exit-now\n");

    let remaining = resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs.len() == 1
    })
    .await;
    assert_eq!(remaining.sessions[0].workspaces[0].tabs[0].panes.len(), 3);
    let fallback_marker = journey_marker("journey-fallback");
    time::timeout(DEADLINE, async {
        while !fallback_marker.exists() {
            client.send(b"fallback\n");
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "focused-exit fallback never reconciled: {:?}",
            client.text()
        )
    });
    assert!(!process_alive(second.child_pid));

    client.send(b"\x02d");
    client.wait_success().await;
    let reattach_pane = remaining.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let mut reattached = spawn_client(reattach_pane, 60);
    reattached.wait_for("JOURNEY_MAIN_READY").await;
    reattached.send(b"reattach\n");
    wait_for_file(journey_marker("journey-reattach")).await;
    reattached.send(b"\x02d");
    reattached.wait_success().await;

    harness.shutdown().await;
}

#[tokio::test]
async fn isolated_keyboard_chaos_journey() {
    let seed = std::env::var("FUT_CHAOS_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0x0f07_ca05);
    let steps = std::env::var("FUT_CHAOS_STEPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(40usize);
    assert!(
        (1..=2_000).contains(&steps),
        "FUT_CHAOS_STEPS must be 1..=2000"
    );

    let harness = Harness::start("exec /bin/sh").await;
    let initial = harness.resources().await;
    let workspace_id = initial.sessions[0].workspaces[0].id;
    let mut current_tab = initial.sessions[0].workspaces[0].tabs[0].id;
    let mut current_pane = initial.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let mut rng = ChaosRng::new(seed);
    let mut pane_processes = HashMap::new();
    let mut tab_history = HashMap::from([(current_tab, current_pane)]);
    let mut last_panes = HashMap::new();
    let mut last_tab = None;

    let spawn_client = |pane: PaneId, columns: u16| {
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
                "stty rows 30 cols {columns}; exec '{}' --socket '{}' pane attach {pane}",
                env!("CARGO_BIN_EXE_fut"),
                harness.socket.display()
            ));
        PtyChild::spawn(command)
    };

    let mut client = spawn_client(current_pane, 100);
    client.wait_for("sh-3.2$").await;
    pane_processes.insert(
        current_pane,
        chaos_identify_pane(&mut client, harness.root.path(), 0, seed).await,
    );

    for step in 1..=steps {
        let before = harness.resources().await;
        let workspace = before.sessions[0]
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .expect("chaos workspace remains open");
        let tab = workspace
            .tabs
            .iter()
            .find(|tab| tab.id == current_tab)
            .expect("current chaos tab remains open");
        let pane_index = tab
            .panes
            .iter()
            .position(|pane| pane.id == current_pane)
            .expect("current chaos pane remains open");
        let previous_tab = current_tab;
        let previous_pane = current_pane;
        let mut reset_history = false;
        let mut action = rng.index(14);
        if matches!(action, 0 | 1) && tab.panes.len() >= 6 {
            action = 2 + rng.index(3);
        }
        if action == 7 && workspace.tabs.len() >= 4 {
            action = 5 + rng.index(2);
        }
        if action == 8 && tab.panes.len() == 1 {
            action = 2 + rng.index(3);
        }
        if matches!(action, 5 | 6) && workspace.tabs.len() == 1 {
            action = 7;
        }
        if action == 11 && workspace.tabs.len() == 1 {
            action = 7;
        }
        if action == 12
            && !last_panes.get(&current_tab).is_some_and(|pane_id| {
                *pane_id != current_pane && tab.panes.iter().any(|pane| pane.id == *pane_id)
            })
        {
            action = 2;
        }
        if action == 13
            && !last_tab.is_some_and(|tab_id| {
                tab_id != current_tab && workspace.tabs.iter().any(|tab| tab.id == tab_id)
            })
        {
            action = if workspace.tabs.len() > 1 { 5 } else { 7 };
        }

        let action_name = match action {
            0 => {
                client.send(b"\x02|");
                let snapshot = resources_when(&harness, |snapshot| {
                    snapshot.sessions[0].workspaces[0]
                        .tabs
                        .iter()
                        .find(|tab| tab.id == current_tab)
                        .is_some_and(|tab| {
                            tab.panes.len()
                                == before.sessions[0].workspaces[0]
                                    .tabs
                                    .iter()
                                    .find(|tab| tab.id == current_tab)
                                    .unwrap()
                                    .panes
                                    .len()
                                    + 1
                        })
                })
                .await;
                let tab = snapshot.sessions[0].workspaces[0]
                    .tabs
                    .iter()
                    .find(|tab| tab.id == current_tab)
                    .unwrap();
                current_pane = tab.panes[pane_index + 1].id;
                pane_processes.insert(
                    current_pane,
                    chaos_identify_pane(&mut client, harness.root.path(), step, seed).await,
                );
                "split-right"
            }
            1 => {
                client.send(b"\x02:");
                client.send(b"split pane down\r");
                let pane_count = tab.panes.len() + 1;
                let snapshot = resources_when(&harness, |snapshot| {
                    snapshot.sessions[0].workspaces[0]
                        .tabs
                        .iter()
                        .find(|tab| tab.id == current_tab)
                        .is_some_and(|tab| tab.panes.len() == pane_count)
                })
                .await;
                let tab = snapshot.sessions[0].workspaces[0]
                    .tabs
                    .iter()
                    .find(|tab| tab.id == current_tab)
                    .unwrap();
                current_pane = tab.panes[pane_index + 1].id;
                pane_processes.insert(
                    current_pane,
                    chaos_identify_pane(&mut client, harness.root.path(), step, seed).await,
                );
                "launcher-split-down"
            }
            2 => {
                client.send(b"\x02o");
                current_pane = tab.panes[(pane_index + 1) % tab.panes.len()].id;
                "focus-next"
            }
            3 => {
                client.send(b"\x02;");
                current_pane = tab.panes[(pane_index + tab.panes.len() - 1) % tab.panes.len()].id;
                "focus-previous"
            }
            4 => {
                client.send(b"\x02z");
                "toggle-zoom"
            }
            5 | 6 => {
                let tab_index = workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.id == current_tab)
                    .unwrap();
                let next_index = if action == 5 {
                    (tab_index + 1) % workspace.tabs.len()
                } else {
                    (tab_index + workspace.tabs.len() - 1) % workspace.tabs.len()
                };
                client.send(if action == 5 { b"\x02n" } else { b"\x02p" });
                current_tab = workspace.tabs[next_index].id;
                current_pane = tab_history
                    .get(&current_tab)
                    .copied()
                    .filter(|pane_id| {
                        workspace.tabs[next_index]
                            .panes
                            .iter()
                            .any(|pane| pane.id == *pane_id)
                    })
                    .unwrap_or(workspace.tabs[next_index].panes[0].id);
                if action == 5 {
                    "tab-next"
                } else {
                    "tab-previous"
                }
            }
            7 => {
                client.send(b"\x02c");
                let tab_count = workspace.tabs.len() + 1;
                let snapshot = resources_when(&harness, |snapshot| {
                    snapshot.sessions[0].workspaces[0].tabs.len() == tab_count
                })
                .await;
                let tab = snapshot.sessions[0].workspaces[0].tabs.last().unwrap();
                current_tab = tab.id;
                current_pane = tab.panes[0].id;
                pane_processes.insert(
                    current_pane,
                    chaos_identify_pane(&mut client, harness.root.path(), step, seed).await,
                );
                "create-tab"
            }
            8 => {
                client.send(b"exit\n");
                let pane_count = tab.panes.len() - 1;
                let snapshot = resources_when(&harness, |snapshot| {
                    snapshot.sessions[0].workspaces[0]
                        .tabs
                        .iter()
                        .find(|tab| tab.id == current_tab)
                        .is_some_and(|tab| tab.panes.len() == pane_count)
                })
                .await;
                let tab = snapshot.sessions[0].workspaces[0]
                    .tabs
                    .iter()
                    .find(|tab| tab.id == current_tab)
                    .unwrap();
                current_pane = tab.panes[pane_index.saturating_sub(1)].id;
                "exit-pane"
            }
            9 => {
                client.send(b"\x02d");
                client.wait_success().await;
                let pane = &workspace.tabs[rng.index(workspace.tabs.len())].panes;
                current_pane = pane[rng.index(pane.len())].id;
                current_tab = workspace
                    .tabs
                    .iter()
                    .find(|tab| tab.panes.iter().any(|pane| pane.id == current_pane))
                    .unwrap()
                    .id;
                client = spawn_client(current_pane, 48 + rng.index(73) as u16);
                tab_history.clear();
                tab_history.insert(current_tab, current_pane);
                last_panes.clear();
                last_tab = None;
                reset_history = true;
                "detach-reattach"
            }
            10 => {
                let (binding, name) = match rng.index(4) {
                    0 => (b"\x02h".as_slice(), "focus-left"),
                    1 => (b"\x02j".as_slice(), "focus-down"),
                    2 => (b"\x02k".as_slice(), "focus-up"),
                    _ => (b"\x02l".as_slice(), "focus-right"),
                };
                client.send(binding);
                let process =
                    chaos_identify_pane(&mut client, harness.root.path(), step, seed).await;
                current_pane = pane_processes
                    .iter()
                    .find_map(|(pane_id, expected)| (expected == &process).then_some(*pane_id))
                    .expect("directional focus remains on a known pane");
                name
            }
            11 => {
                let numbered = workspace
                    .tabs
                    .iter()
                    .take(10)
                    .enumerate()
                    .filter(|(_, tab)| tab.id != current_tab)
                    .collect::<Vec<_>>();
                let (index, tab) = numbered[rng.index(numbered.len())];
                let suffix = if index == 9 { b'0' } else { b'1' + index as u8 };
                client.send(&[2, suffix]);
                current_tab = tab.id;
                current_pane = tab_history
                    .get(&current_tab)
                    .copied()
                    .filter(|pane_id| tab.panes.iter().any(|pane| pane.id == *pane_id))
                    .unwrap_or(tab.panes[0].id);
                "tab-numbered"
            }
            12 => {
                client.send(b"\x02P");
                current_pane = last_panes[&current_tab];
                "pane-last"
            }
            13 => {
                client.send(b"\x02T");
                current_tab = last_tab.expect("last tab action was validated");
                let tab = workspace
                    .tabs
                    .iter()
                    .find(|tab| tab.id == current_tab)
                    .unwrap();
                current_pane = tab_history
                    .get(&current_tab)
                    .copied()
                    .filter(|pane_id| tab.panes.iter().any(|pane| pane.id == *pane_id))
                    .unwrap_or(tab.panes[0].id);
                "tab-last"
            }
            _ => unreachable!(),
        };

        eprintln!("chaos seed={seed} step={step}/{steps} action={action_name}");
        if !reset_history {
            if previous_tab == current_tab && previous_pane != current_pane {
                last_panes.insert(current_tab, previous_pane);
            } else if previous_tab != current_tab {
                last_tab = Some(previous_tab);
            }
        }
        tab_history.insert(current_tab, current_pane);
        chaos_probe_pane(
            &mut client,
            harness.root.path(),
            step,
            pane_processes.get(&current_pane).unwrap(),
            seed,
        )
        .await;
        let snapshot = harness.resources().await;
        let tab = snapshot.sessions[0].workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == current_tab)
            .expect("focused tab exists after chaos action");
        assert!(tab.panes.iter().any(|pane| pane.id == current_pane));
        assert_eq!(
            tab.layout.leaf_ids(),
            tab.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            "layout mismatch after seed={seed} step={step} action={action_name}"
        );
    }

    client.send(b"\x02d");
    client.wait_success().await;
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
    resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs[1].name == "checks"
    })
    .await;
    assert!(matches!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Tab(current_tab),
                name: "work".into(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces[0].tabs[0].name == "work"
    })
    .await;
    top.send(b"\x02d");
    top.wait_success().await;

    let config_directory = harness.root.path().join("home/.config/fut");
    fs::create_dir_all(&config_directory).unwrap();
    fs::write(
        config_directory.join("config.toml"),
        r#"
[ui.icons]
preset = "ascii"
current = "@"

[ui.tab_bar]
position = "bottom"
left = [
  { segments = [{ text = "LEFT " }], priority = 200 },
  { segments = [{ component = "tabs" }] },
]
center = []
right = [{ segments = [{ text = " RIGHT" }], priority = 200 }]

[ui.tab_bar.item]
segments = [{ text = "{" }, { token = "tab.marker" }, { text = ":" }, { token = "tab.name" }, { text = "}" }]

"#,
    )
    .unwrap();

    let mut bottom = spawn_client();
    bottom.wait_for("work").await;
    bottom.wait_for("checks").await;
    bottom.wait_for("LEFT").await;
    bottom.wait_for("RIGHT").await;
    bottom.wait_for("work}").await;
    bottom.wait_for("\x1b[24;1H").await;
    let bar_positioned_on_last_row = bottom.text().split("\x1b[24;1H").any(|suffix| {
        let nearby = suffix
            .char_indices()
            .nth(200)
            .map_or(suffix, |(end, _)| &suffix[..end]);
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
        "[ui.tab_bar]\nposition = \"sideways\"\n",
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
async fn public_component_sidebar_docks_navigates_and_collapses_responsively() {
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
            project: None,
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

    let mut left = spawn_client(124, main_pane);
    left.wait_for("ALPHA_READY").await;
    left.wait_for("feature").await;
    left.send(b"size-main\n");
    left.wait_for("ALPHA_SIZE_23_96").await;
    left.send(b"\x02w");
    left.wait_for("hotkeys").await;
    left.send(b"j\r");
    left.wait_for("ZETA_READY").await;
    left.send(b"linked\nsize-linked\n");
    left.wait_for("ZETA_INPUT").await;
    left.wait_for("ZETA_SIZE_23_96").await;
    left.send(b"\x02W");
    left.send(b"main\n");
    left.wait_for("BRAVO_ACK").await;
    left.send(b"\x02Wlinked\n");
    left.wait_for_count("ZETA_INPUT", 2).await;
    left.send(b"\x02Wmain\n");
    left.wait_for_count("BRAVO_ACK", 2).await;
    left.send(
        &[
            sgr_mouse(0, 28, 6, false),
            sgr_mouse(32, 20, 6, false),
            sgr_mouse(0, 20, 6, true),
        ]
        .concat(),
    );
    left.send(b"size-main\n");
    left.wait_for("ALPHA_SIZE_23_104").await;
    left.send(b"\x02d");
    left.wait_success().await;

    let mut left_reset = spawn_client(124, main_pane);
    left_reset.wait_for("ALPHA_READY").await;
    left_reset.send(b"size-main\n");
    left_reset.wait_for("ALPHA_SIZE_23_96").await;
    left_reset.send(b"\x02d");
    left_reset.wait_success().await;

    let config_directory = harness.root.path().join("home/.config/fut");
    fs::create_dir_all(&config_directory).unwrap();
    fs::write(
        config_directory.join("config.toml"),
        r#"
[ui.sidebar.left]
visibility = "hidden"

[ui.sidebar.right]
visibility = "visible"
components = [
  { component = "workspaces", size = "fill", header = [{ text = "WORKSPACES" }], footer = [{ text = "MODE:" }, { token = "sidebar.visibility" }], row = { left = [{ text = "WS[" }], body = [{ token = "workspace.name" }], right = [{ text = "]" }, { token = "workspace.tab_count" }] } },
]
"#,
    )
    .unwrap();
    let mut configured = spawn_client(124, linked_target.pane_id);
    configured.wait_for("WORKSPACES").await;
    configured.wait_for("WS[").await;
    configured.wait_for("feature").await;
    assert!(matches!(
        harness
            .control_command(ClientMessage::RenameTarget {
                selector: RenameSelector::Workspace(linked_target.workspace_id),
                name: "feature λ".into(),
            })
            .await,
        ServerMessage::CommandCompleted { .. }
    ));
    configured.wait_for("λ").await;
    configured.send(b"\x02]q");
    configured.send(b"size-linked\n");
    configured.wait_for("ZETA_SIZE_23_96").await;
    configured.send(
        &[
            sgr_mouse(0, 97, 6, false),
            sgr_mouse(32, 105, 6, false),
            sgr_mouse(0, 105, 6, true),
        ]
        .concat(),
    );
    configured.send(b"size-linked\n");
    configured.wait_for("ZETA_SIZE_23_104").await;
    configured.send(b"\x02d");
    configured.wait_success().await;

    let mut configured_reset = spawn_client(124, linked_target.pane_id);
    configured_reset.wait_for("ZETA_READY").await;
    configured_reset.send(b"size-linked\n");
    configured_reset.wait_for("ZETA_SIZE_23_96").await;
    configured_reset.send(b"\x02d");
    configured_reset.wait_success().await;

    fs::write(
        config_directory.join("config.toml"),
        r#"
[ui.sidebar.left]
components = [
  { component = "workspaces", size = "fill", header = [{ text = "WORKSPACES" }], footer = [{ text = "MODE:" }, { token = "sidebar.visibility" }], row = { left = [{ text = "WS[" }], body = [{ token = "workspace.name" }], right = [{ text = "]" }, { token = "workspace.tab_count" }] } },
]
"#,
    )
    .unwrap();

    let mut narrow = spawn_client(123, main_pane);
    narrow.wait_for("ALPHA_READY").await;
    narrow.send(b"size-narrow\n");
    narrow.wait_for("NARROW_SIZE_23_95").await;
    narrow.send(b"\x02w");
    narrow.wait_for("feature").await;
    narrow.wait_for("λ").await;
    narrow.send(
        &[
            sgr_mouse(0, 28, 6, false),
            sgr_mouse(32, 20, 6, false),
            sgr_mouse(0, 20, 6, true),
        ]
        .concat(),
    );
    narrow.send(b"q");
    narrow.send(b"size-narrow\n");
    narrow.wait_for("NARROW_SIZE_23_103").await;
    narrow.send(b"\x02d");
    narrow.wait_success().await;

    let mut narrow_reset = spawn_client(123, main_pane);
    narrow_reset.wait_for("ALPHA_READY").await;
    narrow_reset.send(b"size-narrow\n");
    narrow_reset.wait_for("NARROW_SIZE_23_95").await;
    narrow_reset.send(b"\x02d");
    narrow_reset.wait_success().await;

    let mut live_close = spawn_client(124, linked_target.pane_id);
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
    resources_when(&harness, |snapshot| {
        snapshot.sessions[0].workspaces.len() == 1
            && snapshot.sessions[0].workspaces[0].name == "feature done"
    })
    .await;
    live_close.send(b"size-linked\n");
    live_close.wait_for("ZETA_SIZE_23_124").await;
    live_close.send(b"\x02w");
    live_close.wait_for("done").await;
    live_close.wait_for("MODE:automatic").await;
    live_close.clear_output();
    live_close.send(b"h");
    // A concurrent resource redraw may preserve the unchanged `MODE:`
    // prefix and emit only the changed suffix through the terminal diff.
    live_close.wait_for("den").await;
    live_close.clear_output();
    live_close.send(b"h");
    live_close.send(b"qsize-linked\n");
    live_close.wait_for("ZETA_SIZE_23_96").await;
    live_close.send(b"\x02wmqsize-linked\n");
    live_close.wait_for("ZETA_SIZE_23_118").await;
    live_close.send(b"\x02whqsize-linked\n");
    live_close.wait_for("ZETA_SIZE_23_124").await;
    live_close.send(b"\x02w");
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
async fn workspace_and_tab_bars_create_and_rename_logical_contexts() {
    let harness = Harness::start_with(
        "cd nested; printf 'CONTEXT_READY\\r\\n'; while IFS= read -r line; do :; done",
        |root| fs::create_dir_all(root.join("cwd/nested")).unwrap(),
    )
    .await;
    let snapshot = harness.resources().await;
    let original_workspace = snapshot.sessions[0].workspaces[0].id;
    let original_pane = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;

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
            "stty rows 24 cols 120; exec '{}' --socket '{}' pane attach {original_pane}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("CONTEXT_READY").await;

    client.send(b"\x02w");
    client.wait_for("hotkeys").await;
    client.send(b"c");
    let created_workspace = time::timeout(DEADLINE, async {
        loop {
            let snapshot = harness.resources().await;
            if snapshot.sessions[0].workspaces.len() == 2 {
                break snapshot.sessions[0].workspaces[1].id;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("logical workspace was not published");
    client.send(b"q");
    let created_cwd = harness.root.path().join("created-workspace-cwd");
    let created_command = format!(
        "\x1b[200~pwd > '{}'; printf 'CREATED_WORKSPACE_READY\\n'\n\x1b[201~",
        created_cwd.display()
    );
    time::timeout(DEADLINE, async {
        while !file_has_contents(&created_cwd) {
            client.send(created_command.as_bytes());
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "created workspace never accepted input: {:?}",
            client.text()
        )
    });
    assert_eq!(
        fs::read_to_string(&created_cwd).unwrap().trim(),
        fs::canonicalize(harness.root.path().join("cwd/nested"))
            .unwrap()
            .display()
            .to_string()
    );

    let roots = harness.resources().await.sessions[0]
        .workspaces
        .iter()
        .map(|workspace| workspace.root.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        roots[1],
        fs::canonicalize(harness.root.path().join("cwd/nested")).unwrap()
    );

    client.send(b"\x02wr");
    client.wait_for("Rename workspace").await;
    client.send("\x15context λ\r".as_bytes());
    time::timeout(DEADLINE, async {
        loop {
            let snapshot = harness.resources().await;
            if snapshot.sessions[0].workspaces[1].name == "context λ" {
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("workspace rename was not published");
    for _ in 0..10 {
        client.send(b"\x1b");
        time::sleep(Duration::from_millis(50)).await;
        client.send(b"\x02t");
        time::sleep(Duration::from_millis(50)).await;
    }
    client.send(b"r");
    client.wait_for("Rename tab").await;
    client.send("\x15main λ\r".as_bytes());
    time::timeout(DEADLINE, async {
        loop {
            let snapshot = harness.resources().await;
            if snapshot.sessions[0].workspaces[1].tabs[0].name == "main λ" {
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("first tab rename was not published");
    for _ in 0..10 {
        client.send(b"\x1b");
        time::sleep(Duration::from_millis(50)).await;
        client.send(b"\x02t");
        time::sleep(Duration::from_millis(50)).await;
    }
    client.send(b"c q");

    let second_tab_cwd = harness.root.path().join("created-tab-cwd");
    let tab_command = format!(
        "\x1b[200~pwd > '{}'; printf 'CREATED_TAB_READY\\n'\n\x1b[201~",
        second_tab_cwd.display()
    );
    time::timeout(DEADLINE, async {
        while !file_has_contents(&second_tab_cwd) {
            client.send(tab_command.as_bytes());
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("created tab never accepted input: {:?}", client.text()));
    assert_eq!(
        fs::read_to_string(&second_tab_cwd).unwrap().trim(),
        fs::canonicalize(harness.root.path().join("cwd/nested"))
            .unwrap()
            .display()
            .to_string()
    );

    for _ in 0..10 {
        client.send(b"\x1b");
        time::sleep(Duration::from_millis(50)).await;
        client.send(b"\x02t");
        time::sleep(Duration::from_millis(50)).await;
    }
    client.send(b"hr");
    client.wait_for("Rename tab").await;
    client.send(b"\x15first\r");
    time::timeout(DEADLINE, async {
        loop {
            let snapshot = harness.resources().await;
            if snapshot.sessions[0].workspaces[1].tabs[0].name == "first" {
                break;
            }
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("selected tab rename was not published");
    for _ in 0..10 {
        client.send(b"\r");
        time::sleep(Duration::from_millis(50)).await;
    }
    let returned = harness.root.path().join("returned-tab-cwd");
    let returned_command = format!("\x1b[200~pwd > '{}'\n\x1b[201~", returned.display());
    time::timeout(DEADLINE, async {
        while !returned.exists() {
            client.send(returned_command.as_bytes());
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("renamed tab was not selected: {:?}", client.text()));

    let final_snapshot = harness.resources().await;
    assert_eq!(
        final_snapshot.sessions[0].workspaces[0].id,
        original_workspace
    );
    assert_eq!(
        final_snapshot.sessions[0].workspaces[1].id,
        created_workspace
    );
    assert_eq!(final_snapshot.sessions[0].workspaces[1].name, "context λ");
    let tab_names = final_snapshot.sessions[0].workspaces[1]
        .tabs
        .iter()
        .map(|tab| tab.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(tab_names[0], "first");
    assert!(
        !tab_names[1].is_empty(),
        "unnamed tabs display their foreground process"
    );

    client.send(b"\x02d");
    client.wait_success().await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_command_bar_filters_labels_actions_and_matches_direct_dispatch() {
    let harness = Harness::start(
        "printf 'COMMAND_ALPHA_READY\\r\\n'; while IFS= read -r line; do case \"$line\" in alpha) printf 'COMMAND_ALPHA_ACK\\r\\n';; *) printf 'COMMAND_ALPHA_UNEXPECTED_%s\\r\\n' \"$line\";; esac; done",
    )
    .await;
    let snapshot = harness.resources().await;
    let tab_id = snapshot.sessions[0].workspaces[0].tabs[0].id;
    let pane_a = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'COMMAND_ZETA_READY\\r\\n'; while IFS= read -r line; do case \"$line\" in zeta) printf 'COMMAND_ZETA_ACK\\r\\n';; *) printf 'COMMAND_ZETA_UNEXPECTED_%s\\r\\n' \"$line\";; esac; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create command-bar sibling")
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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane_a}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("COMMAND_ALPHA_READY").await;
    client.wait_for("COMMAND_ZETA_READY").await;

    client.send(b"\x02:");
    client.wait_for("Search commands").await;
    client.wait_for("Ctrl-b s").await;
    client.send(b"\x1b[200~frobnicate\nzeta\x1b[201~");
    client.wait_for("No matching commands").await;
    client.send(b"\x15next pane\rzeta\n");
    client.wait_for("COMMAND_ZETA_ACK").await;

    client.send(b"\x02;alpha\n");
    client.wait_for("COMMAND_ALPHA_ACK").await;
    assert!(
        !client.text().contains("UNEXPECTED"),
        "command query leaked to a terminal: {:?}",
        client.text()
    );
    client.send(b"\x02d");
    client.wait_success().await;
    assert!(client.text().contains("\x1b[?2004h"));
    assert!(client.text().contains("\x1b[?2004l"));

    assert!(process_alive(pane_b.child_pid));
    harness.shutdown().await;
}

#[tokio::test]
async fn extension_commands_launch_from_the_palette_with_focused_context() {
    let harness = Harness::start_with("printf 'HOST_READY\\r\\n'; while :; do sleep 1; done", |root| {
        let extension = root.join("extension");
        let bin = extension.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            extension.join("fut-extension.toml"),
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = ['commands']\nid = 'palette-test'\n[commands.probe]\ntitle = 'Extension context probe'\nargv = ['./bin/probe']\n",
        )
        .unwrap();
        let probe = bin.join("probe");
        fs::write(
            &probe,
            "#!/bin/sh\nprintf 'ID=%s\\nCOMMAND=%s\\nROOT=%s\\nSOCKET=%s\\nCWD=%s\\n' \"$FUT_EXTENSION_ID\" \"$FUT_EXTENSION_COMMAND\" \"$FUT_EXTENSION_ROOT\" \"$FUT_SOCKET\" \"$PWD\" > \"$FUT_EXTENSION_ROOT/context\"\nprintf 'EXTENSION_COMMAND_READY\\r\\n'\nread -r _\n",
        )
        .unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("home/.config/fut/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            config,
            format!("extensions = [{:?}]\n", extension.display().to_string()),
        )
        .unwrap();
    })
    .await;
    let snapshot = harness.resources().await;
    let pane = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;
    let extension = harness
        .root
        .path()
        .join("extension")
        .canonicalize()
        .unwrap();
    let cwd = harness.root.path().join("cwd").canonicalize().unwrap();

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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("HOST_READY").await;
    client.send(b"\x02:");
    client.wait_for("Search commands").await;
    client.send(b"extension context probe\r");
    client.wait_for("EXTENSION_COMMAND_READY").await;
    let context = fs::read_to_string(extension.join("context")).unwrap();
    assert!(context.contains("ID=palette-test\n"), "{context:?}");
    assert!(context.contains("COMMAND=probe\n"), "{context:?}");
    assert!(
        context.contains(&format!("ROOT={}\n", extension.display())),
        "{context:?}"
    );
    assert!(
        context.contains(&format!("SOCKET={}\n", harness.socket.display())),
        "{context:?}"
    );
    assert!(
        context.contains(&format!("CWD={}\n", cwd.display())),
        "{context:?}"
    );
    client.send(b"\r");
    time::sleep(Duration::from_millis(100)).await;
    client.send(b"\x02d");
    client.wait_success().await;
    harness.shutdown().await;
}

#[tokio::test]
async fn compiled_rust_extension_conforms_through_public_fut_boundaries() {
    let mut harness = Harness::start_public_with(
        "printf 'RUST_CONFORMANCE_HOST_READY\\r\\n'; while :; do sleep 1; done",
        |root| {
            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples/extensions/rust-status");
            assert!(
                !source.join("bin/rust-status").exists(),
                "the compiled reference must not check in a platform binary"
            );
            let extension = root.join("rust-status");
            fs::create_dir_all(extension.join("src")).unwrap();
            fs::create_dir(extension.join("bin")).unwrap();
            fs::copy(
                source.join("fut-extension.toml"),
                extension.join("fut-extension.toml"),
            )
            .unwrap();
            fs::copy(source.join("src/main.rs"), extension.join("src/main.rs")).unwrap();

            let compiled = Command::new("rustc")
                .args(["--edition=2024", "-O", "-o"])
                .arg(extension.join("bin/rust-status"))
                .arg(extension.join("src/main.rs"))
                .output()
                .expect("compile dependency-free Rust extension fixture");
            assert!(
                compiled.status.success(),
                "rustc failed: {}",
                String::from_utf8_lossy(&compiled.stderr)
            );

            let log = root.join("rust-status.log");
            let config = root.join("home/.config/fut/config.toml");
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::write(
                config,
                format!(
                    "extensions = [{:?}]\n[extension.rust-status]\nlabel = 'global'\nlog_path = {:?}\n[ui.bindings]\n'rust-status:probe' = 'R'\n",
                    extension.display().to_string(),
                    log.display().to_string(),
                ),
            )
            .unwrap();

            let workspace_config = root.join("cwd/.fut/config.toml");
            fs::create_dir_all(workspace_config.parent().unwrap()).unwrap();
            fs::write(
                workspace_config,
                "[extension.rust-status]\nlabel = 'workspace'\n",
            )
            .unwrap();
        },
    )
    .await;
    let extension = harness.root.path().join("rust-status");
    let log = harness.root.path().join("rust-status.log");

    let validation = harness
        .cli()
        .args(["--json", "extension", "validate"])
        .arg(&extension)
        .output()
        .unwrap();
    assert!(
        validation.status.success(),
        "public validation failed: {}",
        String::from_utf8_lossy(&validation.stderr)
    );
    let validation: Value = serde_json::from_slice(&validation.stdout).unwrap();
    assert_eq!(validation["command"], "extension.validate");
    assert_eq!(validation["result"]["valid"], true);
    assert_eq!(validation["result"]["extension"]["id"], "rust-status");

    wait_for(DEADLINE, || {
        fs::read_to_string(&log)
            .is_ok_and(|contents| contents.contains("hook=workspace.created label=workspace"))
    })
    .await;

    let listed = harness.cli().args(["--json", "list"]).output().unwrap();
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let workspace = &listed["result"]["sessions"][0]["workspaces"][0];
    let workspace_id = workspace["id"].as_str().unwrap().to_owned();
    let pane_id = workspace["tabs"][0]["panes"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        workspace["tokens"]["workspace.extension.rust-status.last_event"],
        "workspace.created"
    );

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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane_id}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("RUST_CONFORMANCE_HOST_READY").await;
    client.send(b"\x02R");
    wait_for(DEADLINE, || {
        fs::read_to_string(&log)
            .is_ok_and(|contents| contents.contains("command=probe label=workspace"))
    })
    .await;
    client.send(b"\x02d");
    client.wait_success().await;

    let renamed = harness
        .cli()
        .args(["--json", "workspace", "rename", &workspace_id, "renamed"])
        .output()
        .unwrap();
    assert!(
        renamed.status.success(),
        "public rename failed: {}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    wait_for(DEADLINE, || {
        fs::read_to_string(&log)
            .is_ok_and(|contents| contents.contains("hook=workspace.renamed label=workspace"))
    })
    .await;

    wait_for(DEADLINE, || {
        let output = harness.cli().args(["--json", "list"]).output().unwrap();
        let Ok(snapshot) = serde_json::from_slice::<Value>(&output.stdout) else {
            return false;
        };
        snapshot["result"]["sessions"][0]["workspaces"][0]["tokens"]
            ["workspace.extension.rust-status.last_event"]
            == "workspace.renamed"
    })
    .await;
    let records = fs::read_to_string(&log).unwrap();
    assert!(records.contains("config={\"label\":\"workspace\",\"log_path\":"));

    let shutdown = harness
        .cli()
        .args(["--json", "daemon", "shutdown"])
        .output()
        .unwrap();
    assert!(shutdown.status.success());
    harness.wait_until_exited().await;
}

#[tokio::test]
async fn extension_command_can_activate_the_target_opened_by_its_child() {
    let harness = Harness::start_with("printf 'HOST_READY\r\n'; while :; do sleep 1; done", |root| {
        let extension = root.join("extension");
        let bin = extension.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir(extension.join("opened")).unwrap();
        fs::write(
            extension.join("fut-extension.toml"),
            "api_version = 1\nversion = '1.0.0'\nfut = '>=0.7.0, <1.0.0'\ncapabilities = ['commands']\nid = 'activate-test'\n[commands.open]\ntitle = 'Open and activate'\nargv = ['./bin/open']\nactivate_opened = true\n",
        )
        .unwrap();
        let open = bin.join("open");
        fs::write(
            &open,
            "#!/bin/sh\nexec \"$FUT_BIN\" --socket \"$FUT_SOCKET\" open --background \"$FUT_EXTENSION_ROOT/opened\" --name activated -- /bin/sh -c \"printf 'ACTIVATED_READY\\r\\n'; while :; do sleep 1; done\"\n",
        )
        .unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("home/.config/fut/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            config,
            format!("extensions = [{:?}]\n", extension.display().to_string()),
        )
        .unwrap();
    })
    .await;
    let snapshot = harness.resources().await;
    let pane = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;

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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("HOST_READY").await;
    client.send(b"\x02:open and activate\r");
    client.wait_for("ACTIVATED_READY").await;
    client.send(b"\x02d");
    client.wait_success().await;
    harness.shutdown().await;
}

#[tokio::test]
async fn keyboard_copy_search_uses_client_pbcopy_and_surfaces_success_and_failure() {
    let root = tempfile::Builder::new()
        .prefix("fut-copy-mode-e2e-")
        .tempdir()
        .unwrap();
    let input_log = root.path().join("pty-input");
    let script = format!(
        "printf 'COPY_TARGET λ雪\\r\\nCOPY_READY\\r\\n'; while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; [ \"$line\" = probe ] && printf 'AFTER_COPY_MODE\\r\\n'; done",
        input_log.display()
    );
    let harness = Harness::start(&script).await;
    let snapshot = harness.resources().await;
    let pane = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;

    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let pbcopy = bin.join("pbcopy");
    fs::write(
        &pbcopy,
        "#!/bin/sh\nif [ \"$PBCOPY_FAIL\" = 1 ] && [ ! -e \"$PBCOPY_FAILED_ONCE\" ]; then touch \"$PBCOPY_FAILED_ONCE\"; cat >/dev/null; exit 23; fi\ncount=0\n[ ! -f \"$PBCOPY_COUNT\" ] || read -r count < \"$PBCOPY_COUNT\"\ncount=$((count + 1))\ncat > \"$PBCOPY_CAPTURE.$count\"\nprintf '%s\\n' \"$count\" > \"$PBCOPY_COUNT\"\n",
    )
    .unwrap();
    fs::set_permissions(&pbcopy, fs::Permissions::from_mode(0o755)).unwrap();
    let capture = root.path().join("clipboard");
    let copy_count = root.path().join("clipboard-count");

    let spawn_client = |fail: bool| {
        let mut command = Command::new("/usr/bin/script");
        command
            .env_clear()
            .env("HOME", harness.root.path().join("home"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("PBCOPY_CAPTURE", &capture)
            .env("PBCOPY_COUNT", &copy_count)
            .env("PBCOPY_FAILED_ONCE", root.path().join("pbcopy-failed-once"))
            .env("PBCOPY_FAIL", if fail { "1" } else { "0" })
            .env("TMPDIR", harness.root.path().join("runtime"))
            .env("FUT_RUNTIME_DIR", harness.root.path().join("runtime"))
            .env("TERM", "xterm-256color")
            .args(["-q", "/dev/null", "/bin/sh", "-c"])
            .arg(format!(
                "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane}",
                env!("CARGO_BIN_EXE_fut"),
                harness.socket.display(),
            ));
        PtyChild::spawn(command)
    };

    let mut success = spawn_client(false);
    success.wait_for("COPY_READY").await;
    success.send(&[sgr_mouse(0, 1, 2, false), sgr_mouse(0, 1, 2, true)].concat());
    time::sleep(Duration::from_millis(100)).await;
    assert!(
        !PathBuf::from(format!("{}.1", capture.display())).exists(),
        "a plain click copied text"
    );
    success.send(
        &[
            sgr_mouse(0, 1, 2, false),
            sgr_mouse(32, 11, 2, false),
            sgr_mouse(0, 11, 2, true),
        ]
        .concat(),
    );
    let mouse_capture = PathBuf::from(format!("{}.1", capture.display()));
    wait_for(DEADLINE, || {
        fs::read_to_string(&mouse_capture).is_ok_and(|text| text == "COPY_TARGET")
    })
    .await;
    assert_eq!(fs::read_to_string(mouse_capture).unwrap(), "COPY_TARGET");
    success.wait_for("copied ").await;
    success.clear_output();
    for _ in 0..10 {
        // Let the begin reply become visible before testing the rapid
        // move/move/move/cancel queue. Otherwise a faster local protocol can
        // legitimately coalesce begin and cancel into the already-visible
        // cancellation notice, leaving no new terminal bytes to observe.
        success.send(b"\x02[");
        success.wait_for("COPY ·").await;
        success.clear_output();
        success.send(b"hjlq");
        success.wait_for("copy mode cancelled").await;
        success.clear_output();
    }
    for iteration in 1..=10 {
        // Begin, search, select, move, and copy arrive without pacing. The
        // client must preserve this exact order behind one wire request.
        let expected = if iteration % 2 == 0 {
            "COPY_READY"
        } else {
            "COPY_TARGET λ雪"
        };
        success.send(format!("\x02[/{expected}\r \x1b[Fy").as_bytes());
        let captured = PathBuf::from(format!("{}.{}", capture.display(), iteration + 1));
        wait_for(DEADLINE, || {
            fs::read_to_string(&captured).is_ok_and(|text| text == expected)
        })
        .await;
        assert_eq!(fs::read_to_string(captured).unwrap(), expected);
        success.wait_for("copied ").await;
        success.clear_output();
        if iteration < 10 {
            success.send(b"\x02d");
            success.wait_success().await;
            success = spawn_client(false);
            success.wait_for("COPY_READY").await;
        }
    }
    assert!(!input_log.exists(), "copy-mode keys reached the PTY");
    success.send(b"\x02d");
    success.wait_success().await;

    let mut failure = spawn_client(true);
    failure.wait_for("COPY_READY").await;
    failure.send(b"\x02[/COPY_TARGET\r \x1b[Fy");
    failure.wait_for("FAILED").await;
    assert!(!input_log.exists(), "failed copy exited copy mode");
    failure.clear_output();
    failure.send(b"y");
    let retried_capture = PathBuf::from(format!("{}.12", capture.display()));
    wait_for(DEADLINE, || {
        fs::read_to_string(&retried_capture).is_ok_and(|text| text == "COPY_TARGET λ雪")
    })
    .await;
    assert_eq!(
        fs::read_to_string(retried_capture).unwrap(),
        "COPY_TARGET λ雪"
    );
    failure.wait_for("copied ").await;
    failure.send(b"probe\n");
    failure.wait_for("AFTER_COPY_MODE").await;
    assert_eq!(fs::read_to_string(&input_log).unwrap(), "probe\n");
    failure.send(b"\x02d");
    failure.wait_success().await;

    harness.shutdown().await;
}

#[tokio::test]
async fn raw_copy_mode_ignores_wheel_and_reset_until_cancelled() {
    let mut script = String::new();
    for index in 0..40 {
        use std::fmt::Write as _;
        writeln!(&mut script, "printf 'RAW_SCROLL_{index:02}\\r\\n'").unwrap();
    }
    script.push_str(
        "stty raw -echo; printf '\\033[?1000h\\033[?1006hRAW_MOUSE_READY\r\n'; dd bs=1 count=10 of=wheel.tmp 2>/dev/null; mv wheel.tmp wheel.capture; printf 'RAW_MOUSE_CAPTURED\r\n'; while :; do sleep 1; done",
    );
    let mut harness = Harness::start(&script).await;
    let capture = harness.root.path().join("cwd/wheel.capture");
    let (mut connection, terminal_id, _) = harness.interactive().await;
    snapshot_containing(&mut connection, terminal_id, "RAW_MOUSE_READY").await;
    copy_command(&mut connection, terminal_id, CopyModeAction::Begin).await;
    let historical = copy_command(
        &mut connection,
        terminal_id,
        CopyModeAction::Search {
            query: "RAW_SCROLL_00".into(),
        },
    )
    .await;
    assert!(matches!(
        historical.message,
        ServerMessage::CopyModeSnapshot { .. }
    ));

    let wheel = mouse_event(
        MouseEventKind::Wheel {
            direction: MouseWheelDirection::Up,
        },
        0,
        0,
    );
    send(
        &mut connection,
        ClientMessage::MouseInput {
            terminal_id,
            event: wheel,
        },
    )
    .await;
    send(
        &mut connection,
        ClientMessage::ResetViewport { terminal_id },
    )
    .await;
    let request_id = Uuid::new_v4();
    send_envelope(
        &mut connection,
        Envelope {
            request_id: Some(request_id),
            message: ClientMessage::CopyMode {
                terminal_id,
                action: CopyModeAction::Move {
                    movement: CopyModeMovement::Right,
                },
            },
        },
    )
    .await;
    let response = time::timeout(DEADLINE, async {
        loop {
            let envelope = receive_envelope(&mut connection)
                .await
                .expect("copy-mode connection closed");
            if envelope.request_id == Some(request_id) {
                break envelope.message;
            }
            if matches!(
                envelope.message,
                ServerMessage::Snapshot {
                    terminal_id: id,
                    ref screen,
                } if id == terminal_id && screen.cells.iter().all(|cell| !cell.selected)
            ) {
                panic!("reset emitted a canonical snapshot during copy mode");
            }
        }
    })
    .await
    .expect("copy-mode update timed out");
    assert!(matches!(response, ServerMessage::CopyModeSnapshot { .. }));
    assert!(!capture.exists(), "active copy-mode wheel reached the PTY");

    copy_command(&mut connection, terminal_id, CopyModeAction::Cancel).await;
    send(
        &mut connection,
        ClientMessage::Input {
            bytes: b"after-copy".to_vec(),
        },
    )
    .await;
    wait_for(DEADLINE, || capture.exists()).await;
    assert_eq!(fs::read(capture).unwrap(), b"after-copy");

    harness.detach(&mut connection).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn asynchronous_copy_cursor_loss_precedes_the_canonical_snapshot() {
    let mut harness = Harness::start(
        "printf 'RAW_INVALIDATION_READY\r\n'; while [ ! -e invalidate-now ]; do sleep 0.01; done; printf '\\033[?1049hRAW_ALTERNATE_READY\r\n'; while :; do sleep 1; done",
    )
    .await;
    let (mut connection, terminal_id, _) = harness.interactive().await;
    snapshot_containing(&mut connection, terminal_id, "RAW_INVALIDATION_READY").await;
    copy_command(&mut connection, terminal_id, CopyModeAction::Begin).await;
    fs::write(harness.root.path().join("cwd/invalidate-now"), b"").unwrap();

    time::timeout(DEADLINE, async {
        let mut saw_cursor_loss = false;
        loop {
            let envelope = receive_envelope(&mut connection)
                .await
                .expect("connection closed during copy invalidation");
            match envelope.message {
                ServerMessage::CopyModeError {
                    terminal_id: id,
                    error: CopyModeError::CursorLost,
                    ..
                } if id == terminal_id => {
                    assert_eq!(envelope.request_id, None);
                    saw_cursor_loss = true;
                }
                ServerMessage::Snapshot {
                    terminal_id: id,
                    screen,
                } if id == terminal_id
                    && snapshot_text(&screen).contains("RAW_ALTERNATE_READY") =>
                {
                    assert!(
                        saw_cursor_loss,
                        "invalidating canonical snapshot arrived before typed cursor loss"
                    );
                    assert!(screen.cells.iter().all(|cell| !cell.selected));
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("copy cursor invalidation timed out");

    harness.detach(&mut connection).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_client_exits_copy_mode_on_unsolicited_cursor_loss() {
    let harness = Harness::start(
        "printf 'PUBLIC_INVALIDATION_READY\r\n'; while [ ! -e public-invalidate-now ]; do sleep 0.01; done; printf '\\033[?1049hPUBLIC_ALTERNATE_READY\r\n'; while IFS= read -r line; do [ \"$line\" = probe ] && printf 'PUBLIC_CLIENT_LEFT_COPY_MODE\r\n'; done",
    )
    .await;
    let pane = harness.resources().await.sessions[0].workspaces[0].tabs[0].panes[0].id;
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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("PUBLIC_INVALIDATION_READY").await;
    client.send(b"\x02[");
    client.wait_for("COPY ·").await;
    fs::write(harness.root.path().join("cwd/public-invalidate-now"), b"").unwrap();
    client.wait_for("copy-mode cursor").await;
    client.send(b"probe\n");
    client.wait_for("PUBLIC_CLIENT_LEFT_COPY_MODE").await;
    client.send(b"\x02d");
    client.wait_success().await;

    harness.shutdown().await;
}

#[tokio::test]
async fn copy_mode_attachments_cleanup_independently_across_output_focus_and_exit() {
    let mut harness = Harness::start(
        "printf 'COPY_A_READY\\r\\n'; (sleep 1; printf 'COPY_A_DURING\\r\\n') & while :; do sleep 1; done",
    )
    .await;
    let resources = harness.resources().await;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let pane_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].clone();
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(ClientMessage::CreatePane {
            tab_id,
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'COPY_B_READY\\r\\n'; while :; do sleep 1; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create copy-mode sibling")
    };

    let (mut client_a, terminal_a, _) = harness
        .interactive_for(Some(TargetSelector::Pane(pane_a.id)))
        .await;
    let (mut client_b, selected_b) =
        attach_once(&harness, TargetSelector::Pane(pane_b.pane_id)).await;
    snapshot_containing(&mut client_a, terminal_a, "COPY_A_READY").await;
    snapshot_containing(&mut client_b, selected_b.terminal_id, "COPY_B_READY").await;

    let begin_a = copy_command(&mut client_a, terminal_a, CopyModeAction::Begin).await;
    let begin_b = copy_command(&mut client_b, selected_b.terminal_id, CopyModeAction::Begin).await;
    assert!(matches!(
        begin_a.message,
        ServerMessage::CopyModeSnapshot { .. }
    ));
    assert!(matches!(
        begin_b.message,
        ServerMessage::CopyModeSnapshot { .. }
    ));
    copy_command(&mut client_a, terminal_a, CopyModeAction::ToggleSelection).await;
    copy_command(
        &mut client_b,
        selected_b.terminal_id,
        CopyModeAction::ToggleSelection,
    )
    .await;

    let during = receive_matching(&mut client_a, |message| {
        matches!(message, ServerMessage::Snapshot { terminal_id, screen }
            if *terminal_id == terminal_a
                && snapshot_text(screen).contains("COPY_A_DURING")
                && screen.cells.iter().any(|cell| cell.selected))
    })
    .await;
    assert!(matches!(during, ServerMessage::Snapshot { .. }));

    harness.detach(&mut client_a).await;
    let still_active = copy_command(
        &mut client_b,
        selected_b.terminal_id,
        CopyModeAction::Move {
            movement: CopyModeMovement::Left,
        },
    )
    .await;
    assert!(matches!(
        still_active.message,
        ServerMessage::CopyModeSnapshot { ref screen, .. }
            if screen.cells.iter().any(|cell| cell.selected)
    ));

    let switched = select_view_response(&mut client_b, TargetSelector::Pane(pane_a.id)).await;
    assert_eq!(switched.focused.terminal_id, terminal_a);
    let switched_back =
        select_view_response(&mut client_b, TargetSelector::Pane(pane_b.pane_id)).await;
    assert_eq!(switched_back.focused.terminal_id, selected_b.terminal_id);
    assert!(matches!(
        copy_command(&mut client_b, selected_b.terminal_id, CopyModeAction::Begin,)
            .await
            .message,
        ServerMessage::CopyModeSnapshot { .. }
    ));

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
    let mut exited = false;
    time::timeout(DEADLINE, async {
        loop {
            let Some(message) = receive(&mut client_b).await else {
                assert!(
                    exited,
                    "copy-mode attachment disconnected before terminal exit: {}",
                    harness.logs()
                );
                break;
            };
            match message {
                ServerMessage::TerminalExited { terminal_id, .. }
                    if terminal_id == selected_b.terminal_id =>
                {
                    exited = true;
                }
                ServerMessage::TargetSelected { selected }
                    if selected.focused.terminal_id == terminal_a =>
                {
                    assert!(exited, "fallback preceded terminal exit");
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("copy-mode terminal exit cleanup timed out");
    harness.shutdown().await;
}

#[tokio::test]
async fn copy_search_scans_scrollback_hard_newlines_unicode_repeat_and_recovers() {
    let mut script = String::new();
    for index in 0..40 {
        use std::fmt::Write as _;
        writeln!(&mut script, "printf 'SCROLL_{index:02}\\r\\n'").unwrap();
    }
    script.push_str(
        "printf 'HARD λ\\r\\nNEXT 雪\\r\\né-one é-two\\r\\nSEARCH_READY\\r\\n'; while :; do sleep 1; done",
    );
    let mut harness = Harness::start(&script).await;
    let (mut connection, terminal_id, _) = harness.interactive().await;
    snapshot_containing(&mut connection, terminal_id, "SEARCH_READY").await;
    copy_command(&mut connection, terminal_id, CopyModeAction::Begin).await;

    let scrollback = copy_command(
        &mut connection,
        terminal_id,
        CopyModeAction::Search {
            query: "SCROLL_00".into(),
        },
    )
    .await;
    let ServerMessage::CopyModeSnapshot {
        screen: scrollback, ..
    } = scrollback.message
    else {
        panic!("scrollback search did not return a snapshot")
    };
    assert!(snapshot_text(&scrollback).contains("SCROLL_00"));

    let hard = copy_command(
        &mut connection,
        terminal_id,
        CopyModeAction::Search {
            query: "HARD λ\nNEXT 雪".into(),
        },
    )
    .await;
    let ServerMessage::CopyModeSnapshot { screen: hard, .. } = hard.message else {
        panic!("hard-newline search did not return a snapshot")
    };
    assert!(
        snapshot_text(&hard).contains("HARD λ"),
        "hard-newline viewport was {:?}",
        snapshot_text(&hard)
    );

    let first = copy_command(
        &mut connection,
        terminal_id,
        CopyModeAction::Search {
            query: "\u{301}".into(),
        },
    )
    .await;
    let ServerMessage::CopyModeSnapshot { screen: first, .. } = first.message else {
        panic!("first Unicode search did not return a snapshot")
    };
    let first_cell = selected_cell(&first);
    let next = copy_command(
        &mut connection,
        terminal_id,
        CopyModeAction::RepeatSearch {
            direction: SearchDirection::Forward,
        },
    )
    .await;
    let ServerMessage::CopyModeSnapshot { screen: next, .. } = next.message else {
        panic!("forward repeat did not return a snapshot")
    };
    assert_ne!(selected_cell(&next), first_cell);
    let previous = copy_command(
        &mut connection,
        terminal_id,
        CopyModeAction::RepeatSearch {
            direction: SearchDirection::Backward,
        },
    )
    .await;
    let ServerMessage::CopyModeSnapshot {
        screen: previous, ..
    } = previous.message
    else {
        panic!("backward repeat did not return a snapshot")
    };
    assert_eq!(selected_cell(&previous), first_cell);

    assert!(matches!(
        copy_command(
            &mut connection,
            terminal_id,
            CopyModeAction::Search {
                query: "definitely absent".into(),
            },
        )
        .await
        .message,
        ServerMessage::CopyModeError {
            error: CopyModeError::NoMatch,
            ..
        }
    ));
    assert!(matches!(
        copy_command(
            &mut connection,
            terminal_id,
            CopyModeAction::RepeatSearch {
                direction: SearchDirection::Forward,
            },
        )
        .await
        .message,
        ServerMessage::CopyModeSnapshot { .. }
    ));
    assert!(matches!(
        copy_command(
            &mut connection,
            terminal_id,
            CopyModeAction::Search {
                query: "x".repeat(MAX_SEARCH_QUERY_BYTES + 1),
            },
        )
        .await
        .message,
        ServerMessage::CopyModeError {
            error: CopyModeError::SearchQueryTooLarge { .. },
            ..
        }
    ));

    copy_command(&mut connection, terminal_id, CopyModeAction::Cancel).await;
    harness.detach(&mut connection).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_client_paste_is_mode_aware_focused_literal_and_not_a_fut_prefix() {
    let harness = Harness::start("while :; do sleep 1; done").await;
    let snapshot = harness.resources().await;
    let tab_id = snapshot.sessions[0].workspaces[0].tabs[0].id;
    let capture_a = harness.root.path().join("paste-a.bin");
    let capture_b = harness.root.path().join("paste-b.bin");
    let evaluated = harness.root.path().join("paste-was-evaluated");
    let payload = format!("alpha 雪\n\x02d; touch '{}'", evaluated.display());
    let expected = [b"\x1b[200~".as_slice(), payload.as_bytes(), b"\x1b[201~"].concat();
    let capture_script = |name: &str, path: &std::path::Path| {
        format!(
            ": > '{}'; stty raw -echo; printf '\\033[?2004hPUBLIC_{name}_READY\\r\\n'; dd bs=1 count={} of='{}' 2>/dev/null; printf '\\033[?2004lPUBLIC_{name}_CAPTURED\\r\\n'; while :; do sleep 1; done",
            path.display(),
            expected.len(),
            path.display()
        )
    };
    let create = |script: String| ClientMessage::CreatePane {
        tab_id,
        cwd: None,
        program: Some("/bin/sh".into()),
        argv: vec!["-c".into(), script],
    };
    let ServerMessage::PaneCreated { selected: pane_a } = harness
        .control_command(create(capture_script("A", &capture_a)))
        .await
    else {
        panic!("failed to create focused paste pane")
    };
    let ServerMessage::PaneCreated { selected: pane_b } = harness
        .control_command(create(capture_script("B", &capture_b)))
        .await
    else {
        panic!("failed to create unfocused paste pane")
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
            pane_a.pane_id,
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("PUBLIC_A_READY").await;
    client.wait_for("PUBLIC_B_READY").await;

    client.send(
        [b"\x1b[200~".as_slice(), payload.as_bytes(), b"\x1b[201~"]
            .concat()
            .as_slice(),
    );
    client.wait_for("PUBLIC_A_CAPTURED").await;

    assert_eq!(fs::read(&capture_a).unwrap(), expected);
    assert!(
        fs::read(&capture_b).unwrap().is_empty(),
        "paste reached the unfocused terminal"
    );
    assert!(
        !evaluated.exists(),
        "paste content was evaluated by a shell"
    );
    assert!(process_alive(pane_a.child_pid));
    assert!(process_alive(pane_b.child_pid));

    client.send(b"\x02d");
    client.wait_success().await;
    harness.shutdown().await;
}

#[tokio::test]
async fn command_bar_create_failure_releases_input_to_the_original_terminal() {
    let harness = Harness::start_with_shell(
        "printf 'CREATE_FAILURE_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = after ] && printf 'CREATE_FAILURE_RECOVERED\\r\\n'; done",
        std::path::Path::new("/definitely/missing/fut-shell"),
    )
    .await;
    let snapshot = harness.resources().await;
    let pane = snapshot.sessions[0].workspaces[0].tabs[0].panes[0].id;

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
            "stty rows 24 cols 80; exec '{}' --socket '{}' pane attach {pane}",
            env!("CARGO_BIN_EXE_fut"),
            harness.socket.display(),
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("CREATE_FAILURE_READY").await;
    client.send(b"\x02:create tab\r");
    client.wait_for("create tab failed").await;
    client.send(b"after\n");
    client.wait_for("CREATE_FAILURE_RECOVERED").await;
    assert_eq!(
        harness.resources().await.sessions[0].workspaces[0]
            .tabs
            .len(),
        1
    );
    client.send(b"\x02d");
    client.wait_success().await;
    harness.shutdown().await;
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
        "[ui.tab_bar]\nposition = \"sideways\"\n",
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

#[test]
fn public_doctor_is_read_only_and_json_reports_configuration_errors() {
    let root = tempfile::Builder::new()
        .prefix("fut-e2e-doctor-")
        .tempdir()
        .unwrap();
    let home = root.path().join("home");
    let runtime = root.path().join("missing-runtime");
    fs::create_dir_all(&home).unwrap();

    let healthy = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "en_US.UTF-8")
        .env("TERM", "xterm-256color")
        .env("FUT_RUNTIME_DIR", &runtime)
        .arg("doctor")
        .output()
        .unwrap();
    assert!(
        healthy.status.success(),
        "{}",
        String::from_utf8_lossy(&healthy.stderr)
    );
    let human = String::from_utf8(healthy.stdout).unwrap();
    assert!(human.contains("Fut doctor"));
    assert!(human.contains("active font cannot be detected") || human.contains("visually verify"));
    assert!(!runtime.exists(), "doctor created the runtime directory");

    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o500)).unwrap();
    let unwritable = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .env("FUT_RUNTIME_DIR", &runtime)
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert_eq!(unwritable.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&unwritable.stdout).unwrap()["result"]["status"],
        "error"
    );
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir(&runtime).unwrap();

    let relative_socket = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .args(["--json", "--socket", "relative.sock", "doctor"])
        .output()
        .unwrap();
    assert_eq!(relative_socket.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&relative_socket.stdout).unwrap()["result"]["status"],
        "error"
    );

    let relative_runtime = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .env("FUT_RUNTIME_DIR", "relative-runtime")
        .current_dir(root.path())
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert!(relative_runtime.status.success());
    assert!(!root.path().join("relative-runtime").exists());

    let real_parent = root.path().join("real-runtime-parent");
    let linked_parent = root.path().join("linked-runtime-parent");
    fs::create_dir(&real_parent).unwrap();
    fs::set_permissions(&real_parent, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&real_parent, &linked_parent).unwrap();
    let linked_runtime = linked_parent.join("runtime");
    let linked_parent_output = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .env("FUT_RUNTIME_DIR", &linked_runtime)
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert!(linked_parent_output.status.success());
    assert!(!linked_runtime.exists());

    fs::create_dir_all(home.join(".config/fut")).unwrap();
    fs::write(
        home.join(".config/fut/config.toml"),
        "[ui.tab_bar]\nposition = 'sideways'\n",
    )
    .unwrap();
    let invalid = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "en_US.UTF-8")
        .env("TERM", "xterm-256color")
        .env("FUT_RUNTIME_DIR", &runtime)
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(invalid.stderr.is_empty());
    let lines = String::from_utf8(invalid.stdout).unwrap();
    assert_eq!(lines.lines().count(), 1);
    let envelope: serde_json::Value = serde_json::from_str(lines.trim()).unwrap();
    assert_eq!(envelope["version"], 1);
    assert_eq!(envelope["command"], "doctor");
    assert_eq!(envelope["result"]["status"], "error");
    assert!(
        !runtime.exists(),
        "doctor created state after a config error"
    );

    let stock_config = root.path().join("stock-config");
    fs::create_dir(&stock_config).unwrap();
    let overridden = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "en_US.UTF-8")
        .env("TERM", "xterm-256color")
        .env("FUT_RUNTIME_DIR", &runtime)
        .arg("--config-dir")
        .arg(&stock_config)
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert!(overridden.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&overridden.stdout).unwrap();
    let config = envelope["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "config")
        .unwrap();
    assert_eq!(config["details"]["source"], "--config-dir");
    assert_eq!(config["details"]["present"], false);

    let non_utf8 = root
        .path()
        .join(std::ffi::OsString::from_vec(b"config-\xff.toml".to_vec()));
    let non_utf8_output = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("FUT_CONFIG", non_utf8)
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .env("FUT_RUNTIME_DIR", &runtime)
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert_eq!(non_utf8_output.status.code(), Some(1));
    let non_utf8_json = String::from_utf8(non_utf8_output.stdout).unwrap();
    assert_eq!(non_utf8_json.lines().count(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&non_utf8_json).unwrap()["result"]["status"],
        "error"
    );
}

#[tokio::test]
async fn public_doctor_probes_a_running_daemon_without_mutating_resources() {
    let harness = Harness::start("while :; do sleep 1; done").await;
    let before = resources_when(&harness, |snapshot| {
        !snapshot.sessions[0].workspaces[0].tabs[0].name.is_empty()
    })
    .await;
    let output = harness.cli().args(["--json", "doctor"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let protocol = envelope["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "protocol")
        .unwrap();
    assert_eq!(protocol["status"], "ok");
    assert_eq!(
        protocol["details"]["client_protocol"],
        serde_json::json!(PROTOCOL_VERSION)
    );
    let after = harness.resources().await;
    assert_eq!(without_observations(after), without_observations(before));
    harness.shutdown().await;
}

#[tokio::test]
async fn public_client_transfers_focus_when_the_focused_pane_exits() {
    let harness = Harness::start(
        "printf 'PRIMARY_EXIT_TARGET\r\n'; while IFS= read -r line; do [ \"$line\" = exit ] && exit 23; done",
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
                "trap 'printf SURVIVOR_FOCUSED\\r\\n' WINCH; printf 'SURVIVOR_PANE_READY\r\n'; while :; do IFS= read -r line || continue; [ \"$line\" = b ] && printf 'EXIT_B_INPUT\r\n'; done".into(),
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
    client.wait_for("PRIMARY_EXIT_TARGET").await;
    client.wait_for("SURVIVOR_PANE_READY").await;
    client.send(b"exit\n");
    client.wait_for("SURVIVOR_FOCUSED").await;
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
    client.send(b"\x02s");
    client.wait_for(" navigator").await;
    client.send(b"\x14\x1b[A\r");
    client.wait_for_count("CTRL_C_A_READY", 2).await;
    time::timeout(DEADLINE, async {
        while !client.text().contains("CTRL_C_A_INPUT") {
            client.send(b"a\n");
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("focused tab never accepted input: {:?}", client.text()));
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
            project: None,
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
        project: None,
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
            project: None,
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
            project: None,
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
async fn control_connections_never_reply_to_fire_and_forget_ui_messages() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let resources = harness.resources().await;
    let tab_id = resources.sessions[0].workspaces[0].tabs[0].id;
    let terminal_id = resources.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let mut control = harness.connect().await.expect("connect control client");
    assert!(matches!(
        hello(&mut control, ClientMode::Control, PROTOCOL_VERSION)
            .await
            .expect("receive control welcome"),
        ServerMessage::Welcome { .. }
    ));

    for message in [
        mouse_wheel_message(terminal_id),
        ClientMessage::ResetViewport { terminal_id },
        ClientMessage::ResizeSplit {
            tab_id,
            split_id: fut::domain::SplitId::new(),
            ratio: fut::splits::SplitRatio::from_cells(1, 2).unwrap(),
        },
    ] {
        send_uncorrelated(&mut control, message.clone()).await;
        let ping_request_id = Uuid::new_v4();
        send_envelope(
            &mut control,
            Envelope {
                request_id: Some(ping_request_id),
                message: ClientMessage::Ping,
            },
        )
        .await;
        assert!(matches!(
            receive_envelope(&mut control).await,
            Some(Envelope {
                request_id: Some(request_id),
                message: ServerMessage::Pong { .. },
            }) if request_id == ping_request_id
        ));

        assert_fire_and_forget_request_id_rejected(&mut control, message).await;
    }

    drop(control);
    harness.shutdown().await;
}

#[test]
fn protocol_0_daemon_rejects_current_client_and_accepts_shutdown() {
    let root = tempfile::Builder::new()
        .prefix("fut-e2e-protocol-upgrade-")
        .tempdir()
        .unwrap();
    let runtime = root.path().join("runtime");
    let home = root.path().join("home");
    let cwd = root.path().join("cwd");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let socket = runtime.join("fut.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();

                let (stream, _) = listener.accept().await.unwrap();
                let mut first = Framed::new(stream, codec());
                let hello: Envelope<ClientMessage> =
                    decode_payload(&first.next().await.unwrap().unwrap()).unwrap();
                assert!(matches!(
                    hello.message,
                    ClientMessage::Hello {
                        version: PROTOCOL_VERSION,
                        ..
                    }
                ));
                first
                    .send(Bytes::from(
                        encode_payload(&Envelope {
                            request_id: hello.request_id,
                            message: ServerMessage::IncompatibleProtocol {
                                client: PROTOCOL_VERSION,
                                server: PROTOCOL_VERSION_0_1,
                            },
                        })
                        .unwrap(),
                    ))
                    .await
                    .unwrap();

                let (stream, _) = listener.accept().await.unwrap();
                let mut retry = Framed::new(stream, codec());
                let hello: Envelope<ClientMessage> =
                    decode_payload(&retry.next().await.unwrap().unwrap()).unwrap();
                assert!(matches!(
                    hello.message,
                    ClientMessage::Hello {
                        version: PROTOCOL_VERSION,
                        ..
                    }
                ));
                retry
                    .send(Bytes::from(
                        encode_payload(&Envelope {
                            request_id: hello.request_id,
                            message: ServerMessage::IncompatibleProtocol {
                                client: PROTOCOL_VERSION,
                                server: PROTOCOL_VERSION_0_1,
                            },
                        })
                        .unwrap(),
                    ))
                    .await
                    .unwrap();

                let (stream, _) = listener.accept().await.unwrap();
                let mut shutdown = Framed::new(stream, codec());
                let hello: Envelope<ClientMessage> =
                    decode_payload(&shutdown.next().await.unwrap().unwrap()).unwrap();
                assert!(matches!(
                    hello.message,
                    ClientMessage::Hello {
                        version: PROTOCOL_VERSION_0_1,
                        ..
                    }
                ));
                shutdown
                    .send(Bytes::from(
                        encode_payload(&Envelope {
                            request_id: hello.request_id,
                            message: ServerMessage::Welcome {
                                version: PROTOCOL_VERSION_0_1,
                                server_version: "0.1.0".into(),
                                selected: None,
                                extension_catalog: fut::protocol::ExtensionCatalog {
                                    generation: 1,
                                    fingerprint: "0".repeat(64),
                                    extensions: Vec::new(),
                                    config: fut::protocol::ExtensionCatalogConfig::default(),
                                },
                            },
                        })
                        .unwrap(),
                    ))
                    .await
                    .unwrap();
                let request: Envelope<ClientMessage> =
                    decode_payload(&shutdown.next().await.unwrap().unwrap()).unwrap();
                assert_eq!(request.message, ClientMessage::Shutdown);
                shutdown
                    .send(Bytes::from(
                        encode_payload(&Envelope {
                            request_id: request.request_id,
                            message: ServerMessage::CommandCompleted {
                                command: fut::protocol::AcknowledgedCommand::Shutdown,
                            },
                        })
                        .unwrap(),
                    ))
                    .await
                    .unwrap();
            });
    });

    let current = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("TMPDIR", &runtime)
        .env("FUT_RUNTIME_DIR", &runtime)
        .current_dir(&cwd)
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(!current.status.success());
    let current_error = String::from_utf8_lossy(&current.stderr);
    assert!(current_error.contains("uses protocol 0"), "{current_error}");
    assert!(
        current_error.contains(&format!("requires protocol {PROTOCOL_VERSION}")),
        "{current_error}"
    );
    assert!(socket.exists());
    assert!(!runtime.join("fut-daemon.log").exists());

    let shutdown = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("TMPDIR", &runtime)
        .env("FUT_RUNTIME_DIR", &runtime)
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "shutdown"])
        .output()
        .unwrap();
    assert!(
        shutdown.status.success(),
        "{}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    assert!(String::from_utf8_lossy(&shutdown.stdout).contains("shutdown=true"));
    server.join().unwrap();
}

#[tokio::test]
async fn focused_exit_falls_back_through_previous_tabs_in_the_same_session() {
    let harness = Harness::start(
        "printf 'FALLBACK_A_READY\r\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'FALLBACK_A_INPUT\r\n'; done",
    )
    .await;
    let initial = harness.resources().await;
    let workspace_id = initial.sessions[0].workspaces[0].id;
    let terminal_a = initial.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let ServerMessage::TabCreated { selected: tab_b } = harness
        .control_command(ClientMessage::CreateTab {
            workspace_id,
            name: Some("second".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'FALLBACK_B_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = b ] && printf 'FALLBACK_B_INPUT\\r\\n'; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create second fallback tab")
    };
    let ServerMessage::TabCreated { selected: tab_c } = harness
        .control_command(ClientMessage::CreateTab {
            workspace_id,
            name: Some("third".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'FALLBACK_C_READY\\r\\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create third fallback tab")
    };

    let (mut attached, _) =
        attach_once(&harness, TargetSelector::Terminal(tab_c.terminal_id)).await;
    snapshot_containing(&mut attached, tab_c.terminal_id, "FALLBACK_C_READY").await;
    send(&mut attached, ClientMessage::Input { bytes: vec![0x04] }).await;
    receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TerminalExited { terminal_id, .. } if *terminal_id == tab_c.terminal_id)
    })
    .await;
    let ServerMessage::TargetSelected { selected } = receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TargetSelected { selected } if selected.focused.terminal_id == tab_b.terminal_id)
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(selected.focused.tab_id, tab_b.tab_id);
    let ServerMessage::ResourcesChanged { snapshot } = receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::ResourcesChanged { snapshot } if snapshot.sessions[0].workspaces[0].tabs.iter().all(|tab| tab.id != tab_c.tab_id))
    })
    .await
    else {
        unreachable!()
    };
    assert!(
        snapshot.sessions[0].workspaces[0]
            .tabs
            .iter()
            .all(|tab| tab.id != tab_c.tab_id)
    );
    send_uncorrelated(&mut attached, mouse_wheel_message(tab_c.terminal_id)).await;
    send_uncorrelated(
        &mut attached,
        ClientMessage::ResetViewport {
            terminal_id: tab_c.terminal_id,
        },
    )
    .await;
    assert_no_error_before_pong(&mut attached).await;
    send(
        &mut attached,
        ClientMessage::Input {
            bytes: b"b\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut attached, tab_b.terminal_id, "FALLBACK_B_INPUT").await;

    send(&mut attached, ClientMessage::Input { bytes: vec![0x04] }).await;
    receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TerminalExited { terminal_id, .. } if *terminal_id == tab_b.terminal_id)
    })
    .await;
    receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TargetSelected { selected } if selected.focused.terminal_id == terminal_a)
    })
    .await;
    send(
        &mut attached,
        ClientMessage::Input {
            bytes: b"a\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut attached, terminal_a, "FALLBACK_A_INPUT").await;

    harness.detach(&mut attached).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn focused_exit_falls_back_to_another_workspace_in_the_same_session() {
    let harness = Harness::start(
        "printf 'WORKSPACE_FALLBACK_READY\\r\\n'; while IFS= read -r line; do [ \"$line\" = fallback ] && printf 'WORKSPACE_FALLBACK_INPUT\\r\\n'; done",
    )
    .await;
    let initial = harness.resources().await;
    let session_id = initial.sessions[0].id;
    let original_terminal = initial.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let (mut attached, _) =
        attach_once(&harness, TargetSelector::Terminal(original_terminal)).await;
    snapshot_containing(&mut attached, original_terminal, "WORKSPACE_FALLBACK_READY").await;

    send(
        &mut attached,
        ClientMessage::CreateWorkspace {
            session_id,
            name: Some("temporary".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'EXITING_WORKSPACE_READY\\r\\n'; while IFS= read -r line; do :; done"
                    .into(),
            ],
        },
    )
    .await;
    let ServerMessage::WorkspaceCreated {
        selected: temporary,
    } = receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::WorkspaceCreated { .. })
    })
    .await
    else {
        unreachable!()
    };
    receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TargetSelected { selected } if selected.focused.terminal_id == temporary.terminal_id)
    })
    .await;
    snapshot_containing(
        &mut attached,
        temporary.terminal_id,
        "EXITING_WORKSPACE_READY",
    )
    .await;

    send(&mut attached, ClientMessage::Input { bytes: vec![0x04] }).await;
    receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TerminalExited { terminal_id, .. } if *terminal_id == temporary.terminal_id)
    })
    .await;
    receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TargetSelected { selected } if selected.focused.terminal_id == original_terminal)
    })
    .await;
    send(
        &mut attached,
        ClientMessage::Input {
            bytes: b"fallback\n".to_vec(),
        },
    )
    .await;
    snapshot_containing(&mut attached, original_terminal, "WORKSPACE_FALLBACK_INPUT").await;

    harness.detach(&mut attached).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn public_ctrl_d_closes_a_tab_and_returns_to_its_predecessor() {
    let harness = Harness::start(
        "printf 'PREDECESSOR_TERMINAL_ACTIVE\r\n'; while IFS= read -r line; do [ \"$line\" = a ] && printf 'PREDECESSOR_INPUT_ROUTED\r\n'; done",
    )
    .await;
    let resources = harness.resources().await;
    let workspace_id = resources.sessions[0].workspaces[0].id;
    let terminal_a = resources.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id;
    let ServerMessage::TabCreated { selected: tab_b } = harness
        .control_command(ClientMessage::CreateTab {
            workspace_id,
            name: Some("temporary".into()),
            cwd: None,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'CTRL_D_B_READY\\r\\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create Ctrl-D tab")
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
            tab_b.terminal_id
        ));
    let mut client = PtyChild::spawn(command);
    client.wait_for("CTRL_D_B_READY").await;
    client.send(&[0x04]);
    client.wait_for("PREDECESSOR_TERMINAL_ACTIVE").await;
    client.send(b"a\n");
    client.wait_for("PREDECESSOR_INPUT_ROUTED").await;
    client.send(b"\x02d");
    client.wait_success().await;

    let remaining = harness.resources().await;
    assert_eq!(remaining.sessions[0].workspaces[0].tabs.len(), 1);
    assert_eq!(
        remaining.sessions[0].workspaces[0].tabs[0].panes[0].terminal_id,
        terminal_a
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn last_terminal_exit_detaches_instead_of_crossing_sessions() {
    let mut harness = Harness::start("while IFS= read -r line; do :; done").await;
    let first = harness.resources().await;
    let first_session = first.sessions[0].id;
    let second_cwd = harness.root.path().join("other-session");
    fs::create_dir(&second_cwd).unwrap();
    let ServerMessage::LocationOpened {
        selected: second, ..
    } = harness
        .control_command(ClientMessage::OpenLocation {
            project: None,
            name: Some("other".into()),
            cwd: second_cwd,
            program: Some("/bin/sh".into()),
            argv: vec![
                "-c".into(),
                "printf 'OTHER_SESSION_READY\\r\\n'; while IFS= read -r line; do :; done".into(),
            ],
        })
        .await
    else {
        panic!("failed to create second session")
    };
    let sessions = harness.resources().await;
    assert_eq!(sessions.sessions.len(), 2);
    let second_session = sessions
        .sessions
        .iter()
        .find(|session| session.id != first_session)
        .unwrap()
        .id;

    let (mut attached, _) =
        attach_once(&harness, TargetSelector::Terminal(second.terminal_id)).await;
    snapshot_containing(&mut attached, second.terminal_id, "OTHER_SESSION_READY").await;
    send(&mut attached, ClientMessage::Input { bytes: vec![0x04] }).await;
    receive_matching(&mut attached, |message| {
        matches!(message, ServerMessage::TerminalExited { terminal_id, .. } if *terminal_id == second.terminal_id)
    })
    .await;
    time::timeout(DEADLINE, async {
        while let Some(frame) = attached.next().await {
            let envelope: Envelope<ServerMessage> = decode_payload(&frame.unwrap()).unwrap();
            assert!(
                !matches!(envelope.message, ServerMessage::TargetSelected { .. }),
                "client crossed into another session after its session closed"
            );
        }
    })
    .await
    .expect("client did not detach after its last terminal exited");

    let remaining = resources_when(&harness, |snapshot| snapshot.sessions.len() == 1).await;
    assert_eq!(remaining.sessions[0].id, first_session);
    assert!(
        !remaining
            .sessions
            .iter()
            .any(|session| session.id == second_session)
    );
    assert!(harness.daemon.try_wait().unwrap().is_none());
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
            project: None,
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
    assert_eq!(after.sessions[0].workspaces[0].tabs[0].name, "タブ 雪 v6");
    let mut expected = before.clone();
    expected.sessions[0].name = "セッション 六".into();
    expected.sessions[0].workspaces[0].name = "作業 空間 λ".into();
    assert_eq!(
        without_observations(after.clone()),
        without_observations(expected),
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
    assert_eq!(
        without_observations(harness.resources().await),
        without_observations(after.clone())
    );

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
            name: "interactive".into(),
        },
    )
    .await;
    assert!(matches!(
        receive_matching(&mut attached, |message| matches!(
            message,
            ServerMessage::TargetRenamed { .. }
        ))
        .await,
        ServerMessage::TargetRenamed { .. }
    ));
    let renamed_tab = harness.resources().await;
    assert_eq!(
        renamed_tab.sessions[0].workspaces[0].tabs[0].name,
        "interactive"
    );
    assert_eq!(
        without_observations(renamed_tab),
        without_observations(after.clone())
    );

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
        vec!["session".into(), "rename".into(), session_id.to_string()],
        vec![
            "workspace".into(),
            "rename".into(),
            workspace_id.to_string(),
        ],
        vec!["tab".into(), "rename".into(), tab_id.to_string()],
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
            without_observations(harness.resources().await),
            without_observations(after.clone()),
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
    interactive_mode_with_size(selector, SIZE)
}

fn interactive_mode_with_size(selector: Option<TargetSelector>, size: TerminalSize) -> ClientMode {
    ClientMode::Interactive { size, selector }
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

async fn copy_command(
    connection: &mut Connection,
    terminal_id: TerminalId,
    action: CopyModeAction,
) -> Envelope<ServerMessage> {
    let request_id = Uuid::new_v4();
    send_envelope(
        connection,
        Envelope {
            request_id: Some(request_id),
            message: ClientMessage::CopyMode {
                terminal_id,
                action,
            },
        },
    )
    .await;
    loop {
        let response = receive_envelope(connection)
            .await
            .expect("connection closed during copy-mode command");
        if response.request_id == Some(request_id) {
            return response;
        }
    }
}

async fn send_uncorrelated(connection: &mut Connection, message: ClientMessage) {
    send_envelope(
        connection,
        Envelope {
            request_id: None,
            message,
        },
    )
    .await;
}

fn mouse_wheel_message(terminal_id: TerminalId) -> ClientMessage {
    ClientMessage::MouseInput {
        terminal_id,
        event: mouse_event(
            MouseEventKind::Wheel {
                direction: MouseWheelDirection::Up,
            },
            0,
            0,
        ),
    }
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: MouseModifiers::default(),
        buttons: MouseButtons::default(),
    }
}

fn sgr_mouse(code: u16, column: u16, row: u16, release: bool) -> Vec<u8> {
    format!(
        "\x1b[<{code};{column};{row}{}",
        if release { 'm' } else { 'M' }
    )
    .into_bytes()
}

async fn assert_no_error_before_pong(connection: &mut Connection) {
    let request_id = Uuid::new_v4();
    send_envelope(
        connection,
        Envelope {
            request_id: Some(request_id),
            message: ClientMessage::Ping,
        },
    )
    .await;
    loop {
        let response = receive_envelope(connection)
            .await
            .expect("connection closed before ping response");
        if let ServerMessage::Error { code, message } = response.message {
            panic!("unexpected daemon error ({code}): {message}");
        }
        if response.request_id == Some(request_id) {
            assert!(matches!(response.message, ServerMessage::Pong { .. }));
            return;
        }
    }
}

async fn assert_fire_and_forget_request_id_rejected(
    connection: &mut Connection,
    message: ClientMessage,
) {
    let request_id = Uuid::new_v4();
    send_envelope(
        connection,
        Envelope {
            request_id: Some(request_id),
            message,
        },
    )
    .await;
    loop {
        let response = receive_envelope(connection)
            .await
            .expect("connection closed before fire-and-forget rejection");
        if response.request_id == Some(request_id) {
            assert!(matches!(
                response.message,
                ServerMessage::Error { ref code, .. } if code == "request_id_not_allowed"
            ));
            return;
        }
    }
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
    let envelope: Envelope<ServerMessage> = decode_payload(&frame).expect("decode server frame");
    Some(Envelope {
        request_id: envelope.request_id,
        message: track_screen(connection, envelope.message),
    })
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
            message: ClientMessage::SelectTarget {
                selector,
                expected: None,
            },
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
            message: ClientMessage::SelectTarget {
                selector,
                expected: None,
            },
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

async fn snapshot_with_size(
    connection: &mut Connection,
    terminal_id: TerminalId,
    size: TerminalSize,
) -> ScreenSnapshot {
    let message = receive_matching(connection, |message| {
        matches!(message, ServerMessage::Snapshot { terminal_id: id, screen } if *id == terminal_id && screen.size == size)
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

fn selected_cell(snapshot: &ScreenSnapshot) -> usize {
    snapshot
        .cells
        .iter()
        .position(|cell| cell.selected)
        .expect("copy-mode snapshot has no selected cursor cell")
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

async fn wait_for_file(path: PathBuf) {
    time::timeout(DEADLINE, async {
        while !path.exists() {
            time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("marker did not appear before timeout: {}", path.display()));
}

async fn wait_for_nonempty_file(path: &std::path::Path) {
    wait_for(DEADLINE, || file_has_contents(path)).await;
}

fn file_has_contents(path: &std::path::Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0)
}

async fn chaos_identify_pane(
    client: &mut PtyChild,
    root: &std::path::Path,
    step: usize,
    seed: u64,
) -> String {
    let marker = root.join("cwd").join(format!("a{step}"));
    let command = format!("echo $$>'{}'\n", marker.display());
    let paste = format!("\x1b[200~{command}\x1b[201~");
    time::sleep(Duration::from_millis(100)).await;
    time::timeout(DEADLINE, async {
        loop {
            if file_has_contents(&marker) {
                break;
            }
            client.send(paste.as_bytes());
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("chaos identification timed out: seed={seed} step={step}"));
    fs::read_to_string(marker).unwrap()
}

async fn chaos_probe_pane(
    client: &mut PtyChild,
    root: &std::path::Path,
    step: usize,
    process: &str,
    seed: u64,
) {
    let marker = root.join("cwd").join(format!("p{step}"));
    let command = format!("echo $$>'{}'\n", marker.display());
    let paste = format!("\x1b[200~{command}\x1b[201~");
    time::sleep(Duration::from_millis(100)).await;
    time::timeout(DEADLINE, async {
        loop {
            if file_has_contents(&marker) {
                break;
            }
            client.send(paste.as_bytes());
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("chaos probe timed out: seed={seed} step={step}"));
    assert_eq!(
        fs::read_to_string(marker).unwrap(),
        process,
        "chaos input reached the wrong pane: seed={seed} step={step}; output={:?}",
        client.text()
    );
}

async fn resources_when(
    harness: &Harness,
    condition: impl Fn(&fut::resources::ResourceSnapshot) -> bool,
) -> fut::resources::ResourceSnapshot {
    resources_when_with_timeout(harness, DEADLINE, condition).await
}

async fn resources_when_with_timeout(
    harness: &Harness,
    deadline: Duration,
    condition: impl Fn(&fut::resources::ResourceSnapshot) -> bool,
) -> fut::resources::ResourceSnapshot {
    time::timeout(deadline, async {
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

fn workspace_git_tokens(snapshot: &fut::resources::ResourceSnapshot) -> [Option<&str>; 3] {
    let tokens = &snapshot.sessions[0].workspaces[0].tokens;
    [
        tokens.get("workspace.git_branch").map(String::as_str),
        tokens.get("workspace.git_added").map(String::as_str),
        tokens.get("workspace.git_deleted").map(String::as_str),
    ]
}

/// Strip the state the daemon's background observers write on their own clock
/// — the revision, pane locations, automatic tab names, and published tokens —
/// so before/after snapshot comparisons only see the caller's mutations.
fn without_observations(
    mut snapshot: fut::resources::ResourceSnapshot,
) -> fut::resources::ResourceSnapshot {
    snapshot.revision = 0;
    for session in &mut snapshot.sessions {
        session.tokens.clear();
        for workspace in &mut session.workspaces {
            workspace.tokens.clear();
            for tab in &mut workspace.tabs {
                tab.name.clear();
                tab.tokens.clear();
                for pane in &mut tab.panes {
                    pane.cwd = None;
                    pane.worktree = None;
                    pane.tokens.clear();
                }
            }
        }
    }
    snapshot
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

    fn complete_from(
        &self,
        socket: &std::path::Path,
        words: &[&str],
        terminal_id: TerminalId,
    ) -> std::process::Output {
        self.command()
            .env("FUT_SOCKET", self.0.path().join("missing-env.sock"))
            .env("FUT_TERMINAL_ID", terminal_id.to_string())
            .env("_CLAP_COMPLETE_INDEX", (words.len() + 2).to_string())
            .args(["--", "fut", "--socket"])
            .arg(socket)
            .args(words)
            .output()
            .unwrap()
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

#[test]
fn agent_skill_prints_the_bundled_skill_without_daemon_setup() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("runtime");
    let output = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env("FUT_RUNTIME_DIR", &runtime)
        .args(["agent", "skill"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("../skills/fut/SKILL.md")
    );
    assert!(!runtime.exists());
}

#[test]
fn project_list_and_ls_are_daemonless_ordered_and_versioned() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    let alpha = root.path().join("alpha");
    let beta = root.path().join("beta");
    let runtime = root.path().join("runtime");
    fs::create_dir(&config).unwrap();
    fs::create_dir(&alpha).unwrap();
    fs::create_dir(&beta).unwrap();
    fs::write(
        config.join("config.toml"),
        format!(
            "[projects.beta]\npath = {:?}\n[projects.alpha]\npath = {:?}\n",
            beta, alpha
        ),
    )
    .unwrap();

    let command = |operation: &str, json: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fut"));
        command
            .env_clear()
            .env("HOME", root.path())
            .env("FUT_RUNTIME_DIR", &runtime)
            .args(["--config-dir", config.to_str().unwrap()]);
        if json {
            command.arg("--json");
        }
        command.args(["project", operation]).output().unwrap()
    };

    let human = command("ls", false);
    assert!(human.status.success());
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        format!("alpha\t{}\nbeta\t{}\n", alpha.display(), beta.display())
    );

    let machine = command("list", true);
    assert!(machine.status.success());
    let machine: Value = serde_json::from_slice(&machine.stdout).unwrap();
    assert_eq!(machine["command"], "project.list");
    assert_eq!(machine["result"]["projects"][0]["name"], "alpha");
    assert_eq!(
        machine["result"]["projects"][0]["path"],
        alpha.to_str().unwrap()
    );
    assert_eq!(machine["result"]["projects"][1]["name"], "beta");
    assert_eq!(
        machine["result"]["projects"][1]["path"],
        beta.to_str().unwrap()
    );
    assert!(!runtime.exists());
}

#[tokio::test]
async fn context_resolves_fresh_ancestry_from_terminal_identity_and_get_resolves_explicit_ids() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let snapshot = harness.resources().await;
    let session = &snapshot.sessions[0];
    let workspace = &session.workspaces[0];
    let tab = &workspace.tabs[0];
    let pane = &tab.panes[0];

    let report = harness
        .cli()
        .args([
            "terminal",
            "report",
            "working",
            "--terminal-id",
            &pane.terminal_id.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );

    let explicit = harness
        .cli()
        .args(["--json", "get", &pane.terminal_id.to_string()])
        .output()
        .unwrap();
    assert!(explicit.status.success());
    let explicit: Value = serde_json::from_slice(&explicit.stdout).unwrap();
    assert_eq!(explicit["version"], 1);
    assert_eq!(explicit["command"], "get");
    assert_eq!(explicit["result"]["target"]["kind"], "terminal");
    assert_eq!(
        explicit["result"]["target"]["pane"]["activity"]["state"],
        "working"
    );
    for (id, kind) in [
        (session.id.to_string(), "session"),
        (workspace.id.to_string(), "workspace"),
        (tab.id.to_string(), "tab"),
        (pane.id.to_string(), "pane"),
    ] {
        let output = harness.cli().args(["--json", "get", &id]).output().unwrap();
        assert!(output.status.success(), "get {kind} failed");
        let output: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output["result"]["target"]["kind"], kind);
    }

    let context = harness
        .cli()
        .env("FUT_SESSION_ID", session.id.to_string())
        .env("FUT_WORKSPACE_ID", workspace.id.to_string())
        .env("FUT_TAB_ID", tab.id.to_string())
        .env("FUT_PANE_ID", pane.id.to_string())
        .env("FUT_TERMINAL_ID", pane.terminal_id.to_string())
        .args(["--json", "context"])
        .output()
        .unwrap();
    assert!(context.status.success());
    let context: Value = serde_json::from_slice(&context.stdout).unwrap();
    assert_eq!(context["command"], "context");
    assert_eq!(
        context["result"]["target"]["workspace"]["id"],
        workspace.id.to_string()
    );
    assert_eq!(
        context["result"]["target"]["terminal"]["id"],
        pane.terminal_id.to_string()
    );

    let mismatched = harness
        .cli()
        .env("FUT_SESSION_ID", session.id.to_string())
        .env("FUT_WORKSPACE_ID", workspace.id.to_string())
        .env("FUT_TAB_ID", tab.id.to_string())
        .env("FUT_PANE_ID", PaneId::new().to_string())
        .env("FUT_TERMINAL_ID", pane.terminal_id.to_string())
        .args(["--json", "context"])
        .output()
        .unwrap();
    assert!(mismatched.status.success());
    let mismatched: Value = serde_json::from_slice(&mismatched.stdout).unwrap();
    assert_eq!(
        mismatched["result"]["target"]["pane"]["id"],
        pane.id.to_string()
    );

    harness.shutdown().await;
}

#[test]
fn context_without_terminal_identity_is_a_typed_error_before_daemon_connection() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("FUT_RUNTIME_DIR", root.path())
        .args(["--json", "context"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "missing_context");

    let incomplete = Command::new(env!("CARGO_BIN_EXE_fut"))
        .env_clear()
        .env("FUT_RUNTIME_DIR", root.path())
        .env("FUT_TERMINAL_ID", TerminalId::new().to_string())
        .args(["--json", "context"])
        .output()
        .unwrap();
    assert!(!incomplete.status.success());
    let error: Value = serde_json::from_slice(&incomplete.stderr).unwrap();
    assert_eq!(error["error"]["code"], "command_failed");
}

#[tokio::test]
async fn process_completion_covers_live_resource_operations_and_refresh() {
    let harness = Harness::start("while IFS= read -r line; do :; done").await;
    let env = CompletionEnv::new();
    let snapshot = resources_when(&harness, |snapshot| {
        !snapshot.sessions[0].workspaces[0].tabs[0].name.is_empty()
    })
    .await;
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

    assert_eq!(
        completion_values(&env.complete(&harness.socket, &["pane", "move", ""])),
        [first_pane.to_string(), second_tab_pane.to_string()]
    );
    assert_eq!(
        completion_values(&env.complete_from(
            &harness.socket,
            &["pane", "move", ""],
            workspace.tabs[0].panes[0].terminal_id,
        )),
        [
            first_pane.to_string(),
            second_tab_pane.to_string(),
            second_tab.to_string(),
        ]
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
    assert_eq!(
        without_observations(harness.resources().await),
        without_observations(expanded.clone())
    );

    let split = env.complete(&harness.socket, &["pane", "split", ""]);
    let split_text = String::from_utf8(split.stdout).unwrap();
    assert!(split_text.lines().any(|line| line.starts_with("right")));
    assert!(split_text.lines().any(|line| line.starts_with("down")));
    assert!(
        split_text
            .lines()
            .any(|line| line.starts_with(&first_pane.to_string()))
    );

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
