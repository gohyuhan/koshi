//! Layout normalization: bring a tree back to canonical shape.
//!
//! One pass does all of it:
//!
//! - leaves referencing dead panes are dropped,
//! - emptied splits are pruned,
//! - splits with a single child collapse into that child (a stack reduced
//!   to one pane becomes a plain leaf),
//! - same-direction directional splits merge into their parent when every
//!   weight involved is a plain flex share,
//! - weight values are clamped into their valid ranges, weight lists are
//!   re-paired with children, and directional splits carry
//!   `active_child_index` as `0`.
//!
//! The pass is idempotent: normalizing a normalized tree returns it
//! unchanged.

use std::collections::HashSet;

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::PaneId;

use crate::size::{FlexWeight, SizeConstraint, SizeWeight};
use crate::tree::{LayoutNode, SplitNode};

/// Normalize `layout_tree` against `live_pane_ids`, the set of panes still alive.
///
/// A leaf not in `live_panes` is dropped wherever it sits in the tree. A
/// leaf in it is kept, including a pane held open after its process exited.
///
/// Returns `None` when no live pane remains. Normalizing the returned tree
/// again returns it unchanged.
#[must_use]
pub fn normalize_layout_tree(
    layout_tree: &LayoutNode,
    live_pane_ids: &HashSet<PaneId>,
) -> Option<LayoutNode> {
    let split_node = match layout_tree {
        LayoutNode::Pane(pane_id) => {
            return live_pane_ids.contains(pane_id).then(|| layout_tree.clone());
        }
        LayoutNode::Split(split_node) => split_node,
    };

    // Each child is normalized first. Weights are re-paired by index: a
    // missing weight becomes the default share, an extra one is dropped.
    let mut normalized_children: Vec<NormalizedChild> =
        Vec::with_capacity(split_node.children.len());
    for (child_index, child_node) in split_node.children.iter().enumerate() {
        let Some(layout_node) = normalize_layout_tree(child_node, live_pane_ids) else {
            continue;
        };
        let size_weight = split_node
            .weights
            .get(child_index)
            .copied()
            .unwrap_or_default();
        normalized_children.push(NormalizedChild {
            layout_node,
            size_weight: canonicalize_size_weight(size_weight),
            original_child_index: child_index,
        });
    }
    if normalized_children.is_empty() {
        return None;
    }

    // A stack expands the first surviving child at or after the old active
    // index, or the last survivor when none remains there.
    let is_stacked = split_node.direction == SplitDirection::Stacked;
    let active_child_index = if is_stacked {
        normalized_children
            .iter()
            .position(|normalized_child| {
                normalized_child.original_child_index >= split_node.active_child_index
            })
            .unwrap_or(normalized_children.len() - 1)
    } else {
        0
    };

    if !is_stacked {
        normalized_children = merge_same_direction(split_node.direction, normalized_children);
    }

    if normalized_children.len() == 1 {
        return Some(
            normalized_children
                .into_iter()
                .next()
                .expect("checked length")
                .layout_node,
        );
    }

    let (children, size_weights) = normalized_children
        .into_iter()
        .map(|normalized_child| (normalized_child.layout_node, normalized_child.size_weight))
        .unzip();
    Some(LayoutNode::Split(SplitNode {
        direction: split_node.direction,
        children,
        weights: size_weights,
        active_child_index,
    }))
}

/// A normalized child with the index it had in the original split.
struct NormalizedChild {
    layout_node: LayoutNode,
    size_weight: SizeWeight,
    original_child_index: usize,
}

/// Inline the children of same-direction child splits into their parent.
///
/// The merge runs only when every weight involved is a plain flex share: each
/// kept sibling's weight, each merged child's slot weight, and every weight
/// inside a merged child. A floor, target, or resize offset anywhere returns
/// `normalized_children` unchanged. With `m` the inner weight sum of each merged child
/// (1 for a kept sibling) and `P` the product of every `m`, a kept sibling's
/// share becomes `w·P` and an inlined child's `u·w_slot·P/m_slot`. Every
/// share keeps its exact proportion. If `P` overflows `u128` or a rescaled
/// share exceeds `FlexWeight::MAX`, `normalized_children` is returned unchanged.
fn merge_same_direction(
    direction: SplitDirection,
    normalized_children: Vec<NormalizedChild>,
) -> Vec<NormalizedChild> {
    let merge_factors: Vec<u128> = normalized_children
        .iter()
        .map(|normalized_child| {
            compute_mergeable_weight_sum(direction, normalized_child).map_or(1, u128::from)
        })
        .collect();
    if merge_factors.iter().all(|&merge_factor| merge_factor == 1) {
        return normalized_children;
    }
    // A product past `u128::MAX` keeps the split nested.
    let merge_weight_product = merge_factors
        .iter()
        .try_fold(1u128, |accumulated_product, &merge_factor| {
            accumulated_product.checked_mul(merge_factor)
        });
    let Some(merge_weight_product) = merge_weight_product else {
        return normalized_children;
    };

    // Every rescaled weight is computed first; a share past `FlexWeight::MAX`
    // anywhere returns the normalized_children unchanged.
    let planned_size_weights: Option<Vec<Vec<SizeWeight>>> = normalized_children
        .iter()
        .zip(&merge_factors)
        .map(|(normalized_child, &merge_factor)| {
            compute_merged_size_weights(
                normalized_child,
                merge_factor,
                merge_weight_product / merge_factor,
            )
        })
        .collect();
    let Some(planned_size_weights) = planned_size_weights else {
        return normalized_children;
    };

    let mut merged_children: Vec<NormalizedChild> = Vec::with_capacity(normalized_children.len());
    for (normalized_child_index, (normalized_child, size_weights)) in normalized_children
        .into_iter()
        .zip(planned_size_weights)
        .enumerate()
    {
        if merge_factors[normalized_child_index] == 1 {
            let size_weight = size_weights[0];
            merged_children.push(NormalizedChild {
                layout_node: normalized_child.layout_node,
                size_weight,
                original_child_index: normalized_child.original_child_index,
            });
            continue;
        }
        let LayoutNode::Split(inner_split) = normalized_child.layout_node else {
            unreachable!("only splits plan multiple weights");
        };
        for (child_node, size_weight) in inner_split.children.into_iter().zip(size_weights) {
            merged_children.push(NormalizedChild {
                layout_node: child_node,
                size_weight,
                original_child_index: normalized_child.original_child_index,
            });
        }
    }
    merged_children
}

