# fut

`fut` is an agent-oriented terminal multiplexer organized as sessions, workspaces, tabs, panes, and terminals. One daemon owns the live resource tree; clients and automation address resources by their raw IDs.

The current Rust vertical slice supports multiple project sessions, user-defined workspaces, explicitly opened Git worktrees as natural peer workspaces, multiple tabs and panes, detach/reattach, rename, move, and close operations, semantic `libghostty-vt` terminal snapshots, responsive focus-biased multi-pane rendering with client-local focus and zoom, configurable token-driven tab and workspace chrome, a searchable typed command bar, read-only diagnostics, and reconciliation after external resource changes.

## Run it

```sh
mise install
mise run check

cargo run                          # open the current directory and attach
cargo run -- daemon run            # explicitly run the daemon
cargo run -- open ../api           # control-only; daemon must already exist
cargo run -- open ../api --name api -- /bin/zsh
cargo run -- list
cargo run -- doctor
```

For a disposable dogfooding environment with three linked-worktree workspaces, seven tabs, and nine panes, run:

```sh
mise run demo
```

The demo builds the current binary, creates its fixture under `target/fut-demo`, starts an isolated daemon and socket, and attaches to the main shell. It includes interactive shells, multi-pane tabs, and slowly updating server/test/preview tabs for exercising the tab bar, component sidebar, accordion, global navigator, and command bar. Detach with `Ctrl-b d`, then use `mise run demo:attach` to return or `mise run demo:clean` to stop and remove it. `mise run demo:setup` creates the same fixture without attaching, which is useful for smoke tests. Keep [`scripts/demo`](scripts/demo) representative as new dogfoodable surfaces land.

`mise run journey` runs one deterministic keyboard-driven workflow in a temporary home, runtime, workspace, socket, daemon, and PTY. `mise run journey:chaos` runs a seeded state-machine variant that checks input routing and resource/layout invariants after every action. Reproduce or extend a run with `FUT_CHAOS_SEED=123` and `FUT_CHAOS_STEPS=1000`; step counts are bounded at 2,000.

Interactive UI preferences are safe, non-executable global configuration. Fut reads `$XDG_CONFIG_HOME/fut/config.toml` when `XDG_CONFIG_HOME` is absolute and otherwise `~/.config/fut/config.toml`; `FUT_CONFIG` may select an explicit absolute file, while `--config-dir` selects a directory containing an optional `config.toml`. Configuration controls tab-bar and workspace-row tokens, left/center/right groups, semantic styles, indexed or RGB colors, icon presets and overrides, independent sidebar slots, and pane layout. The Unicode preset is the default; Nerd Fonts are an explicit opt-in because Fut cannot reliably detect the active terminal font.

```toml
[ui]
pane_layout = "splits"

[ui.icons]
preset = "nerd_font"

[ui.tab_bar]
position = "top"
left = [{ segments = [{ token = "workspace.name" }], style = "muted", priority = 200 }]
center = [{ segments = [{ component = "tabs" }] }]
right = [{ segments = [{ token = "client.zoom" }], style = "current", priority = 255 }]

[ui.sidebar.left]
width = 24
components = [
  { component = "workspaces", size = "fill" },
]

[ui.sidebar.right]
width = 24
components = [
  { component = "agents", size = "fill", scope = "session" },
]
```

Unknown fields, unsafe text, malformed or oversized files, ambiguous segments, and out-of-scope tokens are rejected before interactive startup changes terminal state. `Ctrl-b Shift-R` reloads the invoking client's preferences atomically; errors leave its previous configuration active. Control commands and completion do not load preferences. Run `fut doctor` for a read-only configuration, terminal, runtime, protocol, and icon probe. See the [documentation index](docs/index.md), [complete configuration reference](docs/configuration.md), [token catalog](docs/tokens.md), and [`fut doctor` reference](docs/doctor.md).

