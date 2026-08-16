---
id: al-iz60
status: closed
deps: []
links: []
created: 2026-08-16T18:25:09Z
type: task
priority: 1
assignee: Mikkel Malmberg
parent: al-hcfm
tags: [agents, domain, cli]
---
# Add borrowed pane ancestry traversal

Add one typed, borrowed traversal over panes and their session/workspace/tab ancestry, then use it to remove the stringly resource walk from the agent CLI.

## Design

Expose a PanePathRef-style iterator from ResourceSnapshot rather than modeling Agent as a new domain resource. Agent, notification, and future navigator projections filter this traversal. Map integrated panes into an explicit serializable CLI wire type; keep availability as CLI-derived automation semantics rather than shared row state.

## Acceptance Criteria

fut agent list/get and resolution no longer construct or search serde_json::Value internally; their versioned JSON shape remains unchanged under golden tests; traversal borrows snapshots without cloning token maps or pane collections.


## Notes

**2026-08-16T18:40:54Z**

Implemented PanePathRef and ResourceSnapshot::pane_paths; migrated agent list/get/read/resolution to typed borrowed wire structs while preserving JSON shape. Focused tests and golden serialization pass.
