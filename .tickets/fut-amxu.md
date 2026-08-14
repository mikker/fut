---
id: fut-amxu
status: closed
deps: []
links: []
created: 2026-08-14T07:12:13Z
type: feature
priority: 2
assignee: Mikkel Malmberg
tags: [ui]
---
# Filter navigator by resource level

Replace repeated Ctrl-S/W/T/P cycling with scoped resource-level filters, add Ctrl-A reset, and distinguish resource levels visually.

## Acceptance Criteria

Ctrl-S lists sessions; Ctrl-W/T/P list resources within the selected ancestor; Ctrl-A clears resource and text filters; each level has a configurable default color; docs and tests pass.


## Notes

**2026-08-14T07:12:17Z**

Implemented scoped Ctrl-S/W/T/P resource filters, Ctrl-A full reset, filter-aware title/footer, configurable red/blue/green/magenta level styles, documentation, unit coverage, and updated navigator E2E behavior. Refactor pass centralized resource styling/filter labels and simplified scoped range filtering. Validation: mise run check passed (399 unit tests, 93 E2E tests, Clippy warnings denied, formatting, and render targets).
