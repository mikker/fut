---
id: fut-mjrz
status: in_progress
deps: []
links: []
created: 2026-08-08T19:12:04Z
type: feature
priority: 2
assignee: Mikkel Malmberg
---
# Dirty-row snapshot deltas on the wire

If animations still lag after 0.2: send only changed rows per frame instead of full grids. Design is in PERF.md (Remaining hypotheses): diff in the daemon writer task against the last frame actually sent per connection (ordered socket makes the base always correct, coalescing untouched), full-snapshot fallback on attach/resize/size-change/large diffs; client applies rows in place and skips blitting unchanged rows in Screen::render (kills the remaining ~4.5ms full-grid blit). e2e suite needs a helper folding deltas into a tracked grid. Measure with mise run perf:e2e (dense scenario) and FUT_PERF_LOG before/after.

