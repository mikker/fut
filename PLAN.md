# Fut implementation plan

## Current status

The Milestone 0–1 spike and the current daemon-integrated Milestone 2 slice are implemented. They establish:

- a Rust `fut` executable and pinned Rust/Zig toolchain;
- one securely owned per-user daemon socket;
- a pure ResourceTree used by the daemon, separate from its terminal runtime registry;
- several project sessions in one daemon, with explicitly opened linked Git worktrees as peer workspaces;
- stable terminal identities and per-terminal attachment leases;
- a narrow `libghostty-vt` adapter producing Fut-owned semantic snapshots;
- a detachable Ratatui client with a global pane navigator, a responsive focus-biased pane accordion, focused-pane fallback, and per-client pane focus;
- end-to-end tab and explicit pane creation, allowing multiple tabs per workspace and multiple panes per tab;
- end-to-end session, workspace, and tab rename support;
- a noun-first CLI with raw-ID resource operations, control-only `open`, and explicit interactive attach commands;
- a versioned JSON success envelope for noninteractive control commands, using dotted command names;
- compact, versioned JSON error envelopes that preserve daemon codes and classify CLI failures;
- daemon-backed dynamic shell completion that shows live hierarchy while inserting raw IDs;
- multi-pane lifecycle and placement semantics: exact pane/terminal navigation, same-workspace pane movement, ancestor attach ambiguity, sibling preservation, final-pane upward cascade, and whole-tab descendant closure;
- durable terminal lifecycle and confirmed child cleanup;
- separate unit and process-level end-to-end test layers.

The public grammar is now `open`, `session`, `workspace`, `tab`, `pane`, `terminal`, `list`, and `daemon`. Noun-first applies to resource operations; top-level `open` and `list` and the `daemon` lifecycle are intentional non-resource entry points. Resource mutations take raw IDs only; attach operations do too, except that `session attach` permits an exact name. UUID-shaped session attach values have ID precedence. Session, workspace, and tab attachment succeeds only when the ancestor identifies exactly one open terminal; pane and terminal IDs are exact, and the navigator handles interactive selection, including sibling panes. The public CLI has no typed-prefix or selector mini-language. Child commands require a literal `--` and are passed as direct argv. Bare `fut` opens the current directory and then attaches to its returned terminal. `fut open`, `fut tab new`, and `fut pane new TAB_ID [--cwd PATH] [-- COMMAND...]` are control-only; each returns complete selected ancestry and terminal identity, while resource attachment remains separate. Pane creation defaults to the shell, and its cwd defaults to the workspace root with relative paths resolved against that root. The CLI makes no atomic create-and-attach guarantee. The TUI consumes typed protocol IDs directly rather than constructing CLI arguments.

This validates tab and pane creation, same-workspace pane movement, multi-pane lifecycle, proper tab close, session, workspace, and tab rename, structured control output, live completion, revisioned resource streaming, and the first responsive simultaneous-pane UI end-to-end but does not complete authored split semantics in Milestone 2. The client/daemon protocol is version 10. Successful `--json` responses use a `version: 1` envelope and dotted command name. Failures use `{version:1,error:{code,message}}`: daemon codes are preserved, argument errors use `invalid_arguments`, and otherwise untyped command failures use `command_failed`. Human output is deliberately not a machine contract; agents retain raw IDs and use `--json`, while UIs use the typed protocol. Shell completion covers the static grammar and reads live resources without autostarting or mutating the daemon; it displays names, ancestry, roots, and pane-versus-terminal context while inserting raw IDs. `pane new` completion includes eligible tab IDs with hierarchy and excludes closing tabs. `pane move` completion offers movable pane placements and filters destinations to other live tabs in the source workspace. Tabs may contain multiple panes. Exact pane and terminal navigation works; moving a pane preserves its terminal process and attachment lease; ambiguous ancestor attach is rejected; one pane exiting or closing preserves siblings and ancestors; focused-pane exit transfers to an available sibling; the final pane cascades upward; and whole-tab close closes every descendant. The client streams every pane in its selected tab and renders a focus-biased horizontal accordion in resource order, with a 24-column focused minimum, 12-column sibling minimums, neutral focus rails, focused-pane fallback in narrow terminals, one focused cursor, and next/previous cycling. Input and PTY resize authority remain exclusive to the focused terminal, allowing separate clients to focus different sibling terminals while observing the whole tab read-only. Revisioned full resource snapshots and authoritative selected views now reconcile external creation, movement, closure, and lifecycle changes without reselecting; background changes preserve per-client focus, and a moved focused pane follows its terminal to the destination tab. A background close removes the pane at close request, while a focused close retains focus through the final snapshot and terminal exit before transferring or ending the client with its nonzero status. The same stream keeps an open navigator current. This baseline deliberately has no persisted split direction or ratio, explicit zoom, titles, or mouse focus. Input encoding remains a basic client-side mapping and terminal snapshots are full-grid JSON messages. Git checkouts use their canonical common directory as project identity and their top-level checkout as workspace root; non-Git directories use canonical directory identity. Worktrees are not eagerly discovered. Opening a linked checkout adds one peer workspace, while reopening it is side-effect free. Interactive connections can retarget through the global navigator. A workspace disappears when its last terminal exits, a session when its last workspace exits, and only the final session closes Fut and releases the socket.

## Strategy

