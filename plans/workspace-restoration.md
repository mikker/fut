# Saved layouts and workspace restoration

Status: implemented on Powerman (Linux x86_64) and rebased onto the upstream
0.41 release. Installation still requires explicit approval. The installed
Powerman CLI reports 0.41.0 and lacks `layout`. Planetmobile's platform and
connectivity remain unverified. No installed binary has been replaced and
neither live daemon has been restarted.

## Goal

After a daemon restart, recreate the user's project arrangement with fresh
programs in the original directories. Preserve placement, not running program
sessions; users can resume their agents themselves.

## Agreed behavior

### Scope and storage

- Save every open project hosted by a daemon, with its workspaces, tabs, and
  panes. “Windows” means Fut tabs, not OS terminal windows.
- Preserve project, workspace, and tab order; nested workspace relationships;
  explicit names; pane placement; and split proportions.
- Fit split proportions to the current terminal size rather than requiring
  the previous screen dimensions.
- Remember focus from the most recently active client. Zoom and sidebar
  appearance remain UI settings rather than saved-layout state.
- Keep machine-local saved layouts separate from project recipes. Automatic
  saving must not overwrite hand-written project configuration or its trust.
- Keep one latest checkpoint per project initially, not named snapshot history.
- Provide explicit export into project configuration.

### Lifecycle

- Automatic checkpointing and daemon-start restoration are opt-in.
- Also provide explicit Save and Restore operations.
- Capture the arrangement before graceful daemon shutdown starts terminating
  programs. Upgrading the executable does not itself terminate programs.
- Intentionally closing a project session, workspace, tab, or pane removes it
  from future restoration. Detaching does not.
- Refuse restoration into an already-live project without changing its layout
  or terminating or duplicating its programs.

### Programs and directories

- Capture the live foreground executable and arguments when reliably
  identifiable, not merely the pane's original shell command.
- Programs start fresh. Do not automatically replay agent resume identifiers
  or prompts, terminal input history, pipelines, or background jobs.
- Review and approve restart commands before enabling automatic execution.
  Approval covers the executable, arguments, and working directory. A change
  to any of these needs review; geometry and naming changes do not.
- Continue checkpointing unapproved command changes without interrupting work.
  Unapproved or unidentified programs restore as shells with a warning.
- Restore each pane's live working directory and each workspace's root.
- Do not copy running process environments or temporary shell exports. Use
  fresh startup environment and currently approved project settings.
- Saved topology replaces recipe bootstrapping; do not also create the recipe's
  resources or rerun its startup commands or hooks. Extension controls remain
  available for subsequent user actions.
- If a recipe becomes untrusted, restore its affected project shell-only until
  reviewed. Never silently continue using stale recipe approval.
- A missing path or program must not block unrelated restoration. Retain failed
  entries for retry, report failures, and provide a shell fallback where
  possible. Never run a saved program in a substituted directory.

### Remote hosts

- Each daemon saves and restores only projects on its own host.
- Users can explicitly request remote restoration from their local Fut client;
  running a host-local command through SSH is a fallback, not the only workflow.
- Add a narrowly scoped negotiated remote restore capability. The host uses
  its own saved layout and existing approvals; the client does not supply
  replacement commands, directories, configuration, or trust decisions.
- An explicit remote restore can start a missing daemon under the existing
  remote autostart policy. Respect `--attach-only`; never stop, upgrade, or
  replace an existing daemon.
- Background monitoring, transport reconnects, and restoring the local daemon
  must not send remote restore requests or automatically replay them.
- Explicitly enabled host-side startup restoration remains a host setting.
  Coordinate startup and requested restoration so programs cannot start twice.
- Missing capability reports unsupported without changing the host. Recipe
  trust and persistence configuration mutation remain host-local operations.

## Initial controls and export

- Provide CLI commands under `fut layout` plus command-palette actions.
- Restore can target one saved project or all saved projects. Never replace
  a live project; report it as already live and leave its programs untouched.
- Client workflow: `fut --machine HOST layout restore PROJECT`, with
  `--all` for the host's saved projects. Restore occurs on that host, then the
  client attaches to the restored project's remembered pane or shows the
  navigator when restoring all.
- Offer the same explicit remote action in the client. Discovering saved work
  must not itself authorize restarting its programs.
- Host-local CLI remains usable over SSH, including when the client or daemon
  does not support the new remote capability.
- Export writes a reviewable recipe to stdout or a new file. Never silently
  overwrite `.fut/project.toml`; exported recipes use normal recipe trust.
- Checkpointing, approval review, and export must be usable without forcing a
  disruptive daemon restart or replacing a live project.

## Implementation and compatibility

- `docs/projects.md` documents trusted project bootstrapping and recipe trust.
- `src/project_definition.rs` represents workspace roots and parents, pane
  titles, and exact rational split proportions, including explicit export.
