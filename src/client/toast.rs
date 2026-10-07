use std::{collections::VecDeque, time::Duration};

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
const MAX_MESSAGES: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MessageKind {
    Info,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Message {
    pub kind: MessageKind,
    pub text: String,
}

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
    messages: VecDeque<Message>,
}

struct ActiveToast {
    toast: Toast,
    expires_at: Option<Instant>,
}

impl ToastState {
    pub(super) fn replace(&mut self, toast: Option<Toast>) {
        if let Some(message) = toast.as_ref().and_then(Message::from_toast) {
            if self.messages.len() == MAX_MESSAGES {
                self.messages.pop_front();
            }
            self.messages.push_back(message);
        }
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

    pub(super) fn messages(&self) -> impl ExactSizeIterator<Item = &Message> {
        self.messages.iter()
    }

    pub(super) fn clear_messages(&mut self) {
        self.messages.clear();
    }

    pub(super) fn hit_test(
        &self,
        host: Rect,
        tab_bar_position: TabBarPosition,
        column: u16,
        row: u16,
    ) -> bool {
        let Some(active) = self.current.as_ref() else {
            return false;
        };
        let message = match &active.toast {
            Toast::Info(message) | Toast::Error(message) => message,
            Toast::Prompt(_) => return false,
        };
        let area = toast_area(host, tab_bar_position, &sanitize(message));
        column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.current.as_ref().and_then(|toast| toast.expires_at)
    }

    pub(super) fn expire(&mut self) {
        if self.deadline().is_some() {
            self.clear();
        }
    }

    /// `prompt_host` narrows where a prompt is centered, so a prompt about one
    /// pane appears over that pane; other toasts always use `host`.
    pub(super) fn render(
        &self,
        host: Rect,
        prompt_host: Option<Rect>,
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
            prompt_area(prompt_host.unwrap_or(host), &message)
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

impl Message {
    fn from_toast(toast: &Toast) -> Option<Self> {
        match toast {
            Toast::Info(text) => Some(Self {
                kind: MessageKind::Info,
                text: text.clone(),
            }),
            Toast::Error(text) => Some(Self {
                kind: MessageKind::Error,
                text: text.clone(),
            }),
            Toast::Prompt(_) => None,
        }
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
    fn keeps_a_bounded_log_without_recording_prompts() {
        let mut state = ToastState::default();
        state.info("done");
        state.replace(Some(Toast::prompt("Close pane? (y/n)")));
        state.error("failed");

        assert_eq!(
            state.messages().cloned().collect::<Vec<_>>(),
            [
                Message {
                    kind: MessageKind::Info,
                    text: "done".into(),
                },
                Message {
                    kind: MessageKind::Error,
                    text: "failed".into(),
                },
            ]
        );

        for index in 0..MAX_MESSAGES {
            state.info(index.to_string());
        }
        assert_eq!(state.messages().len(), MAX_MESSAGES);
        assert_eq!(state.messages().next().unwrap().text, "0");
    }

    #[test]
    fn only_visible_message_toasts_are_clickable() {
        let host = Rect::new(0, 0, 80, 24);
        let mut state = ToastState::default();
        state.info("a long message");
        let area = toast_area(host, TabBarPosition::Top, "a long message");
        assert!(state.hit_test(host, TabBarPosition::Top, area.x + 1, area.y + 1));
        assert!(!state.hit_test(host, TabBarPosition::Top, 0, 0));

        state.replace(Some(Toast::prompt("continue?")));
        assert!(!state.hit_test(host, TabBarPosition::Top, area.x + 1, area.y + 1));
    }

    #[test]
    fn render_uses_a_bordered_truncated_box() {
        let host = Rect::new(0, 0, 20, 8);
        let mut buffer = Buffer::empty(host);
        let mut state = ToastState::default();
        state.error("a very long failure message");
        state.render(
            host,
            None,
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
    fn prompts_center_on_their_prompt_host_but_toasts_ignore_it() {
        let host = Rect::new(0, 0, 80, 24);
        let pane = Rect::new(41, 0, 39, 24);
        let mut buffer = Buffer::empty(host);
        let mut state = ToastState::default();
        state.replace(Some(Toast::prompt("Close pane? (y/n)")));
        state.render(
            host,
            Some(pane),
            TabBarPosition::Top,
            &StylesConfig::default(),
            &mut buffer,
        );
        let area = prompt_area(pane, "Close pane? (y/n)");
        assert!(area.x >= pane.x);
        assert_eq!(buffer[(area.x, area.y)].symbol(), "╭");

        let mut buffer = Buffer::empty(host);
        state.error("failed");
        state.render(
            host,
            Some(pane),
            TabBarPosition::Top,
            &StylesConfig::default(),
            &mut buffer,
        );
        let area = toast_area(host, TabBarPosition::Top, "failed");
        assert_eq!(buffer[(area.x, area.y)].symbol(), "╭");
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
