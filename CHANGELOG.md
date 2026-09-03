# Changelog

## Unreleased

- Added automatic Wayland and X11 clipboard support on Linux, with clear failures in headless sessions.
- Smoothed extension token pulses across complete pills, including their affixes and caps.
- Navigator parent results now reopen the most recently used available workspace, tab, and pane in their hierarchy.
- Added a searchable `fut --ui-playground`, configurable text spinners, restrained pulse and wave effects, and previews combining effects with inverted tokens and pills.
- Fixed Linux arm64 release binaries crashing with illegal instructions on CPUs without SVE.
- Clear the terminal canvas before rendering Fut's first interactive frame.

## 0.15

- Prompt to inspect, approve, or reject untrusted project recipes before daemon startup instead of hiding the error in the daemon log.
- Added a safe forced-shutdown escape hatch for protocol-incompatible daemons, with automatic delegation to the running daemon's own binary when available.
- Made extension status tokens stateful and clickable for pane navigation or resource-scoped commands, including dedicated Nerd Font glyphs and bundled run links to owner panes or logs.
- Fixed snippet expanders and accessibility-generated typing producing repeated physical-key characters inside Fut.
- Restored configured pill caps around the current workspace title while preserving sidebar status colors and workspace-number alignment.

## 0.14

- Added stable 23-character resource selectors to human-facing output, shell completion, and Fut environments while retaining canonical UUID support.
- Added mode-aware extended keyboard input for modified and unambiguous keys, application cursor mode, and press, repeat, and release events on capable hosts.
- Reduced daemon CPU use for animated terminals while detached or in unobserved workspaces.
- Isolated close confirmations from covered paste and pointer input, and made failed multi-terminal closes report irreversible partial progress honestly.
- Refined agent and terminal attention: projections exclude detection-only agents, covered alerts remain unread, notification clearing handles agents and bells, and next-attention includes the current terminal.
- Made full configuration reloads retain their invocation target and contain sibling-client failures, while project opening now prepares asynchronously with visible cancellation.
- Reconciled concurrent divider drags to shared state and stabilized unnamed tab titles across clients.
- Enabled command-palette rename and close actions for the attached session.

## 0.13

- `fut agent prompt --stdin` now accepts multiline and generated prompts without shell argument interpolation.
- `Ctrl-b Shift-S` now opens projects from a fuzzy configured catalog or a typed path, reusing live sessions and reviewing untrusted recipes in-client.
- Added per-client terminal bell alerts with ancestry markers, navigation, a configurable notification glyph, and opt-in outer-terminal signaling.
- Codex can now report completed turns directly through `fut agent notify codex`, without a separately installed notification adapter.
- Sidebar focus now highlights workspace indices with their titles and only agent titles, preserving status colors for faster scanning.
- Extensions can now detect other active extensions and their versions through `FUT_EXTENSIONS`.
- `fut list` now shows the navigator's concise resource tree, with `-v`/`--verbose` retaining the detailed machine-oriented view.
- The Pi package now teaches Pi to inspect and control Fut resources, including other agent panes.
- Configuration reloads now refresh the focused project's extension settings, with a separate command-palette action for project-only reloads.
- Reject malformed explicit socket and runtime paths with actionable errors, ignore invalid XDG/TMPDIR fallbacks, and preserve non-UTF-8 Unix environment values.
- Give terminal processes a SIGHUP grace period before forced shutdown, and terminate complete extension-hook process groups after timeouts.

## 0.12

- Avoided rejecting project opens when Git starts slowly on a busy or freshly provisioned machine.

## 0.10

- Interactive command popups now support mouse scrolling, application mouse input, and drag-to-copy text selection, with Shift-drag forcing selection.
- Added `--no-config` to start Fut with built-in defaults without loading configuration files.
- Added command-palette actions for renaming and closing the focused session, workspace, or tab, with configurable but unbound shortcuts.
- Added `fut project init` and expanded project recipes to bootstrap declared workspaces and titled tabs, with IDs needed only for focus and split references.
- Project run commands configured with `auto_start` now start once in the initial workspace instead of repeating in every new workspace.
- Project opens now prompt to trust an unapproved repository recipe before launching it.
- The bundled `wt` workflow now retires its Fut workspace after an agent successfully removes the worktree with `wt done` or `wt ship`.

## 0.9

- Added native extension command forms, and let the bundled `wt` workflow create random worktree names and launch a project-configured Pi agent with an optional prompt.
- Project recipe commands now return to a shell when they exit by default; set `exec = true` for the previous pane-closing behavior, and omit the former `version` field.
- Made the `Ctrl-b` command prefix configurable with `ui.prefix`.
- Added shortcuts for switching to the last active tab and workspace.

## 0.8

