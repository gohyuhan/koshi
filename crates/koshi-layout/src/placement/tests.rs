//! Tests for pane placement: swaps, insertions, anchor validation and
//! destination listing.
//!
//! The main fixture is the four-pane screen `A | B B` above `A | C D` over
//! 120 by 40 cells, encoded as `H[A, V[B, H[C, D]]]` with the outer weights
//! 1 and 2. Solved: A `(0, 0, 40, 40)`, B `(40, 0, 80, 20)`,
//! C `(40, 20, 40, 20)`, D `(80, 20, 40, 20)`.

use koshi_core::geometry::Point;
use koshi_test_support::layout_assert::check_exact_tiling;

use super::*;
use crate::size::{SizeConstraint, SizeWeight};
use crate::solver::{solve_layout, StackHeader};

fn build_cell_rect(column: u16, row: u16, column_count: u16, row_count: u16) -> Rect {
    Rect::from_origin_and_size(
        Point { column, row },
        Size {
            column_count,
            row_count,
        },
    )
}

fn build_flex_weight(share: u32) -> SizeWeight {
    SizeWeight::from_primary_constraint(SizeConstraint::Flex(share))
}

fn build_pane_leaf(pane_id: PaneId) -> LayoutNode {
    LayoutNode::Pane(pane_id)
}

fn build_split(direction: SplitDirection, children: Vec<LayoutNode>, shares: &[u32]) -> LayoutNode {
    LayoutNode::Split(SplitNode {
        direction,
        children,
        weights: shares.iter().copied().map(build_flex_weight).collect(),
        active_child_index: 0,
    })
}

fn build_horizontal_split(children: Vec<LayoutNode>, shares: &[u32]) -> LayoutNode {
    build_split(SplitDirection::Horizontal, children, shares)
}

fn build_vertical_split(children: Vec<LayoutNode>, shares: &[u32]) -> LayoutNode {
    build_split(SplitDirection::Vertical, children, shares)
}

fn build_stack(pane_ids: Vec<PaneId>, active_child_index: usize) -> LayoutNode {
    LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        pane_ids,
        active_child_index,
    ))
}

/// Four pane ids in ascending order.
fn build_sorted_pane_ids() -> [PaneId; 4] {
    let mut pane_ids = [PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new()];
    pane_ids.sort_unstable();
    pane_ids
}

const FIXTURE_TAB_RECT: Rect = Rect {
    origin: Point { column: 0, row: 0 },
    cell_size: Size {
        column_count: 120,
        row_count: 40,
    },
};

/// `H[A, V[B, H[C, D]]]` with outer weights 1 and 2.
fn build_fixture_tree([pane_a, pane_b, pane_c, pane_d]: [PaneId; 4]) -> LayoutNode {
    build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_vertical_split(
                vec![
                    build_pane_leaf(pane_b),
                    build_horizontal_split(
                        vec![build_pane_leaf(pane_c), build_pane_leaf(pane_d)],
                        &[1, 1],
                    ),
                ],
                &[1, 1],
            ),
        ],
        &[1, 2],
    )
}

fn build_swap_target(target_pane_id: PaneId) -> PlacementTarget {
    PlacementTarget::Swap { target_pane_id }
}

fn build_insert_target(anchor: PanePlacementAnchor, direction: Direction) -> PlacementTarget {
    PlacementTarget::Insert { anchor, direction }
}

fn list_solved_rects(layout_tree: &LayoutNode, tab_rect: Rect) -> Vec<(PaneId, Rect)> {
    let mut pane_rects = solve_layout(layout_tree, tab_rect).pane_rects;
    pane_rects.sort_by_key(|(pane_id, _)| *pane_id);
    pane_rects
}

fn check_tiles_exactly(layout_tree: &LayoutNode, tab_rect: Rect) {
    let layout_solve = solve_layout(layout_tree, tab_rect);
    check_exact_tiling(&layout_solve.pane_rects, tab_rect).expect("the placed tree tiles the tab");
}

// ── swap ────────────────────────────────────────────────────────────────

#[test]
fn fixture_solves_to_the_documented_rects() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    assert_eq!(
        list_solved_rects(&build_fixture_tree(pane_ids), FIXTURE_TAB_RECT),
        vec![
            (pane_a, build_cell_rect(0, 0, 40, 40)),
            (pane_b, build_cell_rect(40, 0, 80, 20)),
            (pane_c, build_cell_rect(40, 20, 40, 20)),
            (pane_d, build_cell_rect(80, 20, 40, 20)),
        ]
    );
}

#[test]
fn swapping_d_with_b_exchanges_the_two_leaves_and_keeps_every_weight() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    let swapped_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_swap_target(pane_b),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("swap succeeds");

    assert_eq!(
        swapped_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(pane_a),
                build_vertical_split(
                    vec![
                        build_pane_leaf(pane_d),
                        build_horizontal_split(
                            vec![build_pane_leaf(pane_c), build_pane_leaf(pane_b)],
                            &[1, 1]
                        )
                    ],
                    &[1, 1],
                ),
            ],
            &[1, 2],
        )
    );
    assert_eq!(
        list_solved_rects(&swapped_tree, FIXTURE_TAB_RECT),
        vec![
            (pane_a, build_cell_rect(0, 0, 40, 40)),
            (pane_b, build_cell_rect(80, 20, 40, 20)),
            (pane_c, build_cell_rect(40, 20, 40, 20)),
            (pane_d, build_cell_rect(40, 0, 80, 20)),
        ]
    );
    assert_eq!(fixture_tree, build_fixture_tree(pane_ids));
}

