---
id: fut-luqs
status: closed
deps: []
links: [fut-bonr]
created: 2026-08-10T06:53:49Z
type: task
priority: 3
assignee: Mikkel Malmberg
tags: [perf, client, terminal]
---
# Bring full-screen animated rendering to Herdr-class CPU

A release Fut client and daemon running the documented 300x65 four-pane animation workload consume 10-15% total CPU, versus Herdr 7-9% and tmux about 2%. Debug Fut reaches 51-72% and can visibly lag. PERF.md records the reproducible external baseline.

## Design

Profile release daemon and attached client separately with FUT_PERF_LOG plus CPU sampling. Reduce work proportional to host area and independently animated panes, prioritizing full-grid snapshot construction/diffing and client redraw. Preserve the existing responsive control path and use the external baseline in PERF.md as the reference.

## Acceptance Criteria

On the documented reference workload, release Fut stays below 10% total visible CPU and 2.5% background daemon CPU, with stretch goals of 5% and 1.5%. Fifty control queries remain at or below 0.40s and twenty visible reads at or below 0.20s. Add a reproducible in-repo measurement recipe, record before/after results in PERF.md, and keep existing correctness/overload suites green.


## Notes

**2026-08-10T06:53:54Z**

External comparison captured 2026-08-10 in PERF.md. Important methodology correction: initial Fut result was a debug build and is retained only as a warning; acceptance targets use the optimized release rerun.

**2026-08-10T12:02:32Z**

Paused after the post-autoresearch external rerun. On the documented 300x65 four-pane release workload, steady visible CPU was approximately: Fut 10.3% total (daemon 5.5%, client 4.8%), Herdr 0.8.0 12.1% total, and tmux 3.7b 4.1%. Fut narrowly misses the strict <10% target but is now stable around 10-11% and beat Herdr in the same-run comparison. Further material gains likely require the larger dirty-row publication redesign, so lower priority and revisit when profiling or perceived performance warrants it.

**2026-08-26T09:44:48Z**

Re-profiled current main from scratch with separate release daemon/client top and sample profiles plus FUT_PERF_LOG. Root cause for the remaining background budget miss was full-grid snapshot construction continuing for animated terminals with no snapshot consumers. Implemented demand-driven snapshot publication: PTY parsing remains continuous while unobserved, and a new attachment requests a current output-barriered snapshot before its first frame. Added focused regression coverage and scripts/perf/external (mise run perf:external). Same 300x65 four-pane workload, 15 steady samples: before visible daemon/client 3.11%/3.13% (6.24% total), detached daemon 2.62%; after 3.64%/4.11% (7.75% total), detached daemon 1.11% (0.6-1.5%). After control: 50 queries 0.267s; 20 visible reads 0.110s. Release build, fmt, clippy, 593 unit tests, 123 E2E tests, render bench smoke, and isolated Hollywood overload journey all passed. PERF.md records methodology, profiles, noise caveat, and results; docs and changelog updated. Current acceptance targets are met.

**2026-08-26T10:21:04Z**

Final refactor pass simplified snapshot-refresh result handling and removed dead benchmark-script code. Manual disposable-daemon testing detached and reattached twice while output advanced from tick 19 to tick 100; both attachments received and rendered fresh frames. Post-refactor fmt, build, clippy, 593 unit tests, 123 E2E tests (including overload and detached-output continuity), and render benchmark smoke all pass.
