---
id: mul-gnuq
status: closed
deps: []
links: []
created: 2026-08-10T11:27:56Z
type: task
priority: 2
assignee: Mikkel Malmberg
---
# Consider shorter ids than uuid

Very long to type and very unlikely to have collisions at even very short ids


## Notes

**2026-08-26T09:42:44Z**

Implemented exact compact selectors rather than truncating identity. Resource IDs remain UUID-backed in runtime maps and canonical UUIDs in serde/protocol/--json, while Display, shell completion, human output, tokens, persisted/display strings, and FUT_*_ID environments use a canonical f-prefixed 23-character URL-safe encoding of all 128 bits. Every typed CLI ID accepts either form, including get and session name-vs-ID dispatch. This avoids collision/ambiguity and snapshot-dependent mutation resolution while reducing 36 characters to 23. Updated CLI help, user/agent/extension/token docs, CHANGELOG, unit tests, protocol compatibility fixtures, completion parsing, and e2e assertions. Validation: cargo fmt -- --check; cargo test --lib (592 passed); cargo test --test e2e (123 passed); cargo build (success).

**2026-08-26T10:20:37Z**

Refactor pass removed the redundant display/reparse path from resource lookup and compares retained UUID identity directly. Revalidated after refactor: cargo fmt -- --check; cargo test --lib (592 passed); cargo test --test e2e (123 passed); cargo build (success). Manual isolated-daemon smoke tests confirmed 23-character human/environment IDs, canonical UUID JSON, get with both selector forms, terminal targeting by canonical UUID, and tab rename mutations by both compact ID and UUID.