/// The weights a normalized child contributes after merging: its own rescaled share
/// when kept (`factor == 1`), or one rescaled share per inner child when
/// inlined. `None` when a rescaled share overflows `u128` or exceeds
/// `FlexWeight::MAX`, or a kept normalized_child's weight is not a plain flex share.
fn compute_merged_size_weights(
    normalized_child: &NormalizedChild,
    merge_factor: u128,
    merge_scale: u128,
) -> Option<Vec<SizeWeight>> {
    if merge_factor == 1 {
        return scaled_flex(&normalized_child.size_weight, merge_scale)
            .map(|size_weight| vec![size_weight]);
    }
    let LayoutNode::Split(inner_split) = &normalized_child.layout_node else {
        unreachable!("only splits produce a merge factor");
    };
    let slot_flex_weight_share =
        get_plain_flex_weight(&normalized_child.size_weight).expect("only plain-flex slots merge");
    inner_split
        .weights
        .iter()
        .map(|weight| {
            let inner_flex_weight_share =
                get_plain_flex_weight(weight).expect("checked by compute_mergeable_weight_sum");
            let rescaled_flex_weight = u128::from(inner_flex_weight_share)
                .checked_mul(u128::from(slot_flex_weight_share))?
                .checked_mul(merge_scale)?;
            FlexWeight::try_from(rescaled_flex_weight)
                .ok()
                .map(|share| SizeWeight::from_primary_constraint(SizeConstraint::Flex(share)))
        })
        .collect()
}

/// The sum of the inner flex weights when `normalized_child` is a split of `direction`
/// whose slot weight and inner weights are all plain flex shares and whose
/// sum fits `u32`. `None` in every other case.
fn compute_mergeable_weight_sum(
    direction: SplitDirection,
    normalized_child: &NormalizedChild,
) -> Option<u32> {
    let LayoutNode::Split(inner_split) = &normalized_child.layout_node else {
        return None;
    };
    if inner_split.direction != direction || inner_split.children.is_empty() {
        return None;
    }
    get_plain_flex_weight(&normalized_child.size_weight)?;
    let mut total_inner_flex_weight: u32 = 0;
    for size_weight in &inner_split.weights {
        total_inner_flex_weight =
            total_inner_flex_weight.checked_add(get_plain_flex_weight(size_weight)?)?;
    }
    (total_inner_flex_weight > 0).then_some(total_inner_flex_weight)
}

/// The flex share of `weight` when its primary is `Flex` with no `min`, no
/// `preferred`, and a zero `resize_delta`. `None` in every other case.
fn get_plain_flex_weight(weight: &SizeWeight) -> Option<FlexWeight> {
    match weight.primary_constraint {
        SizeConstraint::Flex(flex_weight_share)
            if weight.minimum_cell_count.is_none()
                && weight.preferred_cell_count.is_none()
                && weight.resize_delta == 0 =>
        {
            Some(flex_weight_share)
        }
        _ => None,
    }
}

/// A plain flex weight holding `weight`'s share multiplied by `scale`.
/// `None` when `weight` is not a plain flex share, the product overflows
/// `u128`, or the product exceeds `FlexWeight::MAX`.
fn scaled_flex(weight: &SizeWeight, merge_scale: u128) -> Option<SizeWeight> {
    let flex_weight_share = get_plain_flex_weight(weight)?;
    let rescaled_flex_weight =
        FlexWeight::try_from(u128::from(flex_weight_share).checked_mul(merge_scale)?).ok()?;
    Some(SizeWeight::from_primary_constraint(SizeConstraint::Flex(
        rescaled_flex_weight,
    )))
}

/// Clamp a weight into the ranges the validated constructors enforce:
/// `Flex(0)`, `Fixed(0)`, `Min(0)`, and `Preferred(0)` become `1`,
/// `Percent` clamps to 1–100, and a zero `min` or `preferred` overlay
/// becomes `None`. `resize_delta` passes through.
fn canonicalize_size_weight(size_weight: SizeWeight) -> SizeWeight {
    let canonical_primary_constraint = match size_weight.primary_constraint {
        SizeConstraint::Flex(0) => SizeConstraint::Flex(1),
        SizeConstraint::Percent(percent_value) => {
            SizeConstraint::Percent(percent_value.clamp(1, 100))
        }
        SizeConstraint::Fixed(0) => SizeConstraint::Fixed(1),
        SizeConstraint::Minimum(0) => SizeConstraint::Minimum(1),
        SizeConstraint::Preferred(0) => SizeConstraint::Preferred(1),
        unchanged_constraint => unchanged_constraint,
    };
    SizeWeight {
        primary_constraint: canonical_primary_constraint,
        minimum_cell_count: size_weight
            .minimum_cell_count
            .filter(|&cell_count| cell_count > 0),
        preferred_cell_count: size_weight
            .preferred_cell_count
            .filter(|&cell_count| cell_count > 0),
        resize_delta: size_weight.resize_delta,
    }
}

#[cfg(test)]
mod tests;
