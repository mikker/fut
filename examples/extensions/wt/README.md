# wt

Create a Git worktree and switch Fut to its new peer workspace. Configured
projects apply their trusted workspace recipe there just as they do for the
main checkout.

The extension requires [`wt`](https://github.com/mikker/wt) on `PATH`. Set
`FUT_WT_BIN` to use another `wt` executable.

The discovery and lifecycle adapters also require `python3` on `PATH`; they use
only Python's standard library.

Add the extension's absolute directory to `~/.config/fut/config.toml`:

```toml
extensions = [
  "/absolute/path/to/fut/examples/extensions/wt",
]
```

Reload Fut's configuration, open the command palette with `Ctrl-b :`, and
choose **New worktree**, then enter the worktree name. The launcher runs in the
focused pane's working directory, so the current Git repository determines
where `wt` creates the worktree.

The native form accepts an optional worktree name, command, and initial prompt.
An empty worktree name gets a random `wt-xxxxxx` name. Configure the command
shown by default in a trusted project recipe:

```toml
[extension.wt]
command = ["pi"]
```

The form displays this as `$ pi`. Its `$ ` prefix is cosmetic. Fut starts the
command directly in the new workspace, and passes a nonempty prompt as one
final argument. Clearing the command leaves process selection to the project
recipe.

Bind the command to `Ctrl-b N` if desired:

```toml
[ui.bindings]
"wt:new-worktree" = "N"
```

To open every existing worktree when a project session is first created, put
this trusted setting in that project's `.fut/project.toml`:

```toml
[extension.wt]
open_existing = true
```

The session hook enumerates Git's existing worktrees and opens each through
Fut's normal idempotent project operation. It never creates or deletes one.
The setting is disabled by default and is honored only from trusted global or
project-recipe configuration, never from a workspace `.fut/config.toml`.

The package contains three direct executables:

- `bin/create` composes `wt create` and `fut open --background`, optionally
  overriding the recipe's focused process with the submitted command.
- `bin/open-existing` handles trusted `session.created` discovery.
- `bin/worktree-event` is an adapter for a versioned `worktree.removed` event;
  it calls `fut workspace retire` using the agent terminal's validated caller
  context. Commands launched by **New worktree** install this adapter so a
  successful `wt done` or `wt ship` retires the removed worktree's workspace.
