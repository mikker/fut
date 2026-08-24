---
id: fut-g6lk
status: closed
deps: []
links: []
created: 2026-08-24T11:06:19Z
type: feature
priority: 2
assignee: Mikkel Malmberg
---
# Expose active extensions to extension processes

Let one extension detect which other extensions are active, including their package versions, without depending on Fut's internal socket protocol.

## Acceptance Criteria

Extension commands and lifecycle hooks receive a documented compact JSON environment value mapping every active extension ID to its package version. Tests, authoring docs, and changelog cover the contract.


## Notes

**2026-08-24T11:10:13Z**

Added FUT_EXTENSIONS as a deterministic JSON object of active extension IDs to package versions for interactive/background commands and resource/client hooks. Documented active-vs-installed semantics and added runtime coverage. cargo test passed except one conformance timeout that passed immediately in isolation; cargo build passed.
