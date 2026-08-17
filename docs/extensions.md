---
layout: default
title: Extensions
description: Add local commands, lifecycle hooks, and presentation tokens to Fut.
permalink: /extensions/
---

# Extensions

> **TL;DR:** Add a trusted extension directory to the top-level `extensions`
> array, reload with `Ctrl-b Shift-R`, then find its commands in `Ctrl-b :`.
> Extensions may execute programs; only load directories you trust.

Extensions are trusted local packages that add command-palette actions,
lifecycle hooks, and presentation values without changing Fut
itself. They are ordinary directories containing a strict TOML manifest and,
usually, a few executables.

Fut does not discover, download, or install extensions. You choose every
extension explicitly by absolute path:

```toml
extensions = [
  "/Users/me/.config/fut/extensions/review-status",
]
```

Reload configuration with `Ctrl-b Shift-R`. The complete extension set is
validated atomically; an invalid manifest leaves the current configuration in
place. Duplicate canonical roots or IDs reject the complete set.

## Manifest

Every extension has a `fut-extension.toml` at its root:

```toml
id = "review-status"

[commands.open-review]
title = "Open review"
argv = ["./bin/open-review", "--interactive"]
size = { width = 100, height = 30 }
activate_opened = true

[commands.refresh]
title = "Refresh review status"
argv = ["./bin/refresh"]
mode = "background"

[hooks]
"workspace.created" = ["./bin/workspace-event"]
"workspace.renamed" = ["./bin/workspace-event"]

[[presentation_tokens]]
name = "state"
scope = "workspace"

[[presentation_tokens]]
name = "refreshing"
scope = "workspace"
presentation = "spinner"
```

IDs and declaration names are 1–64 bytes. They begin with a lowercase ASCII
letter, end with a lowercase letter or digit, and may otherwise contain
lowercase letters, digits, and single `.`, `_`, or `-` separators. Unknown
manifest fields are errors.

Executable arrays are direct argv—not shell commands. Fut resolves `./bin/x`
inside the canonical extension root and rejects lexical escapes from it. An
executable may instead be an absolute path or a single name resolved through
`PATH`. Fut never inserts a shell or interpolates manifest values.

## Palette commands

Each `[commands.NAME]` needs a display `title` and non-empty `argv`:

```toml
[commands.diff]
title = "Repository diff"
argv = ["./bin/diff", "--color=always"]
size = { width = 120, height = 40 }
```

Commands appear in `Ctrl-b :` as a stable qualified slug followed by their
title, such as `review-status:open-review  Open review`. Both are searchable.
Commands run in an interactive temporary PTY, inherit the focused pane's live
working directory and client environment. Extension commands run from the
focused workspace root and receive:

| Variable | Value |
| --- | --- |
| `FUT_BIN` | Exact running Fut executable |
| `FUT_SOCKET` | Current daemon socket |
| `FUT_EXTENSION_ID` | Manifest ID |
| `FUT_EXTENSION_COMMAND` | Command declaration name |
| `FUT_EXTENSION_ROOT` | Canonical extension directory |
| `FUT_SESSION_ID` | Focused session UUID |
| `FUT_WORKSPACE_ID` | Focused workspace UUID |
| `FUT_TAB_ID` | Focused tab UUID |
| `FUT_PANE_ID` | Focused pane UUID |
| `FUT_TERMINAL_ID` | Focused terminal UUID |
| `FUT_EXTENSION_CONFIG` | Resolved `[extension.<id>]` table as compact JSON |
| `FUT_EXTENSION_CONFIG_GLOBAL_PATH` | Global source path, when it contributed values |
| `FUT_EXTENSION_CONFIG_WORKSPACE_PATH` | Workspace source path, when it contributed values |

Set `mode = "background"` for a non-interactive command. Fut starts it
asynchronously without opening a temporary surface, keeps successful commands
silent, and shows a bounded spawn or nonzero-exit error as a toast. Background
commands have null stdin and stdout and may not declare `size` or
`activate_opened`; stderr is retained only for the bounded failure diagnostic.
The default mode is `interactive`.