#[test]
fn swapping_a_pane_with_itself_returns_the_tree_unchanged() {
    let pane_ids = build_sorted_pane_ids();
    let fixture_tree = build_fixture_tree(pane_ids);

    let swapped_tree = place_pane_within_tab(
        &fixture_tree,
        pane_ids[3],
        &build_swap_target(pane_ids[3]),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("self-swap succeeds");

    assert_eq!(swapped_tree, fixture_tree);
}

#[test]
fn swapping_inside_an_unnormalized_tree_keeps_its_nesting() {
    let [pane_a, pane_b, pane_c, _] = build_sorted_pane_ids();
    let nested_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_horizontal_split(
                vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
                &[1, 1],
            ),
        ],
        &[1, 1],
    );

    let swapped_tree = place_pane_within_tab(
        &nested_tree,
        pane_a,
        &build_swap_target(pane_c),
        build_cell_rect(0, 0, 80, 20),
        PaneSizing::default(),
    )
    .expect("swap succeeds");

    assert_eq!(
        swapped_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(pane_c),
                build_horizontal_split(
                    vec![build_pane_leaf(pane_b), build_pane_leaf(pane_a)],
                    &[1, 1]
                )
            ],
            &[1, 1]
        )
    );
}

#[test]
fn swapping_with_an_unknown_target_is_target_not_found() {
    let pane_ids = build_sorted_pane_ids();
    let fixture_tree = build_fixture_tree(pane_ids);
    let unknown_pane_id = PaneId::new();

    assert_eq!(
        place_pane_within_tab(
            &fixture_tree,
            pane_ids[3],
            &build_swap_target(unknown_pane_id),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::TargetPaneNotFound {
            pane_id: unknown_pane_id
        })
    );
    assert_eq!(fixture_tree, build_fixture_tree(pane_ids));
}

#[test]
fn a_source_outside_the_tree_is_source_not_found() {
    let pane_ids = build_sorted_pane_ids();
    let fixture_tree = build_fixture_tree(pane_ids);
    let unknown_pane_id = PaneId::new();

    assert_eq!(
        place_pane_within_tab(
            &fixture_tree,
            unknown_pane_id,
            &build_swap_target(pane_ids[0]),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::SourcePaneNotFound {
            pane_id: unknown_pane_id
        })
    );
    assert_eq!(
        place_pane_within_tab(
            &fixture_tree,
            unknown_pane_id,
            &build_insert_target(PanePlacementAnchor::Tab, Direction::Up),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::SourcePaneNotFound {
            pane_id: unknown_pane_id
        })
    );
}

// ── insertion ───────────────────────────────────────────────────────────

#[test]
fn inserting_d_above_the_right_hand_group_puts_d_over_b_and_c() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    let placed_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_insert_target(
            PanePlacementAnchor::Group(vec![pane_b, pane_c, pane_d]),
            Direction::Up,
        ),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(
        placed_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(pane_a),
                build_vertical_split(
                    vec![
                        build_pane_leaf(pane_d),
                        build_pane_leaf(pane_b),
                        build_pane_leaf(pane_c)
                    ],
                    &[2, 1, 1]
                )
            ],
            &[1, 2],
        )
    );
    assert_eq!(
        list_solved_rects(&placed_tree, FIXTURE_TAB_RECT),
        vec![
            (pane_a, build_cell_rect(0, 0, 40, 40)),
            (pane_b, build_cell_rect(40, 20, 80, 10)),
            (pane_c, build_cell_rect(40, 30, 80, 10)),
            (pane_d, build_cell_rect(40, 0, 80, 20)),
        ]
    );
    assert_eq!(fixture_tree, build_fixture_tree(pane_ids));
}

#[test]
fn inserting_d_above_the_whole_tab_gives_d_the_full_width() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    let placed_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_insert_target(PanePlacementAnchor::Tab, Direction::Up),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(
        placed_tree,
        build_vertical_split(
            vec![
                build_pane_leaf(pane_d),
                build_horizontal_split(
                    vec![
                        build_pane_leaf(pane_a),
                        build_vertical_split(
                            vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
                            &[1, 1]
                        )
                    ],
                    &[1, 2]
                ),
            ],
            &[1, 1],
        )
    );
    check_tiles_exactly(&placed_tree, FIXTURE_TAB_RECT);
}

#[test]
fn inserting_d_left_of_a_merges_into_the_root_split_with_exact_shares() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    let placed_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_insert_target(PanePlacementAnchor::Pane(pane_a), Direction::Left),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(
        placed_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(pane_d),
                build_pane_leaf(pane_a),
                build_vertical_split(
                    vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
                    &[1, 1]
                )
            ],
            &[1, 1, 4],
        )
    );
    assert_eq!(
        list_solved_rects(&placed_tree, FIXTURE_TAB_RECT),
        vec![
            (pane_a, build_cell_rect(20, 0, 20, 40)),
            (pane_b, build_cell_rect(40, 0, 80, 20)),
            (pane_c, build_cell_rect(40, 20, 80, 20)),
            (pane_d, build_cell_rect(0, 0, 20, 40)),
        ]
    );
}

#[test]
fn inserting_d_above_c_collapses_the_emptied_split_and_merges_the_column() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    let placed_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_insert_target(PanePlacementAnchor::Pane(pane_c), Direction::Up),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(
        placed_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(pane_a),
                build_vertical_split(
                    vec![
                        build_pane_leaf(pane_b),
                        build_pane_leaf(pane_d),
                        build_pane_leaf(pane_c)
                    ],
                    &[2, 1, 1]
                )
            ],
            &[1, 2],
        )
    );
}

