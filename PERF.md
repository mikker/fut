# Render performance

Working notes for the rendering-performance effort: what the pipeline does
today, where the time goes, how to measure it, and the techniques proven out
by similar projects (Zellij, tmux, Ghostty, mosh).

## Measuring

- `mise run perf:bench` — criterion microbenchmarks (`benches/render.rs`) for
  the hot spots: VT feed → full-grid snapshot, snapshot MessagePack encode/decode,
  and the per-client grid clone. Baseline diffing:
  `cargo bench --bench render -- --save-baseline before` … `-- --baseline before`.
- `mise run perf:e2e` — end-to-end scenarios (`scripts/perf/run`) against a
  disposable daemon using the headless protocol client
  (`examples/perf_client.rs`): `flood` (plain `seq`), `styled` (SGR-heavy
  output), `latency` (keystroke echo p50/p95). Reports snapshots/sec, wire
  MiB/s, largest frame, decode cost, and revisions coalesced away.
- `mise run perf:pager` — run a sanitized `less -X` in a fixed 200x50 PTY over
  a generated diff; measure input-to-first-byte and input-to-last-byte/quiet
  for `d` and `u` at three positions, validate every round trip, hash streams,
  and replay them through Fut parse, snapshot, wire encode/decode, and client
  grid rendering. `-X` matches delta's primary-screen pager behavior and makes
  backward pages exercise reverse-index. Run on the same idle host when
  comparing revisions.
- `FUT_PERF_LOG=/tmp/frames.csv fut … attach` — the real client logs one CSV
  row per frame decode and per draw (`src/client/perf.rs`). Summarize with
  `scripts/perf/report /tmp/frames.csv`.
- `mise run perf:external` — build release Fut and run the disposable 300x65
  four-pane animation workload used for the whole-system budget. On macOS it
  records separate visible daemon/client and detached daemon `top` samples,
  daemon/client `sample` profiles, frame timing, and control latency under
  `target/fut-perf-external`. Set `FUT_PERF_SKIP_BUILD=1` to measure an already
  built release binary without another Cargo invocation.

## The pipeline today

PTY bytes → 64 KiB `read()` (`runtime.rs:1361`) → crossbeam bounded(16) →
VT thread drains up to 2 MiB per pass (`drain_output_batch`) and publishes at
most once per 8 ms per observed terminal; terminals with no snapshot watchers
keep parsing PTY output but defer snapshot construction until attachment →
full-grid snapshot (`snapshot_current`) → tokio `watch` (drops stale) →
per-client clone and row diff → compact
MessagePack full snapshot or dirty-row delta → client apply → 16 ms paced draw
with per-pane dirty revisions and CSI 2026 (`client/mod.rs`).

The client loop is already shaped right (paced, dirty-gated, synchronized
output). Remaining costs for observed terminals are upstream, ranked:

1. **A full-grid rebuild for every published terminal revision.** Pacing caps
   this at 125/s per observed terminal, but four independently animated panes
   in a visible layout can keep paying it continuously.
2. **The daemon builds and clones a full snapshot before deriving a row
   delta.** Deltas save wire/decode work but do not make upstream capture or
   per-client diffing proportional to the changed area.
3. **The client redraw path remains materially size-dependent.** At 300x65,
   an attached release client doing four-pane animation uses about 5–6% of a
   core by itself; debug builds are dramatically worse.
4. Client chrome is rebuilt each frame (`chrome.rs`, `sidebar.rs`), and pane
   layout is recomputed in several input/render paths (`client/mod.rs`).

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

### Round 5 (2026-08-10): cell capture and representation, commits f7f8d38..a6376d8

An autoresearch pass used an aggregate of eleven feed, encode, decode, and
clone operations across plain, styled, TUI, and dense 200x50 workloads. The
benchmark definition was frozen before the baseline and every kept change
passed formatting, clippy, unit, and E2E checks.

| Aggregate metric | before | after | change |
| --- | ---: | ---: | ---: |
| `render_ns` | 9,476,861 ns | 5,639,872 ns | **-40.5%** |
| `feed_ns` | 5,858,699 ns | 4,415,960 ns | -24.6% |
| `wire_ns` | 3,618,161 ns | 1,223,912 ns | -66.2% |

Proven changes, in application order:

- Store short cell contents inline with `CompactString` and reuse Ghostty's
  extraction buffer. Terminal cells are overwhelmingly short graphemes, so
  this removes heap work from capture, cloning, and decode without changing
  the string wire semantics.
- Skip per-cell width extraction and wide-selection normalization whenever
  Ghostty has no active selection. Selection and wide-cell behavior retain
  their original path.
- Specialize the protocol representation: plain cells are strings; styled
  cells are tuples whose two colors and four flags occupy one packed `u64`.
  Keep that packed style word as the in-memory representation too. Protocol
  v14 pins the exact JSON and MessagePack shapes.
