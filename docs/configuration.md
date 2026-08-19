---
layout: default
title: Configuration
description: Configure Fut's client UI, commands, and extensions.
permalink: /configuration/
---

# Configuration

> **TL;DR:** Put overrides in `~/.config/fut/config.toml`, then press
> `Ctrl-b Shift-R`. Everything under `ui` is declarative. `trusted_commands`,
> extension directories, and [trusted project recipes](projects.md) can execute
> programs and should be treated like shell scripts.

Fut's global presentation configuration is safe, declarative, and
non-executable. Separately, `trusted_commands`, explicitly configured local
extension directories, and trusted project recipes are executable boundaries; see
[Security boundary](#security-boundary).

Fut refuses to start or attach another interactive Fut client from inside one of its terminals. If nesting is intentional, set `FUT_ALLOW_NESTED` in the command's environment to force it, for example `FUT_ALLOW_NESTED=1 fut`.

## Location and lifecycle

Fut checks, in order:

1. `config.toml` in the absolute directory passed with `--config-dir`;
2. the absolute path in `FUT_CONFIG`;
3. `$XDG_CONFIG_HOME/fut/config.toml` when `XDG_CONFIG_HOME` is absolute;
4. `~/.config/fut/config.toml`.

A missing implicit file, including one selected through `--config-dir`, uses defaults and is not created. `--config-dir` and `FUT_CONFIG` must be absolute; `FUT_CONFIG` must exist. Configuration is loaded before an interactive client changes terminal state. Press `Ctrl-b Shift-R` to reload the invoking client's configuration; a valid configuration applies bindings and layout immediately, while any location, read, parse, or validation error leaves the complete previous configuration active and appears as a one-line notice. Control commands and shell completion do not load UI configuration; `fut doctor` reads it without creating runtime state.

Files must be regular UTF-8 files no larger than 64 KiB. Unknown fields, invalid values, unsafe control or bidirectional-formatting characters, ambiguous segments, and out-of-scope tokens are errors.

Closing a pane, tab, or workspace asks for confirmation by default. Set `ui.confirm_close = false` to perform those close actions immediately. This setting is client-local and applies to both keyboard commands and contextual menus.

## Example

This example is intentionally customized; it is not a dump of the defaults.
Omit any field to keep its default value.

```toml
extensions = [
  "/Users/me/.config/fut/extensions/review-status",
  "/Users/me/.config/fut/extensions/run",
]

[ui]
pane_layout = "splits" # "splits" or "accordion"
confirm_close = true   # Require confirmation before closing panes, tabs, or workspaces.

[ui.bindings]
open_command_bar = "space"
"run:restart" = "r"
# reload_config = "R"
# enter_copy_mode = "["
# open_navigator = "s"
# open_agents = "a"
# open_left_sidebar = "w"
# open_right_sidebar = "]"
# open_tab_bar = "t"
# open_notifications = "u"
# focus_next_notification = "prefix"
# create_workspace = "C"
# create_tab = "c"
# focus_last_tab = "ctrl-t"
# focus_last_workspace = "ctrl-w"
# focus_last_session = "ctrl-s"
# close_pane = "x"

[trusted_commands.git_diff]
title = "Repository diff"
binding = "g"
program = "/Users/me/.dotfiles/tmux/tmux.symlink/git_diff_popup.sh"
# args = ["--optional-argument"]
size = { width = 120, height = 40 }

[ui.icons]
preset = "nerd_font" # "ascii", "unicode", or "nerd_font"
# current = "*"      # Every icon may be overridden.
# closing = "x"
# overflow = "..."
# workspace = "W"
# tab = "T"
# zoom = "zoom"
# vertical_divider = "|"
# pill_left = ""       # Focused tab/workspace pill caps; empty outside "nerd_font".
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
  { segments = [{ token = "workspace.extension.run.pause", style = "divider", inverted = true, pill = true }], priority = 220 },
  { segments = [{ token = "workspace.extension.run.launching", style = "attention", inverted = true, pill = true }], priority = 220 },
  { segments = [{ token = "workspace.extension.run.play", style = "added", inverted = true, pill = true }], priority = 220 },
  { segments = [{ token = "workspace.extension.run.stop", style = "divider", inverted = true, pill = true }], priority = 220 },
  { segments = [{ token = "workspace.extension.run.cross", style = "error", inverted = true, pill = true }], priority = 220 },
  { segments = [{ token = "client.zoom", suffix = " " }], priority = 255 },
  { segments = [{ token = "client.help" }], style = "muted", priority = 0 },
]

[ui.tab_bar.item]
segments = [
  { text = " " },
  { token = "tab.index" },
  { token = "tab.name", prefix = " " },
  { token = "tab.closing", prefix = " " },
  { token = "tab.activity", prefix = " " },
  { text = " " },
]

[ui.sidebar.left]
width = 28
display = "expanded" # "expanded" or "minimized"
visibility = "automatic" # "visible", "automatic", or "hidden"
components = [
  { component = "workspaces", size = "fill", header = [{ token = "session.name", style = "current" }], footer = [{ token = "sidebar.status", style = "muted" }], row = { left = [{ text = " " }], body = [{ token = "workspace.index" }, { token = "workspace.name", prefix = " " }], right = [{ token = "workspace.tab_count" }, { token = "workspace.closing", prefix = " " }, { text = " " }], detail = [{ text = "    " }, { token = "workspace.git_branch", style = "muted" }, { token = "workspace.git_added", prefix = " " }, { token = "workspace.git_deleted", prefix = " " }] } },
]

[ui.sidebar.right]
width = 28
display = "expanded"
visibility = "automatic"
components = [
  { component = "agents", size = "fill", scope = "session" },
]
```

An explicitly empty array hides that lane or format.

## Local extensions

`extensions` is an explicit list of trusted absolute directory paths, loaded in
order. Fut appends packages enabled through its Fut-owned local managed store;
it never rewrites this array or any other part of `config.toml`. The complete
merged set is accepted or rejected atomically, and references to extension
tokens are validated with it. Fut does not discover or download extensions.
See [Extensions](extensions.md) for managed install commands, trust boundaries,
hooks, tokens, limits, payloads, and examples. Third-party authors should use
the [Extension authoring guide](extension-authoring.md) as the API contract.

The global file, a trusted project recipe, and each workspace's
`.fut/config.toml` may configure a loaded extension with the same namespaced
table. Values layer global → trusted project → workspace:

```toml
[extension.run]
command = ["just", "run"]
signal = "STATUS:READY" # optional literal readiness marker
```

Workspace values recursively override trusted project and global defaults for
explicit commands invoked there. Unknown extension IDs and oversized data are
errors; the extension owns validation of its inner keys and values. Fut gives
extensions the trusted global/project layers separately; the bundled automatic
behaviors use only those values. Workspace-local values remain inert in the
bundled extensions until an explicit command is invoked. See the bundled `run`
extension for optional trusted `auto_start` and manual
`run:restart`, and the bundled `wt` extension for optional existing-worktree
discovery.

## Bindings

Bindings are unique suffixes after the fixed `Ctrl-b` prefix. Set a built-in
action or quoted extension command slug under `[ui.bindings]`; see the
[complete defaults table](usage.md#everyday-controls) for action names.

A suffix may be one printable character or `prefix`, `ctrl-s`, `ctrl-t`,
`ctrl-w`, `space`, `enter`, `tab`, `esc`, `up`, or `down`. `prefix` means
pressing `Ctrl-b` again. Pause after the prefix to see the effective bindings,
or press `Ctrl-b :` to search them in the command palette.

An `[extension_commands."EXTENSION:COMMAND"]` table can set `args` to replace
the arguments supplied after that extension command's manifest executable.
The arguments are direct strings with no shell interpolation. An empty array
passes no arguments, and unknown qualified command slugs are rejected.

## Trusted commands

Each `[trusted_commands.NAME]` table requires `title` and an executable `program`, plus optional `binding`, string array `args`, `size`, and `activate_opened` values. `size` and `activate_opened` have the same semantics as an extension command; omitting size preserves the full-terminal surface, while activation defaults to false. Running a command opens a dashed frame containing a temporary PTY, inherits the focused pane process's live working directory, and sends normal terminal input to the command. The frame names the command and identifies the temporary surface; when the process exits, Fut restores the previous panes, focus, and geometry. A bound trusted command may take a built-in's default key, which unbinds that built-in unless it is explicitly rebound under `ui.bindings`. Explicit binding collisions and duplicate command keys are rejected. Commands appear in the command palette; bound commands also appear in delayed which-key help. Configuration reload replaces them atomically.

## Sidebars

Agents is a read-only projection of terminals with an explicit agent integration. Its `scope` is `tab`, `workspace`, `session`, or `global`. Tab, workspace, and session filters use fresh live focus ancestry when available and otherwise fall back to the selected IDs; global needs no focus anchor. Rows under any closing session, workspace, tab, or pane are omitted; screen detection alone does not add a row. Rows show idle, working, blocked, and daemon-wide unread-completed activity plus session/workspace/tab context. Enter navigates with the row's typed pane ID; `global` safely permits cross-session destinations. The separate Notifications dialog remains the unread-attention surface.

Each sidebar width is independently 4 through 80 cells and includes its one-cell inner divider. Each tagged built-in entry uses `component = "workspaces"` or `component = "agents"` and has either `size = "fill"` or a positive fixed row count; each side may contain at most one `fill` and at most one Workspaces component. Left-drag either visible divider to resize only that side. A docked drag preserves at least 40 terminal columns after accounting for the other docked side; an open drawer's divider is also draggable, while a hidden drawer is not. Dragged widths belong only to that attached client: they are not written to configuration, and both configured widths return on reattach or configuration reload. The active workspace is marked with a bullet.

Display, visibility, components, and relevance are independent per side. `display = "expanded"` uses that side's configured width, while `display = "minimized"` uses a fixed six-cell rail. Minimized Workspaces and Agents rows keep stable numeric positions plus current/activity markers. Opening a minimized sidebar temporarily expands its drawer to the configured width; the rail itself is not draggable, but its open drawer remains resizable. Press `m` inside a component to toggle its side. `visibility = "visible"` docks when geometry permits. `visibility = "automatic"` docks when that side has a relevant configured component: Workspaces with multiple live workspaces, or Agents with an integrated terminal in their configured scope. `visibility = "hidden"` leaves the side as an on-demand drawer. Press `h` to cycle that side through visible → automatic → hidden. With the default Workspaces footer, an open sidebar shows `h`, `m`, and `?` on separate lines with their current states; the `nerd_font` preset adds matching visibility, width, and help icons.

The defaults are left Automatic with one Workspaces `fill`, and right Automatic with one session-scoped Agents `fill`; an empty Agents projection therefore consumes no right-side width. Both sides dock when their combined widths leave 40 terminal columns. If only one can fit, allocation is deterministic: left is considered before right. Any hidden, irrelevant, or non-fitting side remains available as its own edge drawer without reducing terminal geometry. The current side's display and visibility appear in the default footer. `sidebar.display` and `sidebar.visibility` expose those labels to custom sidebar chrome. `vertical_divider` must be exactly one grapheme and one display cell.

## Segments, groups, and components

Every segment sets exactly one of:

- `text` — a literal string;
- `token` — a pure value from Fut's already-materialized client/resource state;
- `component` — a layout-aware repeated collection. The only current component is `tabs`.

Token segments may also set `prefix`, `suffix`, `max_width`, `style`, `inverted = true`, and `pill = true`. Prefix and suffix are emitted only when the token is nonempty. Inversion uses terminal reverse-video after normal style composition: the semantic foreground becomes the fill and the glyph uses the underlying terminal background, so it adapts to light and dark themes. A pill requires inversion and adds the configured `pill_left` and `pill_right` glyphs in that fill color around the complete segment, including its affixes. If either cap is empty, as in the Unicode and ASCII presets, Fut renders only the inverted content. Empty tokens emit neither affixes nor caps. Text segments accept `style` but not token-only options. Components must be the only segment in their group and do not accept segment options.

Tab-item content uses the intrinsic display-cell width of its configured segments. The default format supplies one cell of padding on each side; add or remove text segments to adjust that spacing. The Nerd Font preset additionally reserves one cell at each end of every item so its active-tab pill does not move neighboring tabs. New tabs are created unnamed, so their title follows the foreground process in the focused pane. Name a tab with `fut tab new --name` or `Ctrl-b r` to keep that title fixed; renaming it to an empty string restores automatic naming.

Tab-bar lanes contain groups. A group has `segments`, an optional semantic `style`, and a `priority` from 0 through 255. The tabs component is flexible and keeps its active or keyboard-selected item visible. Groups with higher priority than tabs reserve their complete width first. Tabs then grow toward their complete preferred width. Lower-priority groups appear only in remaining space. Left and right lanes stay edge-aligned; the center lane is geometrically centered and clamped between them. Groups never overlap. By default the right lane names the current workspace, truncated at 20 cells.

Workspace rows have intrinsic `left` and `right` lanes; `body` receives the remaining cells and truncates safely. Expanded default rows show the workspace number and name, reserve leading and trailing padding, and use the same current style as the focused tab instead of a separate active marker. With Nerd Font pill caps, only that title line becomes a pill. The minimized rail keeps its compact active-workspace bullet. A nonempty `detail` format adds a second full-width line. Workspaces are unnamed unless explicitly named, presenting as their live location — the work tree (or directory) every open pane is inside, or `multiple` when panes disagree. The default detail aligns under the row name and shows the Git branch at that live location with its short working-tree diff (`+N` inserted, `-N` deleted). The daemon collects those values with bounded background `git` processes and publishes them atomically into the shared resource snapshot, so rendering never waits, attached clients agree, and non-Git locations simply stay empty. Set `detail = []` for compact one-line rows. Header and footer are optional single-line segment lists. At tiny heights Fut preserves resource rows over decorative header/footer content, while switching and error status remains visible.

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

Styles compose in this order: normal, group style, token style (`activity`, `attention`, `added`, or `deleted` when supplied), segment style, current, attention, closing, selected. Later foreground/background values replace earlier ones; modifiers are added or removed in sequence. An inverted token adds reverse-video after this composition without discarding the composed colors or modifiers.

## Icons

`unicode` is the default and requires no private-use glyphs. `ascii` uses ASCII for the configurable resource icons; ordinary built-in help and truncation text may still use Unicode. `nerd_font` opts into a small Nerd Fonts v3-oriented resource/state set, and additionally uses its Powerline half-circle caps for focused tabs, expanded workspaces, and token segments configured with `pill = true`. Every icon can be overridden under `[ui.icons]`.

Fut cannot reliably detect the active terminal font. Use [`fut doctor`](doctor.md) for an honest visual probe; it never claims that an installed font is active.

## Security boundary

Everything under `ui` remains non-executable: it has no shell commands, file reads, environment interpolation, networking, functions, or expression language, and presentation tokens resolve only already-materialized strings. `trusted_commands` is a deliberate executable trust boundary. Fut executes its `program` directly with the configured `args` and the client's environment; it does not invoke a shell unless you explicitly configure one. Explicit extension roots, enabled managed extensions, and namespaced project tables are the equivalent trust decision for extension manifests, their packaged commands, and values those commands may execute. Managed installation only copies a local package or fetches one explicitly pinned Git commit before validating it; it runs no package scripts, and enablement is the explicit trust decision. An explicit project `recipe` path is also a global trust decision; a repository `.fut/project.toml` becomes executable only after `fut project trust NAME` approves that canonical file's exact current bytes. Only configure commands, extension directories, and recipes you trust. Commands never run during configuration parsing or rendering; resource hooks run only from committed daemon mutations, client hooks run only for attachment lifecycle transitions, and recipes run only while creating a new session or workspace. Trusted recipe extension settings may deliberately make those lifecycle hooks perform work such as opening existing worktrees or starting a managed command. The same-user Unix socket is the authorization boundary for token publication, just as it is for other Fut control commands; Fut does not claim to authenticate the publishing process beyond that boundary.

## Related

- [Projects](projects.md)
- [Presentation tokens](tokens.md)
- [Diagnostics](doctor.md)
