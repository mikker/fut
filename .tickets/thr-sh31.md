---
id: thr-sh31
status: closed
deps: []
links: []
created: 2026-08-14T09:38:16Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: thr-tind
---
# Load explicit extension directories

Add the smallest extension manifest and explicit configuration needed to load a local directory, namespace its declarations, and resolve packaged executable assets relative to its root.

## Design

Require an extension id and explicit local path. Provide FUT_EXTENSION_ID and FUT_EXTENSION_ROOT to executable declarations. Validate ids, paths, duplicate declarations, manifests, and relative argv without automatic discovery or remote installation. Integrate executable project-local extensions with Fut's trust boundary.

## Acceptance Criteria

Explicit extension paths load deterministically with useful diagnostics; ./bin commands resolve against the extension root; duplicate ids and invalid manifests fail atomically; no extension code runs before required trust; no installer, registry, search path, or action surface is introduced.
