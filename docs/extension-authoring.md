---
layout: default
title: Extension authoring
description: Build portable Fut extensions against the public process API.
permalink: /extension-authoring/
---

# Extension authoring

Fut extensions are executable packages, not in-process plugins. An extension
can be written in any language that can start as a normal process, read JSON
from standard input, inspect environment variables, and optionally run Fut's
CLI. Fut does not require JavaScript, Lua, Rust, a dynamic library ABI, or a
language-specific SDK.

The current contract intentionally remains a trusted-process contract. A
[capability-sandboxed WebAssembly tier](../decisions/wasm-extension-tier/) was
evaluated and deferred until concrete lower-trust workloads justify its runtime
and security-maintenance cost.

This page is the third-party contract for manifest API version 1. The shorter
[Extensions](../extensions/) page covers installation and use. The
[`rust-status` example][rust-status] is a standalone compiled implementation;
it has no dependencies and checks in source rather than a platform binary.

## Start with a package

A minimal source repository can look like this:

```text
my-extension/
├── fut-extension.toml
├── bin/
│   └── my-extension      # executable file in a distributable package
├── src/                  # optional; Fut does not build this
└── README.md
```

`fut-extension.toml` must be a regular UTF-8 file at the package root. Keep
runtime files under that root and put mutable state, logs, and caches elsewhere.
Managed installs copy the tree to an immutable, read-only store; they reject
symbolic links, sockets, devices, and other special files. Fut does not run an
installer, compiler, package script, or manifest hook.

Build first, then validate the exact directory users will install:

```sh
fut extension validate ./dist/my-extension
fut --json extension validate ./dist/my-extension
```

Validation is daemonless and does not execute package code. It checks the
manifest, compatibility, declarations, paths, and structural limits. It
resolves `./` executables but does not prove that an executable exists, has the
right architecture, or succeeds, so run your own smoke test too.

## Manifest and versions

```toml
api_version = 1
version = "1.2.0"
fut = ">=0.7.0, <1.0.0"
capabilities = ["commands", "hooks", "presentation_tokens"]
id = "review-status"

[commands.open]
title = "Open review"
argv = ["./bin/review-status", "open"]

[hooks]
"workspace.created" = ["./bin/review-status", "event"]

[[presentation_tokens]]
name = "state"
scope = "workspace"
```

All five metadata keys are required:

| Key | Contract |
| --- | --- |
| `api_version` | Integer selecting the manifest and process contract. Fut currently accepts exactly `1`. |
| `version` | This package's valid SemVer version. Increase it when distributable package behavior changes. |
| `fut` | A SemVer requirement matched against the running Fut package version before activation. |
| `capabilities` | The exact set of cooperation surfaces used below. |
| `id` | Stable package namespace used in command slugs, config, environment, and tokens. |

The three versions answer different questions. `api_version` says how to
interpret this file and processes. `version` identifies your release. `fut`
declares which Fut releases you tested and support. A hook JSON object's
`version` is a fourth, independent version for that payload shape.

Choose the narrowest truthful `fut` range, test its lower and upper edges, and
do not use the package version to imply API compatibility. Fut rejects an
unsupported API or unmatched `fut` requirement before activating any part of
the candidate extension set.

IDs, command names, hook names, and token names are 1–64 bytes. They start with
a lowercase ASCII letter, end with a lowercase letter or digit, and otherwise
contain lowercase letters, digits, or non-consecutive `.`, `_`, and `-`
separators. Unknown manifest fields are errors.

## Capabilities are exact declarations

API v1 has exactly three capabilities:

| Capability | Required when |
| --- | --- |
| `commands` | `[commands]` contains at least one command. |
| `hooks` | `[hooks]` contains at least one hook. |
| `presentation_tokens` | At least one `[[presentation_tokens]]` exists. |

Missing, unknown, duplicate, and declared-but-unused capabilities reject the
manifest. They describe which Fut surfaces a package uses; they are not OS
permissions and do not sandbox the executable.

## Direct argv execution

Every command or hook is a non-empty TOML string array. Element zero is the
executable and later elements are literal arguments:

```toml
argv = ["./bin/review-status", "open", "--format=short"]
```

Fut never inserts a shell, expands variables, interprets quotes, resolves
globs, or joins the array into a command string. If shell behavior is part of
your extension, ship and invoke a reviewed script explicitly.

