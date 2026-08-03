use std::collections::BTreeMap;

use ratatui::layout::Rect;

use crate::domain::TerminalId;

const FOCUSED_MIN_WIDTH: u32 = 24;
const BACKGROUND_MIN_WIDTH: u32 = 12;
const RAIL_WIDTH: u32 = 1;

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
    if host.width == 0 || host.height == 0 || terminals.is_empty() || !terminals.contains(&focused)
    {
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

    let pane_count = u32::try_from(terminals.len()).unwrap_or(u32::MAX);
    let background_count = pane_count.saturating_sub(1);
    let required_width = FOCUSED_MIN_WIDTH
        .saturating_add(background_count.saturating_mul(BACKGROUND_MIN_WIDTH))
        .saturating_add(pane_count.saturating_mul(RAIL_WIDTH));

    if u32::from(host.width) < required_width {
        return BTreeMap::from([(
            focused,
            PaneLayout {
                rail: None,
                content: host,
            },
        )]);
    }

    let extra = u32::from(host.width) - required_width;
    let total_weight = pane_count.saturating_add(1);
    let extra_per_slot = extra / total_weight;
    let remainder = extra % total_weight;
    let mut slot = 0_u32;
    let mut x = host.x;

    terminals
        .iter()
        .copied()
        .map(|terminal| {
            let weight = if terminal == focused { 2 } else { 1 };
            let remainder_width = remainder.saturating_sub(slot).min(weight);
            slot = slot.saturating_add(weight);
            let minimum = if terminal == focused {
                FOCUSED_MIN_WIDTH
            } else {
                BACKGROUND_MIN_WIDTH
            };
            let width = minimum
                .saturating_add(extra_per_slot.saturating_mul(weight))
                .saturating_add(remainder_width) as u16;

            let rail = Rect::new(x, host.y, RAIL_WIDTH as u16, host.height);
            x = x.saturating_add(RAIL_WIDTH as u16);
            let content = Rect::new(x, host.y, width, host.height);
            x = x.saturating_add(width);

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

    fn widths(layouts: &BTreeMap<TerminalId, PaneLayout>, ids: &[TerminalId]) -> Vec<u16> {
        ids.iter().map(|id| layouts[id].content.width).collect()
    }

    #[test]
    fn invalid_inputs_have_no_layouts() {
        let ids = terminals(2);
        assert!(pane_layouts(Rect::new(2, 3, 80, 24), &[], TerminalId::new()).is_empty());
        assert!(pane_layouts(Rect::new(2, 3, 0, 24), &ids, ids[0]).is_empty());
        assert!(pane_layouts(Rect::new(2, 3, 80, 0), &ids, ids[0]).is_empty());
        assert!(pane_layouts(Rect::new(2, 3, 80, 24), &ids, TerminalId::new()).is_empty());
    }

    #[test]
    fn one_pane_uses_the_full_host_without_a_rail() {
        let ids = terminals(1);
        let host = Rect::new(4, 7, 19, 11);
        assert_eq!(
            pane_layouts(host, &ids, ids[0])[&ids[0]],
            PaneLayout {
                rail: None,
                content: host
            }
        );
    }

    #[test]
    fn threshold_is_exact_for_every_focus_position() {
        let ids = terminals(4);
        let threshold = 24 + 3 * 12 + 4;

        for &focused in &ids {
            let below = Rect::new(3, 5, threshold - 1, 7);
            let layouts = pane_layouts(below, &ids, focused);
            assert_eq!(layouts.len(), 1);
            assert_eq!(layouts[&focused].content, below);
            assert_eq!(layouts[&focused].rail, None);

            let layouts = pane_layouts(Rect::new(3, 5, threshold, 7), &ids, focused);
            assert_eq!(layouts.len(), ids.len());
            for id in &ids {
                assert_eq!(
                    layouts[id].content.width,
                    if *id == focused { 24 } else { 12 }
                );
            }
        }
    }

    #[test]
    fn extra_width_uses_weighted_slots_in_input_order() {
        let ids = terminals(3);
        let threshold = 24 + 2 * 12 + 3;
        let focused = ids[1];

        assert_eq!(
            widths(
                &pane_layouts(Rect::new(0, 0, threshold + 3, 1), &ids, focused),
                &ids
            ),
            [13, 26, 12]
        );
        assert_eq!(
            widths(
                &pane_layouts(Rect::new(0, 0, threshold + 4, 1), &ids, focused),
                &ids
            ),
            [13, 26, 13]
        );
        assert_eq!(
            widths(
                &pane_layouts(Rect::new(0, 0, threshold + 8, 1), &ids, focused),
                &ids
            ),
            [14, 28, 14]
        );
    }

    #[test]
    fn rails_precede_content_and_input_order_controls_columns() {
        let ids = terminals(3);
        let ordered = [ids[2], ids[0], ids[1]];
        let layouts = pane_layouts(Rect::new(10, 6, 51, 4), &ordered, ids[0]);

        assert_eq!(layouts[&ids[2]].rail, Some(Rect::new(10, 6, 1, 4)));
        assert_eq!(layouts[&ids[2]].content, Rect::new(11, 6, 12, 4));
        assert_eq!(layouts[&ids[0]].rail, Some(Rect::new(23, 6, 1, 4)));
        assert_eq!(layouts[&ids[0]].content, Rect::new(24, 6, 24, 4));
        assert_eq!(layouts[&ids[1]].rail, Some(Rect::new(48, 6, 1, 4)));
        assert_eq!(layouts[&ids[1]].content, Rect::new(49, 6, 12, 4));
    }

    #[test]
    fn tiny_hosts_show_only_the_focus_without_a_rail() {
        let ids = terminals(5);
        for width in 1..=5 {
            let host = Rect::new(9, 2, width, 3);
            let layouts = pane_layouts(host, &ids, ids[3]);
            assert_eq!(layouts.len(), 1);
            assert_eq!(
                layouts[&ids[3]],
                PaneLayout {
                    rail: None,
                    content: host
                }
            );
        }
    }

    #[test]
    fn broad_layout_invariants_hold() {
        for count in 2..=20 {
            let ids = terminals(count);
            for focus_index in 0..count {
                for width in 1..=300_u16 {
                    let host = Rect::new(7, 11, width, 3);
                    let layouts = pane_layouts(host, &ids, ids[focus_index]);
                    if layouts.len() == 1 {
                        assert_eq!(layouts[&ids[focus_index]].content, host);
                        assert_eq!(layouts[&ids[focus_index]].rail, None);
                        continue;
                    }

                    let mut x = host.x;
                    let mut total = 0_u32;
                    for id in &ids {
                        let layout = layouts[id];
                        let rail = layout.rail.unwrap();
                        assert_eq!(rail, Rect::new(x, host.y, 1, host.height));
                        x += 1;
                        assert_eq!(layout.content.x, x);
                        assert_eq!(
                            (layout.content.y, layout.content.height),
                            (host.y, host.height)
                        );
                        assert!(
                            layout.content.width >= if *id == ids[focus_index] { 24 } else { 12 }
                        );
                        x += layout.content.width;
                        total += 1 + u32::from(layout.content.width);
                    }
                    assert_eq!(total, u32::from(host.width));
                    assert_eq!(x, host.x + host.width);
                }
            }
        }
    }
}
