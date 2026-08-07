---
id: fut-16o2
status: closed
deps: [fut-qlom]
links: []
created: 2026-08-07T08:09:25Z
type: feature
priority: 2
assignee: Mikkel Malmberg
tags: [ui]
---
# Which-key style cheatsheet on prefix hesitation

After ~600-800ms of hesitation while PrefixState is waiting, show a passive overlay listing prefix suffixes and their actions from BindingsConfig + command definitions. Keys pass through to prefix.feed() unchanged and dismiss the overlay; no input handling of its own. Reuses shared dialog chrome. Lists 'Space - command palette' so it funnels into the palette.

