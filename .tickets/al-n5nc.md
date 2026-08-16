---
id: al-n5nc
status: closed
deps: [al-mjvy]
links: []
created: 2026-08-16T18:25:09Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: al-hcfm
tags: [ui, layout, config]
---
# Add an independent right sidebar slot

Add a right sidebar with its own component stack, width, visibility, drawer, and resize state.

## Design

Model fixed SidebarSide values Left and Right rather than one movable sidebar. Allocate both slots against the terminal while preserving minimum terminal width and narrow-host drawer fallback. Keep drag ownership and client-local widths independent. Give each side an unambiguous typed open/focus action and binding.

## Acceptance Criteria

Left and right sidebars can be enabled, hidden, docked, opened as drawers, and resized independently; both may be docked when geometry permits; narrow terminals preserve terminal usability; clicks and divider drags target the correct side; configuration reload and reattach reset client-local widths correctly.


## Notes

**2026-08-16T20:03:54Z**

Implemented fixed left/right SidebarSide slots with independent config, component stacks, docking/drawers, rendering/hit testing, modal ownership, widths, divider drags, bindings, and dual-side/narrow/right-edge coverage. Existing Ctrl-b W remains last-workspace; right drawer uses Ctrl-b ].
