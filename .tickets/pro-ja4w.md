---
id: pro-ja4w
status: closed
deps: []
links: []
created: 2026-08-18T07:22:56Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: pro-pgup
---
# Apply trusted workspace recipes

Load global or .fut/project.toml recipes and atomically create initial tabs, panes, direct commands, cwd, environment, and split layouts for new project/worktree workspaces.

## Acceptance Criteria

Repository recipes require content trust; creation is all-or-nothing in daemon state; existing workspaces are never reconciled; linked worktrees receive the shared recipe.


## Notes

**2026-08-18T08:11:56Z**

Implemented strict versioned recipes with tabs, panes, indexed split topology, direct argv, cwd/env layering, focus, SHA-256 trust for repository files, trusted global recipe paths, atomic resource publication, spawn cleanup, linked-worktree reuse, and bootstrap support. Full check and build pass.

**2026-08-18T09:16:18Z**

Revision: move repository recipe digests out of dotfile configuration into Fut-managed XDG state, with Fut trust/untrust commands and future TUI approval using the same store.

**2026-08-18T09:39:54Z**

Moved repository recipe approval to Fut-managed machine-local XDG state. Added daemonless project trust/untrust commands; approvals bind canonical recipe paths to exact bytes, update atomically with 0600 state, apply without daemon restart, and fail closed when malformed.
