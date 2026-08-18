---
id: thr-tind
status: closed
deps: []
links: [de-8pyh]
created: 2026-08-14T09:38:16Z
type: epic
priority: 2
assignee: Mikkel Malmberg
---
# Add narrowly scoped Fut extensions

Introduce extensions as explicitly loaded local directories that package namespaced configuration and optional executable assets. The extension surface is limited to lifecycle hooks and declared dynamic presentation tokens; it is not a general plugin runtime or ecosystem.

## Design

An extension has a small manifest, a stable id, and a root used to resolve direct argv such as ./bin/refresh. It may declare hooks and presentation tokens. Exclude actions, installation, discovery, registries, dependency resolution, build commands, services, storage APIs, pane entrypoints, and native UI. Existing CLI and event APIs remain the general integration surface.

## Acceptance Criteria

The child tickets establish explicit loading, trusted bounded hook execution, namespaced dynamic token publication, and internal Git-token dogfooding without adding an in-process plugin API.


## Notes

**2026-08-14T14:01:53Z**

Implemented explicit local extension loading, bounded workspace lifecycle hooks, namespaced dynamic presentation tokens, daemon-owned Git token publication, FUT_BIN callback context, and a checked-in smoke extension. Semantic review approved with no comments. Formatting, Clippy, and the full serial test suite pass.
