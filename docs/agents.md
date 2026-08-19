---
layout: default
title: Agent activity
description: Report semantic agent activity to Fut.
---

# Agent activity

> **TL;DR:** Install the integration for your agent, launch it inside Fut, and
> use the default right sidebar or `Ctrl-b u` to follow its state. Automation
> can use `fut --json agent list`, `agent prompt`, `agent wait`, and `agent
> read` without changing client focus.

Programs running inside Fut receive scoped `FUT_SESSION_ID`, `FUT_WORKSPACE_ID`,
`FUT_TAB_ID`, `FUT_PANE_ID`, and `FUT_TERMINAL_ID` environment variables.
Resolve the terminal's current live ancestry, including the pane's current
agent activity, with:

```sh
fut --json context
```

`FUT_TERMINAL_ID` is stable. The ancestor variables describe the spawn
location and may be stale after a pane move; `context` resolves their current
replacements from a fresh daemon snapshot.

Look up any session, workspace, tab, pane, or terminal outside that inherited
context by its UUID without changing visual focus:

```sh
fut --json get UUID
```

Print Fut's bundled agent skill with:

```sh
fut agent skill
```

The printed `SKILL.md` is bundled with the binary, so its instructions match the
installed Fut release.

## Install an integration

Fut ships first-party lifecycle adapters for:

- [Claude Code](agents/claude-code.md)
- [Codex](agents/codex.md)
- [Pi](agents/pi.md)

Claude Code and Codex require their Fut plugin; Codex also requires the
documented `notify` adapter for authoritative turn completion. Follow the
linked installation guides, restart the agent, and launch it inside Fut. A
screen-based Codex fallback exists, but explicit lifecycle reports are more
reliable and take precedence.

## Control integrated agents

An agent is a terminal that has reported through an integration. List and
inspect only those terminals without changing focus:

```sh
fut --json agent list
fut --json agent get TERMINAL_ID
```

Each listed agent includes an `unread` boolean, and the list result includes
`unread_count` for status bars and other external observers:

```sh
fut --json agent list | jq -r '.result.unread_count'
```

Blocked and completed reports become unread daemon-wide. Rendering that
terminal in any attached client marks the event read for every client and for
later CLI calls; a newer event remains unread until it is rendered.

Submit a prompt as one atomic paste-and-Enter operation. Targets are always
explicit, and a currently working agent is rejected with `agent_busy`:

```sh
fut --json agent prompt TERMINAL_ID 'review the failing test'
fut --json agent prompt TERMINAL_ID 'review the failing test' --wait --timeout 2m
fut --json agent wait TERMINAL_ID --timeout 30s
```

`prompt --wait` captures the current lifecycle revision, then requires a fresh
`working` report before it accepts a later `completed`, `blocked`, or `idle`
report. A blocked report is a successful structured outcome. Standalone
`agent wait` returns an already settled agent immediately or waits for a
currently working one. Stable failures include `not_an_agent`, `agent_busy`,
`agent_timeout`, `agent_events_lagged`, and `terminal_exited`.

Read bounded terminal output together with the agent's current state and
`available` flag:

```sh
fut --json agent read TERMINAL_ID
fut --json agent read TERMINAL_ID --source recent-unwrapped --lines 200
```

`available` means the integrated terminal is open and not currently working.
Blocked agents remain available for a follow-up prompt.

## Client sidebar

The default right sidebar lists live explicitly integrated terminals in the
focused session and stays undocked when that projection is empty. Configure an
Agents component on either side with `scope = "tab"`, `"workspace"`,
`"session"`, or `"global"`. Tab, workspace, and session scope use fresh live
focus ancestry when available and otherwise fall back to the selected IDs;
global scope needs no focus anchor. Any closing agent-row ancestor excludes the
row, and detection-only activity does not qualify. Global rows navigate directly
across sessions by pane ID. The
Notifications dialog remains separate and tracks daemon-wide unread blocked or
completed attention.

## Terminal output

Read one terminal without attaching or changing another client's focus:

```sh
fut --json terminal read TERMINAL_ID
fut --json terminal read TERMINAL_ID --source recent --lines 200
fut --json terminal read TERMINAL_ID --source recent-unwrapped --lines 200
fut terminal read TERMINAL_ID --ansi
```

`visible` reads the canonical bottom viewport. `recent` selects the last N
physical rows and preserves soft wraps; `recent-unwrapped` selects the same
physical window and joins its soft-wrapped rows. Historical sources default to
200 rows and accept at most 2,000. `starts_mid_logical_line` is true when an
unwrapped window begins inside a logical line, and `truncated` reports that
older physical rows were omitted. Reads inspect at most 250,000 cells and
return at most 1 MiB; Fut returns a typed error instead of splitting UTF-8 or
silently byte-truncating output. `--ansi` preserves terminal styling.

Wait for current or future plain-text output with one daemon-side deadline:

```sh
fut --json terminal wait-output TERMINAL_ID --literal 'ready' --timeout 30s
fut --json terminal wait-output TERMINAL_ID --regex 'done [0-9]+' --timeout 2m
```

Waits subscribe before their initial output check and then react to terminal
updates; callers do not need polling loops. Durations use `ms`, `s`, or `m` and
range from 1 ms to 1 hour. Match ranges are UTF-8 byte offsets into the returned
text and always land on character boundaries. Literal and regex patterns are
limited to 4 KiB.

The alternate screen supports only `--source visible`; recent history and
unwrapping return `alternate_screen`. Other stable failures include
`invalid_regex`, `output_timeout`, `terminal_exited`, `invalid_output_rows`, and
`output_too_large`.

## Codex screen detection

When the foreground process is Codex and it has not reported through an agent
integration, Fut infers idle, working, and blocked state from the canonical live
bottom viewport. Client scrollback does not affect detection. Resource JSON
keeps inferred provenance explicit under `activity.detection`, including the
matched `rule`; lifecycle reports remain authoritative and clear inferred
provenance.

When testing this fallback, uninstall or disable existing Fut Codex lifecycle
plugins first. A plugin report intentionally takes precedence and prevents the
screen detector from affecting state. Inference stops and clears when Codex is
no longer the foreground process.

A detected transition from working to idle creates the same daemon-wide unread
completion attention as a lifecycle `completed` report.

Set `FUT_AGENT_DETECTION_LOG=1` on the Fut daemon to print each Codex process,
command line, matched rule, state, and quoted canonical screen to daemon stderr.
This diagnostic can include terminal content and should only be enabled while
debugging.

## Report lifecycle from an integration

Report semantic state from an integration with:

```sh
fut agent report working --source codex --agent-session-id SESSION --turn-id TURN
fut agent report blocked --turn-id TURN
fut agent report completed --turn-id TURN
fut agent report idle
fut agent report exited
```

The terminal defaults to `FUT_TERMINAL_ID`; outside that environment pass
`--terminal-id`. `fut terminal report` remains a compatibility alias and
accepts the same metadata.

Completion and blocked reports create daemon-wide unread attention. Use `Ctrl-b u`
to list waiting terminals and `Ctrl-b Ctrl-b` to jump to the next one. Rendering a
terminal marks its current attention as read for every attached client and later CLI
calls.

## Event stream

Outside tools can subscribe to Fut's state changes instead of polling:

```sh
fut events
```

Each line is versioned JSON with the complete resource snapshot — sessions,
workspaces, tabs, panes, and agent activity. The first line is the current
state; every later line is the state after a change. The stream ends when the
daemon exits.
