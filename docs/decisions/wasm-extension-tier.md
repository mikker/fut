---
layout: default
title: Decision record — capability-sandboxed WebAssembly extensions
description: Why Fut is deferring, but not rejecting, a Wasmtime Component Model extension tier.
permalink: /decisions/wasm-extension-tier/
---

# Capability-sandboxed WebAssembly extension tier

- Status: **deferred**
- Date: 2026-08-19
- Scope: ticket `de-4sp3`
- Runtime evaluated: Wasmtime and `wasmtime-wasi` 47.0.3, the latest published
  crates at evaluation time

## Decision

Do not add Wasmtime, WASI, a WIT SDK, or a WebAssembly execution path to Fut
now. Keep manifest API v1 and the managed store centered on supervised,
explicitly trusted subprocesses.

WebAssembly remains the preferred candidate if Fut later needs to execute
portable extensions that are not trusted with the user's account. It is
deferred rather than rejected because Wasmtime's Component Model can express a
small, typed, capability-oriented contract and can deny ambient I/O. It should
not ship merely to let an author choose another implementation language: the
current process contract is already language-neutral.

The deciding evidence is:

1. None of Fut's concrete extensions currently needs a lower-trust portable
   tier. The examples that do useful operating-system work fundamentally need
   process, filesystem, TTY, or device access. The examples that fit a pure
   sandbox are conformance demonstrations rather than unmet user demand.
2. A useful first sandbox can cover lifecycle events, resolved config, bounded
   logging, and declared presentation-token updates. It should not initially
   expose arbitrary process execution or an interactive command surface,
   because either would erase the security distinction.
3. The runtime cost is material for Fut's single-binary distribution. The
   isolated arm64 macOS probe below linked a minimal Component Model host at
   12.8 MB and the same host with WASI 0.2 at 16.8 MB. These are not claimed as
   additive Fut deltas, but they rule out treating the runtime as free.
4. The security tier creates continuing work: hostile-input tests, resource
   policy, Wasmtime security updates, permission UX, four-platform release
   validation, and a versioned WIT contract. That work is only justified by
   concrete lower-trust extensions.

## Current Fut contract and actual workloads

Manifest API v1 already separates logical declarations from implementation
language:

- packages have a strict `fut-extension.toml`, semantic package/Fut versions,
  exact `commands`, `hooks`, and `presentation_tokens` declarations;
- commands and hooks are direct argv, never shell-interpolated;
- workspace and client lifecycle events are versioned JSON;
- resolved config is bounded, namespaced JSON;
- token names and scopes are declared before publication;
- hook input, retained output, queue length, and runtime are bounded;
- managed packages are copied without running build/install scripts, hashed,
  made read-only, installed disabled, and re-verified before load; and
- reload validates a complete generation and swaps it atomically.

Those are the logical contracts a future runtime should reuse. They do not
currently provide containment: an enabled executable inherits the user's OS
authority, environment, network, and access to Fut's same-user control socket.
The manifest capabilities describe cooperation surfaces, not permissions.

The checked-in workloads show where a sandbox would and would not help:

| Extension | What it actually needs | Sandbox fit |
| --- | --- | --- |
| `wt` | interactive input, `wt`, Git/worktree files, `fut open`, and an optional arbitrary post-open process | Poor. Granting process execution and workspace write access would collapse the tier into trusted code. |
| `run` | project-configured process spawning, process groups/signals, locks, mutable state, rotating logs, `less`/editor TTYs, and token updates | Poor. This is explicitly a trusted supervisor. |
| `ghostty-title` | inherited terminal identity and writes to `/dev/tty` | Poor. Device/TTY access is intentionally ambient and client-local. |
| `example-workspace-status` | lifecycle event to one declared token update | Good technically, but it is an example rather than evidence of user demand. |
| `rust-status` | event/config parsing, optional arbitrary log-file append, and token update | The pure status path fits; arbitrary `log_path` does not. It is a conformance fixture. |

