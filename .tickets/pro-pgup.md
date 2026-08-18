---
id: pro-pgup
status: in_progress
deps: []
links: []
created: 2026-08-18T07:10:31Z
type: feature
priority: 1
assignee: Mikkel Malmberg
---
# Add project definitions and workspace recipes

Add an explicit global project catalog and trusted project recipes that create a project's initial tabs, panes, layouts, commands, environment, and working directories. Keep workspace opening idempotent and expose generic primitives for the wt extension to auto-open Git worktrees later.

## Design

Global config declares [projects.NAME] with a root path and optional recipe path. The default repository recipe is .fut/project.toml. Repository-owned executable recipes require content-based trust before execution. Opening a linked worktree applies the shared workspace recipe; Git enumeration remains the wt extension's responsibility.

## Acceptance Criteria

Configured projects resolve by name; a new project/worktree applies its recipe; existing live workspaces are reused without rerunning commands; untrusted repository config cannot execute; direct argv/cwd/env and split layouts are validated; docs and changelog are updated; full build/test passes.


## Notes

**2026-08-18T07:22:35Z**

Started with the catalog foundation: strict [projects.NAME] paths, CLI open-by-name, linked-worktree identity validation, and completion. Recipes, trust, and the in-client picker remain on the parent feature.