Executable element zero may be:

- `./path`, resolved inside the canonical package root; `..` and other lexical
  escapes are rejected;
- an absolute path; or
- one simple executable name resolved with `PATH`.

Other relative paths are rejected. Arguments are not resolved relative to the
package; use `FUT_EXTENSION_ROOT` when an argument must name another packaged
file. Arguments cannot contain NUL. Command titles must be non-empty and cannot
contain control or bidirectional-formatting characters. Managed packages are
read-only, so never write beside the executable.

## Commands

```toml
[commands.inspect]
title = "Inspect review"
argv = ["./bin/review-status", "inspect"]
size = { width = 100, height = 30 }
activate_opened = true

[commands.refresh]
title = "Refresh review status"
argv = ["./bin/review-status", "refresh"]
mode = "background"
```

The public command slug is `<extension-id>:<command-name>`. It appears in
`Ctrl-b :` and can be bound under `[ui.bindings]`. The default `mode` is
`interactive`: Fut creates a temporary PTY, connects normal terminal input and
output, and restores the prior view when the process exits. Attention from the
covered managed terminal remains unread until that terminal is visible again.
`size` requests a popup width of at least 4 and height of at least 3; omitted
dimensions fill the available terminal and Fut clamps the result.

`activate_opened = true` asks Fut to focus the most recent target opened by a
descendant `fut open` after a successful exit. It defaults to false.

A `background` command receives null stdin and stdout and bounded diagnostic
stderr. Success is silent; spawn and nonzero-exit failures become a toast.
Background commands cannot declare `size` or `activate_opened`. Fut imposes no
runtime timeout on either command mode: an interactive command lives until it
exits or its surface is closed, and a background command lives until it exits.

Both modes run with the focused workspace root as their working directory and
receive the client process environment plus these API v1 values:

| Variable | Value |
| --- | --- |
| `FUT_BIN` | Exact running Fut executable; do not replace it with a `PATH` lookup. |
| `FUT_SOCKET` | Socket belonging to that Fut daemon; pass it back with `--socket`. |
| `FUT_EXTENSIONS` | Compact JSON object mapping every active extension ID to its package SemVer version. |
| `FUT_EXTENSION_ID` | Manifest `id`. |
| `FUT_EXTENSION_COMMAND` | Unqualified command declaration name. |
| `FUT_EXTENSION_ROOT` | Canonical package root. |
| `FUT_SESSION_ID` | Focused session compact ID. |
| `FUT_WORKSPACE_ID` | Focused workspace compact ID. |
| `FUT_TAB_ID` | Focused tab compact ID. |
| `FUT_PANE_ID` | Focused pane compact ID. |
| `FUT_TERMINAL_ID` | Focused terminal compact ID. |
| `FUT_EXTENSION_CONFIG` | Resolved config object as compact JSON; at minimum `{}`. |
| `FUT_EXTENSION_CONFIG_GLOBAL_PATH` | Present only when global config contributed values. |
| `FUT_EXTENSION_CONFIG_WORKSPACE_PATH` | Present only when workspace config contributed values. |
| `FUT_EXTENSION_FORM` | Present only for a command with fields; a compact JSON object mapping every field name to its submitted string. |

Inherited variables that are not listed here are host environment, not Fut
API. Do not give them semantic meaning. Future compatible releases may add new
`FUT_*` variables, so ignore unknown names.

`FUT_EXTENSIONS` describes the atomic extension generation that launched the
process, including the current extension. A managed package that is installed
but disabled, or enabled but not yet loaded by a successful reload, is absent.
Test object membership by ID and use the value only when an integration needs
to distinguish versions.

Users may replace all arguments after your executable with:

```toml
[extension_commands."review-status:inspect"]
args = ["inspect", "--format=long"]
```

Keep the executable useful with alternate direct argv and reject unsupported
arguments clearly.

### Command forms

An interactive command may collect bounded text values in Fut before its
temporary PTY starts:

```toml
[[commands.open.fields]]
name = "branch"
label = "Branch"
placeholder = "generated when empty"

[[commands.open.fields]]
name = "command"
label = "Command"
prefix = "$ "
default_config = "command"

[[commands.open.fields]]
name = "prompt"
label = "Prompt"
```

