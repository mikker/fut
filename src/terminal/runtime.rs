use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{broadcast, mpsc as async_mpsc, oneshot, watch};

use crate::domain::{ScreenSnapshot, TerminalId, TerminalSize};

use super::ghostty::GhosttyTerminal;

const QUEUE_CAPACITY: usize = 64;
const OUTPUT_QUEUE_CAPACITY: usize = 16;

#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub size: TerminalSize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalEvent {
    TerminalExited { exit_code: Option<i32> },
    Error { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalLifecycle {
    Running,
    Exited { exit_code: Option<i32> },
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("terminal command queue is full")]
    Busy,
    #[error("terminal runtime has stopped")]
    Stopped,
}

#[derive(Clone)]
pub struct TerminalHandle {
    id: TerminalId,
    child_pid: u32,
    spawn_cwd: PathBuf,
    commands: async_mpsc::Sender<RuntimeMessage>,
    snapshots: watch::Sender<ScreenSnapshot>,
    events: broadcast::Sender<TerminalEvent>,
    lifecycle: watch::Sender<TerminalLifecycle>,
}

impl TerminalHandle {
    #[must_use]
    pub fn id(&self) -> TerminalId {
        self.id
    }

    #[must_use]
    pub fn child_pid(&self) -> u32 {
        self.child_pid
    }

    #[must_use]
    pub fn spawn_cwd(&self) -> &std::path::Path {
        &self.spawn_cwd
    }

    pub async fn input(&self, bytes: Vec<u8>) -> Result<(), CommandError> {
        self.send(RuntimeMessage::Input(bytes))
    }

    pub async fn resize(&self, size: TerminalSize) -> Result<(), CommandError> {
        self.send(RuntimeMessage::Resize(size))
    }

    pub async fn close(&self) -> Result<(), CommandError> {
        if matches!(*self.lifecycle.borrow(), TerminalLifecycle::Exited { .. }) {
            return Ok(());
        }

        let (completion, completed) = oneshot::channel();
        let sent = tokio::time::timeout(
            Duration::from_secs(2),
            self.commands.send(RuntimeMessage::Close(completion)),
        )
        .await
        .map_err(|_| CommandError::Busy)
        .and_then(|result| result.map_err(|_| CommandError::Stopped));
        let result = match sent {
            Ok(()) => completed.await.unwrap_or(Err(CommandError::Stopped)),
            Err(error) => Err(error),
        };
        self.normalize_close_result(result)
    }

    #[must_use]
    pub fn subscribe_snapshots(&self) -> watch::Receiver<ScreenSnapshot> {
        self.snapshots.subscribe()
    }

    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<TerminalEvent> {
        self.events.subscribe()
    }

    #[must_use]
    pub fn lifecycle(&self) -> TerminalLifecycle {
        self.lifecycle.borrow().clone()
    }

    #[must_use]
    pub fn subscribe_lifecycle(&self) -> watch::Receiver<TerminalLifecycle> {
        self.lifecycle.subscribe()
    }

    fn send(&self, message: RuntimeMessage) -> Result<(), CommandError> {
        self.commands
            .try_send(message)
            .map_err(|error| match error {
                async_mpsc::error::TrySendError::Full(_) => CommandError::Busy,
                async_mpsc::error::TrySendError::Closed(_) => CommandError::Stopped,
            })
    }

    fn normalize_close_result(&self, result: Result<(), CommandError>) -> Result<(), CommandError> {
        match result {
            Err(CommandError::Stopped)
                if matches!(*self.lifecycle.borrow(), TerminalLifecycle::Exited { .. }) =>
            {
                Ok(())
            }
            result => result,
        }
    }
}

enum RuntimeMessage {
    Input(Vec<u8>),
    Resize(TerminalSize),
    Close(oneshot::Sender<Result<(), CommandError>>),
}

enum OutputMessage {
    Bytes(Vec<u8>),
    ReaderEof,
    ReaderError {
        message: String,
        raw_os_error: Option<i32>,
    },
}

