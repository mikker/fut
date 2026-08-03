# Fut vision

## Product promise

Fut is a modern terminal multiplexer for work that happens across projects, Git worktrees, and long-running coding agents. It should retain tmux's reliability and terminal-native reach while replacing its server/session/window vocabulary with a hierarchy that matches contemporary development:

```text
one Fut multiplexer
└── session: a live project
    └── workspace: a checkout or worktree
        └── tab: an activity
            └── pane: a terminal placement
```

Every resource is directly navigable. A user can move from a pane in one worktree to a pane in another project without detaching or crossing server namespaces.

Fut remains a terminal-within-a-terminal. The host terminal renders text and windows; Fut owns child terminals, layout, navigation, project lifecycle, and agent-aware context. A native macOS terminal emulator is a possible future client, not the initial product boundary.

## Principles

### One multiplexer, many sessions

Fut runs one user-level daemon with one resource tree. Sessions are ordinary resources inside it. Starting a project must not create a separate daemon, socket namespace, or navigation island.

The daemon exits when no sessions remain. Fut has no empty idle runtime.

### Runtime is ephemeral; definitions are durable

A project definition can persist indefinitely. A live session exists only while it owns live terminals.

Detaching a client preserves live terminals. Closing the last terminal causes natural upward cleanup:

```text
last pane closes      → tab closes
last tab closes       → workspace closes
last workspace closes → session closes
last session closes   → Fut exits
```

Fut will not initially pretend it can restore arbitrary processes after a daemon or machine restart. Reopening a project applies its recipe to create a new runtime. Cold restoration, if ever added, must be explicit about the difference between restoring structure and preserving processes.

### Project and worktree are first-class

A Git repository is one project even when work occurs in several linked checkouts. Its main checkout and all worktrees appear as peer workspaces beneath one session. Non-Git directories use their canonical root as project identity.

Workspace creation should make branching work cheap. Opening a worktree gives it the same useful starting topology as the main checkout without requiring the project file to enumerate every future branch.

### Navigation follows the object model

The hierarchy is visible and searchable, but never compulsory to traverse one level at a time. Stable resource identities and a global navigator make every session, workspace, tab, and pane a direct destination.

Fut should support:

- direct fuzzy navigation across the complete tree;
- next and previous navigation within the current scope;
- fast numbered access where a small ordered scope makes it useful;
- last pane, tab, workspace, and session;
- next blocked or unseen-completion navigation;
- terminal-friendly keybindings compatible with the user's existing tmux habits.

Focus and seen state belong to each attached client. Multiple clients may inspect the same live terminals without being forced to share sidebar state, dialogs, selection, or the same active workspace.

### One typed, agent-oriented control surface

Resource operations follow the hierarchy: noun first, then operation (`workspace rename`, `tab close`). Top-level `open` and `list` and the `daemon` lifecycle are intentional non-resource entry points. The protocol carries typed resource identities, and UIs use those types directly rather than constructing command-line selectors. The public CLI accepts raw opaque IDs for exact operations; mutations are ID-only, with exact session names allowed only as a convenience for session attach. A UUID-shaped session attach value is interpreted as an ID before considering names. Ancestor attachment through a session, workspace, or tab succeeds only when it identifies exactly one open terminal; pane and terminal IDs are exact, and ambiguous interactive navigation belongs in the navigator. It does not expose a `resource:<id>` selector language.

Automation is a first-class client. Agents use versioned JSON responses and raw IDs, while human-readable output is not a contract and remains free to improve. JSON failures use the compact `{version:1,error:{code,message}}` envelope, preserving daemon codes and using `invalid_arguments` or `command_failed` for CLI-originated failures. UIs use the typed protocol rather than parsing CLI output. Manual use does not require memorizing UUIDs: dynamic daemon-backed completion displays live names, hierarchy, roots, and pane-versus-terminal context but inserts the same raw ID used by automation. Completion is read-only, bounded, and silent when no daemon is available; static command completion remains. Bare `fut` deliberately opens the current directory and then attaches to the returned terminal. The explicit CLI commands `open`, `tab new`, and `pane new` are otherwise control-only and return the selected resource ancestry and terminal identity; attachment remains separate, with no claimed atomic combined CLI operation. This keeps one precise control model underneath distinct human and agent affordances.

### Agent awareness is ambient

An agent remains a process in a terminal. Fut does not create a parallel Agent hierarchy or reserve a permanent agent dashboard.

Activity and attention are attached to terminals and rolled up through the existing tree. A session or workspace can show that a descendant is working, blocked, or newly complete. The navigator can derive an attention-only view when needed.

Agent integrations should report a small generic lifecycle protocol. Explicit lifecycle reports are preferred; foreground-process and screen evidence are fallbacks. A completion is an event, while `done` is the client-relative presentation of idle work with an unseen completion.

### Project setup is built in

Fut absorbs the useful part of Tmuxinator: opening a project or worktree can create named tabs, split panes, run commands, set environment variables, and choose working directories.

