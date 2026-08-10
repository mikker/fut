---
id: fut-m59z
status: closed
deps: []
links: [fut-0425]
created: 2026-08-09T20:47:10Z
type: epic
priority: 1
assignee: Mikkel Malmberg
tags: [agent, cli]
---
# Agent-native CLI control surface

Make Fut natural, efficient, and discoverable for coding agents through a stable CLI and bundled skill. Keep pane commands about layout, terminal commands about raw process interaction, and agent commands about lifecycle-aware interaction. Agent-specific integrations only report identity and lifecycle feedback.

## Design

Public command boundary: pane owns layout; terminal owns raw process I/O; agent owns prompt and lifecycle semantics. Prefer explicit targets, versioned JSON, event-driven waits, atomic submission, and no dependence on visual focus or screen-scraped lifecycle inference.

## Acceptance Criteria

An agent can inspect context, create suitable layout, run and observe ordinary commands, launch and prompt an integrated coding agent, wait for completion or blocking without races, read results, and discover the workflow from fut agent skill.


## Notes

**2026-08-09T21:54:52Z**

All child work is complete: stable pane/terminal/agent CLI layers, explicit lifecycle state, bounded event-driven output observation, Pi/Codex/Claude lifecycle adapters, printable validated skill, and a full public-CLI coordination journey.
