---
layout: default
title: Configuration
description: Configure Fut's client chrome and bindings.
permalink: /configuration/
---

# Configuration

Fut's global presentation configuration is safe, declarative, and non-executable. Separately, `trusted_commands` and explicitly configured local extension directories are executable trust boundaries; see [Security boundary](#security-boundary). Multiple terminal windows may attach to the same session at once. They share terminal input and output, while focus, dialogs, scrollback, and configuration remain local to each client. Shared PTYs use the smallest attached client's dimensions; resizing or detaching a client immediately recalculates that geometry, and larger clients separate the shared content from its unused margin with a subtle line and sparse muted star field.

Fut refuses to start or attach another interactive Fut client from inside one of its terminals. If nesting is intentional, set `FUT_ALLOW_NESTED` in the command's environment to force it, for example `FUT_ALLOW_NESTED=1 fut`.

## Location and lifecycle

Fut checks, in order:

1. the absolute path in `FUT_CONFIG`;
2. `$XDG_CONFIG_HOME/fut/config.toml` when `XDG_CONFIG_HOME` is absolute;
3. `~/.config/fut/config.toml`.

A missing implicit file uses defaults and is not created. `FUT_CONFIG` must be absolute and must exist. Configuration is loaded before an interactive client changes terminal state. Press `Ctrl-b Shift-R` to reload the invoking client's configuration; a valid configuration applies bindings and layout immediately, while any location, read, parse, or validation error leaves the complete previous configuration active and appears as a one-line notice. Control commands and shell completion do not load UI configuration; `fut doctor` reads it without creating runtime state.

Files must be regular UTF-8 files no larger than 64 KiB. Unknown fields, invalid values, unsafe control or bidirectional-formatting characters, ambiguous segments, and out-of-scope tokens are errors.

## Complete example

```toml
extensions = [
  "/Users/me/.config/fut/extensions/review-status",
]

[ui]
pane_layout = "splits" # "splits" or "accordion"

[ui.bindings]
open_command_bar = "space"
# reload_config = "R"
# enter_copy_mode = "["
# open_navigator = "s"
# focus_next_notification = "prefix"
# create_workspace = "C"
# create_tab = "c"

[trusted_commands.git_diff]
title = "Repository diff"
binding = "g"
program = "/Users/me/.dotfiles/tmux/tmux.symlink/git_diff_popup.sh"
# args = ["--optional-argument"]

[ui.icons]
preset = "nerd_font" # "ascii", "unicode", or "nerd_font"
# current = "*"      # Every icon may be overridden.
# closing = "x"
# overflow = "..."
# workspace = "W"
# tab = "T"
# zoom = "zoom"
# vertical_divider = "|"
# pill_left = ""       # Focused-tab pill caps; empty outside "nerd_font".
# pill_right = ""

[ui.styles.current]
foreground = "blue"
add_modifiers = ["reversed"]
remove_modifiers = ["underlined"]

[ui.styles.selected]
background = "dark_gray"
remove_modifiers = ["reversed"]

# Navigator resource levels default to red, blue, green, and magenta.
[ui.styles.session]
foreground = "red"

[ui.styles.workspace]
foreground = "blue"

[ui.styles.tab]
foreground = "green"

[ui.styles.pane]
foreground = "magenta"

[ui.styles.divider]
foreground = "dark_gray"

[ui.styles.attention]
foreground = "yellow"
add_modifiers = ["bold"]

[ui.styles.activity]
foreground = "light_cyan"

[ui.styles.added]
foreground = "green"

[ui.styles.deleted]
foreground = "red"

[ui.tab_bar]
position = "top" # "top" or "bottom"
left = [
  { segments = [{ token = "workspace.icon", suffix = " " }, { token = "workspace.name", max_width = 20 }], style = "muted", priority = 200 },
]
center = [
  { segments = [{ component = "tabs" }], priority = 100 },
]
right = [
  { segments = [{ token = "client.zoom", suffix = " " }], priority = 255 },
  { segments = [{ token = "client.help" }], style = "muted", priority = 0 },
]

[ui.tab_bar.item]
segments = [
  { text = " " },
  { token = "tab.index" },
  { token = "tab.name", prefix = " " },
  { token = "tab.closing", prefix = " " },
  { text = " " },
]

[ui.workspace_sidebar]
position = "left" # "left" or "right"
width = 28
display = "expanded" # "expanded" or "minimized"
visibility = "auto_hide_when_single" # "visible", "auto_hide_when_single", or "hidden"
header = [{ token = "session.name", style = "current" }]
footer = [{ token = "sidebar.status", style = "muted" }]

[ui.workspace_sidebar.row]
left = [{ token = "workspace.marker" }, { text = " " }]
body = [{ token = "workspace.index" }, { token = "workspace.name", prefix = " " }]
right = [{ token = "workspace.tab_count" }, { token = "workspace.closing", prefix = " " }, { text = " " }]
detail = [
  { text = "    " },
  { token = "workspace.git_branch", style = "muted" },
  { token = "workspace.git_added", prefix = " " },
  { token = "workspace.git_deleted", prefix = " " },
]
```