`fut open [PATH] [--name NAME] [-- COMMAND...]` replaces the former `new` command. It is always control-only and requires an existing daemon. Bare `fut` is the convenience path that opens the current directory and then attaches to the returned terminal. `fut attach` instead connects only to an existing daemon, opens the global navigator without a terminal lease, and attaches after selection. Attaching to an exact existing resource remains a separate `RESOURCE attach ID` operation; the CLI does not offer an atomic create-and-attach command. Child commands always follow `--`; they are passed as arguments without shell evaluation.

## Control surface

Resource operations are noun-first. Top-level `attach`, `open`, `list`, and the read-only `doctor`, plus the `daemon` lifecycle commands, are intentional non-resource entry points:

```sh
fut session attach SESSION
fut session rename [SESSION_ID] NAME
fut session close [SESSION_ID]

fut workspace attach WORKSPACE_ID
fut workspace rename [WORKSPACE_ID] NAME
fut workspace close [WORKSPACE_ID]

fut tab new [WORKSPACE_ID] [--name NAME] [--cwd PATH] [-- COMMAND...]
fut tab list [WORKSPACE_ID]
fut tab attach TAB_ID
fut tab rename [TAB_ID] NAME
fut tab close [TAB_ID]

fut pane new [TAB_ID] [--cwd PATH] [-- COMMAND...]
fut pane split [PANE_ID] DIRECTION [--cwd PATH] [-- COMMAND...]
fut pane list [TAB_ID]
fut pane attach PANE_ID
fut pane move [PANE_ID] DESTINATION_TAB_ID
fut pane close [PANE_ID]
fut terminal attach TERMINAL_ID

fut attach
fut list
fut daemon run [--cwd PATH] [-- COMMAND...]
fut daemon ping
fut daemon shutdown
```

Explicit operations accept raw IDs. Inside Fut, the bracketed layout IDs may be omitted: Fut resolves the current live ancestry from stable `FUT_TERMINAL_ID`, ignoring potentially stale ancestor environment variables. Mutating inferred operations carry a daemon-side ancestry guard, so a concurrent pane move, close, or exit fails instead of acting on an old ancestor. Attach commands use raw IDs, except `session attach`, which additionally permits an exact session name as a convenience. A UUID-shaped session value always has ID precedence, even if it could be a session name. Attaching by session, workspace, or tab succeeds only when that ancestor identifies exactly one open terminal; multi-pane ancestors are ambiguous, so use an exact pane or terminal ID or choose through the navigator. Pane and terminal IDs identify their terminal exactly. There is no public `resource:<id>` or selector mini-language; the internal client/daemon protocol remains typed.

`open`, `tab new`, and `pane new` are control-only and do not attach. Their results include the complete selected ancestry (`session_id`, `workspace_id`, `tab_id`, `pane_id`, and `terminal_id`, plus the child PID); attachment remains a separate operation. `pane new` takes a raw tab ID and passes argv following a literal `--` directly, without shell evaluation. With no command it starts the default shell. Like `tab new`, its working directory defaults to the workspace root, and a relative `--cwd` resolves against that root. `pane move` is also control-only. It moves a pane to another live tab in the same workspace, appends it after existing destination panes, and preserves its pane ID, terminal ID, process, terminal state, and attachment lease. Moving the final pane out of a tab removes that empty tab. Repeating the completed move is a successful no-op. Cross-workspace movement and explicit insertion positions remain deferred. Names are unique in their scope.

`tab list WORKSPACE_ID` and `pane list TAB_ID` are read-only. They report the same daemon snapshot `fut list` uses, narrowed to one workspace or tab, and add the authored split layout: `tab list` prints one line per tab with its `layout=` tree and its panes, and `pane list` prints that tab's layout and panes. Under `--json` the results carry the snapshot `revision`, the requested ID, and the same `layout`, `tabs`, or `panes` values found in `fut list --json`. An unknown ID fails with the `not_found` code.

A workspace is a logical user context and collection of tabs. Its root supplies working-directory defaults, but Fut does not require it to represent a checkout or manage Git on the user's behalf. Interactive workspace creation inherits the focused terminal's current directory, starts a shell, and permits another workspace in that session to use the same root. Worktrees remain a natural fit and are discovered only when explicitly opened; users and scripts decide how to create or arrange them.

## Automation and interaction

