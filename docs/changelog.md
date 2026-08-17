---
layout: default
title: Changelog
description: What changed in Fut.
permalink: /changelog/
---

# Changelog

## Unreleased

- Added composable left and right sidebars with contextual menus and section dividers.
- Shared agent notification read state across clients and external observers.
- Added alternate configuration directories, close controls, command aliases, and safer nested-client handling.
- Added extension palette commands and configurable temporary-command sizes.
- Added acknowledged retirement for disposable workspaces.

## 0.6

- Refined the global navigator with scoped filters and clearer hierarchy.
- Moved global navigation to `Ctrl-b s`; `Ctrl-b Ctrl-s` now toggles sessions.

## 0.5

- Added Kitty graphics, OSC 8 hyperlinks, explicit local extensions, and presentation tokens.
- Added live workspace location, branch, and diff presentation.
- Improved naming, navigator filters, resource colors, and multi-daemon support.

## 0.4

- Added fallback Codex activity detection when no lifecycle plugin is active.
- Added native text selection and copying by dragging in terminal panes.
- Replaced bottom-row notices with compact corner toasts.
- Improved fuzzy navigation and command search.
- Added a configurable workspace sidebar status rail.
- Added trusted keybinding commands in temporary full-terminal PTYs.
- Added process-aware names for unnamed tabs.
- Added support for attaching multiple terminal windows to one session.
- Fixed slow backward paging in development builds.
- Fixed stale workspace Git metadata while the UI is idle.

## 0.3

- Added pane splitting, terminal automation, lifecycle-aware agent commands, and structured resource discovery.
- Added configurable keybindings and atomic configuration reloads.
- Added mouse resizing for pane and workspace-sidebar dividers.
- Added fuzzy global navigation and unread agent-completion navigation.
- Improved terminal rendering performance and reliability under heavy output.

## 0.2

- Added global navigation, tabs, workspaces, splits, mouse input, copy mode, and agent activity indicators.
- Switched the client/daemon protocol to MessagePack and substantially improved rendering performance.
- Added resource-listing and event-stream APIs for automation.

## 0.1

- Released persistent, configurable terminal multiplexing for macOS.
- Automated GitHub builds, releases, and Homebrew distribution.

For the complete release notes, see the [repository changelog](https://github.com/mikker/fut/blob/main/CHANGELOG.md).
