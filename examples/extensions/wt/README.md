# wt

Create a Git worktree, open it as a peer Fut workspace, and launch an agent
with an initial prompt. When the agent successfully runs `wt done`, `wt ship`,
or `wt rm`, the worktree removal event gracefully retires its Fut workspace.

The extension requires [`wt`](https://github.com/mikker/wt) and `pi` on
`PATH`. Set `FUT_WT_BIN` to use another `wt` executable or `FUT_WT_AGENT` to
use another agent executable that accepts its initial prompt as one argument.

Add the extension's absolute directory to `~/.config/fut/config.toml`:

```toml
extensions = [
  "/absolute/path/to/fut/examples/extensions/wt",
]
```

Reload Fut's configuration, open the command palette with `Ctrl-b :`, and
choose **New agent worktree**. Enter the worktree name and agent prompt. The
launcher runs in the focused pane's working directory, so the current Git
repository determines where `wt` creates the worktree.

The package contains two direct executables:

- `bin/create` composes `wt create`, `fut open`, and the selected agent.
- `bin/worktree-event` accepts `wt`'s versioned removal event and calls
  `fut workspace retire` using the agent terminal's validated caller context.
