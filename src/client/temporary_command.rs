use std::{
    collections::HashMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use tokio::{
    io::AsyncReadExt,
    sync::{broadcast, mpsc, watch},
};

use crate::{
    command::{ACTIVATE_OPENED_SOCKET_ENV, PopupSize},
    domain::{PaneId, ScreenSnapshot, TerminalId, TerminalSize},
    protocol::SelectedTarget,
    terminal::{SpawnSpec, TerminalEvent, TerminalHandle, TerminalLifecycle, spawn_terminal},
};

use super::config::{PaletteCommand, ResolvedExtensionConfig, UiConfig, resolve_extension_config};

const MAX_BACKGROUND_ERROR_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug)]
pub(super) struct ExtensionCommandContext {
    focused: SelectedTarget,
    workspace_root: PathBuf,
    config: ResolvedExtensionConfig,
}

impl ExtensionCommandContext {
    pub(super) fn resolve(
        ui: &UiConfig,
        command: &PaletteCommand,
        focused: &SelectedTarget,
        workspace_root: &Path,
    ) -> anyhow::Result<Self> {
        let extension = command
            .extension
            .as_ref()
            .context("extension command is missing its identity")?;
        Ok(Self {
            focused: focused.clone(),
            workspace_root: workspace_root.to_owned(),
            config: resolve_extension_config(ui, &extension.id, workspace_root)?,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum BackgroundCommandResult {
    Succeeded,
    Failed(String),
}

pub(super) struct TemporaryCommandSurface {
    title: String,
    size: PopupSize,
    handle: TerminalHandle,
    snapshots: watch::Receiver<ScreenSnapshot>,
    events: broadcast::Receiver<TerminalEvent>,
    lifecycle: watch::Receiver<TerminalLifecycle>,
    activation: Option<ActivationSocket>,
    pub screen: ScreenSnapshot,
}

pub(super) enum TemporaryCommandUpdate {
    Screen,
    Exited(Option<i32>),
    Error(String),
    Stopped,
}

impl TemporaryCommandSurface {
    pub(super) async fn spawn(
        command: &PaletteCommand,
        pid: u32,
        fallback: &Path,
        size: TerminalSize,
        socket_path: &Path,
        extension_context: Option<&ExtensionCommandContext>,
    ) -> anyhow::Result<Self> {
        let cwd = if let Some(context) = extension_context {
            context.workspace_root.clone()
        } else {
            process_cwd(foreground_process_id(pid).await)
                .await
                .unwrap_or_else(|| fallback.to_path_buf())
        };
        let mut env = std::env::vars().collect::<HashMap<_, _>>();
        if command.extension.is_some() {
            let context =
                extension_context.context("extension command is missing runtime context")?;
            add_extension_environment(&mut env, command, context, socket_path)?;
        }
        let terminal_id = TerminalId::new();
        let activation = command
            .activate_opened
            .then(|| ActivationSocket::bind(socket_path))
            .transpose()?;
        if let Some(activation) = &activation {
            env.insert(
                ACTIVATE_OPENED_SOCKET_ENV.into(),
                activation.path.display().to_string(),
            );
        }
        let handle = spawn_terminal(SpawnSpec {
            id: terminal_id,
            program: command.program.clone(),
            argv: command.args.clone(),
            cwd,
            env,
            size,
        })
        .with_context(|| format!("start {}", command.title))?;
        let snapshots = handle.subscribe_snapshots();
        let screen = snapshots.borrow().clone();
        let events = handle.subscribe_events();
        let lifecycle = handle.subscribe_lifecycle();
        Ok(Self {
            title: command.title.clone(),
            size: command.size,
            handle,
            snapshots,
            events,
            lifecycle,
            activation,
            screen,
        })
    }

    pub(super) fn title(&self) -> &str {
        &self.title
    }

    pub(super) fn size(&self) -> PopupSize {
        self.size
    }

    pub(super) fn activated_target(&self) -> anyhow::Result<Option<PaneId>> {
        self.activation
            .as_ref()
            .map(ActivationSocket::target)
            .transpose()
            .map(Option::flatten)
    }

    pub(super) async fn input(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.handle.input(bytes).await.context("send command input")
    }

    pub(super) async fn paste(&self, text: String) -> anyhow::Result<()> {
        self.handle.paste(text).await.context("paste command input")
    }

    pub(super) async fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.handle
            .resize(size)
            .await
            .context("resize command surface")
    }

    pub(super) async fn update(&mut self) -> TemporaryCommandUpdate {
        if let TerminalLifecycle::Exited { exit_code } = self.lifecycle.borrow().clone() {
            return TemporaryCommandUpdate::Exited(exit_code);
        }
        tokio::select! {
            changed = self.snapshots.changed() => {
                if changed.is_ok() {
                    self.screen = self.snapshots.borrow_and_update().clone();
                    TemporaryCommandUpdate::Screen
                } else {
                    TemporaryCommandUpdate::Stopped
                }
            }
            event = self.events.recv() => match event {
                Ok(TerminalEvent::TerminalExited { exit_code }) => TemporaryCommandUpdate::Exited(exit_code),
                Ok(TerminalEvent::Error { message }) => TemporaryCommandUpdate::Error(message),
                Err(broadcast::error::RecvError::Lagged(_)) => TemporaryCommandUpdate::Screen,
                Err(broadcast::error::RecvError::Closed) => TemporaryCommandUpdate::Stopped,
            },
            changed = self.lifecycle.changed() => {
                if changed.is_err() {
                    TemporaryCommandUpdate::Stopped
                } else if let TerminalLifecycle::Exited { exit_code } = self.lifecycle.borrow().clone() {
                    TemporaryCommandUpdate::Exited(exit_code)
                } else {
                    TemporaryCommandUpdate::Screen
                }
            }
        }
    }
}

pub(super) fn dispatch_background_command(
    command: PaletteCommand,
    ui: UiConfig,
    focused: SelectedTarget,
    workspace_root: PathBuf,
    socket_path: PathBuf,
    results: mpsc::UnboundedSender<BackgroundCommandResult>,
) {
    tokio::spawn(async move {
        let context_command = command.clone();
        let context = tokio::task::spawn_blocking(move || {
            ExtensionCommandContext::resolve(&ui, &context_command, &focused, &workspace_root)
        })
        .await;
        let result = match context {
            Ok(Ok(context)) => run_background_command(&command, &context, &socket_path).await,
            Ok(Err(error)) => BackgroundCommandResult::Failed(bounded_error(format!(
                "{} config failed: {error:#}",
                command.title
            ))),
            Err(error) => BackgroundCommandResult::Failed(bounded_error(format!(
                "{} config task failed: {error}",
                command.title
            ))),
        };
        let _ = results.send(result);
    });
}

async fn run_background_command(
    command: &PaletteCommand,
    context: &ExtensionCommandContext,
    socket_path: &Path,
) -> BackgroundCommandResult {
    let mut environment = std::env::vars().collect::<HashMap<_, _>>();
    if let Err(error) = add_extension_environment(&mut environment, command, context, socket_path) {
        return BackgroundCommandResult::Failed(bounded_error(format!(
            "{} environment failed: {error:#}",
            command.title
        )));
    }
    let mut process = tokio::process::Command::new(&command.program);
    process
        .args(&command.args)
        .current_dir(&context.workspace_root)
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            return BackgroundCommandResult::Failed(bounded_error(format!(
                "{} failed to start: {error}",
                command.title
            )));
        }
    };
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        match stderr {
            Some(stderr) => drain_bounded(stderr).await,
            None => Ok(Vec::new()),
        }
    });
    let status = child.wait().await;
    let stderr = stderr_task
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    match status {
        Ok(status) if status.success() => BackgroundCommandResult::Succeeded,
        Ok(status) => {
            let detail = String::from_utf8_lossy(&stderr)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let status = status
                .code()
                .map_or_else(|| "unknown".to_owned(), |code| code.to_string());
            let suffix = (!detail.is_empty()).then(|| format!(": {detail}"));
            BackgroundCommandResult::Failed(bounded_error(format!(
                "{} exited with status {status}{}",
                command.title,
                suffix.unwrap_or_default()
            )))
        }
        Err(error) => BackgroundCommandResult::Failed(bounded_error(format!(
            "{} wait failed: {error}",
            command.title
        ))),
    }
}

