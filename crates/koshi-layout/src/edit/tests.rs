//! Tests for structural edits: split (directional pane splits), stack (tabbed pane groups),
//! and remove (delete pane from tree).
//!
//! Tests verify that edits produce correct tree structure, maintain tiling (no gaps/overlaps),
//! update cursor position in stacks, and handle edge cases (removing last pane, missing targets).

use koshi_core::geometry::{Point, Rect, Size};
use koshi_test_support::layout_assert::{
    assert_all_space_occupied, assert_no_outside, assert_no_overlap,
};

use super::*;
use crate::size::SizeWeight;
use crate::solver::{solve, MIN_PANE_SIZE};

/// Wraps a single pane ID as a leaf node ready to insert into a tree.
fn leaf(pane: PaneId) -> LayoutChild {
    LayoutChild::new(LayoutNode::Pane(pane))
}

/// Creates a split node with two equally-weighted pane children in the given direction.
fn pair(direction: SplitDirection, a: PaneId, b: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        direction,
        vec![leaf(a), leaf(b)],
    ))
}

/// The split node that replaced the target leaf, wherever it ended up.
fn find_split_of(tree: &LayoutNode, member: PaneId) -> &SplitNode {
    match tree {
        LayoutNode::Pane(_) => panic!("expected a split in {tree:?}"),
        LayoutNode::Split(split) => {
            if split
                .children
                .iter()
                .any(|child| matches!(child.node, LayoutNode::Pane(id) if id == member))
            {
                split
            } else {
                split
                    .children
                    .iter()
                    .find_map(|child| {
                        child
                            .node
                            .contains_pane(member)
                            .then(|| find_split_of(&child.node, member))
                    })
                    .expect("member not found")
            }
        }
    }
}

#[test]
fn split_right_places_the_new_pane_after_the_target() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Right).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.direction, SplitDirection::Horizontal);
    assert_eq!(split.leaf_panes(), [target, new]);
}

#[test]
fn split_left_places_the_new_pane_before_the_target() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Left).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.direction, SplitDirection::Horizontal);
    assert_eq!(split.leaf_panes(), [new, target]);
}

#[test]
fn split_down_stacks_the_new_pane_below() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Down).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.direction, SplitDirection::Vertical);
    assert_eq!(split.leaf_panes(), [target, new]);
}

#[test]
fn split_up_stacks_the_new_pane_above() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Up).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.direction, SplitDirection::Vertical);
    assert_eq!(split.leaf_panes(), [new, target]);
}

#[test]
fn new_siblings_share_space_equally() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Right).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.weights, [SizeWeight::default(), SizeWeight::default()]);
}

#[test]
fn splitting_a_nested_leaf_touches_only_that_leaf() {
    let (a, b, new) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let split = split_leaf(&tree, b, new, Direction::Down).unwrap();
    assert_eq!(split.leaf_panes(), [a, b, new]);
    // The original left side is untouched; only b's slot became a split.
    let inner = find_split_of(&split, b);
    assert_eq!(inner.direction, SplitDirection::Vertical);
    assert_eq!(inner.children.len(), 2);
}

#[test]
fn split_result_still_tiles_the_tab() {
    let (a, b, new) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let split = split_leaf(&tree, a, new, Direction::Down).unwrap();

    let tab = Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 24 });
    let result = solve(&split, tab);
    assert_all_space_occupied(&result.panes, tab).unwrap();
    assert_no_overlap(&result.panes).unwrap();
    assert_no_outside(&result.panes, tab).unwrap();
}

/// Returns a standard test tab size: 80 columns × 24 rows at origin (0, 0).
fn tab() -> Rect {
    Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 24 })
}

/// Verifies that a solved layout completely tiles the tab with no gaps, overlaps, or panes outside bounds.
fn assert_tiles(tree: &LayoutNode, tab: Rect) {
    let result = solve(tree, tab);
    assert_all_space_occupied(&result.panes, tab).unwrap();
    assert_no_overlap(&result.panes).unwrap();
    assert_no_outside(&result.panes, tab).unwrap();
}

