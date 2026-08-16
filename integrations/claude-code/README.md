# Fut lifecycle adapter for Claude Code

This first-party Claude Code plugin reports lifecycle state for the Claude Code
session running in a Fut terminal. It has no commands, skills, agents, or MCP
servers. It never changes Claude Code decisions or controls prompts, tools,
permissions, terminal layout, output, or worktrees.

## Install

Add Fut's GitHub repository as a marketplace, then install the lifecycle
plugin:

```sh
claude plugin marketplace add mikker/fut
claude plugin install fut@fut-integrations
```

For development from a local Fut checkout, load the plugin directly instead:

```sh
claude --plugin-dir "$PWD/integrations/claude-code"
```

Inside Claude Code, `/hooks` shows the handlers with source `Plugin`. During
plugin development, `/reload-plugins` reloads them. Claude Code documents
`--plugin-dir` as its supported local plugin testing mechanism in
[Create plugins](https://code.claude.com/docs/en/plugins#test-your-plugins-locally).

The adapter is inert unless both `FUT_SOCKET` and `FUT_TERMINAL_ID` are present,
as they are for a process spawned inside Fut. The `fut` binary must be on
`PATH`, and the installed Fut must provide:

```sh
fut agent report STATE --terminal-id ID --source SOURCE \
  [--agent-session-id ID] [--turn-id ID]
```

Every handler has a two-second Claude Code timeout, suppresses Fut output, and
returns success when Fut is absent or rejects a report. Reporting cannot block
or alter Claude Code behavior.

## Event mapping

The adapter consumes only documented command-hook JSON and sends the documented
`session_id` as `--agent-session-id`. Claude Code does not expose a turn ID in
the common hook input, so the adapter does not invent one.

| Fut state | Authoritative Claude Code evidence |
| --- | --- |
| `idle` | `SessionStart` for startup, resume, clear, or fork |
| `working` | `UserPromptSubmit`; `PostToolUse`; `PostToolUseFailure`; `PermissionDenied`; `ElicitationResult` |
| `blocked` | `Notification` with `permission_prompt`, `elicitation_dialog`, or `agent_needs_input`; `StopFailure` |
| `completed` | `Stop`; `Notification` with `idle_prompt` |
| `exited` | `SessionEnd` |

These events and inputs come from the official
[Hooks reference](https://code.claude.com/docs/en/hooks): `UserPromptSubmit`
runs before Claude processes a prompt, `Stop` runs when the main agent finishes
responding, `StopFailure` replaces it for API failures, and notification types
identify permission, elicitation, input, and idle waits. Plugin hook placement,
`${CLAUDE_PLUGIN_ROOT}`, exec-form arguments, and handler timeouts follow the
same reference.

## Limitations

- `Stop` does not fire for a user interrupt. `idle_prompt`, when Claude emits
  it, settles that case; otherwise the next prompt or session event corrects
  the state.
- A separate Stop hook can make Claude continue after the Stop event. Fut may
  briefly show completed until another native working event fires.
- After permission is granted, Claude Code exposes no general "permission
  resumed" hook. The next tool result clears blocked, so a long-running approved
  tool may remain shown as blocked until it finishes.
- `agent_needs_input` describes background sessions only while agent view is
  open. It is still native evidence that the Claude activity in that terminal
  is waiting for input.
- Managed Claude Code policy can disable plugin hooks. Use `/hooks` or the
  plugin manager's Errors view to verify loading.

## Test

```sh
python3 integrations/claude-code/tests/test_adapter.py
claude plugin validate integrations/claude-code
```

The first command needs only Python's standard library. The second uses Claude
Code's official plugin schema validator.
