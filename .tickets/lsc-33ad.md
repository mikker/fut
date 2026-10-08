---
id: lsc-33ad
status: open
deps: []
links: []
created: 2026-10-08T02:41:43Z
type: task
priority: 3
---
# Clamp requested size for remote daemons without large-screens.v1

A client attached through `fut --machine` to a daemon that did not select `large-screens.v1` still requests its full host size, which that daemon rejects above 50,000 cells (`invalid_size` / invalid handshake).

## Design

When the negotiated capabilities lack large-screens.v1, clamp the hello and resize sizes to LEGACY_MAX_VISIBLE_CELLS (preferring to shrink rows and columns proportionally) and show the constrained-size gutter as for shared sizes.

## Acceptance Criteria

Attaching from a large host terminal to an older remote daemon succeeds with a smaller grid instead of failing.
