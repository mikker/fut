# fut

`fut` is an agent-oriented terminal multiplexer organized as sessions, workspaces, tabs, panes, and terminals. One daemon owns the live resource tree; clients and automation address resources by their raw IDs.

The current Rust vertical slice supports multiple project sessions, explicitly opened Git worktrees as peer workspaces, multiple tabs, detach/reattach, rename and close operations, and semantic `libghostty-vt` terminal snapshots.

## Run it

```sh
mise install
mise run check

cargo run                          # open the current directory and attach
cargo run -- daemon run            # explicitly run the daemon
cargo run -- open ../api           # control-only; daemon must already exist
cargo run -- open ../api --name api -- /bin/zsh
cargo run -- list
```

`fut open [PATH] [--name NAME] [-- COMMAND...]` replaces the former `new` command. It is always control-only and requires an existing daemon. Bare `fut` is the convenience path that opens the current directory and then attaches to the returned terminal. Attaching to an existing resource is a separate `RESOURCE attach ID` operation; the CLI does not offer an atomic create-and-attach command. Child commands always follow `--`; they are passed as arguments without shell evaluation.

## Control surface

Resource operations are noun-first. Top-level `open` and `list`, plus the `daemon` lifecycle commands, are intentional non-resource entry points:

```sh
fut session attach SESSION
fut session rename SESSION_ID NAME
fut session close SESSION_ID

fut workspace attach WORKSPACE_ID
fut workspace rename WORKSPACE_ID NAME
fut workspace close WORKSPACE_ID

fut tab new WORKSPACE_ID [--name NAME] [--cwd PATH] [-- COMMAND...]
fut tab attach TAB_ID
fut tab rename TAB_ID NAME
fut tab close TAB_ID

fut pane attach PANE_ID
fut pane close PANE_ID
fut terminal attach TERMINAL_ID

fut list
fut daemon run [--cwd PATH] [-- COMMAND...]
fut daemon ping
fut daemon shutdown
```

Mutation commands accept raw IDs only. Attach commands also use raw IDs, except `session attach`, which additionally permits an exact session name as a convenience. A UUID-shaped session value always has ID precedence, even if it could be a session name. Attaching by session, workspace, or tab succeeds only when that ancestor identifies exactly one open terminal; otherwise use an exact pane or terminal ID, or choose through the navigator. Pane and terminal IDs identify their terminal exactly. There is no public `resource:<id>` or selector mini-language; the internal client/daemon protocol remains typed.

`tab new` defaults its working directory to the workspace root; relative `--cwd` values resolve from that root. Names are unique in their scope. Worktrees are discovered only when explicitly opened.

## Automation and interaction

Global `--json` is available only for noninteractive control commands. Successful output has a versioned envelope and dotted command name:

```json
{"version":1,"command":"workspace.rename","result":{"workspace_id":"…","name":"api"}}
```

Human-readable output is not a machine contract. Agents should use `--json`, retain the raw IDs returned by Fut, and pass those IDs to later commands. The TUI uses typed protocol IDs directly and never constructs CLI selectors.

Failures under `--json` are compact and versioned:

```json
{"version":1,"error":{"code":"not_found","message":"daemon error: resource not found"}}
```

Daemon error codes are preserved. CLI argument failures use `invalid_arguments`; failures without a more specific daemon code use `command_failed`.

Enable shell completion from your shell startup file:

```sh
# zsh
source <(COMPLETE=zsh fut)

# bash
source <(COMPLETE=bash fut)

# fish
COMPLETE=fish fut | source
```

The generated integration completes the static command, option, and path grammar. At resource operands it reads the current daemon snapshot and inserts only the full raw ID. Zsh and fish also display Unicode names, full ancestry, workspace roots, and distinct numbered pane or terminal descriptions; Bash's completion protocol does not preserve those descriptions. `session attach` completion inserts IDs even though exact names remain valid when entered manually. Re-source the generated integration after upgrading Fut so it stays in step with the executable.

Completion never starts a daemon or mutates resources. It honors `--socket` and the normal socket environment precedence, omits closing and guaranteed-invalid targets, and uses a short bounded query. If the daemon is absent, stale, incompatible, or slow, completion fails silently while static suggestions remain available.

Inside the client, `Ctrl-b c` creates and switches to a default shell tab, `Ctrl-b g` opens the global navigator, `Ctrl-b d` detaches, and `Ctrl-b Ctrl-b` sends a literal `Ctrl-b`.

Current limitations include one pane and terminal per created tab, no splits or layout mutations, no pane naming or move operation, basic input encoding, and full-grid JSON snapshots. Fuzzy navigation, project recipes, and agent activity remain planned. Explicit pane creation and multi-pane tab lifecycle semantics are the next implementation slice.

See [VISION.md](VISION.md) for the product direction, [CONTEXT.md](CONTEXT.md) for shared language, and [PLAN.md](PLAN.md) for implementation status and exit criteria.
