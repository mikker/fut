Based on the crates.io source of Crossterm 0.29.0 (MIT).

Fut needs terminal color replies to travel through the existing input reader,
without a second reader consuming keyboard input. Local changes:

- Add `Event::TerminalResponse(Vec<u8>)` for OSC replies and CSI `?997` reports.
- Parse OSC replies through BEL or ST, with a 4096-byte bound, and color-scheme
  reports through their final `n`. Test replies split at every byte boundary.
- Remove example targets (examples are not vendored) and fix an existing
  unnecessary-parentheses warning exposed by the path dependency.

Cargo's crates.io patch ensures Ratatui and Fut use the same input implementation.

Vendored text uses LF line endings.