#[test]
fn inserting_beside_a_group_that_collapses_to_the_source_neighbour_reproduces_the_layout() {
    let pane_ids = build_sorted_pane_ids();
    let [_, _, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    // Removing D leaves the group {C, D} as C alone; D left of it is where D
    // already was.
    let placed_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_insert_target(
            PanePlacementAnchor::Group(vec![pane_c, pane_d]),
            Direction::Right,
        ),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(placed_tree, fixture_tree);
}

#[test]
fn inserting_the_only_pane_beside_the_tab_returns_the_bare_leaf() {
    let [pane_a, _, _, _] = build_sorted_pane_ids();
    let tab_rect = build_cell_rect(0, 0, 40, 20);

    for lone_tree in [
        build_pane_leaf(pane_a),
        build_horizontal_split(vec![build_pane_leaf(pane_a)], &[1]),
    ] {
        assert_eq!(
            place_pane_within_tab(
                &lone_tree,
                pane_a,
                &build_insert_target(PanePlacementAnchor::Tab, Direction::Down),
                tab_rect,
                PaneSizing::default(),
            ),
            Ok(build_pane_leaf(pane_a)),
            "{lone_tree:?}"
        );
    }
}

#[test]
fn an_insertion_result_tiles_the_tab_for_every_pane_anchor_and_direction() {
    let pane_ids = build_sorted_pane_ids();
    let fixture_tree = build_fixture_tree(pane_ids);
    let directions = [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ];

    for anchor_pane_id in pane_ids {
        for direction in directions {
            for source_pane_id in pane_ids {
                if source_pane_id == anchor_pane_id {
                    continue;
                }
                let placed_tree = place_pane_within_tab(
                    &fixture_tree,
                    source_pane_id,
                    &build_insert_target(PanePlacementAnchor::Pane(anchor_pane_id), direction),
                    FIXTURE_TAB_RECT,
                    PaneSizing::default(),
                )
                .expect("insertion succeeds");
                let mut placed_pane_ids = placed_tree.list_leaf_pane_ids();
                placed_pane_ids.sort_unstable();
                assert_eq!(placed_pane_ids, pane_ids.to_vec());
                check_tiles_exactly(&placed_tree, FIXTURE_TAB_RECT);
            }
        }
    }
}

// ── anchor validation ───────────────────────────────────────────────────

#[test]
fn an_anchor_naming_the_source_is_refused() {
    let pane_ids = build_sorted_pane_ids();
    let pane_d = pane_ids[3];
    let fixture_tree = build_fixture_tree(pane_ids);

    for anchor in [
        PanePlacementAnchor::Pane(pane_d),
        PanePlacementAnchor::Group(vec![pane_d]),
    ] {
        assert_eq!(
            place_pane_within_tab(
                &fixture_tree,
                pane_d,
                &build_insert_target(anchor, Direction::Up),
                FIXTURE_TAB_RECT,
                PaneSizing::default(),
            ),
            Err(PlacementError::AnchorIsSource { pane_id: pane_d })
        );
    }
    assert_eq!(fixture_tree, build_fixture_tree(pane_ids));
}

#[test]
fn a_group_repeating_a_pane_is_refused() {
    let pane_ids = build_sorted_pane_ids();
    let [_, pane_b, _, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    assert_eq!(
        place_pane_within_tab(
            &fixture_tree,
            pane_d,
            &build_insert_target(
                PanePlacementAnchor::Group(vec![pane_b, pane_b]),
                Direction::Up
            ),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::GroupPaneDuplicated { pane_id: pane_b })
    );
}

#[test]
fn a_group_naming_an_absent_pane_is_target_not_found() {
    let pane_ids = build_sorted_pane_ids();
    let [_, pane_b, _, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);
    let unknown_pane_id = PaneId::new();

    assert_eq!(
        place_pane_within_tab(
            &fixture_tree,
            pane_d,
            &build_insert_target(
                PanePlacementAnchor::Group(vec![pane_b, unknown_pane_id]),
                Direction::Up
            ),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::TargetPaneNotFound {
            pane_id: unknown_pane_id
        })
    );
    assert_eq!(
        place_pane_within_tab(
            &fixture_tree,
            pane_d,
            &build_insert_target(PanePlacementAnchor::Pane(unknown_pane_id), Direction::Up),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::TargetPaneNotFound {
            pane_id: unknown_pane_id
        })
    );
}

#[test]
fn an_empty_group_is_not_one_subtree() {
    let pane_ids = build_sorted_pane_ids();
    let fixture_tree = build_fixture_tree(pane_ids);

    assert_eq!(
        place_pane_within_tab(
            &fixture_tree,
            pane_ids[3],
            &build_insert_target(PanePlacementAnchor::Group(Vec::new()), Direction::Up),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::GroupIsNotOneSubtree {
            pane_ids: Vec::new()
        })
    );
}

#[test]
fn a_group_that_is_not_one_subtree_is_refused() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    for group_pane_ids in [
        vec![pane_a, pane_c],
        vec![pane_b, pane_c],
        vec![pane_a, pane_d],
    ] {
        assert_eq!(
            place_pane_within_tab(
                &fixture_tree,
                pane_d,
                &build_insert_target(
                    PanePlacementAnchor::Group(group_pane_ids.clone()),
                    Direction::Up
                ),
                FIXTURE_TAB_RECT,
                PaneSizing::default(),
            ),
            Err(PlacementError::GroupIsNotOneSubtree {
                pane_ids: group_pane_ids.clone()
            }),
            "{group_pane_ids:?}"
        );
    }
}

#[test]
fn the_whole_tree_as_a_group_is_a_valid_anchor() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    // Removing D leaves {A, B, C} at the root; inserting D above it equals
    // inserting above the tab.
    let placed_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_insert_target(
            PanePlacementAnchor::Group(vec![pane_a, pane_b, pane_c, pane_d]),
            Direction::Up,
        ),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("insertion succeeds");
    let tab_placed_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_insert_target(PanePlacementAnchor::Tab, Direction::Up),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(placed_tree, tab_placed_tree);
}

