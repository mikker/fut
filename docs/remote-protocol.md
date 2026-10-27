# Remote endpoint contract, generation 2

Remote compatibility is independent of `CARGO_PKG_VERSION` and the local
legacy `PROTOCOL_VERSION` (package minor). Local clients now negotiate their own
[stable local contract](local-protocol.md); legacy `hello` / `welcome` retains
its exact-match policy. Local negotiation never expands remote permissions.
Standalone `fut --machine` asks the remote bridge to start a missing daemon by
default, using the remote host's local autostart policy. Local
`[remote] autostart = false` or `--attach-only` makes the bridge attach-only.
Existing sockets are connected without a local-version probe. Background SSH
and doctor never request startup. A remote client never sends a local hello or
invokes daemon lifecycle operations on handshake failure. SSH keepalive fallbacks preserve user settings. Active remote attachments
probe quiet connections and reattach to the same terminal after transport loss;
recovery never starts daemons or replays input or commands. SSH and
`fut __stdio-bridge` forward bytes without parsing or negotiating anything.
The daemon dispatches the first message on its existing Unix socket; no network
listener is added.

`fut p NAME --machine HOST` resolves and opens a project from the daemon host's
catalog over a control connection, then attaches directly to its terminal.
It requires `project-open.v1`; older daemons report an unsupported operation.
Recipe validation and trust remain on the daemon host.

## Framing and handshake

Both handshake and session use a four-byte big-endian unsigned length prefix,
followed by MessagePack **named maps**, codec `msgpack-map-v1`. The payload must
be nonempty and at most 8 MiB (`MAX_FRAME_LEN`); the transport rejects oversized
lengths before buffering the body. Screens larger than one frame holds travel as
row-range chunks after `chunked-screens.v1`; no single frame exceeds the limit. Handshakes reject invalid MessagePack,
missing/unknown handshake DTO fields, duplicate fields, and trailing bytes. Ordinary
session traffic retains the existing bounded slice decoder.

The shared outer envelope ignores unknown envelope fields. Messages retain the
envelope `{request_id?: UUID, message: {...}}`; replies echo
the request ID, and unsolicited updates omit it. The current remote client uses
a fresh ID for the handshake and verifies its echo. The first message is:

```json
{"message":{"type":"remote_hello","generation":2,"codec":"msgpack-map-v1","client_version":"0.35.0","required":["metadata.v2"],"optional":["health.v1","control-alerts.v1","extension-catalog.v1"],"mode":"control"}}
```

`mode` uses the frozen `ClientMode` payload: `control`, or
`{"interactive":{"size":{"c":80,"r":24},"selector":...}}`.
Interactive offers must require both `metadata.v2` and `interactive.v1` and
provide a valid terminal size. Control offers must require `metadata.v2`.
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

<div class="wide-table" markdown="1">