Omitted fields use defaults. An explicitly empty array hides that lane or format.

## Local extensions

`extensions` is an explicit list of absolute local directory paths. Fut loads the directories in listed order; it does not discover project extensions, search additional paths, download packages, or provide an installer or registry. Every directory must contain a regular UTF-8 `fut-extension.toml` no larger than 64 KiB. At most 32 extensions may be configured, and the complete set is accepted or rejected atomically. Duplicate canonical roots and duplicate extension IDs are errors.

The smallest complete manifest is:

```toml
id = "review-status"
```

IDs and declaration names are 1–64 bytes, start with a lowercase ASCII letter, end with a lowercase letter or digit, and otherwise use lowercase letters, digits, or single `.`, `_`, and `-` separators. Manifests are strict: unknown fields are errors.

An extension may declare implemented workspace lifecycle hooks and dynamic presentation tokens:

```toml
id = "review-status"

[hooks]
"workspace.created" = ["./bin/refresh", "--quiet"]
"workspace.renamed" = ["./bin/refresh", "--quiet"]
"workspace.closed" = ["./bin/refresh", "--quiet"]

[[presentation_tokens]]
name = "state"
scope = "workspace"
```

The only accepted hook names are `workspace.created`, `workspace.renamed`, and `workspace.closed`; any other name rejects the complete extension configuration and prevents daemon setup. The daemon loads the global `extensions` list before creating its initial resources, so `workspace.created` observes the initial workspace too. Hooks are queued in committed mutation order and run one at a time, asynchronously and strictly after a successful resource commit. They cannot veto, delay, rewrite, or roll back the operation. The queue holds 128 events; enqueue never waits, and an event arriving while it is full is dropped with rate-limited diagnostics.

Hook values are direct argv arrays, not shell strings. An executable beginning with `./` is resolved against the extension root and may not lexically escape it; otherwise it must be an absolute path or a single executable name for `PATH` lookup. Fut does not add a shell. The command's working directory is the canonical extension root. A manifest may declare at most 32 hooks and 64 presentation tokens. Commands are limited to 64 argv values of at most 4096 bytes each.

Each hook inherits the daemon environment and receives these overrides: `FUT_BIN` (the exact running Fut executable), `FUT_EXTENSION_ID`, `FUT_EXTENSION_ROOT`, `FUT_EVENT`, `FUT_EVENT_VERSION=1`, `FUT_SOCKET`, `FUT_SESSION_ID`, and `FUT_WORKSPACE_ID`. Fut writes one UTF-8 JSON object plus a trailing newline to stdin, with a 64 KiB serialized limit. Version 1 has this exact shape (only `workspace.renamed` includes `previous_name`):

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

Created and closed payloads carry the workspace name and root at that event; close context is captured before cascade deletion. Each process has a fixed five-second timeout. Fut continuously drains stdout and stderr to avoid pipe backpressure but retains at most 16 KiB of each for diagnostics. Spawn errors, oversized payloads, stdin errors, nonzero exits, output, and timeouts are diagnostic only: they never change daemon state or stop later hooks. Shutdown gives the runner a one-second grace period and then cancels it, so daemon exit cannot hang on an extension.

Token scopes are `session`, `workspace`, `tab`, and `pane`. Names must be unique within a manifest and are qualified as `<scope>.extension.<extension-id>.<name>`; for example, the declaration above becomes `workspace.extension.review-status.state`. Token references in `ui` are checked against the extensions loaded from that same global configuration. Startup and configuration reload reject the complete new configuration if a token is undeclared or used in an incompatible context.

