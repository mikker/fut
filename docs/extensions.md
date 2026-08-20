---
layout: default
title: Extensions
description: Add local commands, lifecycle hooks, and presentation tokens to Fut.
permalink: /extensions/
---

# Extensions

> **TL;DR:** Install a reviewed local package with `fut extension install
> PATH`, or one exact Git commit with `fut extension install-git URL --rev
> COMMIT`; enable its ID, then run `fut extension reload`. Extensions are
> trusted code that may execute programs with your user permissions.

Building a third-party package? Use the complete, language-neutral
[Extension authoring guide](../extension-authoring/) for the API v1 process
contract, compatibility policy, conformance workflow, and release checklist.
Fut has also evaluated a lower-trust WebAssembly tier and
[deferred it pending concrete demand](../decisions/wasm-extension-tier/); no
WebAssembly runtime is part of the current extension system.

Extensions are trusted local packages that add command-palette actions,
lifecycle hooks, and presentation values without changing Fut
itself. They are ordinary directories containing a strict TOML manifest and,
usually, a few executables.

Fut never discovers extensions or checks for updates. You can copy a local
package into Fut's managed store, fetch one explicitly named immutable Git
commit, or choose a directory explicitly by absolute path:

```toml
extensions = [
  "/Users/me/.config/fut/extensions/review-status",
]
```

Reload configuration with `Ctrl-b Shift-R`. Fut first stages and validates the
complete local UI, then asks the daemon to prepare the complete extension set.
Neither becomes visible until both candidates are valid and the daemon commits
the reload against the generation that was prepared. An invalid UI, manifest,
or concurrent stale reload leaves the active daemon extensions and client UI in
place. Enabled managed roots are appended to these explicit paths. Duplicate
canonical roots or IDs reject the complete set.

The daemon is authoritative for validated manifest declarations. Each attached
client receives one complete generation containing the commands, hooks,
presentation tokens, and extension defaults it needs; clients do not reread
manifests to reconstruct that state. A successful reload publishes the same
complete generation to every attached client, and a reconnect receives the
latest generation with its welcome. An open command palette is closed during
the swap so an entry from the old generation cannot select a command by a stale
position.

## Install and manage local packages

The managed store makes a bounded, immutable copy of a local package:

```sh
fut extension install ./review-status
fut extension enable review-status
fut extension reload
```

`install` is daemonless and starts a new package disabled. It canonicalizes the
source path, copies directories and regular files without following symbolic
links, rejects special files, enforces the package limits below, validates the
copied manifest, and atomically renames the staged copy into a path containing
the extension ID, version, and SHA-256 content digest. It never runs an
installer, manifest script, command, or hook. Reinstalling an ID preserves its
enabled state and leaves the prior indexed package intact if staging,
validation, or the atomic index update fails.

Fut records the ID, version, canonical source, content SHA-256, immutable
install path, and enabled state in a strict, versioned, human-readable
`index.json` under `$XDG_DATA_HOME/fut/extensions`, falling back to
`~/.local/share/fut/extensions`. This Fut-owned index is the managed
configuration source; enable and disable update it atomically and never rewrite
`config.toml`. A missing store is an empty managed set. Enabled content is
rehash-verified before every load, so modification causes loading to fail
closed.

Enable and disable affect the next daemon load. Run `fut extension reload` to
apply the change to a running daemon; until then its prior active catalog may
remain in memory. Removal deliberately has no force option: disable the package
first, reload if a daemon is running, then remove it:

```sh
fut extension disable review-status
fut extension reload
fut extension remove review-status
```

Removal unindexes the package immediately. Fut retains its immutable bytes so
an older catalog already held by a running daemon cannot acquire dangling
command or hook paths; those orphaned bytes are reserved for a future safe
garbage-collection pass.

Installation is not an endorsement or a sandbox. Enabling a managed extension
is the same executable trust decision as adding an explicit root: its declared
commands and hooks run with your user permissions. Review the copied local code
before enabling it.

