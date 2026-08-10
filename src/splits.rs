use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};

use crate::domain::{PaneId, SplitId};

pub const HALF_RATIO: SplitRatio = SplitRatio {
    numerator: 1,
    denominator: 2,
};

/// A durable split proportion. Cell-driven updates use the reduced
/// `first_cells / available_cells` fraction, so laying the branch out at the
/// same size always reproduces the exact requested cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SplitRatio {
    numerator: u16,
    denominator: u16,
}

impl SplitRatio {
    pub fn from_cells(first: u16, available: u16) -> Option<Self> {
        if first == 0 || first >= available {
            return None;
        }
        Some(Self::reduced(first, available))
    }

    fn reduced(numerator: u16, denominator: u16) -> Self {
        let divisor = gcd(numerator, denominator);
        Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        }
    }

    pub fn first_cells(self, available: u16) -> u16 {
        if !self.is_valid() {
            return 0;
        }
        u16::try_from(
            u32::from(available) * u32::from(self.numerator) / u32::from(self.denominator),
        )
        .unwrap_or(available)
    }

    pub fn is_valid(self) -> bool {
        self.numerator > 0 && self.numerator < self.denominator
    }
}

impl<'de> Deserialize<'de> for SplitRatio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireRatio {
            numerator: u16,
            denominator: u16,
        }

        let ratio = WireRatio::deserialize(deserializer)?;
        Self::from_cells(ratio.numerator, ratio.denominator).ok_or_else(|| {
            serde::de::Error::custom(format_args!(
                "invalid split ratio {}/{}",
                ratio.numerator, ratio.denominator
            ))
        })
    }
}

impl std::fmt::Display for SplitRatio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.numerator, self.denominator)
    }
}

fn gcd(mut left: u16, mut right: u16) -> u16 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

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
        split_id: SplitId,
        axis: SplitAxis,
        ratio: SplitRatio,
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
                    split_id: SplitId::new(),
                    axis: direction.axis(),
                    ratio: HALF_RATIO,
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
                split_id,
                axis,
                ratio,
                first,
                second,
            } => match ((*first).without(pane_id), (*second).without(pane_id)) {
                (Some(first), Some(second)) => Some(Self::Branch {
                    split_id,
                    axis,
                    ratio,
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
                split_id,
                axis,
                ratio,
                first,
                second,
            } => match (first.retained(keep), second.retained(keep)) {
                (Some(first), Some(second)) => Some(Self::Branch {
                    split_id: *split_id,
                    axis: *axis,
                    ratio: *ratio,
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
        let mut splits = BTreeSet::new();
        self.validate_into(&mut seen, &mut splits)
    }

    pub fn resize(&mut self, split_id: SplitId, ratio: SplitRatio) -> bool {
        if !ratio.is_valid() {
            return false;
        }
        match self {
            Self::Leaf { .. } => false,
            Self::Branch {
                split_id: current,
                ratio: current_ratio,
                first,
                second,
                ..
            } => {
                if *current == split_id {
                    *current_ratio = ratio;
                    true
                } else {
                    first.resize(split_id, ratio) || second.resize(split_id, ratio)
                }
            }
        }
    }

    pub fn ratio(&self, split_id: SplitId) -> Option<SplitRatio> {
        match self {
            Self::Leaf { .. } => None,
            Self::Branch {
                split_id: current,
                ratio,
                first,
                second,
                ..
            } => (*current == split_id)
                .then_some(*ratio)
                .or_else(|| first.ratio(split_id))
                .or_else(|| second.ratio(split_id)),
        }
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

    fn validate_into(&self, seen: &mut BTreeSet<PaneId>, splits: &mut BTreeSet<SplitId>) -> bool {
        match self {
            Self::Leaf { pane_id } => seen.insert(*pane_id),
            Self::Branch {
                split_id,
                ratio,
                first,
                second,
                ..
            } => {
                splits.insert(*split_id)
                    && ratio.is_valid()
                    && first.validate_into(seen, splits)
                    && second.validate_into(seen, splits)
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

    #[test]
    fn cell_ratios_reduce_and_reproduce_exact_positions() {
        let ratio = SplitRatio::from_cells(37, 79).unwrap();
        assert_eq!(ratio.first_cells(79), 37);
        assert_eq!(
            SplitRatio::from_cells(25, 100),
            SplitRatio::from_cells(1, 4)
        );
        assert!(SplitRatio::from_cells(0, 79).is_none());
        assert!(SplitRatio::from_cells(79, 79).is_none());
    }

    #[test]
    fn wire_ratios_are_validated_and_reduced_at_deserialization() {
        let half = SplitRatio::from_cells(1, 2).unwrap();
        assert_eq!(
            serde_json::from_str::<SplitRatio>(r#"{"numerator":2,"denominator":4}"#).unwrap(),
            half
        );
        for malformed in [
            r#"{"numerator":0,"denominator":4}"#,
            r#"{"numerator":4,"denominator":4}"#,
            r#"{"numerator":1,"denominator":0}"#,
            r#"{"numerator":5,"denominator":4}"#,
        ] {
            assert!(
                serde_json::from_str::<SplitRatio>(malformed).is_err(),
                "accepted malformed ratio {malformed}"
            );
        }
    }

    #[test]
    fn branches_have_stable_unique_resize_targets() {
        let a = PaneId::new();
        let b = PaneId::new();
        let c = PaneId::new();
        let mut tree = SplitTree::leaf(a);
        assert!(tree.split(a, SplitDirection::Right, b));
        assert!(tree.split(a, SplitDirection::Down, c));
        let ids = match &tree {
            SplitTree::Branch {
                split_id: outer,
                first,
                ..
            } => match first.as_ref() {
                SplitTree::Branch {
                    split_id: inner, ..
                } => (*outer, *inner),
                _ => panic!("expected nested split"),
            },
            _ => panic!("expected split"),
        };
        assert_ne!(ids.0, ids.1);
        let ratio = SplitRatio::from_cells(2, 3).unwrap();
        assert!(tree.resize(ids.1, ratio));
        assert_eq!(tree.ratio(ids.1), Some(ratio));
        assert_eq!(tree.ratio(ids.0), Some(HALF_RATIO));
        assert!(tree.validate());
    }
}