fn add_extension_environment(
    environment: &mut HashMap<String, String>,
    command: &PaletteCommand,
    context: &ExtensionCommandContext,
    socket_path: &Path,
) -> anyhow::Result<()> {
    let extension = command
        .extension
        .as_ref()
        .context("extension command is missing its identity")?;
    environment.insert(
        "FUT_BIN".into(),
        std::env::current_exe()?.display().to_string(),
    );
    environment.insert("FUT_EXTENSION_COMMAND".into(), extension.command.clone());
    environment.insert("FUT_EXTENSION_ID".into(), extension.id.clone());
    environment.insert(
        "FUT_EXTENSION_ROOT".into(),
        extension.root.display().to_string(),
    );
    environment.insert("FUT_SOCKET".into(), socket_path.display().to_string());
    environment.insert(
        "FUT_SESSION_ID".into(),
        context.focused.session_id.to_string(),
    );
    environment.insert(
        "FUT_WORKSPACE_ID".into(),
        context.focused.workspace_id.to_string(),
    );
    environment.insert("FUT_TAB_ID".into(), context.focused.tab_id.to_string());
    environment.insert("FUT_PANE_ID".into(), context.focused.pane_id.to_string());
    environment.insert(
        "FUT_TERMINAL_ID".into(),
        context.focused.terminal_id.to_string(),
    );
    environment.insert("FUT_EXTENSION_CONFIG".into(), context.config.json.clone());
    if let Some(path) = &context.config.global_source {
        environment.insert(
            "FUT_EXTENSION_CONFIG_GLOBAL_PATH".into(),
            path.display().to_string(),
        );
    } else {
        environment.remove("FUT_EXTENSION_CONFIG_GLOBAL_PATH");
    }
    if let Some(path) = &context.config.workspace_source {
        environment.insert(
            "FUT_EXTENSION_CONFIG_WORKSPACE_PATH".into(),
            path.display().to_string(),
        );
    } else {
        environment.remove("FUT_EXTENSION_CONFIG_WORKSPACE_PATH");
    }
    Ok(())
}

