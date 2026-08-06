# Changelog

## Unreleased

- Added agent activity indicators, per-client completion notifications, waiting-terminal navigation, and Pi integration.
- Added mouse-wheel scrollback, pane click focus, and application mouse-wheel forwarding.
- Added mode-aware terminal paste with bracketed-paste handling and Ghostty control-byte sanitization.
- Added per-client copy mode with ordered keyboard selection, scrollback search, and retry-safe macOS clipboard copying.
- Fixed input loss under terminal queue pressure and stale mouse or viewport events disconnecting clients during pane transitions.
- Added exact protocol compatibility checks, with shutdown support for Fut 0.1 daemons.

## 0.1

- Released persistent, configurable terminal multiplexing for macOS.
- Automated GitHub builds, releases, and Homebrew distribution.
