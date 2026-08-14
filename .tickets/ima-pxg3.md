---
id: ima-pxg3
status: closed
deps: []
links: []
created: 2026-08-14T13:28:42Z
type: feature
priority: 2
assignee: Mikkel Malmberg
---
# Render inline images through Fut

Carry Kitty graphics across Fut's multiplexer boundary and render static inline images in supported host terminals.

## Acceptance Criteria

Inline Kitty image streams render in Ghostty, Kitty, and WezTerm; placements follow pane layout and clipping; image state survives redraws without redundant retransmission; protocol and graphics paths are tested.


## Notes

**2026-08-14T13:28:42Z**

Implemented Kitty graphics capture, bounded PNG transport, snapshot deltas, host-side upload/placement/clipping/cache cleanup, and fallback pixel geometry. Fixed the vendored PNG decoder buffer sizing and expanded-format handling. Manually verified Chafa rendering in Ghostty. Refactor pass removed redundant wire metadata and suppresses unchanged host placement commands. Verified formatting, clippy with warnings denied, 403 unit tests, and 93 serial E2E tests.