#[test]
fn removing_a_middle_pane_reflows_with_no_dead_region() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(b), leaf(c)],
    ));

    let (removed, info) = remove_pane(&tree, tab(), b, MIN_PANE_SIZE).unwrap();
    assert_eq!(removed.leaf_panes(), [a, c]);
    assert_tiles(&removed, tab());

    // Before: a 0..26, b 26..53, c 53..80. After: a 0..40 takes 14 of b's
    // columns, c 40..80 takes 13 — a absorbed more, so it leads.
    assert_eq!(
        info.old_rect,
        Rect::new(Point { x: 26, y: 0 }, Size { cols: 27, rows: 24 })
    );
    assert_eq!(info.absorbed_by, [a, c]);
}

#[test]
fn removing_a_siblingless_leaf_prunes_the_emptied_split() {
    // a beside a column holding only b: removing b must not leave an empty
    // split claiming dead space.
    let (a, b) = (PaneId::new(), PaneId::new());
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(column)],
    ));

    let (removed, info) = remove_pane(&tree, tab(), b, MIN_PANE_SIZE).unwrap();
    assert_eq!(removed.leaf_panes(), [a]);
    assert_tiles(&removed, tab());
    assert_eq!(info.absorbed_by, [a]);
}

#[test]
fn removing_the_last_pane_in_a_split_leaves_a_unary_split_for_normalization() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(column)],
    ));

    let (removed, _) = remove_pane(&tree, tab(), c, MIN_PANE_SIZE).unwrap();
    assert_eq!(removed.leaf_panes(), [a, b]);
    assert_tiles(&removed, tab());
    // The column still exists with one child; normalization collapses it.
    let LayoutNode::Split(outer) = &removed else {
        panic!("root must stay a split");
    };
    let LayoutNode::Split(inner) = &outer.children[1].node else {
        panic!("column must survive as a unary split");
    };
    assert_eq!(inner.children.len(), 1);
    assert_eq!(inner.weights.len(), 1);
}

#[test]
fn absorbed_by_skips_collapsed_stack_members() {
    // x beside a stack: removing x widens the stack, so the collapsed
    // member's header strip crosses x's old rect. Only the active member
    // absorbed real content space; the header must not be listed.
    let (x, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(x), LayoutChild::new(stack)],
    ));

    let (removed, info) = remove_pane(&tree, tab(), x, MIN_PANE_SIZE).unwrap();
    let solved = solve(&removed, tab());
    assert_eq!(solved.stack_headers.len(), 1);
    assert_eq!(solved.stack_headers[0].pane, c);
    assert!(solved.stack_headers[0]
        .rect
        .intersection(info.old_rect)
        .is_some());
    assert_eq!(info.absorbed_by, [b]);
}

#[test]
fn absorbed_by_lists_the_regrown_active_member_of_a_shrunk_stack() {
    // Removing the bottom collapsed member frees only its header row, which
    // the surviving header slides down onto — the active member regrows
    // above it without ever crossing the freed strip. It still changed
    // size, so it must be reported for the PTY resize.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c], 0));

    let (removed, info) = remove_pane(&tree, tab(), c, MIN_PANE_SIZE).unwrap();
    let solved = solve(&removed, tab());
    let a_rect = solved.panes.iter().find(|&&(id, _)| id == a).unwrap().1;
    assert!(a_rect.intersection(info.old_rect).is_none());
    assert_eq!(info.absorbed_by, [a]);
}

#[test]
fn absorbed_by_includes_resized_panes_beyond_the_freed_rect() {
    // Four equal columns; removing the third resizes every survivor, but
    // the leftmost one's new rect never reaches the freed span. It is
    // still listed — last, after the panes that absorbed actual cells.
    let (a, b, x, c) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(b), leaf(x), leaf(c)],
    ));

    let (removed, info) = remove_pane(&tree, tab(), x, MIN_PANE_SIZE).unwrap();
    let solved = solve(&removed, tab());
    let a_rect = solved.panes.iter().find(|&&(id, _)| id == a).unwrap().1;
    assert!(a_rect.intersection(info.old_rect).is_none());
    assert_eq!(info.absorbed_by, [b, c, a]);
}

