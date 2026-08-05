---
layout: default
title: Configuration
description: Configure Fut's client chrome and bindings.
permalink: /configuration/
---

# Configuration

Fut's global UI configuration is safe, declarative, and non-executable. It controls each newly attached client; it never changes daemon-owned resources or runs commands.

## Location and lifecycle

Fut checks, in order:

1. the absolute path in `FUT_CONFIG`;
2. `$XDG_CONFIG_HOME/fut/config.toml` when `XDG_CONFIG_HOME` is absolute;
3. `~/.config/fut/config.toml`.

A missing implicit file uses defaults and is not created. `FUT_CONFIG` must be absolute and must exist. Configuration is loaded once before an interactive client changes terminal state. Live reload is not currently supported. Control commands and shell completion do not load UI configuration; `fut doctor` reads it without creating runtime state.

Files must be regular UTF-8 files no larger than 64 KiB. Unknown fields, invalid values, unsafe control or bidirectional-formatting characters, ambiguous segments, and out-of-scope tokens are errors.

## Complete example

```toml
[ui]
pane_layout = "splits" # "splits" or "accordion"

[ui.bindings]
open_command_bar = "space"
# open_navigator = "g"
# create_tab = "c"

[ui.icons]
preset = "nerd_font" # "ascii", "unicode", or "nerd_font"
# current = "*"      # Every icon may be overridden.
# closing = "x"
# overflow = "..."
# workspace = "W"
# tab = "T"
# zoom = "zoom"
# vertical_divider = "|"

[ui.styles.current]
background = "dark_gray"
remove_modifiers = ["reversed", "underlined"]

[ui.styles.selected]
background = "dark_gray"
add_modifiers = ["underlined"]
remove_modifiers = ["reversed"]

[ui.styles.divider]
foreground = "dark_gray"

[ui.styles.attention]
foreground = "yellow"
add_modifiers = ["bold"]

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
min_width = 12
segments = [
  { text = " " },
  { token = "tab.index" },
  { token = "tab.closing", prefix = " " },
  { text = " " },
]

[ui.workspace_sidebar]
position = "left" # "left" or "right"
width = 24
hide_when_single = true
header = [{ token = "session.name", style = "current" }]
footer = [{ token = "sidebar.status", style = "muted" }]

[ui.workspace_sidebar.row]
left = [{ token = "workspace.marker" }, { text = " " }]
body = [{ token = "workspace.name" }]
right = [{ token = "workspace.tab_count" }, { token = "workspace.closing", prefix = " " }]
detail = [{ token = "workspace.root", style = "muted" }]
```

Omitted fields use defaults. An explicitly empty array hides that lane or format.

Bindings are suffixes after the fixed `Ctrl-b` prefix. Override any action under `ui.bindings`; accepted values are one printable character or the names `space`, `enter`, `tab`, and `esc`. Keys must remain unique. Action names are `open_command_bar`, `open_navigator`, `open_workspace_sidebar`, `open_tab_bar`, `create_tab`, `focus_next_tab`, `focus_previous_tab`, `split_pane_right`, `split_pane_down`, `focus_next_pane`, `focus_previous_pane`, `focus_pane_left`, `focus_pane_down`, `focus_pane_up`, `focus_pane_right`, `focus_last_pane`, `focus_last_tab`, `focus_last_workspace`, `focus_last_session`, `focus_tab_1` through `focus_tab_10`, `toggle_pane_zoom`, and `detach`. The command bar displays and searches the configured bindings.

Sidebar width is 4 through 80 cells and includes its one-cell divider. By default, it collapses when the current session has only one workspace; set `hide_when_single = false` to keep it docked. Otherwise it docks when the host is at least `width + 96` columns wide, so the default width retains the 120-column breakpoint. When collapsed or below that threshold it remains available as an edge drawer without reducing terminal geometry. `vertical_divider` must be exactly one grapheme and one display cell.

## Segments, groups, and components

Every segment sets exactly one of:

- `text` — a literal string;
- `token` — a pure value from Fut's already-materialized client/resource state;
- `component` — a layout-aware repeated collection. The only current component is `tabs`.

Token segments may also set `prefix`, `suffix`, `max_width`, and `style`. Prefix and suffix are emitted only when the token is nonempty. Text segments accept `style` but not affixes or `max_width`. Components must be the only segment in their group and do not accept segment options.

`ui.tab_bar.item.min_width` is a display-cell minimum from 0 through 256. Short items are left-aligned and padded on the right with styled spaces, so current and keyboard-selected states always occupy the same width. The default format supplies one cell of leading padding, and the default minimum is 12; use `0` for intrinsic-width tabs.

Tab-bar lanes contain groups. A group has `segments`, an optional semantic `style`, and a `priority` from 0 through 255. The tabs component is flexible and keeps its active or keyboard-selected item visible. Groups with higher priority than tabs reserve their complete width first. Tabs then grow toward their complete preferred width. Lower-priority groups appear only in remaining space. Left and right lanes stay edge-aligned; the center lane is geometrically centered and clamped between them. Groups never overlap.

Workspace rows have intrinsic `left` and `right` lanes; `body` receives the remaining cells and truncates safely. A nonempty `detail` format adds a second full-width line. The default detail is the workspace root: this is stable daemon-owned resource state, unlike a shell's live working directory, and avoids process inspection during rendering. Set `detail = []` for compact one-line rows. Header and footer are optional single-line segment lists. At tiny heights Fut preserves resource rows over decorative header/footer content, while switching and error status remains visible.

All widths are terminal display cells. Dynamic values are sanitized and truncated at grapheme boundaries. Bars never wrap.

## Styles

The fixed semantic roles are:

- `normal`
- `muted`
- `current`
- `selected`
- `closing`
- `attention`
- `error`
- `divider`

Each style accepts:

```toml
foreground = "yellow"
background = "default"
add_modifiers = ["bold", "underlined"]
remove_modifiers = ["dim"]
```

Modifiers are `bold`, `dim`, `italic`, `underlined`, `reversed`, and `crossed_out`.

Colors may be `default`, ANSI names such as `red`, `blue`, `gray`, `dark_gray`, or `light_cyan`, an indexed color such as `index:123`, or exact RGB such as `#12abef`. Indexed colors remain references to the containing terminal's palette; RGB colors remain exact.

Styles compose in this order: normal, group/segment style, current, attention, closing, selected. Later foreground/background values replace earlier ones; modifiers are added or removed in sequence.

## Icons

`unicode` is the default and requires no private-use glyphs. `ascii` uses ASCII for the configurable resource icons; ordinary built-in help and truncation text may still use Unicode. `nerd_font` opts into a small Nerd Fonts v3-oriented resource/state set. Every icon can be overridden under `[ui.icons]`.

Fut cannot reliably detect the active terminal font. Use [`fut doctor`](doctor.md) for an honest visual probe; it never claims that an installed font is active.

## Security boundary

UI configuration has no shell commands, file reads, environment interpolation, networking, functions, or expression language. Built-in tokens are pure. Future dynamic tokens will use separately designed asynchronous providers with explicit trust, caching, and timeouts rather than executing during rendering.

## Related

- [Presentation tokens](tokens.md)
- [Diagnostics](doctor.md)
