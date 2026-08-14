---
id: lin-aabn
status: closed
deps: []
links: []
created: 2026-08-14T09:26:06Z
type: feature
priority: 2
assignee: Mikkel Malmberg
---
# Preserve OSC 8 hyperlinks through Fut

Carry explicit terminal hyperlinks through emulation, snapshots, deltas, and client rendering so coding-agent links remain clickable.

## Acceptance Criteria

OSC 8 link text renders unchanged and remains clickable through Fut; hyperlink data is bounded and protocol-safe; focused tests cover parsing and rendering.


## Notes

**2026-08-14T09:26:10Z**

Implemented bounded, deduplicated OSC 8 hyperlink transport through Ghostty snapshots and screen deltas, with contiguous-run rendering and a fresh debug-daemon task. Verified manually in Ghostty and with automated tests.
