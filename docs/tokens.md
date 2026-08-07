---
layout: default
title: Presentation tokens
description: Pure values for tab and workspace presentation.
permalink: /tokens/
---

# Presentation tokens

Presentation tokens are pure, typed values expanded from the resource snapshot and client state already held by Fut. They never execute commands or perform I/O. Tokens are valid only in the documented context; a misspelling or out-of-scope token prevents the interactive client from starting.

A token segment is explicit TOML:

```toml
{ token = "tab.name", prefix = "[", suffix = "]", max_width = 24, style = "current" }
```

If the token is empty, its prefix and suffix are also omitted.

## Tab-bar tokens

These may appear in groups under `ui.tab_bar.left`, `center`, or `right`.

| Token | Value |
| --- | --- |
| `fut` | `fut ` when the tab bar is passive; empty while it is keyboard-active |
| `session.name` | Current session name |
| `workspace.name` | Current workspace name |
| `workspace.icon` | Workspace icon from the selected preset |
| `tab.name` | Current tab name |
| `tab.index` | One-based current tab index |
| `tab.pane_count` | Number of panes in the current tab |
| `client.zoom` | Configured zoom icon while zoomed; otherwise empty |
| `client.help` | Contextual create/rename/close help while the tab bar is keyboard-active |
| `client.waiting` | `●` and this client's count of terminals with unseen blocked or completed attention across Fut; empty when zero |
| `session.waiting` | `●` and this client's count of terminals with unseen blocked or completed attention in the current session; empty when zero |

The special `{ component = "tabs" }` segment renders the focus-aware repeated tab collection. At most one may occur in a tab bar.

## Tab-item tokens

These may appear only under `ui.tab_bar.item.segments`.

| Token | Value |
| --- | --- |
| `tab.marker` | Current icon for the active tab, otherwise its one-based index |
| `tab.index` | One-based tab index |
| `tab.name` | Tab name |
| `tab.id` | Full stable tab UUID |
| `tab.closing` | Closing icon while closing; otherwise empty |
| `tab.pane_count` | Number of panes in the tab |
| `tab.icon` | Tab icon from the selected preset |
| `tab.activity` | A spinner for working, `!` for blocked, or `●` for unseen completion; empty when inactive |

Current, closing, and keyboard-selected styles are composed over every item segment.

## Workspace-row tokens

These may appear in `ui.workspace_sidebar.row.left`, `body`, `right`, or `detail`.

| Token | Value |
| --- | --- |
| `workspace.marker` | A solid block (`█`) for the active workspace, otherwise one blank cell |
| `workspace.index` | One-based workspace index |
| `workspace.name` | Workspace name |
| `workspace.id` | Full stable workspace UUID |
| `workspace.root` | Complete, stable workspace root path |
| `workspace.root_name` | Final component of the workspace root |
| `workspace.closing` | Closing icon while closing; otherwise empty |
| `workspace.tab_count` | Number of tabs in the workspace |
| `workspace.icon` | Workspace icon from the selected preset |
| `workspace.activity` | A spinner for working, `!` for blocked, or `●` for unseen completion; empty when inactive |
| `workspace.git_branch` | Current branch of the workspace root; empty outside a Git work tree or until resolved |
| `workspace.git_added` | `+N` inserted lines against `HEAD`, styled `added`; empty when none |
| `workspace.git_deleted` | `-N` deleted lines against `HEAD`, styled `deleted`; empty when none |

Current, closing, and keyboard-selected styles compose over each workspace row.

The Git tokens are resolved by bounded background `git` processes, cached per workspace root and refreshed at most every five seconds. They never block rendering and stay empty for non-Git roots, errors, or timeouts.

## Sidebar header and footer tokens

These may appear in `ui.workspace_sidebar.header` and `footer`:

| Token | Value |
| --- | --- |
| `fut` | `fut` |
| `session.name` | Current session name |
| `workspace.name` | Current workspace name |
| `workspace.icon` | Workspace icon from the selected preset |
| `sidebar.status` | Contextual controls, switching progress, or a retryable error; empty in the passive sidebar |

## Future dynamic tokens

Process titles, clocks, and custom providers are intentionally absent from the synchronous renderer. Implemented semantic activity comes from explicit reports already present in the resource snapshot. Future dynamic providers should be asynchronous, bounded, cached, explicitly trusted where executable, and publish typed values into the same pure rendering context.

## Related

- [Configuration](configuration.md)
- [Diagnostics](doctor.md)