`size` requests the centered outer popup dimensions. Omit either dimension to
fill the terminal in that direction. Fut clamps dimensions to the available
terminal; width must be at least 4 columns and height at least 3 rows. Omitting
`size` preserves the full-terminal surface.

Set `activate_opened = true` when the command should switch the parent Fut
client to the most recent target opened by a descendant `fut open` after the
command exits successfully. It defaults to `false`, so commands can create
background workspaces without changing focus.

Bind an extension command under `[ui.bindings]` with its quoted qualified
palette slug:

```toml
[ui.bindings]
"review-status:open-review" = "r"
```

Bound extension commands also appear in delayed which-key help. Use a
top-level `[trusted_commands.NAME]` entry for a direct executable that is not
supplied by an extension.

Override an extension command's manifest arguments by its same qualified slug:

```toml
[extension_commands."review-status:open-review"]
args = ["--local-choice"]
```

The configured array replaces the manifest arguments after its executable;
use `args = []` to pass none. Unknown command slugs reject the configuration.

## Extension configuration

Extensions can receive bounded, namespaced data from the global Fut config and
the focused workspace's `.fut/config.toml`. Both locations use the same shape:

```toml
[extension.review-status]
remote = "origin"
[extension.review-status.display]
compact = true
```

Workspace values recursively override global defaults; arrays and scalar
values replace their global value. Fut rejects tables for extension IDs that
are not explicitly loaded, limits nesting, key counts, arrays, scalar sizes,
and the resolved payload, and retains the contributing paths for diagnostics.
It does not interpret an extension's inner table—that validation belongs to
the extension. The resolved object is exposed as JSON through
`FUT_EXTENSION_CONFIG`; it is `{}` when neither source configures the extension.

Workspace configuration is read only when an extension command or workspace
hook is explicitly invoked. Reading or entering a project never executes the
configured values. A lifecycle hook may publish status derived from the table,
but only an explicit command can start project-defined work.

## Workspace hooks

The supported hooks are:

- `workspace.created`
- `workspace.renamed`
- `workspace.closed`

```toml
[hooks]
"workspace.created" = ["./bin/workspace-event"]
"workspace.renamed" = ["./bin/workspace-event"]
"workspace.closed" = ["./bin/workspace-event"]
```

Hooks run asynchronously after a successful resource commit. They cannot
veto, delay, rewrite, or roll back it. Fut executes hooks one at a time in
mutation order, with the extension root as their working directory and a
five-second timeout. The initial workspace emits `workspace.created` because
extensions load before daemon setup.

The hook receives one versioned JSON object on stdin:

```json
{
  "version": 1,
  "event": "workspace.renamed",
  "resource_revision": 42,
  "session_id": "SESSION_UUID",
  "workspace": {
    "id": "WORKSPACE_UUID",
    "name": "new-name",
    "root": "/absolute/workspace/root"
  },
  "previous_name": "old-name"
}
```

Only rename events include `previous_name`. Hook processes inherit the daemon
environment plus `FUT_BIN`, `FUT_SOCKET`, `FUT_EXTENSION_ID`,
`FUT_EXTENSION_ROOT`, `FUT_EVENT`, `FUT_EVENT_VERSION`, `FUT_SESSION_ID`, and
`FUT_WORKSPACE_ID`. Workspace hooks also receive the resolved
`FUT_EXTENSION_CONFIG` and its optional global/workspace provenance variables
described above, plus `FUT_WORKSPACE_ROOT`, the absolute root of the workspace
in the event.

Hook failures are diagnostic only. Spawn errors, nonzero exits, output, and
timeouts do not alter daemon state or stop later hooks.

## Client hooks

Client hooks run for each attached interactive client:

- `client.attached` runs after Fut resolves the initially selected session.
- `client.session_changed` runs when that client selects or renames a session.
- `client.detached` runs when that client exits or detaches.

```toml
[hooks]
"client.attached" = ["./bin/client-event"]
"client.session_changed" = ["./bin/client-event"]
"client.detached" = ["./bin/client-event"]
```

These hooks use the same direct argv, timeout, ordering, output bounds, and
diagnostic-only failure behavior as workspace hooks. They run from the client
process, making `/dev/tty` available for deliberate host-terminal integration.
Configuration changes to client hooks take effect when the client next
attaches.

