---
id: fut-gpkw
status: closed
deps: []
links: []
created: 2026-08-15T14:14:13Z
type: feature
priority: 2
assignee: Mikkel Malmberg
tags: [ui, mouse]
---
# Add contextual menus for tabs and workspaces

Add Fut-owned right-click context menus to tab-bar items and workspace-sidebar rows. Right-clicking an inactive resource opens its menu without changing terminal focus; switching remains an explicit menu action. Keep terminal-pane right-click behavior unchanged so applications retain their mouse input.

## Design

Build a small contextual-menu surface positioned at the pointer and clipped to the client bounds. It should share one action model between mouse and keyboard input, support hover/selection, arrows, Enter, Esc, and outside-click dismissal, and target the resource that was right-clicked even when it is not active.

Tab menu: Switch to Tab (inactive only), New Tab, Rename, Close.

Workspace menu: Switch to Workspace (inactive only), New Workspace, Rename, Close, followed by explicit checked settings for Display (Expanded/Minimized) and Visibility (Visible/Auto-hide/Hidden).

Dispatch resource actions through the same request and safeguard paths as existing commands. Do not activate a resource merely by opening its menu. Preserve terminal application right-click handling by limiting this feature to Fut chrome.

## Acceptance Criteria

- Right-clicking a visible tab opens a menu anchored near that tab/pointer and does not switch focus.
- Right-clicking an expanded or minimized workspace row opens a menu for that workspace and does not switch focus.
- Inactive-resource menus offer Switch as the first item; active-resource menus omit or disable it.
- Tab menus provide New, Rename, and Close actions for the intended context.
- Workspace menus provide New, Rename, Close, and explicit checked Display and Visibility choices.
- Menu items work by left click and by Up/Down plus Enter.
- Esc and clicking outside dismiss the menu without acting.
- Menus remain fully visible by flipping/clamping at terminal edges and behave sensibly in tiny clients.
- Closing uses the existing close behavior and safeguards.
- Right-clicks inside terminal panes continue to reach terminal applications as before.
- Rendering, hit-testing, keyboard behavior, and edge placement have focused tests; relevant E2E coverage passes.
