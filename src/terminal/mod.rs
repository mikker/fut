//! PTY process ownership and the isolated Ghostty virtual-terminal adapter.

mod ghostty;
mod runtime;

pub(crate) use ghostty::{
    CopyModeOutcome, MouseInputOutcome, OutputCapture, OutputCaptureError, ViewportSnapshot,
};
pub(crate) use runtime::AttachmentGeometry;
pub use runtime::{
    CommandError, SpawnSpec, TerminalEvent, TerminalHandle, TerminalLifecycle, spawn_terminal,
};

/// Benchmark-only access to the VT feed → snapshot path. Not part of the API.
#[doc(hidden)]
pub mod bench {
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    use crate::domain::{ScreenSnapshot, TerminalSize};

    pub struct VtBench(super::ghostty::GhosttyTerminal);

    impl VtBench {
        pub fn new(size: TerminalSize) -> anyhow::Result<Self> {
            let writer: Arc<Mutex<Box<dyn io::Write + Send>>> =
                Arc::new(Mutex::new(Box::new(io::sink())));
            super::ghostty::GhosttyTerminal::new(size, writer).map(Self)
        }

        pub fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<Option<ScreenSnapshot>> {
            self.0.feed(bytes)
        }

        pub fn write(&mut self, bytes: &[u8]) {
            self.0.vt_write(bytes);
        }

        pub fn snapshot(&mut self) -> anyhow::Result<Option<ScreenSnapshot>> {
            self.0.snapshot_after_feed()
        }
    }
}
