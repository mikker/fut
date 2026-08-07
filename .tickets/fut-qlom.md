---
id: fut-qlom
status: closed
deps: []
links: []
created: 2026-08-07T08:09:25Z
type: task
priority: 2
assignee: Mikkel Malmberg
tags: [ui]
---
# Shared dialog chrome for palette, unread list, and future overlays

Extract a dialog module with centered floating geometry, clear/fill helpers, selected/dim row styles, footer hints, keep-selected-visible scrolling, and a list scrollbar reusing the scrollbar_thumb math from src/client/mod.rs. Re-house NotificationsDialog (currently full-screen) in this chrome, deleting its bespoke header/footer/clear code.

