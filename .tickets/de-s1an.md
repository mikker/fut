---
id: de-s1an
status: open
deps: []
links: []
created: 2026-08-18T19:55:10Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: de-8pyh
tags: [extensions, runtime]
---
# Introduce the daemon-owned extension registry

Replace separately captured extension vectors with one immutable, generation-stamped extension registry that represents the complete active extension set and its resolved global configuration.

## Design

Define a registry/catalog type with a monotonically increasing generation and deterministic fingerprint. The daemon owns the active Arc snapshot. Hook dispatch, token declaration validation, configuration resolution, and protocol-facing catalog data derive from that snapshot. Keep parsing and validation side-effect free so a candidate can be built before publication.

## Acceptance Criteria

There is one authoritative daemon representation of active extensions; consumers no longer retain unrelated startup-only extension sets; equivalent input produces a deterministic catalog/fingerprint; candidate construction performs no activation side effects; unit tests cover identity, duplicate declarations, generation changes, and snapshot retention.

