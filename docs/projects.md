---
layout: default
title: Projects
description: Configure Fut's project catalog and trusted bootstrap recipes.
permalink: /projects/
---

# Projects

Fut can keep an explicit project catalog and bootstrap new project sessions
from trusted recipes.

## Project catalog

Declare projects in the global [configuration](configuration.md):

```toml
projects_dir = "~/dev"

[projects.fut]
path = "~/dev/fut"

[projects.website]
path = "/Users/me/dev/website"
recipe = "~/.config/fut/recipes/website.toml"
```

Project names use ASCII letters, numbers, `-`, or `_` and must be unique. Paths
must be absolute or begin with `~/`; Fut performs no filesystem scanning or
shell interpolation. Open and attach to a catalog entry with `fut project fut`
(or its short alias, `fut p fut`).
`fut open -p fut` (or `--project fut`) still works; add `-b` to that form to
open it in the background.
When `projects_dir` is set, a project name not in the explicit catalog falls
back to a directory with that name below it, so `fut project website` opens
`~/dev/website` in this example. Explicit catalog entries take precedence.
An optional path may select a linked checkout, for example
`fut open ../fut-feature --project fut`; Fut verifies that the path has the same
Git project identity as the configured root before opening it as a peer
workspace.

From an attached client, press `Ctrl-b Shift-S` to fuzzy-filter the configured
catalog or type any path. Suggestions come only from configuration—Fut does not
scan the filesystem. The exact typed value appears as an **Open path** choice,
resolved relative to the focused workspace. Opening a location that is already
live navigates to it rather than creating a duplicate. A newly bootstrapped
project attaches directly to the focus terminal selected by its recipe.
Path and recipe preparation runs in the background. The opener shows
**preparing…** while it resolves the project, remains current as other client
updates arrive, and can be cancelled with Escape or Ctrl-C. Once preparation
has sent the open request, the existing **opening…** phase remains non-cancellable.

List the configured catalog without starting a daemon:

```sh
fut project list
fut project ls
```

Fut rereads the project catalog for every project open, so changing a project
path, `recipe`, or `projects_dir` does not require a daemon restart. Repository
recipe approvals are checked separately each time Fut boots or reloads a
project session, so approving or revoking one does not require a daemon restart.

## Trusted recipes

Any project may initialize the declared workspaces of a newly created project
session from a local recipe; it does not need to appear in the global catalog.
Once the project session is live, opening a linked checkout or creating another
workspace starts one ordinary terminal; it does not recreate the recipe's
workspaces, tabs, panes, or commands. Reopening a live workspace never rereads
or reconciles its recipe and never reruns its commands. A linked Git worktree
uses the recipe configured for the catalog root when it is the checkout that
initially boots the project session, not a file from the linked checkout.

By default Fut looks for `.fut/project.toml` under the project's root. For a
catalog entry, this is the configured catalog root. For an unconfigured path,
it is the resolved Git worktree or directory root. Because that
repository-owned file can run commands, Fut presents it for review when the
project is opened and loads it only after you approve its exact contents. A
catalog entry can also be approved explicitly with:

```sh
fut project init
fut trust
fut trust path/to/project
```

The `workspaces` list is optional. When it is omitted or empty, Fut starts with
one ordinary workspace, tab, and shell pane while still applying project-level
environment and extension configuration.

The in-client project opener performs the same approval safely without a shell:
it displays the recipe contents, asks for `y` or `n`, and records approval
through Fut's machine-local trust store. It never asks you to copy a hash or
edit trust state. If the recipe changes while it is displayed, approval fails
and the opener asks you to review the new contents again.

`fut project init` creates a small starter recipe in the current directory with
the published schema, an agent tab, a Vim tab, and a link back to this page. It
refuses to overwrite an existing `.fut/project.toml`.

This command works without a running daemon. It resolves the configured
project, reads a canonical regular recipe file, and fully validates the recipe
before recording machine-local approval for its exact bytes. Changing any byte
makes the recipe untrusted again. Run `fut untrust [PATH]` to revoke approval.
Both commands default to the current project. The next attempt to bootstrap or
reload the project session will fail before running recipe commands. Existing
live layouts and processes are not reconciled on reload; only the extension
configuration captured by the session is replaced.

