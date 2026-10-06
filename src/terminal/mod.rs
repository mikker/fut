//! PTY process ownership and the isolated Ghostty virtual-terminal adapter.

mod ghostty;
mod program_status;
mod runtime;

/// Scrollback storage budget per terminal, in bytes (allocated as history grows).
pub const DEFAULT_SCROLLBACK_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    pub scrollback_bytes: usize,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback_bytes: DEFAULT_SCROLLBACK_BYTES,
        }
    }
}

const DEFAULT_CELL_PIXEL_WIDTH: u16 = 9;
const DEFAULT_CELL_PIXEL_HEIGHT: u16 = 18;

pub(crate) use ghostty::{
    CopyModeOutcome, MouseInputOutcome, OutputCapture, OutputCaptureError, ViewportSnapshot,
};
pub use program_status::{ProgramState, ProgramStatus};
pub(crate) use runtime::AttachmentConfiguration;
pub use runtime::{
    CommandError, SpawnSpec, TerminalActivity, TerminalEvent, TerminalHandle, TerminalLifecycle,
    spawn_terminal,
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
            super::ghostty::GhosttyTerminal::new(size, writer, super::DEFAULT_SCROLLBACK_BYTES)
                .map(Self)
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
