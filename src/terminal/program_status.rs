//! Program Status Protocol (OSC 7501) reports observed in PTY output.
//!
//! Programs announce their own state through the pty, so Fut sees it locally,
//! over SSH, and inside containers alike. Ghostty ignores the sequence; this
//! scanner reads it from the same bytes before they reach the emulator.
//!
//! Fut's activity model describes one state per terminal, so only the root
//! record is applied. Reports for child records are ignored.

use base64::Engine as _;

const MAX_SEQUENCE_BYTES: usize = 4096;
const MAX_KEY_BYTES: usize = 16;
const MAX_MSG_ENCODED_BYTES: usize = 2732;
const MAX_MSG_DECODED_BYTES: usize = 2048;
const MAX_TITLE_ENCODED_BYTES: usize = 256;
const MAX_TITLE_DECODED_BYTES: usize = 192;
const MAX_APP_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Clear,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramStatus {
    pub state: ProgramState,
    pub app: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Observation {
    /// A valid report addressing the root record.
    Report(ProgramStatus),
    /// `OSC 7501 ; ?`, answered with the same body and terminator.
    Query { bel: bool },
    /// RIS removes every record.
    FullReset,
}

#[derive(Debug, Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
}

/// Incremental scanner tolerant of sequences split across PTY reads.
#[derive(Debug, Default)]
pub(super) struct Scanner {
    state: ScanState,
    body: Vec<u8>,
    overflow: bool,
}

impl Scanner {
    /// Return each completed observation with the chunk offset just past it.
    pub(super) fn scan(&mut self, bytes: &[u8]) -> Vec<(usize, Observation)> {
        let mut observations = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if matches!(self.state, ScanState::Ground) {
                match bytes[index..].iter().position(|&byte| byte == 0x1b) {
                    Some(offset) => {
                        index += offset + 1;
                        self.state = ScanState::Escape;
                    }
                    None => break,
                }
                continue;
            }
            let byte = bytes[index];
            index += 1;
            match self.state {
                ScanState::Ground => unreachable!(),
                ScanState::Escape => {
                    self.state = match byte {
                        b']' => {
                            self.body.clear();
                            self.overflow = false;
                            ScanState::Osc
                        }
                        b'c' => {
                            observations.push((index, Observation::FullReset));
                            ScanState::Ground
                        }
                        0x1b => ScanState::Escape,
                        _ => ScanState::Ground,
                    }
                }
                ScanState::Osc => match byte {
                    0x07 => self.finish(index, true, &mut observations),
                    0x1b => self.state = ScanState::OscEscape,
                    0x18 | 0x1a => self.state = ScanState::Ground,
                    _ => self.push(byte),
                },
                ScanState::OscEscape => {
                    if byte == b'\\' {
                        self.finish(index, false, &mut observations);
                    } else {
                        // Any other escape aborts the OSC and begins anew.
                        self.state = ScanState::Escape;
                        index -= 1;
                    }
                }
            }
        }
        observations
    }

    fn push(&mut self, byte: u8) {
        if self.overflow {
            return;
        }
        // Only OSC 7501 bodies are retained; other OSCs are skipped cheaply.
        let prefix = b"7501;";
        if self.body.len() < prefix.len() && byte != prefix[self.body.len()] {
            self.overflow = true;
            return;
        }
        // OSC, terminator and the body together must fit the sequence limit.
        if self.body.len() + 4 >= MAX_SEQUENCE_BYTES {
            self.overflow = true;
            return;
        }
        self.body.push(byte);
    }

    fn finish(&mut self, end: usize, bel: bool, observations: &mut Vec<(usize, Observation)>) {
        self.state = ScanState::Ground;
        if self.overflow {
            return;
        }
        let Some(body) = self.body.strip_prefix(b"7501;") else {
            return;
        };
        if body.trim_ascii() == b"?" {
            observations.push((end, Observation::Query { bel }));
        } else if let Some(status) = parse_root_report(body) {
            observations.push((end, Observation::Report(status)));
        }
    }
}

