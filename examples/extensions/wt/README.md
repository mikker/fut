# wt

Create a Git worktree, switch Fut to its new peer workspace, and launch an
agent.

The extension requires [`wt`](https://github.com/mikker/wt) and `pi` on
`PATH`. Set `FUT_WT_BIN` to use another `wt` executable.

Add the extension's absolute directory to `~/.config/fut/config.toml`:

```toml
extensions = [
  "/absolute/path/to/fut/examples/extensions/wt",
]
```

Reload Fut's configuration, open the command palette with `Ctrl-b :`, and
choose **New worktree**, then enter the worktree name. The launcher runs in the
focused pane's working directory, so the current Git repository determines
where `wt` creates the worktree. The agent opens without an initial prompt.

The arguments after `./bin/create` in `fut-extension.toml` configure the
post-open action as a direct argv array. For example, launch Codex instead:

```toml
[commands.new-agent-worktree]
title = "New worktree"
argv = ["./bin/create", "codex"]
size = { width = 64, height = 10 }
activate_opened = true
```

Arguments are preserved, so an action can also be configured as
`["./bin/create", "my-agent", "--flag"]`. Running `bin/create` directly
without an action defaults to `pi`.

The package contains two direct executables:

- `bin/create` composes `wt create`, `fut open`, and the configured action.
- `bin/worktree-event` is an adapter for a versioned `worktree.removed` event;
  it calls `fut workspace retire` using the agent terminal's validated caller
  context. Current `wt` releases do not invoke this adapter automatically.