#[test]
fn an_anchor_inside_a_stack_is_refused_with_the_stack_members() {
    let [pane_a, pane_b, pane_c, pane_d] = build_sorted_pane_ids();
    let stack_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_stack(vec![pane_b, pane_c], 0),
        ],
        &[1, 1],
    );
    let tab_rect = build_cell_rect(0, 0, 80, 20);
    let source_tree = build_pane_leaf(pane_d);

    assert_eq!(
        place_pane_within_tab(
            &stack_tree,
            pane_a,
            &build_insert_target(PanePlacementAnchor::Pane(pane_b), Direction::Up),
            tab_rect,
            PaneSizing::default(),
        ),
        Err(PlacementError::AnchorInsideStack {
            stack_pane_ids: vec![pane_b, pane_c]
        })
    );
    assert_eq!(
        place_pane_across_tabs(
            &source_tree,
            pane_d,
            &stack_tree,
            &build_insert_target(PanePlacementAnchor::Pane(pane_c), Direction::Down),
            tab_rect,
            PaneSizing::default(),
        ),
        Err(PlacementError::AnchorInsideStack {
            stack_pane_ids: vec![pane_b, pane_c]
        })
    );
}

#[test]
fn a_group_inside_a_stack_is_refused_with_the_outermost_stack_members() {
    let [pane_a, pane_b, pane_c, pane_d] = build_sorted_pane_ids();
    let inner_group = build_horizontal_split(
        vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
        &[1, 1],
    );
    let stack_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![build_pane_leaf(pane_a), inner_group],
        weights: vec![build_flex_weight(1), build_flex_weight(1)],
        active_child_index: 1,
    });
    let source_tree = build_pane_leaf(pane_d);

    assert_eq!(
        place_pane_across_tabs(
            &source_tree,
            pane_d,
            &stack_tree,
            &build_insert_target(
                PanePlacementAnchor::Group(vec![pane_b, pane_c]),
                Direction::Up
            ),
            build_cell_rect(0, 0, 80, 20),
            PaneSizing::default(),
        ),
        Err(PlacementError::AnchorInsideStack {
            stack_pane_ids: vec![pane_a, pane_b, pane_c]
        })
    );
}

#[test]
fn inserting_beside_a_whole_stack_keeps_the_stack_intact() {
    let [pane_a, pane_b, pane_c, _] = build_sorted_pane_ids();
    let stack_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_stack(vec![pane_b, pane_c], 0),
        ],
        &[1, 1],
    );

    let placed_tree = place_pane_within_tab(
        &stack_tree,
        pane_a,
        &build_insert_target(
            PanePlacementAnchor::Group(vec![pane_b, pane_c]),
            Direction::Up,
        ),
        build_cell_rect(0, 0, 80, 20),
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(
        placed_tree,
        build_vertical_split(
            vec![
                build_pane_leaf(pane_a),
                build_stack(vec![pane_b, pane_c], 0)
            ],
            &[1, 1]
        )
    );
}

// ── stack expansion ─────────────────────────────────────────────────────

#[test]
fn a_pane_swapped_onto_a_collapsed_member_becomes_the_active_member() {
    let [pane_a, pane_b, pane_c, pane_d] = build_sorted_pane_ids();
    let stack_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_stack(vec![pane_b, pane_c], 0),
        ],
        &[1, 1],
    );
    let source_tree = build_pane_leaf(pane_d);

    let placement = place_pane_across_tabs(
        &source_tree,
        pane_d,
        &stack_tree,
        &build_swap_target(pane_c),
        build_cell_rect(0, 0, 80, 20),
        PaneSizing::default(),
    )
    .expect("swap succeeds");

    assert_eq!(
        placement,
        CrossTabPlacement {
            source_tree: Some(build_pane_leaf(pane_c)),
            destination_tree: build_horizontal_split(
                vec![
                    build_pane_leaf(pane_a),
                    build_stack(vec![pane_b, pane_d], 1)
                ],
                &[1, 1]
            ),
        }
    );
}

#[test]
fn a_pane_swapped_into_a_nested_stack_expands_every_stack_above_it() {
    let [pane_a, pane_b, pane_c, pane_d] = build_sorted_pane_ids();
    let inner_stack = build_stack(vec![pane_b, pane_c], 0);
    let outer_stack = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![build_pane_leaf(pane_a), inner_stack],
        weights: vec![build_flex_weight(1), build_flex_weight(1)],
        active_child_index: 0,
    });

    let placed_tree = place_pane_within_tab(
        &outer_stack,
        pane_a,
        &build_swap_target(pane_c),
        build_cell_rect(0, 0, 40, 20),
        PaneSizing::default(),
    )
    .expect("swap succeeds");

    assert_eq!(
        placed_tree,
        LayoutNode::Split(SplitNode {
            direction: SplitDirection::Stacked,
            children: vec![
                build_pane_leaf(pane_c),
                build_stack(vec![pane_b, pane_a], 1)
            ],
            weights: vec![build_flex_weight(1), build_flex_weight(1)],
            active_child_index: 1,
        })
    );
    let _ = pane_d;
}

// ── cross-tab ───────────────────────────────────────────────────────────

