---
layout: default
title: Program status and agents
description: OSC 7501 program status, progress, and agent automation in Fut.
---

# Program status and agents

> **TL;DR:** Any program that speaks OSC 7501 shows status in Fut without a
> plugin. Follow progress, messages, completion, and errors in the normal
> project tree and Agents sidebar. Claude Code, Codex, and Pi emit OSC 7501
> natively; the agent plugins are legacy fallbacks for older versions, and
> screen detection is the last resort. Automation can use
> `fut --json agent list`, `agent prompt`, `agent wait`, and `agent read`
> without changing client focus.

## OSC 7501: the default

Fut implements revision 0.4 of the
[Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status).
A program writes status to its terminal, for example:

```sh
printf '\e]7501;state=working:app=demo:progress=40\e\\'
```

The states are `idle`, `working`, `done`, `blocked`, and `error`. `blocked`
can include `kind=permission`, `question`, or `auth`. `working` and `blocked`
may include an integer `progress=0` through `100`; omitting it means
indeterminate progress. `app` is a short stable program identifier. `title`
and `msg` are base64-encoded, single-line UTF-8 plain text, not markup.
Here is the shell function from the specification:

```sh
status() {
  printf '\e]7501;state=%s:msg=%s\e\\' "$1" "$(printf '%s' "$2" | base64 | tr -d '\n')"
}
status working "Syncing photos"
rsync -a ~/Photos backup:/photos && status done "Photos synced" || status error "rsync failed"
```

A report replaces the entire record at its `id`; no id addresses the root.
Ids such as `build/test` form subtrees. Omitted fields are removed, except
that an omitted app is displayed using its nearest ancestor's app. Send
`state=clear:id=build` to remove that record and its children, or
`state=clear` to remove every record. Fut supports up to 256 records per
terminal, evicting the least recently updated when full.

Records belong to the terminal, not the screen. A new shell prompt (OSC 133
`A`) or terminal-process exit removes transient working and blocked records.
Observed foreground-program exit removes only that reporter's transient
records, preserving unrelated work. Done and error records survive until
replaced or cleared. A full reset (RIS) clears all records, while a soft reset
(DECSTR) and screen switching do not.

Programs can query support with `OSC 7501 ; ? ST`. Fut replies:

```text
OSC 7501 ; ?:states=idle,working,done,blocked,error:kinds=permission,question,auth ST
```

The reply uses the query's terminator (`ESC \\` or BEL) and never includes
record contents. Queries are optional: programs may report immediately.
Fut rejects reports over the protocol's bounds and text containing control
characters. Displayed messages remain untrusted plain text.

### Legacy OSC 9;4 progress

