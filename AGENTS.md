# Project notes

This project uses a CLI ticket system for task management. Run `tk help` when you need to use it.

## Changelog

- `CHANGELOG.md` is user-facing. Only note changes users will notice; omit internal work.
- Keep entries brief and update `Unreleased` as user-facing tasks finish.
- Before releasing, consolidate related entries, move them under the new version, and start a fresh `Unreleased` section.

## Release

- From a clean, synchronized `main`, run `mise run release`.
- It tests, advances the `0.x` version, tags and pushes. GitHub builds both macOS binaries, publishes the release, and updates `mikker/homebrew-tap`.