/// Parse a report body, returning the status only for the root record.
fn parse_root_report(body: &[u8]) -> Option<ProgramStatus> {
    let body = std::str::from_utf8(body).ok()?;
    let mut state = None;
    let mut id = None;
    let mut app = None;
    for pair in body.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key == "id" {
            // Any id, even a malformed one, keeps the report off the root.
            id = Some(value);
        }
        if key.is_empty()
            || key.len() > MAX_KEY_BYTES
            || !key.bytes().all(|byte| byte.is_ascii_lowercase())
            || !value.bytes().all(is_value_byte)
        {
            continue;
        }
        match key {
            "state" => state = Some(value),
            "app" => app = Some(value),
            // Text is not shown yet, but invalid text still discards the report.
            "msg" if !valid_text(value, MAX_MSG_ENCODED_BYTES, MAX_MSG_DECODED_BYTES) => {
                return None;
            }
            "title" if !valid_text(value, MAX_TITLE_ENCODED_BYTES, MAX_TITLE_DECODED_BYTES) => {
                return None;
            }
            _ => {}
        }
    }
    let state = match state? {
        "idle" => ProgramState::Idle,
        "working" => ProgramState::Working,
        "done" => ProgramState::Done,
        "blocked" => ProgramState::Blocked,
        "error" => ProgramState::Error,
        "clear" => ProgramState::Clear,
        _ => return None,
    };
    // Child records, and malformed ids that must not fall back to the root.
    if id.is_some() {
        return None;
    }
    let app = app
        .filter(|app| {
            !app.is_empty() && app.len() <= MAX_APP_BYTES && app.bytes().all(is_name_byte)
        })
        .map(str::to_owned);
    Some(ProgramStatus { state, app })
}

fn is_value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"_.,+/=-".contains(&byte)
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte)
}

/// Base64 UTF-8 text within the limits and free of control characters.
fn valid_text(value: &str, max_encoded: usize, max_decoded: usize) -> bool {
    if value.len() > max_encoded {
        return false;
    }
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(value.trim_end_matches('='))
        .ok()
        .filter(|decoded| decoded.len() <= max_decoded)
        .and_then(|decoded| String::from_utf8(decoded).ok())
        .is_some_and(|text| !text.chars().any(char::is_control))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<Observation> {
        let mut scanner = Scanner::default();
        chunks
            .iter()
            .flat_map(|chunk| scanner.scan(chunk))
            .map(|(_, observation)| observation)
            .collect()
    }

    fn report(state: ProgramState, app: Option<&str>) -> Observation {
        Observation::Report(ProgramStatus {
            state,
            app: app.map(str::to_owned),
        })
    }

    #[test]
    fn parses_the_specification_example() {
        let observations = scan(&[
            b"\x1b]7501;state=blocked:kind=permission:app=terraform:msg=QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/\x1b\\",
        ]);
        assert_eq!(
            observations,
            [report(ProgramState::Blocked, Some("terraform"))]
        );
    }

    #[test]
    fn reassembles_reports_split_across_reads() {
        let bytes = b"out\x1b]7501;state=working:app=cargo\x07more\x1b]7501;state=done\x1b\\";
        let whole = scan(&[bytes]);
        for split in 1..bytes.len() {
            assert_eq!(scan(&[&bytes[..split], &bytes[split..]]), whole, "{split}");
        }
        assert_eq!(
            whole,
            [
                report(ProgramState::Working, Some("cargo")),
                report(ProgramState::Done, None),
            ]
        );
    }

    #[test]
    fn records_where_each_observation_ends() {
        assert_eq!(
            Scanner::default().scan(b"a\x1b]7501;?\x07\x1b[c"),
            [(10, Observation::Query { bel: true })]
        );
    }

    #[test]
    fn answers_queries_with_the_request_terminator() {
        assert_eq!(
            scan(&[b"\x1b]7501;?\x07\x1b]7501;?\x1b\\"]),
            [
                Observation::Query { bel: true },
                Observation::Query { bel: false }
            ]
        );
    }

    #[test]
    fn ignores_child_records_and_unknown_states() {
        assert!(
            scan(&[
                b"\x1b]7501;state=working:id=us-east\x07",
                b"\x1b]7501;state=paused\x07",
                b"\x1b]7501;app=cargo\x07",
                b"\x1b]7501;state=working:id=bad id\x07",
            ])
            .is_empty()
        );
    }

    #[test]
    fn skips_malformed_pairs_but_discards_bad_text() {
        assert_eq!(
            scan(&[b"\x1b]7501;junk: state = idle :app=bad app:x=\x07"]),
            [report(ProgramState::Idle, None)]
        );
        // Base64 of "a\nb" decodes to a control character.
        assert!(scan(&[b"\x1b]7501;state=done:msg=YQpi\x07"]).is_empty());
        assert!(scan(&[b"\x1b]7501;state=done:msg=a\x07"]).is_empty());
    }

    #[test]
    fn discards_oversized_sequences() {
        let mut bytes = b"\x1b]7501;state=working:pad=".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_SEQUENCE_BYTES));
        bytes.push(0x07);
        assert!(scan(&[&bytes]).is_empty());
    }

    #[test]
    fn other_sequences_neither_match_nor_disrupt_scanning() {
        assert_eq!(
            scan(&[b"\x1b]2;7501;state=idle\x07\x1b]75\x1b[0m\x1b]7501;state=idle\x07\x1bc"]),
            [report(ProgramState::Idle, None), Observation::FullReset]
        );
    }
}
