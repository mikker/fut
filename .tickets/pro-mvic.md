---
id: pro-mvic
status: open
deps: [pro-ja4w]
links: []
created: 2026-08-18T07:22:56Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: pro-pgup
---
# Add the in-client project opener

Add Ctrl-b Shift-S Open Project UI with fuzzy configured-project suggestions and free-form path input.

## Acceptance Criteria

Configured projects are suggested without scanning; paths can be entered; live projects navigate instead of duplicating; new projects attach to the recipe-selected terminal.


## Notes

**2026-08-18T09:40:02Z**

Project opener must present untrusted repository recipes for in-client approval and call the shared machine-local trust API; users should never handle hashes or edit trust state.
