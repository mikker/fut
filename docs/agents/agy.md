---
layout: default
title: Antigravity integration
description: Report Antigravity lifecycle activity to Fut.
permalink: /agents/agy/
---

# Antigravity integration

Install Fut first, then install the lifecycle plugin via Antigravity's plugin manager:

```sh
agy plugin install https://github.com/mikker/fut/tree/main/integrations/agy
```

Or for local development:

```sh
agy plugin install ./integrations/agy
```

Restart Antigravity and launch it inside Fut.

The adapter reports working, blocked, and completed activity. It does not
control prompts, permissions, tools, terminal layout, or worktrees, and it
remains inactive when Antigravity is running outside Fut.

The `fut` binary must be on `PATH`.

See [Agent activity](../agents.md) for the sidebar, notifications, and agent
automation commands.