The useful initial sandbox is consequently narrower than manifest API v1: it
is a pure, non-interactive lifecycle transformer. Existing process extensions
remain necessary even if that tier is later added.

## Threat model

### Trusted process tier (current)

The user reviews a package and explicitly enables it. Fut contains accidental
failures with process boundaries, bounded hook I/O, a five-second hook timeout,
and atomic registry reload, but it does not defend the user's account from a
malicious executable. Such an extension can read credentials, make network
requests, spawn or signal processes, alter files, and issue any command allowed
by the same-user Fut socket.

Package hashing and pinned Git provenance protect identity and rollback. They
do not establish author authenticity and do not make executable bytes safe.

### Capability-sandbox tier (possible future)

Assume the component bytes are malicious. Trust Fut's Rust host code,
Wasmtime/Cranelift, the OS, and Fut's package-selection decision. The guest may
only compute and use explicitly linked WIT interfaces; WebAssembly itself has
no raw syscall access. The tier should protect confidentiality and integrity of
host files, environment, network, processes, devices, and the Fut socket, while
bounding CPU, memory, host allocations, outputs, and wall time.

This tier would not protect against:

- a Wasmtime, compiler, or Fut host-function escape;
- information intentionally present in an event or resolved configuration;
- misleading but syntactically safe text in the extension's own namespaced UI
  token;
- denial of service outside enforced compile/run/resource limits; or
- a malicious package persuading the user to grant a future filesystem or
  network capability.

The Wasmtime host and every linked WASI interface become security-critical.
Unsafe native host callbacks and package-supplied precompiled code are outside
the acceptable trusted computing base.

## Runtime and interface choice if revisited

Use WebAssembly Components and WIT, not a private core-Wasm ABI. WIT describes
both imports and exports, carries typed records/variants/strings, supports
versioned package names, and Wasmtime can generate host bindings from a world.
WASI 0.2 is the current stable WASI release and is Component Model based. A
Fut-specific component need not receive the broad `wasi:cli/command` world.

The first world should be synchronous and import nothing. Pass events and
config into one exported function and receive bounded effects. This is simpler
and safer than giving a guest a re-entrant mutation API, makes one hook result
atomic, and avoids `wasmtime-wasi` unless real guest toolchains require a small
allowlist. An illustrative WIT shape is:

```wit
package fut:extension@1.0.0;

interface types {
  enum log-level { debug, info, warn, error }
  enum current-target { session, workspace }
  enum event-kind {
    client-attached,
    client-session-changed,
    client-detached,
    workspace-created,
    workspace-renamed,
    workspace-closed,
  }

  record session { id: string, name: string }
  record workspace { id: string, name: string, root: string }
  record event {
    kind: event-kind,
    resource-revision: option<u64>,
    session: session,
    previous-session: option<session>,
    workspace: option<workspace>,
    previous-name: option<string>,
  }
  record config-sources { global: bool, workspace: bool }
  record invocation {
    event: event,
    config-json: string,
    config-sources: config-sources,
  }

  record token-effect {
    name: string,
    target: current-target,
    value: option<string>, // none clears the value
  }
  record log-entry { level: log-level, message: string }
  record effects {
    tokens: list<token-effect>,
    logs: list<log-entry>,
  }
  record guest-error { message: string }
}

interface lifecycle {
  use types.{invocation, effects, guest-error};
  handle: func(input: invocation) -> result<effects, guest-error>;
}

world lifecycle-extension {
  export lifecycle;
}
```

The sketch deliberately retains config as JSON. Fut's current config values
are recursive and already have a documented 16 KiB serialized bound; inventing
a second typed config language would create migration work without improving
containment. Event data is typed because its shape is small and host-owned.

Returned token names must be present in the existing manifest declaration,
the declared scope must match `current-target`, and the target must come from
the current invocation. Fut applies the existing 1 KiB token-value and control
character rules. Log count, per-message bytes, and total bytes need host-side
bounds. Effects are applied only after a successful call; a trap, timeout, or
guest error applies none. That atomic behavior is intentionally stronger than
the process tier and must be part of the new API contract.

