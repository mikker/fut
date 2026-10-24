//! Execution locality is independent of the transport's socket representation.
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use super::{actions::ClientAction, config::UiConfig};
use crate::protocol::remote::Capabilities;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Locality {
    Local,
    Remote,
}

#[derive(Clone, Debug)]
pub(super) enum Attachment {
    Local {
        socket: PathBuf,
        pane_titles: bool,
    },
    Remote {
        capabilities: Capabilities,
        target: String,
        commands: Arc<RemoteCommands>,
    },
}

#[derive(Debug)]
pub(super) struct RemoteCommands {
    generation: uuid::Uuid,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl Drop for RemoteCommands {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().expect("command tasks lock") {
            task.abort();
        }
    }
}

impl Attachment {
    pub(super) fn remote(capabilities: Capabilities, target: impl Into<String>) -> Self {
        Self::Remote {
            capabilities,
            target: target.into(),
            commands: Arc::new(RemoteCommands {
                generation: uuid::Uuid::new_v4(),
                tasks: Mutex::new(Vec::new()),
            }),
        }
    }

    pub(super) fn command_generation(&self) -> Option<uuid::Uuid> {
        match self {
            Self::Local { .. } => None,
            Self::Remote { commands, .. } => Some(commands.generation),
        }
    }

    pub(super) fn run_background_command(
        &self,
        command: super::config::PaletteCommand,
        focused: crate::protocol::SelectedTarget,
        results: tokio::sync::mpsc::UnboundedSender<
            super::temporary_command::BackgroundCommandResult,
        >,
    ) {
        let Self::Remote {
            target, commands, ..
        } = self
        else {
            unreachable!("remote command dispatcher requires a remote attachment")
        };
        let target = target.clone();
        let generation = commands.generation;
        let task = tokio::spawn(async move {
            let result = super::remote_command::run_remote_background(&command, &focused, &target)
                .await
                .map_err(|error| super::one_line_error(&error));
            let _ = results.send(super::temporary_command::BackgroundCommandResult::Remote {
                generation,
                result,
            });
        });
        let mut tasks = commands.tasks.lock().expect("command tasks lock");
        tasks.retain(|task| !task.is_finished());
        tasks.push(task.abort_handle());
    }

    pub(super) fn supports_terminal_colors(&self) -> bool {
        match self {
            Self::Local { .. } => true,
            Self::Remote { capabilities, .. } => {
                capabilities.contains(crate::protocol::remote::Capability::TerminalColors)
            }
        }
    }

    pub(super) fn supports_attachments(&self) -> bool {
        match self {
            Self::Local { .. } => true,
            Self::Remote { capabilities, .. } => {
                capabilities.contains(crate::protocol::remote::Capability::Attachments)
            }
        }
    }

    pub(super) fn supports_pane_titles(&self) -> bool {
        match self {
            Self::Local { pane_titles, .. } => *pane_titles,
            Self::Remote { capabilities, .. } => {
                capabilities.contains(crate::protocol::remote::Capability::PaneTitles)
            }
        }
    }

    pub(super) fn blocked_command(&self) -> Option<&'static str> {
        match self {
            Self::Local { .. } => None,
            Self::Remote { capabilities, .. }
                if capabilities
                    .contains(crate::protocol::remote::Capability::ConfiguredCommands) =>
            {
                None
            }
            Self::Remote { .. } => Some(
                "commands unavailable during remote attachment · update Fut on the remote host and restart its daemon",
            ),
        }
    }

    pub(super) fn locality(&self) -> Locality {
        match self {
            Self::Local { .. } => Locality::Local,
            Self::Remote { .. } => Locality::Remote,
        }
    }

    pub(super) fn local_socket(&self) -> anyhow::Result<&Path> {
        match self {
            Self::Local { socket, .. } => Ok(socket.as_path()),
            Self::Remote { .. } => {
                anyhow::bail!("local commands unavailable during remote attachment")
            }
        }
    }
}

impl Locality {
    pub(super) fn blocked_action(self, action: ClientAction) -> Option<&'static str> {
        if self == Self::Local {
            return None;
        }
        match action {
            ClientAction::OpenProject => {
                Some("project opener unavailable during remote attachment")
            }
            ClientAction::ReloadConfig | ClientAction::ReloadProjectConfig => {
                Some("config reload unavailable during remote attachment")
            }
            _ => None,
        }
    }

    pub(super) fn permits_link(self, uri: &str) -> bool {
        self == Self::Local
            || uri.split_once("://").is_some_and(|(scheme, rest)| {
                (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
                    && !rest.is_empty()
            })
    }
}

impl Attachment {
    // Phase 1 disables client hooks entirely on remote attachments. Even locally
    // installed hooks assume local FUT_SOCKET and resource context today.
    pub(super) fn client_hooks(
        &self,
        ui: &UiConfig,
    ) -> anyhow::Result<Option<crate::extensions::ClientHookRuntime>> {
        match self {
            Self::Remote { .. } => Ok(None),
            Self::Local { socket, .. } => Ok(Some(crate::extensions::ClientHookRuntime::new(
                ui.extensions.clone(),
                std::env::current_exe()?,
                socket.to_owned(),
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_require_negotiated_support_and_have_distinct_attachment_generations() {
        let legacy = Attachment::remote(Capabilities::default(), "legacy");
        let current = Attachment::remote(Capabilities::ALL, "current");
        assert!(legacy.blocked_command().is_some());
        assert!(current.blocked_command().is_none());
        assert!(
            Attachment::Local {
                socket: "/tmp/fut.sock".into(),
                pane_titles: false
            }
            .blocked_command()
            .is_none()
        );
        assert_ne!(legacy.command_generation(), current.command_generation());
    }

    #[test]
    fn pane_renaming_requires_explicit_selection_on_both_endpoints() {
        let mut local = Attachment::Local {
            socket: "/tmp/fut.sock".into(),
            pane_titles: false,
        };
        assert!(!local.supports_pane_titles());
        let Attachment::Local { pane_titles, .. } = &mut local else {
            unreachable!()
        };
        *pane_titles = true;
        assert!(local.supports_pane_titles());
        assert!(!Attachment::remote(Capabilities::default(), "old-host").supports_pane_titles());
        assert!(Attachment::remote(Capabilities::ALL, "new-host").supports_pane_titles());
    }

    #[tokio::test]
    async fn dropping_attachment_cancels_its_background_commands() {
        let attachment = Attachment::remote(Capabilities::ALL, "remote");
        let task = tokio::spawn(std::future::pending::<()>());
        let Attachment::Remote { commands, .. } = &attachment else {
            unreachable!()
        };
        commands.tasks.lock().unwrap().push(task.abort_handle());
        drop(attachment);
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn image_paste_requires_remote_attachment_negotiation_but_is_available_locally() {
        assert!(
            Attachment::Local {
                socket: PathBuf::from("/tmp/fut.sock"),
                pane_titles: false
            }
            .supports_attachments()
        );
        assert!(!Attachment::remote(Capabilities::default(), "old-host").supports_attachments());
        assert!(Attachment::remote(Capabilities::ALL, "new-host").supports_attachments());
    }
}