- `src/splits.rs` already represents complete split trees with rational sizes.
- `src/daemon/mod.rs` observes foreground PID, name, display command, and cwd.
  The display command from `ps` is not a lossless argument array and must not
  be parsed as one for automatic execution.
- `src/daemon/recipe.rs` prepares and launches recipe terminals and environment.
  `LayoutRuntime` retains recipe digest/IDs and original source roots separately
  from terminal runtime entries; current settings are matched without bootstrap.
- `src/client/navigation.rs` maintains navigation history in each client;
  restoring focus requires explicit persistence rather than guessing from
  daemon resource ordering.
- `src/state_file.rs` provides bounded private state files, locks, atomic
  replacement, and filesystem synchronization for reuse. Isolate checkpoints
  between daemon sockets, including test and custom endpoints.
- Do not let shutdown cleanup or fallback shells overwrite the last intended
  arrangement and restart commands. Retain failed entries for explicit retry.
- `docs/local-protocol.md` and `docs/remote-protocol.md` freeze wire contracts.
  Any new wire behavior needs negotiated capabilities and compatibility tests.
- `src/upgrade.rs` deliberately leaves running daemons untouched.
- A newer local CLI can save a negotiated older daemon read-only before an
  explicitly approved restart. Remote Save never attempts local process inspection.
- Explicit restore-only startup suppresses both initial recipes and automatic
  restore-all before a single-project request. Baseline clients retain their
  frozen default-selection semantics.
- Returning programs retain their shell on exit; direct programs remain direct.

## Verification and rollout

- Real isolated-daemon coverage includes multi-project geometry/order/names,
  missing paths/programs and retries, closure versus detach, opt-in periodic
  checkpoints, abrupt exits, duplicate protection, recipe trust/provenance,
  export round-trips, older-daemon adaptation, and SSH Save/List/Restore.
- Unit tests cover private checkpoint validation/approval, native argv and
  executable aliases, agent argument sanitization, and stable recipe-ID matching.
- Full locked/all-features Rust suite: 969 library, 199 E2E, and 7 upgrade tests
  passed (1,175 total), including 14 saved-layout E2E tests. Remote coverage includes
  actual missing-daemon startup, restore-only mode, and `--attach-only` refusal.
- Pi/Claude/Codex adapters, run extension unit tests, worktree smoke checks, and
  all 17 run extension lifecycle smoke checks passed. Formatting, whitespace,
  and both changed JSON schemas' syntax checks passed.
- The Linux x86_64 release binary is built at `target/release/fut`. macOS native
  process inspection has not been runtime-tested here.
- Strict Clippy reports an existing `large_enum_variant` in the unchanged
  `src/client/remote_command.rs:45`. All-target/all-features Clippy passes with
  only that lint allowed and all other warnings denied.
- See `docs/layouts.md` for controls, boundaries, and staged migration.

## Installation handoff

### Powerman

The installed `/home/maik/.local/bin/fut` is untouched and reports 0.41.0. This
checkout is now based on upstream 0.41, preserving its changes alongside the
clipboard and saved-layout features. The installed CLI does not provide `layout`;
its version does not establish the running daemon's version or negotiated
capabilities. Installation and daemon interruption still require explicit approval.

Staged Save uses negotiated capabilities, not package-version comparison. From
a shell outside Fut, with the original socket/state settings, stage and inspect
the build without replacing anything:

```sh
cd /home/maik/develop/src/fut
new_fut=$(mktemp "$HOME/.local/bin/fut-stage.XXXXXX") &&
install -m 755 target/release/fut "$new_fut" &&
"$new_fut" layout save &&
"$new_fut" layout review
```

Verify every expected project is listed and approve only reviewed fingerprints:

```sh
"$new_fut" layout list
"$new_fut" layout approve FINGERPRINT
"$new_fut" layout enable  # Optional automatic recovery after the upgrade
```

If work continues before cutover, Save and Review again: the older daemon cannot
make periodic or final checkpoints. Only after receiving explicit approval
to replace the binary and interrupt all programs,
in that same outside-Fut shell:

```sh
mv -- "$new_fut" "$HOME/.local/bin/fut" &&
"$HOME/.local/bin/fut" daemon shutdown &&
"$HOME/.local/bin/fut" layout restore --all
```

The rename replaces the executable atomically without altering its running
process. Shutdown is the separate, disruptive step; restored programs are fresh.

### Planetmobile

SSH currently fails with `Could not resolve hostname planetmobile`. Verify
`uname -sm`, its installed executable, and its original socket/state settings
before choosing an artifact. Do not assume Powerman's Linux binary will run
there; use a matching native build of these changes. Then run the staged
host-local Save/Review/Approve sequence before any approved installation or
shutdown. Preserve any package-manager ownership of the installed executable.

Once both endpoints support the feature, restore through the local client:

```sh
fut --machine planetmobile layout restore --all
fut --machine powerman layout restore --all
```