An extension command publishes plain presentation text through the normal control socket:

```sh
fut token publish review-status state ready \
  --workspace-id "$FUT_WORKSPACE_ID"
```

Exactly one of `--session-id`, `--workspace-id`, `--tab-id`, or `--pane-id` is required. The option must match the declaration scope. The daemon rejects unknown extensions and declarations, missing or closing targets, scope mismatches, control and bidirectional formatting characters, values over 1 KiB, and publication beyond the daemon-wide 4096-value materialization bound. Only declared namespaced extension tokens are publishable through this command; Fut's built-in token names are internal. Re-publishing the same value succeeds without changing the resource revision. Materialized values belong to their target and disappear through normal cascade cleanup when it closes.

Values are strings only: they carry no style, action, service, or executable behavior. They are copied into ordinary resource snapshots, so rendering stays synchronous and pure. See [Presentation tokens](tokens.md#extension-tokens) for compatible UI contexts.

The checked-in `examples/extensions/example-workspace-status` extension is executable documentation for this complete path. Its packaged hook calls the exact `$FUT_BIN` through `$FUT_SOCKET` and publishes the latest workspace lifecycle event as a token. The end-to-end suite runs that same example to keep the documented contract from drifting.

Bindings are suffixes after the fixed `Ctrl-b` prefix. `Ctrl-b :` opens the command palette. Override any action under `ui.bindings`; accepted values are one printable character or the names `prefix`, `ctrl-s`, `space`, `enter`, `tab`, `esc`, `up`, and `down`. `prefix` means pressing `Ctrl-b` again. Keys must remain unique. Action names are `open_command_bar`, `reload_config`, `enter_copy_mode`, `open_navigator`, `open_workspace_sidebar`, `open_tab_bar`, `open_notifications`, `focus_next_notification`, `create_workspace`, `create_tab`, `focus_next_tab`, `focus_previous_tab`, `split_pane_right`, `split_pane_down`, `focus_next_pane`, `focus_previous_pane`, `focus_pane_left`, `focus_pane_down`, `focus_pane_up`, `focus_pane_right`, `focus_last_pane`, `focus_last_tab`, `focus_last_workspace`, `focus_last_session`, `focus_next_workspace`, `focus_previous_workspace`, `focus_tab_1` through `focus_tab_10`, `toggle_pane_zoom`, and `detach`. `Ctrl-b Ctrl-s` switches to the last active session, so repeating it toggles between the two most recent sessions. Rebinding `focus_next_notification` away from `prefix` restores `Ctrl-b Ctrl-b` as a literal prefix unless another action uses `prefix`. The palette displays tmux-style command names, descriptions, and configured bindings, and searches all three.

Each `[trusted_commands.NAME]` table requires `title`, `binding`, and an executable `program`, plus an optional string array `args`. Running one opens a dashed frame containing a temporary PTY over the complete client terminal, inherits the focused pane process's live working directory, and sends normal terminal input to the command. The frame names the command and identifies the temporary surface; when the process exits, Fut restores the previous panes, focus, and geometry. A trusted command may take a built-in's default key, which unbinds that built-in unless it is explicitly rebound under `ui.bindings`. Explicit binding collisions and duplicate command keys are rejected. Commands appear in both the command palette and delayed which-key help, and configuration reload replaces them atomically.

`open_navigator` (default `Ctrl-b s`) opens the single cross-resource dialog. Printable text fuzzy-filters individual hierarchical rows against their full ancestor path; every query term must match. While filtering, each result shows that complete session › workspace › tab › pane path in muted text, with only the fuzzy-matched characters emphasized. Use arrows, Home/End, page keys, or `Ctrl-j`/`Ctrl-k` to move. `Ctrl-s` shows only sessions; `Ctrl-w`, `Ctrl-t`, and `Ctrl-p` show workspaces in the selected session, tabs in the selected workspace, or panes in the selected tab. The title breadcrumb names that enclosing scope with each resource name shortened to ten cells. Repeat the active filter to restore the complete tree, or press `Ctrl-a` to clear both the resource filter and text search. With no resource or text filter, Left/Right and Shift-arrows navigate the hierarchy. Enter switches and Escape closes. Plain `q` is search text. Tabs with one pane show a muted `· pane` inline; tabs with multiple panes keep one row per pane. A newly created tab briefly appears as positional `tab 1`, `tab 2`, and so on until its foreground process is available.

`enter_copy_mode` (default `Ctrl-b [`) opens per-client scrollback navigation for the focused terminal. Move by physical terminal cells with arrows or `hjkl`, Home/End, and Page Up/Page Down. Space starts or clears a selection; movement extends it. `y` or Enter copies plain text through the local client's bounded `pbcopy` process and exits only after the clipboard write succeeds. A clipboard error leaves the selection active so `y` can retry. Escape or `q` cancels. `/` opens a literal-search prompt, where Escape closes only the prompt (`q` is ordinary query text); `n` and `N` repeat forward and backward after the prompt closes. Rapid actions are processed in key order; the copy cue reports if the bounded local queue cannot accept another action. Copy-mode keys and search paste never reach the terminal process.

Mouse input uses pane-local cells. The first left click on an unfocused pane changes focus without reaching either application. Focused applications receive only the press, release, drag, motion, and wheel reports enabled by their terminal mouse modes. Without mouse reporting, a wheel uses DEC alternate scroll on an alternate screen and otherwise changes only that client's scrollback viewport. In the default `splits` layout, left-drag a visible pane divider to resize that authored split cell by cell; the shared layout remains in the daemon across detach and updates other clients. Recursive pane minimums still apply. Accordion, zoom, and focused-only fallback layouts have no draggable pane dividers. A gesture keeps its initial owner, so a drag that starts in an application never becomes a Fut resize and a divider drag never reaches the application. Copy mode and modal surfaces suppress new application and divider gestures.

`open_notifications` (default `Ctrl-b u`) opens the per-client list of terminals with unseen blocked or completed reports. `focus_next_notification` (default `Ctrl-b Ctrl-b`) switches to the next such terminal in resource order, wrapping and skipping the current, seen, or closing terminals. It does nothing when none is waiting. Selecting or viewing that terminal marks its current attention seen only for that client.

Inside the workspace sidebar, `1` through `9` and `0` switch straight to that workspace in session order, exactly as Enter switches to the highlighted row. Press `?` for the sidebar's hotkey list; any key returns to the workspaces.

Sidebar width is 4 through 80 cells and includes its one-cell divider. Left-drag the visible divider from either edge to resize it cell by cell. A docked drag preserves at least 40 terminal columns; an open drawer's divider is also draggable, while a hidden drawer is not. The dragged width belongs only to that attached client: it is not written to configuration and the configured width returns on reattach or configuration reload. The active workspace is marked with a bullet.

Display and visibility are independent. `display = "expanded"` uses the configured width, while `display = "minimized"` uses a fixed six-cell rail with each workspace's active marker, number, and status. The marker reserves a trailing cell so round glyphs remain visually separate from the number in fonts such as Iosevka. Opening a minimized sidebar temporarily expands its drawer to the configured width; the rail itself is not draggable, but its open drawer remains resizable. Press `m` inside the sidebar to toggle expanded/minimized. `visibility = "visible"` keeps the chosen display docked whenever the terminal is wide enough. `visibility = "auto_hide_when_single"` (the default) docks it only when the current session has more than one workspace, and `visibility = "hidden"` leaves it as an on-demand drawer. Press `h` to cycle visible → auto-hide when single → hidden. This allows combinations such as a minimized sidebar that also hides when only one workspace exists. With the default footer, an open sidebar shows `h`, `m`, and `?` on separate lines with their current states; the `nerd_font` preset adds matching visibility, width, and help icons.

When not docked or below the sidebar width plus 40 terminal columns, the sidebar remains available as an edge drawer without reducing terminal geometry. The current display and visibility appear in the default footer. `sidebar.display` and `sidebar.visibility` expose their labels to custom sidebar chrome. `vertical_divider` must be exactly one grapheme and one display cell.

## Segments, groups, and components

Every segment sets exactly one of:

- `text` — a literal string;
- `token` — a pure value from Fut's already-materialized client/resource state;
- `component` — a layout-aware repeated collection. The only current component is `tabs`.

Token segments may also set `prefix`, `suffix`, `max_width`, and `style`. Prefix and suffix are emitted only when the token is nonempty. Text segments accept `style` but not affixes or `max_width`. Components must be the only segment in their group and do not accept segment options.

Tab-item content uses the intrinsic display-cell width of its configured segments. The default format supplies one cell of padding on each side; add or remove text segments to adjust that spacing. The Nerd Font preset additionally reserves one cell at each end of every item so its active-tab pill does not move neighboring tabs. New tabs are created unnamed, so their title follows the foreground process in the focused pane. Name a tab with `fut tab new --name` or `Ctrl-b r` to keep that title fixed; renaming it to an empty string restores automatic naming.

Tab-bar lanes contain groups. A group has `segments`, an optional semantic `style`, and a `priority` from 0 through 255. The tabs component is flexible and keeps its active or keyboard-selected item visible. Groups with higher priority than tabs reserve their complete width first. Tabs then grow toward their complete preferred width. Lower-priority groups appear only in remaining space. Left and right lanes stay edge-aligned; the center lane is geometrically centered and clamped between them. Groups never overlap. By default the right lane names the current workspace, truncated at 20 cells.

Workspace rows have intrinsic `left` and `right` lanes; `body` receives the remaining cells and truncates safely. Expanded default rows retain the same marker and workspace number as the minimized rail, followed by the workspace name, and reserve trailing padding after status indicators so glyphs do not clip at the divider. A nonempty `detail` format adds a second full-width line. Workspaces are unnamed unless explicitly named, presenting as their live location — the work tree (or directory) every open pane is inside, or `multiple` when panes disagree. The default detail aligns under the row name and shows the Git branch at that live location with its short working-tree diff (`+N` inserted, `-N` deleted). The daemon collects those values with bounded background `git` processes and publishes them atomically into the shared resource snapshot, so rendering never waits, attached clients agree, and non-Git locations simply stay empty. Set `detail = []` for compact one-line rows. Header and footer are optional single-line segment lists. At tiny heights Fut preserves resource rows over decorative header/footer content, while switching and error status remains visible.

All widths are terminal display cells. Dynamic values are sanitized and truncated at grapheme boundaries. Bars never wrap.

## Styles

The fixed semantic roles are:

- `normal`
- `muted`
- `session`
- `workspace`
- `tab`
- `pane`
- `current`
- `selected`
- `closing`
- `activity`
- `attention`
- `error`
- `divider`
- `added`
- `deleted`

Each style accepts:

```toml
foreground = "yellow"
background = "default"
add_modifiers = ["bold", "underlined"]
remove_modifiers = ["dim"]
```

Modifiers are `bold`, `dim`, `italic`, `underlined`, `reversed`, and `crossed_out`.

Colors may be `default`, ANSI names such as `red`, `blue`, `gray`, `dark_gray`, or `light_cyan`, an indexed color such as `index:123`, or exact RGB such as `#12abef`. Indexed colors remain references to the containing terminal's palette; RGB colors remain exact.

Styles compose in this order: normal, group style, token style (`activity`, `attention`, `added`, or `deleted` when supplied), segment style, current, attention, closing, selected. Later foreground/background values replace earlier ones; modifiers are added or removed in sequence.

## Icons

`unicode` is the default and requires no private-use glyphs. `ascii` uses ASCII for the configurable resource icons; ordinary built-in help and truncation text may still use Unicode. `nerd_font` opts into a small Nerd Fonts v3-oriented resource/state set, and additionally draws the focused tab as a filled pill using powerline half-circle caps. Every icon can be overridden under `[ui.icons]`.

Fut cannot reliably detect the active terminal font. Use [`fut doctor`](doctor.md) for an honest visual probe; it never claims that an installed font is active.

## Security boundary

Everything under `ui` remains non-executable: it has no shell commands, file reads, environment interpolation, networking, functions, or expression language, and presentation tokens resolve only already-materialized strings. `trusted_commands` is a deliberate executable trust boundary. Fut executes its `program` directly with the configured `args` and the client's environment; it does not invoke a shell unless you explicitly configure one. Explicit extension roots are the equivalent trust decision for extension manifests and their packaged commands. Only configure commands and extension directories you trust. Commands never run during configuration parsing or rendering; supported lifecycle hooks run only from committed daemon mutations. The same-user Unix socket is the authorization boundary for token publication, just as it is for other Fut control commands; Fut does not claim to authenticate the publishing process beyond that boundary.

## Related

- [Presentation tokens](tokens.md)
- [Diagnostics](doctor.md)
