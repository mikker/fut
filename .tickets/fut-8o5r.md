---
id: fut-8o5r
status: closed
deps: []
links: []
created: 2026-08-10T13:57:05Z
type: task
priority: 2
assignee: Mikkel Malmberg
---
# Tabs should take name after current process like tmux

I guess - if title blank, set to frontmost process
If set, always use given name


## Notes

**2026-08-11T09:58:21Z**

Implemented automatic unnamed-tab titles from the focused foreground process, stable explicit titles, focus/exit handling, bounded background polling with native process-name lookup, docs, and tests.
