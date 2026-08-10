---
id: fut-u996
status: closed
deps: []
links: []
created: 2026-08-09T20:47:36Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [pane, cli]
---
# Expose pane split through the public CLI

Expose the daemon's existing split-pane capability so agents can create sibling layout without attaching.

## Design

Add fut pane split with an explicit anchor pane, right/down direction, optional cwd, and direct child argv after --. Return the created pane and terminal IDs as versioned JSON. A CLI control command must not change any interactive client's focus.

## Acceptance Criteria

Agents can split an explicit pane right or down, preserve cwd and argv exactly, receive created IDs, and observe the authored layout; failures are pre-spawn and atomic.


## Notes

**2026-08-09T21:02:46Z**

Implemented fut pane split <PANE_ID> <right|down> [--cwd DIR] [-- <COMMAND>...]. Control-mode split returns pane.split versioned JSON with anchor, direction, and selected IDs; daemon behavior preserves interactive focus and validates resource/layout/cwd before spawn with atomic spawn failure. Added CLI parsing/help coverage, delimiter coverage, and an e2e covering cwd/argv, both layout directions, focus stability, JSON IDs, and atomic not-found/invalid-cwd/spawn failures. Verified focused unit/e2e tests and cargo clippy --all-targets -- -D warnings.