An interactive or arbitrary command interface is omitted. If demand later
exists for sandboxed palette actions, add a separately versioned, non-TTY
`action` export whose only effects are typed host operations. Do not expose
`spawn(argv)`, a shell, the Fut socket, or a passthrough `fut` CLI capability.

## Execution, cancellation, and resource policy

A future implementation should compile each validated component once per
registry generation, then create a fresh `Store` and instance per event. Fresh
instances prevent hidden mutable state from crossing events and preserve the
current sequential queue semantics. Wasm work must not execute on Tokio's core
reactor threads.

Runtime execution needs all of these controls; a wall-clock timeout alone is
not sufficient:

- Enable epoch interruption. Drive the engine epoch from a low-frequency host
  timer and use a per-store deadline/callback that traps when the invocation's
  cancellation flag or wall deadline is set. Cancelling the Rust future alone
  does not interrupt synchronous Wasm.
- Enable fuel as a deterministic instruction-work ceiling. Do not choose an
  exact fuel budget from the empty probe; calibrate it with representative
  Rust and at least one non-Rust guest. Fuel accounts for Wasm instructions,
  not expensive host work.
- Preserve Fut's five-second lifecycle timeout as the outer compatibility
  ceiling, while using fuel and host quotas to stop abusive work earlier.
- Attach `StoreLimits` for linear-memory bytes, tables/elements, instances, and
  memories, and set a bounded Wasm stack. Wasmtime's defaults do not impose a
  per-memory byte limit and allow very high instance/table/memory counts.
- Bound simultaneous invocations with the existing sequential event workers.
  Limit returned list lengths, strings, aggregate host allocation, log volume,
  and token operations before applying effects.
- Treat traps, out-of-fuel, epoch interruption, allocation failure, invalid
  canonical values, and resource-limit failures as diagnostic hook failures;
  they must not alter daemon state or prevent later hooks.

The isolated harness used 32 MiB per memory, 8 instances, 4 memories, 4 tables,
and 10,000 table elements only to prove the APIs compose. Those are starting
test values, not accepted production defaults. A real prototype must compile
representative guest components, inspect `Component::resources_required` where
available, then choose the smallest limits with explicit headroom. It must also
attack-test oversized canonical-ABI strings/lists because lifting guest values
can allocate host memory before application-level validation.

Epochs are a coarse wall-time mechanism; Wasmtime documents that a bulk memory
operation is checked at its start rather than preempted internally. The memory
limit is therefore also part of the cancellation bound. Fuel is deterministic
but has greater execution overhead, so both mechanisms serve different jobs.

Compilation is another resource boundary. A malicious, maximum-size component
is compiler input during validation/reload, before store fuel applies. Compile
off the async reactor, disable parallel compilation, preserve the prior
generation on failure, and measure worst-case cancellation/memory behavior.
An in-process timeout cannot forcibly stop a compiling thread. If that cannot
be bounded satisfactorily, compilation and execution belong in a supervised
runner process rather than the daemon.

## Filesystem, network, and WASI policy

The initial world should link no WASI interfaces. It receives no environment,
args, stdio, clocks, random source, filesystem, sockets, or name lookup. It
receives neither `FUT_BIN` nor `FUT_SOCKET`. Components importing anything
outside the exact Fut world fail validation before activation.

If guest-toolchain evidence requires WASI 0.2, link only the required
interfaces. `WasiCtxBuilder` defaults are a useful baseline—closed stdin,
discarded output, no environment/args/preopens, all socket addresses denied,
and name lookup denied—but Fut should configure those choices explicitly and
test them. Do not call `inherit_env`, `inherit_stdio`, `preopened_dir`, or
`inherit_network` by convenience.

Future access policy, in order of preference:

1. Package assets: expose a bounded `read-asset(relative-path)` host operation
   or a read-only package preopen. Reject absolute paths and traversal and
   retain current symlink-free managed-package validation.
