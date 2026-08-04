# fut

`fut` is an agent-oriented terminal multiplexer organized as sessions, workspaces, tabs, panes, and terminals. One daemon owns the live resource tree; clients and automation address resources by their raw IDs.

The current Rust vertical slice supports multiple project sessions, explicitly opened Git worktrees as peer workspaces, multiple tabs and panes, detach/reattach, rename, move, and close operations, semantic `libghostty-vt` terminal snapshots, responsive focus-biased multi-pane rendering with client-local focus, live tab and workspace chrome, a searchable typed command bar, and reconciliation after external resource changes.

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

For a disposable dogfooding environment with three linked-worktree workspaces, seven tabs, and nine panes, run:

```sh
mise run demo
```

The demo builds the current binary, creates its fixture under `target/fut-demo`, starts an isolated daemon and socket, and attaches to the main shell. It includes interactive shells, multi-pane tabs, and slowly updating server/test/preview tabs for exercising the tab bar, workspace sidebar, accordion, global navigator, and command bar. Detach with `Ctrl-b d`, then use `mise run demo:attach` to return or `mise run demo:clean` to stop and remove it. `mise run demo:setup` creates the same fixture without attaching, which is useful for smoke tests. Keep [`scripts/demo`](scripts/demo) representative as new dogfoodable surfaces land.

Interactive UI preferences are safe, non-executable global configuration. Fut reads `$XDG_CONFIG_HOME/fut/config.toml` when `XDG_CONFIG_HOME` is an absolute path and otherwise `~/.config/fut/config.toml`; `FUT_CONFIG` may select an explicit absolute file. A missing implicit file uses defaults and is never created automatically. Interactive startup rejects unknown fields, malformed TOML, non-regular files, and files larger than 64 KiB before bare Fut creates resources or changes the host terminal state. Explicit control commands and completion do not load UI configuration. Preferences are applied per client at attach time; live reload is deferred.

```toml
[ui]
tab_bar_position = "top"            # "top" or "bottom"
workspace_sidebar_position = "left" # "left" or "right"
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

fut pane new TAB_ID [--cwd PATH] [-- COMMAND...]
fut pane attach PANE_ID
fut pane move PANE_ID DESTINATION_TAB_ID
fut pane close PANE_ID
fut terminal attach TERMINAL_ID

fut list
fut daemon run [--cwd PATH] [-- COMMAND...]
fut daemon ping
fut daemon shutdown
```

Mutation commands accept raw IDs only. Attach commands also use raw IDs, except `session attach`, which additionally permits an exact session name as a convenience. A UUID-shaped session value always has ID precedence, even if it could be a session name. Attaching by session, workspace, or tab succeeds only when that ancestor identifies exactly one open terminal; multi-pane ancestors are ambiguous, so use an exact pane or terminal ID or choose through the navigator. Pane and terminal IDs identify their terminal exactly. There is no public `resource:<id>` or selector mini-language; the internal client/daemon protocol remains typed.

`open`, `tab new`, and `pane new` are control-only and do not attach. Their results include the complete selected ancestry (`session_id`, `workspace_id`, `tab_id`, `pane_id`, and `terminal_id`, plus the child PID); attachment remains a separate operation. `pane new` takes a raw tab ID and passes argv following a literal `--` directly, without shell evaluation. With no command it starts the default shell. Like `tab new`, its working directory defaults to the workspace root, and a relative `--cwd` resolves against that root. `pane move` is also control-only. It moves a pane to another live tab in the same workspace, appends it after existing destination panes, and preserves its pane ID, terminal ID, process, terminal state, and attachment lease. Moving the final pane out of a tab removes that empty tab. Repeating the completed move is a successful no-op. Cross-workspace movement and explicit insertion positions remain deferred. Names are unique in their scope. Worktrees are discovered only when explicitly opened.

## Automation and interaction

The client/daemon protocol is version 10. If a rebuilt client finds a running daemon with a different protocol, it reports the mismatch instead of starting a competing daemon; run `fut daemon shutdown` to negotiate with and stop that daemon, then retry. Global `--json` is available only for noninteractive control commands. Successful output has a versioned envelope and dotted command name:

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

The generated integration completes the static command, option, and path grammar. At resource operands it reads the current daemon snapshot and inserts only the full raw ID. This includes eligible tab IDs for `pane new`; movable pane IDs for `pane move`; and, after a source pane is entered, only the other live tabs in that pane's workspace. Zsh and fish also display Unicode names, full ancestry, workspace roots, and distinct numbered pane or terminal descriptions; Bash's completion protocol does not preserve those descriptions. `session attach` completion inserts IDs even though exact names remain valid when entered manually. Re-source the generated integration after upgrading Fut so it stays in step with the executable.

