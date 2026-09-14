//! Property tests: random edit sequences over random tab sizes must never
//! break the layout invariants.
//!
//! Each case starts from a single pane and applies a short random sequence
//! of public edits — directional splits, stacking, removals, resizes,
//! normalization — under a random gap between split children. After every
//! step the solved layout must uphold:
//!
//! - no two panes overlap and nothing leaves the tab,
//! - every visible pane meets the minimum size,
//! - at gap `0`, when the tree is_layout_within_rect at minimum size, the panes tile the tab
//!   exactly (a larger gap leaves blank cells between children, so the tiling
//!   check runs only at gap `0`),
//! - every layout leaf references a live pane id,
//! - solving is deterministic.
//!
//! Failures shrink to a minimal operation sequence and persist a regression seed
//! under `proptest-regressions/` — check those files in when they appear.
//!
//! One fixed operation sequence runs beside the random ones. It uses every operation kind
//! and asserts the exact tree and the exact solved rects it ends on.

use std::collections::HashSet;

use koshi_core::geometry::{Direction, Point, Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use koshi_layout::edit::{add_pane_to_stack, remove_pane, split_leaf};
use koshi_layout::normalize::normalize_layout_tree;
use koshi_layout::resize::resize_layout_with_sizing;
use koshi_layout::size::SizeWeight;
use koshi_layout::solver::{
    is_layout_within_rect, solve_layout, solve_layout_with_sizing, LayoutSolve, PaneSizing,
    StackHeader, MIN_PANE_SIZE,
};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_test_support::layout_assert::{
    check_all_space_occupied, check_live_pane_refs, check_minimum_size_respected, check_no_outside,
    check_no_overlap,
};
use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config, TestRunner};

/// One randomly chosen public edit. A target leaf index selects a leaf in the current
/// leaf list, taken modulo its length.
#[derive(Debug, Clone)]
enum LayoutOperation {
    Split {
        target_leaf_index: usize,
        direction: Direction,
    },
    Stack {
        target_leaf_index: usize,
    },
    Remove {
        target_leaf_index: usize,
    },
    Resize {
        target_leaf_index: usize,
        direction: Direction,
        resize_cell_count: i16,
    },
    Normalize,
}

/// One of the four cardinal directions, each equally likely.
fn build_direction_strategy() -> impl Strategy<Value = Direction> {
    prop_oneof![
        Just(Direction::Left),
        Just(Direction::Right),
        Just(Direction::Up),
        Just(Direction::Down),
    ]
}

/// One random [`LayoutOperation`], each kind equally likely: targets are drawn from
/// `0..16` and resize sizes from `-3..=3`.
fn build_layout_operation_strategy() -> BoxedStrategy<LayoutOperation> {
    Union::new(vec![
        (0..16usize, build_direction_strategy())
            .prop_map(|(target_leaf_index, direction)| LayoutOperation::Split {
                target_leaf_index,
                direction,
            })
            .boxed(),
        (0..16usize)
            .prop_map(|target_leaf_index| LayoutOperation::Stack { target_leaf_index })
            .boxed(),
        (0..16usize)
            .prop_map(|target_leaf_index| LayoutOperation::Remove { target_leaf_index })
            .boxed(),
        (0..16usize, build_direction_strategy(), -3..4i16)
            .prop_map(
                |(target_leaf_index, direction, resize_cell_count)| LayoutOperation::Resize {
                    target_leaf_index,
                    direction,
                    resize_cell_count,
                },
            )
            .boxed(),
        Just(LayoutOperation::Normalize).boxed(),
    ])
    .boxed()
}

/// The sizing a case runs under: the default pane floor and `gap_cell_count` blank
/// cells between split children.
fn build_pane_sizing(gap_cell_count: u16) -> PaneSizing {
    PaneSizing {
        gap_cell_count,
        ..PaneSizing::default()
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
        prop::collection::vec(build_layout_operation_strategy(), 1..12),
        4..=120u16,
        2..=40u16,
        0..=2u16,
    );

    TestRunner::new(config)
        .run(
            &strategy,
            |(operations, column_count, row_count, gap_cell_count)| {
                check_layout_operation_sequence(
                    &operations,
                    column_count,
                    row_count,
                    gap_cell_count,
                );
                Ok(())
            },
        )
        .unwrap();
}