Use `fut trust status [PATH]` to inspect the current recipe without changing
trust. It exits 0 when trusted, 1 when untrusted or absent, and 2 when status
cannot be determined, making it suitable for shell conditionals. Add `--json`
for structured `trusted`, `recipe`, and `sha256` fields.

Approval state is managed exclusively by Fut under
`$XDG_STATE_HOME/fut/trusted-recipes.toml`, falling back to
`~/.local/state/fut/trusted-recipes.toml`. Do not edit this file. A malformed or
unsafe state file causes repository recipes to fail closed.

Alternatively, set `recipe` to an absolute or `~/` path in global config. That
explicit path is inherently trusted; remove the global `recipe` setting to
revoke that trust. Recipe files must be regular UTF-8 files no larger than 64
KiB.

## Recipe format

```toml
#:schema https://fut.sh/schemas/project.json

focus = "main.code.agent"
environment = { RUST_BACKTRACE = "1" }

[extension.wt]
open_existing = true
command = ["pi"]

[extension.run]
command = ["mise", "run", "dev"]
auto_start = true

[[workspaces]]
id = "main"

[[workspaces.tabs]]
id = "code"
title = "Code"
cwd = "."
environment = { PROJECT_ROLE = "development" }
panes = [
  { id = "editor", command = ["nvim", "."] },
  { id = "agent", command = ["pi", "--model", "fast"], exec = true, environment = { AGENT_ROLE = "reviewer" }, split = { target = "editor", direction = "right" } },
]

[[workspaces.tabs]]
title = "project console"
panes = [
  { },
]
```

## Editor completion

Fut publishes a [JSON Schema](/schemas/project.json) for project recipes. Add
this Taplo schema directive as the first line of `.fut/project.toml` to get
validation, documentation, and completion in compatible TOML editors:

```toml
#:schema https://fut.sh/schemas/project.json
```

The schema catches structural errors such as missing pane splits. Fut remains
the authority for checks that JSON Schema cannot express, including unique IDs,
valid focus targets, split targets that name earlier panes, installed extension
configuration, and the total pane limit.

Workspace, tab, and pane IDs are optional recipe-local references. Add an ID
only when the item must be referenced by `focus = "WORKSPACE.TAB.PANE"` or a
pane's `split.target`. IDs use ASCII letters, numbers, `-`, or `_` and never
affect presentation. Set `title` on a workspace or tab when it needs a fixed
display name; without one it follows Fut's normal automatic naming. The first
pane in a tab has no `split`. Every later pane must split an earlier, named pane
in the same tab with direction `right` or `down`.

Commands are direct argument arrays and are never evaluated by a shell. By
default, Fut runs a configured command as a child process and starts the user's
shell when it exits, so stopping the command leaves the pane open at a prompt.
Set `exec = true` on a pane to make its configured command the pane's top-level
process instead. Omit `command` to start the configured shell immediately.

Relative `cwd` values are resolved from the workspace checkout root; pane `cwd`
overrides tab `cwd`, and a tab `cwd` applies to panes that omit one. Environment
values layer from recipe to tab to pane, with the most specific value winning.
`FUT_*` variables are reserved for Fut and cannot be set by a recipe.

Namespaced `[extension.ID]` tables are also part of the exact trusted recipe.
They layer over global extension defaults and are captured when the project
session is created, so the main checkout, linked worktrees, and logical peer
workspaces share one approved project configuration. The bundled `wt`
extension can open existing worktrees with `open_existing = true`; the bundled
`run` extension can start its managed command once in the first declared
workspace when the project session starts with `auto_start = true`. It does not
repeat that command in other declared or subsequently created workspaces. Both
extensions are disabled by default.

The `wt` extension's optional `command` array supplies the default command in
its **New worktree** form. For example, `["pi"]` launches Pi in the new
worktree and allows a one-off initial prompt. Clear the form's command field to
leave process selection to the project recipe.

`fut open -- COMMAND...` still takes priority for the recipe-selected focus
pane when it bootstraps a project session; the rest of the recipe is unchanged.
When it adds a workspace to a live session, the explicit command instead runs
in that workspace's single initial terminal. An explicit open command remains
the pane's top-level process. Recipe topology and working directories are fully
validated before processes start. Fut starts every declared terminal before
publishing the initial resources and closes any terminals already started if a
later spawn fails.

## Related

- [Configuration](configuration.md)
- [Extensions](extensions.md)
