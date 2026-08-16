---
id: al-zorp
status: closed
deps: [al-iz60]
links: []
created: 2026-08-16T18:25:09Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: al-hcfm
tags: [ui, agents]
---
# Build the session-scoped Agents sidebar component

Build the first Herder-style Agents list as a client-side projection of integrated terminal activity in the current session.

## Design

Rows retain TerminalId identity, target PaneId for navigation, and show session/workspace/tab context plus existing ActivityIndicator and per-client unseen attention. Reuse spinner timing, NotificationState, and NavigationHistory. Keep the first version navigate-only and session-scoped. Exercise it privately against the current sidebar before committing to a generalized public component API; do not add a temporary public show_agents option.

## Acceptance Criteria

All open integrated agents in the focused session appear with correct idle, working, blocked, and unseen-completed presentation; selection survives resource refresh when possible; Enter and mouse selection navigate through typed PaneId selection; closing resources disappear safely; no daemon or protocol changes are required.


## Notes

**2026-08-16T19:07:48Z**

Implemented integrated-agent sidebar projection with stable TerminalId selection, PaneId navigation, session filtering, closing-ancestry exclusion, and shared activity/attention indicators. Covered by component unit tests.
