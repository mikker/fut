---
id: mul-e66u
status: closed
deps: []
links: []
created: 2026-08-10T11:29:17Z
type: task
priority: 2
assignee: Mikkel Malmberg
---
# Make layout commands location aware

Ie
`fut tab rename new-name` should know which tab it is in and rename self.
same for `fut workspace close` and so on. No id should imply self.
If not applicable just fail with exit code and do nothing


## Notes

**2026-08-10T13:09:50Z**

Scope clarified: omitted IDs resolve live ancestry from the calling Fut terminal (pane -> tab -> workspace -> session); explicit IDs remain supported; outside/stale/inapplicable context fails without mutation.
