---
id: fut-3qvz
status: open
deps: []
links: []
created: 2026-08-21T09:02:08Z
type: feature
priority: 2
assignee: Mikkel Malmberg
tags: [terminal, input]
---
# Support mode-aware extended keyboard input

Fut currently converts Crossterm key events to fixed legacy byte sequences in src/client/input.rs. This loses modifiers on keys such as Ctrl/Shift+Arrow, cannot distinguish combinations such as Ctrl+I from Tab or Ctrl+M from Enter, discards release events, and does not honor application cursor mode, modifyOtherKeys, or the Kitty keyboard protocol. Implement end-to-end enhanced keyboard handling while retaining a safe legacy fallback.

## Design

Negotiate enhanced keyboard reporting with capable host terminals so Fut receives unambiguous key events. Preserve structured key identity, text, modifiers, and press/repeat/release until the daemon-owned terminal state can encode the event according to the focused application's requested modes. Prefer the pinned libghostty-vt mode-aware key encoder rather than duplicating terminal encoding rules. Keep Fut prefix/binding interception client-local, and route unconsumed events through the structured path. Make terminal send-keys use the same semantics where practical. Do not break ordinary text, Unicode, traditional Ctrl/Alt input, bracketed paste, or legacy hosts.

## Acceptance Criteria

- Ordinary text, Unicode, Enter, Tab, Escape, Backspace, arrows, navigation keys, F1-F12, traditional Ctrl keys, and Alt+character continue to work on legacy hosts.
- Application cursor-key mode is honored instead of always emitting fixed CSI arrow sequences.
- Ctrl/Shift/Alt-modified navigation keys retain their modifiers.
- Applications requesting modifyOtherKeys or Kitty keyboard protocol receive the appropriate mode-aware encoding.
- Distinct combinations such as Ctrl+I versus Tab and Ctrl+M versus Enter remain distinct when the host terminal supports enhanced reporting.
- Press, repeat, and release semantics are forwarded when requested and supported, with safe fallback otherwise.
- Focused unit tests cover mode negotiation and encoding, and an end-to-end PTY test verifies representative legacy and enhanced-keyboard behavior.
- User-facing terminal capability documentation is updated.
