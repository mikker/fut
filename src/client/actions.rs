#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum FocusDirection {
    Left,
    Down,
    Up,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum NavigationScope {
    Pane,
    Tab,
    Workspace,
    Session,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum TabNumber {
    One,
    Two,
    Three,
    Four,
    Five,
    Six,
    Seven,
    Eight,
    Nine,
    Ten,
}

impl TabNumber {
    pub(super) const fn get(self) -> u8 {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::Three => 3,
            Self::Four => 4,
            Self::Five => 5,
            Self::Six => 6,
            Self::Seven => 7,
            Self::Eight => 8,
            Self::Nine => 9,
            Self::Ten => 10,
        }
    }
}

impl NavigationScope {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Tab => "tab",
            Self::Workspace => "workspace",
            Self::Session => "session",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ClientAction {
    OpenCommandBar,
    EnterCopyMode,
    OpenNavigator,
    OpenJump,
    OpenWorkspaceSidebar,
    OpenTabBar,
    OpenNotifications,
    FocusNextNotification,
    CreateTab,
    FocusNextTab,
    FocusPreviousTab,
    FocusNextWorkspace,
    FocusPreviousWorkspace,
    SplitPaneRight,
    SplitPaneDown,
    FocusNextPane,
    FocusPreviousPane,
    FocusPane(FocusDirection),
    FocusLast(NavigationScope),
    FocusTab(TabNumber),
    TogglePaneZoom,
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

pub(super) const ALL_ACTIONS: [ClientAction; 37] = [
    ClientAction::OpenCommandBar,
    ClientAction::EnterCopyMode,
    ClientAction::OpenNavigator,
    ClientAction::OpenJump,
    ClientAction::OpenWorkspaceSidebar,
    ClientAction::OpenTabBar,
    ClientAction::OpenNotifications,
    ClientAction::FocusNextNotification,
    ClientAction::CreateTab,
    ClientAction::FocusNextTab,
    ClientAction::FocusPreviousTab,
    ClientAction::FocusNextWorkspace,
    ClientAction::FocusPreviousWorkspace,
    ClientAction::SplitPaneRight,
    ClientAction::SplitPaneDown,
    ClientAction::FocusNextPane,
    ClientAction::FocusPreviousPane,
    ClientAction::FocusPane(FocusDirection::Left),
    ClientAction::FocusPane(FocusDirection::Down),
    ClientAction::FocusPane(FocusDirection::Up),
    ClientAction::FocusPane(FocusDirection::Right),
    ClientAction::FocusLast(NavigationScope::Pane),
    ClientAction::FocusLast(NavigationScope::Tab),
    ClientAction::FocusLast(NavigationScope::Workspace),
    ClientAction::FocusLast(NavigationScope::Session),
    ClientAction::FocusTab(TabNumber::One),
    ClientAction::FocusTab(TabNumber::Two),
    ClientAction::FocusTab(TabNumber::Three),
    ClientAction::FocusTab(TabNumber::Four),
    ClientAction::FocusTab(TabNumber::Five),
    ClientAction::FocusTab(TabNumber::Six),
    ClientAction::FocusTab(TabNumber::Seven),
    ClientAction::FocusTab(TabNumber::Eight),
    ClientAction::FocusTab(TabNumber::Nine),
    ClientAction::FocusTab(TabNumber::Ten),
    ClientAction::TogglePaneZoom,
    ClientAction::Detach,
];

pub(super) const COMMANDS: [ActionDefinition; 36] = [
    ActionDefinition {
        action: ClientAction::OpenNavigator,
        title: "Open global navigator",
        keywords: "global resources sessions tabs panes switch go",
    },
    ActionDefinition {
        action: ClientAction::OpenJump,
        title: "Jump to resource",
        keywords: "jump find filter search fuzzy quick switcher sessions workspaces tabs panes",
    },
    ActionDefinition {
        action: ClientAction::OpenWorkspaceSidebar,
        title: "Switch workspace",
        keywords: "workspace worktree checkout sidebar drawer switch",
    },
    ActionDefinition {
        action: ClientAction::OpenTabBar,
        title: "Focus tab bar",
        keywords: "tab bar list switch navigation",
    },
    ActionDefinition {
        action: ClientAction::OpenNotifications,
        title: "Open terminals waiting",
        keywords: "notifications unread waiting agents completed blocked",
    },
    ActionDefinition {
        action: ClientAction::FocusNextNotification,
        title: "Switch to next waiting terminal",
        keywords: "notifications unread next waiting agents completed blocked",
    },
    ActionDefinition {
        action: ClientAction::CreateTab,
        title: "Create tab",
        keywords: "create new tab shell",
    },
    ActionDefinition {
        action: ClientAction::FocusNextTab,
        title: "Switch to next tab",
        keywords: "tab next forward cycle switch",
    },
    ActionDefinition {
        action: ClientAction::FocusPreviousTab,
        title: "Switch to previous tab",
        keywords: "tab previous back backward cycle switch",
    },
    ActionDefinition {
        action: ClientAction::FocusNextWorkspace,
        title: "Switch to next workspace",
        keywords: "workspace worktree next forward cycle switch",
    },
    ActionDefinition {
        action: ClientAction::FocusPreviousWorkspace,
        title: "Switch to previous workspace",
        keywords: "workspace worktree previous back backward cycle switch",
    },
    ActionDefinition {
        action: ClientAction::SplitPaneRight,
        title: "Split pane right",
        keywords: "pane split right horizontal create",
    },
    ActionDefinition {
        action: ClientAction::SplitPaneDown,
        title: "Split pane down",
        keywords: "pane split down vertical create",
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
        action: ClientAction::FocusPane(FocusDirection::Left),
        title: "Focus pane left",
        keywords: "focus pane left vim direction",
    },
    ActionDefinition {
        action: ClientAction::FocusPane(FocusDirection::Down),
        title: "Focus pane down",
        keywords: "focus pane down vim direction",
    },
    ActionDefinition {
        action: ClientAction::FocusPane(FocusDirection::Up),
        title: "Focus pane up",
        keywords: "focus pane up vim direction",
    },
    ActionDefinition {
        action: ClientAction::FocusPane(FocusDirection::Right),
        title: "Focus pane right",
        keywords: "focus pane right vim direction",
    },
    ActionDefinition {
        action: ClientAction::FocusLast(NavigationScope::Pane),
        title: "Switch to last pane",
        keywords: "focus switch last previous pane history",
    },
    ActionDefinition {
        action: ClientAction::FocusLast(NavigationScope::Tab),
        title: "Switch to last tab",
        keywords: "focus switch last previous tab history",
    },
    ActionDefinition {
        action: ClientAction::FocusLast(NavigationScope::Workspace),
        title: "Switch to last workspace",
        keywords: "focus switch last previous workspace worktree history",
    },
    ActionDefinition {
        action: ClientAction::FocusLast(NavigationScope::Session),
        title: "Switch to last session",
        keywords: "focus switch last previous session project history",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::One),
        title: "Switch to tab 1",
        keywords: "focus switch numbered tab first one",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::Two),
        title: "Switch to tab 2",
        keywords: "focus switch numbered tab second two",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::Three),
        title: "Switch to tab 3",
        keywords: "focus switch numbered tab third three",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::Four),
        title: "Switch to tab 4",
        keywords: "focus switch numbered tab fourth four",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::Five),
        title: "Switch to tab 5",
        keywords: "focus switch numbered tab fifth five",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::Six),
        title: "Switch to tab 6",
        keywords: "focus switch numbered tab sixth six",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::Seven),
        title: "Switch to tab 7",
        keywords: "focus switch numbered tab seventh seven",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::Eight),
        title: "Switch to tab 8",
        keywords: "focus switch numbered tab eighth eight",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::Nine),
        title: "Switch to tab 9",
        keywords: "focus switch numbered tab ninth nine",
    },
    ActionDefinition {
        action: ClientAction::FocusTab(TabNumber::Ten),
        title: "Switch to tab 10",
        keywords: "focus switch numbered tab tenth ten zero",
    },
    ActionDefinition {
        action: ClientAction::TogglePaneZoom,
        title: "Toggle pane zoom",
        keywords: "pane zoom maximize restore fullscreen",
    },
    ActionDefinition {
        action: ClientAction::EnterCopyMode,
        title: "Enter copy mode",
        keywords: "copy select scrollback search clipboard",
    },
    ActionDefinition {
        action: ClientAction::Detach,
        title: "Detach client",
        keywords: "detach disconnect leave client",
    },
];

