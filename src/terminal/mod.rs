//! PTY process ownership and the isolated Ghostty virtual-terminal adapter.

mod ghostty;
mod runtime;

pub(crate) use ghostty::{MouseWheelOutcome, ViewportSnapshot};
pub use runtime::{
    CommandError, SpawnSpec, TerminalEvent, TerminalHandle, TerminalLifecycle, spawn_terminal,
};
