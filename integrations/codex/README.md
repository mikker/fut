# Codex lifecycle integration

This integration reports Codex's supported native lifecycle decision points to
the Fut terminal containing Codex. A report describes Codex when this hook
fires; it does not claim that a later concurrent hook cannot change what Codex
does. The next native event repairs the reported state. The integration does not
read terminal output or control prompts, permissions, layout, worktrees, or
Codex itself.

It is built only on documented Codex surfaces:

- [Lifecycle hooks](https://learn.chatgpt.com/codex/hooks) provide the session,
  prompt, permission, and tool events plus Codex session and turn IDs.
- [External notifications](https://developers.openai.com/codex/config-advanced#notifications)
  currently provide `agent-turn-complete` and its thread and turn IDs.
- [Plugin packaging](https://developers.openai.com/plugins/build/plugins#bundled-mcp-servers-and-lifecycle-hooks)
  supports a default `hooks/hooks.json` and the installed `PLUGIN_ROOT` path.

## Install

Install a Fut release that exposes `fut agent report` and ensure `python3` is
available, then add Fut's GitHub marketplace and plugin:

```sh
codex plugin marketplace add mikker/fut
codex plugin add fut@fut-integrations
```

Start Codex, run `/hooks`, and trust the Fut Codex hooks. Codex deliberately
skips new or changed non-managed plugin hooks until they are reviewed.

Codex exposes final turn completion through its machine-local `notify` command,
not through plugin hooks. Install the notification adapter at a stable path:

```sh
mkdir -p "$HOME/.local/bin"
curl -fsSL \
  https://raw.githubusercontent.com/mikker/fut/main/integrations/codex/plugins/fut-codex/scripts/fut_codex_lifecycle.py \
  -o "$HOME/.local/bin/fut-codex-notify"
chmod +x "$HOME/.local/bin/fut-codex-notify"
```

Then add the following user-level setting to `~/.codex/config.toml`:

```toml
notify = [
  "fut-codex-notify",
  "--notify",
]
```

`notify` is a single command setting. If it is already in use, configure the
existing notification program to dispatch the same JSON argument to this
adapter as well; do not add a second `notify` key.

The command assumes `~/.local/bin` is on `PATH`.

Restart Codex after changing plugin or notification configuration.

## Test

The fast adapter contract tests use a fake `fut` executable. Set
`FUT_LIVE_BIN` to a built Fut binary to additionally start an isolated daemon
and verify real `agent report`, `agent get`, `agent wait`, and `agent prompt
--wait` behavior:

```sh
python3 -m unittest discover -s integrations/codex/tests -v
FUT_LIVE_BIN="$PWD/target/debug/fut" \
  python3 -m unittest discover -s integrations/codex/tests -v
```

## Native event mapping

| Codex event | Fut state | Identity |
| --- | --- | --- |
| `SessionStart` | `idle` | Codex `session_id` |
| `UserPromptSubmit` | `working` | `session_id`, `turn_id` |
| `PermissionRequest` | `blocked` | `session_id`, `turn_id` |
| `PreToolUse`, `PostToolUse` | `working` | `session_id`, `turn_id` |
| `notify: agent-turn-complete` | `completed` | `thread-id`, `turn-id` |

The adapter is inert unless both `FUT_TERMINAL_ID` and `FUT_SOCKET` are set.
Malformed events, missing `fut`, timeouts, and failed reports are ignored so a
feedback failure cannot break Codex.

`PermissionRequest` means Codex reached an approval decision point. Another
policy hook may resolve that request immediately; the next tool or completion
event clears the transient blocked state. Codex currently exposes no separate
"approval prompt became visible" or "approval resolved" hook.

Completion intentionally uses `notify` rather than `Stop`: Codex permits another
`Stop` hook to continue the turn, so `Stop` is not authoritative completion.

## Limitations

Codex launches matching hooks concurrently. A separate `UserPromptSubmit` hook
can still block a prompt after this adapter reports `working`, and a separate
`PermissionRequest` hook can resolve an approval immediately after this adapter
reports `blocked`. These are native decision-point states, not post-decision
claims. Codex does not currently publish a post-hook-decision event that would
let a passive plugin distinguish those cases; the next tool or completion event
repairs the reported state.
