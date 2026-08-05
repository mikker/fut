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
add_modifiers = ["bold"]

[ui.styles.selected]
add_modifiers = ["reversed"]

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
  { segments = [{ token = "client.zoom", suffix = " " }], style = "current", priority = 255 },
  { segments = [{ token = "client.help" }], style = "muted", priority = 0 },
]

[ui.tab_bar.item]
segments = [
  { text = " " },
  { token = "tab.marker" },
  { text = " " },
  { token = "tab.name", max_width = 32 },
  { token = "tab.closing", prefix = " " },
  { text = " " },
]

[ui.workspace_sidebar]
position = "left" # "left" or "right"
width = 24
header = [{ token = "session.name", style = "current" }]
footer = [{ token = "sidebar.status", style = "muted" }]

[ui.workspace_sidebar.row]
left = [{ token = "workspace.marker" }, { text = " " }]
body = [{ token = "workspace.name" }]
right = [{ token = "workspace.tab_count" }, { token = "workspace.closing", prefix = " " }]
```

Omitted fields use defaults. An explicitly empty array hides that lane or format.

Sidebar width is 4 through 80 cells and includes its one-cell divider. It docks when the host is at least `width + 96` columns wide, so the default width retains the 120-column breakpoint. Below that threshold it remains available as an edge drawer without reducing terminal geometry. `vertical_divider` must be exactly one grapheme and one display cell.

## Segments, groups, and components

Every segment sets exactly one of:

- `text` — a literal string;
- `token` — a pure value from Fut's already-materialized client/resource state;
- `component` — a layout-aware repeated collection. The only current component is `tabs`.

Token segments may also set `prefix`, `suffix`, `max_width`, and `style`. Prefix and suffix are emitted only when the token is nonempty. Text segments accept `style` but not affixes or `max_width`. Components must be the only segment in their group and do not accept segment options.

Tab-bar lanes contain groups. A group has `segments`, an optional semantic `style`, and a `priority` from 0 through 255. The tabs component is flexible and keeps its active or keyboard-selected item visible. Groups with higher priority than tabs reserve their complete width first. Tabs then grow toward their complete preferred width. Lower-priority groups appear only in remaining space. Left and right lanes stay edge-aligned; the center lane is geometrically centered and clamped between them. Groups never overlap.

Workspace rows have intrinsic `left` and `right` lanes; `body` receives the remaining cells and truncates safely. Header and footer are optional single-line segment lists. At tiny heights Fut preserves resource rows over decorative header/footer content, while switching and error status remains visible.

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
