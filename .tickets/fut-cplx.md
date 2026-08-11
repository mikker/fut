---
id: fut-cplx
status: closed
deps: []
links: []
created: 2026-08-10T13:04:09Z
type: task
priority: 2
assignee: Mikkel Malmberg
---
# Make two terminal windows able to hold same session

Like tmux, size is set to smallest window's


## Notes

**2026-08-11T09:58:21Z**

Implemented simultaneous attachments with smallest-client PTY geometry, resize/detach restoration, monotonic geometry ordering against stale races, E2E coverage, docs, and manual two-window verification.
