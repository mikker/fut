---
id: fut-vs29
status: closed
deps: [fut-dmq6]
links: []
created: 2026-08-09T20:48:19Z
type: task
priority: 1
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [agent, e2e]
---
# Add an end-to-end agent-native coordination journey

Prove the complete public workflow through the real binary and daemon rather than isolated command tests.

## Design

Build a deterministic fake integrated agent process that reports lifecycle events and responds through its PTY. Drive Fut only through public pane, terminal, and agent CLI commands. Cover compact context, split, raw command run/read/wait-output, prompt revision barriers, completed and blocked outcomes, result reads, terminal exit, and cleanup.

## Acceptance Criteria

One isolated journey exercises the full documented skill workflow with no sleeps used as synchronization, no visual-focus assumptions, no screen-scraped lifecycle inference, and bounded failure diagnostics.


## Notes

**2026-08-09T21:54:52Z**

Added a deterministic public-CLI coordination journey covering context discovery, no-focus split/launch, raw run/read/wait-output, integrated agent discovery, fresh prompt revision barriers, completed and blocked outcomes, bounded result reads, explicit cleanup, and typed terminal exit. The journey uses no sleeps or visual-focus/screen-scraped lifecycle inference. Focused e2e passes.