The current development protocol is `17` and requires an exact match between clients and daemons. After upgrading from Fut 0.1, `fut daemon shutdown` can still stop its protocol-`0` daemon; otherwise stop the running daemon before retrying. Global `--json` is available only for noninteractive control commands. Successful output has a versioned envelope and dotted command name:

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

Inside the client, `Ctrl-b :` opens the command palette, `Ctrl-b Shift-R` reloads that client's configuration, `Ctrl-b [` enters copy mode, `Ctrl-b c` creates and switches to a default shell tab, `Ctrl-b t` activates the tab bar, `Ctrl-b n`/`Ctrl-b p` wrap through tabs in the current workspace, `Ctrl-b 1` through `Ctrl-b 9` select those one-based tab-bar slots, and `Ctrl-b 0` selects tab 10. `Ctrl-b |` splits the focused pane right, `Ctrl-b _` splits it downward, and `Ctrl-b s` opens the global navigator, where plain text fuzzy-filters every hierarchical resource row against its full ancestor path. `Ctrl-b w` and `Ctrl-b ]` activate the left and right component sidebars, `Ctrl-b u` lists terminals with unseen blocked or completed reports, `Ctrl-b Ctrl-b` selects the next unread waiting terminal, and `Ctrl-b d` detaches. In a multi-pane tab, `Ctrl-b h/j/k/l` focuses left/down/up/right without wrapping, while `Ctrl-b o` and `Ctrl-b ;` cycle next and previous. `Ctrl-b P/T/W` toggles the last pane, tab, or workspace, `Ctrl-b Ctrl-s` toggles the last session, and `Ctrl-b z` toggles pane zoom. Every action suffix can be overridden under `[ui.bindings]`, including `prefix` for a second `Ctrl-b`; rebinding next-unread away from it restores literal-prefix input. Focus and creation changes are acknowledged by the daemon before later input is read.

Copy mode is client-local. Move with arrows or `hjkl`, Home/End, and Page Up/Page Down; press Space to start or clear a selection. `/` searches literal scrollback text and `n`/`N` repeat forward or backward. `y` or Enter copies through bounded local `pbcopy`, while Escape or `q` cancels. Dragging in a focused pane enters the same selection path when its application is not using mouse reporting, then copies and exits on release; a click without a drag does not copy, and Shift-drag forces Fut selection when the host terminal forwards the modified gesture. Clipboard failures leave the selection active for retry, and copy/search work returns explicit errors rather than truncating oversized history.

The one-row horizontal tab bar defaults to the top and can move to the bottom through global configuration. It follows live tab creation, rename, closure, and selection in resource order. Tabs default to numbers without names in left-aligned 12-cell items with one cell of leading padding. The active tab uses a muted background without an underline while the pane is focused; keyboard selection adds an underline without changing the background or item width. Closing tabs use `×`; nearby tabs and edge ellipses preserve context when width is constrained. The bar gives up its row entirely in a one-row host terminal. Its space is excluded from pane geometry and focused-terminal PTY resize. When the workspace list is docked, the list is the outer resource surface and the tab bar spans only the workspace content inside it; the full-screen navigator still replaces the complete client area.

Independent left and right sidebars each stack configured Workspaces and Agents components. Workspaces follows the focused session in resource order, marks the active workspace with a leading bullet and bold row, gives keyboard selection a muted background, shows each stable workspace root on a muted second line, dims closing workspaces with `×`, and remembers the last focused terminal in each workspace. Agents lists explicitly integrated terminals at `tab`, `workspace`, `session`, or `global` scope with activity and ancestry context; it is navigation-only, keeps selection by terminal identity across resource refreshes, and global rows navigate safely across sessions. `Tab` and `BackTab` move component focus. The defaults put Workspaces on the automatic left and session-scoped Agents on the automatic right, which consumes no width when empty. Both may dock while preserving 40 terminal columns; narrow or irrelevant sides retain independent edge drawers and widths.

`Ctrl-b t` activates the tab bar without changing terminal geometry. Use arrow keys or `h/j/k/l`, `Home`/`End`, and `Enter` to choose a tab; `c` creates and switches to a shell tab with normal CWD inheritance, `r` renames the selected tab, and `Esc` or `q` returns to the terminal. Workspace and tab rename dialogs retain the current name, accept `Ctrl-u` to clear it, and keep daemon validation errors editable for retry.

