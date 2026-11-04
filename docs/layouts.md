---
layout: default
title: Saved layouts
description: Save and restore every project on a Fut host, with fresh programs.
permalink: /layouts/
---

# Saved layouts

Save a daemon's complete working setup: every project session, workspace, tab,
pane, directory, explicit name, nested workspace, ordering, and split proportion.
An unrelated repository placed inside a project stays in that project; changing
a pane's directory does not move it into another project.

Programs restart **fresh**. Fut does not restore process memory, terminal
scrollback, shell history, background jobs, or agent conversations. Known agent
resume identifiers and initial prompts are removed rather than replayed.

## Save, review, and restore

```sh
fut layout save
fut layout list
fut layout review
fut layout approve FINGERPRINT   # Copy a reviewed fingerprint from review
fut layout restore PROJECT
# Or restore every saved project:
fut layout restore --all
```

Save captures every live project on this daemon, not just the selected project.
List shows project names, resource counts, and commands awaiting approval.
Restore accepts an unambiguous saved project name. A specific-project restore
attaches to its remembered pane; `--all` opens the navigator. Add `--background`
to restore without attaching. `--json` also avoids attachment.

Review prints exact executable paths, argument arrays, directories, and approval
fingerprints. Approval covers all three: changing a program, argument, or cwd
requires another approval. `fut layout approve all` approves every currently
listed command; use it only after reviewing the entire list.

Unapproved or unidentifiable programs become shells. Fut never treats `ps`
display text, arbitrary shell scripts, or pipelines as restart commands. Programs
that previously returned to a shell retain that behavior; top-level programs
remain top-level programs.

A live project is skipped without merging its layout, closing its panes, or
starting duplicate programs. Missing directories or executables do not prevent
other projects from restoring. Fut opens fallback shells and retains the
intended command and directory for a later restart. It never runs a saved
program in a substituted directory. Starting a new program in a fallback shell
and saving replaces that pane's previous intent.

**Save all project layouts** and **Restore saved project layouts** are also
available in the command palette. They operate on the attached host. Neither has a default keybinding;
use `save_layout` and `restore_layouts` under `[ui.bindings]` to add bindings.

## Optional automatic recovery

```sh
fut layout enable
fut layout disable
```

Automatic recovery is off by default. Enable saves immediately, checkpoints
about every two seconds, and restores saved projects when that daemon next
starts. Graceful shutdown captures before destroying terminals. An abrupt exit
can lose changes since the last successful checkpoint.

Detach leaves the setup intact. Intentionally closing resources removes them
from subsequent checkpoints. A partial restore preserves dormant saved projects
for later restoration. Disable stops automatic saving and startup restoration
without deleting the last checkpoint or changing running programs. Explicit
Save and Restore remain available.

Explicitly restoring one project starts a missing daemon in restore-only mode:
it does not first restore every other project or bootstrap a default recipe.

## Remote hosts

```sh
fut --machine powerman layout save
fut --machine powerman layout list
fut --machine powerman layout restore PROJECT
fut --machine powerman layout restore --all
fut --machine powerman --attach-only layout restore --all --background
```

The checkpoint, paths, programs, and approvals belong to the **remote host**.
Restoring your local daemon does not restore remote daemons. Remote restoration
can start a missing daemon when normal remote autostart permits it;
`--attach-only` and `[remote] autostart = false` prevent startup. Reconnects and
background monitoring never send or replay Restore.

Both endpoints must negotiate `layout-restoration.v1`. The remote capability
allows Save, List, and Restore only. Review, approval, enable/disable, export,
and recipe trust remain host-local; run those commands on the host, for example
through an ordinary SSH shell. A remote client cannot supply replacement
commands or approve saved programs.

## Recipes and storage

Checkpoints are machine-local state, separate from `.fut/project.toml`. Fut
keeps one latest checkpoint per project, not named history. Files are private,
versioned, size-bounded, locked, and atomically replaced under
`$XDG_STATE_HOME/fut` (normally `~/.local/state/fut`). The filename includes a
hash of the absolute daemon socket path. Use the same `--socket`, runtime path,
and state directory when saving and restoring; another socket has independent
checkpoints. Save prints the state path.

Commands can contain sensitive arguments, so treat checkpoints and exported
recipes as private. Process environments are not captured. Restarts use the
new daemon's startup environment and currently trusted recipe settings, with
host-owned `FUT_*` identity applied last.

Restoration recreates saved topology without replaying recipe startup commands
or creation hooks. It retains the recipe's original source root even when
workspaces point at other repositories. If recipe contents change, per-pane
settings are matched only through stable workspace, tab, and pane IDs. An
unmatched pane, invalid configuration, or revoked recipe trust produces a
reported shell fallback rather than using stale or another pane's settings.

Export deliberately creates a separate, reviewable recipe:

```sh
fut layout export PROJECT
fut layout export PROJECT --output restored-project.toml
```

Files are created privately and existing files are never overwritten. Export
preserves workspace roots/parents, pane titles, and split ratios. A layout that
exceeds recipe size or resource/argument limits cannot be exported. Review the
output before installing it as `.fut/project.toml`; normal recipe trust still
applies. Export does not copy recipe environment or extension settings and does
not approve pending restart commands.

## Migrate a running older daemon safely

Build or obtain the new binary separately first. Check its source/release against
the installed version before promotion; negotiated protocol compatibility does
not guarantee that a differently based build preserves every installed feature.
Use that **staged binary** on each daemon's own host, with the original socket/state
settings:

```sh
/path/to/new/fut layout save
/path/to/new/fut layout review
/path/to/new/fut layout approve FINGERPRINT
/path/to/new/fut layout enable       # Optional automatic recovery
```

A negotiated older local daemon can be saved through a read-only adapter. It
uses resource listings, health information, and host-local process inspection;
it does not attach, replace, stop, or restart the daemon. Displayed automatic
names are retained as explicit names; unavailable focus defaults to the first
pane. The adapter cannot recover unavailable recipe provenance. Daemons older
than stable local negotiation cannot use this adapter. Remote Save cannot use
host-local process inspection; stage and run Save on that host instead.

If work continues before cutover, Save and Review again: an older daemon cannot
make periodic or final checkpoints. Only after checking the saved projects and
approvals, replace the installed binary and schedule an **explicitly approved**
daemon shutdown from a shell outside Fut. Replacing the executable alone does not restart the daemon.
Shutdown interrupts all of that daemon's programs. Then run the new binary's
`layout restore --all`, or use the local client's `--machine HOST layout
restore --all` after updating the remote host. Never use a live working daemon
as an installation test.
