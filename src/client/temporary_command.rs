use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use crossterm::event::{
    KeyModifiers, MouseButton as HostMouseButton, MouseEvent as HostMouseEvent,
    MouseEventKind as HostMouseEventKind,
};
use ratatui::layout::Rect;
use tokio::{
    io::AsyncReadExt,
    sync::{broadcast, mpsc, watch},
};

use crate::{
    command::{ACTIVATE_OPENED_SOCKET_ENV, PopupSize},
    domain::{
        ClientId, CopyModeAction, MAX_TERMINAL_OUTPUT_ROWS, MouseButton, MouseButtons, MouseEvent,
        MouseEventKind, MouseModifiers, MouseWheelDirection, PaneId, ScreenSnapshot, TerminalId,
        TerminalOutputSource, TerminalSize,
    },
    protocol::SelectedTarget,
    resources::TrustedProjectConfig,
    terminal::{
        CopyModeOutcome, MouseInputOutcome, SpawnSpec, TerminalEvent, TerminalHandle,
        TerminalLifecycle, spawn_terminal,
    },
};

use super::config::{PaletteCommand, ResolvedExtensionConfig, UiConfig, resolve_extension_config};

const MAX_BACKGROUND_ERROR_BYTES: usize = 4 * 1024;
const FUT_EXTENSION_FORM: &str = "FUT_EXTENSION_FORM";

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
        project: Option<&TrustedProjectConfig>,
    ) -> anyhow::Result<Self> {
        let extension = command
            .extension
            .as_ref()
            .context("extension command is missing its identity")?;
        Ok(Self {
            focused: focused.clone(),
            workspace_root: workspace_root.to_owned(),
            config: resolve_extension_config(ui, &extension.id, workspace_root, project)?,
        })
    }

    pub(super) fn configured_field_default(&self, key: &str) -> anyhow::Result<Option<String>> {
        let config: serde_json::Value = serde_json::from_str(&self.config.json)
            .context("parse resolved extension config for command form")?;
        let Some(value) = config.get(key) else {
            return Ok(None);
        };
        match value {
            serde_json::Value::String(value) => Ok(Some(value.clone())),
            serde_json::Value::Array(values) if values.iter().all(serde_json::Value::is_string) => {
                Ok(Some(
                    values
                        .iter()
                        .map(|value| shell_display(value.as_str().expect("checked string")))
                        .collect::<Vec<_>>()
                        .join(" "),
                ))
            }
            _ => anyhow::bail!(
                "extension config {key:?} must be a string or an array of strings for a form default"
            ),
        }
    }

    #[cfg(test)]
    pub(super) fn test() -> Self {
        Self {
            focused: SelectedTarget {
                session_id: crate::domain::SessionId::new(),
                workspace_id: crate::domain::WorkspaceId::new(),
                tab_id: crate::domain::TabId::new(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
                child_pid: 1,
            },
            workspace_root: PathBuf::from("/workspace"),
            config: ResolvedExtensionConfig {
                json: "{}".into(),
                trusted_json: "{}".into(),
                global_source: None,
                project_source: None,
                workspace_source: None,
            },
        }
    }
}

fn shell_display(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
    mouse: TemporaryMouseState,
    pub screen: ScreenSnapshot,
}

#[derive(Default)]
struct TemporaryMouseState {
    owner: ClientId,
    buttons: MouseButtons,
    mode: TemporaryMouseMode,
    viewport_offset: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum TemporaryMouseMode {
    #[default]
    Idle,
    Selecting {
        anchor: (u16, u16),
        position: Option<(u16, u16)>,
    },
    Copying {
        request_id: uuid::Uuid,
        copy_id: uuid::Uuid,
    },
}

pub(super) struct TemporaryClipboardCopy {
    pub request_id: uuid::Uuid,
    pub text: String,
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
        form: Option<&BTreeMap<String, String>>,
    ) -> anyhow::Result<Self> {
        let crate::extensions::ExtensionCommandExecution::Interactive {
            size: popup_size,
            activate_opened,
        } = command.execution
        else {
            anyhow::bail!("background commands cannot open an interactive surface");
        };
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
            add_form_environment(&mut env, form)?;
            add_extension_environment(&mut env, command, context, socket_path)?;
        }
        let terminal_id = TerminalId::new();
        let activation = activate_opened
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
            size: popup_size,
            handle,
            snapshots,
            events,
            lifecycle,
            activation,
            mouse: TemporaryMouseState::default(),
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

