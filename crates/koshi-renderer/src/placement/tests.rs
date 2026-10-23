//! Tests for pane placement composition: placed tabs, placement targets and
//! the panes a preview's frames draw.

use super::*;

use koshi_core::command::PanePlacementAnchor;
use koshi_core::geometry::SplitDirection;
use koshi_core::ids::TabId;
use koshi_layout::placement::{PlacementTabSizes, SwapTabSizes};
use koshi_layout::solver::{LayoutSolve, PaneSizing};
use koshi_layout::tree::SplitNode;
use koshi_test_support::fixtures::build_cell_rect;

/// The tab size every test preview checks a placement at.
const TEST_TAB_SIZE: Size = Size {
    column_count: 48,
    row_count: 8,
};

/// A tab size that fits every placement of a test with a stack.
const LARGE_TEST_TAB_SIZE: Size = Size {
    column_count: 120,
    row_count: 30,
};

/// `pane_ids` side by side, each with an equal share.
fn build_side_by_side_tree(pane_ids: &[PaneId]) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        pane_ids.iter().copied().map(LayoutNode::Pane).collect(),
    ))
}

/// Tab `tab_id` with `layout_tree` and `focus_history`, solved tiled at
/// `tab_size` as the session solves a preview's tab.
fn build_test_tab_snapshot(
    tab_id: TabId,
    layout_tree: LayoutNode,
    focus_history: Vec<PaneId>,
    tab_size: Size,
) -> PlacementTabSnapshot {
    let pane_sizing = PaneSizing::default();
    let shown_layout_tree = layout_tree.build_shown_layout_tree(&focus_history);
    let layout_solve = solve_layout_with_sizing(
        shown_layout_tree.as_ref().unwrap_or(&layout_tree),
        Rect::from_size_at_origin(tab_size),
        pane_sizing,
    );
    PlacementTabSnapshot {
        layout_tree,
        focus_history,
        tab_snapshot: TabSnapshot {
            tab_id,
            tab_name: "work".to_owned(),
            pane_slots: list_pane_slots(&layout_solve),
            tab_size,
            stack_headers: layout_solve.stack_headers,
            layout_mode: LayoutMode::Tiled,
            is_every_pane_suppressed: layout_solve.is_every_pane_suppressed,
            gap_cell_count: pane_sizing.gap_cell_count,
        },
    }
}

/// A preview of `source_pane_id` from `source` into `destination_tab_id`,
/// checked at `destination_tab_size` for a split or a stack, with no source
/// tab size and no swap tab sizes.
fn build_test_placement_snapshot(
    source_pane_id: PaneId,
    source: PlacementSource,
    destination_tab_id: TabId,
    destination_tab_snapshot: Option<PlacementTabSnapshot>,
    destination_tab_size: Size,
) -> PlacementSnapshot {
    PlacementSnapshot {
        source_pane_id,
        source,
        destination_tab_id,
        destination_tab_snapshot,
        pane_sizing: PaneSizing::default(),
        insertion_tab_sizes: PlacementTabSizes {
            destination_tab_size: Some(destination_tab_size),
            source_tab_size: None,
        },
        swap_tab_sizes: Vec::new(),
    }
}

/// A preview of the first pane of the tab `[first | second]` within that tab.
fn build_same_tab_placement_snapshot(
    first_pane_id: PaneId,
    second_pane_id: PaneId,
) -> PlacementSnapshot {
    let tab_id = TabId::new();
    let source_tab_snapshot = build_test_tab_snapshot(
        tab_id,
        build_side_by_side_tree(&[first_pane_id, second_pane_id]),
        Vec::new(),
        TEST_TAB_SIZE,
    );
    build_test_placement_snapshot(
        first_pane_id,
        PlacementSource::Tiled(source_tab_snapshot),
        tab_id,
        None,
        TEST_TAB_SIZE,
    )
}

/// The placed preview of tab `tab_id`, named `work`, solved as
/// `layout_solve` at `tab_size`, with `layout_tree` and `focus_history`.
fn build_expected_placed_preview(
    tab_id: TabId,
    layout_tree: LayoutNode,
    focus_history: Vec<PaneId>,
    tab_size: Size,
    layout_solve: LayoutSolve,
) -> PlacedPreview {
    PlacedPreview {
        placed_tab_snapshot: PlacementTabSnapshot {
            layout_tree,
            focus_history,
            tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "work".to_owned(),
                pane_slots: list_pane_slots(&layout_solve),
                tab_size,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
        },
        placed_tab_layout: PlacedTabLayout {
            tab_id,
            tab_size,
            layout_solve,
        },
    }
}

