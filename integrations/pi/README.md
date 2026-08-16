# Fut lifecycle adapter for Pi

This first-party Pi extension reports lifecycle state for the Pi session running
in a Fut terminal. It is a one-way adapter: it does not create or change
layouts, submit prompts, read terminal output, or coordinate agents.

## Install

This repository is a Pi package. Add the checkout to Pi using Pi's package
installer:

```sh
pi install /absolute/path/to/fut
```

Local paths are loaded in place. To register the package only for the current
project instead of in your user settings, add `-l`:

```sh
pi install -l /absolute/path/to/fut
```

The root `package.json` registers `integrations/pi/fut.ts` as the package's Pi
extension. Restart Pi after installing it.

The adapter is inert unless both `FUT_SOCKET` and `FUT_TERMINAL_ID` are present,
as they are for Pi started inside Fut. The `fut` binary must be on `PATH` and
provide:

```sh
fut agent report STATE --terminal-id ID --source SOURCE \
  [--agent-session-id ID] [--turn-id ID]
```

Reports are serialized in event order. Each CLI call has a two-second timeout,
and command failures are swallowed so lifecycle reporting cannot fail a Pi
event handler.

## Event mapping

| Fut state | Authoritative Pi evidence |
| --- | --- |
| `idle` | `session_start` |
| `working` | `agent_start`; end of an `ask_user` tool while Pi is still active |
| `blocked` | start of an `ask_user` tool execution |
| `completed` | `agent_settled`, after retries, compaction retries, and queued continuations are exhausted |
| `exited` | `session_shutdown` |

Every report includes `source=pi`, the explicit Fut terminal ID, and Pi's
`SessionManager.getSessionId()` value as the agent session ID. Pi exposes a
per-run numeric turn index only on turn start/end events, not a stable turn ID
on lifecycle and tool events, so the adapter does not invent or forward one.

## Limitations

- Blocked/resumed reporting recognizes the native `ask_user` tool name. Other
  tools that implement their own user interaction do not provide authoritative
  ask-user lifecycle evidence and are not inferred as blocked.
- If Pi is terminated without delivering `session_shutdown`, Fut retains the
  last reported state until terminal exit handling clears it.
- A missing, incompatible, or unresponsive Fut CLI can delay an individual
  report by at most two seconds; the failure is ignored and later reports still
  run in order.

## Test

The tests use Node's built-in TypeScript stripping and test runner:

```sh
node --test integrations/pi/tests/fut.test.ts
```
