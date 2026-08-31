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
//!   re-paired with children, and directional splits carry `active` as `0`.
//!
//! The pass is idempotent: normalizing a normalized tree returns it
//! unchanged.

use std::collections::HashSet;

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::PaneId;

use crate::size::{SizeConstraint, SizeWeight, Weight};
use crate::tree::{LayoutNode, SplitNode};

/// Normalize `tree` against `live_panes`, the set of panes still alive.
///
/// A leaf not in `live_panes` is dropped wherever it sits in the tree. A
/// leaf in it is kept, including a pane held open after its process exited.
///
/// Returns `None` when no live pane remains. Normalizing the returned tree
/// again returns it unchanged.
#[must_use]
pub fn normalize(tree: &LayoutNode, live_panes: &HashSet<PaneId>) -> Option<LayoutNode> {
    let split = match tree {
        LayoutNode::Pane(id) => {
            return live_panes.contains(id).then(|| tree.clone());
        }
        LayoutNode::Split(split) => split,
    };

    // Each child is normalized first. Weights are re-paired by index: a
    // missing weight becomes the default share, an extra one is dropped.
    let mut entries: Vec<Entry> = Vec::with_capacity(split.children.len());
    for (index, child) in split.children.iter().enumerate() {
        let Some(node) = normalize(child, live_panes) else {
            continue;
        };
        let weight = split.weights.get(index).copied().unwrap_or_default();
        entries.push(Entry {
            node,
            weight: canonical_weight(weight),
            old_index: index,
        });
    }
    if entries.is_empty() {
        return None;
    }

    // A stack expands the first surviving child at or after the old active
    // index, or the last survivor when none remains there.
    let stacked = split.direction == SplitDirection::Stacked;
    let active = if stacked {
        entries
            .iter()
            .position(|entry| entry.old_index >= split.active)
            .unwrap_or(entries.len() - 1)
    } else {
        0
    };

    if !stacked {
        entries = merge_same_direction(split.direction, entries);
    }

    if entries.len() == 1 {
        return Some(entries.into_iter().next().expect("checked length").node);
    }

    let (children, weights) = entries
        .into_iter()
        .map(|entry| (entry.node, entry.weight))
        .unzip();
    Some(LayoutNode::Split(SplitNode {
        direction: split.direction,
        children,
        weights,
        active,
    }))
}

/// A normalized child with the index it had in the original split.
struct Entry {
    node: LayoutNode,
    weight: SizeWeight,
    old_index: usize,
}

/// Inline the children of same-direction child splits into their parent.
///
/// The merge runs only when every weight involved is a plain flex share: each
/// kept sibling's weight, each merged child's slot weight, and every weight
/// inside a merged child. A floor, target, or resize offset anywhere returns
/// `entries` unchanged. With `m` the inner weight sum of each merged child
/// (1 for a kept sibling) and `P` the product of every `m`, a kept sibling's
/// share becomes `w·P` and an inlined child's `u·w_slot·P/m_slot`. Every
/// share keeps its exact proportion. If `P` overflows `u128` or a rescaled
/// share exceeds `Weight::MAX`, `entries` is returned unchanged.
fn merge_same_direction(direction: SplitDirection, entries: Vec<Entry>) -> Vec<Entry> {
    let factors: Vec<u128> = entries
        .iter()
        .map(|entry| mergeable_weight_sum(direction, entry).map_or(1, u128::from))
        .collect();
    if factors.iter().all(|&factor| factor == 1) {
        return entries;
    }
    // A product past `u128::MAX` keeps the split nested.
    let product = factors
        .iter()
        .try_fold(1u128, |acc, &factor| acc.checked_mul(factor));
    let Some(product) = product else {
        return entries;
    };

    // Every rescaled weight is computed first; a share past `Weight::MAX`
    // anywhere returns the entries unchanged.
    let planned: Option<Vec<Vec<SizeWeight>>> = entries
        .iter()
        .zip(&factors)
        .map(|(entry, &factor)| planned_weights(entry, factor, product / factor))
        .collect();
    let Some(planned) = planned else {
        return entries;
    };

    let mut merged: Vec<Entry> = Vec::with_capacity(entries.len());
    for (index, (entry, weights)) in entries.into_iter().zip(planned).enumerate() {
        if factors[index] == 1 {
            let weight = weights[0];
            merged.push(Entry {
                node: entry.node,
                weight,
                old_index: entry.old_index,
            });
            continue;
        }
        let LayoutNode::Split(inner) = entry.node else {
            unreachable!("only splits plan multiple weights");
        };
        for (child, weight) in inner.children.into_iter().zip(weights) {
            merged.push(Entry {
                node: child,
                weight,
                old_index: entry.old_index,
            });
        }
    }
    merged
}

