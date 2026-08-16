---
id: fut-gqe3
status: closed
deps: []
links: []
created: 2026-08-16T20:15:41Z
type: feature
priority: 2
assignee: Mikkel Malmberg
---
# Share agent unread status daemon-wide

Move agent notification read state into authoritative daemon resources and expose unread status through fut agent list for external status bars.

## Acceptance Criteria

Blocked and completed events are unread until rendered by any client; read state is shared across clients; agent list JSON includes per-agent unread and unread_count; stale acknowledgements cannot hide newer events.


## Notes

**2026-08-16T20:15:41Z**

Implemented daemon-owned read revisions, protocol acknowledgement, CLI unread fields/count, docs, changelog, resource tests, and end-to-end coverage. Refactor pass removed per-client notification storage and prevents repeated acknowledgements after read. Validation: mise run check; cargo build.