const UP: &[u8] = b"\x1b[A";
const DOWN: &[u8] = b"\x1b[B";

pub(super) const DIRECT_BINDINGS: [DirectBinding; 37] = [
    DirectBinding {
        suffix: b" ",
        action: ClientAction::OpenCommandBar,
    },
    DirectBinding {
        suffix: b"[",
        action: ClientAction::EnterCopyMode,
    },
    DirectBinding {
        suffix: b"g",
        action: ClientAction::OpenNavigator,
    },
    DirectBinding {
        suffix: b"f",
        action: ClientAction::OpenJump,
    },
    DirectBinding {
        suffix: b"w",
        action: ClientAction::OpenWorkspaceSidebar,
    },
    DirectBinding {
        suffix: b"t",
        action: ClientAction::OpenTabBar,
    },
    DirectBinding {
        suffix: b"u",
        action: ClientAction::OpenNotifications,
    },
    DirectBinding {
        suffix: b".",
        action: ClientAction::FocusNextNotification,
    },
    DirectBinding {
        suffix: b"c",
        action: ClientAction::CreateTab,
    },
    DirectBinding {
        suffix: b"n",
        action: ClientAction::FocusNextTab,
    },
    DirectBinding {
        suffix: b"p",
        action: ClientAction::FocusPreviousTab,
    },
    DirectBinding {
        suffix: DOWN,
        action: ClientAction::FocusNextWorkspace,
    },
    DirectBinding {
        suffix: UP,
        action: ClientAction::FocusPreviousWorkspace,
    },
    DirectBinding {
        suffix: b"|",
        action: ClientAction::SplitPaneRight,
    },
    DirectBinding {
        suffix: b"_",
        action: ClientAction::SplitPaneDown,
    },
    DirectBinding {
        suffix: b"l",
        action: ClientAction::FocusPane(FocusDirection::Right),
    },
    DirectBinding {
        suffix: b"o",
        action: ClientAction::FocusNextPane,
    },
    DirectBinding {
        suffix: b"h",
        action: ClientAction::FocusPane(FocusDirection::Left),
    },
    DirectBinding {
        suffix: b";",
        action: ClientAction::FocusPreviousPane,
    },
    DirectBinding {
        suffix: b"j",
        action: ClientAction::FocusPane(FocusDirection::Down),
    },
    DirectBinding {
        suffix: b"k",
        action: ClientAction::FocusPane(FocusDirection::Up),
    },
    DirectBinding {
        suffix: b"P",
        action: ClientAction::FocusLast(NavigationScope::Pane),
    },
    DirectBinding {
        suffix: b"T",
        action: ClientAction::FocusLast(NavigationScope::Tab),
    },
    DirectBinding {
        suffix: b"W",
        action: ClientAction::FocusLast(NavigationScope::Workspace),
    },
    DirectBinding {
        suffix: b"S",
        action: ClientAction::FocusLast(NavigationScope::Session),
    },
    DirectBinding {
        suffix: b"1",
        action: ClientAction::FocusTab(TabNumber::One),
    },
    DirectBinding {
        suffix: b"2",
        action: ClientAction::FocusTab(TabNumber::Two),
    },
    DirectBinding {
        suffix: b"3",
        action: ClientAction::FocusTab(TabNumber::Three),
    },
    DirectBinding {
        suffix: b"4",
        action: ClientAction::FocusTab(TabNumber::Four),
    },
    DirectBinding {
        suffix: b"5",
        action: ClientAction::FocusTab(TabNumber::Five),
    },
    DirectBinding {
        suffix: b"6",
        action: ClientAction::FocusTab(TabNumber::Six),
    },
    DirectBinding {
        suffix: b"7",
        action: ClientAction::FocusTab(TabNumber::Seven),
    },
    DirectBinding {
        suffix: b"8",
        action: ClientAction::FocusTab(TabNumber::Eight),
    },
    DirectBinding {
        suffix: b"9",
        action: ClientAction::FocusTab(TabNumber::Nine),
    },
    DirectBinding {
        suffix: b"0",
        action: ClientAction::FocusTab(TabNumber::Ten),
    },
    DirectBinding {
        suffix: b"z",
        action: ClientAction::TogglePaneZoom,
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

pub(super) fn config_key(action: ClientAction) -> &'static str {
    match action {
        ClientAction::OpenCommandBar => "open_command_bar",
        ClientAction::EnterCopyMode => "enter_copy_mode",
        ClientAction::OpenNavigator => "open_navigator",
        ClientAction::OpenJump => "open_jump",
        ClientAction::OpenWorkspaceSidebar => "open_workspace_sidebar",
        ClientAction::OpenTabBar => "open_tab_bar",
        ClientAction::OpenNotifications => "open_notifications",
        ClientAction::FocusNextNotification => "focus_next_notification",
        ClientAction::CreateTab => "create_tab",
        ClientAction::FocusNextTab => "focus_next_tab",
        ClientAction::FocusPreviousTab => "focus_previous_tab",
        ClientAction::FocusNextWorkspace => "focus_next_workspace",
        ClientAction::FocusPreviousWorkspace => "focus_previous_workspace",
        ClientAction::SplitPaneRight => "split_pane_right",
        ClientAction::SplitPaneDown => "split_pane_down",
        ClientAction::FocusNextPane => "focus_next_pane",
        ClientAction::FocusPreviousPane => "focus_previous_pane",
        ClientAction::FocusPane(FocusDirection::Left) => "focus_pane_left",
        ClientAction::FocusPane(FocusDirection::Down) => "focus_pane_down",
        ClientAction::FocusPane(FocusDirection::Up) => "focus_pane_up",
        ClientAction::FocusPane(FocusDirection::Right) => "focus_pane_right",
        ClientAction::FocusLast(NavigationScope::Pane) => "focus_last_pane",
        ClientAction::FocusLast(NavigationScope::Tab) => "focus_last_tab",
        ClientAction::FocusLast(NavigationScope::Workspace) => "focus_last_workspace",
        ClientAction::FocusLast(NavigationScope::Session) => "focus_last_session",
        ClientAction::FocusTab(TabNumber::One) => "focus_tab_1",
        ClientAction::FocusTab(TabNumber::Two) => "focus_tab_2",
        ClientAction::FocusTab(TabNumber::Three) => "focus_tab_3",
        ClientAction::FocusTab(TabNumber::Four) => "focus_tab_4",
        ClientAction::FocusTab(TabNumber::Five) => "focus_tab_5",
        ClientAction::FocusTab(TabNumber::Six) => "focus_tab_6",
        ClientAction::FocusTab(TabNumber::Seven) => "focus_tab_7",
        ClientAction::FocusTab(TabNumber::Eight) => "focus_tab_8",
        ClientAction::FocusTab(TabNumber::Nine) => "focus_tab_9",
        ClientAction::FocusTab(TabNumber::Ten) => "focus_tab_10",
        ClientAction::TogglePaneZoom => "toggle_pane_zoom",
        ClientAction::Detach => "detach",
    }
}

pub(super) fn default_suffix(action: ClientAction) -> &'static [u8] {
    DIRECT_BINDINGS
        .iter()
        .find(|binding| binding.action == action)
        .expect("every action has a default binding")
        .suffix
}