struct RuntimeQueues {
    control: async_mpsc::Receiver<RuntimeMessage>,
    output: mpsc::Receiver<OutputMessage>,
}

struct RuntimePublishers<'a> {
    snapshots: &'a watch::Sender<ScreenSnapshot>,
    events: &'a broadcast::Sender<TerminalEvent>,
    lifecycle: &'a watch::Sender<TerminalLifecycle>,
}

pub fn spawn_terminal(spec: SpawnSpec) -> Result<TerminalHandle> {
    spec.size.validate()?;
    let spawn_cwd = spec.cwd.clone();
    let pair = native_pty_system().openpty(pty_size(spec.size))?;
    let mut command = CommandBuilder::new(&spec.program);
    command.args(&spec.argv);
    command.cwd(&spec.cwd);
    if !spec.env.contains_key("TERM") {
        command.env("TERM", "xterm-ghostty");
    }
    if !spec.env.contains_key("COLORTERM") {
        command.env("COLORTERM", "truecolor");
    }
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    // Acquire every fallible PTY resource and start the parser before the child
    // exists. After spawn, the runtime thread becomes the sole child owner.
    let reader = pair.master.try_clone_reader()?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
    let (commands, receiver) = async_mpsc::channel(QUEUE_CAPACITY);
    let (output, output_receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
    let (events, _) = broadcast::channel(16);
    let (lifecycle, _) = watch::channel(TerminalLifecycle::Running);
    let id = TerminalId::new();
    let initial = ScreenSnapshot::new(
        0,
        spec.size,
        vec![Default::default(); spec.size.cell_count()?],
        crate::domain::Cursor {
            column: 0,
            row: 0,
            visible: true,
        },
    )?;
    let (snapshots, _) = watch::channel(initial);
    let runtime_snapshots = snapshots.clone();
    let runtime_events = events.clone();
    let runtime_lifecycle = lifecycle.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (child_tx, child_rx) = mpsc::sync_channel(1);
    let size = spec.size;
    let runtime = thread::Builder::new()
        .name(format!("fut-terminal-{id}"))
        .spawn(move || {
            let mut terminal = match GhosttyTerminal::new(size, Arc::clone(&writer)) {
                Ok(terminal) => terminal,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            runtime_snapshots.send_replace(
                terminal
                    .snapshot()
                    .unwrap_or_else(|_| runtime_snapshots.borrow().clone()),
            );
            let _ = ready_tx.send(Ok(()));
            let Ok((child, child_pid)) = child_rx.recv() else {
                return;
            };
            run(
                RuntimeQueues {
                    control: receiver,
                    output: output_receiver,
                },
                RuntimePublishers {
                    snapshots: &runtime_snapshots,
                    events: &runtime_events,
                    lifecycle: &runtime_lifecycle,
                },
                pair.master,
                writer,
                child,
                child_pid,
                &mut terminal,
            );
        })?;

    ready_rx
        .recv()
        .context("terminal runtime stopped during startup")??;
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("spawning PTY child")?;
    let Some(child_pid) = child.process_id() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(anyhow!("PTY child has no process id"));
    };
    drop(pair.slave);
    if let Err(mpsc::SendError((mut child, _))) = child_tx.send((child, child_pid)) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = runtime.join();
        return Err(anyhow!("terminal runtime stopped during startup"));
    }

    if let Err(error) = thread::Builder::new()
        .name(format!("fut-pty-reader-{id}"))
        .spawn(move || read_pty(reader, output))
    {
        let (completion, completed) = oneshot::channel();
        let _ = commands.try_send(RuntimeMessage::Close(completion));
        let _ = completed.blocking_recv();
        let _ = runtime.join();
        return Err(error.into());
    }
    Ok(TerminalHandle {
        id,
        child_pid,
        spawn_cwd,
        commands,
        snapshots,
        events,
        lifecycle,
    })
}

