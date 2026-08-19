---
layout: default
title: Claude Code integration
description: Report Claude Code lifecycle activity to Fut.
permalink: /agents/claude-code/
---

# Claude Code integration

Install Fut first, then add Fut's marketplace and lifecycle plugin:

```sh
claude plugin marketplace add mikker/fut
claude plugin install fut@fut-integrations
```

Restart Claude Code and launch it inside Fut. Run `/hooks` in Claude Code to
confirm that the Fut handlers are loaded with source `Plugin`.

The adapter reports idle, working, blocked, completed, and exited activity. It
does not control prompts, permissions, tools, terminal layout, or worktrees,
and it remains inactive when Claude Code is running outside Fut.

The `fut` binary must be on `PATH`. Managed Claude Code policy can disable
plugin hooks; use `/hooks` or the plugin manager's Errors view if activity does
not appear.

See [Agent activity](../agents.md) for the sidebar, notifications, and agent
automation commands.
