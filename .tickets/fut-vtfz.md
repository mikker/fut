---
id: fut-vtfz
status: closed
deps: []
links: []
created: 2026-08-14T07:12:13Z
type: bug
priority: 1
assignee: Mikkel Malmberg
tags: [daemon]
---
# Allow sibling daemon sockets

Daemons using different socket paths in the same runtime directory currently contend on one shared lock file.

## Acceptance Criteria

Distinct socket paths can run concurrently while duplicate ownership of one socket remains rejected.


## Notes

**2026-08-14T07:12:17Z**

Root cause was one runtime-directory-wide fut.lock shared by every explicit socket. Locks now derive from the complete socket path, preserving duplicate-owner exclusion while allowing debug and release daemons to coexist. Added a regression test. Validation: mise run check passed.
