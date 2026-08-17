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

[hooks]
"workspace.created" = ["./bin/workspace-event"]
"workspace.renamed" = ["./bin/workspace-event"]

[[presentation_tokens]]
name = "state"
scope = "workspace"
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
working directory and client environment, and receive:

| Variable | Value |
| --- | --- |
| `FUT_BIN` | Exact running Fut executable |
| `FUT_SOCKET` | Current daemon socket |
| `FUT_EXTENSION_ID` | Manifest ID |
| `FUT_EXTENSION_COMMAND` | Command declaration name |
| `FUT_EXTENSION_ROOT` | Canonical extension directory |

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
`FUT_WORKSPACE_ID`.

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

Extensions can declare plain string values for a resource scope:

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

The hook queue holds 128 events and never blocks resource mutations. Fut
retains at most 16 KiB each of hook stdout and stderr for diagnostics. A new
event is dropped when the queue is full. Daemon shutdown gives the current hook
a one-second grace period before cancellation.

## Examples

The repository includes three complete extensions:

- [`ghostty-title`](https://github.com/mikker/fut/tree/main/examples/extensions/ghostty-title)
  follows the selected Fut session in the current Ghostty window title.

- [`example-workspace-status`](https://github.com/mikker/fut/tree/main/examples/extensions/example-workspace-status)
  turns workspace events into a sidebar token.
- [`wt`](https://github.com/mikker/fut/tree/main/examples/extensions/wt)
  creates a worktree, opens it as a workspace, and runs a configurable action.

Extensions are an executable trust boundary. Only configure directories you
trust, and keep their executables under the same review and permissions policy
as shell scripts in your dotfiles.

## Related

- [Configuration](../configuration/)
- [Presentation tokens](../tokens/)
- [Diagnostics](../doctor/)
