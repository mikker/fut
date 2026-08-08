//! Headless perf client: attaches to a running daemon like a real client and
//! measures the snapshot pipeline end to end (frames/sec, wire bytes, decode
//! cost, revision drops, input-echo latency) without a host terminal.
//!
//! Usually launched via `scripts/perf/run`, which starts a dedicated daemon.
//! Direct use:
//!
//! ```sh
//! cargo run --release --example perf_client -- --socket /path/fut.sock flood
//! cargo run --release --example perf_client -- --socket /path/fut.sock styled
//! cargo run --release --example perf_client -- --socket /path/fut.sock latency
//! ```
//!
//! The daemon's pane must be running a plain `/bin/sh` reading stdin (the
//! `flood`/`styled` scenarios type shell commands into it).

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use fut::{
    domain::{ScreenSnapshot, TerminalId, TerminalSize},
    protocol::{
        ClientMessage, ClientMode, Envelope, PROTOCOL_VERSION, ServerMessage, codec,
        decode_payload, encode_payload,
    },
};
use futures_util::{SinkExt, StreamExt};
use tokio::{net::UnixStream, time};
use tokio_util::codec::Framed;
use uuid::Uuid;

const DEADLINE: Duration = Duration::from_secs(180);

type Connection = Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>;

struct Args {
    socket: String,
    scenario: String,
    columns: u16,
    rows: u16,
    count: Option<u64>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        socket: String::new(),
        scenario: String::new(),
        columns: 200,
        rows: 50,
        count: None,
    };
    let mut raw = std::env::args().skip(1);
    while let Some(arg) = raw.next() {
        let mut value = |name: &str| raw.next().context(format!("{name} requires a value"));
        match arg.as_str() {
            "--socket" => args.socket = value("--socket")?,
            "--columns" => args.columns = value("--columns")?.parse()?,
            "--rows" => args.rows = value("--rows")?.parse()?,
            "--count" => args.count = Some(value("--count")?.parse()?),
            scenario if !scenario.starts_with('-') => args.scenario = scenario.into(),
            other => bail!("unknown argument {other}"),
        }
    }
    if args.socket.is_empty() || args.scenario.is_empty() {
        bail!(
            "usage: perf_client --socket PATH [--columns N] [--rows N] [--count N] flood|styled|dense|partial|latency"
        );
    }
    Ok(args)
}

async fn send(
    connection: &mut Connection,
    request_id: Option<Uuid>,
    message: ClientMessage,
) -> anyhow::Result<()> {
    let payload = encode_payload(&Envelope {
        request_id,
        message,
    })?;
    connection.send(payload.into()).await?;
    Ok(())
}

/// One decoded frame plus its wire size and decode cost.
struct Frame {
    envelope: Envelope<ServerMessage>,
    wire_bytes: usize,
    decode: Duration,
}

async fn receive(connection: &mut Connection) -> anyhow::Result<Frame> {
    let frame = time::timeout(DEADLINE, connection.next())
        .await
        .context("timed out waiting for a daemon frame")?
        .context("daemon closed the connection")??;
    let started = Instant::now();
    let envelope: Envelope<ServerMessage> = decode_payload(&frame)?;
    Ok(Frame {
        envelope,
        wire_bytes: frame.len(),
        decode: started.elapsed(),
    })
}

fn screen_text(screen: &ScreenSnapshot) -> String {
    let columns = screen.size.columns as usize;
    let mut text = String::with_capacity(screen.cells.len() + screen.size.rows as usize);
    for row in screen.cells.chunks(columns) {
        for cell in row {
            text.push_str(&cell.contents);
        }
        text.push('\n');
    }
    text
}

#[derive(Default)]
struct FloodStats {
    snapshots: u64,
    wire_bytes: u64,
    max_frame: usize,
    decode_total: Duration,
    first_revision: Option<u64>,
    last_revision: u64,
}

impl FloodStats {
    fn accept(&mut self, revision: u64, wire_bytes: usize, decode: Duration) {
        self.snapshots += 1;
        self.wire_bytes += wire_bytes as u64;
        self.max_frame = self.max_frame.max(wire_bytes);
        self.decode_total += decode;
        self.first_revision.get_or_insert(revision);
        self.last_revision = revision;
    }

