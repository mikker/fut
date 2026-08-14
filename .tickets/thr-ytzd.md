---
id: thr-ytzd
status: closed
deps: [thr-f02d]
links: []
created: 2026-08-14T09:38:16Z
type: task
priority: 3
assignee: Mikkel Malmberg
parent: thr-tind
---
# Route built-in Git metadata through the dynamic token store

Dogfood dynamic presentation-token publication with Fut's existing workspace Git branch, insertion, and deletion metadata while keeping the Git collector internal.

## Design

Preserve the existing workspace.git_branch, workspace.git_added, and workspace.git_deleted behavior and asynchronous bounded Git collection. Change their materialization path to use the same store and rendering boundary as extension-published values; do not require or ship a Git extension yet.

## Acceptance Criteria

Existing Git tokens retain names, styles, refresh bounds, empty/error behavior, and multi-client correctness while exercising the dynamic token store. No Git process runs during rendering.
