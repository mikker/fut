---
id: pro-mvic
status: closed
deps: [pro-ja4w]
links: []
created: 2026-08-18T07:22:56Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: pro-pgup
---
# Add the in-client project opener

Add Ctrl-b Shift-S Open Project UI with fuzzy configured-project suggestions and free-form path input.

## Acceptance Criteria

Configured projects are suggested without scanning; paths can be entered; live projects navigate instead of duplicating; new projects attach to the recipe-selected terminal.


## Notes

**2026-08-18T09:40:02Z**

Project opener must present untrusted repository recipes for in-client approval and call the shared machine-local trust API; users should never handle hashes or edit trust state.

**2026-08-25T08:45:20Z**

Implemented the Ctrl-b Shift-S in-client opener with catalog-only fuzzy suggestions, typed paths, idempotent live navigation, recipe-focus attachment, and exact-content in-client trust approval through the shared machine-local trust store. Added focused UI/protocol tests, stabilized the shell chaos fixture, docs, and Unreleased changelog. Full sequential all-target test suite, fmt, clippy, benches, and build pass.

**2026-08-25T09:04:19Z**

Follow-up: fixed the demo's JSON opens to use background mode, added the opener to the demo catalog/hints, and made the current-workspace highlight include its index. Refactor pass consolidated project-opener state access and reused one project resolver. RUST_TEST_THREADS=1 mise run check and mise run build pass.
