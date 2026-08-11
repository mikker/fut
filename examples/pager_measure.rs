//! Capture `less` output for `d` and `u`, then measure Fut's VT and snapshot
//! costs for each captured stream. Run with:
//! `cargo run --release --example pager_measure`

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use fut::{domain::TerminalSize, terminal::bench::VtBench};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const SIZE: TerminalSize = TerminalSize {
    columns: 200,
    rows: 50,
};
const SAMPLES: usize = 200;

fn main() -> Result<()> {
    let fixture = fixture()?;
    let (initial, down, up) = capture_less(&fixture)?;
    let _ = fs::remove_file(&fixture);

    println!("less action output at {}x{}:", SIZE.columns, SIZE.rows);
    println!("  d (forward):  {} bytes", down.len());
    println!("  u (backward): {} bytes", up.len());

    let forward = measure(&initial, &[], &down)?;
    let backward = measure(&initial, &down, &up)?;
    println!("Fut median over {SAMPLES} fresh terminals (release mode expected):");
    println!(
        "  d: parse {:>7.1} us, snapshot {:>7.1} us",
        forward.0, forward.1
    );
    println!(
        "  u: parse {:>7.1} us, snapshot {:>7.1} us",
        backward.0, backward.1
    );
    println!(
        "  u/d: bytes {:.2}x, parse {:.2}x, snapshot {:.2}x",
        up.len() as f64 / down.len() as f64,
        backward.0 / forward.0,
        backward.1 / forward.1
    );
    Ok(())
}

fn fixture() -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("fut-pager-measure-{}.diff", std::process::id()));
    let mut text = String::new();
    for hunk in 0..80 {
        text.push_str(&format!(
            "\x1b[1mdiff --git a/file{hunk}.rs b/file{hunk}.rs\x1b[0m\n"
        ));
        text.push_str("\x1b[36m@@ -10,20 +10,20 @@\x1b[0m\n");
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

fn capture_less(path: &Path) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let pair = native_pty_system().openpty(PtySize {
        rows: SIZE.rows,
        cols: SIZE.columns,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new("less");
    command.args(["-R", path.to_str().context("non-UTF-8 temporary path")?]);
    command.env("TERM", "xterm-256color");
    command.env("LESSHISTFILE", "-");
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

    let initial = read_until_quiet(&rx);
    writer.write_all(b"d")?;
    writer.flush()?;
    let down = read_until_quiet(&rx);
    writer.write_all(b"u")?;
    writer.flush()?;
    let up = read_until_quiet(&rx);
    writer.write_all(b"q")?;
    let _ = child.wait();
    Ok((initial, down, up))
}

fn read_until_quiet(rx: &Receiver<Vec<u8>>) -> Vec<u8> {
    let mut output = Vec::new();
    while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(75)) {
        output.extend(chunk);
    }
    output
}

fn measure(initial: &[u8], prefix: &[u8], action: &[u8]) -> Result<(f64, f64)> {
    let mut parse = Vec::with_capacity(SAMPLES);
    let mut snapshot = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let mut vt = VtBench::new(SIZE)?;
        vt.feed(initial)?;
        vt.feed(prefix)?;
        let start = Instant::now();
        vt.write(action);
        parse.push(start.elapsed().as_nanos());
        let start = Instant::now();
        vt.snapshot()?.context("snapshot suppressed")?;
        snapshot.push(start.elapsed().as_nanos());
    }
    parse.sort_unstable();
    snapshot.sort_unstable();
    Ok((
        parse[SAMPLES / 2] as f64 / 1_000.0,
        snapshot[SAMPLES / 2] as f64 / 1_000.0,
    ))
}
