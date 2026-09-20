//! Size constraints for split children.
//!
//! Each child slot of a split carries a [`SizeWeight`]: how the solver sizes
//! that slot along the split axis. The tree stores these relative
//! constraints and no solved cell rectangles. Every constraint solves to
//! whole cells.

use koshi_core::error::{DomainCategory, DomainError, Severity};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A relative share of leftover space, used by [`SizeConstraint::Flex`].
///
/// Two flex children with weights 2 and 1 receive two thirds and one third of
/// the space remaining after fixed and percent children are placed.
pub type FlexWeight = u32;

/// How a split child claims cells along the split axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SizeConstraint {
    /// A weighted share of the space left over after `Fixed` and `Percent`
    /// children are placed.
    Flex(FlexWeight),
    /// A percentage (1–100) of the parent's axis, floored to whole cells.
    Percent(u8),
    /// An exact number of cells.
    Fixed(u16),
    /// A floor: behaves like `Flex(1)` but never solves below this many cells.
    Minimum(u16),
    /// A target honored when slack allows: behaves like `Flex(1)` that aims
    /// for this many cells.
    Preferred(u16),
}

/// A constraint value the validated constructors reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConstraintError {
    /// The flex weight is `0`.
    #[error("flex weight must be at least 1")]
    ZeroFlexWeight,
    /// The percentage is outside 1–100; `received_percent` is the rejected value.
    #[error("percent must be between 1 and 100, got {received_percent}")]
    PercentOutOfRange { received_percent: u8 },
    /// The fixed size is `0` cells.
    #[error("fixed size must be at least one cell")]
    ZeroFixedCellCount,
    /// The minimum is `0` cells.
    #[error("minimum size must be at least one cell")]
    ZeroMinimumCellCount,
    /// The preferred size is `0` cells.
    #[error("preferred size must be at least one cell")]
    ZeroPreferredCellCount,
}

impl DomainError for ConstraintError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

impl SizeConstraint {
    /// A validated weighted share. Weights start at 1.
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroFlexWeight`] when `flex_weight` is zero.
    pub fn from_flex_weight(flex_weight: FlexWeight) -> Result<Self, ConstraintError> {
        if flex_weight == 0 {
            Err(ConstraintError::ZeroFlexWeight)
        } else {
            Ok(Self::Flex(flex_weight))
        }
    }

    /// A validated percentage of the parent axis (1–100).
    ///
    /// # Errors
    ///
    /// [`ConstraintError::PercentOutOfRange`] when outside 1–100.
    pub fn from_percent(percent_value: u8) -> Result<Self, ConstraintError> {
        if (1..=100).contains(&percent_value) {
            Ok(Self::Percent(percent_value))
        } else {
            Err(ConstraintError::PercentOutOfRange {
                received_percent: percent_value,
            })
        }
    }

    /// A validated exact size in cells (at least one).
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroFixedCellCount`] when `cell_count` is zero.
    pub fn from_fixed_cell_count(cell_count: u16) -> Result<Self, ConstraintError> {
        if cell_count == 0 {
            Err(ConstraintError::ZeroFixedCellCount)
        } else {
            Ok(Self::Fixed(cell_count))
        }
    }

    /// A validated floor in cells (at least one).
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroMinimumCellCount`] when `cell_count` is zero.
    pub fn from_minimum_cell_count(cell_count: u16) -> Result<Self, ConstraintError> {
        if cell_count == 0 {
            Err(ConstraintError::ZeroMinimumCellCount)
        } else {
            Ok(Self::Minimum(cell_count))
        }
    }

    /// A validated target in cells (at least one).
    ///
    /// # Errors
    ///
    /// [`ConstraintError::ZeroPreferredCellCount`] when `cell_count` is zero.
    pub fn from_preferred_cell_count(cell_count: u16) -> Result<Self, ConstraintError> {
        if cell_count == 0 {
            Err(ConstraintError::ZeroPreferredCellCount)
        } else {
            Ok(Self::Preferred(cell_count))
        }
    }
}

/// The complete sizing instruction for one split child.
///
/// `primary_constraint` picks the distribution strategy; `minimum_cell_count` and
/// `preferred_cell_count` overlay a
/// floor and a target on top of any primary; `resize_delta` is the
/// accumulated user resize in cells, applied after the primary distribution
/// on every solve, at any terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SizeWeight {
    /// The distribution strategy for this child.
    pub primary_constraint: SizeConstraint,
    /// Floor in cells along the split axis: a guaranteed minimum applied
    /// after the primary distribution, whatever the primary is. Combinable
    /// with `preferred_cell_count` — both overlays may be set at once.
    pub minimum_cell_count: Option<u16>,
    /// Target in cells along the split axis, honored only with slack that
    /// flexible siblings can give after the primary distribution and
    /// without pushing anyone below a floor. Combinable with
    /// `minimum_cell_count`.
    pub preferred_cell_count: Option<u16>,
    /// Accumulated user-resize offset in cells, applied after `primary_constraint`.
    pub resize_delta: i32,
}

impl SizeWeight {
    /// A weight using `primary_constraint` with no overlays and no resize offset.
    #[must_use]
    pub fn from_primary_constraint(primary_constraint: SizeConstraint) -> Self {
        Self {
            primary_constraint,
            minimum_cell_count: None,
            preferred_cell_count: None,
            resize_delta: 0,
        }
    }
}

impl Default for SizeWeight {
    /// An equal share: `Flex(1)` with no overlays and no resize offset.
    fn default() -> Self {
        Self::from_primary_constraint(SizeConstraint::Flex(1))
    }
}

#[cfg(test)]
mod tests;
