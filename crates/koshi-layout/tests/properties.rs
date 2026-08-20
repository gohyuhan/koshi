//! Property tests: random edit sequences over random tab sizes must never
//! break the layout invariants.
//!
//! Each case starts from a single pane and applies a short random sequence
//! of public edits — directional splits, stacking, removals, resizes,
//! normalization. After every step the solved layout must uphold:
//!
//! - no two panes overlap and nothing leaves the tab,
//! - every visible pane meets the minimum size,
//! - when the tree fits at minimum size, the panes tile the tab exactly,
//! - every layout leaf references a live pane,
//! - solving is deterministic.
//!
//! Failures shrink to a minimal op sequence and persist a regression seed
//! under `proptest-regressions/` — check those files in when they appear.
//!
//! One fixed op sequence runs beside the random ones. It uses every op kind
//! and asserts the exact tree and the exact solved rects it ends on, so those
//! ops are covered on every run and not only when the random search picks
//! them.

use std::collections::HashSet;

use koshi_core::geometry::{Direction, Point, Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use koshi_layout::edit::{add_to_stack, remove_pane, split_leaf};
use koshi_layout::normalize::normalize;
use koshi_layout::resize::resize;
use koshi_layout::solver::{fits, solve, StackHeader, MIN_PANE_SIZE};
use koshi_layout::tree::LayoutNode;
use koshi_test_support::layout_assert::{
    assert_all_space_occupied, assert_live_pane_refs, assert_min_size_respected, assert_no_outside,
    assert_no_overlap,
};
use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config, TestRunner};

/// One randomly chosen public edit. Targets are indices into the current
/// leaf list (taken modulo its length), so every op stays applicable as the
/// tree changes shape.
#[derive(Debug, Clone)]
enum Op {
    Split {
        target: usize,
        direction: u8,
    },
    Stack {
        target: usize,
    },
    Remove {
        target: usize,
    },
    Resize {
        target: usize,
        direction: u8,
        size: i16,
    },
    Normalize,
}

/// Returns a strategy that generates one random public edit operation.
/// Each operation targets a leaf pane by index (wraps modulo leaf count to stay applicable as the tree changes).
fn op_strategy() -> BoxedStrategy<Op> {
    Union::new(vec![
        (0..16usize, 0..4u8)
            .prop_map(|(target, direction)| Op::Split { target, direction })
            .boxed(),
        (0..16usize).prop_map(|target| Op::Stack { target }).boxed(),
        (0..16usize)
            .prop_map(|target| Op::Remove { target })
            .boxed(),
        (0..16usize, 0..4u8, -3..4i16)
            .prop_map(|(target, direction, size)| Op::Resize {
                target,
                direction,
                size,
            })
            .boxed(),
        Just(Op::Normalize).boxed(),
    ])
    .boxed()
}

/// Maps a u8 code to a [`Direction`] by taking the code modulo 4.
/// 0 → Left, 1 → Right, 2 → Up, 3 → Down.
fn direction(code: u8) -> Direction {
    match code % 4 {
        0 => Direction::Left,
        1 => Direction::Right,
        2 => Direction::Up,
        _ => Direction::Down,
    }
}

#[test]
fn random_edit_sequences_uphold_the_layout_invariants() {
    let config = Config {
        cases: 10_000,
        source_file: Some(file!()),
        ..Config::default()
    };
    let strategy = (
        prop::collection::vec(op_strategy(), 1..12),
        4..=120u16,
        2..=40u16,
    );

    TestRunner::new(config)
        .run(&strategy, |(ops, cols, rows)| {
            check_sequence(&ops, cols, rows);
            Ok(())
        })
        .unwrap();
}

/// Runs a sequence of random edits on a test tree and verifies that all layout invariants hold after each step.
/// Starts with a single pane in a tab of the given dimensions, applies each operation, and checks
/// that no panes overlap, no panes leave the tab, minimum sizes are met, space is fully occupied
/// when the tree fits, all referenced panes are still live, and solving is deterministic.
fn check_sequence(ops: &[Op], cols: u16, rows: u16) {
    let tab = Rect::at_origin(Size { cols, rows });
    let first = PaneId::new();
    let mut tree = LayoutNode::Pane(first);
    let mut live: HashSet<PaneId> = HashSet::from([first]);

    assert_invariants(&tree, tab, &live);
    for op in ops {
        apply(op, &mut tree, tab, &mut live);
        assert_invariants(&tree, tab, &live);
    }
}

/// Applies one operation through the public edit API. If the operation is rejected by the API
/// (e.g., no border to resize, attempting to remove the last pane), the tree remains unchanged.
/// This no-op behavior on invalid edits is part of the API contract and is verified by the test.
fn apply(op: &Op, tree: &mut LayoutNode, tab: Rect, live: &mut HashSet<PaneId>) {
    let leaves = tree.leaf_panes();
    let pick = |target: usize| leaves[target % leaves.len()];
    match *op {
        Op::Split {
            target,
            direction: d,
        } => {
            let new = PaneId::new();
            if let Ok(next) = split_leaf(tree, pick(target), new, direction(d)) {
                *tree = next;
                live.insert(new);
            }
        }
        Op::Stack { target } => {
            let new = PaneId::new();
            if let Ok(next) = add_to_stack(tree, pick(target), new) {
                *tree = next;
                live.insert(new);
            }
        }
        Op::Remove { target } => {
            let victim = pick(target);
            if let Ok((next, _)) = remove_pane(tree, tab, victim, MIN_PANE_SIZE) {
                *tree = next;
                live.remove(&victim);
            }
        }
        Op::Resize {
            target,
            direction: d,
            size,
        } => {
            if let Ok(next) = resize(tree, tab, pick(target), direction(d), size) {
                *tree = next;
            }
        }
        Op::Normalize => {
            if let Some(next) = normalize(tree, live) {
                *tree = next;
            }
        }
    }
}

