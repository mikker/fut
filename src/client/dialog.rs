//! Shared chrome for centered floating dialogs: the command palette, the
//! terminals-waiting list, and the which-key cheatsheet all share this
//! geometry, styling, and scrollbar so they read as one family.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

/// Centered floating box, biased toward the upper third of the host.
pub(super) fn dialog_area(host: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = host.width.min(max_width);
    let height = host.height.min(max_height);
    Rect::new(
        host.x.saturating_add((host.width - width) / 2),
        host.y.saturating_add((host.height - height) / 3),
        width,
        height,
    )
}

/// Content area inside a dialog's border; the whole box when it is too small
/// to afford one.
pub(super) fn frame_inner(area: Rect) -> Rect {
    if area.width < 3 || area.height < 3 {
        return area;
    }
    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

/// Clear the box, draw a rounded border with a drop shadow, and return the
/// content area inside. Boxes too small for a border are just cleared.
pub(super) fn render_frame(area: Rect, buffer: &mut Buffer) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let inner = frame_inner(area);
    if inner != area {
        render_shadow(area, buffer);
    }
    clear(area, buffer);
    if inner == area {
        return area;
    }
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    for x in area.x..=right {
        for y in [area.y, bottom] {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol("─");
            }
        }
    }
    for y in area.y..=bottom {
        for x in [area.x, right] {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol("│");
            }
        }
    }
    for ((x, y), corner) in [
        ((area.x, area.y), "╭"),
        ((right, area.y), "╮"),
        ((area.x, bottom), "╰"),
        ((right, bottom), "╯"),
    ] {
        if let Some(cell) = buffer.cell_mut((x, y)) {
            cell.set_symbol(corner);
        }
    }
    inner
}

/// Dim the cells peeking out below and to the right of the box so the
/// underlying content reads as shade, offset toward the bottom-right.
fn render_shadow(area: Rect, buffer: &mut Buffer) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let right = area.x.saturating_add(area.width);
    let bottom = area.y.saturating_add(area.height);
    for y in area.y.saturating_add(1)..bottom {
        for x in right..right.saturating_add(2) {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_style(dim);
            }
        }
    }
    for x in area.x.saturating_add(2)..right.saturating_add(2) {
        if let Some(cell) = buffer.cell_mut((x, bottom)) {
            cell.set_style(dim);
        }
    }
}

/// Reversed-bold title row across the top of the dialog.
pub(super) fn render_title(area: Rect, text: &str, buffer: &mut Buffer) {
    let row = Rect::new(area.x, area.y, area.width, 1);
    fill_row(row, title_style(), buffer);
    buffer.set_stringn(area.x, area.y, text, usize::from(area.width), title_style());
}

fn clear(area: Rect, buffer: &mut Buffer) {
    for row in area.y..area.y.saturating_add(area.height) {
        for column in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buffer.cell_mut((column, row)) {
                cell.reset();
            }
        }
    }
}

pub(super) fn fill_row(area: Rect, style: Style, buffer: &mut Buffer) {
    for column in area.x..area.x.saturating_add(area.width) {
        if let Some(cell) = buffer.cell_mut((column, area.y)) {
            cell.set_symbol(" ").set_style(style);
        }
    }
}

pub(super) fn title_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

pub(super) fn muted_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub(super) fn row_style(selected: bool) -> Style {
    if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    }
}

pub(super) fn render_footer(area: Rect, text: &str, buffer: &mut Buffer) {
    buffer.set_stringn(
        area.x,
        area.y + area.height - 1,
        text,
        usize::from(area.width),
        muted_style(),
    );
}

/// Scrollbar on the right edge of a list body scrolled by whole rows, shown
/// only when the list overflows.
pub(super) fn render_list_scrollbar(
    scroll: usize,
    total_rows: usize,
    body: Rect,
    buffer: &mut Buffer,
) {
    if body.width == 0 {
        return;
    }
    let max_scroll = total_rows.saturating_sub(usize::from(body.height));
    let Some((top, len)) = scrollbar_thumb(scroll, max_scroll, body.height) else {
        return;
    };
    let column = body.x + body.width - 1;
    for row in 0..body.height {
        if let Some(cell) = buffer.cell_mut((column, body.y + row)) {
            cell.set_symbol("▕").set_style(muted_style());
        }
    }
    for row in top..top.saturating_add(len).min(body.height) {
        if let Some(cell) = buffer.cell_mut((column, body.y + row)) {
            cell.set_symbol("▐")
                .set_style(muted_style().add_modifier(Modifier::BOLD));
        }
    }
}

