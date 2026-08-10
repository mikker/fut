---
id: fut-usn7
status: closed
deps: []
links: []
created: 2026-08-09T20:47:36Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [agent, cli]
---
# Add compact context and explicit target discovery

Let an agent cheaply discover its current Fut ancestry and resolve explicit pane, terminal, and agent targets without parsing the complete resource tree.

## Design

Add a compact versioned JSON context/get surface using FUT_SESSION_ID, FUT_WORKSPACE_ID, FUT_TAB_ID, FUT_PANE_ID, and FUT_TERMINAL_ID when present. Never use a UI client's visual focus as an automation default.

## Acceptance Criteria

Inside Fut, one command returns validated current ancestry and activity; outside Fut, explicit IDs remain usable and missing context has a typed error; output is compact and completion/help are covered.


## Notes

**2026-08-09T21:03:02Z**

Implemented top-level fut context and fut get <UUID>. Context requires and parses the complete FUT_SESSION_ID/FUT_WORKSPACE_ID/FUT_TAB_ID/FUT_PANE_ID/FUT_TERMINAL_ID chain, resolves the terminal against the daemon snapshot, and rejects missing, incomplete, invalid, or mismatched ancestry with typed errors. Get globally resolves explicit session/workspace/tab/pane/terminal UUIDs without inherited context or visual focus. Added compact versioned JSON/human output with ancestry and pane activity, dynamic completion, CLI/help/unit/e2e coverage, docs, bundled skill guidance, and changelog. Verified cargo test cli:: --lib, cargo test --test e2e context_, and cargo clippy --all-targets -- -D warnings.
