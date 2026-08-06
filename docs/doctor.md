---
layout: default
title: Diagnostics
description: Read-only checks for Fut's environment and configuration.
permalink: /doctor/
---

# Diagnostics

`fut doctor` performs a bounded, read-only diagnosis of the client environment:

```sh
fut doctor
fut --json doctor
fut --socket /path/to/fut.sock doctor
```

It does not start or stop a daemon, create or repair directories, remove stale sockets, open a project, spawn a terminal, enter raw mode, or change terminal state.

## Checks

Doctor reports:

- the resolved configuration source, file presence, parsing, and validation;
- `TERM`, color hints, TTY attachment, and the terminal program hint;
- whether the selected locale declares UTF-8;
- runtime-directory ownership and private permissions;
- socket type, ownership, and permissions;
- a bounded Fut protocol handshake when a safe socket exists;
- the configured icon preset and a visual glyph probe.

The current development protocol is `2` and requires an exact client/daemon match. A successful handshake reports a compatible protocol-`2` daemon; any other protocol version is incompatible.

## Nerd Fonts

Terminal processes have no reliable cross-terminal API for discovering the active font. Doctor therefore reports the active font as unknown and asks for visual verification. It does not infer support from the terminal program or claim that an installed font is selected.

Enabling `preset = "nerd_font"` makes the icon check a warning until visually verified; it does not prevent Fut from starting.

## Exit status

- `0`: the report contains no errors; informational checks and warnings are allowed.
- `1`: one or more checks are errors, such as malformed configuration, `TERM=dumb`, insecure runtime state, or an incompatible endpoint.

Diagnosed errors are part of the complete report rather than a secondary command failure.

## JSON

`fut --json doctor` writes one compact success-envelope-shaped report to stdout, even when the report's status is `error` and the process exits 1:

```json
{"version":1,"command":"doctor","result":{"status":"warning","checks":[{"id":"config","status":"ok","summary":"valid defaults; no configuration file"}]}}
```

Check statuses are `ok`, `info`, `warning`, and `error`. Details include structured paths, environment hints, protocol versions, icon glyphs, and the explicit `active_font: "unknown"` limitation where applicable.

## Related

- [Configuration](configuration.md)
- [Presentation tokens](tokens.md)