/// A solve that places each of `pane_rects` and suppresses nothing.
fn build_unsuppressed_solve(pane_rects: Vec<(PaneId, Rect)>) -> LayoutSolve {
    LayoutSolve {
        pane_rects,
        suppressed_pane_ids: Vec::new(),
        is_every_pane_suppressed: false,
        stack_headers: Vec::new(),
    }
}

/// A swap tab sizes entry at `tab_size` for each of `target_pane_ids`.
fn build_swap_tab_sizes(target_pane_ids: &[PaneId], tab_size: Size) -> Vec<SwapTabSizes> {
    target_pane_ids
        .iter()
        .map(|&target_pane_id| SwapTabSizes {
            target_pane_id,
            tab_sizes: PlacementTabSizes {
                destination_tab_size: Some(tab_size),
                source_tab_size: None,
            },
        })
        .collect()
}

/// A preview of pane `source_pane_id` of another tab, `[source | neighbor]`,
/// into tab `destination_tab_id` of `destination_tree` with
/// `destination_focus_history`, every placement checked at the large test
/// tab size.
fn build_cross_tab_placement_snapshot(
    source_pane_id: PaneId,
    destination_tab_id: TabId,
    destination_tree: LayoutNode,
    destination_focus_history: Vec<PaneId>,
) -> PlacementSnapshot {
    let destination_pane_ids = destination_tree.list_leaf_pane_ids();
    let mut placement_snapshot = build_test_placement_snapshot(
        source_pane_id,
        PlacementSource::Tiled(build_test_tab_snapshot(
            TabId::new(),
            build_side_by_side_tree(&[source_pane_id, PaneId::new()]),
            Vec::new(),
            LARGE_TEST_TAB_SIZE,
        )),
        destination_tab_id,
        Some(build_test_tab_snapshot(
            destination_tab_id,
            destination_tree,
            destination_focus_history,
            LARGE_TEST_TAB_SIZE,
        )),
        LARGE_TEST_TAB_SIZE,
    );
    placement_snapshot.swap_tab_sizes =
        build_swap_tab_sizes(&destination_pane_ids, LARGE_TEST_TAB_SIZE);
    placement_snapshot
}

#[test]
fn build_placed_preview_swaps_two_panes_within_their_tab() {
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let placement_snapshot = build_same_tab_placement_snapshot(first_pane_id, second_pane_id);

    let placed_preview = placement_snapshot.build_placed_preview(&TiledPlacementTarget::Swap {
        target_pane_id: second_pane_id,
    });

    assert_eq!(
        placed_preview,
        Ok(build_expected_placed_preview(
            placement_snapshot.destination_tab_id,
            build_side_by_side_tree(&[second_pane_id, first_pane_id]),
            vec![first_pane_id],
            TEST_TAB_SIZE,
            build_unsuppressed_solve(vec![
                (second_pane_id, build_cell_rect(0, 0, 24, 8)),
                (first_pane_id, build_cell_rect(24, 0, 24, 8)),
            ]),
        ))
    );
}

#[test]
fn build_placed_preview_swaps_into_another_tab_at_the_swap_tab_size() {
    let source_pane_id = PaneId::new();
    let source_neighbor_pane_id = PaneId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let destination_tab_id = TabId::new();
    let swap_tab_size = Size {
        column_count: 60,
        row_count: 8,
    };
    let mut placement_snapshot = build_test_placement_snapshot(
        source_pane_id,
        PlacementSource::Tiled(build_test_tab_snapshot(
            TabId::new(),
            build_side_by_side_tree(&[source_pane_id, source_neighbor_pane_id]),
            Vec::new(),
            TEST_TAB_SIZE,
        )),
        destination_tab_id,
        Some(build_test_tab_snapshot(
            destination_tab_id,
            build_side_by_side_tree(&[first_pane_id, second_pane_id]),
            Vec::new(),
            TEST_TAB_SIZE,
        )),
        TEST_TAB_SIZE,
    );
    placement_snapshot.swap_tab_sizes = build_swap_tab_sizes(&[second_pane_id], swap_tab_size);

    let placed_preview = placement_snapshot.build_placed_preview(&TiledPlacementTarget::Swap {
        target_pane_id: second_pane_id,
    });

    assert_eq!(
        placed_preview,
        Ok(build_expected_placed_preview(
            destination_tab_id,
            build_side_by_side_tree(&[first_pane_id, source_pane_id]),
            vec![source_pane_id],
            swap_tab_size,
            build_unsuppressed_solve(vec![
                (first_pane_id, build_cell_rect(0, 0, 30, 8)),
                (source_pane_id, build_cell_rect(30, 0, 30, 8)),
            ]),
        ))
    );
    assert_eq!(
        placement_snapshot.build_placed_preview(&TiledPlacementTarget::Swap {
            target_pane_id: first_pane_id,
        }),
        Err(PlacementPreviewError::UnpreviewedTarget),
        "a swap with no swap tab sizes entry is refused"
    );
}

