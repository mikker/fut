---
id: fut-o6nf
status: closed
deps: []
links: []
created: 2026-08-09T20:47:37Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [terminal, cli]
---
# Add terminal read and output waits

Let agents inspect terminal output and wait for ordinary command results without polling in shell loops.

## Design

Add terminal read with bounded visible, recent, and recent-unwrapped text sources plus optional ANSI. Add wait-output for literal or regex matches with explicit timeout. Implement daemon-side observation against terminal state and updates, not repeated CLI capture calls.

## Acceptance Criteria

Reads are bounded and Unicode-correct; wrapped logs can be reconstructed; wait-output checks current output then future changes without races; timeout, terminal exit, invalid regex, alternate-screen limitations, and JSON shapes are documented and tested.


## Notes

**2026-08-09T21:32:52Z**

Implemented `fut terminal read TERMINAL_ID [--source visible|recent|recent-unwrapped] [--lines N] [--ansi]` and `fut terminal wait-output TERMINAL_ID (--literal TEXT|--regex PATTERN) --timeout DURATION [--source ...] [--lines N]`. Reads use Ghostty selection formatting, cap historical windows at 2,000 physical rows, 250,000 inspected cells, and 1 MiB output without UTF-8 truncation; results report requested/returned rows, truncation, and mid-soft-wrap starts. Waits subscribe to snapshot+lifecycle watches before their initial read, use one daemon deadline, inspect final emulator state after exit, and return UTF-8-boundary byte ranges. Historical reads reject alternate screen; visible remains supported. Added regex dependency and bumped protocol 9 to 10. Verified cargo fmt/check, clippy -D warnings, 332 lib tests, and 83 e2e tests.
