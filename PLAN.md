# Fut implementation plan

## Current status

The Milestone 0–1 spike and the current daemon-integrated Milestone 2 slice are implemented. They establish:

- a Rust `fut` executable and pinned Rust/Zig toolchain;
- one securely owned per-user daemon socket;
- a pure ResourceTree used by the daemon, separate from its terminal runtime registry;
- several project sessions in one daemon, with explicitly opened linked Git worktrees as peer workspaces;
- stable terminal identities and per-terminal attachment leases;
- a narrow `libghostty-vt` adapter producing Fut-owned semantic snapshots;
- a detachable Ratatui client with local view state and a global pane navigator;
- end-to-end tab creation, allowing multiple tabs in each workspace;
- durable terminal lifecycle and confirmed child cleanup;
- separate unit and process-level end-to-end test layers.

This validates tab creation end-to-end but does not complete Milestone 2. Each created tab still contains exactly one pane and terminal; visible splits and other rename, move, and layout mutations are deferred. Input encoding remains a basic client-side mapping and snapshots are full-grid JSON messages. Git checkouts use their canonical common directory as project identity and their top-level checkout as workspace root; non-Git directories use canonical directory identity. Worktrees are not eagerly discovered. Opening a linked checkout adds one peer workspace, while reopening it is side-effect free. Interactive connections can retarget through the global navigator. A workspace disappears when its last terminal exits, a session when its last workspace exits, and only the final session closes Fut and releases the socket.

## Strategy

Build Fut as a sequence of narrow vertical slices. The first slice must prove process ownership, terminal correctness, detach/reattach, and the resource model before investing in visual polish or broad integrations.

Each milestone should leave a runnable `fut` binary. New abstractions are earned by the next milestone rather than designed for every possible client or platform in advance.

## Guardrails

These constraints protect the foundational model during implementation:

- Never create one daemon or socket namespace per session.
- Never put the globally active session, workspace, tab, or pane in shared daemon state.
- Keep pane placement identity separate from terminal/process identity.
- Keep Ratatui and client presentation types out of the shared domain model.
- Keep all `libghostty-vt` calls behind one Fut-owned adapter.
- Treat project recipes as creation input, not continuously reconciled state.
- Do not add cold restoration that implies dead processes are still alive.
- Do not model agents as a second resource hierarchy.
- Prefer a small control protocol over an in-process plugin system.
- Start macOS-first without baking macOS UI concepts into the core.

## Milestone 0: Scaffold and executable

Create the smallest Rust project capable of growing along the intended boundaries.

### Work

- Initialize a Cargo workspace and a `fut` executable.
- Establish modules or crates for the domain model, daemon, protocol, terminal runtime, and TUI client without prematurely splitting every module into a package.
- Add formatting, linting, unit-test, and integration-test commands.
- Choose opaque resource ID types and typed resource paths.
- Record hard-to-reverse decisions as ADRs only when implementation forces a real trade-off.

### Exit criteria

- `fut --help` runs.
- Formatting, linting, and tests run with one documented command.
- Domain types do not depend on terminal rendering or transport libraries.

## Milestone 1: One detachable terminal

Prove the lowest useful vertical slice: one daemon owns one interactive terminal and one client renders it.

### Work

- Auto-start or discover one user-level daemon.
- Add a versioned local socket handshake.
- Spawn one shell in a PTY.
- Read PTY output without blocking input or resize handling.
- Feed output into a pinned `libghostty-vt` adapter.
- Render terminal cells in the Ratatui client.
- Encode keyboard, paste, focus, and mouse input back to the PTY.
- Propagate client resize to the active terminal.
- Detach and reattach without terminating the shell.
- Shut down Fut cleanly when its sole terminal exits or is explicitly closed.

### Tests

- PTY lifecycle and child-exit integration tests.
- VT fixtures covering color, Unicode/graphemes, cursor movement, alternate screen, and reflow.
- Protocol version mismatch and reconnect tests.
- A smoke test proving a shell process survives client detachment.

### Exit criteria

- A normal interactive shell is usable through `fut`.
- Editors and full-screen programs render well enough for continued development inside Fut.
- The process remains alive with no client attached and is visible again after reattachment.
- Exiting or closing the sole terminal exits the empty multiplexer and releases its socket.

## Milestone 2: Resource tree and lifecycle

Introduce the real product model before reproducing more tmux behavior.

### Work

- Add Project, Session, Workspace, Tab, Pane, and Terminal domain types.
- Resolve Git project identity through the repository's common directory; use canonical directory identity for non-Git projects.
- Model the main checkout and linked worktrees as peer workspaces.
- Keep terminal identity stable while panes move within the tree.
- Implement create, close, rename, focus, move, split, and switch operations.
- Implement upward cascade closure from pane to session, and close Fut when the last session closes.
- Store client focus and navigation state per connection.
- Define resource snapshots and typed incremental events.
- Provide a JSON control API alongside the interactive protocol.
- Add the global navigator over the complete resource tree.

### Tests

- Pure state-machine tests for every mutation and invariant.
- Property tests for cascade closure and unique/stable identity.
- Multi-client tests proving clients can focus different resources.
- Git fixtures covering a main checkout and several linked worktrees.

### Exit criteria

