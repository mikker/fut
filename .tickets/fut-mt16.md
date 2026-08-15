---
id: fut-mt16
status: closed
deps: []
links: []
created: 2026-08-10T13:29:34Z
type: feature
priority: 2
assignee: Mikkel Malmberg
tags: [ui, keybindings, commands]
---
# Add temporary command surfaces with configurable keybindings

Allow trusted user-configured commands to run from Fut keybindings in temporary terminal surfaces. The motivating use case is mapping prefix+g to ~/.dotfiles/tmux/tmux.symlink/git_diff_popup.sh, matching the existing Herdr and tmux setups: open the repository diff using the focused pane's current working directory, give it the full terminal area, and restore the previous pane when the command exits.

## Design

Design this as an explicit trusted-command boundary rather than weakening pure presentation tokens or running commands during rendering. At minimum, support a temporary full-area pane/overlay command with focused-pane CWD inheritance, normal PTY input/rendering, clean exit restoration, config reload, binding collision validation, and command-palette/which-key discovery. Consider whether detached shell and sized popup variants belong in the same model, but keep the first implementation as small as possible. prefix+g currently opens the global navigator, so configuration must permit safely displacing or rebinding built-in actions.

## Acceptance Criteria

A user can configure prefix+g to launch the existing git_diff_popup.sh from the focused pane's live current directory. The command receives the full client terminal area without permanently mutating tab split topology; q/normal process exit returns to the previously focused pane and geometry. Invalid commands and spawn failures surface a bounded client error and leave the prior view usable. Custom bindings participate in uniqueness validation, config reload, command-palette search, and which-key help. Documentation clearly identifies executable command configuration as trusted and keeps rendering/presentation configuration non-executable. Automated tests cover config parsing and collisions, CWD inheritance, launch/exit restoration, and failure handling.


## Notes

**2026-08-11T09:58:21Z**

Implemented trusted configured temporary full-area PTY commands with focused foreground-process CWD inheritance, binding displacement/collision validation, config reload, command palette and which-key discovery, lifecycle/error handling, docs, tests, and manual PTY verification.

**2026-08-15T13:05:15Z**

Follow-up: moved the built-in navigator default to prefix+s and configured the local repository-diff command on prefix+g, so the motivating command no longer displaces a built-in.