    fn report(&self, label: &str, elapsed: Duration) {
        let seconds = elapsed.as_secs_f64();
        let revisions = self
            .first_revision
            .map(|first| self.last_revision - first + 1)
            .unwrap_or_default();
        println!("== {label} ==");
        println!("wall time            {seconds:>10.2} s");
        println!("snapshots received   {:>10}", self.snapshots);
        println!(
            "snapshots/sec        {:>10.1}",
            self.snapshots as f64 / seconds
        );
        println!(
            "revisions spanned    {revisions:>10}  (coalesced away: {})",
            revisions.saturating_sub(self.snapshots)
        );
        println!(
            "wire received        {:>10.1} MiB  ({:.1} MiB/s)",
            self.wire_bytes as f64 / (1024.0 * 1024.0),
            self.wire_bytes as f64 / (1024.0 * 1024.0) / seconds
        );
        println!("largest frame        {:>10} bytes", self.max_frame);
        println!(
            "avg decode           {:>10.0} us",
            self.decode_total.as_micros() as f64 / self.snapshots.max(1) as f64
        );
    }
}

/// Type a command into the pane's shell and measure until its sentinel shows
/// up in a snapshot.
async fn flood(
    connection: &mut Connection,
    terminal_id: TerminalId,
    label: &str,
    command: &str,
) -> anyhow::Result<()> {
    let command = format!("{command}; echo PERF_\\DONE\n");
    let started = Instant::now();
    send(
        connection,
        None,
        ClientMessage::Input {
            bytes: command.into_bytes(),
        },
    )
    .await?;
    let mut stats = FloodStats::default();
    loop {
        let frame = receive(connection).await?;
        let ServerMessage::Snapshot {
            terminal_id: id,
            screen,
        } = frame.envelope.message
        else {
            continue;
        };
        if id != terminal_id {
            continue;
        }
        stats.accept(screen.revision, frame.wire_bytes, frame.decode);
        if screen_text(&screen).contains("PERF_DONE") {
            break;
        }
    }
    stats.report(label, started.elapsed());
    Ok(())
}

/// Keystroke echo latency: send one character, wait for the snapshot in which
/// it appears. Runs twice — spaced like human typing, and back-to-back like
/// key repeat, which is bounded by the daemon's snapshot pacing.
async fn latency(connection: &mut Connection, terminal_id: TerminalId) -> anyhow::Result<()> {
    let mut expected = 0usize;
    for (label, gap) in [
        ("latency (typing, 30ms gaps)", Duration::from_millis(30)),
        ("latency (burst, back-to-back)", Duration::ZERO),
    ] {
        latency_round(connection, terminal_id, label, gap, &mut expected).await?;
    }
    Ok(())
}

