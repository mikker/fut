//! Reproducible `less -X` input latency and Fut downstream pipeline diagnostic.
//! Run with `cargo run --release --example pager_measure`.

use std::{
    env, fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use fut::{
    client::bench::render_snapshot,
    domain::{ScreenSnapshot, TerminalId, TerminalSize},
    protocol::{Envelope, ServerMessage, decode_payload, encode_payload},
    terminal::bench::VtBench,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const SIZE: TerminalSize = TerminalSize {
    columns: 200,
    rows: 50,
};
const POSITIONS: usize = 3;
const REPLAYS: usize = 200;
const QUIET: Duration = Duration::from_millis(100);
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(2);

struct Capture {
    key: char,
    position: usize,
    before: Vec<u8>,
    bytes: Vec<u8>,
    first_byte: Duration,
    last_byte: Duration,
    quiet: Duration,
}

#[derive(Default)]
struct Costs {
    parse: Vec<u128>,
    snapshot: Vec<u128>,
    encode: Vec<u128>,
    decode: Vec<u128>,
    render: Vec<u128>,
}

fn main() -> Result<()> {
    metadata();
    let fixture = fixture()?;
    let captures = capture_less(&fixture)?;
    let _ = fs::remove_file(&fixture);

    println!(
        "capture: {}x{}, {POSITIONS} positions, quiet={}ms",
        SIZE.columns,
        SIZE.rows,
        QUIET.as_millis()
    );
    for capture in &captures {
        println!(
            "  {}@{}: {:5} bytes hash={:016x} input→first={:7.3}ms input→last={:7.3}ms input→quiet={:7.3}ms",
            capture.key,
            capture.position,
            capture.bytes.len(),
            hash(&capture.bytes),
            ms(capture.first_byte),
            ms(capture.last_byte),
            ms(capture.quiet)
        );
    }

    for key in ['d', 'u'] {
        let mut costs = Costs::default();
        for capture in captures.iter().filter(|capture| capture.key == key) {
            replay(capture, &mut costs)?;
        }
        println!(
            "  {key} downstream median ({REPLAYS} replays/position): parse {:.1}us snapshot {:.1}us encode {:.1}us decode {:.1}us client-render {:.1}us",
            median_us(&mut costs.parse),
            median_us(&mut costs.snapshot),
            median_us(&mut costs.encode),
            median_us(&mut costs.decode),
            median_us(&mut costs.render)
        );
    }
    Ok(())
}

fn metadata() {
    let less = Command::new("less")
        .arg("--version")
        .env_clear()
        .env("PATH", path())
        .output();
    let less = less
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.lines().next().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into());
    println!(
        "platform: {} {} / {}",
        env::consts::OS,
        env::consts::ARCH,
        std::env::consts::FAMILY
    );
    println!(
        "pager: {less}; args=-R -X; TERM=xterm-256color LANG=C LC_ALL=C LESS/LESSOPEN/LESSCLOSE unset"
    );
}

fn path() -> String {
    env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
}

fn fixture() -> Result<PathBuf> {
    let path = env::temp_dir().join(format!("fut-pager-measure-{}.diff", std::process::id()));
    let mut text = String::new();
    for hunk in 0..80 {
        text.push_str(&format!("\x1b[1mdiff --git a/file{hunk}.rs b/file{hunk}.rs\x1b[0m\n\x1b[36m@@ -10,20 +10,20 @@\x1b[0m\n"));
        for line in 0..20 {
            text.push_str(&format!(
                "\x1b[31m-old value {hunk:02}/{line:02} {}\x1b[0m\n",
                "x".repeat(120)
            ));
            text.push_str(&format!(
                "\x1b[32m+new value {hunk:02}/{line:02} {}\x1b[0m\n",
                "y".repeat(120)
            ));
        }
    }
    fs::write(&path, text)?;
    Ok(path)
}

fn capture_less(pathname: &Path) -> Result<Vec<Capture>> {
    let pair = native_pty_system().openpty(PtySize {
        rows: SIZE.rows,
        cols: SIZE.columns,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new("less");
    // Delta uses less without terminal initialization so the pager remains on
    // the primary screen. Backward half-pages then use reverse-index (ESC M),
    // the direction-specific path this diagnostic needs to exercise.
    command.args([
        "-R",
        "-X",
        pathname.to_str().context("non-UTF-8 temporary path")?,
    ]);
    command.env_clear();
    for (key, value) in [
        ("PATH", path()),
        ("TERM", "xterm-256color".into()),
        ("LANG", "C".into()),
        ("LC_ALL", "C".into()),
        ("LESSHISTFILE", "-".into()),
    ] {
        command.env(key, value);
    }
    for key in ["LESS", "LESSOPEN", "LESSCLOSE", "PAGER", "GIT_PAGER", "LV"] {
        command.env_remove(key);
    }
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0; 16 * 1024];
        while let Ok(count) = reader.read(&mut buf) {
            if count == 0 || tx.send(buf[..count].to_vec()).is_err() {
                break;
            }
        }
    });

    let (initial, _, _, _) = read_action(&rx, None)?;
    ensure!(!initial.is_empty(), "less produced no initial screen");
    let mut state = initial;
    let mut captures = Vec::new();
    for position in 0..POSITIONS {
        let down = capture(&rx, &mut writer, 'd', position, &state)?;
        state.extend_from_slice(&down.bytes);
        let up = capture(&rx, &mut writer, 'u', position, &state)?;
        state.extend_from_slice(&up.bytes);
        captures.extend([down, up]);
        if position + 1 < POSITIONS {
            let (advance, _, _, _) = send_and_read(&rx, &mut writer, b"d")?;
            state.extend(advance);
        }
    }
    writer.write_all(b"q")?;
    let _ = child.wait();
    Ok(captures)
}

