# Fut context

## Mental model

Fut is one terminal multiplexer containing live project sessions. Each session contains one or more logical workspaces, each workspace contains tabs, and each tab contains panes. A workspace is user-defined: it may represent a checkout or worktree, but is not equated with either. Agent activity is metadata on this tree rather than a separate navigation hierarchy.

```text
Fut
└── Session
    └── Workspace
        └── Tab
            └── Pane
```

## Terms

### Fut

The product, the running multiplexer, and the source project. `fut` is also the command-line executable used to start, attach to, inspect, and control it.

### Multiplexer

The single running environment that owns all live sessions. Sessions are resources inside the multiplexer, not isolated multiplexer instances.

### Project

The planned durable filesystem identity and optional configuration from which a session can be created. A Git repository and its worktrees would share one project; a non-Git directory could also be a project. Project definitions and persistence are not implemented: current sessions and workspaces are live only.

### Session

The live incarnation of one project. A session groups every workspace belonging to that project and exists only while at least one of its terminals is alive.

A session is a navigable resource inside Fut, not a server, socket namespace, or persistence boundary.

### Workspace

A user-defined live context that groups tabs. A workspace records the filesystem root and working context inherited by its tabs and terminals, but it does not prescribe what that directory means. A Git checkout or worktree fits naturally as a workspace; users may also create several logical workspaces from the same directory within one project session.

### Worktree

A Git checkout linked to the same repository as other checkouts. A worktree can be opened as a workspace; it is not an additional level in Fut's hierarchy, and Fut does not create or manage worktrees implicitly.

### Tab

A named activity inside a workspace, such as `agent`, `editor`, `server`, `console`, or `logs`. A tab contains one or more panes arranged by a layout.

### Pane

A placement and viewport for one terminal within a tab layout. Moving or rearranging a pane does not change the identity of the terminal it displays.

### Terminal

The stable, process-bearing resource behind a pane. A terminal owns the pseudoterminal, child process, terminal state, scrollback, current directory, and activity observations. Terminal identity survives pane movement and client detachment. Pane and terminal titles are not implemented.

### Layout

The arrangement of panes in a tab. Every tab owns an authored split tree as shared runtime state. Clients evaluate that tree against their own viewport, minimum sizes, and selected presentation policy.

### Split tree

The shared runtime topology of a tab. Leaf nodes reference pane placements; branch nodes split left/right or top/bottom and store a ratio. `Ctrl-b |` splits the focused pane to the right and `Ctrl-b _` splits it downward. Closing or moving a pane removes its leaf and collapses any branch left with one child.

The split tree belongs to the tab rather than to one client, but computed rectangles never do. Resource order and split-tree leaf order must agree so navigation, automation, fallback, and rendering do not acquire competing notions of pane order.

### Layout policy

A client-owned choice for presenting a tab's panes. The default policy renders the split tree. An accordion policy may temporarily present the same ordered pane leaves as a focus-biased horizontal accordion without rewriting shared split topology.

### Accordion

An alternate layout policy that gives the focused pane more width while keeping siblings visible when space permits. It is not a substitute for authored split direction, and `pane_min_width` is only a responsive constraint—not a layout model.

### Zoom

A temporary client-owned overlay that gives the focused pane the complete terminal area available to the tab. Zoom does not modify the authored split tree or another client's presentation.

### Client

An attached user interface displaying and controlling the multiplexer. A client's focus, viewport, navigation selection, modes, and seen activity belong to that client rather than to the shared session tree.

### Attach and detach

Attaching connects a client to the live multiplexer. Detaching removes that client without closing terminals or sessions. Detachment is therefore different from closure.

### Close

Removing a live resource and, where applicable, terminating its process. Closure cascades upward when a parent becomes empty: the last pane closes its tab, the last tab closes its workspace, the last workspace closes its session, and the last session closes Fut.

### Project definition

The durable declarative configuration associated with a project. It describes how a new session or workspace should initially be created. It is a creation recipe, not a continuously reconciled desired state.

### Workspace recipe

The portion of a project definition applied when a workspace is first created. It may declare initial tabs, pane layouts, commands, environment, and working directories. The same recipe can initialize the main checkout and newly discovered worktrees.

### Project trust

The user's machine-local approval for the exact current bytes at a repository recipe's canonical path. `fut project trust NAME` validates and approves that recipe, while `fut project untrust NAME` revokes it; neither operation requires a daemon. Changed content is untrusted, and reading safe metadata does not imply permission to run project-provided commands, hooks, or plugins. A recipe path explicitly selected in global configuration is inherently trusted.

### Activity

Current semantic work state associated with a terminal: idle, working, or blocked. It is reported explicitly by an integration or `fut terminal report`; process and terminal inference are planned.

Activity describes a terminal; it is not itself a pane, tab, workspace, session, or separate navigation area.

### Attention event

A noteworthy explicit transition: an agent becoming blocked or completing work. Attention is event-based rather than a permanent activity state.

### Seen

A client-relative record that an attention event has been visited or acknowledged. One client seeing a completion does not erase it for another client.

### Done

The presentation of an idle terminal with an unseen completion event. Done is derived from activity and seen state rather than stored as an independent terminal state.

### Status rollup

The client-derived summary shown on tabs, workspaces, and navigator rows from descendant activity and attention. Rollups make background work visible in the normal project tree.

### Presentation token

A pure, typed value expanded by a client from resource and client state already in memory, such as a workspace name, tab index, closing marker, or zoom state. Presentation tokens perform no I/O or execution. Future dynamic providers publish asynchronously into this rendering context rather than running during a frame.

### Direct navigation

Moving from any current location to any session, workspace, tab, or pane without detaching, changing server namespaces, or walking through each intermediate level.

### Navigator

The searchable, flattened view of Fut's hierarchy used for direct navigation. It may filter for resources needing attention, but does not create a second source of truth.

### Runtime state

The live sessions, terminals, layouts, and process state currently owned by Fut. Runtime state is intentionally distinct from durable project definitions.