- `fut open` now starts or reuses a location and attaches by default; use `-b`/`--background` for control-only opens.
- Added `fut project list` (`project ls`), the `fut o` alias for opening locations, and `-p` as shorthand for `--project`.
- Failed popup commands now retain their terminal output and show its log path.
- Added a language-neutral extension authoring contract, public-boundary conformance test, and source-only compiled Rust example.
- Added safe extension installation and updates from exact Git commits, with optional digest verification, source provenance, explicit enablement, and daemonless management and validation commands.
- Added atomic live extension reloads across attached clients, versioned manifest compatibility, exact capability declarations, catalog inspection, diagnostics, and extension ID completion.
- Added a single run-status token for showing managed command state in workspace lists.
- Made the global navigator taller so more resources are visible at once.
- Added a project catalog with daemonless recipe approval and trusted layouts, commands, environments, working directories, focus, automatic worktree opening, and managed run commands.
- Fixed shared-terminal attachment sizing, failed last-terminal replacement recovery, and transient Codex screen-capture errors.

- Added a searchable global agents dialog on `Ctrl-b a`, with color-coded sidebar statuses and startup Codex detection.
- Added a managed run extension with restart/stop controls, output readiness signals, animated workspace status, live logs, and project-local configuration.
- Added client lifecycle hooks and a Ghostty extension that follows the active Fut session in the window title.
- Added direct key bindings and local argument overrides for extension commands using their qualified palette slugs.
- Added theme-adaptive inverted and Nerd Font pill styling for token segments and the focused workspace title.
- The navigator now identifies and dims sessions already open in another Fut client.
- Fixed the bundled `wt` command not switching to newly created worktrees.
- Fixed linked filenames bleeding through or breaking dialog borders.

## 0.7

- Added independent, composable sidebars with scoped Workspaces and Agents lists, automatic relevance, compact rails, drawers, resizing, and section dividers.
- Added mouse activation and contextual menus for tabs and workspaces.
- Added packaged extension commands, palette-only trusted commands, qualified extension search, and configurable temporary-command sizes.
- Moved the command palette to `Ctrl-b :`, added searchable tmux-style action names, `fut a` and `fut ls` aliases, and `Ctrl-b x` for closing panes.
- Shared agent notification read state daemon-wide, exposed unread counts through `fut agent list`, and removed exited integrations from agent lists.
- Added `--config-dir` and prevented accidental nested Fut clients unless `FUT_ALLOW_NESTED` is set.
- Centered close confirmations, added `ui.confirm_close`, and fixed clean exit after the final successful terminal closes.
- Added acknowledged workspace retirement and improved the bundled `wt` workflow with automatic activation and configurable post-open actions.
- Fixed relative `fut open` paths being resolved from the daemon instead of the invoking process.

## 0.6

- Refined the global navigator hierarchy with inline single-pane tabs and resource-scoped filtered breadcrumbs.
- The global navigator now defaults to `Ctrl-b s`, and `Ctrl-b Ctrl-s` toggles between the current and last active session.

## 0.5

- Render Kitty-protocol inline images through Fut in Ghostty, Kitty, and WezTerm.
- `Ctrl-b C` now creates a workspace; `Ctrl-b c` continues to create a tab.
- Explicit OSC 8 hyperlinks from terminal applications now remain clickable through Fut.
- Added explicit local extensions with bounded manifest loading, asynchronous workspace lifecycle hooks, and namespaced dynamic presentation tokens published through `fut token publish`.
- Workspaces are now unnamed by default and show their live Git work tree or directory, with consistent branch and diff metadata across attached clients.
- The rename dialog can clear tab and workspace names back to automatic naming by submitting an empty name, and supports Option-Backspace/Ctrl-W word deletion and Cmd-Backspace/Ctrl-U line clearing.
- Workspace lists now show the session name in a bold, padded header, abbreviated in the minimized rail.
- Navigator resource shortcuts now filter sessions, scoped workspaces, tabs, or panes instead of cycling through them, and each resource level has a distinct configurable color.
- Fixed independent daemons with different socket names in the same runtime directory blocking each other.

## 0.4

- Added fallback Codex idle, working, and blocked detection from the live terminal screen when no lifecycle plugin is active, including unread completion attention when detected work finishes.
- Dragging in a focused pane now selects and copies text when its application is not using mouse reporting; Shift-drag forces selection, while mouse-aware applications keep their gestures.
- Removed `ui.tab_bar.item.min_width`; tab content now uses its intrinsic width instead of padding short labels to a configured minimum.
- Replaced transient bottom-row notices with compact corner toasts that avoid the tab bar, dismiss confirmations automatically, and keep errors visible until the next keypress.
- Fuzzy navigator results now collapse redundant descendant matches, and the in-client navigator returns parent selections to their most recently focused pane.
- Workspace-sidebar display and visibility are now independent: it can be expanded or minimized to a compact status rail while separately remaining visible, hiding with one workspace, or staying hidden; expanded rows retain workspace numbers, status glyphs have safe edge padding, and open-sidebar hotkeys use separate footer lines with Nerd Font icons when enabled.
- Added trusted configurable keybinding commands that run in a framed temporary full-terminal PTY, inherit the focused pane's working directory, and restore the prior layout on exit.
- Unnamed tabs now follow the foreground process in their focused pane; explicit tab titles remain fixed, and fuzzy navigator results show their complete ancestry with matched characters emphasized.
- Multiple terminal windows can now attach to the same session; its terminal geometry follows the smallest attached client, restores as clients resize or detach, and larger clients show the shared-size margin with a subtle border and sparse muted star field.
- Fixed multi-second backward paging latency in delta/less when running a development build.