/// Starts from one pane in a `column_count` x `row_count` tab with `gap_cell_count` cells between
/// split children, applies each operation in turn, and checks the invariants before
/// the first operation and after every operation. Returns the final tree and the set of
/// live pane ids.
fn check_layout_operation_sequence(
    operations: &[LayoutOperation],
    column_count: u16,
    row_count: u16,
    gap_cell_count: u16,
) -> (LayoutNode, HashSet<PaneId>) {
    let tab_rect = Rect::from_size_at_origin(Size {
        column_count,
        row_count,
    });
    let first_pane_id = PaneId::new();
    let mut layout_tree = LayoutNode::Pane(first_pane_id);
    let mut live_pane_ids: HashSet<PaneId> = HashSet::from([first_pane_id]);

    assert_layout_invariants(&layout_tree, tab_rect, &live_pane_ids, gap_cell_count);
    for operation in operations {
        apply_layout_operation(
            operation,
            &mut layout_tree,
            tab_rect,
            &mut live_pane_ids,
            gap_cell_count,
        );
        assert_layout_invariants(&layout_tree, tab_rect, &live_pane_ids, gap_cell_count);
    }
    (layout_tree, live_pane_ids)
}

/// Applies one operation through the public edit API under `gap_cell_count`. A split
/// or stack adds its new pane to `live_pane_ids`; a removal drops the removed
/// pane from `live_pane_ids`. An edit the API rejects (no border to resize, a
/// resize past the donor's floor, removing the last pane) leaves `layout_tree`
/// and `live_pane_ids` unchanged.
fn apply_layout_operation(
    operation: &LayoutOperation,
    layout_tree: &mut LayoutNode,
    tab_rect: Rect,
    live_pane_ids: &mut HashSet<PaneId>,
    gap_cell_count: u16,
) {
    let leaf_pane_ids = layout_tree.list_leaf_pane_ids();
    let select_leaf_pane_id =
        |target_leaf_index: usize| leaf_pane_ids[target_leaf_index % leaf_pane_ids.len()];
    match *operation {
        LayoutOperation::Split {
            target_leaf_index,
            direction,
        } => {
            let new_pane_id = PaneId::new();
            if let Ok(next_layout_tree) = split_leaf(
                layout_tree,
                select_leaf_pane_id(target_leaf_index),
                new_pane_id,
                direction,
            ) {
                *layout_tree = next_layout_tree;
                live_pane_ids.insert(new_pane_id);
            }
        }
        LayoutOperation::Stack { target_leaf_index } => {
            let new_pane_id = PaneId::new();
            if let Ok(next_layout_tree) = add_pane_to_stack(
                layout_tree,
                select_leaf_pane_id(target_leaf_index),
                new_pane_id,
            ) {
                *layout_tree = next_layout_tree;
                live_pane_ids.insert(new_pane_id);
            }
        }
        LayoutOperation::Remove { target_leaf_index } => {
            let removed_pane_id = select_leaf_pane_id(target_leaf_index);
            if let Ok((next_layout_tree, _)) = remove_pane(
                layout_tree,
                tab_rect,
                removed_pane_id,
                build_pane_sizing(gap_cell_count),
            ) {
                *layout_tree = next_layout_tree;
                live_pane_ids.remove(&removed_pane_id);
            }
        }
        LayoutOperation::Resize {
            target_leaf_index,
            direction,
            resize_cell_count,
        } => {
            if let Ok(next_layout_tree) = resize_layout_with_sizing(
                layout_tree,
                tab_rect,
                select_leaf_pane_id(target_leaf_index),
                direction,
                resize_cell_count,
                build_pane_sizing(gap_cell_count),
            ) {
                *layout_tree = next_layout_tree;
            }
        }
        LayoutOperation::Normalize => {
            if let Some(next_layout_tree) = normalize_layout_tree(layout_tree, live_pane_ids) {
                *layout_tree = next_layout_tree;
            }
        }
    }
}

/// Checks the layout invariants of `layout_tree` solved over `tab_rect` under `gap_cell_count`: no
/// two panes overlap, no pane leaves the tab, every visible pane meets
/// [`MIN_PANE_SIZE`], every leaf names a pane in `live_pane_ids`, and a second solve
/// equals the first. At gap `0`, the panes also tile the tab exactly whenever
/// the tree is_layout_within_rect at minimum size; a larger gap leaves blank cells between
/// children, so that check does not apply.
fn assert_layout_invariants(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    live_pane_ids: &HashSet<PaneId>,
    gap_cell_count: u16,
) {
    let layout_result =
        solve_layout_with_sizing(layout_tree, tab_rect, build_pane_sizing(gap_cell_count));
    check_no_overlap(&layout_result.pane_rects).unwrap();
    check_no_outside(&layout_result.pane_rects, tab_rect).unwrap();
    check_minimum_size_respected(&layout_result.pane_rects, MIN_PANE_SIZE).unwrap();
    if gap_cell_count == 0
        && is_layout_within_rect(layout_tree, tab_rect, build_pane_sizing(gap_cell_count))
    {
        check_all_space_occupied(&layout_result.pane_rects, tab_rect).unwrap();
    }
    check_live_pane_refs(&layout_tree.list_leaf_pane_ids(), live_pane_ids).unwrap();
    assert_eq!(
        solve_layout_with_sizing(layout_tree, tab_rect, build_pane_sizing(gap_cell_count)),
        layout_result
    );
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
        prop::collection::vec(build_layout_operation_strategy(), 1..12),
        4..=120u16,
        2..=40u16,
        0..=2u16,
    );

    TestRunner::new(config)
        .run(
            &strategy,
            |(operations, column_count, row_count, gap_cell_count)| {
                let (layout_tree, live_pane_ids) = check_layout_operation_sequence(
                    &operations,
                    column_count,
                    row_count,
                    gap_cell_count,
                );
                let normalized_layout_tree = normalize_layout_tree(&layout_tree, &live_pane_ids)
                    .expect("at least one live pane always survives");
                prop_assert_eq!(
                    normalize_layout_tree(&normalized_layout_tree, &live_pane_ids),
                    Some(normalized_layout_tree)
                );
                Ok(())
            },
        )
        .unwrap();
}

