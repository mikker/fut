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

## Techniques from similar projects, in likely order of application

1. **Batch before parsing, snapshot after draining** (Zellij PR #229, foot):
   drain the PTY queue, feed it all, then snapshot once — "buffer empty or
   deadline elapsed" (Zellij used 30 ms; 16 ms fits a 60 fps target). Raise
   the 1 KiB read buffer (64 KiB typical).
2. **Pace snapshot production, not just delivery** (Ghostty): the VT thread
   should produce at most one snapshot per ~16 ms while dirty, instead of one
   per chunk that downstream channels discard after the work is done.
3. **Dirty-row deltas on the wire, snapshot fallback** (Ghostty row damage,
   mosh diff-against-acked): send changed rows + cursor; full snapshot on
   attach/resize/fall-behind. libghostty-vt tracks row dirty state.
4. **Latest-wins everywhere with bounded channels** (Zellij #525, tmux):
   already mostly true here (watch + outbound queue + client pending);
   keep it true for whatever replaces full snapshots.
5. **`Arc` the snapshot through the fan-out** — removes clone-per-client.
6. **Cheaper cell representation** — inline small strings (or `char` +
   overflow map) instead of `String` per cell; skip revalidation of
   daemon-built grids; consider a binary encoding once deltas land.

## Baseline (2026-08-07, M-series laptop, release build)

- `flood` 2k lines: 52 grid rebuilds for ~60 KiB of output; 19 KiB largest
  frame; 244 µs avg client decode (80x24-ish default pane).
- Real client, 200x50, `seq 1 20000`: decode avg 1.09 ms/frame (7.1 MiB wire
  for a 5 s session), draw avg 306 µs.
- Idle keystroke echo: p50 0.5 ms, p95 0.6 ms — the pipeline is fast when
  quiet; the problem is wasted work under output pressure.

Re-run `mise run perf:bench` and `mise run perf:e2e` before and after each
change; record deltas here.
