---
id: fut-87vl
status: open
deps: []
links: []
created: 2026-08-07T09:57:50Z
type: feature
priority: 2
assignee: Mikkel Malmberg
---
# Event stream: subscribe to state changes as JSON

Outside tools can drive fut through the CLI, but they can only poll to learn that something changed. Add a way to subscribe to fut's state changes (sessions, workspaces, tabs, panes, focus) as a stream of JSON events. This is the reactive half of the CLI surface and the only new capability external integrations need — fut emits facts, it never runs anyone's scripts or learns their policies. With a real external consumer, the JSON output and event schema become a public API; the existing version envelope becomes a promise.