- Several projects and worktrees coexist in one daemon.
- A client can jump directly from any pane to any other pane.
- Detachment preserves the tree; closing the final terminal removes its session.
- No server-global active resource is required to render or control a client.

## Milestone 3: Project definitions and workspace recipes

Replace the external Tmuxinator/Herdr bootstrap layer with a native, trusted creation flow.

### Work

- Define global user configuration, project configuration, and local override locations.
- Design a small TOML schema for tabs, panes, commands, environment, working directories, and initial layouts.
- Prefer command argument arrays; allow shell evaluation only when explicitly requested.
- Resolve configuration precedence with provenance available to diagnostics.
- Add project trust based on project identity and configuration content.
- Preview executable actions before first trust.
- Apply the same workspace recipe to the main checkout and new worktrees.
- Make repeated open commands attach to live resources rather than duplicate them.
- Add `fut config explain` and recipe validation.
- Port the useful shapes from the existing Tmuxinator projects as fixtures and examples.

### Tests

- Configuration merge and provenance tests.
- Trust invalidation when executable configuration changes.
- No-command execution before trust.
- Idempotent project/workspace open behavior.
- Recipes covering editor, server, console, logs, environment, and multi-pane tabs.

### Exit criteria

- Opening a known project produces its expected initial workspace.
- Opening a new worktree applies the shared workspace recipe.
- An untrusted repository cannot silently execute configuration.
- The current `hdr` bootstrap responsibilities can move into Fut.

## Milestone 4: Daily-driver terminal and navigation behavior

Reach the minimum tmux replacement level suggested by the current dotfiles.

### Work

- Prefix and prefix-free configurable bindings.
- Vim-style directional pane navigation and cycling.
- Last pane, tab, workspace, and session navigation.
- Numbered access within the current scope.
- Splits and new tabs inheriting the focused terminal's current directory.
- Pane zoom, titles, tab names, and automatic numbering.
- Focus-biased accordion layout.
- Scrollback, copy mode, selection, search, and clipboard integration.
- Truecolor, undercurl, hyperlinks, bracketed paste, focus events, mouse support, and extended keyboard protocols.
- Temporary action surfaces for commands such as git diff, URL opening, Hunk, and editor splits.
- Keybinding discovery equivalent to the useful part of tmux which-key.

### Tests

- Golden layout and navigation tests.
- Input encoding fixtures for modified and extended keys.
- CWD inheritance tests across shells and worktrees.
- Long-running output and resize stress tests.

### Exit criteria

- The normal tmux workflow can move to Fut without losing essential navigation or terminal behavior.
- The UI remains useful with the project tree visible, compact, or hidden.
- Focus-biased layout works as a first-class policy rather than a resize hook.

## Milestone 5: Agent activity and attention

Add agent awareness as metadata on the established resource tree.

### Work

- Expose terminal, pane, workspace, session, and socket identity through scoped `FUT_*` environment variables.
- Define a generic activity-report command and protocol.
- Store source, lifecycle state, timestamp, authority, and expiry for each observation.
- Reduce observations into effective idle, working, blocked, or unknown activity.
- Represent completion as an attention event with per-client seen cursors.
- Roll status up from terminals through tabs, workspaces, and sessions.
- Show compact inline status in the ordinary hierarchy.
- Add navigator filters and jumps for blocked and unseen-completion resources.
- Build a first-party Pi integration.
- Add process inspection and screen/OSC evidence only after explicit reports work reliably.

### Tests

- Pure reducer tests for conflicting, stale, and expiring observations.
- Per-client seen/unseen behavior.
- Rollup priority across mixed descendant activity.
- Integration tests for report, completion, focus, and process exit ordering.

### Exit criteria

- Pi can report working, blocked, idle, and completion against its terminal.
- Background status is visible without a separate agent panel.
- A user can jump directly to the next blocked or newly completed terminal.
- Activity degrades safely when an integration disappears or reports out of order.

## Milestone 6: Hardening and broader use

Improve reliability and portability only after the daily-driver model is validated.

### Work

- Bound queues, scrollback, memory use, and noisy-terminal work.
- Crash containment and useful daemon diagnostics.
- Graceful protocol upgrades and stale-socket recovery.
- Linux PTY and process support.
- Independent multi-client resize/input ownership policy.
- Shell integrations for authoritative current directory and prompt boundaries.
- Notification and sound policies derived from attention events.
- Remote attach, richer layouts, terminal images, and native clients only as separately justified projects.

### Exit criteria

- Long-running multi-project sessions remain responsive under concurrent output.
- Failure in one terminal or client does not corrupt the resource tree.
- Linux support does not require changing the project/session/workspace model.

## First implementation slice — spiked

The initial spike completed these steps:

1. Scaffold the Rust executable and test commands.
2. Establish the daemon/client boundary and protocol handshake.
3. Own one PTY in the daemon.
4. Parse it through an isolated `libghostty-vt` adapter.
5. Render and control it from a detachable Ratatui client.

The process-level end-to-end suite now proves that the same child survives detachment and that output produced while detached appears after reattachment. Before Milestone 2, the spike should be exercised interactively as a daily shell and editor host; terminal behavior discovered there belongs in Milestone 1 rather than being papered over in the resource-tree work.
