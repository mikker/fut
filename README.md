# fut

`fut` is a modern, agent-aware terminal multiplexer organized around projects and their worktrees.

The project is currently in its design and first-spike phase:

- [VISION.md](VISION.md) describes the desired end state and architectural direction.
- [CONTEXT.md](CONTEXT.md) defines the project's shared language.
- [PLAN.md](PLAN.md) lays out the implementation sequence and exit criteria.

## Current vertical slice

The current Rust implementation proves one daemon-owned resource tree with multiple live project sessions. Git project identity is the canonical Git common directory, so explicitly opened linked worktrees join the existing session as peer workspaces. Non-Git directories use canonical directory identity. Workspaces can contain multiple tabs, with one pane and terminal per created tab. Terminals retain semantic `libghostty-vt` snapshots and survive detach/reattach.

```sh
mise install
mise run check

cargo run                 # open the current checkout, then attach its returned terminal
cargo run -- new api --cwd ../api -- /bin/zsh
cargo run -- new-tab workspace:<uuid> --name server --cwd services/api -- ./bin/server
cargo run -- list
cargo run -- rename tab:<uuid> "api server"
cargo run -- attach api   # exact session name; typed workspace:/tab:/pane:/terminal: IDs also work
cargo run -- ping
cargo run -- close workspace:<uuid> # close only that workspace and its descendants
cargo run -- shutdown     # stop the daemon
```

Inside the client, `Ctrl-b c` creates and switches to a default shell tab, `Ctrl-b g` opens the global navigator, `Ctrl-b d` detaches, and `Ctrl-b Ctrl-b` sends a literal `Ctrl-b`.

The testing layers can also be run independently:

```sh
mise run test:unit
mise run test:e2e
```

Bare `fut` and `fut attach` open the current checkout and attach the specific terminal returned by the daemon. `fut attach TARGET` is attach-only. `new` requires an already-running daemon. Its name overrides the session name for a new project or the workspace name for a new worktree; an existing workspace ignores it. Implicit resolver names are deterministically suffixed (`name-2`, `name-3`, …) on collision, while explicit duplicate names are errors. Worktrees are discovered only when explicitly opened; Fut does not eagerly enumerate them.

`fut new-tab WORKSPACE [--name NAME] [--cwd PATH] [COMMAND...]` creates a tab in an existing workspace. Its working directory defaults to the workspace root, and a relative `--cwd` is resolved from that root. Commands are passed as arguments without shell evaluation.

Session selectors accept `session:<uuid-or-exact-name>`, `id:<uuid>`, `name:<exact>`, bare UUIDs, and bare exact names. Names containing colons require `name:<exact>`. Other resources use explicit typed UUID selectors. A normal session/workspace selector must still resolve to exactly one open terminal.

`fut rename TARGET NAME` renames sessions, workspaces, and tabs only. Session targets accept the session selector forms above; workspace and tab targets require `workspace:<uuid>` and `tab:<uuid>`. Names are stored exactly as supplied and must be unique in their scope: sessions globally, workspaces within a session, and tabs within a workspace. Renaming to the exact current name is a no-op. The minimal in-client global navigator switches directly between live panes and shows renamed resources when opened after the rename; it does not provide inline editing.

Pane naming and move, split, and layout operations are not implemented yet. Fuzzy matching, project recipes, and agent activity are also not implemented.