Build Fut as a sequence of narrow vertical slices. The first slice must prove process ownership, terminal correctness, detach/reattach, and the resource model before investing in visual polish or broad integrations.

Each milestone should leave a runnable `fut` binary. New abstractions are earned by the next milestone rather than designed for every possible client or platform in advance.

Milestone numbers group capabilities; they do not force implementation order. The current execution order is:

1. build Milestone 4's responsive spatial UI and daily-driver behavior while finishing the remaining authored split semantics from Milestone 2;
2. prove Milestone 5's agent activity and attention model in a narrow Pi-backed spike;
3. return to Milestone 3's trusted project definitions and workspace recipes;
4. harden the resulting product through Milestone 6.

UI work began before Milestone 2 closed: simultaneous pane rendering, per-client focus, and minimal split geometry now bridge resource semantics into Milestone 4. Milestone 4 treats visual quality as product work rather than a final polish pass. Terminal content should dominate a compact, responsive interface with unmistakable focus, a visible but unobtrusive hierarchy, restrained state color, and a small amount of character rather than ornamental panel chrome.

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
- Keep terminal identity stable while panes move within the tree. (Met for same-workspace tab-to-tab movement.)
- Implement create, close, rename, focus, move, split, and switch operations. (Explicit tab/pane create, lifecycle close, rename through tabs, exact switch, per-client focus, same-workspace pane movement, and a first visible multi-pane layout are implemented; authored split geometry remains.)
- Implement upward cascade closure from pane to session, preserve live siblings, close all descendants on whole-tab close, and close Fut when the last session closes. (Met.)
- Store client focus and navigation state per connection.
- Define resource snapshots and typed incremental events.
- Provide a JSON control API alongside the interactive protocol.
- Keep the public CLI noun-first and ID-oriented while preserving typed resource selectors inside the protocol.
- Add dynamic, daemon-backed shell completion that shows resource names and hierarchy but inserts raw IDs. (Implemented.)
- Return structured, versioned JSON errors for every noninteractive `--json` failure. (Implemented.)
- Add the global navigator over the complete resource tree.

### Tests

- Pure state-machine tests for every mutation and invariant.
- Property tests for cascade closure and unique/stable identity.
- Multi-client tests proving clients can focus different resources.
- Git fixtures covering a main checkout and several linked worktrees.

### Exit criteria

- Several projects and worktrees coexist in one daemon.
- A client can jump directly from any pane to any other pane. (Met.)
- Detachment preserves the tree; closing one pane preserves siblings and ancestors, while closing the final terminal removes its session. (Met.)
- No server-global active resource is required to render or control a client. (Met.)
- Shell completion obtains live resources from the daemon, presents enough names and ancestry to distinguish them, and inserts the exact raw ID accepted by the selected command. (Met.)
- Every noninteractive command has versioned JSON success and error output; command names are dotted and scripts never need to parse human output. (Met.)

## Milestone 3: Project definitions and workspace recipes (deferred)

Replace the external Tmuxinator/Herdr bootstrap layer with a native, trusted creation flow.

This capability remains part of the intended product, but implementation follows the daily-driver UI and initial agent-status spike. Until then, explicit CLI creation remains the honest bootstrap path.

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

## Milestone 4: Daily-driver terminal, navigation, and UI

Reach the minimum tmux replacement level suggested by the current dotfiles, with an interface that feels purpose-built rather than inherited from tmux.

### Work

- Establish a small theme and presentation-token system for hierarchy, focus, activity, attention, muted chrome, and terminal-safe contrast.
- Render simultaneous panes with clear focus, responsive degradation in small terminals, pane zoom, and a focus-biased accordion layout. (The accordion and focused-only degradation are implemented; explicit zoom remains.)
- Make project, workspace, tab, and pane context legible without surrounding the terminal in boxes or permanently consuming excessive space.
- Evolve the global navigator into a fast fuzzy destination and action surface, with compact keybinding discovery where context requires it.
- Prefix and prefix-free configurable bindings.
- Vim-style directional pane navigation and cycling.
- Last pane, tab, workspace, and session navigation.
- Numbered access within the current scope.
- Splits and new tabs inheriting the focused terminal's current directory.
- Pane titles, tab names, and automatic numbering.
- Scrollback, copy mode, selection, search, and clipboard integration.
- Truecolor, undercurl, hyperlinks, bracketed paste, focus events, mouse support, and extended keyboard protocols.
- Temporary action surfaces for commands such as git diff, URL opening, Hunk, and editor splits.
- Dogfood the primary shell, editor, agent, split, navigator, tiny-terminal, loading, error, and empty states as one coherent visual system.

### Tests

- Golden layout and navigation tests.
- Input encoding fixtures for modified and extended keys.
- CWD inheritance tests across shells and worktrees.
- Long-running output and resize stress tests.

### Exit criteria

- The normal tmux workflow can move to Fut without losing essential navigation or terminal behavior.
- The UI remains useful with the project tree visible, compact, or hidden.
- Focus-biased layout works as a first-class policy rather than a resize hook.

## Milestone 5: Agent activity and attention spike

Add agent awareness as metadata on the established resource tree.

Start this milestone as a proof-of-concept against the now-usable UI: one explicit reporting protocol, one first-party Pi integration, inline rollups, and direct navigation to attention. Do not begin with process heuristics or a generic integration framework.

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
