---
layout: default
title: Pi integration
description: Report Pi lifecycle activity to Fut.
permalink: /agents/pi/
---

# Pi integration

Install Fut's Pi extension from GitHub:

```sh
pi install git:github.com/mikker/fut
```

Restart Pi after installation, then launch it inside Fut. The extension reports
Pi's idle, working, blocked, completed, and exited activity. Exiting Pi removes
that terminal from the Agents sidebar. The package also gives Pi a Fut skill so
it can inspect and control sessions, panes, terminals, and agents without
changing visual focus.

The integration is a one-way lifecycle adapter. It does not create or change
layouts, submit prompts, read terminal output, or coordinate agents, and it
remains inactive when Pi is running outside Fut. The `fut` binary must be on
`PATH`.

See [Agent activity](../agents.md) for the sidebar, notifications, and agent
automation commands.
