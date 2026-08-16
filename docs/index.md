---
layout: default
title: Documentation
description: Agent-aware terminal multiplexing.
permalink: /
---

# fear, uncertainty and ... terminals?

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
`FUT_INSTALL_DIR` to choose another location.

Or install on macOS with Homebrew:

```sh
brew install mikker/tap/fut
```

## Docs

- [Agent activity](agents.md)
- [Configuration](configuration.md)
- [Extensions](extensions.md)
- [Presentation tokens](tokens.md)
- [Diagnostics](doctor.md)
- [Changelog](changelog.md)

[Source](https://github.com/mikker/fut)

![Fut illustration]({{ '/assets/illustration.webp' | relative_url }})
{: .illustration }