async fn latency_round(
    connection: &mut Connection,
    terminal_id: TerminalId,
    label: &str,
    gap: Duration,
    expected: &mut usize,
) -> anyhow::Result<()> {
    let iterations = 40;
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        time::sleep(gap).await;
        *expected += 1;
        let expected = *expected;
        let started = Instant::now();
        send(
            connection,
            None,
            ClientMessage::Input {
                bytes: b"x".to_vec(),
            },
        )
        .await?;
        loop {
            let frame = receive(connection).await?;
            let ServerMessage::Snapshot {
                terminal_id: id,
                screen,
            } = frame.envelope.message
            else {
                continue;
            };
            if id != terminal_id {
                continue;
            }
            let count = screen
                .cells
                .iter()
                .filter(|cell| cell.contents == "x")
                .count();
            if count >= expected {
                samples.push(started.elapsed());
                break;
            }
        }
    }
    samples.sort();
    let at = |fraction: f64| samples[(samples.len() as f64 * fraction) as usize - 1];
    println!("== {label} ==");
    println!("iterations           {:>10}", samples.len());
    println!(
        "echo p50             {:>10.1} ms",
        at(0.50).as_secs_f64() * 1000.0
    );
    println!(
        "echo p95             {:>10.1} ms",
        at(0.95).as_secs_f64() * 1000.0
    );
    println!(
        "echo max             {:>10.1} ms",
        samples.last().unwrap().as_secs_f64() * 1000.0
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let stream = UnixStream::connect(&args.socket)
        .await
        .with_context(|| format!("connecting to {}", args.socket))?;
    let mut connection = Framed::new(stream, codec());
    send(
        &mut connection,
        Some(Uuid::new_v4()),
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client_version: "fut-perf".into(),
            mode: ClientMode::Interactive {
                size: TerminalSize {
                    columns: args.columns,
                    rows: args.rows,
                },
                selector: None,
            },
        },
    )
    .await?;
    let welcome = receive(&mut connection).await?;
    let terminal_id = match welcome.envelope.message {
        ServerMessage::Welcome {
            selected: Some(selected),
            ..
        } => selected.focused.terminal_id,
        other => bail!("expected a welcome with a focused terminal, received {other:?}"),
    };

    // Let the initial attach snapshots settle before measuring.
    time::sleep(Duration::from_millis(300)).await;
    while let Ok(Some(_)) = time::timeout(Duration::from_millis(100), connection.next()).await {}

    match args.scenario.as_str() {
        "flood" => {
            let command = format!("seq 1 {}", args.count.unwrap_or(100_000));
            flood(&mut connection, terminal_id, "flood (plain)", &command).await?;
        }
        "styled" => {
            let command = format!(
                "awk 'BEGIN{{for(i=0;i<{};i++)printf \"\\033[3%dm%8d styled\\033[1m words\\033[0m here\\n\", i%8, i}}'",
                args.count.unwrap_or(100_000)
            );
            flood(&mut connection, terminal_id, "flood (styled)", &command).await?;
        }
        "dense" => {
            // vtebench dense_cells / animated-orb workload: full-screen
            // repaints with a unique truecolor fg+bg per cell, no scrolling.
            // `--count` is the number of frames here (default 240).
            let frames = args.count.unwrap_or(240).min(10_000);
            let command = format!(
                "awk -v cols={} -v rows={} -v frames={frames} 'BEGIN{{for(f=0;f<frames;f++){{for(r=0;r<rows;r++){{s=sprintf(\"\\033[%d;1H\",r+1);for(c=0;c<cols;c++){{s=s sprintf(\"\\033[38;2;%d;%d;%dm\\033[48;2;%d;%d;%dmo\",(r*5+f*7)%256,(c*3+f*11)%256,(r+c+f*13)%256,(255-r*5)%256,(255-c*3)%256,(255-f*13)%256)}}printf \"%s\",s}}}}printf \"\\033[0m\\033[2J\\033[H\"}}'",
                args.columns, args.rows
            );
            flood(&mut connection, terminal_id, "dense (orb-like)", &command).await?;
        }
        "partial" => {
            // A small animated region on an otherwise static screen — the
            // realistic amp-orb shape (the orb is a badge, not the whole
            // pane). `--count` is the number of frames (default 600).
            let frames = args.count.unwrap_or(600).min(20_000);
            let command = format!(
                "awk -v frames={frames} 'BEGIN{{for(f=0;f<frames;f++){{for(r=20;r<30;r++){{s=sprintf(\"\\033[%d;81H\",r+1);for(c=0;c<40;c++){{s=s sprintf(\"\\033[38;2;%d;%d;%dm\\033[48;2;%d;%d;%dmo\",(r*5+f*7)%256,(c*3+f*11)%256,(r+c+f*13)%256,(255-r*5)%256,(255-c*3)%256,(255-f*13)%256)}}printf \"%s\",s}}}}printf \"\\033[0m\\033[2J\\033[H\"}}'"
            );
            flood(
                &mut connection,
                terminal_id,
                "partial (small orb)",
                &command,
            )
            .await?;
        }
        "latency" => latency(&mut connection, terminal_id).await?,
        other => bail!("unknown scenario {other}"),
    }
    Ok(())
}
