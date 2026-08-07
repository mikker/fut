//! Microbenchmarks for the render pipeline's hot spots:
//!
//! - `feed`: VT parse + full-grid snapshot per PTY chunk (the daemon's cost
//!   for every chunk of PTY output, today once per 1 KiB read)
//! - `encode`/`decode`: JSON wire cost of a full `ScreenSnapshot` per frame
//! - `clone`: the per-attached-client grid clone in the daemon fan-out
//!
//! Run with `mise run perf:bench` (or `cargo bench --bench render`).
//! Compare runs with `critcmp` or criterion's built-in baseline diffing:
//! `cargo bench --bench render -- --save-baseline before`.

use std::fmt::Write;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use fut::{
    domain::{ScreenSnapshot, TerminalSize},
    protocol::{Envelope, ServerMessage, decode_payload, encode_payload},
    terminal::bench::VtBench,
};

const SMALL: TerminalSize = TerminalSize {
    columns: 80,
    rows: 24,
};
const LARGE: TerminalSize = TerminalSize {
    columns: 200,
    rows: 50,
};

/// Plain scrolling text: `seq`-like output, one short line per row.
fn plain_chunk(bytes: usize) -> Vec<u8> {
    let mut out = String::new();
    let mut line = 0u64;
    while out.len() < bytes {
        line += 1;
        writeln!(
            out,
            "line {line}: the quick brown fox jumps over the lazy dog"
        )
        .unwrap();
    }
    out.truncate(bytes);
    out.into_bytes()
}

/// Heavily styled scrolling text: an SGR color change per word, like build
/// tool or test-runner output.
fn styled_chunk(bytes: usize) -> Vec<u8> {
    let mut out = String::new();
    let mut line = 0u64;
    while out.len() < bytes {
        line += 1;
        for word in 0..8u8 {
            write!(
                out,
                "\x1b[3{}m\x1b[1mword{word}\x1b[0m ",
                (line + u64::from(word)) % 8
            )
            .unwrap();
        }
        out.push_str("\r\n");
    }
    out.truncate(bytes);
    out.into_bytes()
}

/// A fullscreen-TUI style repaint: home the cursor and rewrite every row with
/// per-row styling, no scrolling.
fn tui_frame(size: TerminalSize) -> Vec<u8> {
    let mut out = String::from("\x1b[H");
    for row in 0..size.rows {
        write!(out, "\x1b[{};1H\x1b[48;5;{}m", row + 1, row % 16).unwrap();
        let mut text = String::new();
        while text.len() < size.columns as usize {
            write!(text, "row {row} col {} ", text.len()).unwrap();
        }
        text.truncate(size.columns as usize);
        out.push_str(&text);
        out.push_str("\x1b[0m");
    }
    out.into_bytes()
}

fn filled_terminal(size: TerminalSize, chunk: &[u8]) -> VtBench {
    let mut vt = VtBench::new(size).expect("create bench terminal");
    // Warm the grid so every benched feed works on a fully populated screen.
    vt.feed(chunk).expect("prefill bench terminal");
    vt
}

fn styled_snapshot(size: TerminalSize) -> ScreenSnapshot {
    let mut vt = filled_terminal(size, &styled_chunk(64 * 1024));
    vt.feed(b"x")
        .expect("snapshot bench terminal")
        .expect("snapshot is published")
}

fn bench_feed(c: &mut Criterion) {
    let cases = [
        ("plain_80x24", SMALL, plain_chunk(1024)),
        ("plain_200x50", LARGE, plain_chunk(1024)),
        ("styled_200x50", LARGE, styled_chunk(1024)),
        ("plain_64k_200x50", LARGE, plain_chunk(64 * 1024)),
        ("tui_frame_200x50", LARGE, tui_frame(LARGE)),
    ];
    let mut group = c.benchmark_group("feed");
    for (name, size, chunk) in cases {
        group.throughput(Throughput::Bytes(chunk.len() as u64));
        let mut vt = filled_terminal(size, &chunk);
        group.bench_function(name, |b| {
            b.iter(|| vt.feed(&chunk).expect("feed bench terminal"))
        });
    }
    group.finish();
}

fn bench_wire(c: &mut Criterion) {
    let mut group = c.benchmark_group("wire");
    for (name, size) in [("80x24", SMALL), ("200x50", LARGE)] {
        let snapshot = styled_snapshot(size);
        let terminal_id = fut::domain::TerminalId::new();
        let envelope = Envelope {
            request_id: None,
            message: ServerMessage::Snapshot {
                terminal_id,
                screen: snapshot.clone(),
            },
        };
        let encoded = encode_payload(&envelope).expect("encode snapshot");
        group.throughput(Throughput::Bytes(encoded.len() as u64));
        group.bench_function(format!("encode_{name}"), |b| {
            b.iter(|| encode_payload(&envelope).expect("encode snapshot"))
        });
        group.bench_function(format!("decode_{name}"), |b| {
            b.iter(|| decode_payload::<Envelope<ServerMessage>>(&encoded).expect("decode snapshot"))
        });
        group.bench_function(format!("clone_{name}"), |b| b.iter(|| snapshot.clone()));
    }
    group.finish();
}

criterion_group!(benches, bench_feed, bench_wire);
criterion_main!(benches);
