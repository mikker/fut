---
id: fut-d7is
status: closed
deps: []
links: []
created: 2026-08-09T20:42:24Z
type: feature
priority: 1
assignee: Mikkel Malmberg
---
# Bundle and print the Fut agent skill

Add the fut agent command scope and ship a first-party Fut skill that can be printed with fut agent skill without contacting the daemon.

## Acceptance Criteria

fut agent skill prints the bundled canonical SKILL.md exactly; help exposes the command; the skill validates; tests cover parsing and output; Unreleased changelog is updated.


## Notes

**2026-08-09T20:46:04Z**

Implemented fut agent skill with a build-time bundled SKILL.md, agent command help, docs, changelog, exact process-level output coverage, and no-daemon/runtime-state verification. Skill validation and mise run check pass.
