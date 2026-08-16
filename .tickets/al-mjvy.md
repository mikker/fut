---
id: al-mjvy
status: closed
deps: [al-zorp]
links: []
created: 2026-08-16T18:25:09Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: al-hcfm
tags: [ui, config, refactor]
---
# Replace the workspace sidebar with a component stack

Extract the current workspace-only sidebar into a left sidebar surface that composes Workspaces and Agents components.

## Design

Introduce a SidebarComponent enum, a shared ComponentEffect enum executed by the client host, and one sidebar modal surface with a focused component index. Replace workspace-specific layout and hit-test duplication with one computed geometry model consumed by render and input. Support Fixed and Fill vertical sizes with at most one Fill component. Replace [ui.workspace_sidebar] with [ui.sidebar.left] and tagged component entries rather than carrying migration compatibility. Automatic visibility is relevance-driven instead of hard-coded to workspace count.

## Acceptance Criteria

Workspaces and Agents render through the same sidebar host; Tab/BackTab moves component focus and keys route only to the focused component; workspace create/rename/switch/help behavior remains intact; passive clicks, active clicks, drawers, divider dragging, tiny heights, and configuration validation share authoritative geometry; at most one Fill component is accepted.


## Notes

**2026-08-16T19:07:48Z**

Replaced workspace-specific sidebar with [ui.sidebar.left] component stack, exhaustive component/effect types, shared geometry, focused modal component routing, Fixed/Fill sizing, and relevance-driven visibility. Migrated Workspaces and Agents plus config/docs/e2e fixtures.