fn run(
    mut queues: RuntimeQueues,
    publishers: RuntimePublishers<'_>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    child_pid: u32,
    terminal: &mut GhosttyTerminal,
) {
    let mut exit_code = None;
    let mut reader_complete = false;
    'runtime: loop {
        // Output has its own bounded queue, so PTY backpressure can never make
        // control commands Busy. Bound this drain to ensure output still moves.
        for _ in 0..32 {
            let message = match queues.control.try_recv() {
                Ok(message) => message,
                Err(async_mpsc::error::TryRecvError::Empty) => break,
                Err(async_mpsc::error::TryRecvError::Disconnected) => return,
            };
            match message {
                RuntimeMessage::Input(bytes) => {
                    if let Err(error) = writer
                        .lock()
                        .map_err(|_| anyhow!("PTY writer lock poisoned"))
                        .and_then(|mut writer| writer.write_all(&bytes).map_err(Into::into))
                    {
                        send_error(publishers.events, error);
                    }
                }
                RuntimeMessage::Resize(size) => {
                    if let Err(error) = size
                        .validate()
                        .map_err(Into::into)
                        .and_then(|()| master.resize(pty_size(size)))
                    {
                        send_error(publishers.events, error);
                    } else {
                        publish(
                            terminal.resize(size),
                            publishers.snapshots,
                            publishers.events,
                        );
                    }
                }
                RuntimeMessage::Close(completion) => {
                    kill_terminal_processes(&*master, child_pid);
                    if exit_code.is_none()
                        && let Err(error) = child.kill()
                    {
                        send_error(publishers.events, error.into());
                    }
                    // A shell may be concurrently handing the tty to a newly
                    // forked foreground group. Re-read the tty group after
                    // killing the session leader before entering wait().
                    kill_terminal_processes(&*master, child_pid);
                    match child.wait() {
                        Ok(status) => {
                            let code = Some(status.exit_code() as i32);
                            drain_output_until(
                                &queues.output,
                                terminal,
                                &publishers,
                                Duration::from_millis(100),
                            );
                            publish_exit(publishers.events, publishers.lifecycle, code);
                            let _ = completion.send(Ok(()));
                            acknowledge_pending_closes(&mut queues.control);
                            return;
                        }
                        Err(error) => {
                            send_error(publishers.events, anyhow!(error.to_string()));
                            drain_output_until(
                                &queues.output,
                                terminal,
                                &publishers,
                                Duration::from_millis(100),
                            );
                            publish_optional(
                                terminal.finish_synchronized_output(),
                                publishers.snapshots,
                                publishers.events,
                            );
                            let _ = completion.send(Err(CommandError::Stopped));
                            continue 'runtime;
                        }
                    }
                }
            }
        }
        if reader_complete {
            thread::sleep(Duration::from_millis(20));
        } else {
            match queues.output.recv_timeout(Duration::from_millis(20)) {
                Ok(OutputMessage::Bytes(bytes)) => publish_optional(
                    terminal.feed(&bytes),
                    publishers.snapshots,
                    publishers.events,
                ),
                Ok(OutputMessage::ReaderError {
                    message,
                    raw_os_error,
                }) => {
                    reader_complete = true;
                    if raw_os_error != Some(libc::EIO) {
                        let _ = publishers.events.send(TerminalEvent::Error { message });
                    }
                    publish_optional(
                        terminal.finish_synchronized_output(),
                        publishers.snapshots,
                        publishers.events,
                    );
                }
                Ok(OutputMessage::ReaderEof) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    reader_complete = true;
                    publish_optional(
                        terminal.finish_synchronized_output(),
                        publishers.snapshots,
                        publishers.events,
                    );
                }
                Err(mpsc::RecvTimeoutError::Timeout) => publish_optional(
                    terminal.flush_synchronized_output(),
                    publishers.snapshots,
                    publishers.events,
                ),
            }
        }
        if exit_code.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exit_code = Some(status.exit_code() as i32),
                Ok(None) => {}
                Err(error) => send_error(publishers.events, error.into()),
            }
        }
        if reader_complete && let Some(exit_code) = exit_code {
            publish_optional(
                terminal.finish_synchronized_output(),
                publishers.snapshots,
                publishers.events,
            );
            publish_exit(publishers.events, publishers.lifecycle, Some(exit_code));
            break;
        }
    }
}

