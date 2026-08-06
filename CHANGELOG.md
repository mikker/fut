# Changelog

## Unreleased

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
