---
id: de-ee74
status: closed
deps: [de-vhs0, de-lpnu]
links: []
created: 2026-08-18T19:55:11Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: de-8pyh
tags: [extensions, docs, testing]
---
# Publish the language-neutral extension authoring guide and conformance fixtures

Make the process protocol sufficient for external authors who do not know Fut internals or write Rust.

## Design

Document package layout, manifest/API versions, capabilities, argv execution, environment, JSON payloads, configuration, token publication, timeouts, failure semantics, and compatibility policy. Provide a minimal conformance fixture plus at least one packaged extension implemented as a standalone compiled executable, while keeping the protocol language-neutral. Include a release checklist for evolving the contract.

## Acceptance Criteria

An external author can build and validate an extension from public documentation alone; examples exercise commands, hooks, configuration, and tokens; conformance tests run through the public process/control boundary; documentation clearly states the trusted-native-code model and why Fut does not require JS, Lua, or Rust dylibs.


## Notes

**2026-08-18T22:57:37Z**

Published the API v1 language-neutral authoring guide, compatibility/release checklist, conformance runner, and a dependency-free standalone Rust extension example compiled with rustc and exercised through public Fut commands, hooks, config, and token publication. Full formatting, Clippy, unit/E2E, conformance, debug/release build, and Jekyll docs verification passed.
