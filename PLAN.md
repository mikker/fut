# Fut implementation plan

## Current status

The Milestone 0–1 spike and the current daemon-integrated Milestone 2 slice are implemented. They establish:

- a Rust `fut` executable and pinned Rust/Zig toolchain;
- one securely owned per-user daemon socket;
- a pure ResourceTree used by the daemon, separate from its terminal runtime registry;
- several project sessions in one daemon, with user-defined logical workspaces and explicitly opened linked Git worktrees as natural peers;
- stable terminal identities and per-terminal attachment leases;
- a narrow `libghostty-vt` adapter producing Fut-owned semantic snapshots;
- a detachable Ratatui client with a global pane navigator, a responsive focus-biased pane accordion, focused-pane fallback, and per-client pane focus;
- live navigation and action chrome: an activatable horizontal tab/status bar, responsive workspace sidebar, contextual create/rename actions, and searchable typed command bar with direct-binding parity;
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

This validates logical workspace, tab, and pane creation, same-workspace pane movement, authored right/down split topology, multi-pane lifecycle, proper tab close, session, workspace, and tab rename, structured control output, live completion, revisioned resource streaming, and responsive simultaneous-pane UI end-to-end. Workspaces are user-defined collections of tabs with CWD context: Git checkouts fit naturally, but Fut does not prescribe or create them, and multiple workspaces in one session may share a root. `Ctrl-b w` and `Ctrl-b t` activate their respective resource lists, where `c` creates and `r` renames the selection. The client/daemon wire format remains on reserved development epoch `0`; compatibility is not maintained between rapid-development builds, and stable versioning is deferred until the format settles. Successful `--json` responses use a `version: 1` envelope and dotted command name. Failures preserve daemon codes and classify local argument and command errors. Shell completion remains bounded and read-only. Tabs own shared split trees whose validated leaf order matches pane order; clients default to recursively rendered splits and may select the accordion policy without rewriting topology. Focused-only degradation, client-local zoom, exact resize authority, directional and cyclic pane focus, wrapping, numbered and last-resource navigation, interactive workspace/tab CWD inheritance, host-native indexed ANSI colors, and complete command-launcher coverage are implemented. Revisioned resource and selected views reconcile creation, movement, closure, lifecycle, and topology changes; focused exit remains inside its session and publishes the fresh resource snapshot needed to remove closed tab chrome. The baseline still lacks ratio editing, titles, mouse focus, richer input, trusted recipes, and agent activity.

The initial navigation and action-discovery surfaces, explicit client-local pane zoom, shared authored split tree, alternate accordion policy, right/down splits, directional and cyclic pane focus, wrapping, numbered and last-resource navigation, interactive tab CWD inheritance, and complete command-bar action coverage are implemented. The narrow Pi-backed agent-status proof of concept is next.

## Strategy

Build Fut as a sequence of narrow vertical slices. The first slice must prove process ownership, terminal correctness, detach/reattach, and the resource model before investing in visual polish or broad integrations.

Each milestone should leave a runnable `fut` binary. New abstractions are earned by the next milestone rather than designed for every possible client or platform in advance.

Milestone numbers group capabilities; they do not force implementation order. The current execution order is:

1. prove Milestone 5's agent activity and attention model in a narrow Pi-backed spike; Milestone 4's authored splits, directional and cyclic pane navigation, wrapping, numbered and last-resource navigation, CWD inheritance, complete command coverage, initial chrome, accordion policy, and pane zoom are implemented;
2. refine remaining daily-driver terminal behavior around the agent spike as dogfooding exposes it;
3. return to Milestone 3's trusted project definitions and workspace recipes;
4. harden the resulting product through Milestone 6.

UI work began before Milestone 2 closed: simultaneous pane rendering, per-client focus, and minimal split geometry now bridge resource semantics into Milestone 4. Milestone 4 treats visual quality as product work rather than a final polish pass. Terminal content should dominate a compact, responsive interface with unmistakable focus, a visible but unobtrusive hierarchy, restrained state color, and a small amount of character rather than ornamental panel chrome.

