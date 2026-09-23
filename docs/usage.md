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
you detach. Detached terminals keep consuming and parsing their output, but Fut
defers screen rendering until a client observes them again. Runtime state is not
restored after the daemon exits or the machine restarts.

`fut daemon shutdown` gracefully closes every session. If an upgrade leaves an
older, protocol-incompatible daemon running, Fut first asks that daemon's own
executable to shut it down. As a final recovery path,
`fut daemon shutdown --force` verifies the Unix socket belongs to your user and
terminates its owning process without speaking the incompatible protocol.

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
For temporary access to a daemon from another Fut version, `fut attach
--ignore-protocol-mismatch` retries using the daemon's protocol. This is an
unsafe compatibility escape hatch; unsupported protocol changes may still
cause the client to fail.

Fut refuses to start a nested interactive client inside one of its terminals.
If nesting is intentional, run `FUT_ALLOW_NESTED=1 fut`.

## Attach to a remote machine

There are two distinct SSH workflows:

- `ssh -t workbox fut` runs both Fut's client and daemon on `workbox`; the SSH
  session owns terminal integration and clipboard availability.
- `fut --remote workbox` runs the interface, theme, input handling, and
  clipboard locally while the daemon and PTYs remain on `workbox`.

Run Fut's interface locally while its daemon and terminals remain on an SSH
host:

```sh
fut --remote workbox
```

`workbox` is a host or alias accepted by OpenSSH. Fut uses the normal SSH
configuration, authentication, host keys, and jump hosts, then opens the global
navigator for the remote daemon. `fut --remote workbox attach` is equivalent.
The local process renders the interface and uses the local theme, keybindings,
and clipboard; SSH carries the Fut protocol to the remote daemon without
exposing a network listener.

Fut must already be installed on the remote host and its daemon must already be
running. The client and daemon may use different Fut versions when they support
the same remote protocol generation, codec, and required capabilities. Older
releases without the remote handshake need upgrading before remote attachment
can work. For example, connect normally once, start Fut, and
detach before using the local client:

```sh
ssh workbox
fut
# Ctrl-b d, then exit SSH
fut --remote workbox
```

Remote attachment is initially attach-only: it never starts, stops, upgrades,
or replaces the remote daemon. Project opening, configuration reload, configured
commands, extension command actions, and client lifecycle hooks are unavailable
because they currently assume the client and daemon share a filesystem. Remote
terminal links are limited to HTTP and HTTPS. Ordinary terminal input,
navigation, layout actions, copy mode, and pane links continue to work.

The navigator requires metadata support, and attachment additionally requires
interactive terminal support. Bell alerts, extension catalogs, and health checks
are optional: a peer without one of these disables only that feature. A missing
extension catalog also disables its presentation styles and command listings;
local theme and ordinary keybindings still work. An incompatible or malformed
endpoint fails without restarting either daemon or retrying the private local
protocol. The local exact-version check and `--ignore-protocol-mismatch` escape
hatch are unchanged. See the [remote protocol contract](remote-protocol.md).

Remote Fut currently requires macOS or Linux on both ends. Windows clients and
Windows remote daemons are unsupported because the bridge connects to Fut's
private Unix socket. Fut opens outbound OpenSSH processes only: no Fut daemon
listens on TCP and there is no central Fut server.

### Saved machines

Keep a catalog of the SSH machines you attach to:

```sh
fut machine add workbox
fut machine add deploy@10.0.0.7 --label prod
fut machine list
fut machine show prod
fut machine rename prod production
fut machine disable production
fut machine enable production
fut machine remove production
```

`fut machine add` connects through SSH, confirms that a compatible Fut daemon
is already running there, and only then saves the profile. A failed or
cancelled check saves nothing and never changes the terminal. The other
commands edit the catalog locally and never contact SSH.

A profile holds only a stable ID, a unique label, the SSH target, and whether
the machine is enabled. Several profiles may point at the same target.
Passwords, keys, agent sockets, and control sockets stay with OpenSSH, and
targets that embed a password or use URI syntax are rejected. The catalog is
the private file `$XDG_STATE_HOME/fut/machines.toml`
(`~/.local/state/fut/machines.toml` by default). Disabling or removing a
profile only affects the catalog: the remote daemon and its panes keep running.
Open the navigator (`Ctrl-b s`) in a local client to browse Local and every
saved machine together. Fut adds the machine level only when a saved profile
exists, so the one-machine hierarchy and keybindings remain unchanged. Online
resources can be selected directly; Fut freezes input while it prepares the
new attachment, validates a fresh whole-tab view, applies geometry, and then
atomically changes ownership. Tabs never combine panes from different machines.
Agent search and notifications include machine labels when federation is
configured. Cached resources from reconnecting or disabled machines remain
visible as stale summaries but cannot be selected, acknowledged, resized, or
sent commands.

