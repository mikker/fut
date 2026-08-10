---
id: fut-dafg
status: closed
deps: [fut-svyh, fut-usn7, fut-cgr7, fut-o6nf]
links: []
created: 2026-08-09T20:47:48Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [agent, cli]
---
# Complete the lifecycle-aware agent command group

Add the semantic agent surface agreed for Fut: list, get, prompt, wait, read, and report.

## Design

An agent is an integrated process-bearing terminal, not a new ownership-tree resource. Resolve it by explicit target. prompt atomically submits text and Enter, captures an activity revision barrier, observes subsequent working activity, then waits for completed, blocked, or idle. read uses terminal observation but reports agent-specific availability and state. Default prompt rejects a busy target unless an explicit steering mode is designed.

## Acceptance Criteria

fut agent list/get identify only integrated agents; report replaces or deliberately aliases terminal report; prompt --wait cannot succeed from stale idle state; blocked is a successful outcome; waits handle timeouts and exits; read returns bounded output; all commands have stable JSON and end-to-end race coverage.


## Notes

**2026-08-09T21:43:45Z**

Implemented protocol v11 lifecycle-aware agent surface: agent list/get filter integrated terminal resources; prompt uses explicit terminal IDs, existing atomic Run input, revision barrier, bounded loss-detecting lifecycle broadcast, fresh Working-before-settled semantics, busy rejection, and typed timeout/exit/lag errors; wait returns current settled state or event-drives a working agent; read composes bounded terminal output with current lifecycle/availability; agent report is canonical with source/session/turn metadata and terminal report remains a compatibility alias. Added integrated-only completion, docs/help/changelog, protocol/unit coverage, and e2e coverage for back-to-back Working→Completed, stale-idle barrier, busy, blocked success, timeout, exit, bounded read, metadata, and discovery. Validation: cargo fmt; cargo clippy --all-targets -- -D warnings; 334 lib tests; 84 e2e tests; focused reruns green.
