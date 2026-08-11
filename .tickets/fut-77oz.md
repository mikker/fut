---
id: fut-77oz
status: closed
deps: []
links: []
created: 2026-08-11T20:42:24Z
type: feature
priority: 2
assignee: Mikkel Malmberg
---
# Make navigator parent selection history-aware

Keep sessions, workspaces, and tabs selectable in fuzzy navigator results while resolving them to the most recently focused pane in their subtree. Collapse descendants that match only through an already-matching ancestor.

## Acceptance Criteria

Parent selections use remembered focused panes with an open-pane fallback; fuzzy results omit inherited-only descendant duplicates while preserving direct matches; navigator tests and full checks pass.


## Notes

**2026-08-11T20:42:24Z**

Implemented navigator destinations through NavigationHistory, minimal fuzzy-result collapsing, stale/closed destination fallback coverage, and focused unit tests. Refactor pass consolidated navigator construction and extracted fuzzy result shaping. RUST_TEST_THREADS=1 mise run check passes: 382 unit tests and 92 E2E tests.
