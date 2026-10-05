//! Execution locality is independent of the transport's socket representation.
use std::path::{Path, PathBuf};

use super::{actions::ClientAction, config::UiConfig};
use crate::protocol::remote::Capabilities;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Locality {
    Local,
    Remote,
}

#[derive(Clone, Debug)]
pub(super) enum Attachment {
    Local(PathBuf),
    Remote(Capabilities),
}

impl Attachment {
    pub(super) fn supports_terminal_colors(&self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Remote(capabilities) => {
                capabilities.contains(crate::protocol::remote::Capability::TerminalColors)
            }
        }
    }

    pub(super) fn locality(&self) -> Locality {
        match self {
            Self::Local(_) => Locality::Local,
            Self::Remote(_) => Locality::Remote,
        }
    }

    pub(super) fn local_socket(&self) -> anyhow::Result<&Path> {
        match self {
            Self::Local(path) => Ok(path.as_path()),
            Self::Remote(_) => anyhow::bail!("local commands unavailable during remote attachment"),
        }
    }
}

impl Locality {
    pub(super) fn blocked_command(self) -> Option<&'static str> {
        (self == Self::Remote).then_some("commands unavailable during remote attachment")
    }

    pub(super) fn blocked_action(self, action: ClientAction) -> Option<&'static str> {
        if self == Self::Local {
            return None;
        }
        match action {
            ClientAction::RunCommand(_) => self.blocked_command(),
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
            Self::Remote(_) => Ok(None),
            Self::Local(socket) => Ok(Some(crate::extensions::ClientHookRuntime::new(
                ui.extensions.clone(),
                std::env::current_exe()?,
                socket.to_owned(),
            ))),
        }
    }
}
