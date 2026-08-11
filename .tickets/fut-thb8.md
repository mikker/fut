---
id: fut-thb8
status: open
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