#[test]
fn build_placed_preview_splits_into_another_tab_at_the_insertion_tab_size() {
    let source_pane_id = PaneId::new();
    let source_neighbor_pane_id = PaneId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let destination_tab_id = TabId::new();
    let insertion_tab_size = Size {
        column_count: 60,
        row_count: 8,
    };
    let destination_tree = build_side_by_side_tree(&[first_pane_id, second_pane_id]);
    let mut placement_snapshot = build_test_placement_snapshot(
        source_pane_id,
        PlacementSource::Tiled(build_test_tab_snapshot(
            TabId::new(),
            build_side_by_side_tree(&[source_pane_id, source_neighbor_pane_id]),
            Vec::new(),
            TEST_TAB_SIZE,
        )),
        destination_tab_id,
        Some(build_test_tab_snapshot(
            destination_tab_id,
            destination_tree.clone(),
            Vec::new(),
            TEST_TAB_SIZE,
        )),
        insertion_tab_size,
    );
    placement_snapshot.swap_tab_sizes = build_swap_tab_sizes(&[second_pane_id], TEST_TAB_SIZE);

    let placed_preview = placement_snapshot.build_placed_preview(&TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Down,
    });

    assert_eq!(
        placed_preview,
        Ok(build_expected_placed_preview(
            destination_tab_id,
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![destination_tree, LayoutNode::Pane(source_pane_id)],
            )),
            vec![source_pane_id],
            insertion_tab_size,
            build_unsuppressed_solve(vec![
                (first_pane_id, build_cell_rect(0, 0, 30, 4)),
                (second_pane_id, build_cell_rect(30, 0, 30, 4)),
                (source_pane_id, build_cell_rect(0, 4, 60, 4)),
            ]),
        ))
    );
}

#[test]
fn build_placed_preview_shows_each_stack_at_the_member_the_focus_history_gives() {
    let [first_pane_id, hidden_pane_id, second_pane_id, other_pane_id] =
        [PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new()];
    let tab_id = TabId::new();
    // `[A, X active] | [B active, Y]` with focus history `[A, X]`: the client
    // sees `A` on the left.
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                vec![first_pane_id, hidden_pane_id],
                1,
            )),
            LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                vec![second_pane_id, other_pane_id],
                0,
            )),
        ],
    ));
    let placement_snapshot = build_test_placement_snapshot(
        first_pane_id,
        PlacementSource::Tiled(build_test_tab_snapshot(
            tab_id,
            layout_tree,
            vec![first_pane_id, hidden_pane_id],
            LARGE_TEST_TAB_SIZE,
        )),
        tab_id,
        None,
        LARGE_TEST_TAB_SIZE,
    );

    let placed_tab_snapshot = placement_snapshot
        .build_placed_preview(&TiledPlacementTarget::Swap {
            target_pane_id: second_pane_id,
        })
        .expect("the swap target is in the tab")
        .placed_tab_snapshot;

    // `[B, X active] | [A active, Y]`: the right stack shows `A`, and the
    // left stack shows `X`, the next pane of the focus history.
    assert_eq!(
        placed_tab_snapshot.focus_history,
        vec![first_pane_id, hidden_pane_id]
    );
    assert_eq!(
        placed_tab_snapshot
            .tab_snapshot
            .pane_slots
            .iter()
            .filter(|pane_slot| pane_slot.is_visible)
            .map(|pane_slot| pane_slot.pane_id)
            .collect::<Vec<_>>(),
        vec![hidden_pane_id, first_pane_id]
    );
}

