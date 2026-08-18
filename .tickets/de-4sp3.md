---
id: de-4sp3
status: open
deps: [de-ee74, de-xutz]
links: []
created: 2026-08-18T19:55:11Z
type: task
priority: 3
assignee: Mikkel Malmberg
parent: de-8pyh
tags: [extensions, wasm, design]
---
# Evaluate a capability-sandboxed WebAssembly extension tier

After the trusted process ecosystem is proven, determine whether Fut has concrete demand for portable extensions that are less trusted than native executables.

## Design

Prototype only enough Wasmtime/WASI Component Model integration to measure binary size, startup cost, packaging, WIT API ergonomics, cancellation, and filesystem/network capability control. Reuse the manifest and logical extension contract rather than creating a second ecosystem. Compare against continuing with supervised subprocesses. Do not ship a runtime merely for language preference.

## Acceptance Criteria

A written decision records measured costs, required host capabilities, threat model, API sketch, and a ship/defer/reject recommendation; any prototype is clearly isolated; the trusted-process implementation does not depend on the result.

