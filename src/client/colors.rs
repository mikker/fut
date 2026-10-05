//! Host appearance discovery shares Crossterm's input reader with keyboard input.
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

use crate::domain::{Rgb, TerminalColors, TerminalId};

#[derive(Default)]
pub(super) struct HostColors {
    colors: TerminalColors,
    changed: Option<Instant>,
    delivered: Option<(TerminalId, TerminalColors)>,
}

impl HostColors {
    pub fn start(writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(b"\x1b[?2031h")?;
        Self::query(writer)
    }

    fn query(writer: &mut impl Write) -> io::Result<()> {
        // Unknown colors stay unknown. Replies may arrive after startup or over SSH.
        writer.write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\")?;
        for index in 0..16 {
            write!(writer, "\x1b]4;{index};?\x1b\\")?;
        }
        writer.flush()
    }

    pub fn receive(&mut self, response: &[u8], writer: &mut impl Write) -> io::Result<()> {
        if response == b"\x1b[?997;1n" || response == b"\x1b[?997;2n" {
            return Self::query(writer);
        }
        let Some((slot, color)) = parse_color(response) else {
            return Ok(());
        };
        let target = match slot {
            ColorSlot::Foreground => &mut self.colors.foreground,
            ColorSlot::Background => &mut self.colors.background,
            ColorSlot::Palette(index) => &mut self.colors.palette[index],
        };
        if *target != Some(color) {
            *target = Some(color);
            self.changed = Some(Instant::now());
        }
        Ok(())
    }

    pub fn reattach(&mut self) {
        self.delivered = None;
    }

    pub fn update(&mut self, terminal: TerminalId) -> Option<TerminalColors> {
        if self.colors == TerminalColors::default()
            || self
                .changed
                .is_some_and(|at| at.elapsed() < Duration::from_millis(30))
            || self.delivered == Some((terminal, self.colors))
        {
            return None;
        }
        self.changed = None;
        self.delivered = Some((terminal, self.colors));
        Some(self.colors)
    }
}

#[derive(Debug, PartialEq)]
enum ColorSlot {
    Foreground,
    Background,
    Palette(usize),
}

fn parse_color(response: &[u8]) -> Option<(ColorSlot, Rgb)> {
    let body = response.strip_prefix(b"\x1b]")?;
    let body = body
        .strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(b"\x07"))?;
    let mut fields = std::str::from_utf8(body).ok()?.split(';');
    let slot = match fields.next()? {
        "10" => ColorSlot::Foreground,
        "11" => ColorSlot::Background,
        "4" => {
            let index = fields.next()?.parse::<usize>().ok()?;
            if index >= 16 {
                return None;
            }
            ColorSlot::Palette(index)
        }
        _ => return None,
    };
    let mut components = fields.next()?.strip_prefix("rgb:")?.split('/');
    let mut component = || {
        let hex = components.next()?;
        if hex.is_empty() || hex.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len())) - 1;
        Some(((value * 255 + max / 2) / max) as u8)
    };
    let color = Rgb {
        red: component()?,
        green: component()?,
        blue: component()?,
    };
    if components.next().is_some() || fields.next().is_some() {
        return None;
    }
    Some((slot, color))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_colors_for_pane_changes_and_batches_late_replies() {
        let mut host = HostColors::default();
        let first = TerminalId::new();
        let second = TerminalId::new();
        assert_eq!(host.update(first), None);
        host.receive(b"\x1b]11;rgb:ff/ff/ff\x07", &mut Vec::new())
            .unwrap();
        assert_eq!(
            host.update(first),
            None,
            "wait briefly for the remaining palette replies"
        );
        host.changed = Some(Instant::now() - Duration::from_millis(50));
        let colors = host.update(first).unwrap();
        assert_eq!(host.update(first), None);
        assert_eq!(host.update(second), Some(colors));
        assert_eq!(host.update(second), None);
        host.reattach();
        assert_eq!(host.update(second), Some(colors));
        host.receive(b"\x1b]10;rgb:11/22/33\x07", &mut Vec::new())
            .unwrap();
        host.changed = Some(Instant::now() - Duration::from_millis(50));
        assert!(host.update(second).unwrap().foreground.is_some());
        let mut query = Vec::new();
        host.receive(b"\x1b[?997;1n", &mut query).unwrap();
        assert!(query.windows(6).any(|part| part == b"10;?\x1b\\"));
    }

    #[test]
    fn reads_x11_colors_with_both_terminators_and_component_widths() {
        let white = Rgb {
            red: 255,
            green: 255,
            blue: 255,
        };
        for response in [
            b"\x1b]11;rgb:f/f/f\x07".as_slice(),
            b"\x1b]11;rgb:ff/ff/ff\x1b\\",
            b"\x1b]11;rgb:ffff/ffff/ffff\x07",
        ] {
            assert_eq!(parse_color(response), Some((ColorSlot::Background, white)));
        }
        assert_eq!(
            parse_color(b"\x1b]4;3;rgb:0000/8080/ffff\x07"),
            Some((
                ColorSlot::Palette(3),
                Rgb {
                    red: 0,
                    green: 128,
                    blue: 255
                }
            ))
        );
        for malformed in [
            b"\x1b]4;256;rgb:ff/ff/ff\x07".as_slice(),
            b"\x1b]11;rgb:fffzz/0/0\x07",
            b"\x1b]11;rgb:0/0/0/0\x07",
        ] {
            assert_eq!(parse_color(malformed), None);
        }
    }
}
