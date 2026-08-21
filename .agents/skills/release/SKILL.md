---
name: release
description: Release a new Fut version. Use when the user asks to release, publish, or cut a new version.
---

# Release Fut

Keep this mechanical and brief.

1. On `main`, review the `CHANGELOG.md` `Unreleased` entries. Remove project website, installer, distribution, release-process, and other non-app items unless they changed the installed app's behavior. Consolidate related entries, turn the remainder into a concise section for the next `0.x` version, and leave a fresh empty `Unreleased` section. Commit that change.
2. Ensure `main` is clean and is not behind or diverged from `origin/main`. The release script pushes local commits when needed.
3. Run:

```sh
mise run release
```

Do not manually repeat the release script's work. It runs checks, updates the Cargo version, commits it, tags the release, atomically pushes `main` and the tag, then waits for GitHub to build the binaries, publish the release, and update Homebrew. The command succeeds only after the complete workflow succeeds.

Report the released version and the GitHub Actions URL printed by the command. If the command fails, report the error and stop; do not improvise a partial release.
