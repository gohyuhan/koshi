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
//! and asserts the exact tree and the exact solved rects it ends on.

use std::collections::HashSet;

use koshi_core::geometry::{Direction, Point, Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use koshi_layout::edit::{add_to_stack, remove_pane, split_leaf};
use koshi_layout::normalize::normalize;
use koshi_layout::resize::resize;
use koshi_layout::size::SizeWeight;
use koshi_layout::solver::{fits, solve, PaneSizing, SolveResult, StackHeader, MIN_PANE_SIZE};
use koshi_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use koshi_test_support::layout_assert::{
    assert_all_space_occupied, assert_live_pane_refs, assert_min_size_respected, assert_no_outside,
    assert_no_overlap,
};
use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config, TestRunner};

/// One randomly chosen public edit. A target is an index into the current
/// leaf list, taken modulo its length.
#[derive(Debug, Clone)]
enum Op {
    Split {
        target: usize,
        direction: Direction,
    },
    Stack {
        target: usize,
    },
    Remove {
        target: usize,
    },
    Resize {
        target: usize,
        direction: Direction,
        size: i16,
    },
    Normalize,
}

/// One of the four cardinal directions, each equally likely.
fn direction_strategy() -> impl Strategy<Value = Direction> {
    prop_oneof![
        Just(Direction::Left),
        Just(Direction::Right),
        Just(Direction::Up),
        Just(Direction::Down),
    ]
}

/// One random [`Op`], each kind equally likely: targets are drawn from
/// `0..16` and resize sizes from `-3..=3`.
fn op_strategy() -> BoxedStrategy<Op> {
    Union::new(vec![
        (0..16usize, direction_strategy())
            .prop_map(|(target, direction)| Op::Split { target, direction })
            .boxed(),
        (0..16usize).prop_map(|target| Op::Stack { target }).boxed(),
        (0..16usize)
            .prop_map(|target| Op::Remove { target })
            .boxed(),
        (0..16usize, direction_strategy(), -3..4i16)
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

/// Starts from one pane in a `cols` x `rows` tab, applies each op in turn,
/// and checks the invariants before the first op and after every op.
/// Returns the final tree and the set of live panes.
fn check_sequence(ops: &[Op], cols: u16, rows: u16) -> (LayoutNode, HashSet<PaneId>) {
    let tab = Rect::at_origin(Size { cols, rows });
    let first = PaneId::new();
    let mut tree = LayoutNode::Pane(first);
    let mut live: HashSet<PaneId> = HashSet::from([first]);

    assert_invariants(&tree, tab, &live);
    for op in ops {
        apply(op, &mut tree, tab, &mut live);
        assert_invariants(&tree, tab, &live);
    }
    (tree, live)
}

/// Applies one op through the public edit API. A split or stack adds its
/// new pane to `live`; a removal drops the victim from `live`. An edit the
/// API rejects (no border to resize, a resize past the donor's floor,
/// removing the last pane) leaves `tree` and `live` unchanged.
fn apply(op: &Op, tree: &mut LayoutNode, tab: Rect, live: &mut HashSet<PaneId>) {
    let leaves = tree.leaf_panes();
    let pick = |target: usize| leaves[target % leaves.len()];
    match *op {
        Op::Split { target, direction } => {
            let new = PaneId::new();
            if let Ok(next) = split_leaf(tree, pick(target), new, direction) {
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
            if let Ok((next, _)) = remove_pane(tree, tab, victim, PaneSizing::default()) {
                *tree = next;
                live.remove(&victim);
            }
        }
        Op::Resize {
            target,
            direction,
            size,
        } => {
            if let Ok(next) = resize(tree, tab, pick(target), direction, size) {
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

/// Checks the layout invariants of `tree` solved over `tab`: no two panes
/// overlap, no pane leaves the tab, every visible pane meets
/// [`MIN_PANE_SIZE`], the panes tile the tab exactly when the tree fits at
/// minimum size, every leaf names a pane in `live`, and a second solve
/// equals the first.
fn assert_invariants(tree: &LayoutNode, tab: Rect, live: &HashSet<PaneId>) {
    let result = solve(tree, tab);
    assert_no_overlap(&result.panes).unwrap();
    assert_no_outside(&result.panes, tab).unwrap();
    assert_min_size_respected(&result.panes, MIN_PANE_SIZE).unwrap();
    if fits(tree, tab, PaneSizing::default()) {
        assert_all_space_occupied(&result.panes, tab).unwrap();
    }
    assert_live_pane_refs(&tree.leaf_panes(), live).unwrap();
    assert_eq!(solve(tree, tab), result);
}

/// After any random edit sequence, normalizing the resulting tree is
/// idempotent: normalizing a normalized tree returns it unchanged.
///
/// Normalizing does not preserve the solved layout, and this test does not
/// check it. A same-direction merge multiplies the nested weights into the
/// parent, and the solver rounds the flat split differently from the nested
/// one: `vertical(vertical(a, b), c)` over 34 rows solves to rows 8, 9, 17
/// before normalization and 8, 8, 18 after.
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
            let (tree, live) = check_sequence(&ops, cols, rows);
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
        direction: Direction::Right,
    },
    Op::Split {
        target: 1,
        direction: Direction::Down,
    },
    Op::Stack { target: 2 },
    Op::Resize {
        target: 0,
        direction: Direction::Right,
        size: 5,
    },
    Op::Remove { target: 1 },
    Op::Normalize,
];

/// Replay [`PINNED_OPS`] over a fixed 80x24 tab: the invariants hold after
/// every step, and the run ends on one exact tree and one exact placement.
#[test]
fn a_fixed_op_sequence_lands_on_its_exact_layout() {
    let (tree, live) = check_sequence(&PINNED_OPS, 80, 24);
    let tab = Rect::at_origin(Size { cols: 80, rows: 24 });

    // Three panes survive, and the live set is exactly the tree's leaves.
    let leaves = tree.leaf_panes();
    assert_eq!(leaves.len(), 3);
    assert_eq!(live, leaves.iter().copied().collect::<HashSet<_>>());

    // The removal leaves a column holding only the stack, normalization
    // collapses that column, and the stack hangs straight off the root. The
    // resize moved five columns from the second root child to the first.
    assert_eq!(
        tree,
        LayoutNode::Split(SplitNode {
            direction: SplitDirection::Horizontal,
            children: vec![
                LayoutChild::new(LayoutNode::Pane(leaves[0])),
                LayoutChild::new(LayoutNode::Split(SplitNode::stack(
                    vec![leaves[1], leaves[2]],
                    1
                ))),
            ],
            weights: vec![
                SizeWeight {
                    resize_delta: 5,
                    ..SizeWeight::default()
                },
                SizeWeight {
                    resize_delta: -5,
                    ..SizeWeight::default()
                },
            ],
            active: 0,
        })
    );

    // The resized pane holds 45 of the 80 columns, and the stack's 35 split
    // into one header row plus the active member's 23.
    let header = Rect::new(Point { x: 45, y: 0 }, Size { cols: 35, rows: 1 });
    assert_eq!(
        solve(&tree, tab),
        SolveResult {
            panes: vec![
                (leaves[0], Rect::at_origin(Size { cols: 45, rows: 24 })),
                (leaves[1], header),
                (
                    leaves[2],
                    Rect::new(Point { x: 45, y: 1 }, Size { cols: 35, rows: 23 })
                ),
            ],
            suppressed: Vec::new(),
            all_suppressed: false,
            stack_headers: vec![StackHeader {
                pane: leaves[1],
                rect: header,
                position: 0,
                total: 2,
            }],
        }
    );
}
