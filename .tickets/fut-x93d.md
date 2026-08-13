---
id: fut-x93d
status: closed
deps: []
links: []
created: 2026-08-12T08:10:42Z
type: task
priority: 2
assignee: Mikkel Malmberg
---
# Investigate herdr's screen snapshot parsing for agent status

Investigate herdr's screen snapshot parsing for agent status

## Notes

**2026-08-12T11:33:04Z**

Investigation: Herdr detects the foreground agent process, reads a canonical live bottom-buffer snapshot independent of the scrolled viewport, evaluates per-agent TOML manifests over bounded regions/OSC evidence, and stabilizes transitions before publishing status. Complete lifecycle integrations remain authoritative. Fut already has visible/recent/recent-unwrapped Ghostty captures and AgentActivity lifecycle reporting, but lacks foreground agent identification, a detection snapshot source, manifest evaluation, and transition stabilization.\n\nAgreed scope: narrow Codex end-to-end spike. Plan: (1) capture representative Codex idle/working/blocked/transcript/alternate-screen fixtures; (2) add a canonical bottom-buffer detection capture that is unaffected by client scroll and works on the active screen; (3) identify Codex from the terminal foreground process, keeping integrated terminals out of fallback detection; (4) implement only the minimal bounded-region rules needed for those fixtures, informed by Herdr's Codex manifest rather than importing its full generic manifest/update system; (5) publish inferred state through existing AgentActivity with explicit provenance so inferred status cannot masquerade as lifecycle reports; (6) add transition stabilization for ambiguous working-to-idle changes; (7) expose diagnostic output sufficient to inspect snapshot, matched rule, and fallback reason; (8) test fixtures, viewport independence, lifecycle precedence, process replacement/exit, and transition stability; then document findings and decide whether to generalize manifests and support more agents.\n\nDone for the spike: Codex status tracks idle/working/blocked from captured fixtures without being affected by user scroll; lifecycle reports win; process exit/replacement clears inferred status; failures fall back safely to idle/unknown; diagnostics explain every classification; tests establish which parts should become general infrastructure.

**2026-08-12T11:58:05Z**

Implemented the narrow Codex fallback spike. Fut now polls each live foreground process, recognizes Codex process variants, reads the canonical live bottom viewport (independent of client scroll and valid on the active alternate screen), classifies bounded live blockers/confirmation prompts/working indicators with an idle fallback, and requires three observations before working-to-idle. Inferred provenance is explicit as activity.detection { agent, rule }; lifecycle reports clear it and remain authoritative. Detection clears when Codex leaves the foreground. Added detector/stabilizer and lifecycle-precedence tests plus docs and changelog. Validation: full mise run test passed (394 unit tests, 92 e2e tests, render bench targets), targeted new tests passed, and cargo clippy --all-targets --all-features -- -D warnings passed. IMPORTANT manual testing note: uninstall or disable existing Fut Codex lifecycle plugin(s) first; any report intentionally takes precedence and suppresses fallback state updates.

**2026-08-13T12:34:32Z**

Finalized after live testing on macOS with the installed Node-wrapped Codex. Fixed foreground identification by inspecting both process name and command line, added opt-in FUT_AGENT_DETECTION_LOG diagnostics, and added a real daemon/PTY E2E fixture proving idle -> working -> idle. Detected working -> idle now emits a completed event so existing per-client unread attention works. Refactor pass replaced positional process-observation tuples with a named type and found no further useful abstraction for the deliberately narrow Codex spike. Final validation: 397 unit tests and 93 E2E tests passed, render benchmark targets passed, clippy passed with warnings denied, and git diff --check passed.