| Capability | Generation-2 methods and messages | When omitted |
| --- | --- | --- |
| `project-open.v1` | `open_project` with a catalog name; `location_opened` | Named project opening is unavailable |
| `workspace-order.v1` | `move_workspace { workspace_id, position }`; correlated `workspace_moved { resource_revision }` | Workspace reordering is unavailable; update/restart guidance is shown |
| `metadata.v2` | `list_resources`, `watch_resources`; resource snapshots/changes, program-status records and client presence | Required for the navigator and attachment |
| `nested-workspaces.v1` | Optional `parent_workspace_id` fields in workspace snapshots | Workspaces remain flat and generation-2 metadata payloads are unchanged |
| `interactive.v1` | Input/key/paste/mouse, viewport reset/refresh, copy mode, terminal/split resize, target selection, workspace/tab/pane creation and split, rename/close, agent acknowledgement; their replies, full/delta screens and terminal exit | Required for attachment |
| `large-screens.v1` | Interactive screens up to 150,000 visible cells; Kitty images arrive once per generation in `kitty_image` messages, and screen images carry an empty `d` as a reference to those pixels | Screens embed image pixels and stay within 50,000 visible cells; larger hello or resize sizes are rejected |
| `chunked-screens.v1` | Requires `large-screens.v1`. Interactive screens up to 1,048,576 visible cells (65,535 columns or rows at most). A full screen, copy-mode screen, or delta whose rows hold more than 150,000 cells is sent as `screen_begin` (`begin`: `n`, the row count that follows, and `f`, the message it stands for with its `c` cells or `d` rows empty), one or more `screen_rows` (`revision`, `rows` as delta rows: a full screen's rows in order from 0, a delta's in ascending order), and `screen_end` (`revision`, and the request ID of a correlated copy-mode screen). The frames of one revision are contiguous; the receiver presents the screen only at `screen_end`, discards a partial screen when a newer one begins, and asks for a fresh screen after a malformed sequence | Screens stay within 150,000 visible cells; larger hello or resize sizes are rejected, and a larger terminal shared with another client is shown as its top-left corner until this peer's resize shrinks it |
| `scroll-deltas.v1` | Screen deltas may carry `x`, a row shift `{t, e, n}` applied before their rows: rows `t` up to (not including) `e` rotate up by `n` rows, or down when `n` is negative, so scrolling sends only the rows it exposed | Deltas carry only replacement rows; scrolling that changes most rows sends a full screen |
| `terminal-colors.v1` | Interactive `terminal_colors` updates with host foreground, background, and 16 ANSI palette colors | Child applications retain the emulator’s last known colors; host color updates are not sent |
| `attachments.v1` | Interactive `attachment_begin`, `attachment_chunk`, `attachment_commit`, `attachment_cancel`; upload a PNG and paste its daemon-local path into the original terminal | Clipboard image paste is unavailable |
| `health.v1` | `ping` / `pong` | Machine verification probes health when available and otherwise uses the successful handshake |
| `alerts.v1` | Interactive `watch_alerts`, `acknowledge_alerts`, alert snapshots and acknowledgement | No attached-terminal bell subscription or acknowledgement |
| `control-alerts.v1` | Lease-free control `watch_alerts`, `acknowledge_alerts`, alert snapshots and acknowledgement | Background metadata retains no bell summary; agent summaries remain in resources |
| `extension-catalog.v1` | Catalog in the welcome; control `get_extension_catalog` and interactive catalog-change notifications | No extension declarations, styles or command listings; related local overrides are ignored |
| `configured-commands.v1` | Control `command_defaults`, dedicated control `command_start`, `command_operation`; `command_reply` | Remote configured commands are unavailable |

</div>

Interactive resource operations execute **on the daemon's machine**. Paths,
programs and argv in these methods are endpoint-local. Remote resource roots,
PIDs, extension paths and tokens never authorize local filesystem or process
operations. Configuration reload and client hooks remain local-only.
Clipboard and rendering run locally; terminal links allow only HTTP/HTTPS.
Pane links and ordinary terminal/navigation/layout operations remain usable.
An extension's Fut package requirement applies to the hosting daemon, not the
local renderer; remote catalog validation retains structural/fingerprint checks
without imposing the local package version on remote declarations.
Workspace reordering is a session edit like renaming: it is permitted remotely
only with `workspace-order.v1`, in either control or interactive mode. Interactive
targets must belong to the attached session. Positions are one-based among
siblings; parentage is preserved and subtree display remains grouped. Unknown,
closing and out-of-range targets fail atomically. Existing metadata subscribers
receive the new order through normal resource changes, even without this optional
capability. No new fields or events are added to the frozen metadata payload.
No capability in generation 2 permits daemon shutdown, configuration mutation,
agent reporting, token publication, or private control/automation methods.

### Configured commands

Each command uses a separate SSH control connection with the normal handshake
and `configured-commands.v1` negotiated. `command_start` supplies a command spec,
target terminal ID, terminal size, and optional form values. The daemon resolves
the target's live ancestry. A preceding `command_defaults` request resolves form
defaults from remote configuration without launching the command, returning
`Defaults { values }` with its request ID. For extension commands only `{ id, command }` and
an explicit `extension_args_override` decision are authoritative client inputs:
the remote catalog and remote UI configuration supply executable, roots,
execution settings, and config. If the override is false, remote configured args
win; if true, supplied args (including an empty list) win. Generic commands from
the client's config deliberately execute their supplied program/argv remotely,
using the target terminal's remote foreground cwd. The client preserves `~` and
`~/` program paths; the daemon expands them against its own user's home.