## Install a pinned Git package

Fut supports one deliberately narrow remote workflow for repositories served
over HTTPS or an absolute `file:///` URL:

```sh
fut extension install-git https://example.com/review-status.git \
  --rev 0123456789abcdef0123456789abcdef01234567
fut extension enable review-status
fut extension reload
```

`--rev` is required and accepts only a full 40- or 64-character hexadecimal
commit SHA. Branches, tags, abbreviated SHAs, and `HEAD` are rejected. The
fetched object must itself be that commit, so a tag object cannot stand in for
one. There is no registry, package-name lookup, dependency resolution, or
background update check.

Git acquisition happens in a private temporary directory with a per-command
timeout and output limit. Fut fetches only the named commit, without tags or
submodules, checks it out detached, and removes Git metadata before handing the
tree to the normal package installer. Git and package hooks, credential
helpers, submodules, LFS smudge filters, installer scripts, and build scripts
are not run. Only HTTPS and absolute local-file remotes are accepted. The tree
is rejected before installation if it exceeds the package entry or byte limits,
contains a submodule, symbolic link, or special file, or has an invalid
manifest.

Git checkout permissions are normalized from Git's executable bit before Fut
computes its usual package SHA-256. To require bytes published by a package
author, pass that installed-content digest explicitly:

```sh
fut extension install-git https://example.com/review-status.git \
  --rev 0123456789abcdef0123456789abcdef01234567 \
  --sha256 89abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567
```

A mismatch leaves any installed package with that ID selected and unchanged.
The strict store index records the remote URL, canonical commit, normalized
content digest, immutable install path, version, and enabled state. Local
entries from `fut extension install PATH` retain their existing `source`
record.

Updates are equally explicit. `update` is available only for a package already
installed from Git, reuses its recorded remote URL, requires a different full
commit SHA, and accepts the same optional digest check:

```sh
fut extension update review-status \
  --rev fedcba9876543210fedcba9876543210fedcba98 \
  --sha256 76543210fedcba9876543210fedcba9876543210fedcba9876543210fedcba98
fut extension reload
```

The result reports the old and new version, commit, and content digest. Fetch,
digest, compatibility, manifest, or index failures preserve the installed and
active version. Installation and update never reload automatically. Once a
commit is installed, normal loading, catalog listing, enable/disable, and
removal use only the local store and do not contact the recorded remote.

## Inspect, validate, and reload

Use the noun-first extension commands to inspect the daemon's active catalog:

```sh
fut extension list
fut extension show review-status
fut extension reload
```

`list` and `show` report the daemon-authoritative generation, fingerprint,
package version and compatibility requirement, capabilities, canonical package
root, and configuration provenance. `show` also includes the package's hooks,
commands, presentation tokens, and active namespaced defaults. `reload` asks the
daemon to validate and atomically activate its currently configured package
set. A failed reload leaves the prior generation active. This control command
does not replace the interactive `Ctrl-b Shift-R` flow, which continues to
stage local UI configuration and the daemon catalog together before commit.

Validate a directory before adding or activating it:

```sh
fut extension validate ./review-status
fut --json extension validate ./review-status
```

Validation is daemonless and side-effect-free. It reads the package directory,
strictly validates `fut-extension.toml`, resolves packaged argv, and checks the
manifest API plus its `fut` requirement against the running binary. It never
starts a daemon, activates the package, or executes extension code. All
extension management, inspection, validation, and reload commands support
`--json`; successes use Fut's versioned command/result envelope and failures use
its versioned typed error envelope. Shell completion offers active extension
IDs for `show` when a daemon is reachable.

## Manifest

Every extension has a `fut-extension.toml` at its root:

