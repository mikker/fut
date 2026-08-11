use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::Context;
use tokio::sync::{broadcast, watch};

use crate::{
    domain::{ScreenSnapshot, TerminalId, TerminalSize},
    terminal::{SpawnSpec, TerminalEvent, TerminalHandle, spawn_terminal},
};

use super::config::TrustedCommand;

pub(super) struct TemporaryCommandSurface {
    handle: TerminalHandle,
    snapshots: watch::Receiver<ScreenSnapshot>,
    events: broadcast::Receiver<TerminalEvent>,
    pub screen: ScreenSnapshot,
}

pub(super) enum TemporaryCommandUpdate {
    Screen,
    Event(Option<TerminalEvent>),
}

impl TemporaryCommandSurface {
    pub(super) async fn spawn(
        command: &TrustedCommand,
        pid: u32,
        fallback: &Path,
        size: TerminalSize,
    ) -> anyhow::Result<Self> {
        let cwd = process_cwd(pid)
            .await
            .unwrap_or_else(|| fallback.to_path_buf());
        let handle = spawn_terminal(SpawnSpec {
            id: TerminalId::new(),
            program: command.program.clone(),
            argv: command.args.clone(),
            cwd,
            env: std::env::vars().collect::<HashMap<_, _>>(),
            size,
        })
        .with_context(|| format!("start {}", command.title))?;
        let snapshots = handle.subscribe_snapshots();
        let screen = snapshots.borrow().clone();
        let events = handle.subscribe_events();
        Ok(Self {
            handle,
            snapshots,
            events,
            screen,
        })
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
        tokio::select! {
            changed = self.snapshots.changed() => {
                if changed.is_ok() {
                    self.screen = self.snapshots.borrow_and_update().clone();
                    TemporaryCommandUpdate::Screen
                } else {
                    TemporaryCommandUpdate::Event(None)
                }
            }
            event = self.events.recv() => match event {
                Ok(event) => TemporaryCommandUpdate::Event(Some(event)),
                Err(broadcast::error::RecvError::Lagged(_)) => TemporaryCommandUpdate::Screen,
                Err(broadcast::error::RecvError::Closed) => TemporaryCommandUpdate::Event(None),
            }
        }
    }
}

async fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        tokio::fs::read_link(format!("/proc/{pid}/cwd")).await.ok()
    }
    #[cfg(target_os = "macos")]
    {
        let output = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::process::Command::new("/usr/sbin/lsof")
                .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        output
            .status
            .success()
            .then(|| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .find_map(|line| line.strip_prefix('n'))
                    .map(PathBuf::from)
            })
            .flatten()
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

    #[tokio::test]
    async fn live_process_cwd_is_inherited() {
        assert_eq!(
            process_cwd(std::process::id()).await.unwrap(),
            std::env::current_dir().unwrap()
        );
    }

    fn command(program: &str, args: &[&str]) -> TrustedCommand {
        TrustedCommand {
            title: "Test command".into(),
            binding: "x".into(),
            program: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
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
        )
        .await
        .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let TemporaryCommandUpdate::Event(event) = surface.update().await {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            event,
            Some(TerminalEvent::TerminalExited { exit_code: Some(0) })
        );
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
            )
            .await
            .is_err()
        );
    }
}
