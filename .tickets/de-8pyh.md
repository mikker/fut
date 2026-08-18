---
id: de-8pyh
status: open
deps: []
links: [thr-tind]
created: 2026-08-18T19:55:10Z
type: epic
priority: 1
assignee: Mikkel Malmberg
tags: [extensions, ecosystem]
---
# Make Fut extensions dynamically reloadable and distributable

Turn Fut's existing manifest-and-process extension boundary into a coherent third-party ecosystem. Extensions remain out of process and language-neutral: Fut owns manifests, capabilities, lifecycle, configuration, and a versioned control protocol while extension code may be any executable. The first milestone makes one authoritative extension set reloadable without restarting Fut; the second adds compatibility metadata, diagnostics, managed installation, and an authoring contract.

## Design

Build on the current explicitly configured extension directories, direct argv execution, bounded hooks, commands, configuration, and presentation tokens. Introduce an immutable generation-stamped registry owned by the daemon. Candidate sets validate completely before an atomic swap; attached clients consume the daemon-authoritative catalog instead of independently drifting. Lifecycle events bind to the registry generation active when committed. Third-party packages declare API compatibility and capabilities. Managed installs are pinned and explicit. Do not load Rust dylibs or embed JavaScript/Lua. Keep arbitrary native extensions inside the existing trusted-executable boundary; reserve WebAssembly for a later, separately justified sandbox tier.

## Acceptance Criteria

Extensions can be added, changed, and removed without restarting the daemon; reload is atomic across daemon behavior and attached clients; queued events have deterministic generation semantics; manifests expose a versioned compatibility and capability contract; users can validate, inspect, install, update, enable, disable, and remove managed third-party packages with clear provenance; author documentation and end-to-end fixtures prove that extensions can be implemented in any executable language; malformed, incompatible, or partially installed packages never replace the active registry; docs and changelog are updated and the full build/test suite passes.

