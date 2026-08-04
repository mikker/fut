#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ClientAction {
    OpenCommandBar,
    OpenNavigator,
    OpenWorkspaceSidebar,
    CreateTab,
    FocusNextPane,
    FocusPreviousPane,
    Detach,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ActionDefinition {
    pub action: ClientAction,
    pub title: &'static str,
    pub keywords: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DirectBinding {
    pub suffix: &'static [u8],
    pub action: ClientAction,
}

pub(super) const COMMANDS: [ActionDefinition; 6] = [
    ActionDefinition {
        action: ClientAction::OpenNavigator,
        title: "Open global navigator",
        keywords: "global resources sessions tabs panes switch go",
    },
    ActionDefinition {
        action: ClientAction::OpenWorkspaceSidebar,
        title: "Switch workspace",
        keywords: "workspace worktree checkout sidebar drawer switch",
    },
    ActionDefinition {
        action: ClientAction::CreateTab,
        title: "Create tab",
        keywords: "create new tab shell",
    },
    ActionDefinition {
        action: ClientAction::FocusNextPane,
        title: "Focus next pane",
        keywords: "focus next pane forward cycle",
    },
    ActionDefinition {
        action: ClientAction::FocusPreviousPane,
        title: "Focus previous pane",
        keywords: "focus previous pane back backward cycle",
    },
    ActionDefinition {
        action: ClientAction::Detach,
        title: "Detach client",
        keywords: "detach disconnect leave client",
    },
];

pub(super) const DIRECT_BINDINGS: [DirectBinding; 9] = [
    DirectBinding {
        suffix: b"k",
        action: ClientAction::OpenCommandBar,
    },
    DirectBinding {
        suffix: b"g",
        action: ClientAction::OpenNavigator,
    },
    DirectBinding {
        suffix: b"w",
        action: ClientAction::OpenWorkspaceSidebar,
    },
    DirectBinding {
        suffix: b"c",
        action: ClientAction::CreateTab,
    },
    DirectBinding {
        suffix: b"l",
        action: ClientAction::FocusNextPane,
    },
    DirectBinding {
        suffix: b"o",
        action: ClientAction::FocusNextPane,
    },
    DirectBinding {
        suffix: b"h",
        action: ClientAction::FocusPreviousPane,
    },
    DirectBinding {
        suffix: b";",
        action: ClientAction::FocusPreviousPane,
    },
    DirectBinding {
        suffix: b"d",
        action: ClientAction::Detach,
    },
];

pub(super) fn definition(action: ClientAction) -> Option<&'static ActionDefinition> {
    COMMANDS
        .iter()
        .find(|definition| definition.action == action)
}

pub(super) fn binding_label(action: ClientAction) -> String {
    DIRECT_BINDINGS
        .iter()
        .filter(|binding| binding.action == action)
        .map(|binding| {
            format!(
                "Ctrl-b {}",
                std::str::from_utf8(binding.suffix).expect("binding suffixes are UTF-8")
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(super) fn action_for_suffix(suffix: &[u8]) -> Option<ClientAction> {
    DIRECT_BINDINGS
        .iter()
        .find(|binding| binding.suffix == suffix)
        .map(|binding| binding.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_bindings_from_the_same_catalog() {
        for command in COMMANDS {
            let label = binding_label(command.action);
            assert!(!label.is_empty(), "{} has no direct binding", command.title);
            for binding in DIRECT_BINDINGS
                .iter()
                .filter(|binding| binding.action == command.action)
            {
                assert_eq!(action_for_suffix(binding.suffix), Some(command.action));
                let suffix = std::str::from_utf8(binding.suffix).unwrap();
                assert!(label.contains(&format!("Ctrl-b {suffix}")));
            }
        }
        assert!(definition(ClientAction::OpenCommandBar).is_none());
    }
}