Configuration resolves in layers:

```text
built-in defaults
→ user configuration
→ trusted project configuration
→ local project override
→ explicit command options
```

Project configuration is a creation recipe. It initializes missing runtime resources but does not continuously rewrite a user's live session back to the file.

Executable project configuration requires explicit trust. Safe metadata may be read before trust, but entering an unfamiliar checkout must never silently run repository-provided commands.

### Terminal behavior is not a detail

Fut must be a transparent home for existing shells, editors, REPLs, servers, and agents. Its baseline includes:

- Unicode and grapheme-correct terminal state;
- truecolor, undercurl, hyperlinks, and modern escape sequences;
- extended keyboard protocols and unambiguous modified keys;
- correct resize and reflow behavior;
- focus events, bracketed paste, mouse input, and clipboard integration;
- scrollback, selection, search, and copy mode;
- working-directory and title propagation;
- responsive rendering under concurrent terminal output.

The existing workflow vocabulary remains valuable: vim-style pane movement, splits inheriting the current directory, zoom, pane titles, numbered tabs, popup-like temporary actions, and a focus-biased accordion layout.

## Architecture

The shared runtime and each client's presentation are separate domains:

```text
fut client
  focus, viewport, modes, theme, keymap, seen events
       │
       │ local protocol
       ▼
fut daemon
  sessions, workspaces, tabs, pane placements
  terminals, PTYs, terminal state, activity
  project definitions, trust, resource events
```

The daemon owns processes and terminal state. A terminal has an opaque stable identity independent of the pane that displays it. This allows panes to move without changing the identity used by automation, integrations, or activity reports.

The core protocol exposes resources, mutations, snapshots, terminal updates, and typed events. It does not require every client to consume one server-rendered application screen. The initial Ratatui client lives in the same repository and executable, but the core model does not depend on Ratatui widgets or global UI focus. It renders every pane in the selected tab with client-owned geometry and focus while the daemon streams terminal-specific snapshots, revisioned resource snapshots, and authoritative reconciled tab views. The first layout is deliberately small—equal-width columns with focused-pane fallback—rather than a persisted split model or the eventual focus-biased accordion.

Terminal runtimes act independently so slow parsing, detection, rendering, or one noisy process cannot block unrelated terminals. Project configuration is resolved per project/workspace rather than from a daemon-wide current directory.

No database is required initially. The in-memory daemon is authoritative for runtime state; files hold user and project definitions. Any later runtime snapshot format must remain honest about whether it represents live processes, recoverable terminal history, or only recreatable structure.

## Technology direction

Fut is written in Rust. The intended initial stack is:

- Tokio for asynchronous tasks and coordination;
- a Unix PTY backend, likely through `portable-pty` behind a Fut-owned interface;
- public `libghostty-vt`, pinned to an exact revision and hidden behind a narrow adapter;
- Ratatui for the first terminal client;
- Serde and TOML for resource messages and configuration;
- a local Unix socket with a scriptable JSON control surface.

The first supported platform is macOS. The design should remain Unix-shaped enough to add Linux without restructuring the resource model. Windows and a native graphical terminal client are deferred.

`libghostty-vt` supplies terminal parsing, state, input encoding, scrollback, and render cells. Fut supplies PTYs, processes, multiplexing, lifecycle, project semantics, UI, and actual drawing through the host terminal. Ghostty remains isolated behind an adapter because its public API is still unstable.

## End-state experience

A representative flow should be:

1. Run `fut` in a project checkout.
2. Fut finds or starts the one daemon and identifies the project.
3. If the project has no live session, Fut requests trust if necessary and applies its project recipe.
4. The current checkout appears as a workspace with its standard tabs and panes.
5. Creating or opening another worktree adds a peer workspace under the same session and applies the workspace recipe.
6. Agents report activity through the terminal environment and Fut control protocol.
7. Status appears inline on panes, tabs, workspaces, and sessions; completed or blocked work is directly reachable.
8. Detaching leaves all terminals alive.
9. Exiting the final terminal removes the final pane, workspace, and session, then exits Fut.
10. Opening the project later creates a fresh session from its durable definition.

## Initial non-goals

- Building a GPU terminal renderer or native macOS terminal emulator.
- Preserving arbitrary processes across daemon or machine restarts.
- A separate agent orchestration system or dedicated agent dashboard.
- A general plugin ecosystem before the core resource and control APIs settle.
- Reproducing every tmux option or Tmuxinator feature.
- Remote attach, collaborative input, Windows support, or Kitty graphics in the first release.

## Success criteria

The initial product direction is validated when Fut can:

- keep several project sessions in one daemon;
- model a project's main checkout and worktrees as peer workspaces;
- jump directly between any live resources;
- preserve terminals across client detach and reattach;
- remove a session when its last terminal closes and exit Fut when its last session closes;
- initialize a worktree from trusted project-local configuration;
- show Pi activity and unseen completion inline without a separate status area;
- comfortably run the user's current shells, editors, agents, and development commands.
