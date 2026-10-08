---
id: lsc-78c8
status: open
deps: []
links: [lsc-9ea8]
created: 2026-10-08T02:41:43Z
type: feature
priority: 3
---
# Chunk full screens by row range so grid size is not bound by one frame

Full screens are a single frame capped at 8 MiB, so the visible cell limit (150,000) is derived from the worst-case screen encoding. Ghostty, WezTerm's mux, and tmux have no total-cell cap tied to a transport frame.

## Design

Send full screens (and large deltas) as row-range chunks tied to one revision, completed by an end marker. The client assembles chunks and presents only complete screens, discarding a partial screen when a newer revision begins. The outbound writer must never coalesce away part of a chunk sequence. Copy-mode screens need the same path. Remote peers need a new optional capability. Replace the cell cap with Ghostty-style per-dimension limits plus a memory bound.

## Acceptance Criteria

Grids beyond 150,000 cells attach and render; no frame exceeds MAX_FRAME_LEN; older remote peers keep their bounds. Do this after scroll-aware deltas: at 150,000 cells, full-screen sends during scrolling, not frame size or snapshot construction, dominate cost (PERF.md Round 7).
