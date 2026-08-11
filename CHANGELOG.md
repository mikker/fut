# Changelog

## Unreleased

- Fixed multi-second backward paging latency in delta/less when running a development build.

- Development builds now identify themselves with a `-dev` suffix in `fut -v` output.
- Fixed workspace Git branch and diff metadata remaining stale while the UI is idle.
- Combined resource navigation and fuzzy search in the global navigator; `Ctrl-b f` and the `open_jump` binding have been removed.
- Command search now uses fuzzy matching, and unnamed navigator tabs display positional names.

## 0.3

- Added `-v` as the short form of `--version` (`-V` remains supported).
- Pane and workspace-sidebar dividers can now be resized cell by cell with the mouse; pane splits persist and synchronize, while sidebar widths remain client-local.
- Layout commands can now omit their current session, workspace, tab, or pane ID when run inside Fut.
- Added `fut attach` to choose a destination in the global navigator before acquiring a terminal attachment.
- Terminal applications can now select block, bar, or underline cursors, including blinking styles.
- Added atomic in-client configuration reload with `Ctrl-b Shift-R`.
- `Ctrl-b Ctrl-b` now cycles to the next unread waiting terminal by default and remains configurable.
- Added `fut agent skill` to print Fut's bundled agent instructions.
- Added `fut pane split` for creating right or downward splits without attaching.
- Added `fut context` and `fut get` for compact, focus-independent resource discovery.
- Added `fut terminal send-text`, `send-keys`, and `run` for explicit terminal input.
- Added bounded terminal output reads and event-driven literal or regex waits.
- Added lifecycle-aware `fut agent` discovery, prompting, waiting, reading, and reporting commands.
- Added a lifecycle-only Claude Code plugin for reporting agent activity to Fut.
- Updated the Pi integration to report ordered lifecycle state through `fut agent report`.
- Agent activity snapshots now distinguish integrated terminals and retain the latest lifecycle event.
- Improved animated-terminal performance with more compact cells, cheaper snapshots, and batched Ghostty field reads.
- Fixed panes under heavy output getting stuck while closing and blocking attachment.

## 0.2

- Added a jump dialog (`Ctrl-b f`): type to filter all sessions, workspaces, tabs, and panes, Enter switches.
- Fixed heavy terminal output making the whole UI lag or hang (e.g. animated TUIs like amp's orb): the daemon now batches PTY output and publishes at most one screen frame per 8 ms per terminal, instead of rebuilding and shipping the full screen for every kilobyte read.
- Switched the client/daemon wire protocol to MessagePack with a compact cell-style encoding (protocol 5), making heavily styled frames ~4x smaller and ~5x faster to decode.
- Faster screen-snapshot construction in the daemon and cheaper styled-cell drawing in the client.
- Added a render-performance harness: `mise run perf:bench` (microbenchmarks incl. a vtebench-style dense-cells workload), `mise run perf:e2e` (headless flood, styled, dense, and latency scenarios against a disposable daemon), and `FUT_PERF_LOG` client frame logging with `scripts/perf/report`. Findings live in `PERF.md`.
- Tabs are now created unnamed and the tab bar shows each tab's name beside its number.
- Restyled the workspace sidebar: wider rows, a bullet marker for the current workspace, underline-free selection, and a Git branch and diff summary under each workspace.
- Added `Ctrl-b ↑/↓` to cycle workspaces, digit keys in the workspace sidebar to pick one directly, and `h` there to toggle sidebar auto-hide.
- Added the current workspace name to the tab bar's right side.
- Added `fut tab list` and `fut pane list` for inspecting tabs, panes, and split layouts, with `--json` support.
- Added `fut events`: subscribe to state changes as a stream of versioned JSON lines, one full resource snapshot now and after every change.
- Added a pill-shaped focused tab when the `nerd_font` icon preset is active.
- Added agent activity indicators, per-client completion notifications, waiting-terminal navigation, and Pi integration.
- Added per-client mouse-wheel scrollback, click-to-focus, focused-application mouse reporting, alternate-screen wheel translation, and safe held-button cleanup across UI transitions.
- Added mode-aware terminal paste with bracketed-paste handling and Ghostty control-byte sanitization.
- Added per-client copy mode with ordered keyboard selection, scrollback search, and retry-safe macOS clipboard copying.
- Fixed input loss under terminal queue pressure and stale mouse or viewport events disconnecting clients during pane transitions.
- Fixed idle terminals republishing their screen 35 times a second, which kept every attached client redrawing.
- Added exact protocol compatibility checks, with shutdown support for Fut 0.1 daemons.

## 0.1

- Released persistent, configurable terminal multiplexing for macOS.
- Automated GitHub builds, releases, and Homebrew distribution.
