# Fut lifecycle adapter for Google Antigravity (AGY)

This first-party Antigravity plugin reports lifecycle state for the Antigravity
session running in a Fut terminal. It has no commands or MCP servers. It never
changes Antigravity decisions or controls prompts, tools, permissions, terminal
layout, output, or worktrees.

## Install

Install via Antigravity's plugin manager:

```sh
agy plugin install https://github.com/mikker/fut/tree/main/integrations/agy
```

Or for local development:

```sh
agy plugin install ./integrations/agy
```

Or for repository-scoped use, place it in `.agents/plugins/fut`.

Restart Antigravity and launch it inside Fut. The adapter is inert unless both
`FUT_SOCKET` and `FUT_TERMINAL_ID` are present, as they are for a process
spawned inside Fut. The `fut` binary must be on `PATH`.

The daemon validates the inherited terminal ID against the reporting process
tree.

Every handler has a two-second timeout, suppresses Fut output, returns an
`{"decision": "allow"}` JSON payload on stdout to satisfy Antigravity's hook
contract, and exits cleanly when Fut is absent or rejects a report.

## Event mapping

| Fut state | Authoritative Antigravity evidence |
| --- | --- |
| `working` | `PreInvocation`; `PostToolUse` after `ask_question` |
| `blocked` | `PreToolUse` for `ask_question`; `Stop` with `terminationReason: "error"` |
| `completed` | `Stop` with successful settlement (`model_stop`) |

The adapter sends the documented `conversationId` as `--agent-session-id`.

## Test

```sh
python3 integrations/agy/tests/test_adapter.py
```