Programs that only emit ConEmu-style progress (`OSC 9 ; 4 ; state ; percent ST`,
as emitted by tools such as cargo's `term.progress.term-integration`) still
show progress in Fut. Fut
maps it onto the root record:

| OSC 9;4 state | Root record |
| --- | --- |
| `1` set | `working` with the percentage, or indeterminate without one |
| `3` indeterminate | `working`, indeterminate |
| `4` paused | `working`, keeping the last percentage when none is given |
| `2` error | `error` |
| `0` remove | removes the record that OSC 9;4 created |

Paused maps to `working`, not `blocked`: a paused bar does not mean the
program is waiting for the user, and `blocked` would raise attention.

This mapping is always on and deliberately weak. Its records report
`activity.source` as `{"type": "detection", "agent": "progress", "rule":
"osc_9_4"}`, so like screen detection they never make a terminal an agent in
`agent list`, `agent get`, or the Agents projections; they drive the normal
status, tab and workspace progress, and error attention. Explicit OSC 7501
and CLI reports take precedence: they replace the inferred record, OSC 9;4 is
ignored while an explicit reporter owns the terminal, and `9;4;0` only removes
what OSC 9;4 created. Once a program sends any OSC 7501 report, Fut ignores
OSC 9;4 in that terminal until that program exits or a full reset (RIS), so
legacy progress emitted alongside the real protocol cannot overwrite a blocked
kind or message. A new prompt or a soft reset does not end this. The program
is the foreground process that sent the report, as for its records; when the
terminal's own shell reports, only RIS ends it, and a report Fut reads only
after its program has already exited counts as the shell's. Sending `state=clear` does
not make a program the reporter, so an attached Fut client forwarding its
status (including `fut --machine` over SSH) releases the terminal when it
exits. Progress after that exit treats a surviving `done` or `error` root
like any new root report does: it replaces it, and result children stay. A
new prompt or the exit of the foreground program that sent OSC 9;4 ends
inferred working progress; an inferred error stays, like any error, until
`9;4;0` or new progress replaces it.

Programs running inside Fut receive scoped `FUT_SESSION_ID`, `FUT_WORKSPACE_ID`,
`FUT_TAB_ID`, `FUT_PANE_ID`, and `FUT_TERMINAL_ID` environment variables. These
use Fut's compact, reversible 23-character form; CLI ID arguments also accept
canonical UUIDs from structured output.
Resolve the terminal's current live ancestry, including the pane's current
agent activity, with:

```sh
fut --json context
```

`FUT_TERMINAL_ID` is stable. The ancestor variables describe the spawn
location and may be stale after a pane move; `context` resolves their current
replacements from a fresh daemon snapshot.

Look up any session, workspace, tab, pane, or terminal outside that inherited
context by its compact ID or canonical UUID without changing visual focus:

```sh
fut --json get ID
```

Print Fut's bundled agent skill with:

```sh
fut agent skill
```

The printed `SKILL.md` is bundled with the binary, so its instructions match the
installed Fut release.

## Forward status to the outer terminal

An attached Fut client reports its own rollup to the terminal it runs in,
using the same protocol. A terminal that supports OSC 7501, such as Ghostty or
another Fut, then shows what every Fut terminal is doing. Running Fut inside a
Fut terminal (including `fut --machine` over SSH) shows the inner Fut as one
program with a child per busy terminal:

```text
root          app=fut   state=blocked  msg="proj/main/claude: Apply changes?"
fPq...Yw      app=claude-code  state=blocked  title="proj/main/claude"
fX2...0Q      app=cargo        state=working  progress=40  title="proj/main/build"
```

- **Negotiation.** The client sends `OSC 7501 ; ? ST` when it starts and
  reports nothing until the terminal answers. Terminals that do not support the
  protocol, and multiplexers such as tmux that do not forward the reply, get
  nothing beyond that query. Set `alerts.outer_terminal_status = "always"` to
  skip the query, or `"never"` to turn forwarding off; see
  [configuration](configuration.md).
- **Records.** Each Fut terminal whose status is working, blocked, done, or
  error gets one child whose id is the terminal's compact ID. Idle terminals
  are left out. A child carries that terminal's state, blocked kind, progress,
  and program app; its title is the `session/workspace/tab` location, prefixed
  by the machine label when several machines are configured; its message joins
  the program's title and message. The root has `app=fut`, the strongest child
  state (blocked, then error, working, done), that child's kind and
  `location: message`, and progress when exactly one child is working or
  blocked. Done is forwarded whether or not you have seen it inside Fut, so the
  outer terminal tracks its own read state. At most 64 children are sent,
  strongest first. The terminal running the client itself is never included.
- **Ownership.** While attached, the client owns the outer terminal's status
  records. Its first report starts with `state=clear`, which also removes
  records a crashed client left behind. When nothing is busy it sends
  `state=clear` instead of a done report, and detaching or exiting clears
  everything. Switching machines and remote reconnects keep the same client,
  so its records refresh rather than clear. A client killed with `SIGKILL`
  cannot clear: the outer terminal drops its working and blocked records when
  the process exits, but done and error stay until a new client claims the
  terminal or something sends `state=clear`. The client's alternate screen
  does not affect the records.
- **Rate.** Only changes are sent: changed records, removed children cleared
  by id, root last. Changes go out at most every 100 ms and progress-only
  changes at most every 500 ms.

## Legacy plugin fallbacks

Current releases of Claude Code (2.1.295 and later), Codex, and Pi (1.1.0 and
later) emit OSC 7501 themselves, so they show status in Fut with nothing
installed. Fut still ships first-party lifecycle plugins as legacy fallbacks
for older agent versions:

- [Claude Code](agents/claude-code.md)
- [Codex](agents/codex.md)
- [Pi](agents/pi.md)

The plugins call `fut agent report`, so they work only inside Fut. They do
not reach Fut from inside tmux, which runs outside the Fut terminal's process
tree and also drops OSC 7501. All feed the same status records as native
OSC 7501, so installing a plugin alongside a native reporter is harmless but
unnecessary. One exception
remains: Codex does not report turn completion over OSC 7501, so its
documented `notify` command setting is still the way to get completion
attention. Screen-based Codex detection exists as a lower-priority
fallback; explicit OSC or CLI reports take precedence.

## Control integrated agents

The historical `agent` command names also cover non-agent programs such as
build tools that report OSC 7501. List and inspect explicitly reporting
terminals without changing focus:

```sh
fut --json agent list
fut --json agent get TERMINAL_ID
```

Each listed agent includes an `unread` boolean, and the list result includes
`unread_count` for status bars and other external observers:

```sh
fut --json agent list | jq -r '.result.unread_count'
```

Blocked, done, and error transitions become unread daemon-wide. A done record
waits while any record in the same terminal is still working, so an agent whose
sub-tasks are running stays working until the last one settles or clears.
Rendering that terminal in any attached client marks its observed attention
read for every client and later CLI calls; a newer transition remains unread.
Reading a done record quiets its completion indicator without changing its
stored state.
JSON includes `activity.records`, with app, title, message, kind, progress,
and revision metadata, plus explicit `activity.source` provenance.

An agent can name its Fut pane after its current task:

```sh
fut pane rename 'Review authentication changes'
fut pane rename '' # Clear the task title when finished
```

Fut uses `FUT_TERMINAL_ID` to find the agent's pane, even after a move. The
agent does not need to supply a pane ID. Programs outside Fut can call
`fut pane rename PANE_ID NAME`.

All attached clients see the same pane name. The command replaces any manually
assigned name but does not change OSC 7501 status-record titles. See
[Name panes](../usage/#name-panes) for UI controls and the required capability.

Submit a prompt as one atomic paste-and-Enter operation. Targets are always
explicit, and a currently working agent is rejected with `agent_busy`:

```sh
fut --json agent prompt TERMINAL_ID 'review the failing test'
fut --json agent prompt TERMINAL_ID 'review the failing test' --wait --timeout 2m
fut --json agent prompt TERMINAL_ID --stdin < detailed-prompt.md
fut --json agent wait TERMINAL_ID --timeout 30s
```

Use `--stdin` instead of the text argument for multiline prompts or automation
that should not pass prompt contents through shell argument parsing. Prompt text
is read to EOF and submitted with the same atomic paste-and-Enter behavior.

`prompt --wait` captures the current lifecycle revision, then requires a fresh
`working` transition before it accepts a later `done`, `blocked`, `error`, or
`idle` state. Blocked and error are successful structured outcomes, not command
transport failures. Standalone
`agent wait` returns an already settled agent immediately or waits for a
currently working one. Stable failures include `not_an_agent`, `agent_busy`,
`agent_timeout`, `agent_events_lagged`, `agent_exited`, `terminal_exited`, and
`input_backlog_full`, which means the program has stopped reading its input.

An older stored result does not settle a new turn: waits correlate fresh work
with its own record. An integration that exits or clears its active report while
the terminal stays alive returns `agent_exited`; terminal process death remains
`terminal_exited`.

Read bounded terminal output together with the agent's current state and
`available` flag:

```sh
fut --json agent read TERMINAL_ID
fut --json agent read TERMINAL_ID --source recent-unwrapped --lines 200
```

`available` means the reporting terminal is open and no record is currently
working. Blocked agents remain available for a follow-up prompt when no other
record is still working.

## Interactive agent projections

The global Agents dialog (`Ctrl-b a`) and Agents sidebars list only live,
explicitly reporting terminals, including non-agent OSC programs. The default right sidebar uses the focused
session and stays undocked when that projection is empty. Configure an Agents
component on either side with `scope = "tab"`, `"workspace"`, `"session"`, or
`"global"`. Tab, workspace, and session scope use fresh live focus ancestry
when available and otherwise fall back to the selected IDs; global scope needs
no focus anchor. Any closing agent-row ancestor excludes the row, and
detection-only activity does not qualify. Global rows navigate directly across
sessions by pane ID. The Notifications dialog remains separate and tracks
daemon-wide unread blocked, done, or error attention, including attention
inferred by best-effort screen detection. Rows show the program app, title,
message, blocked kind, and determinate percentage when available. Status text
is plain text and cannot inject presentation tokens or styles.

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

When the foreground process is Codex and no explicit reporter currently owns
its status, Fut infers idle, working, and blocked state from the canonical live
bottom viewport. Client scrollback does not affect detection. Resource JSON
keeps inferred provenance explicit under `activity.source` with type
`detection`, including the matched `rule`. OSC and CLI reports remain
authoritative while their reporting process is active.

When testing this fallback, uninstall or disable existing Fut Codex lifecycle
plugins first. A plugin report intentionally takes precedence and prevents the
screen detector from affecting state. Inference stops and clears when Codex is
no longer the foreground process.

A detected transition from working to idle stores done and creates the same
daemon-wide unread completion attention as an explicit done report.

Set `FUT_AGENT_DETECTION_LOG=1` on the Fut daemon to print each Codex process,
command line, matched rule, state, and quoted canonical screen to daemon stderr.
This diagnostic can include terminal content and should only be enabled while
debugging.

## Report status with the CLI fallback

Report semantic state from an integration with:

```sh
fut agent report working --source codex --agent-session-id SESSION --progress 40 --app codex
fut agent report blocked --kind permission --message 'Apply these changes?'
fut agent report completed --message 'Changes ready for review'
fut agent report idle
fut agent report exited
```

The legacy verbs map onto records: `idle`, `working`, and `blocked` retain
their names, `completed` stores done, and `exited` clears the addressed record
and its descendants (all records when no `--id` is given). Use `--id` for a
child record, `--app` for a stable program identifier, `--title` for a short
label, `--message` for plain decoded text, `--kind` for a blocked reason, and
`--progress` for a percentage. CLI reports replace whole records just like OSC
reports; repeat any fields that should remain. `--turn-id` is accepted and
ignored for older integrations, not stored or used for synchronization.

The terminal defaults to `FUT_TERMINAL_ID`; outside that environment pass
`--terminal-id`. `fut terminal report` remains a compatibility alias and
accepts the same fields. Reports using inherited terminal context are owned
by the reporting command's live process branch, so the integration exits when
that agent process ends even if it cannot send a final `exited` report. Automatic
exit cleanup drops only that reporter's transient records, preserving unrelated
work, native replacements, and done/error results.

Done, blocked, and error transitions create daemon-wide unread attention. A
changed blocked kind, title, or message also marks a new request unread;
progress-only updates do not. Use `Ctrl-b u`
to list waiting terminals and `Ctrl-b Ctrl-b` to jump to the next one. A terminal
that both rings the bell and reports attention is listed and counted once, under
its stronger kind, and clearing it clears both. Rendering a terminal marks its
current attention as read for every attached client and later CLI calls.

## Program status (OSC 7501)

Any program can report its own state by writing the
[Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)
escape sequence to its terminal. No Fut CLI or socket is needed, so it also
works over SSH and inside containers:

```sh
status() {
  printf '\033]7501;state=%s:app=%s\033\\' "$1" "$2"
}
status working rsync
rsync -a ~/Photos backup:/photos && status done rsync || status error rsync
```

Fut answers the `OSC 7501 ; ?` support query and applies root-record reports
like lifecycle reports: `working`, `blocked`, and `idle` map directly, `done`
and `error` become completion attention, and `clear` ends the reported
activity. `app` becomes the integration `source`. Working, blocked, and idle
reports belong to the foreground job and end when it exits; `done` and `error`
remain until seen. Child records (reports with an `id`) roll up into the
root's status, `progress` shows in the `tab.progress` and `workspace.progress`
tokens, and `kind` and `msg` appear in the Agents sidebar and dialog.

## Event stream

Outside tools can subscribe to Fut's state changes instead of polling:

```sh
fut events
```

Each line is versioned JSON with the complete resource snapshot — sessions,
workspaces, tabs, panes, and program status records. The first line is the current
state; every later line is the state after a change. The stream ends when the
daemon exits.