    pub(super) async fn input(&mut self, bytes: Vec<u8>) -> anyhow::Result<()> {
        if self.prepare_input().await? {
            self.handle
                .input(bytes)
                .await
                .context("send command input")?;
        }
        Ok(())
    }

    pub(super) async fn paste(&mut self, text: String) -> anyhow::Result<()> {
        if self.prepare_input().await? {
            self.handle
                .paste(text)
                .await
                .context("paste command input")?;
        }
        Ok(())
    }

    pub(super) async fn resize(&mut self, size: TerminalSize) -> anyhow::Result<()> {
        if !self.prepare_input().await? {
            return Ok(());
        }
        self.handle
            .resize(size)
            .await
            .context("resize command surface")
    }

    pub(super) async fn mouse(
        &mut self,
        event: HostMouseEvent,
        content: Rect,
    ) -> anyhow::Result<Option<TemporaryClipboardCopy>> {
        if matches!(self.mouse.mode, TemporaryMouseMode::Copying { .. }) {
            return Ok(None);
        }
        let owner = self.mouse.owner;
        match event.kind {
            HostMouseEventKind::Down(HostMouseButton::Left)
                if !self.screen.mouse_tracking || event.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if let Some(anchor) = popup_cell(content, event.column, event.row, false) {
                    self.mouse.mode = TemporaryMouseMode::Selecting {
                        anchor,
                        position: None,
                    };
                }
                return Ok(None);
            }
            HostMouseEventKind::Drag(HostMouseButton::Left) => {
                if let TemporaryMouseMode::Selecting {
                    anchor,
                    mut position,
                } = self.mouse.mode
                {
                    let Some(cell) = popup_cell(content, event.column, event.row, true) else {
                        return Ok(None);
                    };
                    if position.is_none() {
                        self.apply_copy_action(
                            CopyModeAction::BeginSelection {
                                column: anchor.0,
                                row: anchor.1,
                            },
                            "begin command mouse selection",
                        )
                        .await?;
                    }
                    if position != Some(cell) {
                        self.apply_copy_action(
                            CopyModeAction::SetSelectionEnd {
                                column: cell.0,
                                row: cell.1,
                            },
                            "extend command mouse selection",
                        )
                        .await?;
                        position = Some(cell);
                    }
                    self.mouse.mode = TemporaryMouseMode::Selecting { anchor, position };
                    return Ok(None);
                }
            }
            HostMouseEventKind::Up(HostMouseButton::Left) => {
                if let TemporaryMouseMode::Selecting { position, .. } = self.mouse.mode {
                    let Some(cell) = popup_cell(content, event.column, event.row, true) else {
                        self.mouse.mode = TemporaryMouseMode::Idle;
                        return Ok(None);
                    };
                    let Some(previous) = position else {
                        self.mouse.mode = TemporaryMouseMode::Idle;
                        return Ok(None);
                    };
                    if previous != cell {
                        self.apply_copy_action(
                            CopyModeAction::SetSelectionEnd {
                                column: cell.0,
                                row: cell.1,
                            },
                            "finish command mouse selection",
                        )
                        .await?;
                    }
                    let CopyModeOutcome::Prepared { copy_id, text } = self
                        .handle
                        .copy_mode(owner, CopyModeAction::Copy, self.mouse.viewport_offset)
                        .await
                        .context("prepare command mouse copy")?
                    else {
                        anyhow::bail!("command mouse copy did not prepare clipboard text");
                    };
                    let request_id = uuid::Uuid::new_v4();
                    self.mouse.mode = TemporaryMouseMode::Copying {
                        request_id,
                        copy_id,
                    };
                    return Ok(Some(TemporaryClipboardCopy { request_id, text }));
                }
            }
            _ => {}
        }

