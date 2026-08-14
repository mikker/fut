---
id: thr-f02d
status: closed
deps: [thr-sh31]
links: []
created: 2026-08-14T09:38:16Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: thr-tind
---
# Publish namespaced extension presentation tokens

Let extensions declare namespaced dynamic presentation tokens and let trusted external commands publish their materialized values for a Fut resource scope.

## Design

Rendering remains pure: commands never execute during a frame. A bounded daemon-owned token store carries declared values into resource snapshots or equivalent authoritative client context. Token names are extension-namespaced, scopes are validated, stale/resource-closure cleanup is explicit, and publication uses the normal control CLI/protocol.

## Acceptance Criteria

A declared extension token can be placed in valid UI segment contexts and updated through a versioned CLI command; unknown tokens/scopes are rejected; values are bounded and sanitized; multiple clients reconcile the same authoritative value; rendering performs no extension I/O.
