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

## Remaining hypotheses

1. **Dirty-row deltas on the wire, snapshot fallback** (Ghostty row damage,
   mosh diff-against-acked): send changed rows + cursor; full snapshot on
   attach/resize/fall-behind. The one big-ticket item left — cuts wire and
   decode by ~10–50x during scrolling, but it's a protocol change that
   ripples into the e2e suite's snapshot expectations. At the paced ~125
   frames/s the absolute cost is already modest, so weigh against dogfood
   feel first.
2. **Cheaper cell representation / binary encoding** — `String` per cell
   still allocates ~10k times per snapshot build and JSON decode costs
   ~1.4 ms at 200x50; only worth touching together with the delta protocol.
3. The typed-cadence p95 echo outliers above.

## Baseline (2026-08-07, M-series laptop, release build)

- `flood` 2k lines: 52 grid rebuilds for ~60 KiB of output; 19 KiB largest
  frame; 244 µs avg client decode (80x24-ish default pane).
- Real client, 200x50, `seq 1 20000`: decode avg 1.09 ms/frame (7.1 MiB wire
  for a 5 s session), draw avg 306 µs.
- Idle keystroke echo: p50 0.5 ms, p95 0.6 ms — the pipeline is fast when
  quiet; the problem is wasted work under output pressure.

Re-run `mise run perf:bench` and `mise run perf:e2e` before and after each
change; record deltas here.
