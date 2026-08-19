use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[cfg(test)]
use super::actions::{FocusDirection, HistoryScope};
use super::{actions::ClientAction, config::BindingsConfig};

pub(crate) fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    let mut bytes = match key.code {
        KeyCode::Char(character) => encode_character(character, key.modifiers),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::F(number) => function_key(number)?.to_vec(),
        _ => return None,
    };
    if key.modifiers.contains(KeyModifiers::ALT) && !matches!(key.code, KeyCode::Char(_)) {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn encode_character(character: char, modifiers: KeyModifiers) -> Vec<u8> {
    let mut bytes = if modifiers.contains(KeyModifiers::CONTROL) {
        match character.to_ascii_lowercase() {
            '@' | ' ' => vec![0],
            'a'..='z' => vec![(character.to_ascii_lowercase() as u8) - b'a' + 1],
            '[' => vec![27],
            '\\' => vec![28],
            ']' => vec![29],
            '^' => vec![30],
            '_' => vec![31],
            '?' => vec![127],
            _ => character.to_string().into_bytes(),
        }
    } else {
        character.to_string().into_bytes()
    };
    if modifiers.contains(KeyModifiers::ALT) {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn function_key(number: u8) -> Option<&'static [u8]> {
    Some(match number {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    })
}

pub(super) struct PrefixState {
    waiting: bool,
    bindings: BindingsConfig,
}

impl Default for PrefixState {
    fn default() -> Self {
        Self::new(BindingsConfig::default())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum PrefixAction {
    Wait,
    Dispatch(ClientAction),
    Send(Vec<u8>),
}

impl PrefixState {
    pub(super) fn new(bindings: BindingsConfig) -> Self {
        Self {
            waiting: false,
            bindings,
        }
    }

    pub(super) fn replace_bindings(&mut self, bindings: BindingsConfig) {
        self.waiting = false;
        self.bindings = bindings;
    }

    pub(super) fn feed(&mut self, bytes: Vec<u8>) -> PrefixAction {
        if !self.waiting {
            if bytes == self.bindings.prefix() {
                self.waiting = true;
                PrefixAction::Wait
            } else {
                PrefixAction::Send(bytes)
            }
        } else {
            self.waiting = false;
            if let Some(action) = self.bindings.action_for_suffix(&bytes) {
                PrefixAction::Dispatch(action)
            } else if bytes == self.bindings.prefix() {
                PrefixAction::Send(self.bindings.prefix().to_vec())
            } else {
                PrefixAction::Send([self.bindings.prefix(), bytes.as_slice()].concat())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn encodes_characters_modifiers_and_navigation() {
        assert_eq!(
            encode_key(key(KeyCode::Char('é'), KeyModifiers::NONE)),
            Some("é".as_bytes().to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![3])
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Left, KeyModifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::F(12), KeyModifiers::NONE)),
            Some(b"\x1b[24~".to_vec())
        );
    }

    #[test]
    fn ignores_key_release() {
        let mut event = key(KeyCode::Char('x'), KeyModifiers::NONE);
        event.kind = KeyEventKind::Release;
        assert_eq!(encode_key(event), None);
    }

    #[test]
    fn prefix_dispatches_itself_and_preserves_other_sequences() {
        let mut prefix = PrefixState::default();
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"[".to_vec()),
            PrefixAction::Dispatch(ClientAction::EnterCopyMode)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"d".to_vec()),
            PrefixAction::Dispatch(ClientAction::Detach)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"s".to_vec()),
            PrefixAction::Dispatch(ClientAction::OpenNavigator)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"a".to_vec()),
            PrefixAction::Dispatch(ClientAction::OpenAgents)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"w".to_vec()),
            PrefixAction::Dispatch(ClientAction::OpenLeftSidebar)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"]".to_vec()),
            PrefixAction::Dispatch(ClientAction::OpenRightSidebar)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"t".to_vec()),
            PrefixAction::Dispatch(ClientAction::OpenTabBar)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"c".to_vec()),
            PrefixAction::Dispatch(ClientAction::CreateTab)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"C".to_vec()),
            PrefixAction::Dispatch(ClientAction::CreateWorkspace)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"l".to_vec()),
            PrefixAction::Dispatch(ClientAction::FocusPane(FocusDirection::Right))
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"o".to_vec()),
            PrefixAction::Dispatch(ClientAction::FocusNextPane)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"h".to_vec()),
            PrefixAction::Dispatch(ClientAction::FocusPane(FocusDirection::Left))
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b";".to_vec()),
            PrefixAction::Dispatch(ClientAction::FocusPreviousPane)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"k".to_vec()),
            PrefixAction::Dispatch(ClientAction::FocusPane(FocusDirection::Up))
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b":".to_vec()),
            PrefixAction::Dispatch(ClientAction::OpenCommandBar)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"\x1b[B".to_vec()),
            PrefixAction::Dispatch(ClientAction::FocusNextWorkspace)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"\x1b[A".to_vec()),
            PrefixAction::Dispatch(ClientAction::FocusPreviousWorkspace)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(vec![20]),
            PrefixAction::Dispatch(ClientAction::FocusLast(HistoryScope::Tab))
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(vec![23]),
            PrefixAction::Dispatch(ClientAction::FocusLast(HistoryScope::Workspace))
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(vec![19]),
            PrefixAction::Dispatch(ClientAction::FocusLast(HistoryScope::Session))
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(vec![2]),
            PrefixAction::Dispatch(ClientAction::FocusNextNotification)
        );
        assert_eq!(prefix.feed(vec![2]), PrefixAction::Wait);
        assert_eq!(
            prefix.feed(b"x".to_vec()),
            PrefixAction::Dispatch(ClientAction::ClosePane)
        );
    }
}
