use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel as channel;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{broadcast, mpsc as async_mpsc, oneshot, watch};

use crate::domain::{
    ClientId, CopyModeAction, CopyModeError, MouseEvent, MouseEventKind, ScreenSnapshot,
    TerminalId, TerminalSize,
};

use super::{
    CopyModeOutcome, MouseInputOutcome, ViewportSnapshot,
    ghostty::{CopyModeFailure, GhosttyTerminal},
};

const QUEUE_CAPACITY: usize = 64;
const OUTPUT_QUEUE_CAPACITY: usize = 16;
const DROP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
// Bound how much PTY output one drain pass parses before it snapshots, so a
// very chatty PTY still lets control messages (keystrokes) interleave promptly.
const OUTPUT_DRAIN_BYTE_BUDGET: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub id: TerminalId,
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
    #[error("terminal emulator operation failed: {0}")]
    Emulator(String),
    #[error(transparent)]
    CopyMode(#[from] CopyModeError),
}

#[derive(Clone)]
pub struct TerminalHandle {
    id: TerminalId,
    child_pid: u32,
    spawn_cwd: PathBuf,
    commands: RuntimeCommands,
    snapshots: watch::Sender<ScreenSnapshot>,
    events: broadcast::Sender<TerminalEvent>,
    lifecycle: watch::Sender<TerminalLifecycle>,
}

/// Command sender paired with the runtime thread's doorbell. The runtime
/// parks on PTY output between commands; ringing after every enqueue wakes
/// it immediately instead of on its next 20ms output poll, which is the
/// difference between wheel input applying instantly and piling up.
#[derive(Clone)]
struct RuntimeCommands {
    channel: async_mpsc::Sender<RuntimeMessage>,
    doorbell: channel::Sender<()>,
}

impl RuntimeCommands {
    async fn send(&self, message: RuntimeMessage) -> Result<(), CommandError> {
        self.channel
            .send(message)
            .await
            .map_err(|_| CommandError::Stopped)?;
        self.ring();
        Ok(())
    }

    fn try_send(
        &self,
        message: RuntimeMessage,
    ) -> Result<(), async_mpsc::error::TrySendError<RuntimeMessage>> {
        self.channel.try_send(message)?;
        self.ring();
        Ok(())
    }

