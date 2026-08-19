---
layout: default
title: Using Fut
description: Start Fut, navigate its resource tree, and use its terminal features.
permalink: /usage/
---

# Using Fut

> **TL;DR:** Run `fut` in a project. Most interactive actions start with
> `Ctrl-b`; pause after the prefix to see every binding, or press `Ctrl-b :`
> to search the command palette. Detach with `Ctrl-b d` and return with `fut`.

## Start and return

```sh
cd ~/code/my-project
fut
```

Bare `fut` starts the daemon when needed, opens the current directory, and
attaches a client. The daemon keeps sessions and terminal processes alive after
you detach. Runtime state is not restored after the daemon exits or the machine
restarts.

Run `fut` from another directory to open it in the existing daemon. Fut groups
resources like this:

```text
session      a project-level container
└─ workspace a user-defined context, often a checkout or worktree
   └─ tab     a named or process-titled terminal layout
      └─ pane a placement of one running terminal
```

Use `fut attach` to open the global navigator before attaching, or target an
unambiguous resource directly with `fut session attach`, `fut workspace
attach`, `fut tab attach`, `fut pane attach`, or `fut terminal attach`.

Fut refuses to start a nested interactive client inside one of its terminals.
If nesting is intentional, run `FUT_ALLOW_NESTED=1 fut`.

For Git repositories, Fut groups linked worktrees from the same repository as
peer workspaces in one session. Ordinary directories get their own session.
Opening the same location again reuses it; bare Git repositories are not valid
workspaces.

## Everyday controls

All bindings below follow the `Ctrl-b` prefix. The configuration name is the
key to override under `[ui.bindings]`.

| Key | Action | Configuration name |
| --- | --- | --- |
| `:` | Search every command and configured extension action | `open_command_bar` |
| `R` | Reload configuration | `reload_config` |
| `[` | Enter copy mode | `enter_copy_mode` |
| `s` | Search sessions, workspaces, tabs, and panes | `open_navigator` |
| `a` | Search agents and see their live status | `open_agents` |
| `w` | Open the left sidebar | `open_left_sidebar` |
| `]` | Open the right sidebar | `open_right_sidebar` |
| `t` | Focus the tab bar | `open_tab_bar` |
| `u` | List unread agent notifications | `open_notifications` |
| `Ctrl-b` | Jump to the next unread notification | `focus_next_notification` |
| `C` | Create a workspace | `create_workspace` |
| `c` | Create a tab | `create_tab` |
| `n` | Switch to the next tab | `focus_next_tab` |
| `p` | Switch to the previous tab | `focus_previous_tab` |
| `Down` | Switch to the next workspace | `focus_next_workspace` |
| `Up` | Switch to the previous workspace | `focus_previous_workspace` |
| `|` | Split the pane right | `split_pane_right` |
| `_` | Split the pane down | `split_pane_down` |
| `o` | Focus the next pane | `focus_next_pane` |
| `;` | Focus the previous pane | `focus_previous_pane` |
| `h` / `j` / `k` / `l` | Focus the pane left / down / up / right | `focus_pane_left` / `focus_pane_down` / `focus_pane_up` / `focus_pane_right` |
| `P` | Switch to the last active pane | `focus_last_pane` |
| `Ctrl-t` | Switch to the last active tab | `focus_last_tab` |
| `Ctrl-w` | Switch to the last active workspace | `focus_last_workspace` |
| `Ctrl-s` | Switch to the last active session | `focus_last_session` |
| `1`–`9`, `0` | Select tab 1–10 | `focus_tab_1` through `focus_tab_10` |
| `z` | Toggle pane zoom | `toggle_pane_zoom` |
| `x` | Close the focused pane | `close_pane` |
| `d` | Detach | `detach` |

Pause for 700 ms after `Ctrl-b` to see the complete, current binding list.
Bindings can be changed in [Configuration](../configuration/).

The tab bar and workspace rows also support the mouse: left-click to switch,
right-click for create, rename, close, and sidebar display actions. Drag pane
or sidebar dividers to resize them. Fut preserves application mouse reporting;
when an application does not claim the mouse, the wheel scrolls client-local
history and dragging selects text. Hold Shift to force Fut selection.

## Copy and scrollback

Press `Ctrl-b [` for copy mode. Move with arrows, `hjkl`, Home/End, or page
keys. Space starts or clears a selection; `y` or Enter copies it; Escape or `q`
cancels. `/` searches literal text and `n`/`N` repeats the search.

Clipboard writes use a local `pbcopy` executable. It is available by default
on macOS. On Linux, put a `pbcopy` wrapper around a clipboard tool such as
`wl-copy` or `xclip` on `PATH`. A failed copy keeps the selection active for
another attempt.

## Workspaces, tabs, and panes

Workspaces are organizational contexts, not Git objects. Fut uses their root as
a working-directory fallback but does not create or manage worktrees. Unnamed
workspaces show their live directory or Git work tree; unnamed tabs follow the
focused pane's foreground process. Rename either to keep a fixed label, or
submit an empty name to restore the automatic label.

Pane splits and divider sizes are shared daemon state. Client focus, zoom,
scrollback, dialogs, and configuration are local. Multiple clients may attach
to the same session; they share terminal input and output, and shared PTYs use
the smallest attached client's dimensions.

Fut renders standard terminal mouse modes, indexed and RGB color, OSC 8
hyperlinks, cursor shapes, bracketed paste, alternate screens, and Kitty
graphics used by terminals such as Ghostty, Kitty, and WezTerm.

## CLI and automation

The CLI is noun-first. Ask the installed version for exact arguments:

```sh
fut --help
fut pane --help
fut pane split --help
```

Common control commands include:

```sh
fut open ../api --name api -- zsh
fut open -b ../api
fut tab new --name tests -- mise run test
fut pane split right --cwd ../api -- zsh
fut pane move PANE_ID DESTINATION_TAB_ID
fut list
fut events
fut extension list
fut extension show EXTENSION_ID
fut extension validate PATH
fut extension install PATH
fut extension install-git URL --rev COMMIT [--sha256 DIGEST]
fut extension update EXTENSION_ID --rev COMMIT [--sha256 DIGEST]
fut extension enable EXTENSION_ID
fut extension disable EXTENSION_ID
fut extension remove EXTENSION_ID
fut extension reload
```

Commands after `--` are passed directly, without shell evaluation. Creation
and mutation commands do not change another client's visual focus. Inside Fut,
many resource IDs may be omitted and are resolved from the caller's live
terminal ancestry. Automation should instead use `--json`, retain returned raw
UUIDs, and pass explicit IDs to later commands.

For terminal I/O, lifecycle-aware agent control, and event streaming, see
[Agent activity](../agents/). Fut also bundles machine-readable operating
instructions through `fut agent skill`.

Enable dynamic shell completion in your startup file:

```sh
# zsh
source <(COMPLETE=zsh fut)

# bash
source <(COMPLETE=bash fut)

# fish
COMPLETE=fish fut | source
```

Re-source completion after upgrading. Resource completion is bounded and
read-only; it never starts a daemon.
Active extension-ID completion uses the same bounded, read-only daemon catalog
lookup. Package validation itself is daemonless and never executes extension
code.

## Next steps

- [Agent activity](../agents/) — integrations, notifications, and automation
- [Configuration](../configuration/) — bindings, layout, sidebars, and styles
- [Extensions](../extensions/) — trusted local commands, hooks, and tokens
- [Diagnostics](../doctor/) — check configuration and terminal compatibility
