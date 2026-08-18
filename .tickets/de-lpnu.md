---
id: de-lpnu
status: open
deps: [de-lhfk]
links: []
created: 2026-08-18T19:55:11Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: de-8pyh
tags: [extensions, cli, doctor]
---
# Add extension inspection, validation, and reload commands

Expose the extension registry as an understandable operational surface before adding package installation.

## Design

Add noun-first commands for listing active extensions, showing manifest source/version/capabilities, validating a package without activating it, and requesting reload. Provide versioned JSON output and useful human diagnostics. Extend doctor output with generation, compatibility failures, and package provenance without executing extension code.

## Acceptance Criteria

Users and automation can list, inspect, validate, and reload extensions; JSON success and error envelopes follow existing CLI conventions; validation is daemonless and side-effect free; doctor explains active generation and rejected candidates; completion and docs cover the new grammar.

