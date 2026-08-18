---
id: de-lhfk
status: closed
deps: [de-rf4s]
links: []
created: 2026-08-18T19:55:11Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: de-8pyh
tags: [extensions, client, protocol]
---
# Synchronize extension generations across attached clients

Make commands, presentation tokens, client hooks, and daemon hooks change as one coherent extension generation for every attached client.

## Design

Publish the daemon-authoritative extension catalog and generation through the typed protocol. A reload validates the client's complete UI candidate and the daemon's extension candidate before either becomes visible, then clients reconcile command bindings, presentation declarations, and client-hook runtimes from the acknowledged catalog. Broadcast generation changes to other attached clients. Avoid independent client filesystem reads as the source of extension truth.

## Acceptance Criteria

The initiating client never reports success while daemon and client capabilities disagree; all attached clients converge on the same generation; added and removed palette commands, bindings, token presentation, and client hooks update without reattach; invalid UI or extension configuration preserves the active state; multi-client and reconnect tests cover convergence and rollback.


## Notes

**2026-08-18T21:41:27Z**

Implemented and verified with strict Clippy, the full unit suite, extension-focused E2E coverage, and a successful build. The full E2E run had one unrelated focus-transfer timing failure that passed immediately when rerun serially.
