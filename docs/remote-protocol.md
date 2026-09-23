# Remote endpoint contract, generation 1

Remote compatibility is independent of `CARGO_PKG_VERSION` and the local
`PROTOCOL_VERSION` (package minor). Local `hello` / `welcome`, exact protocol
checks and the explicit local mismatch escape hatch keep their existing behavior.
Standalone `fut --remote` asks the remote bridge to start a missing daemon by
default, using the remote host's local autostart policy. Local
`[remote] autostart = false` or `--attach-only` makes the bridge attach-only.
Existing sockets are connected without a local-version probe. Background SSH
and doctor never request startup. A remote client never sends a local hello or
invokes daemon lifecycle operations on handshake failure. SSH and
`fut __stdio-bridge` forward bytes without parsing or negotiating anything.
The daemon dispatches the first message on its existing Unix socket; no network
listener is added.

## Framing and handshake

Both handshake and session use a four-byte big-endian unsigned length prefix,
followed by MessagePack **named maps**, codec `msgpack-map-v1`. The payload must
be nonempty and at most 8 MiB (`MAX_FRAME_LEN`); the transport rejects oversized
lengths before buffering the body. Handshakes reject invalid MessagePack,
missing/unknown handshake DTO fields, duplicate fields, and trailing bytes. Ordinary
session traffic retains the existing bounded slice decoder.

The shared outer envelope ignores unknown envelope fields. Messages retain the
envelope `{request_id?: UUID, message: {...}}`; replies echo
the request ID, and unsolicited updates omit it. The current remote client uses
a fresh ID for the handshake and verifies its echo. The first message is:

```json
{"message":{"type":"remote_hello","generation":1,"codec":"msgpack-map-v1","client_version":"0.22.0","required":["metadata.v1"],"optional":["health.v1","control-alerts.v1","extension-catalog.v1"],"mode":"control"}}
```

`mode` uses the frozen `ClientMode` payload: `control`, or
`{"interactive":{"size":{"columns":80,"rows":24},"selector":...}}`.
Interactive offers must require both `metadata.v1` and `interactive.v1` and
provide a valid terminal size. Control offers must require `metadata.v1`.
Selectors, UUIDs and terminal dimensions retain their existing encoding.

The daemon replies with `remote_welcome`, containing `generation`, `codec`,
`server_version`, `capabilities`, `selected`, and `extension_catalog`.
`selected` is a `SelectedView` for interactive mode and null for control.
`extension_catalog` is present as a catalog only when its capability is selected;
otherwise it is null. These fields are not the local `welcome` DTO.

Capability names and the codec are 1–64 ASCII bytes from `[a-z0-9.-]`.
There are at most 32 names total across required and optional offers, and at most
32 selections. Duplicates, including overlaps between required and optional,
are invalid. Unknown required names fail; unknown optional names are ignored.
The daemon selects the supported intersection. Clients reject duplicate,
unknown, unoffered, or missing required selections. Package version strings are
diagnostics only: 1–128 ASCII bytes from `[A-Za-z0-9.+_-]`, never compared for
compatibility or interpreted as commands, terminal controls, or paths.

Errors use `{"type":"endpoint_error","error":{"code":...}}`:
`incompatible_generation` (with numeric `client` and `server`),
`unsupported_codec`, `missing_required_capability`, `invalid_handshake`, or
`method_not_negotiated`. The Rust `EndpointError` remains available through
client error chains. Negotiation rejection closes that connection before leasing
a terminal. Unreadable frames may receive an uncorrelated `invalid_handshake`.
Timeout, EOF, and I/O failure also end only the connection. Selection failures
use the existing operational `error` response. Nothing requests shutdown,
replacement, upgrade, or fallback. Another endpoint and existing clients keep
running.

## Negotiated methods and locality

The allowlists in `src/protocol/remote.rs` are authoritative. Capability
selection permits methods; existing control/interactive mode and resource-scope
checks still apply. `detach`, `detached`, operational errors and endpoint errors
are common to every negotiated connection.

