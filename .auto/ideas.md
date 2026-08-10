# Deferred ideas

- Add an honest retained-buffer client-render benchmark and design only if
  profiling confirms `Screen::render` is still a dominant product cost; do not
  alter the current metric mid-session.
- Port Herdr's libghostty dirty-row extraction concept: keep the canonical
  `ScreenSnapshot` as retained runtime state, patch only Ghostty `Dirty::Partial`
  rows after VT writes, and fall back to a full rebuild on `Dirty::Full`, resize,
  selection/copy-mode, viewport, or uncertain state. This directly attacks
  Fut's remaining full-grid snapshot cost without benchmark-specific behavior.
  Do not introduce Arc rows by themselves: run 24 showed their per-row
  allocation and nested serde overhead regress full-grid workloads by 10%.
  Revisit row sharing only atomically with dirty-row publication and validate
  against a partial-update production workload.
- Evaluate a server-side retained composed frame/direct-ANSI client mode after
  dirty snapshot capture. Herdr and tmux avoid Fut's duplicated semantic-grid
  materialization plus client ratatui full-buffer rebuild, but this is a larger
  boundary redesign and should follow measurement of the dirty-row win.
