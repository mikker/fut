use std::time::Duration;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use tokio::time::Instant;
use unicode_width::UnicodeWidthStr;

use super::{
    chrome::sanitize,
    config::{SemanticStyle, StylesConfig, TabBarPosition},
    dialog,
    presentation::truncate,
};

const INFO_LIFETIME: Duration = Duration::from_secs(3);
const MAX_WIDTH: u16 = 64;
const HORIZONTAL_MARGIN: u16 = 2;
const VERTICAL_MARGIN: u16 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Toast {
    Info(String),
    Prompt(String),
    Error(String),
}

impl Toast {
    pub(super) fn info(message: impl Into<String>) -> Self {
        Self::Info(message.into())
    }

    pub(super) fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    pub(super) fn prompt(message: impl Into<String>) -> Self {
        Self::Prompt(message.into())
    }
}

#[derive(Default)]
pub(super) struct ToastState {
    current: Option<ActiveToast>,
}

struct ActiveToast {
    toast: Toast,
    expires_at: Option<Instant>,
}

impl ToastState {
    pub(super) fn replace(&mut self, toast: Option<Toast>) {
        self.current = toast.map(|toast| {
            let expires_at =
                matches!(toast, Toast::Info(_)).then(|| Instant::now() + INFO_LIFETIME);
            ActiveToast { toast, expires_at }
        });
    }

    pub(super) fn info(&mut self, message: impl Into<String>) {
        self.replace(Some(Toast::info(message)));
    }

    pub(super) fn error(&mut self, message: impl Into<String>) {
        self.replace(Some(Toast::error(message)));
    }

    pub(super) fn clear(&mut self) {
        self.current = None;
    }

    pub(super) fn is_visible(&self) -> bool {
        self.current.is_some()
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.current.as_ref().and_then(|toast| toast.expires_at)
    }

    pub(super) fn expire(&mut self) {
        if self.deadline().is_some() {
            self.clear();
        }
    }

    pub(super) fn render(
        &self,
        host: Rect,
        tab_bar_position: TabBarPosition,
        styles: &StylesConfig,
        buffer: &mut Buffer,
    ) {
        let Some(active) = self.current.as_ref() else {
            return;
        };
        let (message, role, centered) = match &active.toast {
            Toast::Info(message) => (message.as_str(), SemanticStyle::Normal, false),
            Toast::Prompt(message) => (message.as_str(), SemanticStyle::Normal, true),
            Toast::Error(message) => (message.as_str(), SemanticStyle::Error, false),
        };
        let message = sanitize(message);
        let area = if centered {
            prompt_area(host, &message)
        } else {
            toast_area(host, tab_bar_position, &message)
        };
        let content = dialog::render_frame(area, buffer);
        if content.width == 0 || content.height == 0 {
            return;
        }
        let horizontal_padding = u16::from(content.width >= 3);
        let text_width = content
            .width
            .saturating_sub(horizontal_padding.saturating_mul(2));
        let text = truncate(&message, usize::from(text_width));
        let style = styles.apply(
            role,
            styles
                .apply(SemanticStyle::Normal, Style::default())
                .add_modifier(Modifier::BOLD),
        );
        dialog::fill_row(content, style, buffer);
        buffer.set_stringn(
            content.x.saturating_add(horizontal_padding),
            content.y,
            text,
            usize::from(text_width),
            style,
        );
    }
}

fn toast_area(host: Rect, tab_bar_position: TabBarPosition, message: &str) -> Rect {
    if host.width == 0 || host.height == 0 {
        return host;
    }
    let message_width = u16::try_from(UnicodeWidthStr::width(message)).unwrap_or(u16::MAX);
    let width = message_width
        .saturating_add(4)
        .min(MAX_WIDTH)
        .min(host.width);
    let height = 3.min(host.height);
    let horizontal_margin = HORIZONTAL_MARGIN.min(host.width.saturating_sub(width));
    let vertical_margin = VERTICAL_MARGIN.min(host.height.saturating_sub(height));
    let x = host
        .x
        .saturating_add(host.width - width - horizontal_margin);
    let y = match tab_bar_position {
        TabBarPosition::Top => host
            .y
            .saturating_add(host.height - height - vertical_margin),
        TabBarPosition::Bottom => host.y.saturating_add(vertical_margin),
    };
    Rect::new(x, y, width, height)
}

fn prompt_area(host: Rect, message: &str) -> Rect {
    if host.width == 0 || host.height == 0 {
        return host;
    }
    let message_width = u16::try_from(UnicodeWidthStr::width(message)).unwrap_or(u16::MAX);
    let width = message_width
        .saturating_add(4)
        .min(MAX_WIDTH)
        .min(host.width);
    let height = 3.min(host.height);
    Rect::new(
        host.x.saturating_add((host.width - width) / 2),
        host.y.saturating_add((host.height - height) / 2),
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_tab_bar_and_stays_inside_the_host() {
        let host = Rect::new(3, 4, 80, 24);
        assert_eq!(
            toast_area(host, TabBarPosition::Top, "config reloaded"),
            Rect::new(62, 24, 19, 3)
        );
        assert_eq!(
            toast_area(host, TabBarPosition::Bottom, "config reloaded"),
            Rect::new(62, 5, 19, 3)
        );
        assert_eq!(
            toast_area(Rect::new(3, 4, 2, 1), TabBarPosition::Top, "long message"),
            Rect::new(3, 4, 2, 1)
        );
    }

    #[test]
    fn information_expires_but_errors_wait_for_dismissal() {
        let mut state = ToastState::default();
        state.info("done");
        assert!(state.deadline().is_some());
        state.expire();
        assert!(!state.is_visible());

        state.error("failed");
        assert_eq!(state.deadline(), None);
        state.expire();
        assert!(state.is_visible());
    }

    #[test]
    fn render_uses_a_bordered_truncated_box() {
        let host = Rect::new(0, 0, 20, 8);
        let mut buffer = Buffer::empty(host);
        let mut state = ToastState::default();
        state.error("a very long failure message");
        state.render(
            host,
            TabBarPosition::Top,
            &StylesConfig::default(),
            &mut buffer,
        );
        let area = toast_area(host, TabBarPosition::Top, "a very long failure message");
        assert_eq!(buffer[(area.x, area.y)].symbol(), "╭");
        assert_eq!(buffer[(area.x + area.width - 1, area.y + 2)].symbol(), "╯");
        assert!(
            buffer[(area.x + 1, area.y + 1)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn prompts_are_dead_center_even_with_offset_and_tiny_hosts() {
        assert_eq!(
            prompt_area(Rect::new(3, 4, 80, 24), "Close workspace? (y/n)"),
            Rect::new(30, 14, 26, 3)
        );
        assert_eq!(
            prompt_area(Rect::new(3, 4, 2, 1), "Close workspace? (y/n)"),
            Rect::new(3, 4, 2, 1)
        );
    }
}