        self.forward_mouse(event, content).await?;
        Ok(None)
    }

    pub(super) async fn finish_clipboard(
        &mut self,
        request_id: uuid::Uuid,
        copied: bool,
    ) -> anyhow::Result<bool> {
        let TemporaryMouseMode::Copying {
            request_id: expected,
            copy_id,
        } = self.mouse.mode
        else {
            return Ok(false);
        };
        if request_id != expected {
            return Ok(false);
        }
        let action = if copied {
            CopyModeAction::FinalizeCopy { copy_id }
        } else {
            CopyModeAction::Cancel
        };
        self.apply_copy_action(action, "finish command mouse copy")
            .await?;
        self.mouse.mode = TemporaryMouseMode::Idle;
        Ok(true)
    }

    pub(super) async fn cancel_mouse_copy(&mut self) {
        if let Ok(Some(screen)) = self.handle.clear_client(self.mouse.owner).await {
            self.screen = screen;
        }
        self.mouse.mode = TemporaryMouseMode::Idle;
        self.mouse.viewport_offset = None;
    }

    async fn prepare_input(&mut self) -> anyhow::Result<bool> {
        if matches!(self.mouse.mode, TemporaryMouseMode::Copying { .. }) {
            return Ok(false);
        }
        if self.copy_mode_active() {
            let screen = self
                .handle
                .clear_client(self.mouse.owner)
                .await
                .context("cancel command mouse selection before input")?;
            if let Some(screen) = screen {
                self.screen = screen;
            }
        }
        self.mouse.mode = TemporaryMouseMode::Idle;
        if self.mouse.viewport_offset.take().is_some() {
            self.screen = self
                .handle
                .viewport_snapshot(None)
                .await
                .context("return command viewport to bottom before input")?
                .screen;
        }
        Ok(true)
    }

    fn copy_mode_active(&self) -> bool {
        matches!(
            self.mouse.mode,
            TemporaryMouseMode::Selecting {
                position: Some(_),
                ..
            } | TemporaryMouseMode::Copying { .. }
        )
    }

    async fn apply_copy_action(
        &mut self,
        action: CopyModeAction,
        context: &'static str,
    ) -> anyhow::Result<()> {
        let outcome = self
            .handle
            .copy_mode(self.mouse.owner, action, self.mouse.viewport_offset)
            .await
            .context(context)?;
        self.accept_copy_outcome(outcome);
        Ok(())
    }

    async fn forward_mouse(&mut self, event: HostMouseEvent, content: Rect) -> anyhow::Result<()> {
        let button = match event.kind {
            HostMouseEventKind::Down(button)
            | HostMouseEventKind::Up(button)
            | HostMouseEventKind::Drag(button) => Some(normalize_button(button)),
            _ => None,
        };
        let clamp = matches!(
            event.kind,
            HostMouseEventKind::Up(_) | HostMouseEventKind::Drag(_)
        );
        let Some((column, row)) = popup_cell(content, event.column, event.row, clamp) else {
            return Ok(());
        };
        match event.kind {
            HostMouseEventKind::Down(button) => {
                let button = normalize_button(button);
                self.mouse.buttons.set(button, true);
            }
            HostMouseEventKind::Up(button) => {
                let button = normalize_button(button);
                if !self.mouse.buttons.contains(button) {
                    return Ok(());
                }
                self.mouse.buttons.set(button, false);
                if button == MouseButton::Left {
                    self.mouse.mode = TemporaryMouseMode::Idle;
                }
            }
            HostMouseEventKind::Drag(button)
                if !self.mouse.buttons.contains(normalize_button(button)) =>
            {
                return Ok(());
            }
            _ => {}
        }
        let kind = match event.kind {
            HostMouseEventKind::Down(button) => MouseEventKind::Press {
                button: normalize_button(button),
            },
            HostMouseEventKind::Up(button) => MouseEventKind::Release {
                button: normalize_button(button),
            },
            HostMouseEventKind::Drag(_) => MouseEventKind::Motion { button },
            HostMouseEventKind::Moved => MouseEventKind::Motion { button: None },
            HostMouseEventKind::ScrollUp | HostMouseEventKind::ScrollDown => {
                MouseEventKind::Wheel {
                    direction: if matches!(event.kind, HostMouseEventKind::ScrollUp) {
                        MouseWheelDirection::Up
                    } else {
                        MouseWheelDirection::Down
                    },
                }
            }
            HostMouseEventKind::ScrollLeft | HostMouseEventKind::ScrollRight => return Ok(()),
        };
        let outcome = self
            .handle
            .mouse_input(
                MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers: MouseModifiers {
                        shift: event.modifiers.contains(KeyModifiers::SHIFT),
                        control: event.modifiers.contains(KeyModifiers::CONTROL),
                        alt: event.modifiers.contains(KeyModifiers::ALT),
                    },
                    buttons: self.mouse.buttons,
                },
                self.mouse.viewport_offset,
                true,
            )
            .await
            .context("send command mouse input")?;
        if let MouseInputOutcome::Scrolled(viewport)
        | MouseInputOutcome::ReturnedToBottom(viewport) = outcome
        {
            self.mouse.viewport_offset = viewport.offset;
            self.screen = viewport.screen;
        }
        Ok(())
    }

    fn accept_copy_outcome(&mut self, outcome: CopyModeOutcome) {
        match outcome {
            CopyModeOutcome::Active(viewport) => {
                self.mouse.viewport_offset = viewport.offset;
                self.screen = viewport.screen;
            }
            CopyModeOutcome::Finalized { screen } | CopyModeOutcome::Cancelled { screen } => {
                self.mouse.viewport_offset = None;
                self.screen = screen;
            }
            CopyModeOutcome::Prepared { .. } => {}
        }
    }

    pub(super) async fn write_failure_log(&self, socket_path: &Path) -> anyhow::Result<PathBuf> {
        let output = self
            .handle
            .read_output(
                TerminalOutputSource::RecentUnwrapped,
                MAX_TERMINAL_OUTPUT_ROWS,
                false,
            )
            .await
            .context("read failed command output")?;
        let directory = socket_path
            .parent()
            .context("resolve command log directory")?
            .join("command-logs");
        tokio::fs::create_dir_all(&directory)
            .await
            .with_context(|| format!("create command log directory {}", directory.display()))?;
        let path = directory.join(format!("command-{}.log", self.handle.id()));
        tokio::fs::write(&path, output.text)
            .await
            .with_context(|| format!("write command log {}", path.display()))?;
        Ok(path)
    }

    pub(super) async fn update(&mut self) -> TemporaryCommandUpdate {
        if let TerminalLifecycle::Exited { exit_code } = self.lifecycle.borrow().clone() {
            return TemporaryCommandUpdate::Exited(exit_code);
        }
        tokio::select! {
            changed = self.snapshots.changed() => {
                if changed.is_ok() {
                    let canonical = self.snapshots.borrow_and_update().clone();
                    let viewport = if self.copy_mode_active() {
                        self.handle
                            .copy_mode_snapshot(self.mouse.owner, self.mouse.viewport_offset)
                            .await
                    } else if self.mouse.viewport_offset.is_some() {
                        self.handle.viewport_snapshot(self.mouse.viewport_offset).await
                    } else {
                        self.screen = canonical;
                        return TemporaryCommandUpdate::Screen;
                    };
                    match viewport {
                        Ok(viewport) => {
                            self.mouse.viewport_offset = viewport.offset;
                            self.screen = viewport.screen;
                            TemporaryCommandUpdate::Screen
                        }
                        Err(error) => {
                            self.mouse.mode = TemporaryMouseMode::Idle;
                            self.mouse.viewport_offset = None;
                            self.screen = canonical;
                            TemporaryCommandUpdate::Error(format!(
                                "mouse selection cancelled: {error}"
                            ))
                        }
                    }
                } else {
                    TemporaryCommandUpdate::Stopped
                }
            }
            event = self.events.recv() => match event {
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

fn normalize_button(button: HostMouseButton) -> MouseButton {
    match button {
        HostMouseButton::Left => MouseButton::Left,
        HostMouseButton::Middle => MouseButton::Middle,
        HostMouseButton::Right => MouseButton::Right,
    }
}

fn popup_cell(area: Rect, column: u16, row: u16, clamp: bool) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if !clamp && (column < area.x || column >= area.right() || row < area.y || row >= area.bottom())
    {
        return None;
    }
    Some((
        column.clamp(area.x, area.right() - 1) - area.x,
        row.clamp(area.y, area.bottom() - 1) - area.y,
    ))
}

fn add_form_environment(
    environment: &mut HashMap<String, String>,
    form: Option<&BTreeMap<String, String>>,
) -> anyhow::Result<()> {
    environment.remove(FUT_EXTENSION_FORM);
    if let Some(form) = form {
        environment.insert(FUT_EXTENSION_FORM.into(), serde_json::to_string(form)?);
    }
    Ok(())
}

pub(super) fn dispatch_background_command(
    command: PaletteCommand,
    ui: UiConfig,
    focused: SelectedTarget,
    workspace_root: PathBuf,
    project: Option<TrustedProjectConfig>,
    socket_path: PathBuf,
    results: mpsc::UnboundedSender<BackgroundCommandResult>,
) {
    tokio::spawn(async move {
        let context_command = command.clone();
        let context = tokio::task::spawn_blocking(move || {
            ExtensionCommandContext::resolve(
                &ui,
                &context_command,
                &focused,
                &workspace_root,
                project.as_ref(),
            )
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
    if let Err(error) = add_form_environment(&mut environment, None)
        .and_then(|()| add_extension_environment(&mut environment, command, context, socket_path))
    {
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
    environment.insert(
        "FUT_EXTENSION_TRUSTED_CONFIG".into(),
        context.config.trusted_json.clone(),
    );
    if let Some(path) = &context.config.global_source {
        environment.insert(
            "FUT_EXTENSION_CONFIG_GLOBAL_PATH".into(),
            path.display().to_string(),
        );
    } else {
        environment.remove("FUT_EXTENSION_CONFIG_GLOBAL_PATH");
    }
    if let Some(path) = &context.config.project_source {
        environment.insert(
            "FUT_EXTENSION_CONFIG_PROJECT_PATH".into(),
            path.display().to_string(),
        );
    } else {
        environment.remove("FUT_EXTENSION_CONFIG_PROJECT_PATH");
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

    fn mouse(kind: HostMouseEventKind, column: u16, row: u16) -> HostMouseEvent {
        HostMouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn shifted_mouse(kind: HostMouseEventKind, column: u16, row: u16) -> HostMouseEvent {
        HostMouseEvent {
            modifiers: KeyModifiers::SHIFT,
            ..mouse(kind, column, row)
        }
    }

    async fn wait_for_screen(
        surface: &mut TemporaryCommandSurface,
        predicate: impl Fn(&ScreenSnapshot) -> bool,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !predicate(&surface.screen) {
                assert!(!matches!(
                    surface.update().await,
                    TemporaryCommandUpdate::Exited(_) | TemporaryCommandUpdate::Stopped
                ));
            }
        })
        .await
        .unwrap();
    }

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

    #[test]
    fn form_environment_is_exact_and_never_inherited() {
        let mut environment = HashMap::from([(FUT_EXTENSION_FORM.into(), "stale".into())]);
        add_form_environment(&mut environment, None).unwrap();
        assert!(!environment.contains_key(FUT_EXTENSION_FORM));

        let form = BTreeMap::from([
            ("command".into(), "pi --model sonnet".into()),
            ("prompt".into(), "fix λ".into()),
            ("worktree".into(), String::new()),
        ]);
        add_form_environment(&mut environment, Some(&form)).unwrap();
        assert_eq!(
            environment[FUT_EXTENSION_FORM],
            r#"{"command":"pi --model sonnet","prompt":"fix λ","worktree":""}"#
        );
    }

    #[test]
    fn configured_form_defaults_accept_strings_and_quote_argv() {
        let context = ExtensionCommandContext {
            focused: SelectedTarget {
                session_id: crate::domain::SessionId::new(),
                workspace_id: crate::domain::WorkspaceId::new(),
                tab_id: crate::domain::TabId::new(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
                child_pid: 1,
            },
            workspace_root: PathBuf::from("/workspace"),
            config: ResolvedExtensionConfig {
                json: r#"{"command":["pi","two words","it's"]}"#.into(),
                trusted_json: "{}".into(),
                global_source: None,
                project_source: None,
                workspace_source: None,
            },
        };
        assert_eq!(
            context
                .configured_field_default("command")
                .unwrap()
                .as_deref(),
            Some("pi 'two words' 'it'\"'\"'s'")
        );
        assert_eq!(context.configured_field_default("missing").unwrap(), None);
    }

    fn command(program: &str, args: &[&str]) -> PaletteCommand {
        PaletteCommand {
            title: "Test command".into(),
            binding: Some("x".into()),
            program: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            execution: crate::extensions::ExtensionCommandExecution::Interactive {
                size: PopupSize::default(),
                activate_opened: false,
            },
            extension: None,
            fields: Vec::new(),
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
    async fn mouse_drag_selects_popup_text_and_prepares_a_clipboard_copy() {
        let temporary = tempfile::tempdir().unwrap();
        let input = temporary.path().join("input");
        let script = format!(
            "printf COPY_TARGET; stty raw -echo; dd bs=1 count=1 of='{}' 2>/dev/null; sleep 60",
            input.display()
        );
        let mut surface = TemporaryCommandSurface::spawn(
            &command("/bin/sh", &["-c", &script]),
            std::process::id(),
            temporary.path(),
            TerminalSize {
                columns: 20,
                rows: 4,
            },
            Path::new("/tmp/fut-test.sock"),
            None,
            None,
        )
        .await
        .unwrap();
        wait_for_screen(&mut surface, |screen| {
            screen
                .cells
                .iter()
                .map(|cell| cell.contents.as_str())
                .collect::<String>()
                .contains("COPY_TARGET")
        })
        .await;

        let content = Rect::new(5, 3, 20, 4);
        assert!(
            surface
                .mouse(
                    mouse(HostMouseEventKind::Down(HostMouseButton::Left), 5, 3),
                    content,
                )
                .await
                .unwrap()
                .is_none()
        );
        surface
            .mouse(
                mouse(HostMouseEventKind::Drag(HostMouseButton::Left), 15, 3),
                content,
            )
            .await
            .unwrap();
        let Some(TemporaryClipboardCopy { request_id, text }) = surface
            .mouse(
                mouse(HostMouseEventKind::Up(HostMouseButton::Left), 15, 3),
                content,
            )
            .await
            .unwrap()
        else {
            panic!("mouse release did not prepare a popup copy")
        };
        assert_eq!(text, "COPY_TARGET");
        assert!(surface.screen.cells.iter().any(|cell| cell.selected));
        assert!(
            surface
                .mouse(
                    mouse(HostMouseEventKind::Down(HostMouseButton::Left), 5, 3),
                    content,
                )
                .await
                .unwrap()
                .is_none()
        );
        surface.input(b"x".to_vec()).await.unwrap();
        match fs::read(&input) {
            Ok(bytes) => assert!(
                bytes.is_empty(),
                "input escaped while clipboard copy was pending"
            ),
            Err(error) => assert_eq!(error.kind(), ErrorKind::NotFound),
        }
        assert!(surface.finish_clipboard(request_id, true).await.unwrap());
        assert!(surface.screen.cells.iter().all(|cell| !cell.selected));
        surface.input(b"y".to_vec()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !fs::read(&input).is_ok_and(|bytes| bytes == b"y") {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn mouse_aware_popup_receives_content_local_gestures() {
        let temporary = tempfile::tempdir().unwrap();
        let capture = temporary.path().join("mouse.capture");
        let script = format!(
            "stty raw -echo; printf '\\033[?1000h\\033[?1006hREADY'; dd bs=1 count=18 of='{}' 2>/dev/null; sleep 60",
            capture.display()
        );
        let mut surface = TemporaryCommandSurface::spawn(
            &command("/bin/sh", &["-c", &script]),
            std::process::id(),
            temporary.path(),
            TerminalSize {
                columns: 20,
                rows: 4,
            },
            Path::new("/tmp/fut-test.sock"),
            None,
            None,
        )
        .await
        .unwrap();
        wait_for_screen(&mut surface, |screen| screen.mouse_tracking).await;

        let content = Rect::new(5, 3, 20, 4);
        for kind in [
            HostMouseEventKind::Down(HostMouseButton::Left),
            HostMouseEventKind::Up(HostMouseButton::Left),
        ] {
            assert!(
                surface
                    .mouse(mouse(kind, 7, 4), content)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if fs::read(&capture).is_ok_and(|bytes| bytes.len() == 18) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fs::read(capture).unwrap(), b"\x1b[<0;3;2M\x1b[<0;3;2m");

        surface
            .mouse(
                shifted_mouse(HostMouseEventKind::Down(HostMouseButton::Left), 5, 3),
                content,
            )
            .await
            .unwrap();
        surface
            .mouse(
                shifted_mouse(HostMouseEventKind::Drag(HostMouseButton::Left), 10, 3),
                content,
            )
            .await
            .unwrap();
        let Some(TemporaryClipboardCopy { request_id, text }) = surface
            .mouse(
                shifted_mouse(HostMouseEventKind::Up(HostMouseButton::Left), 10, 3),
                content,
            )
            .await
            .unwrap()
        else {
            panic!("Shift-drag did not override popup mouse tracking")
        };
        assert_eq!(text, "READY");
        assert!(surface.finish_clipboard(request_id, true).await.unwrap());
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
                None,
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn failed_command_output_is_written_beside_the_socket() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("fut.sock");
        let mut surface = TemporaryCommandSurface::spawn(
            &command("/bin/sh", &["-c", "printf 'actionable failure\\n'; exit 2"]),
            std::process::id(),
            temporary.path(),
            TerminalSize {
                columns: 40,
                rows: 10,
            },
            &socket,
            None,
            None,
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    surface.update().await,
                    TemporaryCommandUpdate::Exited(Some(2))
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();

        let path = surface.write_failure_log(&socket).await.unwrap();

        assert_eq!(
            path.parent(),
            Some(temporary.path().join("command-logs").as_path())
        );
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("actionable failure")
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
                trusted_json: r#"{"command":["just","run"]}"#.into(),
                global_source: Some(global_config.clone()),
                project_source: None,
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
            None,
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
        launch.execution = crate::extensions::ExtensionCommandExecution::Background;
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
            None,
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
        launch.execution = crate::extensions::ExtensionCommandExecution::Background;
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
                trusted_json: "{}".into(),
                global_source: None,
                project_source: None,
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
