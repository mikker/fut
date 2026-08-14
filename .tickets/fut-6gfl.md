---
id: fut-6gfl
status: closed
deps: []
links: []
created: 2026-08-14T07:33:14Z
type: chore
priority: 2
assignee: Mikkel Malmberg
---
# Reset debug daemons before demo

Ensure the disposable demo starts from the freshly built debug binary rather than leaving older debug daemons and sockets alive.

## Acceptance Criteria

mise run demo and demo:setup stop existing target/debug/fut daemons before rebuilding.


## Notes

**2026-08-14T07:33:14Z**

The demo setup now discovers and stops every daemon launched from this checkout's target/debug/fut before rebuilding. Validation: bash syntax check and mise run check passed.