All command replies use `command_reply`. Popup starts return `Ready` with the
start request ID, then asynchronous `Screen` and `Done` with no request ID.
Background starts have no screen or `Ready`: they return `Done` with the start
request ID. `Done` includes the exit code, optional activated pane, and optional
failure output. Input, key input, paste, resize, mouse, clipboard completion and
copy cancellation go through `command_operation`, require request IDs, and get
matching `Ack` (or `Error`). Mouse coordinates include the content rectangle
`[x, y, width, height]`; clipboard requests return text and a copy request ID in
the Ack and are completed with `FinishClipboard`. Clipboard access stays local.
The connection is dedicated after start: other control methods cannot interleave.
Disconnect or detach closes the popup or cancels the background process.

### Image attachment uploads

`attachments.v1` is optional and offered by interactive clients. Upload messages
are ordered, fire-and-forget envelopes without request IDs:

1. `attachment_begin` supplies a fresh `transfer_id`, explicit `terminal_id`, and
   declared byte `size` (8 bytes to 20 MiB).
2. `attachment_chunk` supplies the same transfer ID and 1–64 KiB of `bytes`.
3. `attachment_commit` supplies the transfer ID. The daemon verifies the exact
   size and PNG signature, writes a private temporary file, and pastes its quoted
   daemon-local path into the original terminal without submitting it.

`attachment_cancel`, a replacement begin, errors, and disconnect discard pending
state. Each connection has at most one pending transfer. Clients cancel work
when switching endpoints and never replay uploads after reconnect. Switching
tabs on the same endpoint does not change the target.

Commit success uses the existing `command_completed` message with `paste`;
upload failures use an uncorrelated operational error with code
`attachment_error`. No destination path is accepted from the client. Pending
and retained attachments share a daemon-wide 256 MiB budget, with at least
64 KiB charged per file. Completed files remain until daemon shutdown rather
than being evicted while an application may still use their paths. Abrupt
termination may leave private files in the OS temporary directory.

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
difference between `ssh -t host fut` and `fut --machine host` are documented in
[Using Fut](usage.md#attach-to-a-remote-machine).

## Frozen semantics and evolution

Generation 2 deliberately reuses `Envelope`, `ClientMode`, `SelectedView`,
`ExtensionCatalog`, and the allowlisted `ClientMessage` / `ServerMessage`
payloads and their transitive resource, screen, split, copy, alert and domain
types. Their wire names, enum cases, field representations, defaults, bounds,
request correlation, event ordering and behavior are a **frozen remote
contract**. In generation 2, `PaneSnapshot.activity` is a `ProgramStatus` with
OSC 7501 records; `metadata.v2` freezes that transitive shape. A package minor
bump does not change the contract. This is not permission to expose every local
message.

Generation 1 and `metadata.v1` froze the earlier agent-activity shape at commit
`f77344c`. Generation 2 intentionally has no generation-1 adapter: peers offering
generation 1 are rejected as incompatible rather than receiving a lossy status
translation.

Changes to shared local types must preserve the selected remote contract. New
behavior must have a new optional capability and be emitted only after selection;
changing existing required semantics, representations, or introducing enum cases
an existing peer cannot decode requires a new capability payload/adapter or a
new remote generation. Do not send new shared fields merely because local serde
currently ignores unknown fields. The handshake DTOs themselves are frozen;
future generations with additional handshake fields need separately decoded
DTOs rather than extending generation 2 in place. Extension points are the
bounded capability lists. Keep wire fixtures and tests
for older offers, optional omission, rejection isolation, and the local protocol
when evolving this contract. Multiple simultaneous generations or codecs are not
implemented.
