---
id: pro-l88r
status: closed
deps: []
links: []
created: 2026-08-18T07:22:07Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: pro-pgup
---
# Add explicit project catalog

Declare named project roots in global config and open them by name, including verified linked-worktree path overrides.

## Acceptance Criteria

[projects.NAME] accepts absolute and ~/ paths; fut open --project resolves names; optional paths must share project identity; completion and docs cover the feature; checks pass.

