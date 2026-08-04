use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::domain::PaneId;

pub const HALF_RATIO: u16 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

impl SplitDirection {
    pub fn axis(self) -> SplitAxis {
        match self {
            Self::Right => SplitAxis::Horizontal,
            Self::Down => SplitAxis::Vertical,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SplitTree {
    Leaf {
        pane_id: PaneId,
    },
    Branch {
        axis: SplitAxis,
        first_basis_points: u16,
        first: Box<SplitTree>,
        second: Box<SplitTree>,
    },
}

impl SplitTree {
    pub fn leaf(pane_id: PaneId) -> Self {
        Self::Leaf { pane_id }
    }

    pub fn leaf_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.append_leaf_ids(&mut ids);
        ids
    }

    pub fn split(&mut self, anchor: PaneId, direction: SplitDirection, pane_id: PaneId) -> bool {
        match self {
            Self::Leaf { pane_id: current } if *current == anchor => {
                *self = Self::Branch {
                    axis: direction.axis(),
                    first_basis_points: HALF_RATIO,
                    first: Box::new(Self::leaf(anchor)),
                    second: Box::new(Self::leaf(pane_id)),
                };
                true
            }
            Self::Leaf { .. } => false,
            Self::Branch { first, second, .. } => {
                first.split(anchor, direction, pane_id) || second.split(anchor, direction, pane_id)
            }
        }
    }

    pub fn without(self, pane_id: PaneId) -> Option<Self> {
        match self {
            Self::Leaf { pane_id: current } => (current != pane_id).then_some(Self::leaf(current)),
            Self::Branch {
                axis,
                first_basis_points,
                first,
                second,
            } => match ((*first).without(pane_id), (*second).without(pane_id)) {
                (Some(first), Some(second)) => Some(Self::Branch {
                    axis,
                    first_basis_points,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(child), None) | (None, Some(child)) => Some(child),
                (None, None) => None,
            },
        }
    }

    pub fn retained(&self, keep: impl Fn(PaneId) -> bool + Copy) -> Option<Self> {
        match self {
            Self::Leaf { pane_id } => keep(*pane_id).then_some(Self::leaf(*pane_id)),
            Self::Branch {
                axis,
                first_basis_points,
                first,
                second,
            } => match (first.retained(keep), second.retained(keep)) {
                (Some(first), Some(second)) => Some(Self::Branch {
                    axis: *axis,
                    first_basis_points: *first_basis_points,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(child), None) | (None, Some(child)) => Some(child),
                (None, None) => None,
            },
        }
    }

    pub fn validate(&self) -> bool {
        let mut seen = BTreeSet::new();
        self.validate_into(&mut seen)
    }

    fn append_leaf_ids(&self, ids: &mut Vec<PaneId>) {
        match self {
            Self::Leaf { pane_id } => ids.push(*pane_id),
            Self::Branch { first, second, .. } => {
                first.append_leaf_ids(ids);
                second.append_leaf_ids(ids);
            }
        }
    }

    fn validate_into(&self, seen: &mut BTreeSet<PaneId>) -> bool {
        match self {
            Self::Leaf { pane_id } => seen.insert(*pane_id),
            Self::Branch {
                first_basis_points,
                first,
                second,
                ..
            } => {
                (1..10_000).contains(first_basis_points)
                    && first.validate_into(seen)
                    && second.validate_into(seen)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_order_and_collapse_are_deterministic() {
        let a = PaneId::new();
        let b = PaneId::new();
        let c = PaneId::new();
        let mut tree = SplitTree::leaf(a);
        assert!(tree.split(a, SplitDirection::Right, b));
        assert!(tree.split(a, SplitDirection::Down, c));
        assert_eq!(tree.leaf_ids(), [a, c, b]);
        assert!(tree.validate());

        let tree = tree.without(a).unwrap();
        assert_eq!(tree.leaf_ids(), [c, b]);
        let tree = tree.without(c).unwrap();
        assert_eq!(tree, SplitTree::leaf(b));
        assert!(tree.without(b).is_none());
    }
}
