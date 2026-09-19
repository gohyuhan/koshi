//! Tests for layout modes: fullscreen and tiled.

use koshi_core::geometry::{Rect, Size, SplitDirection};

use super::*;
use crate::solver::{solve_layout, solve_layout_with_mode, LayoutSolve, PaneSizing};
use crate::tree::{LayoutNode, SplitNode};

/// [`solve_layout_with_mode`] at the default [`PaneSizing`].
fn solve_layout_in_mode(
    layout_tree: &LayoutNode,
    layout_mode: LayoutMode,
    tab_rect: Rect,
) -> LayoutSolve {
    solve_layout_with_mode(layout_tree, layout_mode, tab_rect, PaneSizing::default())
}

fn build_pane_leaf(pane_id: PaneId) -> LayoutNode {
    LayoutNode::Pane(pane_id)
}

fn build_test_tab_rect() -> Rect {
    Rect::from_size_at_origin(Size {
        column_count: 80,
        row_count: 24,
    })
}

/// Builds a test layout: a horizontal split with the first pane on the left
/// and two vertically stacked panes on the right.
fn build_nested_layout(
    first_pane_id: PaneId,
    second_pane_id: PaneId,
    third_pane_id: PaneId,
) -> LayoutNode {
    let vertical_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    ));
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), vertical_split],
    ))
}

#[test]
fn fullscreen_promotes_the_focused_pane_and_hides_the_rest() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_nested_layout(first_pane_id, second_pane_id, third_pane_id);

    let layout_result = solve_layout_in_mode(
        &layout_tree,
        LayoutMode::Fullscreen {
            focused_pane_id: second_pane_id,
        },
        build_test_tab_rect(),
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (first_pane_id, Rect::empty_at_origin()),
            (second_pane_id, build_test_tab_rect()),
            (third_pane_id, Rect::empty_at_origin()),
        ]
    );
    // Hidden panes are not suppressed; they can be toggled back. An overlay
    // should not be drawn over a pane that fits on screen.
    assert!(layout_result.suppressed_pane_ids.is_empty());
    assert!(!layout_result.is_all_panes_suppressed);
}

#[test]
fn leaving_fullscreen_restores_the_exact_prior_layout() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_nested_layout(first_pane_id, second_pane_id, third_pane_id);
    let tiled_layout_before_fullscreen =
        solve_layout_in_mode(&layout_tree, LayoutMode::Tiled, build_test_tab_rect());

    // Entering and leaving fullscreen does not modify the tree. The tiled
    // solve after toggling fullscreen must match the original solve.
    let original_layout_tree = layout_tree.clone();
    let _ = solve_layout_in_mode(
        &layout_tree,
        LayoutMode::Fullscreen {
            focused_pane_id: third_pane_id,
        },
        build_test_tab_rect(),
    );
    assert_eq!(layout_tree, original_layout_tree);
    assert_eq!(
        solve_layout_in_mode(&layout_tree, LayoutMode::Tiled, build_test_tab_rect()),
        tiled_layout_before_fullscreen
    );
}

#[test]
fn tiled_mode_matches_the_plain_solve() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_nested_layout(first_pane_id, second_pane_id, third_pane_id);
    assert_eq!(
        solve_layout_in_mode(&layout_tree, LayoutMode::Tiled, build_test_tab_rect()),
        solve_layout(&layout_tree, build_test_tab_rect())
    );
}

#[test]
fn fullscreen_of_the_only_pane_matches_the_tiled_solve() {
    let pane_id = PaneId::new();
    let layout_tree = LayoutNode::Pane(pane_id);

    let layout_result = solve_layout_in_mode(
        &layout_tree,
        LayoutMode::Fullscreen {
            focused_pane_id: pane_id,
        },
        build_test_tab_rect(),
    );
    assert_eq!(
        layout_result,
        solve_layout(&layout_tree, build_test_tab_rect())
    );
    assert_eq!(layout_result.pane_rects, [(pane_id, build_test_tab_rect())]);
}

#[test]
fn layout_mode_serializes_as_an_externally_tagged_enum() {
    let focused_pane_id = PaneId::new();
    let focused_pane_id_json = serde_json::to_value(focused_pane_id).unwrap();

    assert_eq!(
        serde_json::to_value(LayoutMode::Tiled).unwrap(),
        serde_json::json!("Tiled")
    );
    assert_eq!(
        serde_json::to_value(LayoutMode::Fullscreen { focused_pane_id }).unwrap(),
        serde_json::json!({ "Fullscreen": { "focused_pane_id": focused_pane_id_json } })
    );
}

#[test]
fn layout_mode_round_trips_through_serde() {
    let focused_pane_id = PaneId::new();
    for mode in [
        LayoutMode::Tiled,
        LayoutMode::Fullscreen { focused_pane_id },
    ] {
        let serialized_layout_mode_json = serde_json::to_string(&mode).unwrap();
        assert_eq!(
            serde_json::from_str::<LayoutMode>(&serialized_layout_mode_json).unwrap(),
            mode
        );
    }
}

