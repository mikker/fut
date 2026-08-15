use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::Context;
use tokio::sync::{broadcast, watch};

use crate::{
    domain::{ScreenSnapshot, TerminalId, TerminalSize},
    terminal::{SpawnSpec, TerminalEvent, TerminalHandle, TerminalLifecycle, spawn_terminal},
};

use super::config::PaletteCommand;

pub(super) struct TemporaryCommandSurface {
    title: String,
    handle: TerminalHandle,
    snapshots: watch::Receiver<ScreenSnapshot>,
    events: broadcast::Receiver<TerminalEvent>,
    lifecycle: watch::Receiver<TerminalLifecycle>,
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
        let handle = spawn_terminal(SpawnSpec {
            id: TerminalId::new(),
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
            handle,
            snapshots,
            events,
            lifecycle,
            screen,
        })
    }

    pub(super) fn title(&self) -> &str {
        &self.title
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

    fn command(program: &str, args: &[&str]) -> PaletteCommand {
        PaletteCommand {
            title: "Test command".into(),
            binding: Some("x".into()),
            program: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
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
