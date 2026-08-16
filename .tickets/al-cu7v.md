---
id: al-cu7v
status: closed
deps: [al-n5nc]
links: []
created: 2026-08-16T18:25:10Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: al-hcfm
tags: [ui, agents, config, docs]
---
# Add configurable agent scopes and default composition

Finish the Agents component with configurable scope and a useful built-in left/right composition.

## Design

Support tab, workspace, session, and global scopes by filtering borrowed pane ancestry against the client's focused target. Keep explicit-integration presence as the initial inclusion rule; detection-only agents and attention-only filtering remain separate product decisions. Default Workspaces to the left and Agents to an auto-relevant right sidebar at session scope. Keep the Notifications dialog distinct initially.

## Acceptance Criteria

Each scope includes exactly the expected live integrated terminals; global rows switch directly across sessions through typed PaneId selection; closing ancestry is excluded; defaults consume no right-side width when no agents match; configuration, docs, examples, and user-facing changelog describe composition and scope semantics.


## Notes

**2026-08-16T20:03:54Z**

Added tab/workspace/session/global Agents scopes over fresh-or-selected ancestry, default automatic Workspaces-left and session-Agents-right composition, compact agent rails, cross-session typed navigation, documentation, examples, and changelog. Detection-only agents and arbitrary plugin UI remain out of scope.
