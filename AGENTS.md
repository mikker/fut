# Project notes

This project uses a CLI ticket system for task management. Run `tk help` when you need to use it.

## Changelog

- `CHANGELOG.md` covers user-visible changes to the Fut app itself. Omit project website, installer, distribution, CI, release-process, and other repository-maintenance changes unless they alter the installed app's behavior.
- Keep entries brief and update `Unreleased` as user-facing tasks finish.
- Before releasing, review `Unreleased`, prune anything outside that scope, consolidate related entries, move them under the new version, and start a fresh `Unreleased` section.

## Release

- From a clean, synchronized `main`, run `mise run release`.
- It tests, advances the `0.x` version, tags and pushes. GitHub builds both macOS binaries, publishes the release, and updates `mikker/homebrew-tap`.