While a local Fut interface is open, it independently watches Local and every
enabled saved machine for resource, presence, agent, alert, and extension
metadata. These background connections never request terminal screens or start,
stop, or upgrade a daemon. Background SSH is non-interactive and requires an
already trusted host key and non-prompting authentication (normally an
`ssh-agent`, hardware agent, or an OpenSSH configuration that needs no prompt).
Connect with ordinary `ssh` first to review a new host key; Fut never accepts or
repairs host keys and stores no credentials. Transient disconnects
retain clearly stale metadata and retry with a capped delay; host-key,
authentication, installation, and protocol problems wait for interactive repair.
Changes made with `fut machine enable`, `disable`, `rename`, or `remove` are
picked up by an open interface without changing its active machine.
The catalog accepts at most 256 profiles. Retry delays grow from one to at most
30 seconds independently per endpoint, so one failed machine neither blocks
another machine nor steals focus when it reconnects.

For Git repositories, Fut groups linked worktrees from the same repository as
peer workspaces in one session. Ordinary directories get their own session.
Opening the same location again reuses it; bare Git repositories are not valid
workspaces.

## Everyday controls

These default bindings follow the `Ctrl-b` prefix, configurable as `ui.prefix`.
The configuration name is the key to override under `[ui.bindings]`, or to
assign directly without a prefix under `[ui.hotkeys]`.

| Key | Action | Configuration name |
| --- | --- | --- |
| `:` | Search every command and configured extension action | `open_command_bar` |
| `m` | Show messages from the current client session | `open_messages` |
| `Shift-S` | Open a configured project or path | `open_project` |
| `R` | Reload global and focused-project configuration | `reload_config` |
| `[` | Enter copy mode | `enter_copy_mode` |
| `s` | Search sessions, workspaces, tabs, and panes | `open_navigator` |
| `a` | Search agents and see their live status | `open_agents` |
| `w` | Open the left sidebar | `open_left_sidebar` |
| `]` | Open the right sidebar | `open_right_sidebar` |
| `t` | Focus the tab bar | `open_tab_bar` |
| `u` | List agent notifications and terminal alerts | `open_notifications` |
| `Prefix` | Jump to the next notification or terminal alert | `focus_next_notification` |
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
The command palette also provides unbound `rename-session`, `rename-workspace`,
`rename-tab`, `kill-session`, `kill-workspace`, and `kill-tab` actions for the
focused resource. Session actions target the session to which the client is
attached; closing it ends that attachment along with its terminals.

Press `Ctrl-b Shift-S` to open a project without leaving the client. Fut fuzzy
filters only the explicit `[projects]` catalog and never scans for repositories.
Typing also adds an **Open path** row for the exact value, resolved relative to
the focused workspace; obvious paths and queries without a project match select
that row automatically. Opening a live project navigates to its existing
terminal. A new project applies its recipe and focuses the terminal selected by
that recipe.
If a repository recipe is not yet trusted, Fut shows its exact contents for
review and accepts or declines machine-local approval in the same dialog.

The tab bar and workspace rows also support the mouse: left-click to switch or
activate a clickable extension token, and right-click for create, rename,
close, and sidebar display actions. Token clicks include their visible affixes
and pills and take precedence over switching the surrounding tab or expanded
workspace row. Click a toast to open the complete message log for the current
client session. Drag pane or sidebar dividers to resize them. Fut preserves
application mouse reporting; when an application does not claim the mouse, the
wheel scrolls client-local history and dragging selects text. Hold Shift to
force Fut selection.

## Copy and scrollback

Press `Ctrl-b [` for copy mode. Move with arrows, `hjkl`, Home/End, or page
keys. Space starts or clears a selection; `y` or Enter copies it; Escape or `q`
cancels. `/` searches literal text and `n`/`N` repeats the search.