/// The fixed sequence replayed by [`a_fixed_layout_operation_sequence_lands_on_its_exact_layout`],
/// holding one of every operation kind [`build_layout_operation_strategy`] can generate.
///
/// Each target leaf index selects a leaf at that step: split pane 0 to
/// the right, split the new pane downward, stack a pane onto the last leaf,
/// widen pane 0 by five columns, remove leaf 1, then normalize_layout_tree.
const FIXED_LAYOUT_OPERATIONS: [LayoutOperation; 6] = [
    LayoutOperation::Split {
        target_leaf_index: 0,
        direction: Direction::Right,
    },
    LayoutOperation::Split {
        target_leaf_index: 1,
        direction: Direction::Down,
    },
    LayoutOperation::Stack {
        target_leaf_index: 2,
    },
    LayoutOperation::Resize {
        target_leaf_index: 0,
        direction: Direction::Right,
        resize_cell_count: 5,
    },
    LayoutOperation::Remove {
        target_leaf_index: 1,
    },
    LayoutOperation::Normalize,
];

/// Replay [`FIXED_LAYOUT_OPERATIONS`] over a fixed 80x24 tab: the invariants hold after
/// every step, and the run ends on one exact tree and one exact placement.
#[test]
fn a_fixed_layout_operation_sequence_lands_on_its_exact_layout() {
    let (layout_tree, live_pane_ids) =
        check_layout_operation_sequence(&FIXED_LAYOUT_OPERATIONS, 80, 24, 0);
    let tab_rect = Rect::from_size_at_origin(Size {
        column_count: 80,
        row_count: 24,
    });

    // Three panes survive, and the live set is exactly the tree's leaves.
    let leaf_pane_ids = layout_tree.list_leaf_pane_ids();
    assert_eq!(leaf_pane_ids.len(), 3);
    assert_eq!(
        live_pane_ids,
        leaf_pane_ids.iter().copied().collect::<HashSet<_>>()
    );

    // The removal leaves a column holding only the stack, normalization
    // collapses that column, and the stack hangs straight off the root. The
    // resize moved five columns from the second root child to the first.
    assert_eq!(
        layout_tree,
        LayoutNode::Split(SplitNode {
            direction: SplitDirection::Horizontal,
            children: vec![
                LayoutNode::Pane(leaf_pane_ids[0]),
                LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                    vec![leaf_pane_ids[1], leaf_pane_ids[2]],
                    1
                )),
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
            active_child_index: 0,
        })
    );

    // The resized pane holds 45 of the 80 columns, and the stack's 35 split
    // into one header row plus the active member's 23.
    let stack_header_rect = Rect::from_origin_and_size(
        Point { column: 45, row: 0 },
        Size {
            column_count: 35,
            row_count: 1,
        },
    );
    assert_eq!(
        solve_layout(&layout_tree, tab_rect),
        LayoutSolve {
            pane_rects: vec![
                (
                    leaf_pane_ids[0],
                    Rect::from_size_at_origin(Size {
                        column_count: 45,
                        row_count: 24
                    })
                ),
                (leaf_pane_ids[1], stack_header_rect),
                (
                    leaf_pane_ids[2],
                    Rect::from_origin_and_size(
                        Point { column: 45, row: 1 },
                        Size {
                            column_count: 35,
                            row_count: 23
                        }
                    )
                ),
            ],
            suppressed_pane_ids: Vec::new(),
            is_all_panes_suppressed: false,
            stack_headers: vec![StackHeader {
                pane_id: leaf_pane_ids[1],
                header_rect: stack_header_rect,
                member_index: 0,
                member_count: 2,
            }],
        }
    );
}
