# fut

`fut` is a modern, agent-aware terminal multiplexer organized around projects and their worktrees.

The project is currently in its design and first-spike phase:

- [VISION.md](VISION.md) describes the desired end state and architectural direction.
- [CONTEXT.md](CONTEXT.md) defines the project's shared language.
- [PLAN.md](PLAN.md) lays out the implementation sequence and exit criteria.

## Current vertical slice

The current Rust implementation proves one daemon-owned resource tree with multiple live project sessions. Each session currently has one workspace, one tab, one pane, and one terminal. Terminals retain semantic `libghostty-vt` snapshots and survive detach/reattach.

```sh
mise install
mise run check

cargo run                 # start or find the daemon, then attach (unambiguous session only)
cargo run -- new api --cwd ../api -- /bin/zsh
cargo run -- list
cargo run -- attach api   # exact name; also id:<uuid>, name:<exact>, or bare UUID
cargo run -- ping
cargo run -- close api    # close and reap the complete session
cargo run -- shutdown     # stop the daemon
```

Inside the client, `Ctrl-b d` detaches and `Ctrl-b Ctrl-b` sends a literal `Ctrl-b`.

The testing layers can also be run independently:

```sh
mise run test:unit
mise run test:e2e
```

`new` requires an already-running daemon in this slice. Project identity is the canonical working directory; Git common-directory/worktree discovery is next. Session selectors accept `id:<uuid>` and `name:<exact>`; bare UUIDs select IDs and other bare values select exact names. There is no in-client retargeting, navigator, direct switching, project recipe, or agent activity yet.
