---
id: fut-3qvz
status: closed
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

## Notes

**2026-08-26T07:01:28Z**

Implemented the focused B-10 application-cursor-mode correction: interactive and temporary-command arrow events now retain structured identity through the terminal runtime and use current DECCKM state, with unit and end-to-end coverage. This does not complete the remaining enhanced-keyboard acceptance criteria; keep this ticket open for modifier protocols, key disambiguation, and press/repeat/release support.

**2026-08-26T09:45:13Z**

Completed the remaining extended-keyboard slice: capable direct hosts negotiate Crossterm/Kitty enhancement flags; all supported interactive and send-keys input now crosses the client/daemon boundary as structured identity, text, modifiers, and action; daemon-owned libghostty-vt encoding honors legacy, DECCKM, modifyOtherKeys, and Kitty modes. Added legacy/mode unit coverage and a PTY E2E covering negotiation, Ctrl-I vs Tab, Ctrl-M vs Enter, modified navigation, repeat, and release. Updated docs/usage.md and CHANGELOG.md. Validation: cargo fmt -- --check; cargo test --lib (595 passed); focused E2E tests public_enhanced_keyboard_preserves_disambiguation_modifiers_and_event_types, public_arrow_keys_use_the_daemon_terminals_current_cursor_mode, public_terminal_input_is_literal_validated_bracket_aware_atomic_and_exit_typed, and public_wheel_uses_dec_alternate_scroll_and_terminal_cursor_key_mode (all passed); cargo build passed; git diff --check passed.

**2026-08-26T10:21:45Z**

Refactor pass removed the legacy Alt post-encoding repair and configured libghostty-vt's Option/Alt behavior directly, leaving one encoder path. Manual isolated-daemon validation passed for structured terminal send-keys in legacy mode, modifyOtherKeys mode 2, and Kitty flags 11; each probe matched exact expected bytes (including Unicode, Ctrl/Alt, navigation, F keys, Ctrl-I vs Tab, Ctrl-M vs Enter, and modified navigation). Final validation: cargo fmt -- --check; cargo test --lib (595 passed); four focused PTY E2E tests passed; cargo build; git diff --check.
