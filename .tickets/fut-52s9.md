---
id: fut-52s9
status: closed
deps: []
links: []
created: 2026-08-10T06:09:42Z
type: bug
priority: 1
assignee: Mikkel Malmberg
tags: [daemon, terminal, agent]
---
# Closing descendant process can poison attachment indefinitely

A pane running a shell that repeatedly launches a high-output child can remain closing forever after pane close. The stale closing pane remains in the resource tree and causes bare or session attachment to fail with target_closing.

## Design

Reproduce with an isolated process tree and close request. Fix terminal runtime/process-group finalization at the authoritative lifecycle boundary; do not make attachment ignore closing resources.

## Acceptance Criteria

Close terminates the complete owned child process group within a bound, finalizes the pane exactly once, preserves sibling terminals, and default attachment remains usable. Regression covers stubborn descendants and output saturation.


## Notes

**2026-08-10T06:20:36Z**

Live daemon sample confirmed the close runtime blocked in wait4 while its PTY reader blocked sending to the full bounded output queue. Replaced blocking wait with a bounded try_wait loop that drains/batches PTY output; added a separate end-to-end close acknowledgement deadline so two-phase resource closing rolls back instead of poisoning attachment. Deterministic full-queue reaper and real saturated-descendant regressions pass. Full mise run check passes (336 lib, 86 e2e).
