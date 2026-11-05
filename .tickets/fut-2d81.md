---
id: fut-2d81
status: closed
deps: [fut-e49e]
links: [fut-e49e]
created: 2026-11-05T21:00:39Z
type: feature
priority: 2
tags: [layouts, cli, ux]
---
# Show existing approval status in the saved-command review menu

Make saved command approvals visible in the interactive review checklist. Users must be able to distinguish commands already approved from commands still awaiting approval without running another command.

## Acceptance Criteria

- Show an explicit approved or pending status for every restart command; do not hide approved entries.
- Initialize previously approved commands as checked and unapproved or changed commands as unchecked.
- Distinguish saved approval status from unconfirmed checkbox changes, and make the effect of unchecking an approved entry clear.
- Reopening Review reflects persisted approvals; cancellation leaves the prior status unchanged.
- Add tests covering approved, pending, changed, confirmed, and cancelled states.

## Notes

Every command retains its saved approved/pending label alongside a checkbox and the proposed effect. Existing approvals start checked; unchecking explicitly revokes approval on confirmation. Reopening reflects persisted status, and cancellation preserves approvals. Covered by rendering, persistence, and pseudo-terminal tests.