/// Verifies the layout invariants after an edit.
/// Checks that: panes do not overlap, panes stay within the tab bounds, all visible panes meet minimum size,
/// when the tree fits at minimum size the panes exactly tile the tab with no gaps, all layout leaf references
/// point to live panes, and solving the same tree twice produces identical results.
fn assert_invariants(tree: &LayoutNode, tab: Rect, live: &HashSet<PaneId>) {
    let result = solve(tree, tab);
    assert_no_overlap(&result.panes).unwrap();
    assert_no_outside(&result.panes, tab).unwrap();
    assert_min_size_respected(&result.panes, MIN_PANE_SIZE).unwrap();
    if fits(tree, tab, MIN_PANE_SIZE) {
        assert_all_space_occupied(&result.panes, tab).unwrap();
    }
    assert_live_pane_refs(&tree.leaf_panes(), live).unwrap();
    // Solving is deterministic: solving the same tree and rect twice must produce the same placements.
    assert_eq!(solve(tree, tab), result);
}

/// Property: after any random edit sequence, normalizing the resulting tree
/// is idempotent — normalizing a normalized tree returns it unchanged.
///
/// This does NOT assert that normalizing preserves the solved layout
/// (`solve(tree) == solve(normalize(tree))`) — that stronger claim is
/// false: normalize's same-direction merge can change which panes a
/// too-small tab suppresses, by flattening a nested split's children into
/// direct siblings of a pane that previously sat outside the nested
/// split's own (failing) trailing-suppression run.
#[test]
fn normalizing_after_any_random_edit_sequence_is_idempotent() {
    let config = Config {
        cases: 2_000,
        source_file: Some(file!()),
        ..Config::default()
    };
    let strategy = (
        prop::collection::vec(op_strategy(), 1..12),
        4..=120u16,
        2..=40u16,
    );

    TestRunner::new(config)
        .run(&strategy, |(ops, cols, rows)| {
            let tab = Rect::at_origin(Size { cols, rows });
            let first = PaneId::new();
            let mut tree = LayoutNode::Pane(first);
            let mut live: HashSet<PaneId> = HashSet::from([first]);
            for op in &ops {
                apply(op, &mut tree, tab, &mut live);
            }

            let normalized =
                normalize(&tree, &live).expect("at least one live pane always survives");
            prop_assert_eq!(normalize(&normalized, &live), Some(normalized));
            Ok(())
        })
        .unwrap();
}

/// The fixed sequence replayed by [`a_fixed_op_sequence_lands_on_its_exact_layout`],
/// holding one of every op kind [`op_strategy`] can generate.
///
/// Each target is an index into the leaf list at that step: split pane 0 to
/// the right, split the new pane downward, stack a pane onto the last leaf,
/// widen pane 0 by five columns, remove leaf 1, then normalize.
const PINNED_OPS: [Op; 6] = [
    Op::Split {
        target: 0,
        direction: 1,
    },
    Op::Split {
        target: 1,
        direction: 3,
    },
    Op::Stack { target: 2 },
    Op::Resize {
        target: 0,
        direction: 1,
        size: 5,
    },
    Op::Remove { target: 1 },
    Op::Normalize,
];

/// Replay [`PINNED_OPS`] over a fixed 80x24 tab: the invariants hold after
/// every step, and the run ends on one exact tree and one exact placement.
#[test]
fn a_fixed_op_sequence_lands_on_its_exact_layout() {
    let tab = Rect::at_origin(Size { cols: 80, rows: 24 });
    let first = PaneId::new();
    let mut tree = LayoutNode::Pane(first);
    let mut live: HashSet<PaneId> = HashSet::from([first]);

    assert_invariants(&tree, tab, &live);
    for op in &PINNED_OPS {
        apply(op, &mut tree, tab, &mut live);
        assert_invariants(&tree, tab, &live);
    }

    // The removal left a column holding only the stack; normalization
    // collapsed it, so the stack now hangs straight off the root.
    let LayoutNode::Split(root) = &tree else {
        panic!("the root must be a split");
    };
    assert_eq!(root.direction, SplitDirection::Horizontal);
    assert_eq!(root.children.len(), 2);
    let LayoutNode::Split(stack) = &root.children[1].node else {
        panic!("the stack must sit directly under the root");
    };
    assert_eq!(stack.direction, SplitDirection::Stacked);
    assert_eq!(stack.active, 1);

    // Three panes survive: the resized one holds 45 of the 80 columns, and the
    // stack's 35 split into one header row plus the active member's 23.
    let leaves = tree.leaf_panes();
    assert_eq!(leaves.len(), 3);
    assert_eq!(live.len(), 3);
    let result = solve(&tree, tab);
    assert_eq!(
        result.panes,
        [
            (leaves[0], Rect::at_origin(Size { cols: 45, rows: 24 })),
            (
                leaves[1],
                Rect::new(Point { x: 45, y: 0 }, Size { cols: 35, rows: 1 })
            ),
            (
                leaves[2],
                Rect::new(Point { x: 45, y: 1 }, Size { cols: 35, rows: 23 })
            ),
        ]
    );
    assert!(result.suppressed.is_empty());
    assert!(!result.all_suppressed);
    assert_eq!(
        result.stack_headers,
        [StackHeader {
            pane: leaves[1],
            rect: Rect::new(Point { x: 45, y: 0 }, Size { cols: 35, rows: 1 }),
            position: 0,
            total: 2,
        }]
    );
}
