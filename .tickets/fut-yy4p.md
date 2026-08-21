---
id: fut-yy4p
status: open
deps: []
links: []
created: 2026-08-21T09:15:37Z
type: feature
priority: 2
assignee: Mikkel Malmberg
tags: [terminal, alerts, ui]
---
# Add terminal-native alerts

Add terminal-native attention signals alongside, but semantically separate from, coding-agent lifecycle activity: BEL events, unread output from terminals not visible to a client, and configurable silence monitoring.

## Design

Keep compact authoritative alert state on daemon-owned terminals and roll it up through pane, tab, workspace, and session ancestry. Model BEL, unread output, and silence as distinct sources rather than inferring agent completion or blocking. Each client owns independent seen cursors so one client viewing an alert does not clear it for another. Use monotonic revisions/timestamps instead of an unbounded event log. Silence is a timer over last output with an explicit threshold; unread output is based on visibility; BEL is captured at the PTY/parser boundary without altering terminal bytes. Feed all sources into shared discoverable attention UI and typed navigation while keeping outer-terminal audible/visual notifications opt-in.

## Acceptance Criteria

BEL is detected correctly, including adjacent to fragmented escape sequences, and repeated bells are represented without unbounded history. Unread output is recorded only according to documented client visibility rules and remains distinct from semantic agent activity. Configurable silence monitoring resets on output and expires at the configured threshold. Alert state rolls up through the resource tree and appears in tabs, workspace rows, and the navigator. Clients have independent seen state across simultaneous attachment and detach/reattach. Users can navigate to the next alert and clear or acknowledge their own seen state. Tests cover multiple clients, focused and unfocused output, detach intervals, process exit ordering, rollup precedence, timer behavior, and bounded snapshots. User-facing configuration and usage documentation are updated.

