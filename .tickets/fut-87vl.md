---
id: fut-87vl
status: closed
deps: []
links: []
created: 2026-08-07T09:57:50Z
type: feature
priority: 2
assignee: Mikkel Malmberg
---
# Event stream: subscribe to state changes as JSON

Outside tools can drive fut through the CLI, but they can only poll to learn that something changed. Add a way to subscribe to fut's state changes (sessions, workspaces, tabs, panes, focus) as a stream of JSON events. This is the reactive half of the CLI surface and the only new capability external integrations need — fut emits facts, it never runs anyone's scripts or learns their policies. With a real external consumer, the JSON output and event schema become a public API; the existing version envelope becomes a promise.


## Notes

**2026-08-08T21:47:27Z**

Implemented: protocol 7 adds control-only WatchResources; daemon control_loop streams ResourcesChanged snapshots off the existing resource_changes watch channel; new 'fut events' prints one versioned JSON line ({version,command:"events",result:<snapshot>}) for the current state and each change, ending cleanly when the daemon exits. E2E test + docs + changelog. Not committed.
