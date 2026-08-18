---
id: de-ahh4
status: closed
deps: [de-lpnu]
links: []
created: 2026-08-18T19:55:11Z
type: feature
priority: 2
assignee: Mikkel Malmberg
parent: de-8pyh
tags: [extensions, packages, security]
---
# Add a managed local extension store

Install third-party extension packages into a Fut-owned user data directory while keeping activation explicit and reversible.

## Design

Support installation from an explicit local directory or package artifact into an immutable/versioned staging location, validate before atomic placement, record source provenance and content digest, and provide enable, disable, and remove operations. Never execute build/install scripts. Enabling updates the configured active set through the normal atomic reload path; disabling does not delete package data.

## Acceptance Criteria

A valid package can be installed, enabled, disabled, and removed without hand-editing absolute paths; interrupted or invalid installs leave no active partial package; provenance and digest are inspectable; ID/version collisions require an explicit resolution; package contents execute only after explicit enablement; filesystem and end-to-end tests cover rollback and recovery.


## Notes

**2026-08-18T22:14:31Z**

Implemented a bounded XDG-managed local package store with immutable content-addressed copies, strict versioned index metadata, install/enable/disable/remove CLI commands, digest verification, safe copy rules, atomic index updates, and config merging. Superseded/unindexed immutable bytes are retained so live older daemon catalogs never acquire dangling executable paths. Full check/build passed before the final retention fix; strict Clippy, store unit tests, focused E2E, and build passed afterward.