- Extract ordinary cells from Ghostty's raw codepoint, reserving the string
  grapheme API for actual multi-codepoint clusters. Background-only cells and
  empty cells remain spaces.
- Construct Fut's zero style directly for unstyled cells rather than invoking
  Ghostty's FFI-backed `Style::default` for every blank/plain grid position.
- Expose Ghostty's existing multi-get API through the vendored safe wrapper.
  Snapshot capture now batches raw cell, styling, content-tag, and codepoint
  reads, removing roughly 20,000 FFI calls per 200x50 full-grid capture.

The faster wire path exposed a timing-sensitive copy-mode burst test. Its
retained-screen observation was made timing-independent before the protocol
change was accepted; this was a correctness prerequisite, not a benchmark
shortcut.

#### Rejected or inconclusive approaches

Do not repeat these experiments unchanged:

| Approach | Result | What a materially new attempt requires |
| --- | --- | --- |
| Retain a canonical snapshot inside `GhosttyTerminal` and patch dirty rows | Correct but 1.8% slower: returning an owned complete snapshot required a full-grid clone, which erased the extraction saving. | Publish a full-or-row-patch event and mutate the runtime's retained snapshot in place, or otherwise eliminate the ownership clone. |
| Store each snapshot row in `Arc<[Cell]>` | About 10% slower even when cells were moved rather than cloned. Fifty allocations and nested-row serde outweighed cheap snapshot clones. | Introduce sharing atomically with dirty-row publication and measure partial production workloads; Arc rows alone are not useful. |
| Gate cell styling queries with Ghostty's row-level styled flag | 1.8% slower. Per-row queries and false-positive styled rows cost more than they saved. | Use a genuinely batched accessor rather than another layer of row gating. |
| `Arc<ScreenSnapshot>` fan-out | Previously rejected: clone cost stopped being material after publication pacing. | Reprofile a demonstrated fan-out bottleneck first. |
| Skip canonical viewport restoration before every normal snapshot | Full correctness suite passed, but benchmark runs occurred during a noisy period and did not beat the best score. | Re-establish the invariant explicitly in the terminal state model, then rerun under stable host conditions; do not assume this is a proven win. |
| Move the trusted-cell length cap into grapheme extraction | Two stable benchmark runs were positive (0.3–1.2%), but formatting/E2E timeout failures prevented a clean keep; an earlier noisy run was strongly negative. | Retry only with a clean check environment and require a full successful run; this remains inconclusive. |

Dirty-row extraction itself remains promising—Herdr proves the underlying
Ghostty damage API works—but only after Fut's publication boundary can carry
partial state without recreating or cloning a complete grid. Full-screen dense
animations will still dirty every row, so this is primarily a typing and
partial-update production win, not a universal replacement for faster
full-grid extraction.

### Round 6 (2026-08-26): demand-driven snapshot construction

Fresh release profiles of the documented 300x65 workload separated the daemon
from the client. While visible, `snapshot_current` remained the daemon's main
rendering stack and ratatui buffer construction/diffing remained the client's;
the client log measured 700 µs average draws and 34 µs average decode. While
detached, no daemon consumer needed screen frames, but all four terminal threads
continued constructing full snapshots at animation rate.

Terminal runtimes now parse PTY output continuously but only construct paced
screen snapshots while a snapshot receiver exists. A newly attached watcher
explicitly asks the runtime for a current, output-barriered snapshot before it
publishes its first frame. This preserves terminal state, attach correctness,
and control scheduling while removing the entire full-grid publication path
from detached terminals. A focused unit regression pins the no-observer rule,
and `scripts/perf/external` captures the external recipe in-repo.

Both runs used the same release binary methodology, 8-second warmup, fifteen
steady one-second `top` samples after discarding `top`'s initial zero sample,
and the same child commands. The five-thread batch made visible CPU noisier,
so visible before/after should be read as acceptance checks, not an improvement
claim; the architectural change is intentionally inactive while attached.

| Metric | before | after |
| --- | ---: | ---: |
| visible daemon CPU | 3.11% (1.1–4.2%) | 3.64% (2.4–5.0%) |
| visible client CPU | 3.13% (1.1–4.3%) | 4.11% (3.1–5.2%) |
| visible total CPU | 6.24% | 7.75% |
| detached daemon CPU | 2.62% (1.6–3.2%) | **1.11% (0.6–1.5%)** |
| 50 control queries | 0.79 s | 0.267 s |
| 20 visible reads | 0.34 s | 0.110 s |
| client decode average / p95 | 34 / 90 µs | 29 / 53 µs |
| client draw average / p95 | 700 / 1,646 µs | 679 / 1,707 µs |

The after run meets the primary visible budget (<10% total), background budget
(<2.5%, reaching the 1.5% stretch target), and both control budgets (≤0.40 s
and ≤0.20 s). The before control timings exceeded the historical baseline under
concurrent machine load; the after timings demonstrate that the new attach
refresh does not put snapshot work on ordinary control requests.

