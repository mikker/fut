---
layout: default
title: Documentation
description: Agent-aware terminal multiplexing.
permalink: /
---

# fear, uncertainty and ... terminals?

> **TL;DR:** Fut is a persistent, agent-aware terminal multiplexer. Install it,
> run `fut` in a project, and use `Ctrl-b` for commands. Start with
> [Using Fut](usage.md).

<pre class="resource-tree"><code>$ fut <span class="cursor">█</span>                     (just one)
└── session[name]           (this is the project)
    └── workspace[name]     (a user-defined context)
        └── tab             (this is the agent/vim/you do you)
            └── pane        (same as above)</code></pre>

Latest release: **{{ site.version }}**

```sh
curl -fsSL https://fut.sh/install.sh | bash
```

Installs the latest macOS or Linux release to `~/.local/bin`. Set
`FUT_INSTALL_DIR` to choose another location. Release binaries support arm64
and x86_64. The installer needs `curl`, `tar`, and either `sha256sum` or
`shasum`; it prints a PATH reminder when needed.

Re-run the installer to upgrade, set `FUT_VERSION` to install a specific tag,
or remove the installed `fut` binary to uninstall.

Or install on macOS with Homebrew:

```sh
brew install mikker/tap/fut
```

## Docs

- [Using Fut](usage.md)
- [Agent activity](agents.md)
- [Configuration](configuration.md)
- [Extensions](extensions.md)
- [Presentation tokens](tokens.md)
- [Diagnostics](doctor.md)
- [Changelog](changelog.md)

[Source](https://github.com/mikker/fut)

![Fut illustration]({{ '/assets/illustration.webp' | relative_url }})
{: .illustration }