`name` and `label` are required. `prefix` is cosmetic and is not included in
the submitted value. `placeholder` appears only for an empty inactive field.
Set either a literal string `default` or `default_config`, which names one key
under the resolved `[extension.<id>]` table. A configured default must be a
string or an array of strings; arrays are displayed as a safely quoted command
line. Fields are optional by default and the extension owns semantic
validation after submission.

Tab, Shift-Tab, and the arrow keys move between fields. Enter advances and
submits from the last field; Escape cancels without starting the command. Fut
passes the submitted strings in `FUT_EXTENSION_FORM`. Forms are limited to
interactive commands and do not change the command's direct-argv execution.

## Configuration

Global `config.toml` and a workspace's `.fut/config.toml` share a namespace:

```toml
[extension.review-status]
remote = "origin"

[extension.review-status.display]
compact = true
```

Workspace objects recursively override global objects. Arrays and scalar
values replace rather than merge. TOML strings, integers, finite floats,
booleans, datetimes, arrays, and tables become their JSON equivalents; a TOML
datetime is a JSON string. Fut validates bounds and the extension ID but does
not validate your inner schema.

Parse `FUT_EXTENSION_CONFIG` with a real JSON parser, reject duplicate keys if
your parser permits them, validate every value you use, apply your own defaults,
and produce actionable errors. Commands fail before spawn if workspace config
cannot resolve. Workspace hooks log a warning and fall back first to valid
global-only config, then `{}`; inspect the optional provenance variables when
that distinction matters. Client hooks do not receive extension config.

Configuration is data, not an instruction to Fut. A project is read without
executing configured values; only a later extension command or lifecycle hook
can choose to act on them. Do not start project-configured programs merely
because a workspace was opened unless that behavior is the reviewed purpose of
your hook.

## JSON lifecycle hooks

Supported API v1 hook keys are exactly:

- `workspace.created`, `workspace.renamed`, `workspace.closed`
- `client.attached`, `client.session_changed`, `client.detached`

Hooks receive one UTF-8 JSON object followed by a newline on stdin. Parse the
object, require the payload `version` you implement, require its `event` to
match `FUT_EVENT`, and ignore unknown object members so additive evolution is
safe. Do not parse JSON with regular expressions or depend on member order.

Every workspace event has this shape:

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

Only `workspace.renamed` has `previous_name`. Workspace hooks run from the
package root and inherit the daemon environment plus `FUT_BIN`, `FUT_SOCKET`,
`FUT_EXTENSIONS`, `FUT_EXTENSION_ID`, `FUT_EXTENSION_ROOT`, `FUT_EVENT`,
`FUT_EVENT_VERSION`, `FUT_SESSION_ID`, `FUT_WORKSPACE_ID`,
`FUT_WORKSPACE_ROOT`, resolved config, and its optional provenance paths.

Client events have this shape:

```json
{
  "version": 1,
  "event": "client.session_changed",
  "session": { "id": "SESSION_UUID", "name": "current" },
  "previous_session": { "id": "PREVIOUS_UUID", "name": "previous" }
}
```

Only `client.session_changed` has `previous_session`. Client hooks run from the
package root and inherit the client environment plus `FUT_BIN`, `FUT_SOCKET`,
`FUT_EXTENSIONS`, `FUT_EXTENSION_ID`, `FUT_EXTENSION_ROOT`, `FUT_EVENT`,
`FUT_EVENT_VERSION`, `FUT_SESSION_ID`, and `FUT_SESSION_NAME`. They deliberately
run in the interactive client, where `/dev/tty` is available. They have no
workspace IDs or config.

Workspace hooks run asynchronously after a committed mutation and cannot veto,
delay, rewrite, or roll it back. Hooks are serialized in mutation and extension
order. The initial workspace emits `workspace.created` because extensions load
before daemon setup. Client hooks are similarly serialized per client; reload
changes future events without synthesizing detach or attach. Each invocation
has a five-second deadline. Spawn failure, stdin failure, nonzero exit, output,
or timeout is diagnostic only and does not undo state or stop later hooks.

The queue holds 128 events and never blocks a resource mutation; a new event is
dropped when full. Fut retains at most 16 KiB each of stdout and stderr for
diagnostics. Daemon or client shutdown gives the active hook one second to
finish before cancellation. Make hooks idempotent, bounded, and tolerant of a
target disappearing while they run.

## Publish presentation tokens

Declare every token before publishing it:

```toml
[[presentation_tokens]]
name = "state"
scope = "workspace"

[presentation_tokens.variants.waiting]
text = "…"
style = "divider"

[presentation_tokens.variants.running]
text = "▶"
nerd_font_text = "󰐊"
style = "added"

[presentation_tokens.variants.refreshing]
text = "…"
style = "attention"
presentation = "pulse"

[[presentation_tokens]]
name = "refreshing"
scope = "workspace"
presentation = "spinner"
```

Scopes are `session`, `workspace`, `tab`, and `pane`. `presentation` is `plain`
by default. `spinner` treats any non-empty value as presence and replaces it
with an animated spinner; `pulse` keeps the text and applies a two-second
bright-to-dim pulse using the terminal's faint-text support. A token may instead
declare named `variants`, each with a nonempty `text`, semantic `style`, and
optional presentation. `nerd_font_text` supplies an alternate glyph when
`[ui.icons] preset = "nerd_font"`; for spinners it replaces the animation, while
plain and pulsing variants retain their presentation. Publishing the variant
name displays its configured glyph and style. This keeps state-specific display
choices in the extension manifest while UI layouts configure one stable token.
Styles use the same semantic names as UI segments, such as `divider`,
`attention`, `added`, and `error`. Publish state changes, not animation frames:

```sh
"$FUT_BIN" --socket "$FUT_SOCKET" \
  token publish "$FUT_EXTENSION_ID" state ready \
  --workspace-id "$FUT_WORKSPACE_ID" \
  --action-pane-id "$FUT_PANE_ID"
```

Use the one target option matching the declaration: `--session-id`,
`--workspace-id`, `--tab-id`, or `--pane-id`. The visible UI token name is
`<scope>.extension.<extension-id>.<token-name>`. An empty string suppresses its
segment. Values are unstyled UTF-8 plain text without control or
bidirectional-formatting characters; they cannot carry markup.

A non-empty publication can carry at most one click action. Use
`--action-pane-id PANE` to navigate the clicking client to a live pane beneath
the token target, or `--action-command COMMAND` to run a command declared by
the same extension. The flags are mutually exclusive.
Command names are their unqualified manifest names, such as `logs` for
`run:logs`. Command actions receive the clicked token resource's
workspace and ancestry as their runtime context. Fut rejects actions on empty
values, foreign or undeclared commands, and panes that are closing or outside
the publication target.

Publication goes through the documented CLI using the exact `FUT_BIN` and
`FUT_SOCKET`. A wrong extension, undeclared name, wrong scope, invalid value or
action, closed target, or exhausted daemon-wide materialization limit fails the
CLI call. Treat disappearance as normal cleanup in close/detach races. Values
and actions live in shared resource snapshots, reach all clients, and disappear
with the target. A referenced pane can close later; clients handle that stale
action safely when clicked.

## Public contract versus daemon internals

The supported extension boundary is deliberately small:

- `fut-extension.toml` for a declared API version;
- direct argv, documented `FUT_*` variables, and versioned hook JSON;
- documented noninteractive `fut` commands, especially `token publish`, called
  through the supplied executable and socket; and
- documented versioned `--json` command/result and error envelopes.

The Unix socket's raw framing, MessagePack payloads, handshake, protocol number,
Rust `ClientMessage`/`ServerMessage` types, and daemon-to-client extension
catalog are Fut implementation details. They are not the extension API even
when visible in the source tree. Do not open `FUT_SOCKET` yourself or copy
`src/protocol.rs`; invoke `FUT_BIN --socket FUT_SOCKET ...`. Raw daemon protocol
versions may change independently of manifest API v1.

## Limits and failure model

| Resource | API v1 limit |
| --- | ---: |
| Active/configured extensions | 32 |
| Packages indexed by the managed store | 32 |
| Manifest size | 64 KiB |
| Commands / hooks / presentation tokens | 32 / 32 / 64 |
| Fields per interactive command | 16 |
| Values in one argv array | 64 |
| Bytes in one argv value or command title | 4,096 |
| Identifier bytes | 64 |
| Resolved executable or root path bytes | 4,096 |
| Hook stdin payload | 64 KiB |
| Hook runtime / queued events | 5 seconds / 128 |
| Retained hook stdout and stderr | 16 KiB each |
| Retained background-command failure stderr | 4 KiB |
| Published token value | 1 KiB |
| Materialized extension token values per daemon | 4,096 |
| Extension config keys / nesting depth | 128 / 8 |
| Bytes per config key / serialized scalar | 128 / 4 KiB |
| Values in one config array | 128 |
| Resolved config JSON | 16 KiB |
| One global or workspace Fut config file | 64 KiB |
| Managed package files / total filesystem entries | 1,024 / 2,048 |
| One managed file / total managed content | 16 MiB / 64 MiB |
| Git remote URL | 4,096 bytes |
| Each Git command / retained output stream | 30 seconds / 256 KiB |

