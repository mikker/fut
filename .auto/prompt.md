# Autoresearch: Render pipeline performance

## Objective

Reduce real production CPU work in Fut's terminal rendering pipeline, guided by
the broad workloads documented in `PERF.md`. Optimize VT feed/snapshot capture,
daemon diff/serialization, client decode/materialization, and client rendering
without changing observable terminal output or responsiveness.

The benchmark score intentionally combines plain, styled, TUI, and dense
workloads across feed, encode, decode, and clone operations. Treat it as a
directional proxy for the 300x65 four-pane CPU budget, not as the product goal.
Periodically validate meaningful wins with `mise run perf:e2e`; do not trade
typing/control latency or correctness for benchmark throughput.

## Metrics

- **Primary**: `render_ns` (ns, lower is better) — sum of Criterion means for
  eleven representative production operations spanning four workload shapes.
- **Secondary**: `feed_ns`, `wire_ns`, and `wall_s`; inspect Criterion output
  and `target/criterion/**/new/estimates.json` to localize changes.

## How to Run

`./.auto/measure.sh` — runs the complete render microbenchmark and emits
structured `METRIC` lines. Criterion uses a short warmup/measurement window;
repeat borderline results because host scheduling noise is real.

Correctness checks run automatically through `.auto/checks.sh` after a passing
benchmark. Run `mise run perf:e2e` before and after substantial pipeline changes
and before considering the work finished.

## Files in Scope

- `src/terminal/ghostty.rs` — VT parsing and full-grid snapshot construction.
- `src/terminal/runtime.rs` — PTY ingestion, batching, pacing, and publication.
- `src/daemon/mod.rs` — snapshot fan-out, row diffing, and wire messages.
- `src/protocol.rs` — production MessagePack encoding/decoding.
- `src/domain.rs` — screen/cell data structures where a simpler representation
  can improve production work without weakening validation.
- `src/client/mod.rs` — delta application, frame gating, and screen rendering.
- Other `src/client/*.rs` files only when profiling identifies material chrome
  or layout work in the normal draw path.
- `Cargo.toml`/`Cargo.lock` only if a production dependency change is strongly
  justified; prefer no new dependencies.
- `PERF.md` for brief, user-relevant validated results only.

## Off Limits

- `.auto/measure.sh`, `.auto/checks.sh`, and the metric definition after the
  baseline is recorded.
- `benches/render.rs`, `examples/perf_client.rs`, and `scripts/perf/**`.
- Criterion configuration, workload sizes/content, or compiler flags.
- Benchmark-specific code paths, fixture detection, cached/precomputed fixture
  answers, skipped output, reduced terminal fidelity, and any other benchmark
  gaming.
- Reducing snapshot/draw cadence merely to lower measured work. Existing pacing
  and interactive latency are product behavior, not optimization loopholes.

## Constraints

- Never overfit to one benchmark. A keep must improve the aggregate score and
  have a plausible workload-independent production explanation.
- Never cheat the benchmarks. Production must perform the same semantic work
  and produce byte-for-byte equivalent protocol/state behavior unless a real
  protocol design improvement is deliberately tested end to end.
- All tests, formatting, and clippy must pass.
- Preserve protocol safety, delta resynchronization, Unicode/wide-cell behavior,
  styles, selection, cursor, scrollback, copy mode, and synchronized output.
- Preserve or improve `latency`; do not accept throughput wins caused by extra
  buffering or delay.
- Prefer simple structural wins. Reject brittle complexity for noise-level gains.
- Record deferred promising ideas in `.auto/ideas.md`.

## What's Been Tried

- Existing work in `PERF.md`: PTY batching, style reuse, 8 ms snapshot pacing,
  trusted snapshot construction, MessagePack packed styles, and dirty-row deltas
  are already shipped. Arc fan-out and naive pane/chrome caching were previously
  investigated and rejected.
- Baseline pending.