### External multiplexer baseline (2026-08-10)

This is the aspirational whole-system comparison. All three multiplexers ran
optimized release binaries on the same machine, at a 300x65 host viewport,
with the same 2x2 layout and child commands:

- `cmatrix -ab -u 3`
- `pipes.sh -p 5 -f 30 -R -K`
- `htop`
- looping `genact --speed-factor 2` with repeated `--modules` arguments for
  `botnet`, `bruteforce`, `cryptomining`, `memdump`, and `weblog`

CPU is steady-state process CPU from repeated one-second `top` samples and
excludes the identical child workloads. “Background” leaves the animated
workspace running but displays a quiet workspace. tmux's server also owned
unrelated existing sessions, so its number is a conservative upper bound for
this workload's overhead.

| Multiplexer | background CPU | visible/attached CPU | relevant memory |
| --- | ---: | ---: | ---: |
| tmux 3.7b | 1.3–1.4% server | 1.6–2.1% server; client ~0% | 54–59 MiB shared server + 2 MiB client |
| Herdr 0.8.0 | 2.0–2.5% combined process | 7.1–9.1% combined process | 26–27 MiB |
| Fut 0.2 release | 1.9–2.8% daemon | 5.7–8.3% daemon + 4.7–6.4% client; **10–15% total** | 16–18 MiB daemon + 14 MiB client |

Control surfaces were already comparable; these timings include CLI process
startup:

| Operation | tmux | Herdr | Fut release |
| --- | ---: | ---: | ---: |
| 50 server/control queries | 0.30 s | 0.33 s | 0.32 s |
| 20 visible-pane reads | 0.12 s | 0.15 s | 0.15 s |

Performance budgets derived from this comparison:

- **Primary:** keep the four-pane 300x65 visible workload below 10% total CPU
  in release builds (Herdr-class), without regressing control latency.
- **Stretch:** below 5% total visible CPU, moving toward tmux's ~2% class.
- **Background:** at most 2.5% daemon CPU, with 1.5% as the stretch target.
- **Control:** 50 control queries in at most 0.40 s and 20 visible reads in at
  most 0.20 s on this machine.

Debug Fut measured 51–72% total CPU while attached and 14–20% detached under
the same workload. That explains development-build lag but is not an honest
cross-product baseline. Always use an optimized build for external
comparisons; retain debug results only as a warning against evaluating UI
smoothness from `target/debug/fut`.

## Remaining hypotheses

### Pager direction measurement

`mise run perf:pager` measures only this deterministic synthetic case. It
reports pager response timing and separately replays the exact streams through
the practical in-process Fut path. The 100 ms quiet window is deliberately
included in input-to-quiet, while input-to-last-byte avoids that fixed floor.
Each nonempty captured action must produce a different complete parsed screen;
first-byte and quiet waits have explicit failure bounds rather than silently
accepting an empty or unchanged capture. Results are host- and pager-version-
specific; record the command's metadata and output when investigating.

One release run on 2026-08-11 (macOS/aarch64, `less` 668) measured three
positions: `d` input-to-first-byte 0.044–0.069 ms and input-to-last-byte
0.245–0.330 ms; `u` 0.024–0.077 ms and 0.204–0.291 ms. The fixed quiet check
made input-to-quiet 100.9–105.4 ms. Across 200 replays per position, downstream
medians were 9.0/20.7 µs parse (`d`/`u`), 257.6/257.9 µs snapshot,
149.2/149.6 µs encode, 467.6/466.5 µs decode, and 102.5/103.0 µs client grid
render. Captured streams were 3,559–3,919 bytes; the command printed the six
individual stream hashes for comparison.

This diagnostic can show a directional difference in the measured pager or
in-process stages. It does **not** measure PTY scheduling, daemon wake-up and
coalescing, socket I/O, ratatui terminal diff/flush, or the host terminal's
paint. Therefore it neither proves nor rules out a rendering-cache effect in
those unmeasured stages, and it does not by itself identify a root cause.

The original latency was specific to development builds and delta's pager
shape. Delta runs `less` on the primary screen; each `u` half-page emits 33
`ESC M` reverse-index operations at 300x65. Ghostty's Zig Debug mode made that
captured 12.8 KiB update take 5.7–7.8 seconds to parse, versus 0.06–0.20 seconds
for a forward page. The complete real client path reproduced the same
asymmetry: `d` reached host output in 185–262 ms while `u` took 5.2–7.9
seconds. Direct delta/less paging took under 6 ms in both directions, locating
the delay inside Fut's VT feed rather than the pager, socket, client draw, or
host terminal.

Fut's dev profile now builds at opt-level 1 with debug info and debug assertions
intact. It builds `libghostty-vt-sys` without debug info, which makes its build
script select the optimized native terminal engine. On the captured stream,
reverse-index parsing falls to 0.10–0.23 ms; full snapshots remain 5–6 ms in
the Rust development build. Release behavior is unchanged.

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
