---
id: al-hcfm
status: closed
deps: []
links: []
created: 2026-08-16T18:25:09Z
type: epic
priority: 1
assignee: Mikkel Malmberg
tags: [ui, agents, extensions]
---
# Compose sidebars from built-in components

Turn Fut's workspace-specific sidebar into independently configurable left and right component stacks, and add a Herder-style agents list as a projection of terminal activity. Agents remain metadata on the existing resource tree; built-in UI components are not an arbitrary executable plugin runtime.

## Design

Use exhaustive client-side Rust component types and host-owned typed effects. Preserve one modal ClientSurface: a sidebar surface owns the focused component and routes input. Compute sidebar geometry once for rendering and hit testing. Components declare simple Fixed or Fill vertical sizing and relevance for automatic visibility. Reuse segment rendering, tokens, semantic styles, and typed actions, but do not unify sidebar geometry with the one-row tab-bar allocator. Existing local extensions remain bounded hooks, commands, and presentation tokens; external UI declarations are explicitly deferred.

## Acceptance Criteria

Users can configure left and right sidebars independently and compose built-in Workspaces and Agents components. The Agents component can project matching agent terminals without introducing agent resources or daemon-global focus. Existing workspace navigation, drawers, resizing, minimized behavior where supported, per-client attention, and narrow-terminal degradation remain correct. No arbitrary extension-rendered UI or shared sidebar/tab-bar runtime registry is introduced.


## Notes

**2026-08-16T20:04:25Z**

Completed all children. Independent Fable review found sidebar-origin navigation, closing-focus relevance, unfocused hit geometry, binding, minimized rail, full-frame paint, duplicate Workspaces, and zero-height focus issues; all substantive findings were fixed. Full fmt, clippy -D warnings, all-target build, 459 library tests, and 104 e2e tests pass. External plugin UI and tab-bar registry unification remain deliberately deferred.