fn drain_output_until(
    output: &mpsc::Receiver<OutputMessage>,
    terminal: &mut GhosttyTerminal,
    publishers: &RuntimePublishers<'_>,
    timeout: Duration,
) {
    let deadline = std::time::Instant::now() + timeout;
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        match output.recv_timeout(remaining) {
            Ok(OutputMessage::Bytes(bytes)) => publish_optional(
                terminal.feed(&bytes),
                publishers.snapshots,
                publishers.events,
            ),
            Ok(OutputMessage::ReaderEof) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(OutputMessage::ReaderError {
                message,
                raw_os_error,
            }) => {
                if raw_os_error != Some(libc::EIO) {
                    let _ = publishers.events.send(TerminalEvent::Error { message });
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }
    publish_optional(
        terminal.finish_synchronized_output(),
        publishers.snapshots,
        publishers.events,
    );
}

#[cfg(unix)]
fn kill_terminal_processes(master: &dyn MasterPty, child_pid: u32) {
    // A shell can put its foreground command in a different process group.
    // Kill that tty foreground group as well as the child's session group so
    // no descendant can retain the PTY and prevent confirmed reap.
    // SAFETY: getpgid/kill accept integer process ids and retain no pointers.
    unsafe {
        if let Some(foreground_group) = master.process_group_leader()
            && foreground_group != libc::getpgrp()
        {
            libc::kill(-foreground_group, libc::SIGKILL);
        }
        let process_group = libc::getpgid(child_pid as i32);
        if process_group > 0 && process_group != libc::getpgrp() {
            libc::kill(-process_group, libc::SIGKILL);
        } else {
            // Some PTY implementations do not place the command in a distinct
            // process group. Never signal Fut's own group, but still kill the
            // child itself before the confirmed wait below.
            libc::kill(child_pid as i32, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_child_pid: u32) {}

fn acknowledge_pending_closes(receiver: &mut async_mpsc::Receiver<RuntimeMessage>) {
    while let Ok(message) = receiver.try_recv() {
        if let RuntimeMessage::Close(completion) = message {
            let _ = completion.send(Ok(()));
        }
    }
}

fn read_pty(mut reader: Box<dyn Read + Send>, output: mpsc::SyncSender<OutputMessage>) {
    // Keep parser work per runtime turn bounded so control latency does not
    // depend on how much output a single PTY read happened to return.
    let mut buffer = vec![0; 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = output.send(OutputMessage::ReaderEof);
                break;
            }
            Ok(length) => {
                if output
                    .send(OutputMessage::Bytes(buffer[..length].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) => {
                let _ = output.send(OutputMessage::ReaderError {
                    message: error.to_string(),
                    raw_os_error: error.raw_os_error(),
                });
                break;
            }
        }
    }
}

fn publish(
    result: Result<ScreenSnapshot>,
    snapshots: &watch::Sender<ScreenSnapshot>,
    events: &broadcast::Sender<TerminalEvent>,
) {
    match result {
        Ok(snapshot) => {
            snapshots.send_replace(snapshot);
        }
        Err(error) => send_error(events, error),
    }
}
fn publish_optional(
    result: Result<Option<ScreenSnapshot>>,
    snapshots: &watch::Sender<ScreenSnapshot>,
    events: &broadcast::Sender<TerminalEvent>,
) {
    match result {
        Ok(Some(snapshot)) => {
            snapshots.send_replace(snapshot);
        }
        Ok(None) => {}
        Err(error) => send_error(events, error),
    }
}
fn publish_exit(
    events: &broadcast::Sender<TerminalEvent>,
    lifecycle: &watch::Sender<TerminalLifecycle>,
    exit_code: Option<i32>,
) {
    lifecycle.send_replace(TerminalLifecycle::Exited { exit_code });
    let _ = events.send(TerminalEvent::TerminalExited { exit_code });
}
fn send_error(events: &broadcast::Sender<TerminalEvent>, error: anyhow::Error) {
    let _ = events.send(TerminalEvent::Error {
        message: error.to_string(),
    });
}
fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.columns,
        pixel_width: 0,
        pixel_height: 0,
    }
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell(script: &str, env: HashMap<String, String>) -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec!["-c".into(), script.into()],
            cwd: "/".into(),
            env,
            size: TerminalSize {
                columns: 30,
                rows: 5,
            },
        }
    }

    async fn wait_for_text(receiver: &mut watch::Receiver<ScreenSnapshot>, needle: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if receiver
                    .borrow()
                    .cells
                    .iter()
                    .map(|cell| cell.contents.as_str())
                    .collect::<String>()
                    .contains(needle)
                {
                    break;
                }
                receiver.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| {
            let contents = receiver
                .borrow()
                .cells
                .iter()
                .map(|cell| cell.contents.as_str())
                .collect::<String>();
            panic!("snapshot did not contain {needle:?}: {contents:?}");
        });
    }

    #[tokio::test]
    async fn passes_explicit_args_env_and_input_then_reports_exit_once() {
        let mut env = HashMap::new();
        env.insert("FUT_TEST".into(), "works".into());
        let handle = spawn_terminal(shell(
            "printf '%s:' \"$FUT_TEST\"; IFS= read -r line; printf '%s' \"$line\"",
            env,
        ))
        .unwrap();
        let mut snapshots = handle.subscribe_snapshots();
        let mut events = handle.subscribe_events();
        handle.input(b"input\n".to_vec()).await.unwrap();
        wait_for_text(&mut snapshots, "works:input").await;
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event, TerminalEvent::TerminalExited { exit_code: Some(0) });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), events.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn resizes_pty_and_snapshot() {
        let handle = spawn_terminal(shell("sleep 2", HashMap::new())).unwrap();
        let mut snapshots = handle.subscribe_snapshots();
        handle
            .resize(TerminalSize {
                columns: 17,
                rows: 4,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while snapshots.borrow().size.columns != 17 {
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshots.borrow().cells.len(), 68);
        handle.close().await.unwrap();
    }

    #[tokio::test]
    async fn synchronized_child_output_publishes_only_complete_frames() {
        let handle = spawn_terminal(shell(
            "stty -echo; printf 'OLD_FRAME'; IFS= read -r start; printf '\\033[?2026h\\r\\033[2KNEW_PARTIAL'; IFS= read -r finish; printf '_COMPLETE\\033[?2026l'; while IFS= read -r line; do :; done",
            HashMap::new(),
        ))
        .unwrap();
        let mut snapshots = handle.subscribe_snapshots();
        wait_for_text(&mut snapshots, "OLD_FRAME").await;
        handle.input(b"start\n".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let partial = snapshots
            .borrow()
            .cells
            .iter()
            .map(|cell| cell.contents.as_str())
            .collect::<String>();
        assert!(partial.contains("OLD_FRAME"), "{partial:?}");
        assert!(!partial.contains("NEW_PARTIAL"), "{partial:?}");

        handle.input(b"release\n".to_vec()).await.unwrap();
        wait_for_text(&mut snapshots, "NEW_PARTIAL_COMPLETE").await;
        let complete = snapshots
            .borrow()
            .cells
            .iter()
            .map(|cell| cell.contents.as_str())
            .collect::<String>();
        assert!(!complete.contains("OLD_FRAME"), "{complete:?}");
        handle.close().await.unwrap();
    }

    #[tokio::test]
    async fn close_waits_for_process_death_and_is_repeatable() {
        let handle = spawn_terminal(shell("sleep 60", HashMap::new())).unwrap();
        let pid = handle.child_pid();

        tokio::time::timeout(Duration::from_secs(5), handle.close())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !std::process::Command::new("/bin/sh")
                .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
                .status()
                .unwrap()
                .success()
        );
        handle.close().await.unwrap();
    }

    #[tokio::test]
    async fn close_racing_natural_exit_is_successful() {
        for _ in 0..20 {
            let handle = spawn_terminal(shell("exit 0", HashMap::new())).unwrap();
            handle.close().await.unwrap();
            assert!(matches!(
                handle.lifecycle(),
                TerminalLifecycle::Exited { .. }
            ));
        }
    }

    #[tokio::test]
    async fn concurrent_and_repeated_closes_are_successful() {
        let handle = Arc::new(spawn_terminal(shell("sleep 60", HashMap::new())).unwrap());
        let closes = (0..8).map(|_| {
            let handle = Arc::clone(&handle);
            tokio::spawn(async move { handle.close().await })
        });
        for close in closes {
            close.await.unwrap().unwrap();
        }
        handle.close().await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_exit_is_durable_for_late_subscribers() {
        let handle = spawn_terminal(shell("exit 7", HashMap::new())).unwrap();
        let mut lifecycle = handle.subscribe_lifecycle();
        tokio::time::timeout(Duration::from_secs(5), async {
            while matches!(*lifecycle.borrow(), TerminalLifecycle::Running) {
                lifecycle.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        assert_eq!(
            *handle.subscribe_lifecycle().borrow(),
            TerminalLifecycle::Exited { exit_code: Some(7) }
        );
        assert_eq!(
            handle.lifecycle(),
            TerminalLifecycle::Exited { exit_code: Some(7) }
        );
    }

    #[tokio::test]
    async fn lifecycle_exit_follows_all_queued_pty_output() {
        let handle = spawn_terminal(shell(
            "head -c 8192 /dev/zero | tr '\\000' x; printf '\\r\\nFUT_FINAL_MARKER'",
            HashMap::new(),
        ))
        .unwrap();
        let mut lifecycle = handle.subscribe_lifecycle();
        tokio::time::timeout(Duration::from_secs(5), async {
            while matches!(*lifecycle.borrow(), TerminalLifecycle::Running) {
                lifecycle.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let snapshot = handle.subscribe_snapshots().borrow().clone();
        let contents = snapshot
            .cells
            .iter()
            .map(|cell| cell.contents.as_str())
            .collect::<String>();
        assert!(contents.contains("FUT_FINAL_MARKER"), "{contents:?}");
    }

    #[tokio::test]
    async fn exit_forces_an_unclosed_synchronized_frame() {
        let handle = spawn_terminal(shell(
            "printf '\\033[?2026hFINAL_SYNC_FRAME'",
            HashMap::new(),
        ))
        .unwrap();
        let mut lifecycle = handle.subscribe_lifecycle();
        tokio::time::timeout(Duration::from_secs(5), async {
            while matches!(*lifecycle.borrow(), TerminalLifecycle::Running) {
                lifecycle.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let contents = handle
            .subscribe_snapshots()
            .borrow()
            .cells
            .iter()
            .map(|cell| cell.contents.as_str())
            .collect::<String>();
        assert!(contents.contains("FINAL_SYNC_FRAME"), "{contents:?}");
    }

    #[tokio::test]
    async fn reader_eof_finishes_sync_and_keeps_control_responsive() {
        let handle = spawn_terminal(shell(
            "printf '\\033[?2026hEOF_SYNC_FRAME'; exec 0<&- 1>&- 2>&-; sleep 60",
            HashMap::new(),
        ))
        .unwrap();
        let mut snapshots = handle.subscribe_snapshots();
        wait_for_text(&mut snapshots, "EOF_SYNC_FRAME").await;

        tokio::time::timeout(Duration::from_secs(5), handle.close())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn sustained_output_cannot_fill_the_control_queue() {
        let handle = spawn_terminal(shell(
            "i=0; while [ $i -lt 100 ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; i=$((i+1)); done; while IFS= read -r line; do :; done",
            HashMap::new(),
        ))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(
            handle
                .resize(TerminalSize {
                    columns: 20,
                    rows: 4,
                })
                .await
                .is_ok()
        );
        let close = tokio::time::timeout(Duration::from_secs(15), handle.close())
            .await
            .unwrap();
        assert!(!matches!(close, Err(CommandError::Busy)));
    }
}
