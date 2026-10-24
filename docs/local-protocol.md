# Local endpoint contract, generation 1

Local CLI, UI, metadata supervision, autostart readiness and doctor negotiate a
stable contract on the existing Unix socket. Package versions are diagnostic;
they are not compared for compatibility. The framing remains four-byte
big-endian lengths and bounded MessagePack named maps, `msgpack-map-v1`.

The first envelope contains `local_hello`:

```json
{"message":{"type":"local_hello","generation":1,"codec":"msgpack-map-v1","client_version":"0.38.0","required":["local.v1"],"optional":[],"mode":"control"}}
```

Interactive mode retains the existing `ClientMode` representation with terminal
size and optional selector. The reply is `local_welcome`, with `generation`,
`codec`, `server_version`, `capabilities`, `selected` and `extension_catalog`.
Control selection is null; interactive selection is required. The catalog is
always present. Replies echo the handshake request ID (or omit it when the
request did). Handshakes are strictly decoded, including rejection of trailing
bytes and unknown DTO fields, and validated before any terminal is leased.

`local.v1` is required and freezes the complete current local contract: all
allowlisted requests, responses and their transitive payloads, defaults, bounds,
correlation, ordering and behavior. This includes contextual targeting, agent
automation, extension reload, shutdown, nested workspace metadata, separate
Kitty images, scroll deltas and chunked screens. The exhaustive allowlists in
`src/protocol/local.rs` force an explicit decision when adding message variants.
They do not replace mode, resource-scope or other operational checks.

Unknown optional capabilities are ignored; unknown required capabilities fail.
Names, version strings and capability lists use the same bounded validation as
the remote contract. Generation, codec and capability errors use `endpoint_error`.
A rejected connection closes without changing daemon lifecycle or leasing a
terminal. Normal operations never retry using a version claimed by the peer.

## Evolution

This first step establishes the stable baseline, not a generic translation
engine or an unlimited promise to support every past release. Compatible app
releases retain generation 1 and `local.v1`, so updating a client alone need not
interrupt running terminals.

`local.v1` is required. Clients can also offer `pane-titles.v1`.
When selected, pane snapshots include `name`, or `""` for an unnamed pane.
`rename_target` accepts a `pane` selector. The contextual `rename_pane` command
renames the calling terminal's pane. The existing contextual `rename` command
still rejects `scope: pane`.

Pane names are plain text shared by clients. Names use at most 512 UTF-8 bytes
and cannot contain control characters. An empty name clears the title. Clients
still wait for both the rename acknowledgement and the corresponding resource
revision.

Without this capability, the daemon omits pane names and rejects pane rename
requests before dispatch. Clients that offer it as optional accept a welcome
without it and disable pane renaming for that connection. Legacy exact-version
connections cannot rename panes.

New features must update the capability types, selection validation, and
boundary allowlists. Listing an optional capability does not authorize new
behavior.

- Keep existing wire shapes and semantics unchanged, including fields in shared
  domain types. Rust defaults or ignored unknown fields alone do not establish
  compatibility.
- Add optional capabilities for new behavior, gate both sending and dispatch,
  and report unsupported features without rejecting otherwise useful connections.
- For changed representations, add explicit boundary adapters and fixtures for
  each supported representation; do not change the frozen baseline in place.
- Incompatible semantics without a safe adapter require a new generation.
- Keep the remote allowlists separate. A remote offer of `local.v1` must never
  grant private local operations, even though both use the same Unix socket.

## Transition from release-number locking

The legacy `hello` / `welcome` path still requires an exact package-minor
`PROTOCOL_VERSION` match, for older clients from the current release. Existing
daemons predating `local_hello` cannot negotiate this contract: they need one
explicit shutdown and restart. Ordinary operations do not silently fall back.
Explicit `daemon shutdown` retains legacy handshake / daemon-binary recovery;
`--force` remains the last-resort recovery path. The navigator's explicit unsafe
legacy mismatch escape hatch does not override negotiated generation or
capability failures.