#[test]
fn absorbed_by_keeps_layout_order_on_an_exact_tie() {
    let (a, x, b) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(x), leaf(b)],
    ));
    let wide = Rect::new(Point { x: 0, y: 0 }, Size { cols: 90, rows: 24 });

    // Three even 30-column panes; removing the middle one leaves a 50/50
    // split where both survivors absorb exactly 15 of its freed columns —
    // an exact tie, broken by layout order.
    let (removed, info) = remove_pane(&tree, wide, x, MIN_PANE_SIZE).unwrap();
    assert_eq!(removed.leaf_panes(), [a, b]);
    assert_eq!(info.absorbed_by, [a, b]);
}

#[test]
fn remove_pane_measures_the_freed_rect_against_the_given_min() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let tab = Rect::new(Point { x: 0, y: 0 }, Size { cols: 12, rows: 24 });

    // Under the default floor both panes fit, so `a` freed only its half.
    let (_, small) = remove_pane(&tree, tab, a, MIN_PANE_SIZE).unwrap();
    assert_eq!(
        small.old_rect,
        Rect::new(Point { x: 0, y: 0 }, Size { cols: 6, rows: 24 })
    );

    // An 8-column floor needs ten bordered columns per pane, so `b` is
    // suppressed and `a` owned the whole tab — its freed rect is the full width.
    // Fails if remove_pane ignores `min`.
    let (_, large) = remove_pane(&tree, tab, a, Size { cols: 8, rows: 1 }).unwrap();
    assert_eq!(
        large.old_rect,
        Rect::new(Point { x: 0, y: 0 }, Size { cols: 12, rows: 24 })
    );
}

#[test]
fn removing_a_suppressed_pane_reports_a_zero_area_old_rect() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(b), leaf(c)],
    ));
    // Three bordered panes need twelve columns; nine fits only a and b, so
    // c solves to a zero-area suppressed rect before removal.
    let narrow = Rect::new(Point { x: 0, y: 0 }, Size { cols: 9, rows: 24 });

    let (removed, info) = remove_pane(&tree, narrow, c, MIN_PANE_SIZE).unwrap();
    assert_eq!(removed.leaf_panes(), [a, b]);
    assert_eq!(info.old_rect, Rect::zero());
    // a and b were already at their final floor-clamped sizes; losing the
    // already-invisible c changes nothing about them.
    assert!(info.absorbed_by.is_empty());
}

#[test]
fn removing_the_active_stack_child_activates_the_next_one() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c], 1));

    let (removed, _) = remove_pane(&tree, tab(), b, MIN_PANE_SIZE).unwrap();
    let LayoutNode::Split(stack) = &removed else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active, 1);
    let collapsed: Vec<bool> = stack.children.iter().map(|child| child.collapsed).collect();
    assert_eq!(collapsed, [true, false]);
    assert_eq!(removed.leaf_panes(), [a, c]);
}

#[test]
fn removing_the_last_active_stack_child_steps_back() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b], 1));

    let (removed, _) = remove_pane(&tree, tab(), b, MIN_PANE_SIZE).unwrap();
    let LayoutNode::Split(stack) = &removed else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active, 0);
    assert!(!stack.children[0].collapsed);
}

#[test]
fn removing_before_the_active_stack_child_keeps_it_active() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c], 2));

    let (removed, _) = remove_pane(&tree, tab(), a, MIN_PANE_SIZE).unwrap();
    let LayoutNode::Split(stack) = &removed else {
        panic!("stack must survive");
    };
    // c is still the active pane, now at index 1.
    assert_eq!(stack.active, 1);
    assert!(!stack.children[1].collapsed);
    assert_eq!(removed.leaf_panes(), [b, c]);
}

#[test]
fn removing_after_the_active_stack_child_keeps_it_active() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c], 1));

    let (removed, _) = remove_pane(&tree, tab(), c, MIN_PANE_SIZE).unwrap();
    let LayoutNode::Split(stack) = &removed else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active, 1);
    assert!(!stack.children[1].collapsed);
    assert_eq!(removed.leaf_panes(), [a, b]);
}

