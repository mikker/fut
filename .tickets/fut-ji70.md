---
id: fut-ji70
status: closed
deps: []
links: []
created: 2026-08-24T11:41:39Z
type: feature
priority: 2
assignee: Mikkel Malmberg
---
# Keep sidebar status colors outside focused titles

Limit passive current/focus styling in expanded workspace and agent sidebar rows to the title text so independently colored run and agent status indicators remain easy to scan.

## Acceptance Criteria

Current workspace and agent titles retain clear focus styling while status indicators preserve their semantic colors. Interactive keyboard selection remains a whole-row state. Tests and changelog cover the default behavior.


## Notes

**2026-08-24T11:45:57Z**

Expanded sidebar rows now apply current styling only to workspace.name and the agent source label. Run/extension and agent lifecycle statuses retain semantic foreground colors; keyboard selection and closing remain whole-row states. cargo test (560 unit, 117 e2e) and cargo build passed.