    fn ring(&self) {
        // A full doorbell already has a pending wake; dropping this ring is fine.
        let _ = self.doorbell.try_send(());
    }
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
        self.commands.send(RuntimeMessage::Input(bytes)).await
    }

    pub async fn paste(&self, text: String) -> Result<(), CommandError> {
        send_paste_with_backpressure(&self.commands, text).await
    }

    pub async fn resize(&self, size: TerminalSize) -> Result<(), CommandError> {
        self.send(RuntimeMessage::Resize(size))
    }

    pub(crate) async fn mouse_input(
        &self,
        event: MouseEvent,
        viewport_offset: Option<usize>,
        pty_input_allowed: bool,
    ) -> Result<MouseInputOutcome, CommandError> {
        send_mouse_input(&self.commands, event, viewport_offset, pty_input_allowed).await
    }

    pub(crate) async fn viewport_snapshot(
        &self,
        viewport_offset: Option<usize>,
    ) -> Result<ViewportSnapshot, CommandError> {
        let (completion, completed) = oneshot::channel();
        self.send(RuntimeMessage::ViewportSnapshot {
            viewport_offset,
            completion,
        })?;
        completed.await.unwrap_or(Err(CommandError::Stopped))
    }

    pub(crate) async fn copy_mode(
        &self,
        owner: ClientId,
        action: CopyModeAction,
        viewport_offset: Option<usize>,
    ) -> Result<CopyModeOutcome, CommandError> {
        let (completion, completed) = oneshot::channel();
        self.commands
            .send(RuntimeMessage::CopyMode {
                owner,
                action,
                viewport_offset,
                completion,
            })
            .await?;
        completed.await.unwrap_or(Err(CommandError::Stopped))
    }

    pub(crate) async fn copy_mode_snapshot(
        &self,
        owner: ClientId,
        viewport_offset: Option<usize>,
    ) -> Result<ViewportSnapshot, CommandError> {
        let (completion, completed) = oneshot::channel();
        self.commands
            .send(RuntimeMessage::CopyModeSnapshot {
                owner,
                viewport_offset,
                completion,
            })
            .await?;
        completed.await.unwrap_or(Err(CommandError::Stopped))
    }

    pub(crate) async fn clear_client(
        &self,
        owner: ClientId,
    ) -> Result<Option<ScreenSnapshot>, CommandError> {
        let (completion, completed) = oneshot::channel();
        let sent = self
            .commands
            .send(RuntimeMessage::ClearCopyMode { owner, completion })
            .await;
        if matches!(sent, Err(CommandError::Stopped))
            && matches!(*self.lifecycle.borrow(), TerminalLifecycle::Exited { .. })
        {
            return Ok(None);
        }
        sent?;
        match completed.await.unwrap_or(Err(CommandError::Stopped)) {
            Err(CommandError::Stopped)
                if matches!(*self.lifecycle.borrow(), TerminalLifecycle::Exited { .. }) =>
            {
                Ok(None)
            }
            result => result,
        }
    }

    /// Last-resort cleanup for an unexpectedly dropped attachment. Normal
    /// lifecycle paths await [`Self::clear_client`] before dropping ownership.
    /// This fallback retries the bounded ordered queue and waits for runtime
    /// acknowledgement only until one deadline on a short-lived helper thread.
    pub(crate) fn clear_client_on_drop(&self, owner: ClientId) {
        let commands = self.commands.clone();
        let terminal_id = self.id;
        let deadline = Instant::now() + DROP_CLEANUP_TIMEOUT;
        let _ = thread::Builder::new()
            .name(format!("fut-copy-cleanup-{terminal_id}"))
            .spawn(move || clear_client_before_deadline(&commands, owner, deadline));
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
        .unwrap_or(Err(CommandError::Busy));
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

fn clear_client_before_deadline(commands: &RuntimeCommands, owner: ClientId, deadline: Instant) {
    let (completion, mut completed) = oneshot::channel();
    let mut message = RuntimeMessage::ClearCopyMode { owner, completion };
    loop {
        match commands.try_send(message) {
            Ok(()) => break,
            Err(async_mpsc::error::TrySendError::Full(returned)) => message = returned,
            Err(async_mpsc::error::TrySendError::Closed(_)) => return,
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }

    loop {
        match completed.try_recv() {
            Ok(_) | Err(oneshot::error::TryRecvError::Closed) => return,
            Err(oneshot::error::TryRecvError::Empty) => {}
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

async fn send_paste_with_backpressure(
    commands: &RuntimeCommands,
    text: String,
) -> Result<(), CommandError> {
    let (completion, completed) = oneshot::channel();
    commands
        .send(RuntimeMessage::Paste { text, completion })
        .await?;
    completed.await.unwrap_or(Err(CommandError::Stopped))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MouseSendPolicy {
    Lossless,
    Disposable,
}

fn mouse_send_policy(kind: MouseEventKind) -> MouseSendPolicy {
    match kind {
        MouseEventKind::Press { .. } | MouseEventKind::Release { .. } => MouseSendPolicy::Lossless,
        MouseEventKind::Motion { .. } | MouseEventKind::Wheel { .. } => MouseSendPolicy::Disposable,
    }
}

async fn send_mouse_input(
    commands: &RuntimeCommands,
    event: MouseEvent,
    viewport_offset: Option<usize>,
    pty_input_allowed: bool,
) -> Result<MouseInputOutcome, CommandError> {
    let policy = mouse_send_policy(event.kind);
    let (completion, completed) = oneshot::channel();
    let message = RuntimeMessage::MouseInput {
        event,
        viewport_offset,
        pty_input_allowed,
        completion,
    };
    match policy {
        MouseSendPolicy::Lossless => commands.send(message).await?,
        MouseSendPolicy::Disposable => commands.try_send(message).map_err(|error| match error {
            async_mpsc::error::TrySendError::Full(_) => CommandError::Busy,
            async_mpsc::error::TrySendError::Closed(_) => CommandError::Stopped,
        })?,
    }
    completed.await.unwrap_or(Err(CommandError::Stopped))
}

enum RuntimeMessage {
    Input(Vec<u8>),
    Paste {
        text: String,
        completion: oneshot::Sender<Result<(), CommandError>>,
    },
    Resize(TerminalSize),
    MouseInput {
        event: MouseEvent,
        viewport_offset: Option<usize>,
        pty_input_allowed: bool,
        completion: oneshot::Sender<Result<MouseInputOutcome, CommandError>>,
    },
    ViewportSnapshot {
        viewport_offset: Option<usize>,
        completion: oneshot::Sender<Result<ViewportSnapshot, CommandError>>,
    },
    CopyMode {
        owner: ClientId,
        action: CopyModeAction,
        viewport_offset: Option<usize>,
        completion: oneshot::Sender<Result<CopyModeOutcome, CommandError>>,
    },
    CopyModeSnapshot {
        owner: ClientId,
        viewport_offset: Option<usize>,
        completion: oneshot::Sender<Result<ViewportSnapshot, CommandError>>,
    },
    ClearCopyMode {
        owner: ClientId,
        completion: oneshot::Sender<Result<Option<ScreenSnapshot>, CommandError>>,
    },
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

struct OutputProducer {
    sender: channel::Sender<OutputMessage>,
    produced: Arc<AtomicU64>,
}

impl OutputProducer {
    fn send(&self, message: OutputMessage) -> Result<(), channel::SendError<OutputMessage>> {
        // Publish the sequence before the bounded send can block. An acquire
        // snapshot can therefore include this message even while it is still
        // waiting for the runtime to free a queue slot.
        self.produced
            .fetch_update(Ordering::Release, Ordering::Relaxed, |produced| {
                produced.checked_add(1)
            })
            .expect("PTY output sequence overflow");
        self.sender.send(message)
    }
}

struct OutputQueue {
    receiver: channel::Receiver<OutputMessage>,
    produced: Arc<AtomicU64>,
    consumed: u64,
}

impl OutputQueue {
    fn barrier_target(&self) -> u64 {
        self.produced.load(Ordering::Acquire)
    }

    fn record_consumed(&mut self) {
        self.consumed = self
            .consumed
            .checked_add(1)
            .expect("PTY output consumption sequence overflow");
    }
}

struct RuntimeQueues {
    control: async_mpsc::Receiver<RuntimeMessage>,
    doorbell: channel::Receiver<()>,
    output: OutputQueue,
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
    let (doorbell_sender, doorbell) = channel::bounded(1);
    let commands = RuntimeCommands {
        channel: commands,
        doorbell: doorbell_sender,
    };
    let (output, output_queue) = output_queue();
    let (events, _) = broadcast::channel(16);
    let (lifecycle, _) = watch::channel(TerminalLifecycle::Running);
    let id = spec.id;
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
                    doorbell,
                    output: output_queue,
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
                RuntimeMessage::Paste { text, completion } => {
                    let result = paste_after_output_barrier(
                        &mut queues.output,
                        terminal,
                        &publishers,
                        &mut reader_complete,
                        text,
                    );
                    let _ = completion.send(result);
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
                RuntimeMessage::MouseInput {
                    event,
                    viewport_offset,
                    pty_input_allowed,
                    completion,
                } => {
                    let result = mouse_input_after_output_barrier(
                        &mut queues.output,
                        terminal,
                        &publishers,
                        &mut reader_complete,
                        event,
                        viewport_offset,
                        pty_input_allowed,
                    );
                    let _ = completion.send(result);
                }
                RuntimeMessage::ViewportSnapshot {
                    viewport_offset,
                    completion,
                } => {
                    let result = terminal
                        .viewport_snapshot(viewport_offset)
                        .map_err(|error| CommandError::Emulator(error.to_string()));
                    let _ = completion.send(result);
                }
                RuntimeMessage::CopyMode {
                    owner,
                    action,
                    viewport_offset,
                    completion,
                } => {
                    drain_output_barrier(
                        &mut queues.output,
                        terminal,
                        &publishers,
                        &mut reader_complete,
                    );
                    let result = copy_mode_result(
                        terminal.copy_mode(owner, action, viewport_offset),
                        &publishers,
                    );
                    publish_copy_exit(&result, publishers.snapshots);
                    let _ = completion.send(result);
                }
                RuntimeMessage::CopyModeSnapshot {
                    owner,
                    viewport_offset,
                    completion,
                } => {
                    let result = copy_mode_result(
                        terminal.copy_mode_snapshot(owner, viewport_offset),
                        &publishers,
                    );
                    let _ = completion.send(result);
                }
                RuntimeMessage::ClearCopyMode { owner, completion } => {
                    drain_output_barrier(
                        &mut queues.output,
                        terminal,
                        &publishers,
                        &mut reader_complete,
                    );
                    let result = terminal
                        .clear_copy_mode(owner)
                        .map_err(|error| CommandError::Emulator(error.to_string()));
                    if let Ok(Some(screen)) = &result {
                        publishers.snapshots.send_replace(screen.clone());
                    }
                    let _ = completion.send(result);
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
                                &mut queues.output,
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
                                &mut queues.output,
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
        // Park until PTY output arrives, a command rings the doorbell, or the
        // synchronized-output flush interval elapses. Commands must never wait
        // out the full timeout: interactive latency depends on waking now.
        if reader_complete {
            let _ = queues.doorbell.recv_timeout(Duration::from_millis(20));
        } else {
            channel::select! {
                recv(queues.output.receiver) -> message => match message {
                    Ok(message) => {
                        reader_complete = drain_output_batch(
                            &mut queues.output,
                            message,
                            terminal,
                            &publishers,
                        );
                    }
                    Err(channel::RecvError) => {
                        reader_complete = true;
                        publish_optional(
                            terminal.finish_synchronized_output(),
                            publishers.snapshots,
                            publishers.events,
                        );
                    }
                },
                recv(queues.doorbell) -> _ => {}
                default(Duration::from_millis(20)) => publish_optional(
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

fn paste_after_output_barrier(
    output: &mut OutputQueue,
    terminal: &mut GhosttyTerminal,
    publishers: &RuntimePublishers<'_>,
    reader_complete: &mut bool,
    text: String,
) -> Result<(), CommandError> {
    drain_output_barrier(output, terminal, publishers, reader_complete);
    terminal
        .paste(text)
        .map_err(|error| CommandError::Emulator(error.to_string()))
}

fn mouse_input_after_output_barrier(
    output: &mut OutputQueue,
    terminal: &mut GhosttyTerminal,
    publishers: &RuntimePublishers<'_>,
    reader_complete: &mut bool,
    event: MouseEvent,
    viewport_offset: Option<usize>,
    pty_input_allowed: bool,
) -> Result<MouseInputOutcome, CommandError> {
    drain_output_barrier(output, terminal, publishers, reader_complete);
    terminal
        .mouse_input(event, viewport_offset, pty_input_allowed)
        .map_err(|error| CommandError::Emulator(error.to_string()))
}

fn drain_output_barrier(
    output: &mut OutputQueue,
    terminal: &mut GhosttyTerminal,
    publishers: &RuntimePublishers<'_>,
    reader_complete: &mut bool,
) {
    if !*reader_complete {
        let target = output.barrier_target();
        while output.consumed < target {
            match output.receiver.recv() {
                Ok(message) => {
                    if consume_output_message(output, message, terminal, publishers) {
                        *reader_complete = true;
                    }
                }
                Err(_) => {
                    // A failed producer send is only observable here as a
                    // disconnected queue. Stop waiting rather than hanging on
                    // a sequence for which no message can arrive.
                    *reader_complete = true;
                    publish_optional(
                        terminal.finish_synchronized_output(),
                        publishers.snapshots,
                        publishers.events,
                    );
                    break;
                }
            }
        }
    }
}

fn copy_mode_result<T>(
    result: std::result::Result<T, CopyModeFailure>,
    publishers: &RuntimePublishers<'_>,
) -> std::result::Result<T, CommandError> {
    result.map_err(|error| match error {
        CopyModeFailure::Semantic(error) => CommandError::CopyMode(error),
        CopyModeFailure::CursorLost {
            canonical,
            cleanup_error,
        } => {
            if let Some(canonical) = canonical {
                publishers.snapshots.send_replace(canonical);
            }
            if let Some(cleanup_error) = cleanup_error {
                send_error(publishers.events, cleanup_error);
            }
            CommandError::CopyMode(CopyModeError::CursorLost)
        }
        CopyModeFailure::Emulator(error) => CommandError::Emulator(error.to_string()),
    })
}

fn publish_copy_exit(
    result: &std::result::Result<CopyModeOutcome, CommandError>,
    snapshots: &watch::Sender<ScreenSnapshot>,
) {
    if let Ok(CopyModeOutcome::Finalized { screen } | CopyModeOutcome::Cancelled { screen }) =
        result
    {
        snapshots.send_replace(screen.clone());
    }
}

/// Parse every `Bytes` message already sitting in the output queue behind
/// `first` and build a single snapshot at the end, instead of snapshotting
/// after every PTY read. A non-`Bytes` message ends the drain immediately
/// and is handled exactly as `consume_output_message` would handle it on its
/// own, after any pending bytes are snapshotted so its state is up to date.
fn drain_output_batch(
    output: &mut OutputQueue,
    first: OutputMessage,
    terminal: &mut GhosttyTerminal,
    publishers: &RuntimePublishers<'_>,
) -> bool {
    let mut budget = OUTPUT_DRAIN_BYTE_BUDGET;
    let mut fed_any = false;
    let mut pending = Some(first);
    loop {
        let message = match pending.take() {
            Some(message) => message,
            None => match output.receiver.try_recv() {
                Ok(message) => message,
                Err(_) => break,
            },
        };
        let OutputMessage::Bytes(bytes) = message else {
            if fed_any {
                publish_optional(
                    terminal.snapshot_after_feed(),
                    publishers.snapshots,
                    publishers.events,
                );
            }
            return consume_output_message(output, message, terminal, publishers);
        };
        fed_any = true;
        budget = budget.saturating_sub(bytes.len());
        terminal.vt_write(&bytes);
        output.record_consumed();
        if budget == 0 {
            break;
        }
    }
    if fed_any {
        publish_optional(
            terminal.snapshot_after_feed(),
            publishers.snapshots,
            publishers.events,
        );
    }
    false
}

fn consume_output_message(
    output: &mut OutputQueue,
    message: OutputMessage,
    terminal: &mut GhosttyTerminal,
    publishers: &RuntimePublishers<'_>,
) -> bool {
    let reader_complete = process_output_message(message, terminal, publishers);
    output.record_consumed();
    reader_complete
}

fn process_output_message(
    message: OutputMessage,
    terminal: &mut GhosttyTerminal,
    publishers: &RuntimePublishers<'_>,
) -> bool {
    match message {
        OutputMessage::Bytes(bytes) => {
            publish_optional(
                terminal.feed(&bytes),
                publishers.snapshots,
                publishers.events,
            );
            false
        }
        OutputMessage::ReaderError {
            message,
            raw_os_error,
        } => {
            if raw_os_error != Some(libc::EIO) {
                let _ = publishers.events.send(TerminalEvent::Error { message });
            }
            publish_optional(
                terminal.finish_synchronized_output(),
                publishers.snapshots,
                publishers.events,
            );
            true
        }
        OutputMessage::ReaderEof => {
            publish_optional(
                terminal.finish_synchronized_output(),
                publishers.snapshots,
                publishers.events,
            );
            true
        }
    }
}

fn drain_output_until(
    output: &mut OutputQueue,
    terminal: &mut GhosttyTerminal,
    publishers: &RuntimePublishers<'_>,
    timeout: Duration,
) {
    let deadline = std::time::Instant::now() + timeout;
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        match output.receiver.recv_timeout(remaining) {
            Ok(message) => {
                if consume_output_message(output, message, terminal, publishers) {
                    break;
                }
            }
            Err(channel::RecvTimeoutError::Disconnected | channel::RecvTimeoutError::Timeout) => {
                break;
            }
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

fn output_queue() -> (OutputProducer, OutputQueue) {
    let (sender, receiver) = channel::bounded(OUTPUT_QUEUE_CAPACITY);
    let produced = Arc::new(AtomicU64::new(0));
    (
        OutputProducer {
            sender,
            produced: Arc::clone(&produced),
        },
        OutputQueue {
            receiver,
            produced,
            consumed: 0,
        },
    )
}

fn read_pty(mut reader: Box<dyn Read + Send>, output: OutputProducer) {
    // The VT thread now drains and batches everything already queued before
    // it snapshots (see `drain_output_batch`), so a large read buffer here
    // just means fewer, bigger chunks to hand off rather than more parser
    // turns — control latency is bounded by the drain's byte budget instead.
    let mut buffer = vec![0; 64 * 1024];
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

    fn test_commands(sender: async_mpsc::Sender<RuntimeMessage>) -> RuntimeCommands {
        RuntimeCommands {
            channel: sender,
            doorbell: channel::bounded(1).0,
        }
    }

    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test PTY write failure",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn test_terminal(writer: Box<dyn Write + Send>) -> GhosttyTerminal {
        GhosttyTerminal::new(
            TerminalSize {
                columns: 30,
                rows: 5,
            },
            Arc::new(Mutex::new(writer)),
        )
        .unwrap()
    }

    fn shell(script: &str, env: HashMap<String, String>) -> SpawnSpec {
        SpawnSpec {
            id: TerminalId::new(),
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

    async fn assert_runtime_paste(bracketed: bool, expected: &[u8]) {
        let temporary = tempfile::tempdir().unwrap();
        let capture = temporary.path().join("paste.bin");
        let enable = if bracketed { "\\033[?2004h" } else { "" };
        let script = format!(
            "stty raw -echo; printf '{enable}PASTE_READY\\r\\n'; dd bs=1 count={} of='{}' 2>/dev/null",
            expected.len(),
            capture.display()
        );
        let handle = spawn_terminal(shell(&script, HashMap::new())).unwrap();
        let mut snapshots = handle.subscribe_snapshots();
        wait_for_text(&mut snapshots, "PASTE_READY").await;

        handle
            .paste("héllo 雪\nnext\0\x1b[201~".into())
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read(&capture).is_ok_and(|bytes| bytes.len() == expected.len()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(capture).unwrap(), expected);
    }

    #[tokio::test]
    async fn runtime_paste_uses_current_child_terminal_mode() {
        assert_runtime_paste(false, b"h\xc3\xa9llo \xe9\x9b\xaa\rnext  [201~").await;
        assert_runtime_paste(
            true,
            b"\x1b[200~h\xc3\xa9llo \xe9\x9b\xaa\nnext  [201~\x1b[201~",
        )
        .await;
    }

    #[test]
    fn queued_mode_transitions_are_processed_before_each_paste() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut terminal = test_terminal(Box::new(RecordingWriter(Arc::clone(&captured))));
        let initial = terminal.snapshot().unwrap();
        let (snapshots, _) = watch::channel(initial);
        let (events, mut event_receiver) = broadcast::channel(4);
        let (lifecycle, _) = watch::channel(TerminalLifecycle::Running);
        let publishers = RuntimePublishers {
            snapshots: &snapshots,
            events: &events,
            lifecycle: &lifecycle,
        };
        let (output, mut queued_output) = output_queue();
        let mut reader_complete = false;

        output
            .send(OutputMessage::Bytes(b"\x1b[?2004h".to_vec()))
            .unwrap();
        paste_after_output_barrier(
            &mut queued_output,
            &mut terminal,
            &publishers,
            &mut reader_complete,
            "enabled\n".into(),
        )
        .unwrap();

        output
            .send(OutputMessage::Bytes(b"\x1b[?2004l".to_vec()))
            .unwrap();
        paste_after_output_barrier(
            &mut queued_output,
            &mut terminal,
            &publishers,
            &mut reader_complete,
            "disabled\n".into(),
        )
        .unwrap();

        assert_eq!(
            *captured.lock().unwrap(),
            b"\x1b[200~enabled\n\x1b[201~disabled\r".to_vec()
        );
        assert!(!reader_complete);
        assert!(matches!(
            event_receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn saturated_output_barrier_makes_mouse_tracking_mode_authoritative() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut terminal = test_terminal(Box::new(RecordingWriter(Arc::clone(&captured))));
        let initial = terminal.snapshot().unwrap();
        let (snapshots, _) = watch::channel(initial);
        let (events, _) = broadcast::channel(4);
        let (lifecycle, _) = watch::channel(TerminalLifecycle::Running);
        let publishers = RuntimePublishers {
            snapshots: &snapshots,
            events: &events,
            lifecycle: &lifecycle,
        };
        let (output, mut queued_output) = output_queue();
        let mut reader_complete = false;

        for _ in 0..OUTPUT_QUEUE_CAPACITY {
            output.send(OutputMessage::Bytes(b"x".to_vec())).unwrap();
        }
        let producer = thread::spawn(move || {
            output
                .send(OutputMessage::Bytes(
                    b"\x1b[?1049h\x1b[?1007h\x1b[?1h\x1b[?1000h\x1b[?1006h".to_vec(),
                ))
                .unwrap();
            output
                .send(OutputMessage::Bytes(b"\x1b[?1000l".to_vec()))
                .unwrap();
            output
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while queued_output.barrier_target() != 17 {
            assert!(
                Instant::now() < deadline,
                "tracking transition never reached the saturated output barrier"
            );
            thread::yield_now();
        }

        let event = MouseEvent {
            kind: MouseEventKind::Press {
                button: crate::domain::MouseButton::Left,
            },
            column: 2,
            row: 1,
            modifiers: Default::default(),
            buttons: crate::domain::MouseButtons {
                left: true,
                ..Default::default()
            },
        };
        assert!(matches!(
            mouse_input_after_output_barrier(
                &mut queued_output,
                &mut terminal,
                &publishers,
                &mut reader_complete,
                event,
                None,
                true,
            )
            .unwrap(),
            MouseInputOutcome::Handled
        ));
        let output = producer.join().unwrap();
        assert_eq!(queued_output.consumed, 17);
        assert_eq!(queued_output.barrier_target(), 18);
        assert_eq!(*captured.lock().unwrap(), b"\x1b[<0;3;2M".to_vec());

        let wheel = MouseEvent {
            kind: MouseEventKind::Wheel {
                direction: crate::domain::MouseWheelDirection::Up,
            },
            buttons: Default::default(),
            ..event
        };
        assert!(matches!(
            mouse_input_after_output_barrier(
                &mut queued_output,
                &mut terminal,
                &publishers,
                &mut reader_complete,
                wheel,
                None,
                true,
            )
            .unwrap(),
            MouseInputOutcome::Handled
        ));
        assert_eq!(queued_output.consumed, 18);
        assert_eq!(
            *captured.lock().unwrap(),
            b"\x1b[<0;3;2M\x1bOA\x1bOA\x1bOA".to_vec()
        );

        output
            .send(OutputMessage::Bytes(b"\x1b[?1007l".to_vec()))
            .unwrap();
        // With alternate scroll disabled the wheel is consumed locally; at
        // the top of an empty history that means dropping it outright.
        assert!(matches!(
            mouse_input_after_output_barrier(
                &mut queued_output,
                &mut terminal,
                &publishers,
                &mut reader_complete,
                wheel,
                None,
                true,
            )
            .unwrap(),
            MouseInputOutcome::Handled
        ));
        assert_eq!(queued_output.consumed, 19);
        assert_eq!(
            *captured.lock().unwrap(),
            b"\x1b[<0;3;2M\x1bOA\x1bOA\x1bOA".to_vec()
        );
        assert!(!reader_complete);
    }

    #[test]
    fn paste_barrier_includes_blocked_seventeenth_output_and_stops_at_its_target() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut terminal = test_terminal(Box::new(RecordingWriter(Arc::clone(&captured))));
        let initial = terminal.snapshot().unwrap();
        let (snapshots, _) = watch::channel(initial);
        let (events, _) = broadcast::channel(4);
        let (lifecycle, _) = watch::channel(TerminalLifecycle::Running);
        let publishers = RuntimePublishers {
            snapshots: &snapshots,
            events: &events,
            lifecycle: &lifecycle,
        };
        let (output, mut queued_output) = output_queue();
        let mut reader_complete = false;

        for _ in 0..OUTPUT_QUEUE_CAPACITY {
            output.send(OutputMessage::Bytes(b"x".to_vec())).unwrap();
        }

        let producer = thread::spawn(move || {
            output
                .send(OutputMessage::Bytes(b"\x1b[?2004h".to_vec()))
                .unwrap();
            output
                .send(OutputMessage::Bytes(b"\x1b[?2004l".to_vec()))
                .unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while queued_output.barrier_target() != 17 {
            assert!(
                std::time::Instant::now() < deadline,
                "seventeenth output was not produced"
            );
            thread::yield_now();
        }

        paste_after_output_barrier(
            &mut queued_output,
            &mut terminal,
            &publishers,
            &mut reader_complete,
            "barrier\n".into(),
        )
        .unwrap();
        producer.join().unwrap();

        assert_eq!(queued_output.consumed, 17);
        assert_eq!(queued_output.barrier_target(), 18);
        assert_eq!(
            *captured.lock().unwrap(),
            b"\x1b[200~barrier\n\x1b[201~".to_vec()
        );

        paste_after_output_barrier(
            &mut queued_output,
            &mut terminal,
            &publishers,
            &mut reader_complete,
            "after\n".into(),
        )
        .unwrap();
        assert_eq!(queued_output.consumed, 18);
        assert_eq!(
            *captured.lock().unwrap(),
            b"\x1b[200~barrier\n\x1b[201~after\r".to_vec()
        );
        assert!(!reader_complete);
    }

    #[test]
    fn paste_write_failure_is_correlated_without_an_unsolicited_runtime_error() {
        let mut terminal = test_terminal(Box::new(FailingWriter));
        let initial = terminal.snapshot().unwrap();
        let (snapshots, _) = watch::channel(initial);
        let (events, mut event_receiver) = broadcast::channel(4);
        let (lifecycle, _) = watch::channel(TerminalLifecycle::Running);
        let publishers = RuntimePublishers {
            snapshots: &snapshots,
            events: &events,
            lifecycle: &lifecycle,
        };
        let (_output, mut queued_output) = output_queue();
        let mut reader_complete = false;

        let error = paste_after_output_barrier(
            &mut queued_output,
            &mut terminal,
            &publishers,
            &mut reader_complete,
            "paste".into(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CommandError::Emulator(message) if message.contains("writing encoded paste to PTY")
        ));
        assert!(matches!(
            event_receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
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
    async fn acknowledged_copy_cleanup_is_ordered_before_same_owner_can_begin_again() {
        let handle = spawn_terminal(shell("printf content; sleep 60", HashMap::new())).unwrap();
        let owner = ClientId::new();
        let CopyModeOutcome::Active(selected) = handle
            .copy_mode(owner, CopyModeAction::Begin, None)
            .await
            .unwrap()
        else {
            panic!("copy mode did not begin")
        };
        let canonical = handle
            .clear_client(owner)
            .await
            .unwrap()
            .expect("active copy mode returns a canonical snapshot");
        assert!(canonical.revision > selected.screen.revision);
        assert!(canonical.cells.iter().all(|cell| !cell.selected));

        assert!(matches!(
            handle
                .copy_mode(owner, CopyModeAction::Begin, None)
                .await
                .unwrap(),
            CopyModeOutcome::Active(_)
        ));
        handle.clear_client(owner).await.unwrap();
        handle.close().await.unwrap();
    }

    #[tokio::test]
    async fn queued_wheel_input_is_applied_promptly() {
        let handle = spawn_terminal(shell(
            "i=0; while [ $i -le 60 ]; do echo \"HIST_$i\"; i=$((i+1)); done; echo READY; while IFS= read -r line; do :; done",
            HashMap::new(),
        ))
        .unwrap();
        let mut snapshots = handle.subscribe_snapshots();
        wait_for_text(&mut snapshots, "READY").await;

        // A wheel round-trip must not wait out the runtime's 20ms output
        // poll: trackpad flings queue hundreds of events, and per-event poll
        // latency turns a burst into seconds of lag. 100 round-trips at poll
        // latency would take ~2s; woken promptly they take milliseconds.
        let started = Instant::now();
        let mut offset = None;
        for _ in 0..100 {
            let outcome = handle
                .mouse_input(
                    MouseEvent {
                        kind: MouseEventKind::Wheel {
                            direction: crate::domain::MouseWheelDirection::Up,
                        },
                        column: 0,
                        row: 0,
                        modifiers: crate::domain::MouseModifiers::default(),
                        buttons: crate::domain::MouseButtons::default(),
                    },
                    offset,
                    true,
                )
                .await
                .unwrap();
            if let MouseInputOutcome::Scrolled(viewport)
            | MouseInputOutcome::ReturnedToBottom(viewport) = outcome
            {
                offset = viewport.offset;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "100 wheel round-trips took {:?}; runtime wake-up latency regressed",
            started.elapsed()
        );
        handle.close().await.unwrap();
    }

    #[test]
    fn drop_cleanup_is_bounded_when_the_command_queue_is_saturated_or_stalled() {
        for saturated in [true, false] {
            let (commands, _stalled_receiver) = async_mpsc::channel(1);
            let commands = test_commands(commands);
            if saturated {
                assert!(commands.try_send(RuntimeMessage::Input(Vec::new())).is_ok());
            }
            let started = Instant::now();
            clear_client_before_deadline(
                &commands,
                ClientId::new(),
                started + Duration::from_millis(40),
            );
            assert!(
                started.elapsed() < Duration::from_millis(250),
                "drop cleanup outlived its deadline with saturated={saturated}"
            );
        }
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
    async fn copy_cleanup_is_idempotent_after_the_runtime_has_exited() {
        let handle = spawn_terminal(shell("exit 0", HashMap::new())).unwrap();
        let mut lifecycle = handle.subscribe_lifecycle();
        tokio::time::timeout(Duration::from_secs(5), async {
            while matches!(*lifecycle.borrow(), TerminalLifecycle::Running) {
                lifecycle.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(handle.clear_client(ClientId::new()).await.unwrap(), None);
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

    #[tokio::test]
    async fn paste_waits_for_bounded_queue_capacity_and_runtime_completion() {
        let (commands, mut receiver) = async_mpsc::channel(1);
        let commands = test_commands(commands);
        commands
            .try_send(RuntimeMessage::Input(b"first".to_vec()))
            .unwrap();

        let waiting = tokio::spawn({
            let commands = commands.clone();
            async move { send_paste_with_backpressure(&commands, "second λ".into()).await }
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!waiting.is_finished());

        let RuntimeMessage::Input(first) = receiver.recv().await.unwrap() else {
            panic!("queued command was not input")
        };
        assert_eq!(first, b"first");
        let RuntimeMessage::Paste {
            text: second,
            completion,
        } = receiver.recv().await.unwrap()
        else {
            panic!("backpressured command was not paste")
        };
        assert_eq!(second, "second λ");
        assert!(!waiting.is_finished());
        completion
            .send(Err(CommandError::Emulator("write failed".into())))
            .unwrap();
        assert!(matches!(
            waiting.await.unwrap(),
            Err(CommandError::Emulator(message)) if message == "write failed"
        ));
    }

    #[tokio::test]
    async fn mouse_release_backpressures_behind_a_saturated_press_without_reordering() {
        assert_eq!(
            mouse_send_policy(MouseEventKind::Press {
                button: crate::domain::MouseButton::Left,
            }),
            MouseSendPolicy::Lossless
        );
        assert_eq!(
            mouse_send_policy(MouseEventKind::Motion { button: None }),
            MouseSendPolicy::Disposable
        );

        let (commands, mut receiver) = async_mpsc::channel(1);
        let commands = test_commands(commands);
        let (press_completion, _press_completed) = oneshot::channel();
        commands
            .try_send(RuntimeMessage::MouseInput {
                event: MouseEvent {
                    kind: MouseEventKind::Press {
                        button: crate::domain::MouseButton::Left,
                    },
                    column: 1,
                    row: 1,
                    modifiers: Default::default(),
                    buttons: crate::domain::MouseButtons {
                        left: true,
                        ..Default::default()
                    },
                },
                viewport_offset: None,
                pty_input_allowed: true,
                completion: press_completion,
            })
            .unwrap();

        let release = tokio::spawn({
            let commands = commands.clone();
            async move {
                send_mouse_input(
                    &commands,
                    MouseEvent {
                        kind: MouseEventKind::Release {
                            button: crate::domain::MouseButton::Left,
                        },
                        column: 2,
                        row: 1,
                        modifiers: Default::default(),
                        buttons: Default::default(),
                    },
                    None,
                    true,
                )
                .await
            }
        });
        tokio::task::yield_now().await;
        assert!(
            !release.is_finished(),
            "release was discarded at saturation"
        );

        let RuntimeMessage::MouseInput { event, .. } = receiver.recv().await.unwrap() else {
            panic!("first queued command was not a mouse press")
        };
        assert!(matches!(event.kind, MouseEventKind::Press { .. }));
        let RuntimeMessage::MouseInput {
            event, completion, ..
        } = receiver.recv().await.unwrap()
        else {
            panic!("backpressured command was not a mouse release")
        };
        assert!(matches!(event.kind, MouseEventKind::Release { .. }));
        assert!(completion.send(Ok(MouseInputOutcome::Handled)).is_ok());
        assert!(matches!(
            release.await.unwrap(),
            Ok(MouseInputOutcome::Handled)
        ));
    }

    #[tokio::test]
    async fn input_waiting_for_capacity_reports_receiver_closure_as_stopped() {
        let (commands, mut receiver) = async_mpsc::channel(1);
        let commands = test_commands(commands);
        commands
            .try_send(RuntimeMessage::Input(b"first".to_vec()))
            .unwrap();

        let waiting = tokio::spawn({
            let commands = commands.clone();
            async move {
                commands
                    .send(RuntimeMessage::Input(b"second".to_vec()))
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!waiting.is_finished());

        receiver.close();
        assert!(matches!(waiting.await.unwrap(), Err(CommandError::Stopped)));
        let RuntimeMessage::Input(first) = receiver.recv().await.unwrap() else {
            panic!("queued command was not input")
        };
        assert_eq!(first, b"first");
        assert!(receiver.recv().await.is_none());
    }
}
