---
id: fut-zu97
status: closed
deps: [fut-dafg]
links: []
created: 2026-08-09T20:48:01Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [agent, integration, codex]
---
# Add a lifecycle-only Codex integration

Provide a first-party Codex adapter that reports authoritative lifecycle feedback to Fut when Codex runs inside a Fut terminal.

## Design

Use supported Codex hooks or plugin surfaces only. Translate native events into idle, working, blocked, and completed reports with identity metadata when available. Do not control Codex, layout, prompts, output, permissions, or worktrees.

## Acceptance Criteria

Installation is documented and repeatable; outside Fut it is inert; inside Fut the sidebar, events, agent wait, and prompt --wait observe correct transitions; hook failures do not break Codex.


## Notes

**2026-08-09T21:39:29Z**

Implemented a lifecycle-only Codex plugin prototype under integrations/codex using official stable Codex hooks plus notify. Mapping: SessionStart=idle, UserPromptSubmit=working, PermissionRequest=blocked, Pre/PostToolUse=working, notify agent-turn-complete=completed; forwards Codex session/thread and turn IDs to the forthcoming fut agent report CLI. Adapter is inert without FUT_TERMINAL_ID+FUT_SOCKET and swallows malformed input/report failures. Plugin validation and 4 adapter tests pass. Not closing: official Codex docs state matching hooks run concurrently. Another UserPromptSubmit hook may block after working is reported, and another PermissionRequest hook may resolve after blocked is reported; Codex exposes no passive post-hook-decision event. Narrow acceptance adjustment: define these as native decision-point states with eventual repair on tool/completion, or wait for post-decision lifecycle events. Strict correctness under intervening hooks is not currently possible for a passive plugin.

**2026-08-09T21:40:20Z**

Contract accepted by orchestration: reports are native Codex decision-point states at hook execution time, not post-decision authority; later concurrent hooks may change behavior and the next native event repairs state. Documentation now states this explicitly. Ticket remains in_progress pending fut-dafg, when the adapter's exact fut agent report calls and observable Fut behavior must be verified against the real binary before closure.

**2026-08-09T21:46:09Z**

Final live verification completed after fut-dafg landed. Built target/debug/fut and ran the adapter against an isolated real daemon. Verified SessionStart -> idle with source=codex/session identity through agent get; UserPromptSubmit -> working/unavailable with turn identity; notify agent-turn-complete releases a blocking agent wait as completed; agent prompt --wait observes a fresh working revision and completed turn through its revision barrier; PermissionRequest -> blocked and agent wait returns blocked successfully. Added opt-in live test and repeatable test instructions. Final validation: plugin validator passed, 4 fast adapter tests passed, live real-binary test passed, JSON parsing and diff check passed. Contract remains explicitly limited to native decision-point truth; concurrent later hooks may change behavior and the next native event repairs state.
