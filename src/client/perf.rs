//! Frame-time logging for render-performance work.
//!
//! Set `FUT_PERF_LOG=/path/to/file.csv` before attaching and the client
//! appends one CSV row per instrumented event:
//!
//! ```text
//! ts_us,event,dur_us,bytes
//! ```
//!
//! `decode` rows time the JSON decode of each daemon frame (bytes = wire
//! frame length); `draw` rows time one full `terminal.draw` pass.
//! `scripts/perf/report` summarizes these logs.

use std::{
    env,
    fs::File,
    io::{BufWriter, Write},
    time::{Duration, Instant},
};

pub struct PerfLog {
    out: BufWriter<File>,
    epoch: Instant,
}

impl PerfLog {
    pub fn from_env() -> Option<Self> {
        let path = env::var_os("FUT_PERF_LOG")?;
        let file = File::create(&path).ok()?;
        let mut out = BufWriter::new(file);
        let _ = writeln!(out, "ts_us,event,dur_us,bytes");
        Some(Self {
            out,
            epoch: Instant::now(),
        })
    }

    pub fn record(&mut self, event: &str, duration: Duration, bytes: usize) {
        let _ = writeln!(
            self.out,
            "{},{event},{},{bytes}",
            self.epoch.elapsed().as_micros(),
            duration.as_micros(),
        );
        let _ = self.out.flush();
    }
}