#[test]
fn build_placed_preview_refuses_a_source_tab_too_small_for_what_stays() {
    let source_pane_id = PaneId::new();
    let source_neighbor_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let mut placement_snapshot = build_test_placement_snapshot(
        source_pane_id,
        PlacementSource::Tiled(build_test_tab_snapshot(
            source_tab_id,
            build_side_by_side_tree(&[source_pane_id, source_neighbor_pane_id]),
            Vec::new(),
            TEST_TAB_SIZE,
        )),
        destination_tab_id,
        Some(build_test_tab_snapshot(
            destination_tab_id,
            LayoutNode::Pane(destination_pane_id),
            Vec::new(),
            TEST_TAB_SIZE,
        )),
        TEST_TAB_SIZE,
    );
    let stack_target = TiledPlacementTarget::Stack {
        target_pane_id: destination_pane_id,
    };
    placement_snapshot.insertion_tab_sizes.source_tab_size = Some(TEST_TAB_SIZE);
    assert_eq!(
        placement_snapshot
            .build_placed_preview(&stack_target)
            .map(|placed_preview| placed_preview.placed_tab_snapshot.layout_tree),
        Ok(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![destination_pane_id, source_pane_id],
            0,
        ))),
        "the source tab fits what stays at its full size"
    );

    placement_snapshot.insertion_tab_sizes.source_tab_size = Some(Size {
        column_count: 1,
        row_count: 1,
    });

    assert_eq!(
        placement_snapshot.build_placed_preview(&stack_target),
        Err(PlacementPreviewError::NoRoom {
            tab_id: source_tab_id
        })
    );
}

#[test]
fn build_placed_preview_puts_a_floating_pane_under_the_whole_tab() {
    let floating_pane_id = PaneId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let destination_tab_id = TabId::new();
    let destination_tree = build_side_by_side_tree(&[first_pane_id, second_pane_id]);
    let placement_snapshot = build_test_placement_snapshot(
        floating_pane_id,
        PlacementSource::Floating,
        destination_tab_id,
        Some(build_test_tab_snapshot(
            destination_tab_id,
            destination_tree.clone(),
            Vec::new(),
            TEST_TAB_SIZE,
        )),
        TEST_TAB_SIZE,
    );

    let placed_preview = placement_snapshot.build_placed_preview(&TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Down,
    });

    assert_eq!(
        placed_preview,
        Ok(build_expected_placed_preview(
            destination_tab_id,
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![destination_tree, LayoutNode::Pane(floating_pane_id)],
            )),
            vec![floating_pane_id],
            TEST_TAB_SIZE,
            build_unsuppressed_solve(vec![
                (first_pane_id, build_cell_rect(0, 0, 24, 4)),
                (second_pane_id, build_cell_rect(24, 0, 24, 4)),
                (floating_pane_id, build_cell_rect(0, 4, 48, 4)),
            ]),
        ))
    );
}

