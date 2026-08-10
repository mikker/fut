use std::collections::BTreeMap;

use ratatui::layout::Rect;

use crate::domain::{SplitId, TerminalId};
use crate::{
    domain::PaneId,
    splits::{SplitAxis, SplitTree},
};

use super::actions::FocusDirection;

const FOCUSED_MIN_WIDTH: u32 = 24;
const BACKGROUND_MIN_WIDTH: u32 = 12;
const RAIL_WIDTH: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PaneLayout {
    pub(super) rail: Option<Rect>,
    pub(super) content: Rect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AuthoredLayout {
    pub(super) panes: BTreeMap<PaneId, Rect>,
    pub(super) dividers: Vec<SplitDivider>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SplitDivider {
    pub(super) split_id: SplitId,
    pub(super) axis: SplitAxis,
    pub(super) area: Rect,
    pub(super) branch_area: Rect,
    pub(super) available: u16,
    pub(super) first_min: u16,
    pub(super) first_max: u16,
    pub(super) first_size: u16,
}

impl SplitDivider {
    pub(super) fn contains(self, column: u16, row: u16) -> bool {
        column >= self.area.x
            && column < self.area.right()
            && row >= self.area.y
            && row < self.area.bottom()
    }

    pub(super) fn first_size_at(self, column: u16, row: u16) -> u16 {
        let proposed = match self.axis {
            SplitAxis::Horizontal => column.saturating_sub(self.branch_area.x),
            SplitAxis::Vertical => row.saturating_sub(self.branch_area.y),
        };
        proposed.clamp(self.first_min, self.first_max)
    }
}

pub(super) fn authored_layout(
    host: Rect,
    tree: &SplitTree,
    focused: PaneId,
    zoomed: bool,
) -> AuthoredLayout {
    if host.width == 0 || host.height == 0 || !tree.leaf_ids().contains(&focused) {
        return AuthoredLayout {
            panes: BTreeMap::new(),
            dividers: Vec::new(),
        };
    }
    if zoomed {
        return AuthoredLayout {
            panes: BTreeMap::from([(focused, host)]),
            dividers: Vec::new(),
        };
    }
    let minimum = split_minimum(tree, focused);
    if host.width < minimum.0 || host.height < minimum.1 {
        return AuthoredLayout {
            panes: BTreeMap::from([(focused, host)]),
            dividers: Vec::new(),
        };
    }
    let mut layout = AuthoredLayout {
        panes: BTreeMap::new(),
        dividers: Vec::new(),
    };
    place_split(tree, focused, host, &mut layout);
    layout
}

pub(super) fn authored_navigation_layout(
    host: Rect,
    tree: &SplitTree,
    focused: PaneId,
) -> AuthoredLayout {
    let minimum = split_minimum(tree, focused);
    authored_layout(
        Rect::new(0, 0, host.width.max(minimum.0), host.height.max(minimum.1)),
        tree,
        focused,
        false,
    )
}

pub(super) fn directional_neighbor<Id: Copy + Ord>(
    panes: &BTreeMap<Id, Rect>,
    order: &[Id],
    focused: Id,
    direction: FocusDirection,
) -> Option<Id> {
    let focused_rect = *panes.get(&focused)?;
    order
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(index, id)| {
            let candidate = *panes.get(&id)?;
            let (primary_gap, orthogonal, orthogonal_centers) = match direction {
                FocusDirection::Left if candidate.right() <= focused_rect.x => (
                    focused_rect.x - candidate.right(),
                    interval_separation(
                        candidate.y,
                        candidate.bottom(),
                        focused_rect.y,
                        focused_rect.bottom(),
                    ),
                    center_twice(candidate.y, candidate.height)
                        .abs_diff(center_twice(focused_rect.y, focused_rect.height)),
                ),
                FocusDirection::Right if candidate.x >= focused_rect.right() => (
                    candidate.x - focused_rect.right(),
                    interval_separation(
                        candidate.y,
                        candidate.bottom(),
                        focused_rect.y,
                        focused_rect.bottom(),
                    ),
                    center_twice(candidate.y, candidate.height)
                        .abs_diff(center_twice(focused_rect.y, focused_rect.height)),
                ),
                FocusDirection::Up if candidate.bottom() <= focused_rect.y => (
                    focused_rect.y - candidate.bottom(),
                    interval_separation(
                        candidate.x,
                        candidate.right(),
                        focused_rect.x,
                        focused_rect.right(),
                    ),
                    center_twice(candidate.x, candidate.width)
                        .abs_diff(center_twice(focused_rect.x, focused_rect.width)),
                ),
                FocusDirection::Down if candidate.y >= focused_rect.bottom() => (
                    candidate.y - focused_rect.bottom(),
                    interval_separation(
                        candidate.x,
                        candidate.right(),
                        focused_rect.x,
                        focused_rect.right(),
                    ),
                    center_twice(candidate.x, candidate.width)
                        .abs_diff(center_twice(focused_rect.x, focused_rect.width)),
                ),
                _ => return None,
            };
            Some((
                (
                    orthogonal.0,
                    primary_gap,
                    orthogonal.1,
                    orthogonal_centers,
                    index,
                ),
                id,
            ))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, id)| id)
}

fn interval_separation(
    first_start: u16,
    first_end: u16,
    second_start: u16,
    second_end: u16,
) -> (u8, u16) {
    if first_end <= second_start {
        (1, second_start - first_end)
    } else if second_end <= first_start {
        (1, first_start - second_end)
    } else {
        (0, 0)
    }
}

fn center_twice(start: u16, length: u16) -> u32 {
    u32::from(start) * 2 + u32::from(length)
}

fn split_minimum(tree: &SplitTree, focused: PaneId) -> (u16, u16) {
    match tree {
        SplitTree::Leaf { pane_id } => (if *pane_id == focused { 24 } else { 12 }, 3),
        SplitTree::Branch {
            axis,
            first,
            second,
            ..
        } => {
            let first = split_minimum(first, focused);
            let second = split_minimum(second, focused);
            match axis {
                SplitAxis::Horizontal => (
                    first.0.saturating_add(1).saturating_add(second.0),
                    first.1.max(second.1),
                ),
                SplitAxis::Vertical => (
                    first.0.max(second.0),
                    first.1.saturating_add(1).saturating_add(second.1),
                ),
            }
        }
    }
}

fn place_split(tree: &SplitTree, focused: PaneId, area: Rect, layout: &mut AuthoredLayout) {
    match tree {
        SplitTree::Leaf { pane_id } => {
            layout.panes.insert(*pane_id, area);
        }
        SplitTree::Branch {
            split_id,
            axis,
            ratio,
            first,
            second,
        } => {
            let first_min = split_minimum(first, focused);
            let second_min = split_minimum(second, focused);
            match axis {
                SplitAxis::Horizontal => {
                    let available = area.width - 1;
                    let first_max = available - second_min.0;
                    let first_width = ratio.first_cells(available).clamp(first_min.0, first_max);
                    let first_area = Rect::new(area.x, area.y, first_width, area.height);
                    let divider = Rect::new(area.x + first_width, area.y, 1, area.height);
                    let second_area =
                        Rect::new(divider.x + 1, area.y, available - first_width, area.height);
                    layout.dividers.push(SplitDivider {
                        split_id: *split_id,
                        axis: *axis,
                        area: divider,
                        branch_area: area,
                        available,
                        first_min: first_min.0,
                        first_max,
                        first_size: first_width,
                    });
                    place_split(first, focused, first_area, layout);
                    place_split(second, focused, second_area, layout);
                }
                SplitAxis::Vertical => {
                    let available = area.height - 1;
                    let first_max = available - second_min.1;
                    let first_height = ratio.first_cells(available).clamp(first_min.1, first_max);
                    let first_area = Rect::new(area.x, area.y, area.width, first_height);
                    let divider = Rect::new(area.x, area.y + first_height, area.width, 1);
                    let second_area =
                        Rect::new(area.x, divider.y + 1, area.width, available - first_height);
                    layout.dividers.push(SplitDivider {
                        split_id: *split_id,
                        axis: *axis,
                        area: divider,
                        branch_area: area,
                        available,
                        first_min: first_min.1,
                        first_max,
                        first_size: first_height,
                    });
                    place_split(first, focused, first_area, layout);
                    place_split(second, focused, second_area, layout);
                }
            }
        }
    }
}

pub(super) fn pane_layouts(
    host: Rect,
    terminals: &[TerminalId],
    focused: TerminalId,
    zoomed: bool,
) -> BTreeMap<TerminalId, PaneLayout> {
    if host.width == 0 || host.height == 0 || terminals.is_empty() || !terminals.contains(&focused)
    {
        return BTreeMap::new();
    }

    if terminals.len() == 1 || zoomed {
        let terminal = if zoomed { focused } else { terminals[0] };
        return BTreeMap::from([(
            terminal,
            PaneLayout {
                rail: None,
                content: host,
            },
        )]);
    }

    let pane_count = u32::try_from(terminals.len()).unwrap_or(u32::MAX);
    let required_width = accordion_required_width(terminals.len());

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

pub(super) fn navigation_pane_layouts(
    host: Rect,
    terminals: &[TerminalId],
    focused: TerminalId,
) -> BTreeMap<TerminalId, PaneLayout> {
    let width = u16::try_from(accordion_required_width(terminals.len()))
        .unwrap_or(u16::MAX)
        .max(host.width);
    pane_layouts(
        Rect::new(0, 0, width, host.height.max(1)),
        terminals,
        focused,
        false,
    )
}

fn accordion_required_width(pane_count: usize) -> u32 {
    let pane_count = u32::try_from(pane_count).unwrap_or(u32::MAX);
    let background_count = pane_count.saturating_sub(1);
    FOCUSED_MIN_WIDTH
        .saturating_add(background_count.saturating_mul(BACKGROUND_MIN_WIDTH))
        .saturating_add(pane_count.saturating_mul(RAIL_WIDTH))
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
    fn directional_neighbors_follow_edges_overlap_and_stable_leaf_order() {
        let a = PaneId::new();
        let b = PaneId::new();
        let c = PaneId::new();
        let order = [a, b, c];
        let panes = BTreeMap::from([
            (a, Rect::new(0, 0, 30, 21)),
            (b, Rect::new(31, 0, 30, 10)),
            (c, Rect::new(31, 11, 30, 10)),
        ]);

        assert_eq!(
            directional_neighbor(&panes, &order, a, FocusDirection::Right),
            Some(b),
            "equal right candidates use leaf order"
        );
        assert_eq!(
            directional_neighbor(&panes, &order, b, FocusDirection::Down),
            Some(c)
        );
        assert_eq!(
            directional_neighbor(&panes, &order, c, FocusDirection::Up),
            Some(b)
        );
        assert_eq!(
            directional_neighbor(&panes, &order, c, FocusDirection::Left),
            Some(a)
        );
        assert_eq!(
            directional_neighbor(&panes, &order, a, FocusDirection::Left),
            None
        );

        let direct = PaneId::new();
        let corner = PaneId::new();
        let panes = BTreeMap::from([
            (a, Rect::new(0, 10, 10, 10)),
            (corner, Rect::new(11, 0, 10, 10)),
            (direct, Rect::new(20, 11, 10, 8)),
        ]);
        assert_eq!(
            directional_neighbor(&panes, &[a, corner, direct], a, FocusDirection::Right),
            Some(direct),
            "orthogonal overlap beats a nearer corner touch"
        );
    }

    #[test]
    fn navigation_layout_expands_tiny_authored_and_accordion_hosts() {
        let left = PaneId::new();
        let top_right = PaneId::new();
        let bottom_right = PaneId::new();
        let mut tree = SplitTree::leaf(left);
        assert!(tree.split(left, crate::splits::SplitDirection::Right, top_right));
        assert!(tree.split(top_right, crate::splits::SplitDirection::Down, bottom_right));
        let authored = authored_navigation_layout(Rect::new(7, 9, 1, 1), &tree, left);
        assert_eq!(authored.panes.len(), 3);
        assert_eq!(authored.panes[&left].x, 0);

        let terminals = terminals(4);
        let accordion = navigation_pane_layouts(Rect::new(7, 9, 1, 1), &terminals, terminals[2]);
        assert_eq!(accordion.len(), 4);
        assert_eq!(accordion[&terminals[0]].content.x, 1);
    }

    #[test]
    fn invalid_inputs_have_no_layouts() {
        let ids = terminals(2);
        assert!(pane_layouts(Rect::new(2, 3, 80, 24), &[], TerminalId::new(), false).is_empty());
        assert!(pane_layouts(Rect::new(2, 3, 0, 24), &ids, ids[0], false).is_empty());
        assert!(pane_layouts(Rect::new(2, 3, 80, 0), &ids, ids[0], false).is_empty());
        assert!(pane_layouts(Rect::new(2, 3, 80, 24), &ids, TerminalId::new(), false).is_empty());
    }

    #[test]
    fn one_pane_uses_the_full_host_without_a_rail() {
        let ids = terminals(1);
        let host = Rect::new(4, 7, 19, 11);
        assert_eq!(
            pane_layouts(host, &ids, ids[0], false)[&ids[0]],
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
            let layouts = pane_layouts(below, &ids, focused, false);
            assert_eq!(layouts.len(), 1);
            assert_eq!(layouts[&focused].content, below);
            assert_eq!(layouts[&focused].rail, None);

            let layouts = pane_layouts(Rect::new(3, 5, threshold, 7), &ids, focused, false);
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
                &pane_layouts(Rect::new(0, 0, threshold + 3, 1), &ids, focused, false,),
                &ids
            ),
            [13, 26, 12]
        );
        assert_eq!(
            widths(
                &pane_layouts(Rect::new(0, 0, threshold + 4, 1), &ids, focused, false,),
                &ids
            ),
            [13, 26, 13]
        );
        assert_eq!(
            widths(
                &pane_layouts(Rect::new(0, 0, threshold + 8, 1), &ids, focused, false,),
                &ids
            ),
            [14, 28, 14]
        );
    }

    #[test]
    fn rails_precede_content_and_input_order_controls_columns() {
        let ids = terminals(3);
        let ordered = [ids[2], ids[0], ids[1]];
        let layouts = pane_layouts(Rect::new(10, 6, 51, 4), &ordered, ids[0], false);

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
            let layouts = pane_layouts(host, &ids, ids[3], false);
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
                    let layouts = pane_layouts(host, &ids, ids[focus_index], false);
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

    #[test]
    fn explicit_zoom_gives_only_the_focus_the_full_host() {
        let ids = terminals(3);
        let host = Rect::new(7, 9, 80, 24);

        for &focused in &ids {
            assert_eq!(
                pane_layouts(host, &ids, focused, true),
                BTreeMap::from([(
                    focused,
                    PaneLayout {
                        rail: None,
                        content: host,
                    },
                )])
            );
        }
    }

    #[test]
    fn authored_splits_honor_axes_ratios_and_focused_minimum_fallback() {
        let a = PaneId::new();
        let b = PaneId::new();
        let mut horizontal = SplitTree::leaf(a);
        assert!(horizontal.split(a, crate::splits::SplitDirection::Right, b));
        let layout = authored_layout(Rect::new(0, 0, 80, 23), &horizontal, a, false);
        assert_eq!(layout.panes[&a], Rect::new(0, 0, 39, 23));
        assert_eq!(layout.dividers.len(), 1);
        assert_eq!(layout.dividers[0].area, Rect::new(39, 0, 1, 23));
        assert_eq!(layout.dividers[0].branch_area, Rect::new(0, 0, 80, 23));
        assert_eq!(layout.dividers[0].available, 79);
        assert_eq!(layout.dividers[0].first_min, 24);
        assert_eq!(layout.dividers[0].first_max, 67);
        assert_eq!(layout.dividers[0].first_size, 39);
        assert_eq!(layout.panes[&b], Rect::new(40, 0, 40, 23));

        let split_id = layout.dividers[0].split_id;
        assert!(horizontal.resize(
            split_id,
            crate::splits::SplitRatio::from_cells(37, 79).unwrap()
        ));
        let exact = authored_layout(Rect::new(0, 0, 80, 23), &horizontal, a, false);
        assert_eq!(exact.dividers[0].area, Rect::new(37, 0, 1, 23));

        let mut vertical = SplitTree::leaf(a);
        assert!(vertical.split(a, crate::splits::SplitDirection::Down, b));
        let layout = authored_layout(Rect::new(2, 3, 80, 23), &vertical, a, false);
        assert_eq!(layout.panes[&a], Rect::new(2, 3, 80, 11));
        assert_eq!(layout.dividers[0].area, Rect::new(2, 14, 80, 1));
        assert_eq!(layout.dividers[0].available, 22);
        assert_eq!(layout.dividers[0].first_min, 3);
        assert_eq!(layout.dividers[0].first_max, 19);
        assert_eq!(layout.panes[&b], Rect::new(2, 15, 80, 11));

        let narrow = authored_layout(Rect::new(4, 5, 36, 6), &horizontal, a, false);
        assert_eq!(narrow.panes, BTreeMap::from([(a, Rect::new(4, 5, 36, 6))]));
        assert!(narrow.dividers.is_empty());
    }
}
