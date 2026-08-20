# Run extension

One managed long-running command per workspace: restart it with a keystroke,
watch its status in the tab bar, follow its log, and trust that closing the
workspace cleans it up. Commands remain manual by default. Trusted global or
project-recipe configuration may opt into starting the runner when a workspace
is created; workspace-local overrides can never enable automatic execution.

## Configuration

The command comes from the shared `[extension.run]` table in global Fut config,
a trusted `.fut/project.toml`, or a workspace's `./.fut/config.toml`. Values
layer in that order. Fut resolves the table and hands it to this extension;
the extension validates the inner keys:

```toml
[extension.run]
command = ["just", "run"]   # required argv array; never a shell string
grace_seconds = 3           # optional, 0–120: SIGTERM→SIGKILL escalation delay
signal = "STATUS:READY"      # optional literal marker in combined stdout/stderr
auto_start = true            # optional; trusted global/project config only
```

Put `auto_start` in `.fut/project.toml` when the project should run the command
once in the first workspace as its session starts. New workspaces and worktrees
do not start it again. The trusted command and all settings used for that
automatic start come from global configuration plus the exact approved project
recipe. A `.fut/config.toml` may still override manual `run:restart`, but cannot
affect automatic execution.

Load the extension and bind restart:

```toml
extensions = ["/absolute/path/to/fut/extensions/run"]

[ui.bindings]
"run:restart" = "r"

[ui.tab_bar]
right = [
  { segments = [{ token = "workspace.extension.run.pause", style = "divider", inverted = true, pill = true }], priority = 220 },
  { segments = [{ token = "workspace.extension.run.launching", style = "attention", inverted = true, pill = true }], priority = 220 },
  { segments = [{ token = "workspace.extension.run.play", style = "added", inverted = true, pill = true }], priority = 220 },
  { segments = [{ token = "workspace.extension.run.stop", style = "divider", inverted = true, pill = true }], priority = 220 },
  { segments = [{ token = "workspace.extension.run.cross", style = "error", inverted = true, pill = true }], priority = 220 },
]

[ui.sidebar.left]
components = [
  { component = "workspaces", row = { right = [{ token = "workspace.extension.run.status" }] } },
]
```

With the Nerd Font icon preset, these render as theme-adaptive filled pills.
Unicode and ASCII presets omit the unavailable cap glyphs but keep the status
content inverted.

## Commands

| Command | Mode | Behavior |
| --- | --- | --- |
| `run:restart` | background | Gracefully stops the current runner, then starts exactly one replacement. Silent on success; launch failures surface as a toast. |
| `run:stop` | background | Stops without restarting. A no-op when nothing runs; malformed config falls back to the default three-second grace so a typo cannot strand the process. |
| `run:logs` | interactive | `less -R +F` on the workspace log. `Ctrl-C` leaves follow mode, `F` resumes, `v` opens VISUAL/EDITOR, `q` returns to Fut. |
| `run:edit-logs` | interactive | Opens the log directly in `VISUAL`, else `EDITOR`, else `vi`. |

## Status tokens

The manifest declares five style-specific workspace tokens; the extension
keeps exactly one populated (publishing the new one before clearing the rest,
so the indicator never blinks):

| Token | Glyph | Meaning |
| --- | --- | --- |
| `workspace.extension.run.pause` | … | configured, not started yet |
| `workspace.extension.run.launching` | animated spinner | child started but has not emitted the configured `signal` |
| `workspace.extension.run.play` | ▶ | running |
| `workspace.extension.run.stop` | ■ | exited cleanly (code 0) |
| `workspace.extension.run.cross` | ✗ | stopped, killed, signaled, or nonzero exit |

The additional `workspace.extension.run.status` token carries the same current
glyph in one stable token, using a static `…` while launching. It is intended
for a compact workspace row where per-state styling and animation are less
important than configuring the status as one segment.

The example uses dark-gray pause/stop, yellow launching, green play, and red
cross pills. A workspace without run configuration publishes nothing.

When `signal` is configured, every restart begins in `launching`. The
supervisor searches the combined stdout/stderr byte stream for the exact UTF-8
literal, including when it spans read chunks, and switches that generation to
`play` once. A restart discards the previous generation's readiness. Exiting
before the marker still yields `stop` for code 0 or `cross` for a nonzero exit,
signal, or explicit termination. Without `signal`, start continues to publish
`play` immediately and remains there until exit or stop.

## How it works

State lives under `${XDG_STATE_HOME:-~/.local/state}/fut-run/<socket-hash>/
<workspace-id>/`, so parallel worktrees, logical workspaces, and separate
daemons never share process, state, token, or log ownership.

- A detached **supervisor** process owns each runner generation. It holds
  `supervisor.lock` (flock) for its whole life — that lock, not any recorded
  PID, is the liveness truth, so a stale `state.json` can never kill an
  unrelated process.
- The child runs in its **own process group** with combined stdout/stderr
  piped through the supervisor into the log. Stop and restart SIGTERM the
  supervisor, which forwards SIGTERM to the group and escalates to SIGKILL
  after `grace_seconds`.
- `control.lock` serializes restart/stop/close end to end, and every restart
  bumps a **generation** in `state.json`. A superseded supervisor records and
  publishes nothing, which settles rapid-restart and natural-exit-versus-
  restart races: the newest generation always owns the final token.
- The launching token is published once per state transition. Its manifest
  declares `presentation = "spinner"`, so each client renders frames using
  Fut's existing 100 ms clock; no extension or Fut subprocess runs per frame.
- The log is a stable `run.log` with start/stop/exit separators. At 2 MiB its
  content is copied to `run.log.1`, then the open `run.log` is truncated in
  place. This bounds disk use to two files while preserving the inode, so an
  already-open `less +F` continues following across rotations.
- `workspace.closed` terminates the runner and deletes its state directory.
  Client detach touches nothing — the runner keeps going.

## Dependencies

`python3` (3.9+; only the standard library), `less`, and a POSIX `sh` for the
smoke test. macOS and Linux only (flock + process groups).

## Known limitations

- If the daemon exits without closing its workspaces (crash, `kill -9`),
  runners keep going but their state directories are keyed by the old
  workspace IDs; stop them manually and delete the state directory.
- A run command that daemonizes grandchildren into their own sessions can
  keep the log pipe open after the command itself exits; the supervisor
  records the exit after a short drain and the strays stay outside its group.

## Smoke test

```sh
./test/smoke
```

Runs against a stub `fut` binary and a temporary state root — no daemon
needed. It covers config validation, chunk-spanning readiness and restart
reset, lifecycle and token transitions, launch failure, five concurrent
restarts, workspace isolation, stale-PID safety, SIGKILL escalation,
inode-stable log rotation, bounded close cleanup, both hooks, and viewer
plumbing.
`FUT_RUN_STATE_DIR` and `FUT_RUN_MAX_LOG_BYTES` exist for exactly this kind
of testing.