`Ctrl-b :` opens a borderless command palette over the terminal area without changing pane geometry or PTY size. It covers every dispatchable client action except opening the already-open launcher: global and workspace navigation, tab creation, cycling, numbering and history, right/down splits, directional and cyclic pane focus, zoom, and detach. Each action shows its configured direct binding from the same catalog used for dispatch, and reverse coverage tests reject actions or bindings omitted from the launcher. Search uses the same case-insensitive, multi-token fuzzy ranking as the navigator across names, aliases, and bindings. Use normal text or paste to filter, `↑`/`↓`, `Ctrl-p`/`Ctrl-n`, `Tab`/`BackTab`, `Home`/`End`, or page keys to choose, `Enter` to run, `Ctrl-u` to clear, and `Esc` or `Ctrl-c` to close. Empty results remain editable, Unicode backspace is grapheme-safe, pasted control runs cannot dispatch actions, and tiny clients retain a clipped prompt and selected action. Direct bindings and command choices invoke the same client-local typed dispatcher; neither path constructs CLI commands.

Multi-pane tabs default to a shared authored split tree matching tmux's split model. Right and down splits begin at an even ratio, inherit the focused terminal's current directory, survive detachment, collapse cleanly on closure, and preserve their topology across clients with different viewport sizes. Interactive new tabs use the same fresh process-CWD lookup, recorded spawn-directory fallback, and workspace-root fallback. Each client computes rectangles locally with 24 focused columns, 12 sibling columns, three rows per leaf, and one-cell dividers; if the complete tree does not fit, only focus fills the host. Directional navigation uses the underlying unzoomed logical geometry, so hidden panes remain reachable while zoomed or below the rendering minimum. The focus-biased horizontal accordion remains available as the alternate `pane_layout = "accordion"` policy without rewriting shared topology. Explicit client-local zoom overlays either policy, resizes the focused PTY, keeps pane navigation available, and shows a reversed `zoom` status in the tab bar. The exclusive input and resize lease remains on the focused terminal only.

Mouse input is hit-tested against terminal content. A left click on an unfocused pane selects it through typed focus without forwarding that initiating gesture. In the focused pane, application mouse reporting takes precedence; otherwise a left drag selects and copies terminal text, with Shift as an explicit selection override. Requested motion, modifiers, and wheel input use Ghostty's application mouse modes and pane-local cells. Wheel routing prefers application reports, then DEC alternate scroll as three mode-aware cursor keys on the alternate screen, then three lines of client-local scrollback. Other clients' viewports are never changed, and copy mode or modal surfaces suppress application mouse input. Keyboard input, paste, and host resize return the focused pane to the live bottom.

Multi-pane lifecycle is supported: pane and terminal navigation is exact; pane placement can move between tabs in one workspace without disturbing its terminal; exiting or closing one pane preserves its siblings and ancestors; and closing a whole tab closes all descendant panes and removes its tab-bar entry. When the focused terminal exits, Fut walks backward through pane, tab, and workspace resource order inside that session, then forward if there was no predecessor. It never crosses into another session: exhausting the attached session detaches that client and closes the empty session, while other sessions and the daemon remain alive. Interactive clients receive revisioned resource snapshots and authoritative complete tab views, including open split topology. External pane creation, movement, closure, and process exit therefore update every affected client without reselecting. Authored split topology is shared resource state; computed rectangles, accordion policy, zoom, focus, and viewport remain client-owned. Pane titles, pane naming, cross-workspace movement, richer keyboard input, and project recipes remain planned.

Fut preserves indexed ANSI colors as palette indices instead of resolving them through an internal default palette. The containing terminal therefore applies its own theme to ANSI colors, while explicit 24-bit colors remain exact RGB values.

See [VISION.md](VISION.md) for the product direction, [CONTEXT.md](CONTEXT.md) for shared language, and [PLAN.md](PLAN.md) for implementation status and exit criteria.
