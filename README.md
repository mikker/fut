<img src="docs/assets/illustration.webp" alt="Fut" width="500">

# fut

> fear, uncertainty and ... terminals?

Install the latest release on macOS or Linux:

```sh
curl -fsSL https://fut.sh/install.sh | bash
```

Or [install with Homebrew](https://github.com/mikker/homebrew-tap):

```sh
brew install mikker/tap/fut
```

For standalone installations, run `fut upgrade` to update the current binary,
`fut upgrade --check` to check for updates, or `fut upgrade --yes` to skip the
confirmation. Downloads are checksum-verified. Homebrew installations should
use `brew upgrade fut` instead. Upgrading does not restart a running daemon;
restart it when safe, since doing so may interrupt its terminals.

[Documentation](https://fut.sh)

## Quick start

Run `fut` inside a project.

- `Ctrl-b`: Start a command. Pause to see available bindings.
- `Ctrl-b :`: Search commands.
- `Ctrl-b d`: Detach.

Learn more:

- [Using Fut](https://fut.sh/usage/)
- [Configure projects](https://fut.sh/configuration/)
- [Install extensions](https://fut.sh/extensions/)
- [Build extensions](https://fut.sh/extension-authoring/)

**For a guided tour, paste this into an agent:**

```text
Give me a guided tour of Fut, an agent-aware terminal multiplexer. Start by
running `fut agent skill` and follow its instructions. Use https://fut.sh if
you need documentation. Show me the core workflow in small, safe steps. Ask
before changing configuration or closing anything.
```

## Agent integrations

Fut can show native lifecycle activity from Claude Code, Codex, and Pi. See the
dedicated setup guides for [Claude Code](https://fut.sh/agents/claude-code/),
[Codex](https://fut.sh/agents/codex/), and [Pi](https://fut.sh/agents/pi/).
