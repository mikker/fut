---
layout: default
title: Agent activity
description: Report semantic agent activity to Fut.
---

# Agent activity

Programs running inside Fut receive scoped `FUT_SESSION_ID`, `FUT_WORKSPACE_ID`,
`FUT_TAB_ID`, `FUT_PANE_ID`, and `FUT_TERMINAL_ID` environment variables.

Report semantic state from the terminal with:

```sh
fut terminal report working
fut terminal report blocked
fut terminal report completed
fut terminal report idle
```

Completion and blocked reports create per-client attention. Use `Ctrl-b u` to list
waiting terminals and `Ctrl-b .` to jump to the next one. Viewing a terminal marks
its current attention as seen only for that client.

## Event stream

Outside tools can subscribe to Fut's state changes instead of polling:

```sh
fut events
```

Each line is versioned JSON with the complete resource snapshot — sessions,
workspaces, tabs, panes, and agent activity. The first line is the current
state; every later line is the state after a change. The stream ends when the
daemon exits.

## Pi

Install Fut's Pi extension directly from the repository:

```sh
pi install git:github.com/mikker/fut
```

The extension reports Pi's working, blocked, completed, and idle transitions when
Pi runs inside Fut.
