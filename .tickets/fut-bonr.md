---
id: fut-bonr
status: closed
deps: [fut-52s9]
links: [fut-luqs]
created: 2026-08-10T06:09:42Z
type: task
priority: 2
assignee: Mikkel Malmberg
tags: [perf, e2e, terminal]
---
# Add Hollywood overload terminal stress journey

Exercise several simultaneous full-grid/high-output terminals, bounded control responsiveness, fair snapshot progress, and cleanup.

## Design

Use deterministic in-repo child fixtures rather than requiring Homebrew demos. Model matrix-like output, alternate-screen redraws, burst logs, and a descendant-spawning loop. Keep runtime bounded and diagnostics actionable.

## Acceptance Criteria

Stress test proves daemon ping/control latency remains bounded, ordinary sibling input/output progresses, all owned panes close, attachment remains possible, and no child processes or closing resources leak.


## Notes

**2026-08-10T06:20:36Z**

Added isolated_hollywood_overload_journey_stays_controllable_and_closes_cleanly plus mise run journey:overload. It drives matrix-like child bursts, alternate-screen redraws, and ANSI log streaming simultaneously; asserts bounded ping/read/close, sibling recovery, no closing resources, and default reattachment. Focused journey passes in 0.81s; full mise run check passes.
