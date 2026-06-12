//! Layout normalization: bring a tree back to canonical shape after edits.
//!
//! Edits deliberately leave debris — removal keeps unary splits, splitting
//! nests same-direction splits — so each edit stays small and obviously
//! correct. Normalization is the one pass that cleans all of it up:
//!
//! - leaves referencing dead panes are dropped,
//! - emptied splits are pruned,
//! - splits with a single child collapse into that child (a stack reduced
//!   to one pane becomes a plain leaf),
//! - same-direction directional splits merge into their parent when the
//!   merge cannot change the solved layout,
//! - weight values are clamped into their valid ranges, weight lists are
//!   re-paired with children, and stacks keep exactly one expanded child.
//!
//! The pass is idempotent: normalizing a normalized tree returns it
//! unchanged. Callers run it after removals and on restored snapshots, not
//! on every solve.

use std::collections::HashSet;

use tile_core::geometry::SplitDirection;
use tile_core::ids::PaneId;

use crate::size::{SizeConstraint, SizeWeight, Weight};
use crate::tree::{LayoutChild, LayoutNode, SplitNode};

/// Normalize `tree` against the set of panes that are still alive.
///
/// Returns `None` when no live pane remains — there is no layout left and
/// the caller closes the tab. A pane held open after its process exits is
/// still a live pane (it has a visible placeholder), so it survives here;
/// dropping it is the caller's explicit removal, not normalization's.
#[must_use]
pub fn normalize(tree: &LayoutNode, live_panes: &HashSet<PaneId>) -> Option<LayoutNode> {
    normalize_node(tree, live_panes)
}

fn normalize_node(node: &LayoutNode, live: &HashSet<PaneId>) -> Option<LayoutNode> {
    let split = match node {
        LayoutNode::Pane(id) => {
            return live.contains(id).then(|| node.clone());
        }
        LayoutNode::Split(split) => split,
    };

    // Children first, so every fix below sees already-canonical subtrees.
    // Weights are re-paired by index here; a missing weight becomes the
    // default share.
    let mut entries: Vec<Entry> = Vec::with_capacity(split.children.len());
    for (index, child) in split.children.iter().enumerate() {
        let Some(node) = normalize_node(&child.node, live) else {
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

    // A stack's active child must survive reseating before any merging —
    // though merging never applies to stacks, keeping the order explicit.
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
        .enumerate()
        .map(|(index, entry)| {
            (
                LayoutChild {
                    node: entry.node,
                    collapsed: stacked && index != active,
                },
                entry.weight,
            )
        })
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

/// Inline children of same-direction child splits into their parent, when
/// doing so provably cannot change the solved layout.
///
/// A merge is safe only when the child split's slot and all of its inner
/// weights are plain flex shares (no floors, targets, or resize offsets).
/// Exactness then comes from rescaling: with `m` the inner weight sum of
/// each merged child (1 for the rest) and `P` their product, a kept child's
/// weight becomes `w·P/m_self`, and an inlined child's `u·w_slot·P/m_slot`.
/// Every share keeps its exact proportion. If any rescaled weight would
/// overflow, nothing merges — a nested split is valid, just not canonical.
fn merge_same_direction(direction: SplitDirection, entries: Vec<Entry>) -> Vec<Entry> {
    let factors: Vec<u128> = entries
        .iter()
        .map(|entry| mergeable_weight_sum(direction, entry).map_or(1, u128::from))
        .collect();
    if factors.iter().all(|&factor| factor == 1) {
        return entries;
    }
    // The rescaling factor product can overflow only on absurdly deep
    // hostile trees, but a nested split is valid — so refuse the merge
    // rather than wrap.
    let product = factors
        .iter()
        .try_fold(1u128, |acc, &factor| acc.checked_mul(factor));
    let Some(product) = product else {
        return entries;
    };

    // Plan every rescaled weight before touching the tree, so an overflow
    // anywhere aborts the merge with the entries untouched.
    let mut planned: Vec<Vec<SizeWeight>> = Vec::with_capacity(entries.len());
    for index in 0..entries.len() {
        let scale = product / factors[index];
        match planned_weights(&entries[index], factors[index], scale) {
            Some(weights) => planned.push(weights),
            None => return entries,
        }
    }

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
                node: child.node,
                weight,
                old_index: entry.old_index,
            });
        }
    }
    merged
}

/// The weights an entry contributes after merging: its own rescaled share
/// when kept, or one rescaled share per inner child when inlined. `None`
/// when any rescale overflows.
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
            let rescaled = u128::from(inner_share) * u128::from(slot_share) * scale;
            Weight::try_from(rescaled)
                .ok()
                .map(|share| SizeWeight::new(SizeConstraint::Flex(share)))
        })
        .collect()
}

/// When this entry is a safely mergeable same-direction split, the sum of
/// its inner flex weights; `None` otherwise.
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

/// The flex share of a weight that is nothing but a flex share.
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

/// Multiply a plain flex weight's share by `scale`, refusing on overflow or
/// when the weight is not plain flex (those entries block merging instead).
fn scaled_flex(weight: &SizeWeight, scale: u128) -> Option<SizeWeight> {
    let share = plain_flex(weight)?;
    let rescaled = Weight::try_from(u128::from(share) * scale).ok()?;
    Some(SizeWeight::new(SizeConstraint::Flex(rescaled)))
}

/// Clamp a weight's values into the ranges the validated constructors
/// enforce, so a tree from an untrusted snapshot solves like a built one.
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
