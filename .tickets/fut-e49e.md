---
id: fut-e49e
status: closed
deps: []
links: [fut-2d81]
created: 2026-11-05T21:00:39Z
type: feature
priority: 2
tags: [layouts, cli, ux]
---
# Approve saved restart commands through an interactive review checklist

Make `fut layout review` the normal approval workflow: show all saved restart commands in a menu and let the user check the commands they approve, then confirm together. Do not require copying fingerprints or running individual approval commands.

## Acceptance Criteria

- List every captured restart command, including previously approved commands, with project, working directory, executable, and arguments available for inspection.
- Support keyboard navigation, checkbox selection, explicit confirmation, and cancellation without changing approvals.
- Keep approval bound to the exact executable, arguments, and working directory; reject stale selections if the saved commands change before confirmation.
- When no restart commands were captured, show an explicit explanation rather than blank output. Explain that affected panes restore as shells and provide available capture warnings.
- Keep JSON/noninteractive review usable without opening a menu; approval administration remains host-local.
- Add tests for selecting multiple commands, confirmation, cancellation, stale commands, empty review, and noninteractive use.

## Notes

Implemented a host-local keyboard checklist with exact command inspection, explicit batch confirmation, cancellation, and atomic stale-review rejection. Empty reviews explain shell fallback and show capture warnings. JSON and redirected review remain read-only. Full Rust suite passes (1,197 tests), including pseudo-terminal workflow tests.