fn capture(
    rx: &Receiver<Vec<u8>>,
    writer: &mut impl Write,
    key: char,
    position: usize,
    before: &[u8],
) -> Result<Capture> {
    let (bytes, first_byte, last_byte, quiet) =
        send_and_read(rx, writer, key.to_string().as_bytes())?;
    ensure!(!bytes.is_empty(), "{key}@{position} produced no output");
    validate_transition(before, &bytes).with_context(|| {
        format!("{key}@{position} did not produce a complete screen transition")
    })?;
    Ok(Capture {
        key,
        position,
        before: before.to_vec(),
        bytes,
        first_byte,
        last_byte,
        quiet,
    })
}

fn send_and_read(
    rx: &Receiver<Vec<u8>>,
    writer: &mut impl Write,
    input: &[u8],
) -> Result<(Vec<u8>, Duration, Duration, Duration)> {
    writer.write_all(input)?;
    writer.flush()?;
    read_action(rx, Some(Instant::now()))
}

fn read_action(
    rx: &Receiver<Vec<u8>>,
    start: Option<Instant>,
) -> Result<(Vec<u8>, Duration, Duration, Duration)> {
    let started = start.unwrap_or_else(Instant::now);
    let first = rx
        .recv_timeout(FIRST_BYTE_TIMEOUT)
        .context("timed out waiting for first pager byte")?;
    let first_at = started.elapsed();
    let mut output = first;
    let mut last_at = started.elapsed();
    loop {
        match rx.recv_timeout(QUIET) {
            Ok(chunk) => {
                output.extend(chunk);
                last_at = started.elapsed();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("pager output reader disconnected"),
        }
    }
    Ok((output, first_at, last_at, started.elapsed()))
}

fn snapshot(stream: &[u8]) -> Result<ScreenSnapshot> {
    let mut vt = VtBench::new(SIZE)?;
    vt.feed(stream)?;
    vt.snapshot()?.context("snapshot suppressed")
}

fn validate_transition(before: &[u8], action: &[u8]) -> Result<()> {
    let mut before_snapshot = snapshot(before)?;
    let mut after_stream = Vec::with_capacity(before.len() + action.len());
    after_stream.extend_from_slice(before);
    after_stream.extend_from_slice(action);
    let mut after_snapshot = snapshot(&after_stream)?;
    before_snapshot.revision = 0;
    after_snapshot.revision = 0;
    let before = encode_payload(&before_snapshot)?;
    let after = encode_payload(&after_snapshot)?;
    ensure!(
        before != after,
        "screen remained unchanged ({:016x} == {:016x})",
        hash(&before),
        hash(&after)
    );
    Ok(())
}

fn replay(capture: &Capture, costs: &mut Costs) -> Result<()> {
    for _ in 0..REPLAYS {
        let mut vt = VtBench::new(SIZE)?;
        vt.feed(&capture.before)?;
        let start = Instant::now();
        vt.write(&capture.bytes);
        costs.parse.push(start.elapsed().as_nanos());
        let start = Instant::now();
        let screen = vt.snapshot()?.context("snapshot suppressed")?;
        costs.snapshot.push(start.elapsed().as_nanos());
        let envelope = Envelope {
            request_id: None,
            message: ServerMessage::Snapshot {
                terminal_id: TerminalId::new(),
                screen,
            },
        };
        let start = Instant::now();
        let wire = encode_payload(&envelope)?;
        costs.encode.push(start.elapsed().as_nanos());
        let start = Instant::now();
        let decoded: Envelope<ServerMessage> = decode_payload(&wire)?;
        costs.decode.push(start.elapsed().as_nanos());
        let ServerMessage::Snapshot { screen, .. } = decoded.message else {
            unreachable!()
        };
        let start = Instant::now();
        std::hint::black_box(render_snapshot(&screen));
        costs.render.push(start.elapsed().as_nanos());
    }
    Ok(())
}

fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
fn ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1_000.0
}
fn median_us(values: &mut [u128]) -> f64 {
    values.sort_unstable();
    values[values.len() / 2] as f64 / 1_000.0
}