The hook receives one versioned JSON object on stdin:

```json
{
  "version": 1,
  "event": "client.session_changed",
  "session": {
    "id": "SESSION_UUID",
    "name": "current-session"
  },
  "previous_session": {
    "id": "PREVIOUS_SESSION_UUID",
    "name": "previous-session"
  }
}
```

`previous_session` appears only for `client.session_changed`. Hook processes
inherit the client environment plus `FUT_BIN`, `FUT_SOCKET`,
`FUT_EXTENSION_ID`, `FUT_EXTENSION_ROOT`, `FUT_EVENT`, `FUT_EVENT_VERSION`,
`FUT_SESSION_ID`, and `FUT_SESSION_NAME`.

## Presentation tokens

Extensions can declare bounded values and their client presentation for a
resource scope:

```toml
[[presentation_tokens]]
name = "state"
scope = "workspace"
```

Scopes are `session`, `workspace`, `tab`, and `pane`. Publish a value through
the normal Fut control socket:

```sh
"$FUT_BIN" --socket "$FUT_SOCKET" \
  token publish review-status state ready \
  --workspace-id "$FUT_WORKSPACE_ID"
```

The target option must match the declaration scope. The UI token is qualified
as `<scope>.extension.<extension-id>.<name>`:

```toml
[ui.sidebar.left]
components = [
  { component = "workspaces", row = { right = [{ token = "workspace.extension.review-status.state" }] } },
]
```

Presentation defaults to `plain`: the client renders the validated published
string exactly as token text. `presentation = "spinner"` instead treats any
non-empty published value as presence and renders an animated spinner using
the client's existing 100 ms clock. Empty still suppresses the segment and its
affixes. Animation is entirely client-side—publication happens only when the
extension's state changes, never once per frame—and does not give plain token
values a control or styling channel.

Values are plain text. They cannot inject styles, actions, or executable
behavior. Unpublished values are empty; published values live in ordinary
resource snapshots, are shared by every attached client, and disappear when
their target closes. See [Presentation tokens](../tokens/) for compatible UI
contexts.

## Limits and failure behavior

| Resource | Limit |
| --- | ---: |
| Configured extensions | 32 |
| Manifest size | 64 KiB |
| Commands per manifest | 32 |
| Hooks per manifest | 32 |
| Presentation tokens per manifest | 64 |
| Arguments per executable array | 64 |
| Bytes per argument | 4,096 |
| Hook stdin payload | 64 KiB |
| Published token value | 1 KiB |
| Materialized token values per daemon | 4,096 |
| Extension config keys | 128 |
| Extension config nesting depth | 8 |
| Values in one extension config array | 128 |
| Resolved extension config JSON | 16 KiB |

The hook queue holds 128 events and never blocks resource mutations. Fut
retains at most 16 KiB each of hook stdout and stderr for diagnostics. A new
event is dropped when the queue is full. Daemon shutdown gives the current hook
a one-second grace period before cancellation.

## Examples

The repository includes four complete extensions:

- [`ghostty-title`](https://github.com/mikker/fut/tree/main/examples/extensions/ghostty-title)
  follows the selected Fut session in the current Ghostty window title.

- [`example-workspace-status`](https://github.com/mikker/fut/tree/main/examples/extensions/example-workspace-status)
  turns workspace events into a sidebar token.
- [`wt`](https://github.com/mikker/fut/tree/main/examples/extensions/wt)
  creates a worktree, opens it as a workspace, and runs a configurable action.
- [`run`](https://github.com/mikker/fut/tree/main/examples/extensions/run)
  explicitly manages one long-running command per workspace, with safe
  restart/stop, optional output readiness, live logs, and styled status tokens.

Extensions and their namespaced project configuration are an executable trust
boundary. Merely loading `[extension.<id>]` does not run it, but invoking the
extension may deliberately execute its values. Only configure directories and
projects you trust, and keep their executables under the same review and
permissions policy as shell scripts in your dotfiles.

## Related

- [Configuration](../configuration/)
- [Presentation tokens](../tokens/)
- [Diagnostics](../doctor/)