#[test]
fn stale_fullscreen_focus_falls_back_to_tiled() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_nested_layout(first_pane_id, second_pane_id, third_pane_id);

    let missing_pane_id = PaneId::new();
    let layout_result = solve_layout_in_mode(
        &layout_tree,
        LayoutMode::Fullscreen {
            focused_pane_id: missing_pane_id,
        },
        build_test_tab_rect(),
    );
    assert_eq!(
        layout_result,
        solve_layout(&layout_tree, build_test_tab_rect())
    );
}

#[test]
fn fullscreen_promotes_a_collapsed_stack_member_without_touching_the_stack() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![second_pane_id, third_pane_id],
        0,
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), stack],
    ));
    let original_layout_tree = layout_tree.clone();

    // Fullscreen promotes the collapsed third pane to fill the entire
    // tab, while the stack and all siblings are hidden.
    let layout_result = solve_layout_in_mode(
        &layout_tree,
        LayoutMode::Fullscreen {
            focused_pane_id: third_pane_id,
        },
        build_test_tab_rect(),
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (first_pane_id, Rect::empty_at_origin()),
            (second_pane_id, Rect::empty_at_origin()),
            (third_pane_id, build_test_tab_rect()),
        ]
    );
    assert!(layout_result.stack_headers.is_empty());
    assert!(layout_result.suppressed_pane_ids.is_empty());

    // Entering fullscreen does not modify the stack structure, so exiting
    // fullscreen restores all prior collapse state.
    assert_eq!(layout_tree, original_layout_tree);
    let tiled_layout_after_fullscreen =
        solve_layout_in_mode(&layout_tree, LayoutMode::Tiled, build_test_tab_rect());
    let LayoutNode::Split(root_split_node) = &layout_tree else {
        panic!("root must stay a split");
    };
    let LayoutNode::Split(stack) = &root_split_node.children[1] else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active_child_index, 0);
    assert_eq!(
        tiled_layout_after_fullscreen,
        solve_layout(&layout_tree, build_test_tab_rect())
    );
    assert_eq!(tiled_layout_after_fullscreen.stack_headers.len(), 1);
    assert_eq!(
        tiled_layout_after_fullscreen.stack_headers[0].pane_id,
        third_pane_id
    );
}

#[test]
fn fullscreen_of_the_active_stack_member_round_trips_identically() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, second_pane_id],
        1,
    ));
    let original_layout_solution = solve_layout(&layout_tree, build_test_tab_rect());

    let fullscreen_layout_solution = solve_layout_in_mode(
        &layout_tree,
        LayoutMode::Fullscreen {
            focused_pane_id: second_pane_id,
        },
        build_test_tab_rect(),
    );
    assert_eq!(
        fullscreen_layout_solution.pane_rects,
        [
            (first_pane_id, Rect::empty_at_origin()),
            (second_pane_id, build_test_tab_rect())
        ]
    );

    assert_eq!(
        solve_layout_in_mode(&layout_tree, LayoutMode::Tiled, build_test_tab_rect()),
        original_layout_solution
    );
}

#[test]
fn fullscreen_in_a_too_small_tab_suppresses_and_flags_the_overlay() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(first_pane_id),
            build_pane_leaf(second_pane_id),
        ],
    ));
    let undersized_tab_rect = Rect::from_size_at_origin(Size {
        column_count: 1,
        row_count: 1,
    });

    let layout_result = solve_layout_in_mode(
        &layout_tree,
        LayoutMode::Fullscreen {
            focused_pane_id: first_pane_id,
        },
        undersized_tab_rect,
    );
    assert_eq!(layout_result.suppressed_pane_ids, [first_pane_id]);
    assert!(layout_result.is_all_panes_suppressed);
}

#[test]
fn fullscreen_suppresses_a_tab_that_fits_content_but_not_the_border() {
    // Pane content requires (2,1) space minimum; borders add (1,1) on each
    // side. In a 3x2 tab, content fits but a border (4x3 total) does not.
    // Fullscreen suppresses the pane to avoid drawing borders that overflow.
    // At exactly (4,3), the border fits, so the pane shows with 1-cell inset.
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(first_pane_id),
            build_pane_leaf(second_pane_id),
        ],
    ));

    let undersized_tab_rect = Rect::from_size_at_origin(Size {
        column_count: 3,
        row_count: 2,
    });
    let suppressed_layout = solve_layout_in_mode(
        &layout_tree,
        LayoutMode::Fullscreen {
            focused_pane_id: first_pane_id,
        },
        undersized_tab_rect,
    );
    assert_eq!(suppressed_layout.suppressed_pane_ids, [first_pane_id]);
    assert!(suppressed_layout.is_all_panes_suppressed);

    let border_fitting_tab_rect = Rect::from_size_at_origin(Size {
        column_count: 4,
        row_count: 3,
    });
    let visible_layout = solve_layout_in_mode(
        &layout_tree,
        LayoutMode::Fullscreen {
            focused_pane_id: first_pane_id,
        },
        border_fitting_tab_rect,
    );
    assert!(visible_layout.suppressed_pane_ids.is_empty());
    assert!(!visible_layout.is_all_panes_suppressed);
    assert_eq!(
        visible_layout.pane_rects,
        [
            (first_pane_id, border_fitting_tab_rect),
            (second_pane_id, Rect::empty_at_origin())
        ]
    );
    assert_eq!(
        border_fitting_tab_rect
            .compute_inner_with_border()
            .cell_size,
        Size {
            column_count: 2,
            row_count: 1,
        }
    );
}