#[test]
fn a_stack_reduced_to_one_member_normalizes_to_a_plain_leaf() {
    use std::collections::HashSet;

    use crate::normalize::normalize;

    let (a, b, x) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![a, b], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(x), LayoutChild::new(stack)],
    ));

    let (removed, _) = remove_pane(&tree, tab(), a, MIN_PANE_SIZE).unwrap();
    let live: HashSet<PaneId> = [x, b].into_iter().collect();
    let normalized = normalize(&removed, &live).unwrap();

    let LayoutNode::Split(outer) = &normalized else {
        panic!("root must stay a split");
    };
    // The one-member stack collapsed into b's plain leaf.
    assert_eq!(outer.children[1].node, LayoutNode::Pane(b));
    assert_tiles(&normalized, tab());
    // No header strip remains for a pane that is no longer stacked.
    assert!(solve(&normalized, tab()).stack_headers.is_empty());
}

#[test]
fn a_non_active_stack_member_keeps_its_header_and_stays_selectable() {
    use std::collections::HashSet;

    use crate::focus::stack_activate;
    use crate::normalize::normalize;

    let (active, inactive) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![active, inactive], 0));

    // Both panes are in the live set, so normalization keeps the whole stack.
    let live: HashSet<PaneId> = [active, inactive].into_iter().collect();
    let mut normalized = normalize(&tree, &live).unwrap();
    assert_eq!(normalized.leaf_panes(), [active, inactive]);

    // The non-active member's header is still drawn, and it can be activated.
    let result = solve(&normalized, tab());
    assert_eq!(result.stack_headers.len(), 1);
    assert_eq!(result.stack_headers[0].pane, inactive);

    let stack = normalized.stack_containing_mut(inactive).unwrap();
    let change = stack_activate(stack, inactive).unwrap();
    assert_eq!(change.newly_active, inactive);
}

#[test]
fn removing_the_last_stack_member_prunes_the_stack() {
    let (x, a) = (PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![a], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(x), LayoutChild::new(stack)],
    ));

    let (removed, info) = remove_pane(&tree, tab(), a, MIN_PANE_SIZE).unwrap();
    assert_eq!(removed.leaf_panes(), [x]);
    assert_eq!(info.absorbed_by, [x]);
    assert_tiles(&removed, tab());
}

/// A left-leaning tree of `panes.len() - 1` splits alternating
/// horizontal/vertical by depth. Index 0 is the outermost leaf, the last
/// index the deepest.
fn deep_alternating(panes: &[PaneId]) -> LayoutNode {
    let (&last, rest) = panes.split_last().expect("need at least one pane");
    let mut node = LayoutNode::Pane(last);
    for (index, &pane) in rest.iter().enumerate().rev() {
        let direction = if index % 2 == 0 {
            SplitDirection::Horizontal
        } else {
            SplitDirection::Vertical
        };
        node = LayoutNode::Split(SplitNode::with_equal_weights(
            direction,
            vec![leaf(pane), LayoutChild::new(node)],
        ));
    }
    node
}

/// Close every pane except `keep_index`, visiting victims in `order`,
/// normalizing after each removal. A big tab keeps every survivor fitting, so
/// the layout must tile exactly and solve deterministically at every step, and
/// end as the single kept leaf.
fn close_all_but_one_in_order(order: &[usize], keep_index: usize) {
    use std::collections::HashSet;

    use crate::normalize::normalize;

    let panes: Vec<PaneId> = (0..51).map(|_| PaneId::new()).collect();
    let mut tree = deep_alternating(&panes);
    let big = Rect::new(
        Point { x: 0, y: 0 },
        Size {
            cols: 1000,
            rows: 1000,
        },
    );
    let mut live: HashSet<PaneId> = panes.iter().copied().collect();

    for &index in order {
        assert_ne!(index, keep_index, "the kept pane is never removed");
        let victim = panes[index];
        let (next, _) = remove_pane(&tree, big, victim, MIN_PANE_SIZE).unwrap();
        live.remove(&victim);
        tree = normalize(&next, &live).unwrap();

        // Every surviving leaf is still live, the layout tiles the big tab
        // exactly, and solving twice agrees.
        for pane in tree.leaf_panes() {
            assert!(live.contains(&pane), "dead pane {pane} left in the tree");
        }
        assert_tiles(&tree, big);
        assert_eq!(solve(&tree, big), solve(&tree, big));
    }

    assert_eq!(tree, LayoutNode::Pane(panes[keep_index]));
}

