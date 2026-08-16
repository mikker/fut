use std::{
    collections::HashMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use tokio::sync::{broadcast, watch};

use crate::{
    command::{ACTIVATE_OPENED_SOCKET_ENV, PopupSize},
    domain::{PaneId, ScreenSnapshot, TerminalId, TerminalSize},
    terminal::{SpawnSpec, TerminalEvent, TerminalHandle, TerminalLifecycle, spawn_terminal},
};

use super::config::PaletteCommand;

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
    ) -> anyhow::Result<Self> {
        let cwd = process_cwd(foreground_process_id(pid).await)
            .await
            .unwrap_or_else(|| fallback.to_path_buf());
        let mut env = std::env::vars().collect::<HashMap<_, _>>();
        if let Some(extension) = &command.extension {
            env.insert(
                "FUT_BIN".into(),
                std::env::current_exe()?.display().to_string(),
            );
            env.insert("FUT_EXTENSION_COMMAND".into(), extension.command.clone());
            env.insert("FUT_EXTENSION_ID".into(), extension.id.clone());
            env.insert(
                "FUT_EXTENSION_ROOT".into(),
                extension.root.display().to_string(),
            );
            env.insert("FUT_SOCKET".into(), socket_path.display().to_string());
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
                "printf '%s\\n%s\\n%s\\n%s\\n%s' \"$FUT_EXTENSION_ID\" \"$FUT_EXTENSION_COMMAND\" \"$FUT_EXTENSION_ROOT\" \"$FUT_SOCKET\" \"$FUT_BIN\" > \"$1\"",
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
    }
}