Manifest, compatibility, duplicate-root/ID, config, and catalog errors reject
the complete candidate generation. Reload is atomic: failure preserves the
previous daemon catalog and client UI. Validation and install do not execute
code. Command failures affect that invocation only. Hook failures are
diagnostic only. Token publication is atomic for one value and reports whether
it changed the resource revision.

## Trust and distribution

An enabled extension is trusted native code running with the Fut process's
user permissions. It can access the filesystem, network, environment, TTY, and
other processes to the same extent as that user. Capabilities are declarations,
not containment. Review source and built artifacts, minimize dependencies,
avoid embedded secrets, and document every external program and persistent
file your extension uses.

For a local build, validate and install the built package directory:

```sh
fut extension validate ./dist/review-status
fut extension install ./dist/review-status
fut extension enable review-status
fut extension reload
```

Install starts disabled. Fut copies only directories and regular files,
preserves executable intent, records a content SHA-256, makes the copy
read-only, and verifies that digest before load. Reinstalling an ID preserves
its enablement state. Disable before remove; loading and reload never contact a
recorded remote.

Pinned Git installation accepts only HTTPS or an absolute `file:///` URL and
an exact full 40- or 64-hex commit:

```sh
fut extension install-git https://example.com/review-status.git \
  --rev 0123456789abcdef0123456789abcdef01234567 \
  --sha256 89abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567
```

Fut fetches that commit without tags, submodules, embedded credentials,
credential helpers or prompts, hooks, LFS smudging, or build scripts, removes
Git metadata, normalizes permissions, and passes the tree through normal
validation. Therefore a pinned Git commit must already contain the executable
package for the user's platform. For compiled extensions, publish reviewed
per-platform package commits/repositories or ask users to build a local package
and use `extension install`; a source-only commit like the checked-in Rust
example cannot be built by `install-git`.

`--sha256` pins Fut's normalized installed-content digest in addition to the
Git object. Publish the commit and digest over an authenticated channel. Update
is explicit, requires a different full commit, preserves the old selection on
failure, and never reloads automatically.

## Conformance workflow

The repository's minimal conformance tool builds a copied `rust-status`
package in a temporary directory and exercises only supported Fut boundaries:

```sh
scripts/extension-conformance
```

It runs public `fut --json extension validate`, loads the package through
normal config, invokes its command through a public binding, triggers its hook
with public `fut workspace rename`, resolves global plus workspace config, and
observes its token in public `fut --json list` output. Use the fixture as a
starting point for a language-specific package smoke test; keep validation and
at least one real process invocation in release CI.

## API evolution checklist

Before changing or releasing an extension contract:

1. Classify the change. Additive optional data may fit the current API;
   removing, renaming, retyping, changing execution/failure semantics, or
   making an optional value required needs a new manifest `api_version` or
   payload `version` as appropriate.
2. Preserve direct argv and all documented API v1 environment meanings. New
   environment variables and ignorable JSON members must be optional.
3. Update validation before runtime behavior, including exact capabilities,
   bounds, error messages, and atomic rollback coverage.
4. Update this guide, the end-user extension reference, examples, JSON envelope
   documentation, and `Unreleased` together.
5. Extend the conformance fixture through public CLI/process boundaries; never
   make it depend on raw daemon frames or Rust protocol types.
6. Test the oldest and newest Fut versions allowed by each example's `fut`
   requirement, invalid/unknown versions, timeouts, nonzero exits, full queues,
   oversized input/output, disappearing targets, and reload rollback.
7. For compiled packages, build and smoke-test every published OS/architecture,
   verify executable bits, record the exact commit and normalized package
   SHA-256, and confirm installation performs no build step.
8. Review the trust statement and dependencies, then keep old API handling for
   the documented compatibility window or fail early with a precise version
   error.

[rust-status]: https://github.com/mikker/fut/tree/main/examples/extensions/rust-status