#[test]
fn deep_tree_closed_newest_first_collapses_to_the_outermost_pane() {
    // LIFO: remove the deepest (last-created) leaf first, up to the outermost.
    let order: Vec<usize> = (1..51).rev().collect();
    close_all_but_one_in_order(&order, 0);
}

#[test]
fn deep_tree_closed_oldest_first_collapses_to_the_deepest_pane() {
    // FIFO: remove the outermost leaf first, down to the deepest.
    let order: Vec<usize> = (0..50).collect();
    close_all_but_one_in_order(&order, 50);
}

#[test]
fn deep_tree_closed_in_a_fixed_scrambled_order_stays_consistent() {
    // A fixed permutation (i*20 mod 51 is a full cycle since 20 and 51 are
    // coprime), skipping the pane we keep. Same invariants, arbitrary order.
    let keep = 25;
    let order: Vec<usize> = (0..51)
        .map(|i| (i * 20) % 51)
        .filter(|&index| index != keep)
        .collect();
    assert_eq!(order.len(), 50);
    close_all_but_one_in_order(&order, keep);
}

#[test]
fn splitting_a_removed_pane_is_rejected_then_a_live_pane_still_splits() {
    // Remove a pane, then try to split the now-dead id: rejected, tree
    // unchanged. The next split against a live pane still works.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let (after_remove, _) = remove_pane(&tree, tab(), a, MIN_PANE_SIZE).unwrap();
    assert_eq!(after_remove.leaf_panes(), [b]);

    let snapshot = after_remove.clone();
    let err = split_leaf(&after_remove, a, PaneId::new(), Direction::Right).unwrap_err();
    assert_eq!(err, SplitError::PaneNotFound { target: a });
    assert_eq!(after_remove, snapshot);

    let new = PaneId::new();
    let split = split_leaf(&after_remove, b, new, Direction::Right).unwrap();
    assert_eq!(split.leaf_panes(), [b, new]);
}

#[test]
fn removing_the_last_pane_is_rejected_then_it_can_still_be_split() {
    // The last pane cannot be removed, but the rejection leaves it intact and
    // a following split succeeds.
    let a = PaneId::new();
    let tree = LayoutNode::Pane(a);
    let err = remove_pane(&tree, tab(), a, MIN_PANE_SIZE).unwrap_err();
    assert_eq!(err, RemoveError::LastPane { pane: a });
    assert_eq!(tree, LayoutNode::Pane(a));

    let new = PaneId::new();
    let split = split_leaf(&tree, a, new, Direction::Down).unwrap();
    assert_eq!(split.leaf_panes(), [a, new]);
}

#[test]
fn removing_stack_members_until_one_remains_then_normalizing_gives_a_leaf() {
    use std::collections::HashSet;

    use crate::normalize::normalize;

    // A four-member stack, closed one member at a time. The stack keeps
    // exactly one expanded child throughout, and the final survivor
    // normalizes to a plain leaf with no header.
    let (a, b, c, d) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let mut tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c, d], 1));

    for victim in [d, a, b] {
        let (next, _) = remove_pane(&tree, tab(), victim, MIN_PANE_SIZE).unwrap();
        tree = next;
        // After every removal exactly one child stays expanded.
        if let LayoutNode::Split(stack) = &tree {
            let expanded = stack.children.iter().filter(|c| !c.collapsed).count();
            assert_eq!(expanded, 1, "a stack always has one expanded member");
        }
    }
    assert_eq!(tree.leaf_panes(), [c]);

    let live: HashSet<PaneId> = [c].into_iter().collect();
    let normalized = normalize(&tree, &live).unwrap();
    assert_eq!(normalized, LayoutNode::Pane(c));
    assert!(solve(&normalized, tab()).stack_headers.is_empty());
}

#[test]
fn removing_the_only_pane_is_rejected() {
    let a = PaneId::new();
    let tree = LayoutNode::Pane(a);
    let err = remove_pane(&tree, tab(), a, MIN_PANE_SIZE).unwrap_err();
    assert_eq!(err, RemoveError::LastPane { pane: a });
}

