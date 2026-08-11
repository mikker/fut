---
layout: default
title: Configuration
description: Configure Fut's client chrome and bindings.
permalink: /configuration/
---

# Configuration

Fut's global presentation configuration is safe, declarative, and non-executable. A separate, explicitly named `trusted_commands` section may run programs from keybindings; see [Security boundary](#security-boundary). Multiple terminal windows may attach to the same session at once. They share terminal input and output, while focus, dialogs, scrollback, and configuration remain local to each client. Shared PTYs use the smallest attached client's dimensions; resizing or detaching a client immediately recalculates that geometry.

## Location and lifecycle

Fut checks, in order:

1. the absolute path in `FUT_CONFIG`;
2. `$XDG_CONFIG_HOME/fut/config.toml` when `XDG_CONFIG_HOME` is absolute;
3. `~/.config/fut/config.toml`.

A missing implicit file uses defaults and is not created. `FUT_CONFIG` must be absolute and must exist. Configuration is loaded before an interactive client changes terminal state. Press `Ctrl-b Shift-R` to reload the invoking client's configuration; a valid configuration applies bindings and layout immediately, while any location, read, parse, or validation error leaves the complete previous configuration active and appears as a one-line notice. Control commands and shell completion do not load UI configuration; `fut doctor` reads it without creating runtime state.

Files must be regular UTF-8 files no larger than 64 KiB. Unknown fields, invalid values, unsafe control or bidirectional-formatting characters, ambiguous segments, and out-of-scope tokens are errors.

## Complete example

```toml
[ui]
pane_layout = "splits" # "splits" or "accordion"

[ui.bindings]
open_command_bar = "space"
# reload_config = "R"
# enter_copy_mode = "["
# open_navigator = "g"
# focus_next_notification = "prefix"
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
min_width = 8
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
visibility = "auto_hide_when_single" # "visible", "auto_hide_when_single", or "hidden"
header = [{ token = "session.name", style = "current" }]
footer = [{ token = "sidebar.status", style = "muted" }]

[ui.workspace_sidebar.row]
left = [{ token = "workspace.marker" }, { text = " " }]
body = [{ token = "workspace.name" }]
right = [{ token = "workspace.tab_count" }, { token = "workspace.closing", prefix = " " }]
detail = [
  { text = "  " },
  { token = "workspace.git_branch", style = "muted" },
  { token = "workspace.git_added", prefix = " " },
  { token = "workspace.git_deleted", prefix = " " },
]
```

Omitted fields use defaults. An explicitly empty array hides that lane or format.

Bindings are suffixes after the fixed `Ctrl-b` prefix. Override any action under `ui.bindings`; accepted values are one printable character or the names `prefix`, `space`, `enter`, `tab`, `esc`, `up`, and `down`. `prefix` means pressing `Ctrl-b` again. Keys must remain unique. Action names are `open_command_bar`, `reload_config`, `enter_copy_mode`, `open_navigator`, `open_workspace_sidebar`, `open_tab_bar`, `open_notifications`, `focus_next_notification`, `create_tab`, `focus_next_tab`, `focus_previous_tab`, `split_pane_right`, `split_pane_down`, `focus_next_pane`, `focus_previous_pane`, `focus_pane_left`, `focus_pane_down`, `focus_pane_up`, `focus_pane_right`, `focus_last_pane`, `focus_last_tab`, `focus_last_workspace`, `focus_last_session`, `focus_next_workspace`, `focus_previous_workspace`, `focus_tab_1` through `focus_tab_10`, `toggle_pane_zoom`, and `detach`. Rebinding `focus_next_notification` away from `prefix` restores `Ctrl-b Ctrl-b` as a literal prefix unless another action uses `prefix`. The command bar displays and searches the configured bindings.

Each `[trusted_commands.NAME]` table requires `title`, `binding`, and an executable `program`, plus an optional string array `args`. Running one opens a temporary PTY over the complete client terminal, inherits the focused pane process's live working directory, and sends normal terminal input to the command. When it exits, Fut restores the previous panes, focus, and geometry. A trusted command may take a built-in's default key (as `git_diff` takes `g` above); rebind that built-in under `ui.bindings` to keep it. Explicit binding collisions and duplicate command keys are rejected. Commands appear in both the command palette and delayed which-key help, and configuration reload replaces them atomically.

`open_navigator` (default `Ctrl-b g`) opens the single cross-resource dialog. Printable text fuzzy-filters individual hierarchical rows against their full ancestor path; every query term must match. Use arrows, Home/End, page keys, or `Ctrl-j`/`Ctrl-k` to move. With an empty query, Left/Right and Shift-arrows navigate the hierarchy while `Ctrl-s`/`Ctrl-w`/`Ctrl-t`/`Ctrl-p` cycle structural levels. Enter switches and Escape closes. Plain `q` is search text. A newly created tab briefly appears as positional `tab 1`, `tab 2`, and so on until its foreground process is available.

