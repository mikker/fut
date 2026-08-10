---
id: fut-dmq6
status: closed
deps: [fut-u996, fut-dafg, fut-pan0]
links: []
created: 2026-08-09T20:48:14Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [agent, skill, docs]
---
# Expand the bundled Fut skill for complete agent workflows

Update the printable first-party skill once the control surface exists so agents can safely compose layout, raw terminal, and lifecycle-aware agent operations.

## Design

Keep the skill concise and make installed CLI help authoritative. Teach explicit targeting, same-tab and same-cwd defaults, no-focus background work, JSON ID parsing, ordinary command execution, agent launch/prompt/wait/read, blocked handling, alternate-screen fallback, and ownership-safe cleanup. Do not add worktree or repository policy.

## Acceptance Criteria

fut agent skill documents every shipped agent-native primitive with executable recipes; it contains no nonexistent commands; skill metadata validates; realistic forward tests can inspect, run, delegate, collect results, and clean up without relying on UI focus.


## Notes

**2026-08-09T21:51:06Z**

Rewrote the bundled skill as a 192-line imperative workflow reference. It makes installed help authoritative and covers context/get with explicit JSON IDs; background same-tab/same-cwd pane splits; pane-based agent launch; raw run/send/read/wait-output; full agent list/get/prompt/wait/read/report lifecycle; fresh-prompt barriers and blocked success outcomes; alternate-screen visible fallback; and ownership-aware cleanup. Removed stale lifecycle/revision guidance and repository policy. Left agents/openai.yaml unchanged because its metadata already validates and names $fut. Validation: skill-creator quick_validate passed; cargo build passed; installed fut agent skill exactly matches source; every documented primitive help path resolved; focused e2e tests passed for skill printing, context/get, split/no-focus/cwd/argv, terminal input, terminal output/wait, and agent lifecycle/barrier/output. Dedicated combined forward journey remains tracked by fut-vs29.
