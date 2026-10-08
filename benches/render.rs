//! Microbenchmarks for the render pipeline's hot spots:
//!
//! - `feed`: VT parse + full-grid snapshot per PTY chunk (the daemon's cost
//!   for every chunk of PTY output, today once per 1 KiB read)
//! - `encode`/`decode`: MessagePack wire cost of a full `ScreenSnapshot` per
//!   frame
//! - `clone`: the per-attached-client grid clone in the daemon fan-out
//! - `scale`: the same per-frame costs at large-display grid sizes, up to
//!   what a 5120x2160 display produces with small fonts. Sizes above
//!   `MAX_VISIBLE_CELLS` are skipped so the file runs against older caps.
//! - `graphics`: per-frame costs for a screen showing a Kitty image
//!
//! Run with `mise run perf:bench` (or `cargo bench --bench render`).
//! Compare runs with `critcmp` or criterion's built-in baseline diffing:
//! `cargo bench --bench render -- --save-baseline before`.

use std::fmt::Write;

use base64::{Engine, engine::general_purpose::STANDARD};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use fut::{
    domain::{MAX_VISIBLE_CELLS, ScreenSnapshot, TerminalSize},
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

/// vtebench dense_cells / animated-orb workload: one full-screen repaint
/// with a unique truecolor fg+bg pair per cell, no scrolling. The `phase`
/// shifts every color, the way an animation introduces new styles each
/// frame instead of reusing interned ones.
fn dense_frame(size: TerminalSize, phase: u32) -> Vec<u8> {
    let mut out = String::new();
    for row in 0..size.rows {
        write!(out, "\x1b[{};1H", row + 1).unwrap();
        for column in 0..size.columns {
            let (r, c, f) = (u32::from(row), u32::from(column), phase);
            write!(
                out,
                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}mo",
                (r * 5 + f * 7) % 256,
                (c * 3 + f * 11) % 256,
                (r + c + f * 13) % 256,
                (255 - r * 5 % 256),
                (255 - c * 3 % 256),
                (r * c + f * 17) % 256
            )
            .unwrap();
        }
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
        ("dense_200x50", LARGE, dense_frame(LARGE, 0)),
    ];
    let mut group = c.benchmark_group("feed");
    for (name, size, chunk) in cases {
        group.throughput(Throughput::Bytes(chunk.len() as u64));
        let mut vt = filled_terminal(size, &chunk);
        group.bench_function(name, |b| {
            b.iter(|| vt.feed(&chunk).expect("feed bench terminal"))
        });
    }
    // Animated variant: every frame carries fresh colors, so ghostty's
    // interned style table sees new styles per frame instead of cache hits.
    let frames: Vec<Vec<u8>> = (0..64).map(|phase| dense_frame(LARGE, phase)).collect();
    let bytes_per_frame = frames[0].len() as u64;
    group.throughput(Throughput::Bytes(bytes_per_frame));
    let mut vt = filled_terminal(LARGE, &frames[0]);
    let mut next = 0usize;
    group.bench_function("dense_anim_200x50", |b| {
        b.iter(|| {
            next = (next + 1) % frames.len();
            vt.feed(&frames[next]).expect("feed bench terminal")
        })
    });
    group.finish();
}

fn dense_snapshot(size: TerminalSize, phase: u32) -> ScreenSnapshot {
    let mut vt = filled_terminal(size, &dense_frame(size, phase));
    vt.feed(b"x")
        .expect("snapshot bench terminal")
        .expect("snapshot is published")
}

