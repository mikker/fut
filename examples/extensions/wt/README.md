# wt

Create a Git worktree, switch Fut to its new peer workspace, and launch an
optional configured command. Without one, the new workspace opens a shell.

The extension requires [`wt`](https://github.com/mikker/wt) on `PATH`. Any
configured post-open command must also be available on `PATH`. Set `FUT_WT_BIN`
to use another `wt` executable.

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

Configure the optional post-open action in your Fut config as a direct argument
array. For example, launch Pi and bind the command to `Ctrl-b N`:

```toml
[ui.bindings]
"wt:new-worktree" = "N"

[extension_commands."wt:new-worktree"]
args = ["pi"]
```

Arguments are preserved, so an action can also be configured as
`["my-agent", "--flag"]`. Omit the `extension_commands` entry (or configure
`args = []`) to open the default shell.

The package contains two direct executables:

- `bin/create` composes `wt create`, `fut open`, and the configured action.
- `bin/worktree-event` is an adapter for a versioned `worktree.removed` event;
  it calls `fut workspace retire` using the agent terminal's validated caller
  context. Current `wt` releases do not invoke this adapter automatically.
