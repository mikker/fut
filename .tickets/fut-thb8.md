---
id: fut-thb8
status: closed
deps: []
links: []
created: 2026-08-10T13:30:49Z
type: task
priority: 2
assignee: Mikkel Malmberg
---
# Paging backwards in git diff (u key in pager) has latency that paging forwards (d) doesn't. Is this a rendering cache issue?

Paging backwards in git diff (u key in pager) has latency that paging forwards (d) doesn't. Is this a rendering cache issue?


## Notes

**2026-08-11T07:46:43Z**

Added mise run perf:pager, a reproducible release diagnostic covering pager input-to-first/last-byte timing plus Fut parse, snapshot, encode/decode, and client-grid rendering across three positions. On less 668 at 200x50, backward paging showed no pager-output or downstream snapshot/render disadvantage; only VT parsing was ~12µs slower. This does not cover daemon scheduling, socket I/O, terminal diff/flush, host paint, or the original real git-diff session, so the perceived latency remains unresolved and needs real-session end-to-end tracing.

**2026-08-11T09:00:05Z**

Reproduced against the actual gd pipeline (git diff -> git-delta -> less) in a 300x65 PTY and traced through a real debug daemon/client. Direct delta paging was symmetric (<6ms), but Fut delivered d in 185-262ms and u in 5.2-7.9s. Delta's less -X backward half-page emits 33 reverse-index (ESC M) operations; Ghostty's Zig Debug mode took 5.7-7.8s to parse the captured 12.8KiB update. Fixed the contributor-facing dev profile by optimizing dependencies and making libghostty-vt-sys select the optimized native engine while leaving Fut itself unoptimized with full debug info; kept Ghostty Debug mode for tests. The same full client probe now delivers both d and u in 1.6-1.7ms to first host output and 15-18ms to last output. Updated perf:pager to exercise less -X/reverse-index. mise run check and mise run perf:pager pass.

**2026-08-11T09:09:57Z**

Follow-up: the first dev-profile shape optimized dependencies but left Fut itself at opt-level 0. Bare futd then spun in debug HashSet validation before daemon autostart. Reproduced with no socket/output and sampled the hot loop in config validation. Changed the whole dev profile to opt-level 1 (debug info/assertions retained), while explicitly keeping libghostty-vt-sys in Debug for the test profile. Bare futd now creates its socket and renders first output in ~78ms; mise run check passes.
