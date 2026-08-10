---
name: changelog
description: Add a user-facing entry to CHANGELOG.md for work that just landed. Use when wrapping up a feature or fix, or when asked to record what changed.
---

# Changelog

Record what shipped while the change is fresh. Do not reconstruct the changelog from commits at release time.

1. Open the root `CHANGELOG.md`.
2. Add one bullet under `## Unreleased`.
3. Write it for a user reading release notes, not as a commit message:
   - Keep it to one concise line.
   - Describe the user-visible effect, not implementation details or file paths.
   - Skip refactors, dependencies, CI, tests, and other internal work unless users notice the result.

Match the existing tense and style. Do not add category subheadings, links, or contributor credits.

Entries remain under `Unreleased` until the release skill promotes them to a version section.