#[test]
fn moving_the_only_pane_of_a_tab_into_another_tab_empties_the_source() {
    let [pane_a, pane_b, pane_c, pane_d] = build_sorted_pane_ids();
    let source_tree = build_pane_leaf(pane_d);
    let destination_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_vertical_split(
                vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
                &[1, 1],
            ),
        ],
        &[1, 2],
    );

    let placement = place_pane_across_tabs(
        &source_tree,
        pane_d,
        &destination_tree,
        &build_insert_target(PanePlacementAnchor::Pane(pane_c), Direction::Right),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(
        placement,
        CrossTabPlacement {
            source_tree: None,
            destination_tree: build_fixture_tree([pane_a, pane_b, pane_c, pane_d]),
        }
    );
}

#[test]
fn a_cross_tab_swap_exchanges_the_leaves_and_keeps_both_trees_weights() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let (pane_x, pane_y) = (PaneId::new(), PaneId::new());
    let other_tree = build_horizontal_split(
        vec![build_pane_leaf(pane_x), build_pane_leaf(pane_y)],
        &[3, 1],
    );
    let fixture_tree = build_fixture_tree(pane_ids);

    let placement = place_pane_across_tabs(
        &other_tree,
        pane_x,
        &fixture_tree,
        &build_swap_target(pane_a),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("swap succeeds");

    assert_eq!(
        placement,
        CrossTabPlacement {
            source_tree: Some(build_horizontal_split(
                vec![build_pane_leaf(pane_a), build_pane_leaf(pane_y)],
                &[3, 1]
            )),
            destination_tree: build_horizontal_split(
                vec![
                    build_pane_leaf(pane_x),
                    build_vertical_split(
                        vec![
                            build_pane_leaf(pane_b),
                            build_horizontal_split(
                                vec![build_pane_leaf(pane_c), build_pane_leaf(pane_d)],
                                &[1, 1]
                            )
                        ],
                        &[1, 1],
                    ),
                ],
                &[1, 2],
            ),
        }
    );
    assert_eq!(
        other_tree,
        build_horizontal_split(
            vec![build_pane_leaf(pane_x), build_pane_leaf(pane_y)],
            &[3, 1]
        )
    );
    assert_eq!(fixture_tree, build_fixture_tree(pane_ids));
}

#[test]
fn a_cross_tab_insertion_removes_the_leaf_and_normalizes_the_source() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let (pane_x, pane_y) = (PaneId::new(), PaneId::new());
    let fixture_tree = build_fixture_tree(pane_ids);
    let other_tree = build_horizontal_split(
        vec![build_pane_leaf(pane_x), build_pane_leaf(pane_y)],
        &[1, 1],
    );

    let placement = place_pane_across_tabs(
        &fixture_tree,
        pane_d,
        &other_tree,
        &build_insert_target(PanePlacementAnchor::Tab, Direction::Down),
        build_cell_rect(0, 0, 80, 24),
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(
        placement,
        CrossTabPlacement {
            source_tree: Some(build_horizontal_split(
                vec![
                    build_pane_leaf(pane_a),
                    build_vertical_split(
                        vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
                        &[1, 1]
                    )
                ],
                &[1, 2],
            )),
            destination_tree: build_vertical_split(
                vec![
                    build_horizontal_split(
                        vec![build_pane_leaf(pane_x), build_pane_leaf(pane_y)],
                        &[1, 1]
                    ),
                    build_pane_leaf(pane_d)
                ],
                &[1, 1],
            ),
        }
    );
}

#[test]
fn a_pane_present_in_both_trees_is_refused() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, _, _, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);
    let (pane_x, pane_y) = (PaneId::new(), PaneId::new());
    let other_tree = build_horizontal_split(
        vec![build_pane_leaf(pane_x), build_pane_leaf(pane_y)],
        &[1, 1],
    );

    assert_eq!(
        place_pane_across_tabs(
            &fixture_tree,
            pane_d,
            &fixture_tree,
            &build_swap_target(pane_a),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::PaneInBothTrees { pane_id: pane_d })
    );
    assert_eq!(
        place_pane_across_tabs(
            &other_tree,
            pane_x,
            &fixture_tree,
            &build_swap_target(pane_y),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::PaneInBothTrees { pane_id: pane_y })
    );
}

#[test]
fn a_cross_tab_source_outside_its_tree_is_source_not_found() {
    let pane_ids = build_sorted_pane_ids();
    let fixture_tree = build_fixture_tree(pane_ids);
    let (pane_x, pane_y) = (PaneId::new(), PaneId::new());
    let other_tree = build_horizontal_split(
        vec![build_pane_leaf(pane_x), build_pane_leaf(pane_y)],
        &[1, 1],
    );
    let unknown_pane_id = PaneId::new();

    assert_eq!(
        place_pane_across_tabs(
            &other_tree,
            unknown_pane_id,
            &fixture_tree,
            &build_swap_target(pane_ids[0]),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::SourcePaneNotFound {
            pane_id: unknown_pane_id
        })
    );
}

// ── minimum size ────────────────────────────────────────────────────────

#[test]
fn a_destination_too_small_for_the_result_is_refused_with_both_sizes() {
    let [pane_a, pane_b, pane_c, _] = build_sorted_pane_ids();
    let destination_tree = build_horizontal_split(
        vec![build_pane_leaf(pane_a), build_pane_leaf(pane_b)],
        &[1, 1],
    );
    let source_tree = build_pane_leaf(pane_c);
    let destination_tab_rect = build_cell_rect(0, 0, 7, 3);

    let expected_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_pane_leaf(pane_b),
            build_pane_leaf(pane_c),
        ],
        &[1, 1, 1],
    );
    assert_eq!(
        place_pane_across_tabs(
            &source_tree,
            pane_c,
            &destination_tree,
            &build_insert_target(PanePlacementAnchor::Pane(pane_b), Direction::Right),
            destination_tab_rect,
            PaneSizing::default(),
        ),
        Err(PlacementError::DestinationTooSmall {
            required_size: compute_minimum_size(&expected_tree, PaneSizing::default()),
            available_size: Size {
                column_count: 7,
                row_count: 3
            },
        })
    );
    assert_eq!(
        compute_minimum_size(&expected_tree, PaneSizing::default()),
        Size {
            column_count: 12,
            row_count: 3
        }
    );
}

