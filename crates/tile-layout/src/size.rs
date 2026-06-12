//! Size constraints for split children.
//!
//! Each child slot of a split carries a [`SizeWeight`]: how the solver should
//! size that slot along the split axis. The tree never stores solved cell
//! rectangles — only these relative constraints — so the same tree re-solves
//! cleanly at any terminal size.
//!
//! The constraint vocabulary is complete from day one: declarative layouts
//! need the full set, and adding kinds later would force a solver rewrite.
//! All sizing is discrete cell math; nothing is stored as a bare percentage.

use serde::{Deserialize, Serialize};

/// A relative share of leftover space, used by [`SizeConstraint::Flex`].
///
/// Two flex children with weights 2 and 1 receive two thirds and one third of
/// the space remaining after fixed and percent children are placed.
pub type Weight = u32;

/// How a split child claims cells along the split axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SizeConstraint {
    /// A weighted share of the space left over after `Fixed` and `Percent`
    /// children are placed.
    Flex(Weight),
    /// A percentage (1–100) of the parent's axis, floored to whole cells.
    Percent(u8),
    /// An exact number of cells.
    Fixed(u16),
    /// A floor: behaves like `Flex(1)` but never solves below this many cells.
    Min(u16),
    /// A target honored when slack allows: behaves like `Flex(1)` that aims
    /// for this many cells.
    Preferred(u16),
}

/// The complete sizing instruction for one split child.
///
/// `primary` picks the distribution strategy; `min` and `preferred` overlay a
/// floor and a target on top of any primary; `resize_delta` records explicit
/// user resizes as exact cell offsets applied after the primary distribution.
/// Keeping resizes as deltas means a terminal resize re-solves from the same
/// intent instead of baking one screen size into the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SizeWeight {
    /// The distribution strategy for this child.
    pub primary: SizeConstraint,
    /// Floor in cells along the split axis, combinable with any primary.
    pub min: Option<u16>,
    /// Target in cells along the split axis, honored only within slack.
    pub preferred: Option<u16>,
    /// Accumulated user-resize offset in cells, applied after `primary`.
    pub resize_delta: i32,
}

impl Default for SizeWeight {
    /// An equal share: `Flex(1)` with no overrides and no resize offset.
    /// This is the weight new panes receive on insertion.
    fn default() -> Self {
        Self {
            primary: SizeConstraint::Flex(1),
            min: None,
            preferred: None,
            resize_delta: 0,
        }
    }
}

#[cfg(test)]
mod tests;