/// Thumb placement within a track of `height` rows: `(top, len)` with the
/// thumb sized proportionally to how much of the content the viewport shows
/// and positioned by `scrolled_from_top` out of `max_scroll`.
pub(super) fn scrollbar_thumb(
    scrolled_from_top: usize,
    max_scroll: usize,
    height: u16,
) -> Option<(u16, u16)> {
    if max_scroll == 0 || height == 0 {
        return None;
    }
    let track = usize::from(height);
    let total = max_scroll + track;
    let len = (track * track).div_ceil(total).clamp(1, track);
    let max_top = track - len;
    let top = (scrolled_from_top.min(max_scroll) * max_top + max_scroll / 2) / max_scroll;
    Some((top as u16, len as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollbar_thumb_requires_scrollback_and_a_track() {
        assert_eq!(scrollbar_thumb(0, 0, 10), None);
        assert_eq!(scrollbar_thumb(5, 10, 0), None);
    }

    #[test]
    fn scrollbar_thumb_is_proportional_to_visible_share() {
        // Ten visible rows over ten rows of history: half the content is
        // visible, so the thumb covers half the track.
        assert_eq!(scrollbar_thumb(0, 10, 10), Some((0, 5)));
        // Deep history still leaves a grabbable one-cell thumb.
        assert_eq!(scrollbar_thumb(0, 10_000, 10), Some((0, 1)));
    }

    #[test]
    fn scrollbar_thumb_tracks_the_viewport_position() {
        let position = |scrolled| scrollbar_thumb(scrolled, 30, 10).unwrap();
        assert_eq!(position(0), (0, 3), "scrolled to the top");
        assert_eq!(position(15), (4, 3), "midway through history");
        assert_eq!(position(29), (7, 3), "one row above the bottom");
        let (top, len) = position(29);
        assert!(top + len <= 10, "thumb stays inside the track");
    }

    #[test]
    fn frame_draws_border_and_dims_a_bottom_right_shadow() {
        let host = Rect::new(0, 0, 12, 8);
        let mut buffer = Buffer::empty(host);
        for row in 0..host.height {
            for column in 0..host.width {
                buffer[(column, row)].set_symbol("x");
            }
        }
        let area = Rect::new(1, 1, 8, 5);
        let inner = render_frame(area, &mut buffer);
        assert_eq!(inner, Rect::new(2, 2, 6, 3));
        assert_eq!(buffer[(1, 1)].symbol(), "╭");
        assert_eq!(buffer[(8, 5)].symbol(), "╯");
        assert_eq!(buffer[(4, 1)].symbol(), "─");
        assert_eq!(buffer[(1, 3)].symbol(), "│");
        assert_eq!(buffer[(3, 3)].symbol(), " ", "interior is cleared");
        // Shadow dims the content two columns right and one row below.
        assert!(buffer[(9, 2)].modifier.contains(Modifier::DIM));
        assert!(buffer[(10, 6)].modifier.contains(Modifier::DIM));
        assert!(!buffer[(0, 0)].modifier.contains(Modifier::DIM));
        assert_eq!(buffer[(9, 2)].symbol(), "x", "shadow keeps content");
    }

    #[test]
    fn tiny_frames_skip_border_and_shadow() {
        let host = Rect::new(0, 0, 10, 4);
        let mut buffer = Buffer::empty(host);
        let area = Rect::new(0, 0, 10, 2);
        assert_eq!(render_frame(area, &mut buffer), area);
        assert_eq!(buffer[(0, 0)].symbol(), " ");
    }

    #[test]
    fn geometry_is_centered_bounded_and_zero_safe() {
        assert_eq!(
            dialog_area(Rect::new(4, 5, 100, 20), 80, 14),
            Rect::new(14, 7, 80, 14)
        );
        assert_eq!(
            dialog_area(Rect::new(4, 5, 20, 3), 80, 14),
            Rect::new(4, 5, 20, 3)
        );
        assert_eq!(
            dialog_area(Rect::new(4, 5, 0, 0), 80, 14),
            Rect::new(4, 5, 0, 0)
        );
    }
}