async fn drain_bounded(mut reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let remaining = MAX_BACKGROUND_ERROR_BYTES.saturating_sub(retained.len());
        retained.extend_from_slice(&chunk[..count.min(remaining)]);
    }
    Ok(retained)
}

fn bounded_error(mut message: String) -> String {
    if message.len() <= MAX_BACKGROUND_ERROR_BYTES {
        return message;
    }
    let mut boundary = MAX_BACKGROUND_ERROR_BYTES.saturating_sub(3);
    while !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    message.truncate(boundary);
    message.push_str("...");
    message
}

struct ActivationSocket {
    socket: std::os::unix::net::UnixDatagram,
    path: PathBuf,
}

impl ActivationSocket {
    fn bind(daemon_socket: &Path) -> anyhow::Result<Self> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);

        let directory = daemon_socket
            .parent()
            .context("resolve activation socket directory")?;
        let path = directory.join(format!(
            ".fut-a-{}-{}.sock",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let socket = std::os::unix::net::UnixDatagram::bind(&path)
            .with_context(|| format!("bind command activation socket {}", path.display()))?;
        socket
            .set_nonblocking(true)
            .context("make command activation socket nonblocking")?;
        Ok(Self { socket, path })
    }

    fn target(&self) -> anyhow::Result<Option<PaneId>> {
        let mut payload = [0_u8; 128];
        let mut target = None;
        loop {
            match self.socket.recv(&mut payload) {
                Ok(length) => {
                    target = Some(
                        std::str::from_utf8(&payload[..length])
                            .context("command activation target is not UTF-8")?
                            .parse()
                            .context("command activation target is not a pane ID")?,
                    );
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(target),
                Err(error) => return Err(error).context("read command activation target"),
            }
        }
    }
}

impl Drop for ActivationSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

async fn foreground_process_id(child_pid: u32) -> u32 {
    let output = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        tokio::process::Command::new("/bin/ps")
            .args(["-o", "tpgid=", "-p", &child_pid.to_string()])
            .output(),
    )
    .await
    .ok()
    .and_then(Result::ok);
    output
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .unwrap_or(child_pid)
}

async fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        tokio::fs::read_link(format!("/proc/{pid}/cwd")).await.ok()
    }
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CStr, os::unix::ffi::OsStringExt};

        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>();
        // SAFETY: proc_pidinfo initializes at most `size` bytes in `info`, whose
        // pointer and declared size exactly match proc_vnodepathinfo.
        let written = unsafe {
            libc::proc_pidinfo(
                i32::try_from(pid).ok()?,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                i32::try_from(size).expect("proc_vnodepathinfo size fits i32"),
            )
        };
        if usize::try_from(written).ok()? != size {
            return None;
        }
        // SAFETY: the exact struct size was initialized above, and vip_path is
        // a NUL-terminated MAXPATHLEN buffer supplied by libproc.
        let info = unsafe { info.assume_init() };
        let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
        Some(PathBuf::from(std::ffi::OsString::from_vec(
            path.to_bytes().to_vec(),
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::config::ExtensionCommandIdentity;

    #[tokio::test]
    async fn live_process_cwd_is_inherited() {
        assert_eq!(
            process_cwd(std::process::id()).await.unwrap(),
            std::env::current_dir().unwrap()
        );
    }

    #[test]
    fn activation_socket_returns_the_most_recent_opened_pane_and_cleans_up() {
        let temporary = tempfile::tempdir().unwrap();
        let listener = ActivationSocket::bind(&temporary.path().join("daemon.sock")).unwrap();
        let path = listener.path.clone();
        let sender = std::os::unix::net::UnixDatagram::unbound().unwrap();
        let first = PaneId::new();
        let second = PaneId::new();
        sender.send_to(first.to_string().as_bytes(), &path).unwrap();
        sender
            .send_to(second.to_string().as_bytes(), &path)
            .unwrap();

        assert_eq!(listener.target().unwrap(), Some(second));
        drop(listener);
        assert!(!path.exists());
    }

    fn command(program: &str, args: &[&str]) -> PaletteCommand {
        PaletteCommand {
            title: "Test command".into(),
            binding: Some("x".into()),
            program: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            size: PopupSize::default(),
            activate_opened: false,
            extension: None,
            mode: crate::extensions::ExtensionCommandMode::Interactive,
        }
    }

    #[tokio::test]
    async fn exit_restores_by_completing_the_temporary_surface() {
        let cwd = std::env::current_dir().unwrap();
        let mut surface = TemporaryCommandSurface::spawn(
            &command("/bin/sh", &["-c", "exit 0"]),
            std::process::id(),
            &cwd,
            TerminalSize {
                columns: 40,
                rows: 10,
            },
            Path::new("/tmp/fut-test.sock"),
            None,
        )
        .await
        .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let TemporaryCommandUpdate::Exited(exit_code) = surface.update().await {
                    break exit_code;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(event, Some(0));
    }

    #[tokio::test]
    async fn spawn_failures_are_returned_without_a_surface() {
        let cwd = std::env::current_dir().unwrap();
        assert!(
            TemporaryCommandSurface::spawn(
                &command("/definitely/not/a/fut-command", &[]),
                std::process::id(),
                &cwd,
                TerminalSize {
                    columns: 40,
                    rows: 10
                },
                Path::new("/tmp/fut-test.sock"),
                None,
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn extension_commands_receive_their_runtime_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("environment");
        let socket = temporary.path().join("fut.sock");
        let mut launch = command(
            "/bin/sh",
            &[
                "-c",
                "printf '%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s' \"$FUT_EXTENSION_ID\" \"$FUT_EXTENSION_COMMAND\" \"$FUT_EXTENSION_ROOT\" \"$FUT_SOCKET\" \"$FUT_BIN\" \"$FUT_SESSION_ID\" \"$FUT_WORKSPACE_ID\" \"$FUT_TAB_ID\" \"$FUT_PANE_ID\" \"$FUT_TERMINAL_ID\" \"$FUT_EXTENSION_CONFIG\" \"$FUT_EXTENSION_CONFIG_GLOBAL_PATH\" \"$FUT_EXTENSION_CONFIG_WORKSPACE_PATH\" \"$PWD\" > \"$1\"",
                "sh",
                output.to_str().unwrap(),
            ],
        );
        launch.binding = None;
        launch.extension = Some(ExtensionCommandIdentity {
            id: "test-extension".into(),
            root: temporary.path().to_owned(),
            command: "launch".into(),
        });
        let focused = SelectedTarget {
            session_id: crate::domain::SessionId::new(),
            workspace_id: crate::domain::WorkspaceId::new(),
            tab_id: crate::domain::TabId::new(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
            child_pid: std::process::id(),
        };
        let global_config = temporary.path().join("global.toml");
        let workspace_config = temporary.path().join(".fut/config.toml");
        let context = ExtensionCommandContext {
            focused: focused.clone(),
            workspace_root: temporary.path().to_owned(),
            config: ResolvedExtensionConfig {
                json: r#"{"command":["just","run"]}"#.into(),
                global_source: Some(global_config.clone()),
                workspace_source: Some(workspace_config.clone()),
            },
        };
        let cwd = std::env::current_dir().unwrap();
        let mut surface = TemporaryCommandSurface::spawn(
            &launch,
            std::process::id(),
            &cwd,
            TerminalSize {
                columns: 40,
                rows: 10,
            },
            &socket,
            Some(&context),
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    surface.update().await,
                    TemporaryCommandUpdate::Exited(Some(0))
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let values = std::fs::read_to_string(output).unwrap();
        let mut values = values.lines();
        assert_eq!(values.next(), Some("test-extension"));
        assert_eq!(values.next(), Some("launch"));
        assert_eq!(values.next(), Some(temporary.path().to_str().unwrap()));
        assert_eq!(values.next(), Some(socket.to_str().unwrap()));
        assert_eq!(
            values.next(),
            Some(std::env::current_exe().unwrap().to_str().unwrap())
        );
        let focused_ids = [
            focused.session_id.to_string(),
            focused.workspace_id.to_string(),
            focused.tab_id.to_string(),
            focused.pane_id.to_string(),
            focused.terminal_id.to_string(),
        ];
        for id in &focused_ids {
            assert_eq!(values.next(), Some(id.as_str()));
        }
        assert_eq!(values.next(), Some(r#"{"command":["just","run"]}"#));
        assert_eq!(values.next(), Some(global_config.to_str().unwrap()));
        assert_eq!(values.next(), Some(workspace_config.to_str().unwrap()));
        let canonical_workspace = fs::canonicalize(temporary.path()).unwrap();
        assert_eq!(values.next(), Some(canonical_workspace.to_str().unwrap()));
    }

    #[tokio::test]
    async fn background_dispatch_returns_immediately_and_is_silent_on_success() {
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("completed");
        let mut launch = command(
            "/bin/sh",
            &[
                "-c",
                "sleep 0.25; printf '%s' \"$PWD\" > \"$1\"",
                "sh",
                output.to_str().unwrap(),
            ],
        );
        launch.mode = crate::extensions::ExtensionCommandMode::Background;
        launch.extension = Some(ExtensionCommandIdentity {
            id: "test-extension".into(),
            root: temporary.path().to_owned(),
            command: "restart".into(),
        });
        let focused = SelectedTarget {
            session_id: crate::domain::SessionId::new(),
            workspace_id: crate::domain::WorkspaceId::new(),
            tab_id: crate::domain::TabId::new(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
            child_pid: std::process::id(),
        };
        let (sender, mut results) = mpsc::unbounded_channel();

        dispatch_background_command(
            launch,
            UiConfig::default(),
            focused,
            temporary.path().to_owned(),
            temporary.path().join("fut.sock"),
            sender,
        );

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(75), results.recv())
                .await
                .is_err(),
            "dispatch waited for the child command"
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), results.recv())
                .await
                .unwrap(),
            Some(BackgroundCommandResult::Succeeded)
        );
        assert_eq!(
            fs::read_to_string(output).unwrap(),
            fs::canonicalize(temporary.path())
                .unwrap()
                .display()
                .to_string()
        );
    }

    #[tokio::test]
    async fn background_exit_failure_retains_only_bounded_stderr() {
        let temporary = tempfile::tempdir().unwrap();
        let mut launch = command(
            "/bin/sh",
            &[
                "-c",
                "printf 'actionable '; yes x | head -c 10000 >&2; exit 7",
            ],
        );
        launch.mode = crate::extensions::ExtensionCommandMode::Background;
        launch.extension = Some(ExtensionCommandIdentity {
            id: "test-extension".into(),
            root: temporary.path().to_owned(),
            command: "stop".into(),
        });
        let context = ExtensionCommandContext {
            focused: SelectedTarget {
                session_id: crate::domain::SessionId::new(),
                workspace_id: crate::domain::WorkspaceId::new(),
                tab_id: crate::domain::TabId::new(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
                child_pid: std::process::id(),
            },
            workspace_root: temporary.path().to_owned(),
            config: ResolvedExtensionConfig {
                json: "{}".into(),
                global_source: None,
                workspace_source: None,
            },
        };

        let BackgroundCommandResult::Failed(message) =
            run_background_command(&launch, &context, &temporary.path().join("fut.sock")).await
        else {
            panic!("nonzero command unexpectedly succeeded");
        };
        assert!(message.contains("status 7"), "{message}");
        assert!(message.len() <= MAX_BACKGROUND_ERROR_BYTES);
    }
}
