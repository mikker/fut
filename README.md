# fut

`fut` is a modern, agent-aware terminal multiplexer organized around projects and their worktrees.

The project is currently in its design and first-spike phase:

- [VISION.md](VISION.md) describes the desired end state and architectural direction.
- [CONTEXT.md](CONTEXT.md) defines the project's shared language.
- [PLAN.md](PLAN.md) lays out the implementation sequence and exit criteria.

## Initial spike

The current Rust spike proves one daemon-owned PTY, semantic terminal snapshots through `libghostty-vt`, a local Ratatui client, and detach/reattach without losing the child process.

```sh
mise install
mise run check

cargo run                 # start or find the daemon, then attach
cargo run -- ping
cargo run -- close        # close the current terminal and, when empty, Fut
cargo run -- shutdown     # stop the daemon
```

Inside the client, `Ctrl-b d` detaches and `Ctrl-b Ctrl-b` sends a literal `Ctrl-b`.

The testing layers can also be run independently:

```sh
mise run test:unit
mise run test:e2e
```

The spike intentionally owns only one terminal. Sessions, workspaces, tabs, panes, project recipes, and agent activity begin in later milestones.