Clipboard writes use `pbcopy` on macOS. On Linux, Fut automatically uses
`wl-copy` in Wayland sessions or `xclip`/`xsel` in X11 sessions. Install
`wl-clipboard`, `xclip`, or `xsel` if your desktop does not provide one. In an
SSH or headless session without access to a graphical clipboard, Fut reports
that the clipboard is unavailable. A failed copy keeps the selection active
for another attempt.

## Workspaces, tabs, and panes

Workspaces are organizational contexts, not Git objects. Fut uses their root as
a working-directory fallback but does not create or manage worktrees. Unnamed
workspaces show their live directory or Git work tree; unnamed tabs follow the
oldest surviving pane's foreground process. Focusing a pane does not change the
shared tab label. Rename either to keep a fixed label, or submit an empty name
to restore the automatic label.

`fut open --parent-workspace WORKSPACE_ID PATH` explicitly nests a newly
created workspace beneath another workspace in the same session. Nesting is
display and provenance metadata: the navigator, workspace sidebar, `fut list`,
and next/previous workspace traversal use depth-first tree order, while JSON
snapshots expose `parent_workspace_id`. Closing a parent never closes its
children; when it disappears, its direct children inherit its parent and keep
their own descendants. Children become top-level only when their removed parent
was top-level. Reopening an existing location with this creation-only option is
rejected rather than silently changing its parent.

Pane splits and divider sizes are shared daemon state. Client focus, zoom,
scrollback, dialogs, and configuration are local. Multiple clients may attach
to the same session. Concurrent divider drags use the last ratio accepted by
the daemon, and every client reconciles to that shared ratio. If another client
changes the pane topology during a drag, Fut cancels the stale drag and shows
`Layout changed in another client`. Attached clients also share terminal input
and output, and shared PTYs use the smallest attached client's dimensions.

## Terminal alerts

Fut reports real BEL characters as terminal-native attention without
interpreting ordinary output as agent completion or blocking. An OSC string
terminator is not a bell, and repeated bells are represented by a bounded
counter rather than an event log.

Bells roll up to tabs, workspace rows, sessions, and navigator results. Press
`Ctrl-b u` to inspect them, Enter to switch to the pane, or `c` to acknowledge
the selected bell or agent notification without changing agent lifecycle state.
`Ctrl-b Ctrl-b` moves to the next alert using typed pane navigation, considering
the current terminal last; when it is the only waiting terminal, Fut reveals and
acknowledges it or confirms that its already-visible attention was cleared.
Rendering the focused pane acknowledges its current bell for this client only;
another attached client keeps its own seen state. A new outer terminal starts at
the current bell baseline. Agent lifecycle state remains separate.
See [Configuration](../configuration/) for the opt-in outer-terminal BEL.

Fut renders standard terminal mouse modes, indexed and RGB color, OSC 8
hyperlinks, cursor shapes, bracketed paste, alternate screens, application
cursor keys, mode-aware modified keys, and Kitty graphics used by terminals
such as Ghostty, Kitty, and WezTerm. When attached directly from Ghostty,
Kitty, WezTerm, foot, or Alacritty, Fut negotiates the Kitty keyboard protocol
with the outer terminal. This keeps keys such as `Ctrl-I` distinct from Tab,
preserves modified navigation keys, and lets inner applications request
modifyOtherKeys or Kitty press/repeat/release reporting. Plain text stays on the
traditional UTF-8 path so input generated by accessibility tools and input
methods retains its associated text. Other outer terminals, including terminal
chains that do not advertise this capability, stay on the traditional input
path: text, Unicode, Ctrl/Alt chords, navigation keys, and F1–F12 remain
compatible, but ambiguities already present in legacy terminal input cannot be
recovered.

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
fut list --verbose
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
fut machine add TARGET [--label LABEL]
fut machine list
fut machine show MACHINE
fut machine rename MACHINE LABEL
fut machine enable MACHINE
fut machine disable MACHINE
fut machine remove MACHINE
```

Commands after `--` are passed directly, without shell evaluation. Creation
and mutation commands do not change another client's visual focus. Inside Fut,
many resource IDs may be omitted and are resolved from the caller's live
terminal ancestry. Human-facing output and shell completion use 23-character
compact Fut IDs. They encode the complete 128-bit identity, so they are stable
and do not depend on the current set of resources. Every ID argument also
accepts the canonical UUID form. Automation should use `--json`, retain the
returned canonical UUIDs, and pass explicit IDs to later commands.

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