pub(super) fn parse_suffix(value: &str) -> Option<(Vec<u8>, String)> {
    let bytes = match value {
        "space" => b" ".to_vec(),
        "enter" => b"\r".to_vec(),
        "tab" => b"\t".to_vec(),
        "escape" | "esc" => b"\x1b".to_vec(),
        "up" => UP.to_vec(),
        "down" => DOWN.to_vec(),
        _ if value.chars().count() == 1 && !value.chars().next()?.is_control() => {
            value.as_bytes().to_vec()
        }
        _ => return None,
    };
    let label = suffix_name(&bytes);
    Some((bytes, label))
}

pub(super) fn suffix_name(suffix: &[u8]) -> String {
    match suffix {
        b" " => "Space".into(),
        b"\r" => "Enter".into(),
        b"\t" => "Tab".into(),
        b"\x1b" => "Esc".into(),
        _ if suffix == UP => "Up".into(),
        _ if suffix == DOWN => "Down".into(),
        _ => String::from_utf8_lossy(suffix).into_owned(),
    }
}

#[cfg(test)]
const fn requires_launcher(action: ClientAction) -> bool {
    match action {
        ClientAction::OpenCommandBar => false,
        ClientAction::EnterCopyMode
        | ClientAction::OpenNavigator
        | ClientAction::OpenJump
        | ClientAction::OpenWorkspaceSidebar
        | ClientAction::OpenTabBar
        | ClientAction::OpenNotifications
        | ClientAction::FocusNextNotification
        | ClientAction::CreateTab
        | ClientAction::FocusNextTab
        | ClientAction::FocusPreviousTab
        | ClientAction::FocusNextWorkspace
        | ClientAction::FocusPreviousWorkspace
        | ClientAction::SplitPaneRight
        | ClientAction::SplitPaneDown
        | ClientAction::FocusNextPane
        | ClientAction::FocusPreviousPane
        | ClientAction::FocusPane(FocusDirection::Left)
        | ClientAction::FocusPane(FocusDirection::Down)
        | ClientAction::FocusPane(FocusDirection::Up)
        | ClientAction::FocusPane(FocusDirection::Right)
        | ClientAction::FocusLast(NavigationScope::Pane)
        | ClientAction::FocusLast(NavigationScope::Tab)
        | ClientAction::FocusLast(NavigationScope::Workspace)
        | ClientAction::FocusLast(NavigationScope::Session)
        | ClientAction::FocusTab(TabNumber::One)
        | ClientAction::FocusTab(TabNumber::Two)
        | ClientAction::FocusTab(TabNumber::Three)
        | ClientAction::FocusTab(TabNumber::Four)
        | ClientAction::FocusTab(TabNumber::Five)
        | ClientAction::FocusTab(TabNumber::Six)
        | ClientAction::FocusTab(TabNumber::Seven)
        | ClientAction::FocusTab(TabNumber::Eight)
        | ClientAction::FocusTab(TabNumber::Nine)
        | ClientAction::FocusTab(TabNumber::Ten)
        | ClientAction::TogglePaneZoom
        | ClientAction::Detach => true,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::client::config::BindingsConfig;

    #[test]
    fn every_command_has_bindings_from_the_same_catalog() {
        let bindings = BindingsConfig::default();
        for command in COMMANDS {
            let label = bindings.label(command.action);
            assert!(!label.is_empty(), "{} has no direct binding", command.title);
            for binding in DIRECT_BINDINGS
                .iter()
                .filter(|binding| binding.action == command.action)
            {
                assert_eq!(
                    bindings.action_for_suffix(binding.suffix),
                    Some(command.action)
                );
                let expected = suffix_name(binding.suffix);
                assert!(label.contains(&format!("Ctrl-b {expected}")));
            }
        }
        assert!(definition(ClientAction::OpenCommandBar).is_none());
        for action in ALL_ACTIONS {
            if requires_launcher(action) {
                assert!(
                    definition(action).is_some(),
                    "{action:?} is absent from launcher"
                );
            }
        }
        for binding in DIRECT_BINDINGS {
            if binding.action != ClientAction::OpenCommandBar {
                assert!(definition(binding.action).is_some());
            }
        }
        assert_eq!(
            DIRECT_BINDINGS
                .iter()
                .map(|binding| binding.suffix)
                .collect::<HashSet<_>>()
                .len(),
            DIRECT_BINDINGS.len(),
            "direct binding suffixes must be unique"
        );
        assert_eq!(
            COMMANDS
                .iter()
                .map(|command| command.action)
                .collect::<HashSet<_>>()
                .len(),
            COMMANDS.len(),
            "launcher actions must be unique"
        );
        assert_eq!(
            bindings.action_for_suffix(b"z"),
            Some(ClientAction::TogglePaneZoom)
        );
        assert_eq!(bindings.label(ClientAction::TogglePaneZoom), "Ctrl-b z");
    }
}
