---
name: fut
description: Control Fut, a project-oriented terminal multiplexer. Use when the user explicitly mentions Fut or asks to inspect or manage Fut resources, create background terminals, send or collect terminal I/O, or coordinate reported coding-agent activity.
---

# Fut

Use Fut's noun-first CLI to inspect and control sessions, workspaces, tabs,
panes, terminals, and integrated coding agents without changing visual focus.

## Discover the installed CLI

Treat the installed binary as authoritative. Inspect the relevant command before
using it:

```sh
fut --help
fut pane --help
fut pane split --help
fut terminal --help
fut agent --help
fut agent prompt --help
fut agent skill
```

Do not run bare `fut` for discovery; it opens the current directory and attaches
interactively. Do not assume commands from tmux, Herdr, or another Fut release
exist.

## Resolve explicit targets

Processes inside Fut inherit a complete resource chain:

```sh
printf '%s\n' \
  "$FUT_SESSION_ID" \
  "$FUT_WORKSPACE_ID" \
  "$FUT_TAB_ID" \
  "$FUT_PANE_ID" \
  "$FUT_TERMINAL_ID"
fut --json context
```

`context` validates all available `FUT_*` IDs as one live ancestry. Treat
`missing_context` and `invalid_context` as typed failures; do not fill gaps from
another client's focus.

Look up any known resource UUID explicitly, including from outside Fut:

```sh
fut --json get "$resource_id"
fut --json list
fut --json agent list
```

Read IDs from JSON responses. Creation commands put the new resource IDs under
`.result.selected`; agent commands use `.result.agent` or `.result.agents`.
Pass those IDs explicitly to every later command. Check the exit status, parse
successful JSON from stdout, and parse a structured JSON error from stderr on
failure.

## Create a background terminal

Split beside an existing pane to stay in the same tab. Omit `--cwd` to inherit
the anchor pane's current directory. The command creates the pane in the
background and never selects it in a client:

```sh
fut --json pane split "$FUT_PANE_ID" right -- codex
fut --json pane split "$FUT_PANE_ID" down --cwd /path/to/project -- zsh
```

Record `.result.selected.pane_id` and `.result.selected.terminal_id` immediately.
Use `pane new` when a tab ID, rather than an anchor pane, is the intended scope:

```sh
fut --json pane new "$FUT_TAB_ID" --cwd "$PWD" -- codex
```

Arguments after `--` are a child argument vector. Wait for a launched agent's
integration report before expecting it in `agent list`; inspect its explicit
terminal ID with `agent get`.

## Control an ordinary terminal

Submit one literal shell command plus Enter atomically:

```sh
fut --json terminal run "$terminal_id" 'mise run test'
```

Use lower-level input only when atomic command submission is not appropriate:

```sh
fut --json terminal send-text "$terminal_id" 'draft text'
fut --json terminal send-keys "$terminal_id" ctrl+c
```

`send-text` does not press Enter. `send-keys` accepts named keys, characters,
and chords exposed by installed help.

Collect bounded output without attaching:

```sh
fut --json terminal read "$terminal_id"
fut --json terminal read "$terminal_id" --source recent-unwrapped --lines 200
fut --json terminal wait-output "$terminal_id" --literal 'READY' --timeout 30s
fut --json terminal wait-output "$terminal_id" --regex 'done [0-9]+' --timeout 2m
```

Prefer one daemon-side `wait-output` deadline to polling. Treat `output_timeout`,
`terminal_exited`, invalid regexes, and output limits as typed outcomes.

## Coordinate an integrated agent

An integrated agent is a terminal that has reported semantic lifecycle state.
Discover and inspect it without focus changes:

```sh
fut --json agent list
fut --json agent get "$terminal_id"
fut --json agent read "$terminal_id" --source recent-unwrapped --lines 200
```

Submit a prompt atomically. Add `--wait` and `--timeout` when the result is needed:

```sh
fut --json agent prompt "$terminal_id" 'Review the failing test.'
fut --json agent prompt "$terminal_id" 'Review it, fix it, and report tests.' --wait --timeout 2m
```

`prompt --wait` is the fresh-prompt barrier: it requires a new `working` report
after submission before accepting a later `completed`, `blocked`, or `idle`
state. Inspect `.result.activity.state`; `blocked` is a successful structured
outcome, so surface its output or blocker instead of treating exit status zero
as completion or blindly retrying.

Wait for work already in progress separately:

```sh
fut --json agent wait "$terminal_id" --timeout 2m
```

Standalone `agent wait` may return an already settled state immediately. It does
not establish a fresh prompt barrier. Treat `not_an_agent`, `agent_busy`,
`agent_timeout`, `agent_events_lagged`, and `terminal_exited` as typed failures.

## Report lifecycle from an integration

Only an integration for the process in that terminal should report lifecycle:

```sh
fut --json agent report idle --terminal-id "$FUT_TERMINAL_ID" --source codex
fut --json agent report working --terminal-id "$FUT_TERMINAL_ID" --source codex --agent-session-id "$agent_session_id" --turn-id "$turn_id"
fut --json agent report blocked --terminal-id "$FUT_TERMINAL_ID" --source codex --agent-session-id "$agent_session_id" --turn-id "$turn_id"
fut --json agent report completed --terminal-id "$FUT_TERMINAL_ID" --source codex --agent-session-id "$agent_session_id" --turn-id "$turn_id"
```

Inside Fut, `--terminal-id` may be omitted because `FUT_TERMINAL_ID` is used.
Do not manufacture reports on behalf of another agent or infer semantic state
from its screen.

## Handle alternate-screen programs

Recent history is unavailable while a full-screen program uses the alternate
screen. Fall back to its visible viewport:

```sh
fut --json agent read "$terminal_id" --source visible
fut --json terminal read "$terminal_id" --source visible
```

Prefer `agent read` for integrated agents because it includes lifecycle and
availability. Use raw `terminal read`, `send-text`, and `send-keys` for an
unintegrated interactive program. Never screen-scrape a completion state when
lifecycle reports are available.

## Clean up only owned resources

Keep the pane and terminal IDs returned by creation. Collect final output and
state before closing the pane:

```sh
fut --json agent get "$created_terminal_id"
fut --json agent read "$created_terminal_id" --source recent-unwrapped --lines 200
fut --json pane close "$created_pane_id"
```

If the process is not an integrated agent, use `terminal read` before closing.
Close only resources created for the current task unless the user explicitly
requests broader cleanup. Never select, attach to, or visually focus a resource
merely to inspect, control, wait for, or close it.
