---
id: de-vhs0
status: open
deps: []
links: []
created: 2026-08-18T19:55:10Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: de-8pyh
tags: [extensions, protocol]
---
# Version the third-party extension package contract

Give extension authors a stable, explicit compatibility contract rather than relying on the current implicit manifest shape and Fut release version.

## Design

Add a manifest API version, package version, Fut compatibility range, and declared capabilities. Version hook payloads and document the stable environment/control surfaces. Reject unknown API versions and unsatisfied compatibility before activation. Capabilities describe Fut cooperation and are not presented as an OS sandbox.

## Acceptance Criteria

Manifests declare enough metadata for Fut to determine compatibility before execution; existing first-party examples are migrated; unsupported API versions and incompatible Fut ranges fail with actionable diagnostics; capability declarations are strict and bounded; compatibility behavior is covered by fixtures and tests.

