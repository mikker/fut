//! Sets the clipboard of the terminal Fut runs in via OSC 52.
//!
//! Clipboard tools only reach the graphical session the client process lives
//! in, which fails for remote or nested attachments. OSC 52 hands the copy to
//! the outer terminal itself, which owns the desktop clipboard; terminals that
//! do not implement it ignore the sequence. It is a companion to the tool
//! copy, never a replacement: the tool result still decides the copy toast.

use std::io::{self, Write};

use base64::{Engine, engine::general_purpose::STANDARD};

/// Keeps one write well inside terminal clipboard budgets; larger copies rely
/// on the clipboard tools alone.
pub(super) const MAX_OSC52_BYTES: usize = 128 * 1024;

/// Writes the OSC 52 clipboard sequence for `text` to the host terminal.
/// Oversized text is skipped rather than truncated, so the selection is never
/// silently cut short.
pub(super) fn write(text: &str, writer: &mut impl Write) -> io::Result<()> {
    if text.len() > MAX_OSC52_BYTES {
        return Ok(());
    }
    writer.write_all(b"\x1b]52;c;")?;
    writer.write_all(STANDARD.encode(text).as_bytes())?;
    writer.write_all(b"\x1b\\")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_text_into_a_single_clipboard_selection() {
        let mut output = Vec::new();
        write("λ雪 clipboard", &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("\x1b]52;c;{}\x1b\\", STANDARD.encode("λ雪 clipboard"))
        );
    }

    #[test]
    fn oversized_text_is_skipped_without_a_partial_write() {
        let mut output = Vec::new();
        write(&"\0".repeat(MAX_OSC52_BYTES + 1), &mut output).unwrap();
        assert!(output.is_empty());
    }
}
