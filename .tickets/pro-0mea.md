---
id: pro-0mea
status: closed
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


## Notes

**2026-08-18T20:08:02Z**

Implemented trusted project extension configuration, a generic session.created hook, opt-in wt existing-worktree discovery, and run auto_start for every new workspace. Removed wt's obsolete post-open command path. Verified with full mise check, extension smoke tests, docs build, and cargo build.