/// The weights an entry contributes after merging: its own rescaled share
/// when kept (`factor == 1`), or one rescaled share per inner child when
/// inlined. `None` when a rescaled share overflows `u128` or exceeds
/// `Weight::MAX`, or a kept entry's weight is not a plain flex share.
fn planned_weights(entry: &Entry, factor: u128, scale: u128) -> Option<Vec<SizeWeight>> {
    if factor == 1 {
        return scaled_flex(&entry.weight, scale).map(|weight| vec![weight]);
    }
    let LayoutNode::Split(inner) = &entry.node else {
        unreachable!("only splits produce a merge factor");
    };
    let slot_share = plain_flex(&entry.weight).expect("only plain-flex slots merge");
    inner
        .weights
        .iter()
        .map(|weight| {
            let inner_share = plain_flex(weight).expect("checked by mergeable_weight_sum");
            let rescaled = u128::from(inner_share)
                .checked_mul(u128::from(slot_share))?
                .checked_mul(scale)?;
            Weight::try_from(rescaled)
                .ok()
                .map(|share| SizeWeight::new(SizeConstraint::Flex(share)))
        })
        .collect()
}

/// The sum of the inner flex weights when `entry` is a split of `direction`
/// whose slot weight and inner weights are all plain flex shares and whose
/// sum fits `u32`. `None` in every other case.
fn mergeable_weight_sum(direction: SplitDirection, entry: &Entry) -> Option<u32> {
    let LayoutNode::Split(inner) = &entry.node else {
        return None;
    };
    if inner.direction != direction || inner.children.is_empty() {
        return None;
    }
    plain_flex(&entry.weight)?;
    let mut sum: u32 = 0;
    for weight in &inner.weights {
        sum = sum.checked_add(plain_flex(weight)?)?;
    }
    (sum > 0).then_some(sum)
}

/// The flex share of `weight` when its primary is `Flex` with no `min`, no
/// `preferred`, and a zero `resize_delta`. `None` in every other case.
fn plain_flex(weight: &SizeWeight) -> Option<Weight> {
    match weight.primary {
        SizeConstraint::Flex(share)
            if weight.min.is_none() && weight.preferred.is_none() && weight.resize_delta == 0 =>
        {
            Some(share)
        }
        _ => None,
    }
}

/// A plain flex weight holding `weight`'s share multiplied by `scale`.
/// `None` when `weight` is not a plain flex share, the product overflows
/// `u128`, or the product exceeds `Weight::MAX`.
fn scaled_flex(weight: &SizeWeight, scale: u128) -> Option<SizeWeight> {
    let share = plain_flex(weight)?;
    let rescaled = Weight::try_from(u128::from(share).checked_mul(scale)?).ok()?;
    Some(SizeWeight::new(SizeConstraint::Flex(rescaled)))
}

/// Clamp a weight into the ranges the validated constructors enforce:
/// `Flex(0)`, `Fixed(0)`, `Min(0)`, and `Preferred(0)` become `1`,
/// `Percent` clamps to 1–100, and a zero `min` or `preferred` overlay
/// becomes `None`. `resize_delta` passes through.
fn canonical_weight(weight: SizeWeight) -> SizeWeight {
    let primary = match weight.primary {
        SizeConstraint::Flex(0) => SizeConstraint::Flex(1),
        SizeConstraint::Percent(p) => SizeConstraint::Percent(p.clamp(1, 100)),
        SizeConstraint::Fixed(0) => SizeConstraint::Fixed(1),
        SizeConstraint::Min(0) => SizeConstraint::Min(1),
        SizeConstraint::Preferred(0) => SizeConstraint::Preferred(1),
        valid => valid,
    };
    SizeWeight {
        primary,
        min: weight.min.filter(|&cells| cells > 0),
        preferred: weight.preferred.filter(|&cells| cells > 0),
        resize_delta: weight.resize_delta,
    }
}

#[cfg(test)]
mod tests;