Completion never starts a daemon or mutates resources. It honors `--socket` and the normal socket environment precedence, omits closing and guaranteed-invalid targets, and uses a short bounded query. If the daemon is absent, stale, incompatible, or slow, completion fails silently while static suggestions remain available.

Inside the client, `Ctrl-b k` opens the command bar, `Ctrl-b c` creates and switches to a default shell tab, `Ctrl-b g` opens the global navigator, `Ctrl-b w` activates workspace navigation, `Ctrl-b d` detaches, and `Ctrl-b Ctrl-b` sends a literal `Ctrl-b`. In a multi-pane tab, `Ctrl-b l` or `Ctrl-b o` focuses the next pane, while `Ctrl-b h` or `Ctrl-b ;` focuses the previous pane. Navigation wraps in resource order; these are previous/next operations rather than geometric left/right navigation. Focus changes are acknowledged by the daemon before later input is read.

The one-row horizontal tab bar defaults to the top and can move to the bottom through global configuration. It follows live tab creation, rename, closure, and selection in resource order. The active tab uses a bold `●`; closing tabs use `×`; nearby tabs and edge ellipses preserve context when width is constrained. The bar gives up its row entirely in a one-row host terminal. Its space is excluded from pane geometry and focused-terminal PTY resize, while the full-screen navigator still replaces the complete client area.

At 120 columns and wider, a 24-column peer-workspace sidebar docks beside the terminal, leaving at least 96 columns for terminal content. It follows the current session's workspaces in resource order, marks the current checkout with `●`, dims closing workspaces with `×`, remembers the last focused terminal in each workspace, and can be placed on the left or right. At 119 columns and below it consumes no permanent space. `Ctrl-b w` activates the docked list or opens the same list as an edge drawer without resizing the PTY; use `↑`/`↓` or `j`/`k`, `Home`/`End`, `Enter`, and `Esc` or `q`. Switching resolves to an exact stable terminal and waits for daemon acknowledgement before accepting more input. The sidebar preserves the selected/current workspace through live rename, close, movement, and overflow updates, and the global navigator remains the cross-session surface.

`Ctrl-b k` opens a borderless command bar over the terminal area without changing pane geometry or PTY size. It indexes the existing typed client actions—global and workspace navigation, tab creation, pane focus, and detach—and shows their actual direct bindings from the same binding catalog used for dispatch. Search is case-insensitive and token-based across names, aliases, and bindings. Use normal text or paste to filter, `↑`/`↓`, `Ctrl-p`/`Ctrl-n`, `Tab`/`BackTab`, `Home`/`End`, or page keys to choose, `Enter` to run, `Ctrl-u` to clear, and `Esc` or `Ctrl-c` to close. Empty results remain editable, Unicode backspace is grapheme-safe, pasted control runs cannot dispatch actions, and tiny clients retain a clipped prompt and selected action. Direct bindings and command choices invoke the same client-local typed dispatcher; neither path constructs CLI commands.

Multi-pane tabs use a responsive focus-biased horizontal accordion in resource order. When all panes fit, the focused pane receives at least 24 content columns, every sibling receives at least 12, and remaining width is distributed with twice the weight on focus. A one-cell neutral rail marks every visible pane; the focused rail is bold, background rails are dim, terminal colors are left untouched, and only the focused cursor is shown. If those minimums do not fit, the client falls back to the focused pane at full size and restores the accordion when space returns. The exclusive input and resize lease remains on the focused terminal only, so separate clients may focus different sibling terminals; background panes are observed read-only and clipped to their current grids until focused.

Multi-pane lifecycle is supported: pane and terminal navigation is exact; pane placement can move between tabs in one workspace without disturbing its terminal; exiting or closing one pane preserves its siblings and ancestors; when the focused pane exits the client transfers focus to the next available sibling; the final pane cascades through empty ancestors; and closing a whole tab closes all descendant panes and terminals. Interactive clients receive revisioned resource snapshots and authoritative complete tab views. External pane creation, movement, closure, and process exit therefore update every affected client without reselecting: background membership changes preserve focus, while a moved focused pane follows its stable terminal into the destination tab. Two-phase close remains visible in the model: a background pane disappears when its close is requested, while a focused pane remains selected until its final snapshot and exit are delivered, then transfers to an available sibling; if none remains, the client exits and preserves a nonzero child status. The navigator, tab bar, and workspace sidebar refresh from the same resource stream. The responsive accordion, navigation chrome, and command bar are client-owned presentation, not yet a persisted split system. Authored direction and ratios, explicit zoom, pane titles, mouse focus, pane naming, cross-workspace movement, richer input, project recipes, and agent activity remain planned.

See [VISION.md](VISION.md) for the product direction, [CONTEXT.md](CONTEXT.md) for shared language, and [PLAN.md](PLAN.md) for implementation status and exit criteria.