## Authored splits and complete actions (implemented)

The default tab layout is a shared authored split tree rather than the former accordion-only presentation. A leaf references one pane ID. A branch stores an axis, ratio, and two children. The tree is daemon-owned runtime resource state, serializes through snapshots and the typed protocol, and contains no Ratatui rectangles or client focus. Its leaf traversal is the tab's canonical pane order and must exactly cover the tab's panes.

- `Ctrl-b |` splits the focused pane to the right; `Ctrl-b _` splits it downward. New branches begin 1:1, the new pane receives focus only after correlated creation and selection acknowledgements, and input remains paused through that transition.
- Interactive splits inherit the focused terminal's current directory. The daemon queries the terminal's root child PID through a platform adapter at split time rather than trusting client or shell text (`lsof` on the current macOS implementation and `/proc` on Linux). If that fresh query is unavailable because the process raced exit or the OS refuses it, creation falls back to the terminal's recorded spawn directory, then the workspace root. Foreground-process-group, direct libproc, and OSC 7 refinement may follow without changing the split contract.
- Closing a pane removes its leaf and collapses a one-child branch. Moving a pane removes and collapses at the source, then splits the destination tab's final open leaf to the right at 1:1, placing the moved pane in the new right leaf. This preserves the existing append order and does not change pane, terminal, process, PTY, or lease identity.
- Automation has an explicit typed relative split operation addressed by raw pane ID and direction. Existing tab-relative pane creation remains deterministic by inserting a right split at the final leaf until project recipes can author complete trees.
- The default client layout recursively renders the authored tree. A leaf requires 24 content columns when focused, 12 otherwise, and three content rows; every branch divider consumes one neutral cell on its axis. Subtree minimums compose recursively. If the entire tree fits, each branch applies its ratio clamped to both child minimums. If it does not fit, only the focused pane fills the host with no divider. These exact rules apply independently per client and never mutate the tree. Explicit zoom remains a client overlay.
- The accordion remains a separate client-owned policy over split-tree leaf order. Safe UI configuration provides `pane_layout = "splits" | "accordion"`, defaulting to `"splits"`; this preference is attach-time-only like existing chrome placement.
- `Ctrl-b n` and `Ctrl-b p` wrap through open tabs in the current workspace and resolve the client's last valid terminal for each tab, falling back deterministically when history is stale.
- `Ctrl-b h/j/k/l` uses unzoomed logical client geometry for non-wrapping directional pane focus; `Ctrl-b o` and `Ctrl-b ;` retain canonical cycling. `Ctrl-b P/T/W/S` toggles acknowledged per-client history, and `Ctrl-b 1` through `9` plus `0` select exact one-based tab slots 1 through 10.
- Interactive new tabs inherit CWD through the same fresh process lookup, recorded spawn-directory fallback, and workspace-root fallback as anchored splits. Control-plane tab creation without an explicit CWD remains rooted in its workspace.
- The command bar includes every dispatchable `ClientAction` except `OpenCommandBar` itself. A reverse catalog test fails if any direct binding or action lacks a launcher definition. Direct bindings and launcher choices continue through one typed dispatcher.

Validation covers pure split-tree mutation and collapse, directional geometry, per-client history, wire-format round trips, lifecycle reconciliation, CWD inheritance, direct and launcher dispatch for tab/split/navigation actions, responsive degradation, alternate accordion behavior, host-native indexed ANSI colors, and real PTY geometry and resize authority. Stable protocol versioning remains intentionally deferred.

## Guardrails

These constraints protect the foundational model during implementation:

- Never create one daemon or socket namespace per session.
- Never put the globally active session, workspace, tab, or pane in shared daemon state.
- Keep pane placement identity separate from terminal/process identity.
- Keep authored split topology in shared tab resources, but computed rectangles, layout policy, zoom, viewport, and focus per client.
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
- Let users model the main checkout and linked worktrees as peer workspaces without requiring that interpretation.
- Keep terminal identity stable while panes move within the tree. (Met for same-workspace tab-to-tab movement.)
- Implement create, close, rename, focus, move, split, and switch operations. (Implemented through explicit tab/pane create, authored right/down splits, lifecycle close and topology collapse, rename, exact switch, per-client focus, and same-workspace pane movement.)
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

- Add a persistent, responsive tab bar that doubles as a compact status line: preserve terminal space, make the active tab unmistakable, and leave room for scoped activity and attention later. (Implemented with live full-snapshot reconciliation, one-row suppression, Unicode-safe overflow, and globally configured top/bottom placement.)
- Add a responsive workspace sidebar that makes the current context and its peers visible without opening the global navigator. Default to the left, permit right placement through the same safe global UI configuration, dock it in wide terminals, and collapse it in narrower terminals. (Implemented with a 120-column dock threshold, zero-width narrow collapse, `Ctrl-b w` edge drawer, contextual create/rename, exact-terminal switching, remembered per-workspace focus, and live snapshot reconciliation.)
- Add a searchable command bar over typed client actions. Open it with `Ctrl-b :`, show each action's direct binding when one exists, and let direct keys and the command bar invoke the same action rather than constructing CLI commands. Treat terminal delivery of macOS Command-modified keys as a later compatibility path, not the baseline binding. (Implemented with every dispatchable client action except its own opener, one catalog and dispatcher, reverse coverage invariants, filtering, bounded input, paste isolation, and modal behavior.)
- Establish a small theme and presentation-token system for hierarchy, focus, activity, attention, muted chrome, and terminal-safe contrast.
- Render simultaneous panes with clear focus, responsive degradation in small terminals, pane zoom, and switchable layout policies. (Implemented with the default authored split policy, alternate focus-biased accordion, explicit client-local zoom, persistent cue, focused PTY resizing, and command-bar parity.)
- Make project, workspace, tab, and pane context legible without surrounding the terminal in boxes or permanently consuming excessive space.
- Evolve the global navigator into a fast fuzzy destination and action surface, with compact keybinding discovery where context requires it.
- Prefix and prefix-free configurable bindings.
- Vim-style directional pane navigation and cycling. (Implemented over unzoomed logical client geometry, including tiny-layout reachability.)
- Last pane, tab, workspace, and session navigation. (Implemented as per-client acknowledged-transition history with stale descendant fallback.)
- Numbered access within the current scope. (Implemented for one-based tab slots 1–10 in the current workspace.)
- Splits and new tabs inheriting the focused terminal's current directory. (Implemented with fresh process lookup, recorded spawn-directory fallback, then workspace root.)
- Pane titles, tab names, and automatic numbering.
- Scrollback, copy mode, selection, search, and clipboard integration.
- Truecolor, undercurl, hyperlinks, bracketed paste, focus events, mouse support, and extended keyboard protocols.
- Temporary action surfaces for commands such as git diff, URL opening, Hunk, and editor splits.
- Dogfood the primary shell, editor, agent, split, navigator, tiny-terminal, loading, error, and empty states as one coherent visual system.

### Tests

- Golden layout and navigation tests, including narrow degradation for the workspace and tab/status bars.
- Command-bar filtering, selection, action-dispatch, keybinding-label, empty-result, and tiny-terminal tests.
- Parity tests proving direct bindings and command-bar choices dispatch the same typed client actions.
- Input encoding fixtures for modified and extended keys.
- CWD inheritance tests across shells and worktrees.
- Long-running output and resize stress tests.

### Exit criteria

- The normal tmux workflow can move to Fut without losing essential navigation or terminal behavior.
- The UI remains useful with the project tree visible, compact, or hidden.
- The current workspace and tab remain legible during normal terminal use, and available client actions are discoverable without consulting documentation.
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