#[test]
fn removing_a_missing_pane_is_rejected_and_the_input_is_unchanged() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let snapshot = tree.clone();

    let missing = PaneId::new();
    let err = remove_pane(&tree, tab(), missing, MIN_PANE_SIZE).unwrap_err();
    assert_eq!(err, RemoveError::PaneNotFound { pane: missing });
    assert_eq!(tree, snapshot);
}

#[test]
fn stacking_onto_a_plain_pane_creates_a_stack_with_the_new_pane_active() {
    let (a, b, n) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let stacked = add_to_stack(&tree, b, n).unwrap();
    assert_eq!(stacked.leaf_panes(), [a, b, n]);
    let LayoutNode::Split(outer) = &stacked else {
        panic!("root must stay a split");
    };
    let LayoutNode::Split(stack) = &outer.children[1].node else {
        panic!("b's slot must become a stack");
    };
    assert_eq!(stack.direction, SplitDirection::Stacked);
    assert_eq!(stack.active, 1);
    let collapsed: Vec<bool> = stack.children.iter().map(|child| child.collapsed).collect();
    assert_eq!(collapsed, [true, false]);
}

#[test]
fn stacking_onto_a_stack_member_appends_to_that_stack() {
    let (a, b, n) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b], 0));

    let stacked = add_to_stack(&tree, a, n).unwrap();
    let LayoutNode::Split(stack) = &stacked else {
        panic!("stack must survive");
    };
    assert_eq!(stack.children.len(), 3);
    assert_eq!(stack.weights.len(), 3);
    assert_eq!(stack.active, 2);
    assert_eq!(stacked.leaf_panes(), [a, b, n]);
    let collapsed: Vec<bool> = stack.children.iter().map(|child| child.collapsed).collect();
    assert_eq!(collapsed, [true, true, false]);
}

#[test]
fn stacked_layout_still_tiles_after_the_edit() {
    let (a, b, n) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let stacked = add_to_stack(&tree, b, n).unwrap();
    assert_tiles(&stacked, tab());
}

#[test]
fn a_directional_split_treats_the_whole_stack_as_one_operand() {
    let (x, a, b, n) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![a, b], 1));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(x), LayoutChild::new(stack.clone())],
    ));

    // Splitting downward from a stack member puts the new pane under the
    // stack, with the stack itself intact above it.
    let split = split_leaf(&tree, b, n, Direction::Down).unwrap();
    let LayoutNode::Split(outer) = &split else {
        panic!("root must stay a split");
    };
    let LayoutNode::Split(column) = &outer.children[1].node else {
        panic!("the stack's slot must become a vertical split");
    };
    assert_eq!(column.direction, SplitDirection::Vertical);
    assert_eq!(column.children[0].node, stack);
    assert_eq!(column.children[1].node, LayoutNode::Pane(n));
    assert_eq!(split.leaf_panes(), [x, a, b, n]);
}

#[test]
fn a_directional_split_before_a_stack_places_the_new_pane_first() {
    let (a, b, n) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![a, b], 0));

    let split = split_leaf(&stack, a, n, Direction::Left).unwrap();
    let LayoutNode::Split(row) = &split else {
        panic!("root must become a split");
    };
    assert_eq!(row.direction, SplitDirection::Horizontal);
    assert_eq!(row.children[0].node, LayoutNode::Pane(n));
    assert_eq!(row.children[1].node, stack);
}

#[test]
fn stacking_onto_a_missing_anchor_is_rejected() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let snapshot = tree.clone();

    let missing = PaneId::new();
    let err = add_to_stack(&tree, missing, PaneId::new()).unwrap_err();
    assert_eq!(err, SplitError::PaneNotFound { target: missing });
    assert_eq!(tree, snapshot);
}

#[test]
fn missing_target_is_an_error_and_the_input_is_unchanged() {
    let (a, b, new) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let snapshot = tree.clone();

    let missing = PaneId::new();
    let err = split_leaf(&tree, missing, new, Direction::Right).unwrap_err();
    assert_eq!(err, SplitError::PaneNotFound { target: missing });
    assert_eq!(tree, snapshot);
}
