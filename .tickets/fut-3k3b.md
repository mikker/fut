---
id: fut-3k3b
status: closed
deps: []
links: []
created: 2026-08-14T07:33:14Z
type: task
priority: 2
assignee: Mikkel Malmberg
tags: [ui]
---
# Label workspace list with session name

Show the owning session above workspace rows, including a compact minimized form and deliberate vertical padding.

## Acceptance Criteria

Expanded and minimized workspace lists identify the session in bold without displacing rows on tiny terminals.


## Notes

**2026-08-14T07:33:14Z**

Added the default bold session header, two padding rows, minimized abbreviation, tiny-height fallback, and rendering coverage. Validation: mise run check passed.