`enter_copy_mode` (default `Ctrl-b [`) opens per-client scrollback navigation for the focused terminal. Move by physical terminal cells with arrows or `hjkl`, Home/End, and Page Up/Page Down. Space starts or clears a selection; movement extends it. `y` or Enter copies plain text through the local client's bounded `pbcopy` process and exits only after the clipboard write succeeds. A clipboard error leaves the selection active so `y` can retry. Escape or `q` cancels. `/` opens a literal-search prompt, where Escape closes only the prompt (`q` is ordinary query text); `n` and `N` repeat forward and backward after the prompt closes. Rapid actions are processed in key order; the copy cue reports if the bounded local queue cannot accept another action. Copy-mode keys and search paste never reach the terminal process.

Mouse input uses pane-local cells. The first left click on an unfocused pane changes focus without reaching either application. Focused applications receive only the press, release, drag, motion, and wheel reports enabled by their terminal mouse modes. Without mouse reporting, a wheel uses DEC alternate scroll on an alternate screen and otherwise changes only that client's scrollback viewport. In the default `splits` layout, left-drag a visible pane divider to resize that authored split cell by cell; the shared layout remains in the daemon across detach and updates other clients. Recursive pane minimums still apply. Accordion, zoom, and focused-only fallback layouts have no draggable pane dividers. A gesture keeps its initial owner, so a drag that starts in an application never becomes a Fut resize and a divider drag never reaches the application. Copy mode and modal surfaces suppress new application and divider gestures.

`open_notifications` (default `Ctrl-b u`) opens the per-client list of terminals with unseen blocked or completed reports. `focus_next_notification` (default `Ctrl-b Ctrl-b`) switches to the next such terminal in resource order, wrapping and skipping the current, seen, or closing terminals. It does nothing when none is waiting. Selecting or viewing that terminal marks its current attention seen only for that client.

Inside the workspace sidebar, `1` through `9` and `0` switch straight to that workspace in session order, exactly as Enter switches to the highlighted row. Press `?` for the sidebar's hotkey list; any key returns to the workspaces.

Sidebar width is 4 through 80 cells and includes its one-cell divider. Left-drag the visible divider from either edge to resize it cell by cell. A docked drag preserves at least 96 terminal columns; an open drawer's divider is also draggable, while a hidden drawer is not. The dragged width belongs only to that attached client: it is not written to configuration and the configured width returns on reattach or configuration reload. The active workspace is marked with a bullet. `visibility = "visible"` keeps the sidebar docked whenever the terminal is wide enough. `visibility = "auto_hide_when_single"` (the default) docks it only when the current session has more than one workspace. `visibility = "hidden"` leaves it as an on-demand drawer. Press `h` inside the sidebar to cycle visible → auto-hide when single → hidden. When not docked or below the `width + 96` column threshold (124 by default), it remains available as an edge drawer without reducing terminal geometry. `vertical_divider` must be exactly one grapheme and one display cell.

## Segments, groups, and components

Every segment sets exactly one of:

- `text` — a literal string;
- `token` — a pure value from Fut's already-materialized client/resource state;
- `component` — a layout-aware repeated collection. The only current component is `tabs`.

Token segments may also set `prefix`, `suffix`, `max_width`, and `style`. Prefix and suffix are emitted only when the token is nonempty. Text segments accept `style` but not affixes or `max_width`. Components must be the only segment in their group and do not accept segment options.

`ui.tab_bar.item.min_width` is a display-cell minimum from 0 through 256. Short items are left-aligned and padded on the right with styled spaces, so current and keyboard-selected states always occupy the same width. The default format supplies one cell of leading padding, and the default minimum is 8; use `0` for intrinsic-width tabs. New tabs are created unnamed, so their title follows the foreground process in the focused pane. Name a tab with `fut tab new --name` or `Ctrl-b r` to keep that title fixed; renaming it to an empty string restores automatic naming.

Tab-bar lanes contain groups. A group has `segments`, an optional semantic `style`, and a `priority` from 0 through 255. The tabs component is flexible and keeps its active or keyboard-selected item visible. Groups with higher priority than tabs reserve their complete width first. Tabs then grow toward their complete preferred width. Lower-priority groups appear only in remaining space. Left and right lanes stay edge-aligned; the center lane is geometrically centered and clamped between them. Groups never overlap. By default the right lane names the current workspace, truncated at 20 cells.

Workspace rows have intrinsic `left` and `right` lanes; `body` receives the remaining cells and truncates safely. A nonempty `detail` format adds a second full-width line, and one blank line separates each entry. The default detail indents two cells under the row name and shows the workspace root's Git branch with its short working-tree diff (`+N` inserted, `-N` deleted). Those Git values come from bounded background `git` processes cached per root, so rendering never waits and non-Git roots simply stay empty. Set `detail = []` for compact one-line rows. Header and footer are optional single-line segment lists. At tiny heights Fut preserves resource rows over decorative header/footer content, while switching and error status remains visible.

All widths are terminal display cells. Dynamic values are sanitized and truncated at grapheme boundaries. Bars never wrap.

## Styles

The fixed semantic roles are:

- `normal`
- `muted`
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

Everything under `ui` remains non-executable: it has no shell commands, file reads, environment interpolation, networking, functions, or expression language, and built-in presentation tokens are pure. `trusted_commands` is a deliberate executable trust boundary. Fut executes its `program` directly with the configured `args` and the client's environment; it does not invoke a shell unless you explicitly configure one. Only add commands from configuration you trust. Commands run on demand, never during parsing or rendering.

## Related

- [Presentation tokens](tokens.md)
- [Diagnostics](doctor.md)
