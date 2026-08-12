---
id: fut-l08p
status: closed
deps: []
links: []
created: 2026-08-11T21:19:16Z
type: task
priority: 2
assignee: Mikkel Malmberg
---
# Make mouse selection -> copy work

Make mouse selection -> copy work

## Design

Follow tmux and Herdr: application mouse reporting has precedence; otherwise a focused-pane left drag enters Fut's client-local copy path. Record the anchor on button-down, begin selection only after a drag, coalesce motion by terminal cell, and copy/finalize on release. Shift overrides application reporting when the host forwards the modified gesture. Carry mouse-reporting state in revisioned screen snapshots so the split daemon/client architecture makes the routing decision from explicit terminal state.

## Acceptance Criteria

Plain clicks do not copy. Plain drags select and copy when application mouse reporting is off; reported gestures still reach mouse-aware applications; Shift-drag selects through Fut. Selection uses pane-local visible/history coordinates, existing clipboard error recovery, bounded copy limits, and client-local ownership. Protocol, unit, E2E, docs, and changelog coverage pass.

## Notes

**2026-08-12T09:39:18Z**

Implemented tmux/Herdr-style mouse selection: revisioned snapshots expose application mouse tracking, plain focused-pane drags fall back to client-local copy mode, Shift overrides reporting when forwarded, clicks remain inert, drag updates coalesce, and release uses the existing retry-safe pbcopy/finalize path. Added protocol/runtime/client/unit/E2E coverage, bumped protocol 17 to 18, and updated changelog/docs. RUST_TEST_THREADS=1 mise run check passes (390 unit tests, 92 E2E tests, Clippy, formatting, and all targets).

**2026-08-12T11:22:45Z**

Refactor pass completed: folded the pending selection anchor into the per-button mouse state, centralized copy-mode begin classification and viewport math, removed duplicated copy-mode initialization, and simplified active drag handling. Re-ran 13/13 manual SGR/PTTY mouse scenarios after the refactor. Final RUST_TEST_THREADS=1 mise run check passes: formatting, Clippy with warnings denied, 390 unit tests, 92 E2E tests, all targets, examples, and render benchmarks.
