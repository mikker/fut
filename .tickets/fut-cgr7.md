---
id: fut-cgr7
status: closed
deps: []
links: []
created: 2026-08-09T20:47:36Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [terminal, cli]
---
# Add raw terminal input and command submission

Expose raw process interaction under fut terminal for agents and scripts.

## Design

Add send-text, send-keys, and run. send-text is literal and does not submit; send-keys accepts validated logical keys; run atomically writes command text and Enter. Resolve an explicit terminal target and serialize the complete operation in the daemon. Reuse live bracketed-paste and key encoding behavior.

## Acceptance Criteria

Literal Unicode text, logical keys, control chords, bracketed paste, and atomic run work against explicit terminals; invalid keys write nothing; JSON acknowledgements and typed terminal-exit errors are covered end to end.


## Notes

**2026-08-09T21:15:42Z**

Implemented target-first `fut terminal send-text <TERMINAL_ID> <TEXT>`, `send-keys <TERMINAL_ID> <KEY>...`, and `run <TERMINAL_ID> <COMMAND>`. Text uses the live Ghostty paste encoder without Enter; keys are fully parsed and encoded through the live client key path before connecting; run is one runtime message and one PTY write containing mode-aware paste plus CR. Added targeted protocol operation/ack (protocol 9), recent-exit tombstones for typed terminal_exited responses, compact versioned JSON acks, parser/protocol/Ghostty tests, and E2E coverage for Unicode, no-submit, late-invalid atomic rejection, chords, both bracketed-paste modes, atomic run, and finalized exits. `mise run check` passed: 327 unit tests, 82 E2E tests, formatting, clippy, and benchmark smoke tests.