#[test]
fn build_placed_preview_refuses_each_placement_the_session_refuses() {
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let placement_snapshot = build_same_tab_placement_snapshot(first_pane_id, second_pane_id);
    let split_target = |destination_tab_id: TabId| TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: PanePlacementAnchor::Pane(second_pane_id),
        direction: Direction::Right,
    };
    assert_eq!(
        placement_snapshot
            .build_placed_preview(&split_target(placement_snapshot.destination_tab_id))
            .map(|placed_preview| placed_preview.placed_tab_snapshot.layout_tree),
        Ok(build_side_by_side_tree(&[second_pane_id, first_pane_id])),
        "a split of the destination tab is accepted"
    );

    assert_eq!(
        placement_snapshot.build_placed_preview(&split_target(TabId::new())),
        Err(PlacementPreviewError::UnpreviewedTarget),
        "a split of another tab"
    );
    let unknown_pane_id = PaneId::new();
    assert_eq!(
        placement_snapshot.build_placed_preview(&TiledPlacementTarget::Swap {
            target_pane_id: unknown_pane_id,
        }),
        Err(PlacementPreviewError::LayoutRefusal(
            PlacementError::TargetPaneNotFound {
                pane_id: unknown_pane_id
            }
        )),
        "a swap with a pane the tab does not hold"
    );
    assert_eq!(
        placement_snapshot.build_placed_preview(&TiledPlacementTarget::Split {
            destination_tab_id: placement_snapshot.destination_tab_id,
            anchor: PanePlacementAnchor::Pane(unknown_pane_id),
            direction: Direction::Right,
        }),
        Err(PlacementPreviewError::LayoutRefusal(
            PlacementError::TargetPaneNotFound {
                pane_id: unknown_pane_id
            }
        )),
        "a split of a pane the tab does not hold"
    );
    let mut sizeless_placement_snapshot = placement_snapshot.clone();
    sizeless_placement_snapshot
        .insertion_tab_sizes
        .destination_tab_size = None;
    assert_eq!(
        sizeless_placement_snapshot.build_placed_preview(&TiledPlacementTarget::Swap {
            target_pane_id: second_pane_id,
        }),
        Err(PlacementPreviewError::NoDestinationTabSize),
        "no destination tab size"
    );
    let mut too_small_placement_snapshot = placement_snapshot;
    too_small_placement_snapshot
        .insertion_tab_sizes
        .destination_tab_size = Some(Size {
        column_count: 3,
        row_count: 1,
    });
    assert_eq!(
        too_small_placement_snapshot.build_placed_preview(&split_target(
            too_small_placement_snapshot.destination_tab_id
        )),
        Err(PlacementPreviewError::NoRoom {
            tab_id: too_small_placement_snapshot.destination_tab_id
        }),
        "a destination tab too small for the placed tree"
    );
}

#[test]
fn build_placed_preview_refuses_a_floating_pane_with_no_destination_tab() {
    let floating_pane_id = PaneId::new();
    let destination_tab_id = TabId::new();
    let placement_snapshot = build_test_placement_snapshot(
        floating_pane_id,
        PlacementSource::Floating,
        destination_tab_id,
        None,
        TEST_TAB_SIZE,
    );

    assert_eq!(
        placement_snapshot.build_placed_preview(&TiledPlacementTarget::Split {
            destination_tab_id,
            anchor: PanePlacementAnchor::Tab,
            direction: Direction::Down,
        }),
        Err(PlacementPreviewError::UnpreviewedTarget)
    );
}

#[test]
fn list_placement_targets_lists_swaps_and_stacks_then_splits_of_each_span() {
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let placement_snapshot = build_same_tab_placement_snapshot(first_pane_id, second_pane_id);
    let destination_tab_id = placement_snapshot.destination_tab_id;
    let mut expected_placement_targets = vec![
        TiledPlacementTarget::Swap {
            target_pane_id: second_pane_id,
        },
        TiledPlacementTarget::Stack {
            target_pane_id: second_pane_id,
        },
    ];
    for anchor in [
        PanePlacementAnchor::Pane(second_pane_id),
        PanePlacementAnchor::Tab,
    ] {
        for direction in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            expected_placement_targets.push(TiledPlacementTarget::Split {
                destination_tab_id,
                anchor: anchor.clone(),
                direction,
            });
        }
    }

    assert_eq!(
        placement_snapshot.list_placement_targets(),
        expected_placement_targets
    );
}

#[test]
fn list_placement_targets_and_drawable_panes_are_empty_with_no_displayed_tab() {
    let placement_snapshot = build_test_placement_snapshot(
        PaneId::new(),
        PlacementSource::Floating,
        TabId::new(),
        None,
        TEST_TAB_SIZE,
    );

    assert_eq!(placement_snapshot.list_placement_destinations(), None);
    assert_eq!(placement_snapshot.list_placement_targets(), Vec::new());
    assert_eq!(placement_snapshot.list_drawable_pane_ids(), Vec::new());
}

#[test]
fn list_drawable_pane_ids_leaves_out_a_stack_member_no_placement_opens() {
    let source_pane_id = PaneId::new();
    let tiled_pane_id = PaneId::new();
    let shown_stack_pane_id = PaneId::new();
    let collapsed_stack_pane_id = PaneId::new();
    let placement_snapshot = build_cross_tab_placement_snapshot(
        source_pane_id,
        TabId::new(),
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(tiled_pane_id),
                LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                    vec![shown_stack_pane_id, collapsed_stack_pane_id],
                    0,
                )),
            ],
        )),
        vec![shown_stack_pane_id],
    );

    assert_eq!(
        placement_snapshot.list_drawable_pane_ids(),
        vec![tiled_pane_id, shown_stack_pane_id, source_pane_id]
    );
}

