---
id: fut-do0r
status: open
deps: []
links: []
created: 2026-08-17T12:07:44Z
type: feature
priority: 2
assignee: Mikkel Malmberg
tags: [extensions, processes, ui, config]
---
# Add a managed run extension with workspace status and logs

Create a generalized Fut `run` extension for explicitly starting, restarting, stopping, and inspecting one project-defined long-running command per workspace. Dogfood it in Tuna with `just run` and bind restart to prefix+r.

Project and global configuration use the same namespaced table instead of extension-specific files:

```toml
# .fut/config.toml or the global Fut config
[extension.run]
command = ["just", "run"]
```

Project-local values override global defaults. Project configuration is inert until the user explicitly invokes a run command; loading or entering a project must never auto-start it.

## Design

Fut seams:
- Give palette/extension commands focused FUT_SESSION_ID, FUT_WORKSPACE_ID, FUT_TAB_ID, FUT_PANE_ID, and FUT_TERMINAL_ID context, alongside the existing FUT_BIN/FUT_SOCKET extension environment.
- Add non-interactive extension commands that run asynchronously without opening a temporary PTY, remain silent on success, and surface bounded launch/exit failures as a toast. Restart and stop use this mode; logs and edit-logs remain interactive.
- Add a namespaced `[extension.<id>]` configuration map to global config and `./.fut/config.toml`. Resolve global defaults then project-local overrides for the focused workspace, reject configuration for unknown extensions, bound the data, retain provenance for diagnostics, and expose the resolved table to that extension command. Inner-table validation belongs to the extension. This is explicitly invoked project configuration, not an auto-running workspace recipe.

Run extension:
- Add `examples/extensions/run` with restart, stop, logs, and edit-logs commands plus workspace-created/closed hooks.
- Scope manager state and logs by workspace ID so parallel worktrees and logical workspaces are independent.
- A supervisor owns an exclusive lock and the child process group, captures combined stdout/stderr, records command/cwd/PIDs/timestamps/exit details, and prevents repeated-hotkey races or stale-PID kills. Restart sends graceful termination, escalates after a short timeout, then starts a fresh supervisor. Workspace close terminates its runner. Client detach does not.
- Append run separators to a stable log and rotate at a bounded size. `run:logs` opens `less -R +F`; Ctrl-C leaves follow mode, F resumes, v invokes VISUAL/EDITOR, and q returns to Fut. `run:edit-logs` opens the same file directly through VISUAL then EDITOR.
- Declare four workspace presentation tokens and populate exactly one logical state: configured/unstarted = pause (muted), running = play (added/green), clean exit = stop (muted), killed or nonzero exit = cross (error). A workspace without run configuration publishes no indicator.

Tuna dogfood:
- Add `.fut/config.toml` with `[extension.run] command = ["just", "run"]`.
- Configure the extension path globally, bind `run:restart` to lowercase `r` (uppercase R remains reload config), and add the four styled tokens to the tab bar right/notification lane. No Tuna application code or user-facing Tuna changelog entry is needed.

## Acceptance Criteria

- Global and `.fut/config.toml` use the same `[extension.run]` shape, with project values overriding global defaults for the focused workspace.
- Merely loading project config never executes its command.
- prefix+r starts a configured workspace runner; pressing it again terminates the complete previous process group and starts exactly one replacement without showing a temporary surface.
- Separate Fut workspaces have independent process, state, token, and log ownership.
- The tab bar displays pause before first run, green play while running, stop after exit 0, and an error cross after explicit termination, signal, or nonzero exit. Unconfigured workspaces show nothing.
- Stop terminates without restarting; closing a workspace cleans up its runner; detaching a client leaves it running.
- Logs update live in a full-screen temporary viewer, remain scrollable, open in VISUAL/EDITOR from the viewer, and also open through a separate command.
- Locks/generation checks cover rapid restart, stale state, and natural-exit-versus-restart races without killing an unrelated PID.
- Fut unit/integration tests cover config precedence and bounds, command context, non-interactive dispatch, token transitions, process-group restart/stop, workspace isolation, and checked-in extension smoke behavior.
- Fut docs and Unreleased changelog describe extension configuration, non-interactive commands, runtime context, status tokens, controls, trust boundary, and the run example.
- `cargo test` and a debug build pass, followed by manual Tuna dogfooding of start, rapid restart, stop, failure, logs/editor, detach/reattach, and workspace closure.

