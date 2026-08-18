---
id: de-rf4s
status: open
deps: [de-s1an, de-vhs0]
links: []
created: 2026-08-18T19:55:10Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: de-8pyh
tags: [extensions, reload]
---
# Atomically reload daemon extensions

Allow the running daemon to validate and activate a changed extension set without restarting sessions or terminals.

## Design

Add a daemon control operation that builds a complete candidate registry and swaps it only on success. Preserve the previous registry on every read, parse, validation, or compatibility failure. Bind each committed lifecycle event to the registry snapshot active at commit time so newly added extensions do not receive old queued events and removed extensions do not unpredictably process later events. In-flight bounded processes may finish; future dispatch uses the new generation.

## Acceptance Criteria

Adding, changing, and removing hooks and token declarations takes effect without daemon restart; failed reloads leave the previous generation fully active; event behavior across reload is deterministic under queue backlog; reload cannot block resource commits; process-level tests cover successful reload, rollback, removal, and in-flight hooks.