#[test]
fn a_same_tab_insertion_that_needs_more_rows_than_the_tab_has_is_refused() {
    let [pane_a, pane_b, pane_c, _] = build_sorted_pane_ids();
    let row_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_pane_leaf(pane_b),
            build_pane_leaf(pane_c),
        ],
        &[1, 1, 1],
    );
    let tab_rect = build_cell_rect(0, 0, 12, 3);

    assert_eq!(
        place_pane_within_tab(
            &row_tree,
            pane_c,
            &build_insert_target(PanePlacementAnchor::Tab, Direction::Up),
            tab_rect,
            PaneSizing::default(),
        ),
        Err(PlacementError::DestinationTooSmall {
            required_size: Size {
                column_count: 8,
                row_count: 6
            },
            available_size: Size {
                column_count: 12,
                row_count: 3
            },
        })
    );
    assert_eq!(
        row_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(pane_a),
                build_pane_leaf(pane_b),
                build_pane_leaf(pane_c)
            ],
            &[1, 1, 1]
        )
    );
}

// ── destinations ────────────────────────────────────────────────────────

#[test]
fn destinations_of_d_list_every_visible_slot_group_and_the_tab() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);
    let layout_solve = solve_layout(&fixture_tree, FIXTURE_TAB_RECT);

    let destinations =
        list_placement_destinations(&fixture_tree, &layout_solve, FIXTURE_TAB_RECT, pane_d);

    assert_eq!(
        destinations,
        PlacementDestinations {
            swap_slots: vec![
                SwapSlot {
                    pane_id: pane_a,
                    slot_rect: build_cell_rect(0, 0, 40, 40)
                },
                SwapSlot {
                    pane_id: pane_b,
                    slot_rect: build_cell_rect(40, 0, 80, 20)
                },
                SwapSlot {
                    pane_id: pane_c,
                    slot_rect: build_cell_rect(40, 20, 40, 20)
                },
            ],
            insertion_spans: vec![
                InsertionSpan {
                    anchor: PanePlacementAnchor::Pane(pane_a),
                    span_rect: build_cell_rect(0, 0, 40, 40)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Pane(pane_b),
                    span_rect: build_cell_rect(40, 0, 80, 20)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Pane(pane_c),
                    span_rect: build_cell_rect(40, 20, 40, 20)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Group(vec![pane_b, pane_c, pane_d]),
                    span_rect: build_cell_rect(40, 0, 80, 40)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Group(vec![pane_c, pane_d]),
                    span_rect: build_cell_rect(40, 20, 80, 20)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Tab,
                    span_rect: FIXTURE_TAB_RECT
                },
            ],
        }
    );
}

#[test]
fn every_listed_insertion_span_places_without_error() {
    let pane_ids = build_sorted_pane_ids();
    let pane_d = pane_ids[3];
    let fixture_tree = build_fixture_tree(pane_ids);
    let layout_solve = solve_layout(&fixture_tree, FIXTURE_TAB_RECT);
    let destinations =
        list_placement_destinations(&fixture_tree, &layout_solve, FIXTURE_TAB_RECT, pane_d);

    for insertion_span in destinations.insertion_spans {
        for direction in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            let placed_tree = place_pane_within_tab(
                &fixture_tree,
                pane_d,
                &build_insert_target(insertion_span.anchor.clone(), direction),
                FIXTURE_TAB_RECT,
                PaneSizing::default(),
            )
            .unwrap_or_else(|placement_error| {
                panic!(
                    "{:?} {direction:?}: {placement_error}",
                    insertion_span.anchor
                )
            });
            check_tiles_exactly(&placed_tree, FIXTURE_TAB_RECT);
        }
    }
    for swap_slot in destinations.swap_slots {
        place_pane_within_tab(
            &fixture_tree,
            pane_d,
            &build_swap_target(swap_slot.pane_id),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        )
        .expect("every listed swap slot places");
    }
}

#[test]
fn a_stack_is_one_group_and_its_members_are_swap_slots_but_not_spans() {
    let [pane_a, pane_b, pane_c, _] = build_sorted_pane_ids();
    let stack_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_stack(vec![pane_b, pane_c], 0),
        ],
        &[1, 1],
    );
    let tab_rect = build_cell_rect(0, 0, 80, 20);
    let layout_solve = solve_layout(&stack_tree, tab_rect);
    assert_eq!(
        layout_solve.stack_headers,
        vec![StackHeader {
            pane_id: pane_c,
            header_rect: build_cell_rect(40, 19, 40, 1),
            member_index: 1,
            member_count: 2,
        }]
    );

    let destinations = list_placement_destinations(&stack_tree, &layout_solve, tab_rect, pane_a);

    assert_eq!(
        destinations,
        PlacementDestinations {
            swap_slots: vec![
                SwapSlot {
                    pane_id: pane_b,
                    slot_rect: build_cell_rect(40, 0, 40, 19)
                },
                SwapSlot {
                    pane_id: pane_c,
                    slot_rect: build_cell_rect(40, 19, 40, 1)
                },
            ],
            insertion_spans: vec![
                InsertionSpan {
                    anchor: PanePlacementAnchor::Group(vec![pane_b, pane_c]),
                    span_rect: build_cell_rect(40, 0, 40, 20)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Tab,
                    span_rect: tab_rect
                },
            ],
        }
    );
}