fn bench_wire(c: &mut Criterion) {
    let mut group = c.benchmark_group("wire");
    let cases: Vec<(&str, ScreenSnapshot)> = vec![
        ("styled_80x24", styled_snapshot(SMALL)),
        ("styled_200x50", styled_snapshot(LARGE)),
        ("dense_200x50", dense_snapshot(LARGE, 1)),
    ];
    for (name, snapshot) in cases {
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

/// Grid sizes a 5120x2160 display produces, from today's cell cap up to
/// small fonts: (name, size).
const SCALE_SIZES: [(&str, TerminalSize); 5] = [
    ("200x50", LARGE),
    (
        "250x200",
        TerminalSize {
            columns: 250,
            rows: 200,
        },
    ),
    (
        "508x160",
        TerminalSize {
            columns: 508,
            rows: 160,
        },
    ),
    (
        "731x154",
        TerminalSize {
            columns: 731,
            rows: 154,
        },
    ),
    (
        "833x180",
        TerminalSize {
            columns: 833,
            rows: 180,
        },
    ),
];

fn snapshot_envelope(screen: ScreenSnapshot) -> Envelope<ServerMessage> {
    Envelope {
        request_id: None,
        message: ServerMessage::Snapshot {
            terminal_id: fut::domain::TerminalId::new(),
            screen,
        },
    }
}

fn bench_scale(c: &mut Criterion) {
    let mut group = c.benchmark_group("scale");
    group.sample_size(20);
    for (name, size) in SCALE_SIZES {
        let cells = usize::from(size.columns) * usize::from(size.rows);
        if cells > MAX_VISIBLE_CELLS {
            eprintln!("scale/{name}: skipped, {cells} cells exceed cap {MAX_VISIBLE_CELLS}");
            continue;
        }
        let frames: Vec<Vec<u8>> = (0..8).map(|phase| dense_frame(size, phase)).collect();
        let mut vt = filled_terminal(size, &frames[0]);
        let mut next = 0usize;
        group.throughput(Throughput::Elements(cells as u64));
        group.bench_function(format!("feed_dense_{name}"), |b| {
            b.iter(|| {
                next = (next + 1) % frames.len();
                vt.feed(&frames[next]).expect("feed bench terminal")
            })
        });

        let envelope = snapshot_envelope(dense_snapshot(size, 1));
        let encoded = encode_payload(&envelope).expect("encode snapshot");
        eprintln!("scale/{name}: dense frame {} bytes", encoded.len());
        group.bench_function(format!("encode_dense_{name}"), |b| {
            b.iter(|| encode_payload(&envelope).expect("encode snapshot"))
        });
        group.bench_function(format!("decode_dense_{name}"), |b| {
            b.iter(|| decode_payload::<Envelope<ServerMessage>>(&encoded).expect("decode snapshot"))
        });
        group.bench_function(format!("clone_dense_{name}"), |b| {
            b.iter(|| envelope.clone())
        });
    }
    group.finish();
}

/// Deterministic noise so the PNG encoder cannot shrink the image much,
/// approximating a photo or screenshot shown by an image previewer.
fn noise_rgba(width: u32, height: u32) -> Vec<u8> {
    let mut state = 0x2545_f491_u32;
    (0..width * height * 4)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

/// Kitty graphics APC sequence transmitting and displaying one RGBA image
/// across `columns`x`rows` cells, chunked as real clients send it.
fn kitty_image(id: u32, width: u32, height: u32, columns: u16, rows: u16) -> Vec<u8> {
    let payload = STANDARD.encode(noise_rgba(width, height));
    let chunks: Vec<&[u8]> = payload.as_bytes().chunks(4096).collect();
    let mut out = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        if index == 0 {
            write!(
                out_string(&mut out),
                "\x1b_Ga=T,f=32,s={width},v={height},q=2,c={columns},r={rows},i={id},m={more};"
            )
            .unwrap();
        } else {
            write!(out_string(&mut out), "\x1b_Gm={more};").unwrap();
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// `write!` target appending UTF-8 to a byte buffer.
fn out_string(out: &mut Vec<u8>) -> impl Write + '_ {
    struct Bytes<'a>(&'a mut Vec<u8>);
    impl Write for Bytes<'_> {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            self.0.extend_from_slice(s.as_bytes());
            Ok(())
        }
    }
    Bytes(out)
}

fn bench_graphics(c: &mut Criterion) {
    let mut group = c.benchmark_group("graphics");
    let mut vt = filled_terminal(LARGE, &styled_chunk(64 * 1024));
    vt.feed(b"\x1b[H").expect("home cursor");
    vt.feed(&kitty_image(1, 640, 480, 80, 30))
        .expect("display image");
    // A one-cell text update next to a still image: the common case for an
    // image previewer or a TUI with an embedded picture.
    let screen = vt
        .feed(b"\x1b[40;100Hx")
        .expect("feed bench terminal")
        .expect("snapshot is published");
    let previous = vt
        .feed(b"\x1b[40;100Hy")
        .expect("feed bench terminal")
        .expect("snapshot is published");
    // What the daemon writes for a full screen: image pixels travel once in
    // their own `kitty_image` frame, so the screen carries references.
    let mut wire_screen = screen.clone();
    wire_screen.graphics.strip_pixels();
    let envelope = snapshot_envelope(wire_screen);
    let encoded = encode_payload(&envelope).expect("encode snapshot");
    eprintln!("graphics/image_200x50: full frame {} bytes", encoded.len());

    let mut toggle = false;
    group.bench_function("feed_image_200x50", |b| {
        b.iter(|| {
            toggle = !toggle;
            vt.feed(if toggle {
                b"\x1b[40;100Hx"
            } else {
                b"\x1b[40;100Hy"
            })
            .expect("feed bench terminal")
        })
    });
    group.bench_function("encode_image_200x50", |b| {
        b.iter(|| encode_payload(&envelope).expect("encode snapshot"))
    });
    group.bench_function("decode_image_200x50", |b| {
        b.iter(|| decode_payload::<Envelope<ServerMessage>>(&encoded).expect("decode snapshot"))
    });
    group.bench_function("clone_image_200x50", |b| b.iter(|| screen.clone()));
    // The daemon checks whether graphics changed before every delta.
    group.bench_function("graphics_eq_image_200x50", |b| {
        b.iter(|| {
            std::hint::black_box(&previous.graphics) == std::hint::black_box(&screen.graphics)
        })
    });
    group.finish();
}

criterion_group!(benches, bench_feed, bench_wire, bench_scale, bench_graphics);
criterion_main!(benches);
