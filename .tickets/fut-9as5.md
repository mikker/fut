---
id: fut-9as5
status: closed
deps: [fut-dafg]
links: []
created: 2026-08-09T20:48:01Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [agent, integration, claude]
---
# Add a lifecycle-only Claude Code integration

Provide a first-party Claude Code adapter that reports authoritative lifecycle feedback to Fut when Claude Code runs inside a Fut terminal.

## Design

Use supported Claude Code hooks or plugin surfaces only. Translate native events into idle, working, blocked, and completed reports with identity metadata when available. Do not control Claude Code, layout, prompts, output, permissions, or worktrees.

## Acceptance Criteria

Installation is documented and repeatable; outside Fut it is inert; inside Fut the sidebar, events, agent wait, and prompt --wait observe correct transitions; hook failures do not break Claude Code.


## Notes

**2026-08-09T21:40:44Z**

Implemented a self-contained lifecycle-only Claude Code plugin under integrations/claude-code using official command hooks and `${CLAUDE_PLUGIN_ROOT}` exec-form handlers. Mapping: SessionStart/End→idle; UserPromptSubmit and post-tool/permission-denied/elicitation-result events→working; native permission/elicitation/input notifications and StopFailure→blocked; Stop and idle_prompt→completed. The reporter is inert without FUT_SOCKET+FUT_TERMINAL_ID, passes source=claude-code and documented session_id metadata to `fut agent report`, bounds hook execution to 2 seconds, suppresses output, ignores Fut failures, and has no decision/control output. Documented repeatable `claude --plugin-dir` loading, verification, mappings, and limitations with official code.claude.com hook/plugin references. Verified with the official `claude plugin validate`, integration Python tests including 200KB input/failure/scoping cases, sh syntax/diff checks, and a real Fut daemon smoke showing agent.get source/session/working state.