#[test]
fn a_suppressed_pane_is_neither_a_slot_nor_part_of_a_group() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);
    let layout_solve = LayoutSolve {
        pane_rects: vec![
            (pane_a, build_cell_rect(0, 0, 40, 40)),
            (pane_b, build_cell_rect(40, 0, 80, 20)),
            (pane_c, build_cell_rect(40, 20, 80, 20)),
            (pane_d, Rect::empty_at_origin()),
        ],
        suppressed_pane_ids: vec![pane_d],
        is_all_panes_suppressed: false,
        stack_headers: Vec::new(),
    };

    let destinations =
        list_placement_destinations(&fixture_tree, &layout_solve, FIXTURE_TAB_RECT, pane_a);

    assert_eq!(
        destinations,
        PlacementDestinations {
            swap_slots: vec![
                SwapSlot {
                    pane_id: pane_b,
                    slot_rect: build_cell_rect(40, 0, 80, 20)
                },
                SwapSlot {
                    pane_id: pane_c,
                    slot_rect: build_cell_rect(40, 20, 80, 20)
                },
            ],
            insertion_spans: vec![
                InsertionSpan {
                    anchor: PanePlacementAnchor::Pane(pane_b),
                    span_rect: build_cell_rect(40, 0, 80, 20)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Pane(pane_c),
                    span_rect: build_cell_rect(40, 20, 80, 20)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Tab,
                    span_rect: FIXTURE_TAB_RECT
                },
            ],
        }
    );
}

#[test]
fn a_single_child_split_repeats_no_group_and_the_root_is_never_a_group() {
    let [pane_a, pane_b, pane_c, _] = build_sorted_pane_ids();
    let wrapped_tree = build_horizontal_split(
        vec![build_horizontal_split(
            vec![
                build_pane_leaf(pane_a),
                build_vertical_split(
                    vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
                    &[1, 1],
                ),
            ],
            &[1, 1],
        )],
        &[1],
    );
    let tab_rect = build_cell_rect(0, 0, 80, 20);
    let layout_solve = solve_layout(&wrapped_tree, tab_rect);

    let destinations = list_placement_destinations(&wrapped_tree, &layout_solve, tab_rect, pane_a);

    assert_eq!(
        destinations.insertion_spans,
        vec![
            InsertionSpan {
                anchor: PanePlacementAnchor::Pane(pane_b),
                span_rect: build_cell_rect(40, 0, 40, 10)
            },
            InsertionSpan {
                anchor: PanePlacementAnchor::Pane(pane_c),
                span_rect: build_cell_rect(40, 10, 40, 10)
            },
            InsertionSpan {
                anchor: PanePlacementAnchor::Group(vec![pane_b, pane_c]),
                span_rect: build_cell_rect(40, 0, 40, 20)
            },
            InsertionSpan {
                anchor: PanePlacementAnchor::Tab,
                span_rect: tab_rect
            },
        ]
    );
}

#[test]
fn a_tree_holding_only_the_source_lists_nothing() {
    let [pane_a, _, _, _] = build_sorted_pane_ids();
    let lone_tree = build_pane_leaf(pane_a);
    let tab_rect = build_cell_rect(0, 0, 80, 20);
    let layout_solve = solve_layout(&lone_tree, tab_rect);

    assert_eq!(
        list_placement_destinations(&lone_tree, &layout_solve, tab_rect, pane_a),
        PlacementDestinations {
            swap_slots: Vec::new(),
            insertion_spans: Vec::new(),
        }
    );
}

#[test]
fn placement_error_is_a_recoverable_layout_error() {
    let placement_error = PlacementError::AnchorIsSource {
        pane_id: PaneId::new(),
    };
    assert_eq!(placement_error.category(), DomainCategory::Layout);
    assert_eq!(placement_error.get_severity(), Severity::Recoverable);
}

// ── breaking attempts ───────────────────────────────────────────────────