```toml
api_version = 1
version = "1.2.0"
fut = ">=0.7.0, <1.0.0"
capabilities = ["commands", "hooks", "presentation_tokens"]
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

All five top-level metadata fields are required. `api_version` selects the
manifest and process contract; Fut currently accepts exactly `1`. `version`
is the extension package's SemVer version, while `fut` is a SemVer requirement
matched against the running Fut package version before the extension can be
activated. Invalid versions, unsupported API versions, and incompatible Fut
requirements reject the complete candidate extension set.

`capabilities` must contain exactly the cooperation surfaces used by the
manifest: `commands` when `[commands]` is non-empty, `hooks` when `[hooks]` is
non-empty, and `presentation_tokens` when any `[[presentation_tokens]]` are
present. Unknown, duplicate, missing, and declared-but-unused capabilities are
errors. Capabilities let Fut validate what a package asks Fut to provide; they
are not operating-system permissions or a sandbox. Extensions are trusted
native executables and retain the access of the Fut process that starts them.

IDs and declaration names are 1–64 bytes. They begin with a lowercase ASCII
letter, end with a lowercase letter or digit, and may otherwise contain
lowercase letters, digits, and single `.`, `_`, or `-` separators. Unknown
manifest fields are errors.

### API version 1 contract

API version 1 covers direct argv execution, the documented `FUT_*`
environment variables, resolved extension configuration, version-1 hook JSON
payloads, and control through the exact `FUT_BIN` and `FUT_SOCKET` supplied to
the extension. The extension-specific control surface is `fut token publish`;
extensions may also invoke Fut's documented non-interactive commands through
that binary and socket. Hook payload `version` is versioned independently from
the manifest's `api_version`, so consumers must validate it before reading an
event.

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
Commands run in an interactive temporary PTY with the client environment.
Extension commands run from the focused workspace root and receive:

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
| `FUT_EXTENSION_TRUSTED_CONFIG` | Global and trusted-project layers only, as compact JSON |
| `FUT_EXTENSION_CONFIG_GLOBAL_PATH` | Global source path, when it contributed values |
| `FUT_EXTENSION_CONFIG_PROJECT_PATH` | Trusted project recipe path, when it contributed values |
| `FUT_EXTENSION_CONFIG_WORKSPACE_PATH` | Workspace source path, when it contributed values |
| `FUT_EXTENSION_FORM` | Submitted command fields as compact JSON, when declared |

Interactive commands may declare `[[commands.NAME.fields]]` entries with a
required `name` and `label`, plus optional `prefix`, `placeholder`, `default`,
or `default_config`. Fut collects those values in a native form before opening
the command surface. `default_config` reads one string or string-array key from
the resolved extension config; a prefix such as `$ ` is display-only. The
command receives all submitted strings in `FUT_EXTENSION_FORM`. See the
[extension authoring guide](extension-authoring.md#command-forms) for the full
contract.

Set `mode = "background"` for a non-interactive command. Fut starts it
asynchronously without opening a temporary surface, keeps successful commands
silent, and shows a bounded spawn or nonzero-exit error as a toast. Background
commands have null stdin and stdout and may not declare `size` or
`activate_opened` or fields; stderr is retained only for the bounded failure diagnostic.
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

Extensions can receive bounded, namespaced data from global Fut config, a
trusted `.fut/project.toml`, and the focused workspace's `.fut/config.toml`.
All locations use the same shape:

```toml
[extension.review-status]
remote = "origin"
[extension.review-status.display]
compact = true
```

Values recursively layer global → trusted project → workspace; arrays and
scalar values replace the less specific value. Fut rejects tables for
extension IDs that are not explicitly loaded, limits nesting, key counts,
arrays, scalar sizes, and the resolved payload, and retains contributing paths
for diagnostics. It does not interpret an extension's inner table—that
validation belongs to the extension. The complete resolved object is exposed
through `FUT_EXTENSION_CONFIG`. `FUT_EXTENSION_TRUSTED_CONFIG` omits the
workspace layer, allowing lifecycle hooks to gate automatic execution on only
globally configured or exactly approved project values.

Workspace configuration is read only when an extension command or resource
hook is invoked. Trusted extension code ultimately decides how to use it. The
bundled run and wt extensions gate automatic `auto_start` and `open_existing`
behavior on `FUT_EXTENSION_TRUSTED_CONFIG`, so workspace-local values cannot
enable those operations.

## Resource hooks

The supported hooks are:

- `session.created`
- `workspace.created`
- `workspace.renamed`
- `workspace.closed`

```toml
[hooks]
"session.created" = ["./bin/session-event"]
"workspace.created" = ["./bin/workspace-event"]
"workspace.renamed" = ["./bin/workspace-event"]
"workspace.closed" = ["./bin/workspace-event"]
```

Hooks run asynchronously after a successful resource commit. They cannot
veto, delay, rewrite, or roll back it. Fut executes hooks one at a time in
mutation order, with the extension root as their working directory and a
five-second timeout. The initial workspace emits both `session.created` and
`workspace.created` because extensions load before daemon setup.
`session.created` runs once for a new live project session and carries its
initial workspace context.

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
`FUT_WORKSPACE_ID`. Resource hooks also receive `FUT_EXTENSION_CONFIG`,
`FUT_EXTENSION_TRUSTED_CONFIG`, and their optional global/project/workspace
provenance variables described above, plus `FUT_WORKSPACE_ROOT`, the absolute
root of the workspace in the event.

Hook failures are diagnostic only. Spawn errors, nonzero exits, output, and
timeouts do not alter daemon state or stop later hooks.

## Client hooks

Client hooks run for each attached interactive client:

- `client.attached` runs after Fut resolves the initially selected session.
- `client.session_changed` runs when that client selects or renames a session.
- `client.detached` runs when that client exits or detaches.

Reloading reconfigures future client-hook events without synthesizing a detach
or attach. A hook process already running may finish with the generation that
started it; later lifecycle events use the newly committed generation.

```toml
[hooks]
"client.attached" = ["./bin/client-event"]
"client.session_changed" = ["./bin/client-event"]
"client.detached" = ["./bin/client-event"]
```

These hooks use the same direct argv, timeout, ordering, output bounds, and
diagnostic-only failure behavior as resource hooks. They run from the client
process, making `/dev/tty` available for deliberate host-terminal integration.
Configuration changes to client hooks take effect after the committed catalog
swap for that client.

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
| Files in one managed package | 1,024 |
| Filesystem entries in one managed package | 2,048 |
| Size of one managed package file | 16 MiB |
| Total managed package content | 64 MiB |
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

## Bundled extensions and examples

The repository includes three ready-to-use extensions:

- [`ghostty-title`](https://github.com/mikker/fut/tree/main/extensions/ghostty-title)
  follows the selected Fut session in the current Ghostty window title.
- [`wt`](https://github.com/mikker/fut/tree/main/extensions/wt)
  creates a worktree, opens it as a workspace, and runs a configurable action.
- [`run`](https://github.com/mikker/fut/tree/main/extensions/run)
  explicitly manages one long-running command per workspace, with safe
  restart/stop, optional output readiness, live logs, and styled status tokens.

Two complete examples demonstrate extension authoring patterns:

- [`example-workspace-status`](https://github.com/mikker/fut/tree/main/examples/extensions/example-workspace-status)
  turns workspace events into a sidebar token.
- [`rust-status`](https://github.com/mikker/fut/tree/main/examples/extensions/rust-status)
  is a dependency-free compiled Rust example covering commands, hooks, JSON
  config, and token publication without checking in a platform binary.

Extensions and their namespaced project configuration are an executable trust
boundary. Merely loading `[extension.<id>]` does not run it, but invoking the
extension may deliberately execute its values. Only configure directories and
projects you trust, and keep their executables under the same review and
permissions policy as shell scripts in your dotfiles.

## Related

- [Extension authoring](../extension-authoring/)
- [WebAssembly sandbox-tier decision](../decisions/wasm-extension-tier/)
- [Configuration](../configuration/)
- [Presentation tokens](../tokens/)
- [Diagnostics](../doctor/)