| Capability | Generation-1 methods and messages | When omitted |
| --- | --- | --- |
| `metadata.v1` | `list_resources`, `watch_resources`; resource snapshots/changes and client presence | Required for the navigator and attachment |
| `nested-workspaces.v1` | Optional `parent_workspace_id` fields in workspace snapshots | Workspaces remain flat and generation-1 metadata payloads are unchanged |
| `interactive.v1` | Input/key/paste/mouse, viewport reset/refresh, copy mode, terminal/split resize, target selection, workspace/tab/pane creation and split, rename/close, agent acknowledgement; their replies, full/delta screens and terminal exit | Required for attachment |
| `health.v1` | `ping` / `pong` | Machine verification probes health when available and otherwise uses the successful handshake |
| `alerts.v1` | Interactive `watch_alerts`, `acknowledge_alerts`, alert snapshots and acknowledgement | No attached-terminal bell subscription or acknowledgement |
| `control-alerts.v1` | Lease-free control `watch_alerts`, `acknowledge_alerts`, alert snapshots and acknowledgement | Background metadata retains no bell summary; agent summaries remain in resources |
| `extension-catalog.v1` | Catalog in the welcome; control `get_extension_catalog` and interactive catalog-change notifications | No extension declarations, styles or command listings; related local overrides are ignored |

Interactive resource operations execute **on the daemon's machine**. Paths,
programs and argv in these methods are endpoint-local. Remote resource roots,
PIDs, extension paths and tokens never authorize local filesystem or process
operations. The current UI blocks project opening, configuration reload,
configured/extension commands and client hooks regardless of capabilities.
Clipboard and rendering run locally; terminal links allow only HTTP/HTTPS.
Pane links and ordinary terminal/navigation/layout operations remain usable.
An extension's Fut package requirement applies to the hosting daemon, not the
local renderer; remote catalog validation retains structural/fingerprint checks
without imposing the local package version on remote declarations.
No capability in generation 1 permits daemon shutdown, configuration mutation,
agent reporting, token publication, or private control/automation methods.

## Endpoint supervision

The local UI supervises Local and every enabled saved SSH profile independently.
Background SSH runs non-interactively with strict host-key checking and carries a
control connection only: resource, presence, agent, alert, and extension metadata,
never terminal screen frames. Each connection instance has a client-side
generation, so late events from a replaced socket cannot overwrite its successor.
Disconnects retain the last complete snapshots as stale and revoke authority to
send input. Transient failures retry with capped exponential backoff; host-key,
authentication, installation, and compatibility failures require interactive
repair. Health uses correlated `ping`/`pong`, or correlated `list_resources` as
the fallback when `health.v1` is absent.

Use `fut doctor` to validate OpenSSH, the private saved-profile catalog, and a
bounded non-interactive compatibility handshake for enabled profiles. Doctor
does not repair hosts or mutate either endpoint. Operational setup and the
difference between `ssh -t host fut` and `fut --remote host` are documented in
[Using Fut](usage.md#attach-to-a-remote-machine).

## Frozen semantics and evolution

Generation 1 deliberately reuses `Envelope`, `ClientMode`, `SelectedView`,
`ExtensionCatalog`, and the allowlisted `ClientMessage` / `ServerMessage`
payloads and their transitive resource, screen, split, copy, alert and domain
types. Their wire names, enum cases, field representations, defaults, bounds,
request correlation, event ordering and behavior are a **frozen remote
contract**, based on the payloads at commit `f77344c`. A package minor bump does
not change that contract. This is not permission to expose every local message.

Changes to shared local types must preserve the selected remote contract. New
behavior must have a new optional capability and be emitted only after selection;
changing existing required semantics, representations, or introducing enum cases
an existing peer cannot decode requires a new capability payload/adapter or a
new remote generation. Do not send new shared fields merely because local serde
currently ignores unknown fields. The handshake DTOs themselves are frozen;
future generations with additional handshake fields need separately decoded
DTOs rather than extending generation 1 in place. Extension points are the
bounded capability lists. Keep wire fixtures and tests
for older offers, optional omission, rejection isolation, and the local protocol
when evolving this contract. Multiple generations or codecs are not implemented
until needed.
