# Render performance

Working notes for the rendering-performance effort: what the pipeline does
today, where the time goes, how to measure it, and the techniques proven out
by similar projects (Zellij, tmux, Ghostty, mosh).

## Measuring

- `mise run perf:bench` — criterion microbenchmarks (`benches/render.rs`) for
  the hot spots: VT feed → full-grid snapshot, snapshot JSON encode/decode,
  and the per-client grid clone. Baseline diffing:
  `cargo bench --bench render -- --save-baseline before` … `-- --baseline before`.
- `mise run perf:e2e` — end-to-end scenarios (`scripts/perf/run`) against a
  disposable daemon using the headless protocol client
  (`examples/perf_client.rs`): `flood` (plain `seq`), `styled` (SGR-heavy
  output), `latency` (keystroke echo p50/p95). Reports snapshots/sec, wire
  MiB/s, largest frame, decode cost, and revisions coalesced away.
- `FUT_PERF_LOG=/tmp/frames.csv fut … attach` — the real client logs one CSV
  row per frame decode and per draw (`src/client/perf.rs`). Summarize with
  `scripts/perf/report /tmp/frames.csv`.

## The pipeline today

PTY bytes → 1 KiB `read()` (`runtime.rs:1033`) → crossbeam bounded(16) →
VT thread `feed()` (`ghostty.rs:162`) which parses **and rebuilds the entire
grid** (`snapshot_current`, `ghostty.rs:1039`) → tokio `watch` (drops stale) →
per-client clone (`daemon/mod.rs:1017`) → outbound queue (coalesces to newest)
→ full-grid JSON (`protocol.rs:314`) → client decode → 16 ms paced draw with
per-pane dirty revisions and CSI 2026 (`client/mod.rs`).

The client loop is already shaped right (paced, dirty-gated, synchronized
output). The costs are upstream, ranked:

1. **One full-grid rebuild per 1 KiB of PTY output.** `snapshot_current`
   allocates a `String` per cell and makes ~6 FFI calls per cell; a 200x50
   pane is 10k allocations + 60k FFI calls per KiB. Then
   `ScreenSnapshot::new` (`domain.rs:463`) re-walks every cell with grapheme
   segmentation the daemon's own output doesn't need.
2. **Full-grid JSON per frame per client** (~100–300 KiB at 200x50), encoded
   in the daemon and decoded on the client (~1 ms per frame measured).
   Snapshots that get coalesced away downstream are still fully built and
   often fully serialized first.
3. **Full-grid `clone()` per attached client per revision** in
   `watch_attachment`; an `Arc<ScreenSnapshot>` would remove it.
4. Client chrome rebuilt from scratch every frame (`chrome.rs:227`,
   `sidebar.rs:388`), and `pane_layouts` allocating per call including
   mouse-move paths (`client/mod.rs:2617`).

## Results log

### Round 1 (2026-08-07), commits b4accd3..345daa2

| Metric (200x50 flood of `seq 1 100000`) | before | after |
| --- | --- | --- |
| snapshots built + sent | 600 (plain) / 1628 (styled) | 30 / 38 |
| wire volume | 11.1 MiB / 41.1 MiB | 0.6 MiB / 1.0 MiB |
| styled flood wall time | 0.61 s | 0.30 s |
| snapshot build (feed/plain_200x50) | 629 µs | 498 µs |
| echo latency, typing cadence | p50 0.5 ms | p50 0.7 ms |
| echo latency, back-to-back burst | p50 0.5 ms | p50 8.4 ms (pacing floor) |

- `b4accd3` batch PTY output before snapshotting (drain queue, 64 KiB reads).
- `eee1e1f` client: reuse resolved styles across same-styled cell runs.
- `6de608e` pace snapshot production to ≥8 ms apart per terminal
  (`SNAPSHOT_MIN_INTERVAL`); this is what collapsed the flood numbers.
- `345daa2` trusted snapshot constructor (skip per-cell revalidation),
  per-row selection lookup, skip style fetch for unstyled cells.

