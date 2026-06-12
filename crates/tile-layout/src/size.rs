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
use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};

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

/// A rejected constraint value. Construction is the validation boundary:
/// values from config or commands go through the constructors below, so a
/// constraint that exists is always meaningful to the solver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConstraintError {
    /// A flex weight of zero would claim no share at all.
    #[error("flex weight must be at least 1")]
    ZeroFlexWeight,
    /// Percentages outside 1–100 cannot describe a share of the axis.
    #[error("percent must be between 1 and 100, got {got}")]
    PercentOutOfRange { got: u8 },
    /// A fixed size of zero cells is not a visible pane.
    #[error("fixed size must be at least one cell")]
    ZeroFixed,
    /// A minimum of zero cells is no floor at all.
    #[error("minimum size must be at least one cell")]
    ZeroMin,
    /// A preferred size of zero cells is not a usable target.
    #[error("preferred size must be at least one cell")]
    ZeroPreferred,
}

impl DomainError for ConstraintError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

impl SizeConstraint {
    /// A validated weighted share. Weights start at 1.
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroFlexWeight`] when `weight` is zero.
    pub fn flex(weight: Weight) -> Result<Self, ConstraintError> {
        if weight == 0 {
            Err(ConstraintError::ZeroFlexWeight)
        } else {
            Ok(Self::Flex(weight))
        }
    }

    /// A validated percentage of the parent axis (1–100).
    ///
    /// # Errors
    ///
    /// [`ConstraintError::PercentOutOfRange`] when outside 1–100.
    pub fn percent(percent: u8) -> Result<Self, ConstraintError> {
        if (1..=100).contains(&percent) {
            Ok(Self::Percent(percent))
        } else {
            Err(ConstraintError::PercentOutOfRange { got: percent })
        }
    }

    /// A validated exact size in cells (at least one).
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroFixed`] when `cells` is zero.
    pub fn fixed(cells: u16) -> Result<Self, ConstraintError> {
        if cells == 0 {
            Err(ConstraintError::ZeroFixed)
        } else {
            Ok(Self::Fixed(cells))
        }
    }

    /// A validated floor in cells (at least one).
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroMin`] when `cells` is zero.
    pub fn min(cells: u16) -> Result<Self, ConstraintError> {
        if cells == 0 {
            Err(ConstraintError::ZeroMin)
        } else {
            Ok(Self::Min(cells))
        }
    }

    /// A validated target in cells (at least one).
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroPreferred`] when `cells` is zero.
    pub fn preferred(cells: u16) -> Result<Self, ConstraintError> {
        if cells == 0 {
            Err(ConstraintError::ZeroPreferred)
        } else {
            Ok(Self::Preferred(cells))
        }
    }
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

impl SizeWeight {
    /// A weight using `primary` with no overlays and no resize offset.
    /// `primary` carries its own validation, so this cannot fail.
    #[must_use]
    pub fn new(primary: SizeConstraint) -> Self {
        Self {
            primary,
            min: None,
            preferred: None,
            resize_delta: 0,
        }
    }

    /// Overlay a validated floor in cells on top of the primary constraint.
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroMin`] when `cells` is zero.
    pub fn with_min(mut self, cells: u16) -> Result<Self, ConstraintError> {
        if cells == 0 {
            return Err(ConstraintError::ZeroMin);
        }
        self.min = Some(cells);
        Ok(self)
    }

    /// Overlay a validated target in cells on top of the primary constraint.
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroPreferred`] when `cells` is zero.
    pub fn with_preferred(mut self, cells: u16) -> Result<Self, ConstraintError> {
        if cells == 0 {
            return Err(ConstraintError::ZeroPreferred);
        }
        self.preferred = Some(cells);
        Ok(self)
    }
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
