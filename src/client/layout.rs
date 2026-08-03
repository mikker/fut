use std::collections::BTreeMap;

use ratatui::layout::Rect;

use crate::domain::TerminalId;

const MIN_CONTENT_WIDTH: u16 = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PaneLayout {
    pub(super) rail: Option<Rect>,
    pub(super) content: Rect,
}

pub(super) fn pane_layouts(
    host: Rect,
    terminals: &[TerminalId],
    focused: TerminalId,
) -> BTreeMap<TerminalId, PaneLayout> {
    if host.width == 0 || host.height == 0 || terminals.is_empty() {
        return BTreeMap::new();
    }

    if terminals.len() == 1 {
        return BTreeMap::from([(
            terminals[0],
            PaneLayout {
                rail: None,
                content: host,
            },
        )]);
    }

    let pane_count = u16::try_from(terminals.len()).unwrap_or(u16::MAX);
    let required_width = pane_count.saturating_mul(MIN_CONTENT_WIDTH + 1);
    if host.width < required_width {
        return BTreeMap::from([(
            focused,
            PaneLayout {
                rail: None,
                content: host,
            },
        )]);
    }

    let content_width = host.width - pane_count;
    let base_width = content_width / pane_count;
    let remainder = content_width % pane_count;
    let mut x = host.x;

    terminals
        .iter()
        .copied()
        .enumerate()
        .map(|(index, terminal)| {
            let rail = Rect::new(x, host.y, 1, host.height);
            x += 1;
            let width = base_width + u16::from(index < usize::from(remainder));
            let content = Rect::new(x, host.y, width, host.height);
            x += width;

            (
                terminal,
                PaneLayout {
                    rail: Some(rail),
                    content,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminals(count: usize) -> Vec<TerminalId> {
        (0..count).map(|_| TerminalId::new()).collect()
    }

    #[test]
    fn empty_terminal_list_has_no_layouts() {
        assert!(pane_layouts(Rect::new(2, 3, 80, 24), &[], TerminalId::new()).is_empty());
    }

    #[test]
    fn zero_width_or_height_has_no_layouts() {
        let ids = terminals(2);
        assert!(pane_layouts(Rect::new(2, 3, 0, 24), &ids, ids[0]).is_empty());
        assert!(pane_layouts(Rect::new(2, 3, 80, 0), &ids, ids[0]).is_empty());
    }

    #[test]
    fn one_pane_uses_the_full_host_without_a_rail() {
        let ids = terminals(1);
        let host = Rect::new(4, 7, 19, 11);

        assert_eq!(
            pane_layouts(host, &ids, ids[0]),
            BTreeMap::from([(
                ids[0],
                PaneLayout {
                    rail: None,
                    content: host,
                },
            )])
        );
    }

    #[test]
    fn multiple_panes_are_vertical_columns_with_preceding_rails() {
        let ids = terminals(3);
        let layouts = pane_layouts(Rect::new(10, 5, 39, 8), &ids, ids[1]);

        assert_eq!(layouts[&ids[0]].rail, Some(Rect::new(10, 5, 1, 8)));
        assert_eq!(layouts[&ids[0]].content, Rect::new(11, 5, 12, 8));
        assert_eq!(layouts[&ids[1]].rail, Some(Rect::new(23, 5, 1, 8)));
        assert_eq!(layouts[&ids[1]].content, Rect::new(24, 5, 12, 8));
        assert_eq!(layouts[&ids[2]].rail, Some(Rect::new(36, 5, 1, 8)));
        assert_eq!(layouts[&ids[2]].content, Rect::new(37, 5, 12, 8));
    }

    #[test]
    fn remainder_is_distributed_from_left_to_right() {
        let ids = terminals(3);
        let layouts = pane_layouts(Rect::new(0, 0, 44, 6), &ids, ids[0]);

        assert_eq!(layouts[&ids[0]].content, Rect::new(1, 0, 14, 6));
        assert_eq!(layouts[&ids[1]].content, Rect::new(16, 0, 14, 6));
        assert_eq!(layouts[&ids[2]].content, Rect::new(31, 0, 13, 6));
    }

    #[test]
    fn insufficient_width_shows_only_the_focused_pane() {
        let ids = terminals(3);
        let host = Rect::new(3, 9, 38, 2);

        assert_eq!(
            pane_layouts(host, &ids, ids[2]),
            BTreeMap::from([(
                ids[2],
                PaneLayout {
                    rail: None,
                    content: host,
                },
            )])
        );
    }

    #[test]
    fn input_order_controls_column_order() {
        let ids = terminals(3);
        let ordered = [ids[2], ids[0], ids[1]];
        let layouts = pane_layouts(Rect::new(0, 0, 39, 1), &ordered, ids[0]);

        assert_eq!(layouts[&ids[2]].rail.unwrap().x, 0);
        assert_eq!(layouts[&ids[0]].rail.unwrap().x, 13);
        assert_eq!(layouts[&ids[1]].rail.unwrap().x, 26);
    }

    #[test]
    fn columns_cover_every_host_cell_once() {
        let ids = terminals(4);
        let host = Rect::new(7, 11, 57, 3);
        let layouts = pane_layouts(host, &ids, ids[0]);
        let mut covered = vec![0_u8; usize::from(host.width * host.height)];

        for layout in layouts.values() {
            for rect in [layout.rail.unwrap(), layout.content] {
                assert!(rect.width > 0 && rect.height > 0);
                for y in rect.y..rect.y + rect.height {
                    for x in rect.x..rect.x + rect.width {
                        let index = usize::from((y - host.y) * host.width + (x - host.x));
                        covered[index] += 1;
                    }
                }
            }
        }

        assert!(covered.into_iter().all(|count| count == 1));
    }
}