#[test]
fn list_drawable_pane_ids_adds_a_stack_member_a_placement_opens() {
    let source_pane_id = PaneId::new();
    let shown_stack_pane_id = PaneId::new();
    let collapsed_stack_pane_id = PaneId::new();
    let tab_id = TabId::new();
    let placement_snapshot = build_test_placement_snapshot(
        source_pane_id,
        PlacementSource::Tiled(build_test_tab_snapshot(
            tab_id,
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Horizontal,
                vec![
                    LayoutNode::Pane(source_pane_id),
                    LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                        vec![shown_stack_pane_id, collapsed_stack_pane_id],
                        0,
                    )),
                ],
            )),
            vec![shown_stack_pane_id],
            LARGE_TEST_TAB_SIZE,
        )),
        tab_id,
        None,
        LARGE_TEST_TAB_SIZE,
    );

    assert_eq!(
        placement_snapshot.list_drawable_pane_ids(),
        vec![source_pane_id, shown_stack_pane_id, collapsed_stack_pane_id],
        "a swap of the source pane with the collapsed member opens that member"
    );
}

/// The panes `placement_snapshot` draws, listed by building every placement
/// of [`PlacementSnapshot::list_placement_targets`]: each visible pane of the
/// displayed tab, then each pane that a placement the session accepts shows
/// first, in target order.
fn list_drawable_pane_ids_by_building_every_placement(
    placement_snapshot: &PlacementSnapshot,
) -> Vec<PaneId> {
    let Some(displayed_tab_snapshot) = placement_snapshot.find_displayed_tab_snapshot() else {
        return Vec::new();
    };
    let mut placed_tab_snapshots = vec![displayed_tab_snapshot.tab_snapshot.clone()];
    for placement_target in placement_snapshot.list_placement_targets() {
        if let Ok(placed_preview) = placement_snapshot.build_placed_preview(&placement_target) {
            placed_tab_snapshots.push(placed_preview.placed_tab_snapshot.tab_snapshot);
        }
    }
    let mut drawable_pane_ids = Vec::new();
    for tab_snapshot in &placed_tab_snapshots {
        for pane_slot in &tab_snapshot.pane_slots {
            if pane_slot.is_visible && !drawable_pane_ids.contains(&pane_slot.pane_id) {
                drawable_pane_ids.push(pane_slot.pane_id);
            }
        }
    }
    drawable_pane_ids
}

#[test]
fn list_drawable_pane_ids_lists_a_floating_pane_after_panes_that_all_show() {
    let floating_pane_id = PaneId::new();
    let tiled_pane_ids = [
        PaneId::new(),
        PaneId::new(),
        PaneId::new(),
        PaneId::new(),
        PaneId::new(),
    ];
    let destination_tab_id = TabId::new();
    let placement_snapshot = build_test_placement_snapshot(
        floating_pane_id,
        PlacementSource::Floating,
        destination_tab_id,
        Some(build_test_tab_snapshot(
            destination_tab_id,
            build_side_by_side_tree(&tiled_pane_ids),
            Vec::new(),
            LARGE_TEST_TAB_SIZE,
        )),
        LARGE_TEST_TAB_SIZE,
    );
    let mut expected_drawable_pane_ids = tiled_pane_ids.to_vec();
    expected_drawable_pane_ids.push(floating_pane_id);

    assert_eq!(
        placement_snapshot.list_drawable_pane_ids(),
        expected_drawable_pane_ids
    );
    assert_eq!(
        list_drawable_pane_ids_by_building_every_placement(&placement_snapshot),
        expected_drawable_pane_ids
    );
}

#[test]
fn list_drawable_pane_ids_leaves_out_a_floating_pane_no_placement_fits() {
    let floating_pane_id = PaneId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let destination_tab_id = TabId::new();
    let mut placement_snapshot = build_test_placement_snapshot(
        floating_pane_id,
        PlacementSource::Floating,
        destination_tab_id,
        Some(build_test_tab_snapshot(
            destination_tab_id,
            build_side_by_side_tree(&[first_pane_id, second_pane_id]),
            Vec::new(),
            TEST_TAB_SIZE,
        )),
        TEST_TAB_SIZE,
    );
    placement_snapshot.insertion_tab_sizes.destination_tab_size = Some(Size {
        column_count: 4,
        row_count: 2,
    });

    assert_eq!(
        placement_snapshot.list_drawable_pane_ids(),
        vec![first_pane_id, second_pane_id]
    );
    assert_eq!(
        list_drawable_pane_ids_by_building_every_placement(&placement_snapshot),
        vec![first_pane_id, second_pane_id]
    );
}