2. Workspace files: require a separate user grant per extension and workspace,
   default read-only, and display it independently from logical manifest
   capabilities. A workspace lifecycle event does not imply filesystem access.
3. Network: keep denied. If demanded, prefer a bounded host HTTP operation with
   explicit schemes/hosts/ports, redirect/DNS/IP policy, response-size and time
   limits. Do not expose raw sockets or blanket inherited networking.
4. Process, TTY, devices, and the Fut control socket: never grant these in this
   tier. Use the trusted-process tier instead.

## Package and API coexistence

There should remain one extension ecosystem and one managed store. A future
manifest API v2 could make the execution kind explicit while retaining IDs,
versions, Fut requirements, lifecycle names, config namespace, token
declarations, content digest, provenance, enablement, atomic reload, and store
limits:

```toml
api_version = 2
id = "review-status"
version = "1.0.0"
fut = ">=0.9.0, <1.0.0"
capabilities = ["hooks", "presentation_tokens"]

[runtime]
kind = "wasm-component"
path = "./extension.wasm"
world = "fut:extension/lifecycle-extension@1.0.0"

[hooks]
"workspace.created" = { handler = "handle" }
"workspace.renamed" = { handler = "handle" }

[[presentation_tokens]]
name = "status"
scope = "workspace"
```

API v1 remains implicitly `trusted-process` and keeps its argv tables exactly
as documented. API v2 validation rejects `commands` for the initial Wasm
runtime, checks that the component is a regular package file, type-checks its
world and imports without executing it, and includes runtime kind/component
digest/world in the registry fingerprint and daemon catalog. `list`, `show`,
validation errors, and enablement UX must say either “trusted process” or
“sandboxed component” rather than overloading today's capability list.

The current store index does not need a parallel package type: it already
records ID, version, normalized content digest, immutable path, provenance,
and enabled state. The present 16 MiB per-file and 64 MiB package limits can
initially bound the portable component and assets, subject to representative
guest measurements.

Do not accept a package-supplied Wasmtime serialized component as an
optimization. Deserializing precompiled artifacts is unsafe for untrusted
bytes, and artifacts must match the engine configuration and target, defeating
the portable-package goal. Compiling portable `.wasm` in Fut requires a
compiler such as Cranelift. A Fut-owned cache could later be keyed by package
digest, Wasmtime version, engine configuration, and target and protected as
trusted local state, but it adds code, invalidation, and security work and was
not included in this evaluation.

## Cost evidence

### Published crate and platform data

As of 2026-08-19, crates.io/docs.rs published Wasmtime 47.0.3 and
`wasmtime-wasi` 47.0.3 (released 2026-07-31), both with MSRV 1.94. Fut uses Rust
1.95, so the current toolchain is compatible.

| Published item | Wasmtime 47.0.3 | `wasmtime-wasi` 47.0.3 |
| --- | ---: | ---: |
| Unpacked crate source reported by docs.rs | 4.54 MB | 1.09 MB |
| Downloaded `.crate` archive in this evaluation | 1,036,332 bytes | 224,100 bytes |
| docs.rs average successful build duration for this release | 1m 51s | 58s |

The docs.rs durations are service measurements of each crate, not additive Fut
CI predictions. The archives exclude transitive dependencies. In the isolated
Cargo graph below, the custom Component host resolved 113 normal packages and
the WASI p2 host 189; many overlap Fut's existing graph, so these too are not
incremental counts.

Fut releases native binaries for arm64 and x86-64 on both macOS and Linux.
Wasmtime classifies x86-64 macOS/Linux, Cranelift, the Component Model, and the
Rust embedding API as tier 1. It classifies arm64 macOS/Linux as tier 2 because
continuous fuzzing is missing. The platform matrix therefore works in
principle, but Fut's two arm64 artifacts would adopt a weaker upstream support
tier for an untrusted-code boundary.

