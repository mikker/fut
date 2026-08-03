# Fut context

## Mental model

Fut is one terminal multiplexer containing live project sessions. Each session contains one or more checkouts, each checkout contains tabs, and each tab contains panes. Agent activity is metadata on this tree rather than a separate navigation hierarchy.

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

A durable filesystem identity and optional configuration from which a session can be created. A Git repository and all of its worktrees are one project. A non-Git directory may also be a project.

A project is not necessarily live. Its definition may remain after its session has closed.

### Session

The live incarnation of one project. A session groups every workspace belonging to that project and exists only while at least one of its terminals is alive.

A session is a navigable resource inside Fut, not a server, socket namespace, or persistence boundary.

### Workspace

A live checkout of a project. The main checkout and every Git worktree are peers and each becomes one workspace. A workspace supplies the filesystem root and working context inherited by its tabs and terminals.

### Worktree

A Git checkout linked to the same repository as other checkouts. A worktree is represented by a workspace; it is not an additional level in Fut's hierarchy.

### Tab

A named activity inside a workspace, such as `agent`, `editor`, `server`, `console`, or `logs`. A tab contains one or more panes arranged by a layout.

### Pane

A placement and viewport for one terminal within a tab layout. Moving or rearranging a pane does not change the identity of the terminal it displays.

### Terminal

The stable, process-bearing resource behind a pane. A terminal owns the pseudoterminal, child process, terminal state, scrollback, current directory, title, and activity observations. Terminal identity survives pane movement and client detachment.

### Layout

The arrangement and sizing policy for panes in a tab. A split tree is the default layout shape. A focus-biased or accordion layout gives the focused pane a useful working width while leaving its siblings visible.

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

The user's approval for a project's executable configuration. Reading safe metadata does not imply permission to run project-provided commands, hooks, or plugins.

### Activity

Current work state associated with a terminal, such as idle, working, blocked, or unknown. Activity can be reported by an integration or inferred from process and terminal evidence.

Activity describes a terminal; it is not itself a pane, tab, workspace, session, or separate navigation area.

### Attention event

A noteworthy transition, such as an agent completing work or becoming blocked. Attention is event-based rather than a permanent activity state.

### Seen

A client-relative record that an attention event has been visited or acknowledged. One client seeing a completion does not erase it for another client.

### Done

The presentation of an idle terminal with an unseen completion event. Done is derived from activity and seen state rather than stored as an independent terminal state.

### Status rollup

The derived summary shown on a tab, workspace, or session from the activity and attention of its descendants. Rollups make background work visible in the normal project tree.

### Direct navigation

Moving from any current location to any session, workspace, tab, or pane without detaching, changing server namespaces, or walking through each intermediate level.

### Navigator

The searchable, flattened view of Fut's hierarchy used for direct navigation. It may filter for resources needing attention, but does not create a second source of truth.

### Runtime state

The live sessions, terminals, layouts, and process state currently owned by Fut. Runtime state is intentionally distinct from durable project definitions.
