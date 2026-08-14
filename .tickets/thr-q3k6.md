---
id: thr-q3k6
status: closed
deps: [thr-sh31]
links: []
created: 2026-08-14T09:38:16Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: thr-tind
---
# Run bounded extension lifecycle hooks

Allow loaded extensions to declare direct-argv commands for a small stable set of committed Fut lifecycle events.

## Design

Hooks run asynchronously after state commits, cannot veto or rewrite the originating operation, receive relevant FUT resource context plus bounded JSON event data, and share one command runner for cwd/env/timeout/output/cancellation/trust behavior. Keep the initial event vocabulary demand-driven.

## Acceptance Criteria

A trusted extension can react to documented lifecycle events using packaged commands; slow or failing hooks cannot block or corrupt daemon state; execution and output are bounded; event ordering and process-exit behavior are tested; hooks add no actions or in-process callbacks.
