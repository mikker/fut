//! Shared configuration and parent-client handoff for temporary commands.

use anyhow::{Result, bail};
use ratatui::layout::Rect;
use serde::Deserialize;

pub(crate) const ACTIVATE_OPENED_SOCKET_ENV: &str = "FUT_COMMAND_ACTIVATE_SOCKET";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PopupSize {
    pub width: Option<u16>,
    pub height: Option<u16>,
}

impl PopupSize {
    pub(crate) fn validate(self) -> Result<()> {
        if self.width.is_some_and(|width| width < 4) {
            bail!("size.width must be at least 4 columns");
        }
        if self.height.is_some_and(|height| height < 3) {
            bail!("size.height must be at least 3 rows");
        }
        Ok(())
    }

    pub(crate) fn area(self, host: Rect) -> Rect {
        let width = self.width.unwrap_or(host.width).min(host.width);
        let height = self.height.unwrap_or(host.height).min(host.height);
        Rect::new(
            host.x + host.width.saturating_sub(width) / 2,
            host.y + host.height.saturating_sub(height) / 2,
            width,
            height,
        )
    }
}
