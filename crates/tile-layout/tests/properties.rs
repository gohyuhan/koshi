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

use std::collections::HashSet;

use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config, TestRunner};
use tile_core::geometry::{Direction, Point, Rect, Size};
use tile_core::ids::PaneId;
use tile_layout::edit::{add_to_stack, remove_pane, split_leaf};
use tile_layout::normalize::normalize;
use tile_layout::resize::resize;
use tile_layout::solver::{fits, solve, MIN_PANE_SIZE};
use tile_layout::tree::LayoutNode;
use tile_test_support::layout_assert::{
    assert_all_space_occupied, assert_live_pane_refs, assert_min_size_respected, assert_no_outside,
    assert_no_overlap,
};

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
        amount: u16,
    },
    Normalize,
}

fn op_strategy() -> BoxedStrategy<Op> {
    Union::new(vec![
        (0..16usize, 0..4u8)
            .prop_map(|(target, direction)| Op::Split { target, direction })
            .boxed(),
        (0..16usize).prop_map(|target| Op::Stack { target }).boxed(),
        (0..16usize)
            .prop_map(|target| Op::Remove { target })
            .boxed(),
        (0..16usize, 0..4u8, 1..4u16)
            .prop_map(|(target, direction, amount)| Op::Resize {
                target,
                direction,
                amount,
            })
            .boxed(),
        Just(Op::Normalize).boxed(),
    ])
    .boxed()
}

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

fn check_sequence(ops: &[Op], cols: u16, rows: u16) {
    let tab = Rect::new(Point { x: 0, y: 0 }, Size { cols, rows });
    let first = PaneId::new();
    let mut tree = LayoutNode::Pane(first);
    let mut live: HashSet<PaneId> = HashSet::from([first]);

    assert_invariants(&tree, tab, &live);
    for op in ops {
        apply(op, &mut tree, tab, &mut live);
        assert_invariants(&tree, tab, &live);
    }
}

/// Apply one op through the public API. A rejected edit (no border to
/// resize, last pane, …) leaves the tree as it was — that is itself part
/// of the contract under test.
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
            if let Ok((next, _)) = remove_pane(tree, tab, victim) {
                *tree = next;
                live.remove(&victim);
            }
        }
        Op::Resize {
            target,
            direction: d,
            amount,
        } => {
            if let Ok(next) = resize(tree, tab, pick(target), direction(d), amount) {
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

fn assert_invariants(tree: &LayoutNode, tab: Rect, live: &HashSet<PaneId>) {
    let result = solve(tree, tab);
    assert_no_overlap(&result.panes).unwrap();
    assert_no_outside(&result.panes, tab).unwrap();
    assert_min_size_respected(&result.panes, MIN_PANE_SIZE).unwrap();
    if fits(tree, tab, MIN_PANE_SIZE) {
        assert_all_space_occupied(&result.panes, tab).unwrap();
    }
    assert_live_pane_refs(&tree.leaf_panes(), live).unwrap();
    // Same tree, same rect, same placement — no flicker between solves.
    assert_eq!(solve(tree, tab), result);
}
