---
id: mul-kyqv
status: closed
deps: []
links: []
created: 2026-08-10T11:27:10Z
type: task
priority: 2
assignee: Mikkel Malmberg
---
# Support cursor type change api (block / caret / underline / etc)

Nvim/tmux can change this so insert mode is line, normal is block etc


## Notes

**2026-08-10T13:09:50Z**

Scope clarified: honor block/bar/underline and blinking where representable; safely fall back to block and restore a sane host cursor.