#[test]
fn list_drawable_pane_ids_lists_what_building_every_placement_lists() {
    let [tiled_pane_id, shown_stack_pane_id, collapsed_stack_pane_id, moved_pane_id] =
        [PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new()];
    let [subtree_first_pane_id, subtree_second_pane_id] = [PaneId::new(), PaneId::new()];
    let [inner_shown_pane_id, inner_collapsed_pane_id] = [PaneId::new(), PaneId::new()];
    let floating_pane_id = PaneId::new();
    let destination_tab_id = TabId::new();
    let plain_pane_beside_stack_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(tiled_pane_id),
            LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                vec![shown_stack_pane_id, collapsed_stack_pane_id],
                0,
            )),
        ],
    ));
    // `[T | stack(H[X, U], Y active)]`: the collapsed member `H[X, U]` shows
    // `X` on its header, and `U` nowhere.
    let collapsed_subtree_member_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(tiled_pane_id),
            LayoutNode::Split(SplitNode {
                active_child_index: 1,
                ..SplitNode::with_equal_weights(
                    SplitDirection::Stacked,
                    vec![
                        build_side_by_side_tree(&[subtree_first_pane_id, subtree_second_pane_id]),
                        LayoutNode::Pane(shown_stack_pane_id),
                    ],
                )
            }),
        ],
    ));
    // `stack(H[X, stack(P active, Q)], Y active)`: a stack inside the
    // collapsed member.
    let nested_stack_tree = LayoutNode::Split(SplitNode {
        active_child_index: 1,
        ..SplitNode::with_equal_weights(
            SplitDirection::Stacked,
            vec![
                LayoutNode::Split(SplitNode::with_equal_weights(
                    SplitDirection::Horizontal,
                    vec![
                        LayoutNode::Pane(subtree_first_pane_id),
                        LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                            vec![inner_shown_pane_id, inner_collapsed_pane_id],
                            0,
                        )),
                    ],
                )),
                LayoutNode::Pane(shown_stack_pane_id),
            ],
        )
    });
    let floating_placement_snapshot = build_test_placement_snapshot(
        floating_pane_id,
        PlacementSource::Floating,
        destination_tab_id,
        Some(build_test_tab_snapshot(
            destination_tab_id,
            plain_pane_beside_stack_tree,
            vec![shown_stack_pane_id],
            LARGE_TEST_TAB_SIZE,
        )),
        LARGE_TEST_TAB_SIZE,
    );
    let subtree_placement_snapshot = build_cross_tab_placement_snapshot(
        moved_pane_id,
        destination_tab_id,
        collapsed_subtree_member_tree,
        vec![shown_stack_pane_id],
    );
    let nested_stack_placement_snapshot = build_cross_tab_placement_snapshot(
        moved_pane_id,
        destination_tab_id,
        nested_stack_tree,
        vec![shown_stack_pane_id],
    );

    for (placement_snapshot, expected_drawable_pane_ids) in [
        (
            &floating_placement_snapshot,
            vec![tiled_pane_id, shown_stack_pane_id, floating_pane_id],
        ),
        // A swap of `X` with the moved pane opens `H[moved, U]`.
        (
            &subtree_placement_snapshot,
            vec![
                tiled_pane_id,
                shown_stack_pane_id,
                moved_pane_id,
                subtree_second_pane_id,
            ],
        ),
        // A swap of `X` with the moved pane opens the member, and its inner
        // stack shows `P`.
        (
            &nested_stack_placement_snapshot,
            vec![shown_stack_pane_id, moved_pane_id, inner_shown_pane_id],
        ),
    ] {
        assert_eq!(
            placement_snapshot.list_drawable_pane_ids(),
            expected_drawable_pane_ids
        );
        assert_eq!(
            list_drawable_pane_ids_by_building_every_placement(placement_snapshot),
            expected_drawable_pane_ids
        );
    }
}
