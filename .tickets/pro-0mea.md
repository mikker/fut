---
id: pro-0mea
status: open
deps: [pro-ja4w]
links: []
created: 2026-08-18T07:22:57Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: pro-pgup
---
# Auto-open worktrees from the wt extension

Let the wt extension optionally enumerate existing Git worktrees and open missing ones through Fut's generic project/workspace recipe operation.

## Acceptance Criteria

Disabled by default; configured wt extension opens existing worktrees idempotently; core Fut retains neutral workspace semantics; no implicit worktree creation or deletion.