Wasmtime makes a major release monthly. Non-LTS releases receive two months of
support; versions divisible by 12 are LTS for 24 months, with security fixes
backported to supported releases. Shipping creates an explicit, recurring
security-update obligation. A future implementation should select a
then-supported LTS after remeasurement rather than freeze this record's 47.x
probe.

### Isolated link/startup probe

This was a disposable project under `/tmp`, not a change to Fut. Environment:
Apple arm64, macOS/Darwin 27.0.0, Rust/Cargo 1.95.0, default Cargo release
profile, Wasmtime 47.0.3. The custom host disabled Wasmtime defaults and enabled
only `anyhow`, `cranelift`, `runtime`, `component-model`, and `std`. The WASI
host additionally used `wasmtime-wasi` with defaults disabled and `p2` enabled.
Both configured fuel, epochs, and `StoreLimits`, compiled a 139-byte component
exporting an empty `run`, instantiated it, and called it.

| arm64 macOS artifact | Unstripped bytes | `tar -czf` bytes |
| --- | ---: | ---: |
| Empty Rust control binary | 428,064 | not recorded |
| Minimal custom Component host | 12,825,152 | 4,248,245 |
| Minimal Component + WASI p2 host | 16,819,872 | 5,491,366 |
| Current Fut release binary, for scale | 15,774,576 | 5,821,477 |

The standalone custom host is 12,397,088 bytes larger than its empty control;
WASI p2 adds 3,994,720 bytes to that standalone host. These are reproducible
isolated link results, not an estimate that Fut would grow by those exact
amounts: linker deduplication with existing Fut dependencies, target, compiler,
features, LTO/strip settings, and real host code will change the integrated
delta. Every binary in the table used the default, unstripped Cargo release
profile.

After one warm-up, 30 fresh host processes were sampled. Timers started inside
`main`, so they exclude OS process-launch time and represent warm filesystem/OS
caches:

| Host | Engine median (min–max) | Compile median (min–max) | Instantiate + empty call median (min–max) |
| --- | ---: | ---: | ---: |
| Custom Component | 0.053 ms (0.051–0.066) | 0.784 ms (0.752–0.949) | 0.029 ms (0.026–0.048) |
| Component + WASI p2 | 0.055 ms (0.053–0.066) | 0.775 ms (0.757–0.919) | 0.133 ms (0.123–0.156) |

One warm `/usr/bin/time -l` sample reported 8,060,928 bytes maximum RSS for the
custom host and 8,781,824 bytes for the WASI host. The guest does no useful
work, has no imports, and allocates no application memory. These numbers only
show that tiny warm invocations are cheap; they do not predict realistic guest
compile time, cold start, peak memory, or Fut startup. The first observed runs
after builds compiled in roughly 14–26 ms, but cache state was uncontrolled, so
that range is deliberately not treated as a cold-start estimate.

The practical conclusion is limited but clear: steady-state invocation overhead
is unlikely to block small hooks, while binary/build/platform/security cost is
large enough to require demand. A shipping prototype must repeat integrated
measurements on all four release targets with realistic Rust and non-Rust
components, cold and warm reloads, and p50/p95 distributions.

## Supervised subprocess alternatives

Continuing with subprocesses is the right answer for present workloads. Fut can
incrementally improve process groups, kill escalation, optional command
timeouts, environment documentation, diagnostics, and per-command resource
limits without claiming that those controls sandbox native code. The `run`
example demonstrates why long-lived commands need opt-in supervision rather
than one global timeout.

OS-native containment of arbitrary executables is not a portable replacement:
macOS and Linux expose different sandbox, syscall, namespace, and resource
facilities, and a broadly useful grant for process execution/workspace access
would still expose the user's data. Use OS controls as defense-in-depth for
trusted processes if justified, never as the documented lower-trust contract.

