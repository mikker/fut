---
id: de-xutz
status: closed
deps: [de-ahh4]
links: []
created: 2026-08-18T19:55:11Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: de-8pyh
tags: [extensions, packages, distribution]
---
# Add pinned remote extension install and update workflows

Allow third-party packages to be acquired from explicit remote sources without introducing a central registry or ambient discovery.

## Design

Accept a narrow documented remote source form, resolve it to immutable content, verify a recorded digest, and retain source/revision provenance. Updates are explicit, stage and validate a complete replacement, show the version/source transition, and activate through atomic registry reload. Do not run remote build hooks, resolve transitive dependencies, or silently update.

## Acceptance Criteria

Users can install and explicitly update a package from a supported remote source; the installed bytes are pinned and auditable; checksum or compatibility failures preserve the installed and active version; offline listing/removal works; no automatic discovery, central registry, dependency resolver, or background update is added.


## Notes

**2026-08-18T22:41:41Z**

Implemented explicit pinned Git installation and updates using exact full commit IDs, HTTPS/absolute file URLs, bounded isolated Git execution, strict provenance, optional content-digest verification, and rollback-safe immutable installation. Remote credentials/queries/fragments, branches, tags, hooks, LFS smudging, submodules, symlinks, special files, and automatic updates are rejected. Full test/build passed before final URL hardening; strict Clippy, store unit tests, focused Git E2E, and build passed afterward.
