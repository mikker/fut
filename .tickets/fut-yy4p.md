---
id: fut-yy4p
status: closed
deps: []
links: []
created: 2026-08-21T09:15:37Z
type: feature
priority: 2
assignee: Mikkel Malmberg
tags: [terminal, alerts, ui]
---
# Add terminal-native alerts

Add terminal-native BEL attention alongside, but semantically separate from, coding-agent lifecycle activity.

## Design

Keep compact authoritative bell state on daemon-owned terminals and roll it up through pane, tab, workspace, and session ancestry. Each client owns an independent seen cursor so one client viewing a bell does not clear it for another. Use counters instead of an unbounded event log. Capture BEL at the PTY/parser boundary without altering terminal bytes. Feed bells into shared discoverable attention UI and typed navigation while keeping outer-terminal audible/visual notifications opt-in. Ordinary terminal output does not create alerts.

## Acceptance Criteria

BEL is detected correctly, including adjacent to fragmented escape sequences, and repeated bells are represented without unbounded history. Ordinary output does not create alerts. Bell state rolls up through the resource tree and appears in tabs, workspace rows, and the navigator. Clients have independent seen state across simultaneous attachment and detach/reattach. Users can navigate to the next bell and clear or acknowledge their own seen state. Tests cover multiple clients, detach intervals, process exit ordering, rollups, and bounded snapshots. User-facing configuration and usage documentation are updated.

## Notes

**2026-08-25T09:54:21Z**

Completed BEL-only terminal alerts: daemon-owned bounded bell counters, independent per-client seen state, ancestry rollups, navigation/clearing, and opt-in outer-terminal signaling. Ordinary output and silence do not create alerts. Validation: 572 unit tests pass; Clippy and build pass.

**2026-08-25T09:56:55Z**

Made the notification-count glyph configurable as ui.icons.notification and documented preserved spacing. Local config now uses the requested Nerd Font glyph with a trailing space.