If demand appears but adding 12+ MB of isolated runtime footprint to every Fut
binary remains unacceptable, evaluate an optional `fut-extension-runner`
subprocess that embeds Wasmtime. Fut would supervise it and exchange typed,
bounded invocations/effects over a private protocol. This keeps runtime cost
out of installations that never enable sandboxed packages and isolates runtime
crashes/compile stalls from the daemon, at the cost of another four-platform
artifact, version negotiation, installation/upgrade UX, and IPC. Do not build
that sidecar before the same demand triggers are met.

## Revisit and ship gates

Reopen this decision when at least one demand trigger is evidenced:

- two independently useful proposed extensions fit events + config +
  token/log effects without filesystem, network, process, TTY, or arbitrary
  Fut control; or
- one credible distribution/security use case is blocked specifically because
  users cannot trust a native package with their account; or
- repeated per-platform native packaging is preventing adoption of a concrete
  extension, and one portable component is demonstrated with at least two guest
  language toolchains.

Do not reopen it solely for implementation-language preference.

Before changing the recommendation to ship, require all of the following:

1. Pin a supported Wasmtime LTS compatible with Fut's toolchain and assign
   ownership for prompt security patching.
2. Freeze a versioned WIT world and manifest migration after conformance tests
   from Rust and at least one non-Rust guest toolchain.
3. Prove denial of undeclared imports, environment, filesystem, network, raw
   sockets, processes, TTY/devices, and the Fut socket with hostile fixtures.
4. Prove infinite-loop, bulk-memory, deep-stack, memory/table growth,
   oversized canonical value, output/log flood, timeout, cancellation,
   shutdown, trap, and reload-rollback behavior.
5. Measure realistic integrated binary/archive delta, clean build time, cold
   and warm startup/reload, compile time, instance time, and peak memory on all
   four Fut release targets. Set budgets before implementation and meet them.
6. Decide embedded versus optional runner from those measurements; do not use
   package-supplied precompiled artifacts.
7. Make requested versus granted permissions inspectable, default-denied, and
   distinct from today's logical manifest capabilities.

## Prototype disposition

No prototype or runtime dependency is retained. The measurement project and
generated component were isolated under `/tmp` and removed after recording the
environment, dependency features, fixture size, method, and results above.
Fut's `Cargo.toml`, `Cargo.lock`, source, tests, and trusted-process behavior are
unchanged by this decision.

## Sources

Primary sources accessed 2026-08-19:

- [Wasmtime 47.0.3 crate metadata and feature graph](https://docs.rs/crate/wasmtime/47.0.3)
- [`wasmtime-wasi` 47.0.3 crate metadata and feature graph](https://docs.rs/crate/wasmtime-wasi/47.0.3)
- [Wasmtime security model](https://docs.wasmtime.dev/security.html)
- [Wasmtime platform support tiers](https://docs.wasmtime.dev/stability-tiers.html)
- [Wasmtime release and LTS policy](https://docs.wasmtime.dev/stability-release.html)
- [Wasmtime interruption mechanisms](https://docs.wasmtime.dev/examples-interrupting-wasm.html)
- [`Config` fuel, epoch, stack, and memory controls](https://docs.rs/wasmtime/47.0.3/wasmtime/struct.Config.html)
- [`StoreLimitsBuilder` resource limits](https://docs.rs/wasmtime/47.0.3/wasmtime/struct.StoreLimitsBuilder.html)
- [Component compilation, serialization, and resources](https://docs.rs/wasmtime/47.0.3/wasmtime/component/struct.Component.html)
- [Precompiled artifact compatibility and safety](https://docs.wasmtime.dev/examples-pre-compiling-wasm.html)
- [Minimal embedding and compiler-size tradeoffs](https://docs.wasmtime.dev/examples-minimal.html)
- [`WasiCtxBuilder` default-deny details](https://docs.rs/wasmtime-wasi/47.0.3/wasmtime_wasi/struct.WasiCtxBuilder.html)
- [WASI 0.2 interfaces and Component Model foundation](https://wasi.dev/releases/wasi-p2)
- [WIT packages, worlds, types, and versioning](https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md)
- [Component Model worlds](https://component-model.bytecodealliance.org/design/worlds.html)
