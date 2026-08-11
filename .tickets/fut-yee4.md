---
id: fut-yee4
status: closed
deps: []
links: []
created: 2026-08-11T08:04:46Z
type: task
priority: 2
assignee: Mikkel Malmberg
---
# Add "toast" notifications system with small bordered dialogs in corner

For updates, like "config reloaded"


## Notes

**2026-08-11T18:01:27Z**

Implemented a typed single-toast system for all former client notices. Toasts use the existing rounded frame/shadow, render on the right opposite the configured tab bar, auto-dismiss informational feedback after three seconds, and retain errors/actionable feedback until the next key or replacement. Copy-mode feedback now uses the shared toast path. Added unit coverage, retained E2E feedback assertions, updated the changelog, and mise run check passes.

**2026-08-11T19:39:56Z**

Follow-up polish: toast text now preserves configured foreground/background colors, uses bold emphasis instead of reverse video, and has one-cell horizontal padding. Demo scripts now build and run target/debug/fut by default; the isolated demo setup/cleanup smoke test and full mise run check pass.

**2026-08-11T20:42:33Z**

Final refactor pass removed the obsolete copy-mode notice rendering path, kept toast-owned cursor suppression explicit, and verified formatting, Clippy, 382 unit tests, and 92 E2E tests via RUST_TEST_THREADS=1 mise run check.