Investigated and rejected: `Arc<ScreenSnapshot>` through the fan-out
(clone cost only mattered at pre-pacing frame rates); caching
`pane_layouts`/chrome models client-side (already event-gated, or no clean
invalidation key — see eee1e1f's commit discussion).

Observed but unexplained: occasional 7–14 ms outliers in typed-cadence echo
latency (p95); p50 unaffected. Worth a look if input feel is ever off.

### Round 2 (2026-08-07): the amp/orb halt, commit 6f76458

Animated full-screen TUIs (amp's glowing orb) are the dense-cells case:
every frame restyles every cell with fresh truecolor values. Added a
vtebench-style `dense` workload (240 frames ≈ 4 s of animation at 200x50,
~90 MiB of escapes; `scripts/perf/run dense`, plus `feed/dense*` benches).

| 240 orb frames | main | perf-rendering |
| --- | --- | --- |
| wall time | 14.6 s (falls behind forever) | 4.9 s (real time) |
| grid rebuilds | 88,856 | 597 |
| frames to client | 7,495 (514/s) | 597 (122/s) |
| wire | 949 MiB | 76 MiB |

On main the client needs >100% of a core just to decode+draw the frame
stream, so it stalls and stops responding — that is the reported halt.
On this branch the real client renders the same animation at ~30 fps
effective using ~41% of a core (decode 8.2 ms + draw 4.2 ms per frame;
dense frames are ~650 KiB of JSON).

Diagnosis notes: VT feed handles dense input at ~100–140 MiB/s
(`feed/dense_anim_200x50` 3.6 ms/frame); macOS ptys deliver ~16 MiB/s in
1 KiB reads, so ingest is never the limit — snapshot pacing is what keeps
production at 122/s regardless of input rate.

### Round 3 (2026-08-07): MessagePack wire, commit af7a720

JSON decode was the client's bottleneck during dense animations (8.4 ms
per 200x50 fully-styled frame — caps the client near 30fps). Benchmarked
packed-styles-in-JSON (2.2 ms), msgpack struct maps (5.1 ms), and both
combined (1.6 ms); shipped the combination. Protocol v5.

Real client during the orb: decode 8.2 ms → 1.5 ms (busy 28.7% → 8.2%),
dense frames 143 KB → 37 KB, and the client now draws at the 16 ms pacing
ceiling (~60fps) instead of decode-starving. Draw (4.5 ms full-grid blit)
is now the largest per-frame client cost.

### Round 4 (2026-08-08): dirty-row deltas, branch row-deltas (fut-mjrz)

The writer task now diffs each outgoing snapshot against the last frame
actually sent per connection and ships only changed rows (full-snapshot
fallback on attach/resize/size change/>60% rows changed; RefreshTerminal
resync as safety net). Protocol v6. A/B on identical code (delta path
toggled), true 200x50 sizing:

| scenario | full-only | deltas | delta share |
| --- | --- | --- | --- |
| partial (40x10 orb, 600 frames) | 2.9 MiB | 0.8 MiB | 100% |
| dense (full-screen anim) | 102 MiB | 46 MiB | 99.8% |
| flood plain/styled | ~same | ~same | 3–6% (fallback, correct) |
| typing echo p50 / p95 | 6.4 / 12.1 ms | 3.5 / 7.4 ms | one-row frames |

Real client during the partial orb: decode 208 µs, draw 561 µs per frame
(<1% busy). Dense stayed mostly-delta because the VT thread's intermediate
snapshots each touch ~40% of rows — a genuine halving, found not predicted.

Also fixed en route: the perf client never resized its pane (all previous
scenarios silently ran at 80x24 — earlier rounds' absolute numbers are not
comparable across that fix), and a client panic on malformed delta rows.

## Remaining hypotheses

1. **Skip blitting unchanged rows in `Screen::render`** — deferred: ratatui
   resets the draw buffer every frame, so row-skipping would blank rows; it
   needs a retained-buffer approach. Draw is 0.6–4.5 ms depending on styled
   area; revisit only if profiling demands.
2. The typed-cadence p95 echo outliers above.

## Baseline (2026-08-07, M-series laptop, release build)

- `flood` 2k lines: 52 grid rebuilds for ~60 KiB of output; 19 KiB largest
  frame; 244 µs avg client decode (80x24-ish default pane).
- Real client, 200x50, `seq 1 20000`: decode avg 1.09 ms/frame (7.1 MiB wire
  for a 5 s session), draw avg 306 µs.
- Idle keystroke echo: p50 0.5 ms, p95 0.6 ms — the pipeline is fast when
  quiet; the problem is wasted work under output pressure.

Re-run `mise run perf:bench` and `mise run perf:e2e` before and after each
change; record deltas here.
