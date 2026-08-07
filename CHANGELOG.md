# Changelog

## Unreleased

- Tabs are now created unnamed and the tab bar shows each tab's name beside its number.
- Restyled the workspace sidebar: wider rows, a bullet marker for the current workspace, underline-free selection, and a Git branch and diff summary under each workspace.
- Added a jump dialog (`Ctrl-b f`): type to filter all sessions, workspaces, tabs, and panes, Enter switches.
- Added `Ctrl-b ↑/↓` to cycle workspaces, digit keys in the workspace sidebar to pick one directly, and `h` there to toggle sidebar auto-hide.
- Added the current workspace name to the tab bar's right side.
- Added `fut tab list` and `fut pane list` for inspecting tabs, panes, and split layouts, with `--json` support.
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