#[test]
fn swapping_twice_restores_the_original_tree() {
    let pane_ids = build_sorted_pane_ids();
    let [_, pane_b, _, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    let swapped_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_swap_target(pane_b),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("swap succeeds");
    let restored_tree = place_pane_within_tab(
        &swapped_tree,
        pane_d,
        &build_swap_target(pane_b),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("swap back succeeds");

    assert_eq!(restored_tree, fixture_tree);
}

#[test]
fn inserting_d_below_a_puts_d_after_a_in_a_new_vertical_split() {
    let pane_ids = build_sorted_pane_ids();
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let fixture_tree = build_fixture_tree(pane_ids);

    let placed_tree = place_pane_within_tab(
        &fixture_tree,
        pane_d,
        &build_insert_target(PanePlacementAnchor::Pane(pane_a), Direction::Down),
        FIXTURE_TAB_RECT,
        PaneSizing::default(),
    )
    .expect("insertion succeeds");

    assert_eq!(
        placed_tree,
        build_horizontal_split(
            vec![
                build_vertical_split(
                    vec![build_pane_leaf(pane_a), build_pane_leaf(pane_d)],
                    &[1, 1]
                ),
                build_vertical_split(
                    vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
                    &[1, 1]
                ),
            ],
            &[1, 2],
        )
    );
    assert_eq!(
        list_solved_rects(&placed_tree, FIXTURE_TAB_RECT),
        vec![
            (pane_a, build_cell_rect(0, 0, 40, 20)),
            (pane_b, build_cell_rect(40, 0, 80, 20)),
            (pane_c, build_cell_rect(40, 20, 80, 20)),
            (pane_d, build_cell_rect(0, 20, 40, 20)),
        ]
    );
}

#[test]
fn swapping_the_active_stack_member_out_leaves_the_active_slot_to_the_target() {
    let [pane_a, pane_b, pane_c, _] = build_sorted_pane_ids();
    let stack_tree = build_horizontal_split(
        vec![
            build_stack(vec![pane_b, pane_c], 0),
            build_pane_leaf(pane_a),
        ],
        &[1, 1],
    );

    let swapped_tree = place_pane_within_tab(
        &stack_tree,
        pane_b,
        &build_swap_target(pane_a),
        build_cell_rect(0, 0, 80, 20),
        PaneSizing::default(),
    )
    .expect("swap succeeds");

    assert_eq!(
        swapped_tree,
        build_horizontal_split(
            vec![
                build_stack(vec![pane_a, pane_c], 0),
                build_pane_leaf(pane_b)
            ],
            &[1, 1],
        )
    );
}

#[test]
fn a_group_naming_the_source_and_an_absent_pane_reports_the_absent_pane() {
    let pane_ids = build_sorted_pane_ids();
    let pane_d = pane_ids[3];
    let fixture_tree = build_fixture_tree(pane_ids);
    let unknown_pane_id = PaneId::new();

    assert_eq!(
        place_pane_within_tab(
            &fixture_tree,
            pane_d,
            &build_insert_target(
                PanePlacementAnchor::Group(vec![pane_d, unknown_pane_id]),
                Direction::Up
            ),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::TargetPaneNotFound {
            pane_id: unknown_pane_id
        })
    );
}

#[test]
fn a_cross_tab_group_naming_the_source_is_target_not_found() {
    let [pane_a, pane_b, pane_c, pane_d] = build_sorted_pane_ids();
    let source_tree = build_pane_leaf(pane_d);
    let destination_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_vertical_split(
                vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
                &[1, 1],
            ),
        ],
        &[1, 2],
    );

    assert_eq!(
        place_pane_across_tabs(
            &source_tree,
            pane_d,
            &destination_tree,
            &build_insert_target(
                PanePlacementAnchor::Group(vec![pane_b, pane_c, pane_d]),
                Direction::Up
            ),
            FIXTURE_TAB_RECT,
            PaneSizing::default(),
        ),
        Err(PlacementError::TargetPaneNotFound { pane_id: pane_d })
    );
}

#[test]
fn destinations_of_a_collapsed_member_list_its_own_stack_as_a_group() {
    let [pane_a, pane_b, pane_c, _] = build_sorted_pane_ids();
    let stack_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_stack(vec![pane_b, pane_c], 0),
        ],
        &[1, 1],
    );
    let tab_rect = build_cell_rect(0, 0, 80, 20);
    let layout_solve = solve_layout(&stack_tree, tab_rect);

    let destinations = list_placement_destinations(&stack_tree, &layout_solve, tab_rect, pane_c);

    assert_eq!(
        destinations,
        PlacementDestinations {
            swap_slots: vec![
                SwapSlot {
                    pane_id: pane_a,
                    slot_rect: build_cell_rect(0, 0, 40, 20)
                },
                SwapSlot {
                    pane_id: pane_b,
                    slot_rect: build_cell_rect(40, 0, 40, 19)
                },
            ],
            insertion_spans: vec![
                InsertionSpan {
                    anchor: PanePlacementAnchor::Pane(pane_a),
                    span_rect: build_cell_rect(0, 0, 40, 20)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Group(vec![pane_b, pane_c]),
                    span_rect: build_cell_rect(40, 0, 40, 20)
                },
                InsertionSpan {
                    anchor: PanePlacementAnchor::Tab,
                    span_rect: tab_rect
                },
            ],
        }
    );
}

#[test]
fn a_group_rectangle_near_u16_max_saturates_instead_of_overflowing() {
    let [pane_a, pane_b, pane_c, _] = build_sorted_pane_ids();
    let layout_tree = build_horizontal_split(
        vec![
            build_pane_leaf(pane_a),
            build_vertical_split(
                vec![build_pane_leaf(pane_b), build_pane_leaf(pane_c)],
                &[1, 1],
            ),
        ],
        &[1, 1],
    );
    let layout_solve = LayoutSolve {
        pane_rects: vec![
            (pane_a, build_cell_rect(0, 0, 10, 20)),
            (pane_b, build_cell_rect(65530, 0, 10, 10)),
            (pane_c, build_cell_rect(65530, 10, 10, 10)),
        ],
        suppressed_pane_ids: Vec::new(),
        is_all_panes_suppressed: false,
        stack_headers: Vec::new(),
    };
    let tab_rect = build_cell_rect(0, 0, 65535, 20);

    let destinations = list_placement_destinations(&layout_tree, &layout_solve, tab_rect, pane_a);

    assert_eq!(
        destinations.insertion_spans,
        vec![
            InsertionSpan {
                anchor: PanePlacementAnchor::Pane(pane_b),
                span_rect: build_cell_rect(65530, 0, 10, 10)
            },
            InsertionSpan {
                anchor: PanePlacementAnchor::Pane(pane_c),
                span_rect: build_cell_rect(65530, 10, 10, 10)
            },
            InsertionSpan {
                anchor: PanePlacementAnchor::Group(vec![pane_b, pane_c]),
                span_rect: build_cell_rect(65530, 0, 5, 20)
            },
            InsertionSpan {
                anchor: PanePlacementAnchor::Tab,
                span_rect: tab_rect
            },
        ]
    );
}