- Development builds now identify themselves with a `-dev` suffix in `fut -v` output.
- Fixed workspace Git branch and diff metadata remaining stale while the UI is idle.
- Combined resource navigation and fuzzy search in the global navigator; `Ctrl-b f` and the `open_jump` binding have been removed.
- Command search now uses fuzzy matching, and unnamed navigator tabs display positional names.

## 0.3

- Added `-v` as the short form of `--version` (`-V` remains supported).
- Pane and workspace-sidebar dividers can now be resized cell by cell with the mouse; pane splits persist and synchronize, while sidebar widths remain client-local.
- Layout commands can now omit their current session, workspace, tab, or pane ID when run inside Fut.
- Added `fut attach` to choose a destination in the global navigator before acquiring a terminal attachment.
- Terminal applications can now select block, bar, or underline cursors, including blinking styles.
- Added atomic in-client configuration reload with `Ctrl-b Shift-R`.
- `Ctrl-b Ctrl-b` now cycles to the next unread waiting terminal by default and remains configurable.
- Added `fut agent skill` to print Fut's bundled agent instructions.
- Added `fut pane split` for creating right or downward splits without attaching.
- Added `fut context` and `fut get` for compact, focus-independent resource discovery.
- Added `fut terminal send-text`, `send-keys`, and `run` for explicit terminal input.
- Added bounded terminal output reads and event-driven literal or regex waits.
- Added lifecycle-aware `fut agent` discovery, prompting, waiting, reading, and reporting commands.
- Added a lifecycle-only Claude Code plugin for reporting agent activity to Fut.
- Updated the Pi integration to report ordered lifecycle state through `fut agent report`.
- Agent activity snapshots now distinguish integrated terminals and retain the latest lifecycle event.
- Improved animated-terminal performance with more compact cells, cheaper snapshots, and batched Ghostty field reads.
- Fixed panes under heavy output getting stuck while closing and blocking attachment.

## 0.2

- Added a jump dialog (`Ctrl-b f`): type to filter all sessions, workspaces, tabs, and panes, Enter switches.
- Fixed heavy terminal output making the whole UI lag or hang (e.g. animated TUIs like amp's orb): the daemon now batches PTY output and publishes at most one screen frame per 8 ms per terminal, instead of rebuilding and shipping the full screen for every kilobyte read.
- Switched the client/daemon wire protocol to MessagePack with a compact cell-style encoding (protocol 5), making heavily styled frames ~4x smaller and ~5x faster to decode.
- Faster screen-snapshot construction in the daemon and cheaper styled-cell drawing in the client.
- Added a render-performance harness: `mise run perf:bench` (microbenchmarks incl. a vtebench-style dense-cells workload), `mise run perf:e2e` (headless flood, styled, dense, and latency scenarios against a disposable daemon), and `FUT_PERF_LOG` client frame logging with `scripts/perf/report`. Findings live in `PERF.md`.
- Tabs are now created unnamed and the tab bar shows each tab's name beside its number.
- Restyled the workspace sidebar: wider rows, a bullet marker for the current workspace, underline-free selection, and a Git branch and diff summary under each workspace.
- Added `Ctrl-b ↑/↓` to cycle workspaces, digit keys in the workspace sidebar to pick one directly, and `h` there to toggle sidebar auto-hide.
- Added the current workspace name to the tab bar's right side.
- Added `fut tab list` and `fut pane list` for inspecting tabs, panes, and split layouts, with `--json` support.
- Added `fut events`: subscribe to state changes as a stream of versioned JSON lines, one full resource snapshot now and after every change.
- Added a pill-shaped focused tab when the `nerd_font` icon preset is active.
- Added agent activity indicators, per-client completion notifications, waiting-terminal navigation, and Pi integration.
- Added per-client mouse-wheel scrollback, click-to-focus, focused-application mouse reporting, alternate-screen wheel translation, and safe held-button cleanup across UI transitions.
- Added mode-aware terminal paste with bracketed-paste handling and Ghostty control-byte sanitization.
- Added per-client copy mode with ordered keyboard selection, scrollback search, and retry-safe macOS clipboard copying.
- Fixed input loss under terminal queue pressure and stale mouse or viewport events disconnecting clients during pane transitions.
- Fixed idle terminals republishing their screen 35 times a second, which kept every attached client redrawing.
- Added exact protocol compatibility checks, with shutdown support for Fut 0.1 daemons.

## 0.1

- Released persistent, configurable terminal multiplexing for macOS.
- Automated GitHub builds, releases, and Homebrew distribution.
