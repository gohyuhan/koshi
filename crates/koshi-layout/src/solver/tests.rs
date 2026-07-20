//! Tests for geometry solver: tree + rect → pane rectangles.

use koshi_core::geometry::{Point, SplitDirection};
use koshi_test_support::layout_assert::{
    assert_all_space_occupied, assert_min_size_respected, assert_no_outside, assert_no_overlap,
};

use super::*;
use crate::tree::LayoutChild;

/// Construct a rectangle with the given origin (x, y) and size (cols, rows).
fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x, y }, Size { cols, rows })
}

/// Wrap a pane ID in a leaf node.
fn leaf(pane: PaneId) -> LayoutChild {
    LayoutChild::new(LayoutNode::Pane(pane))
}

/// Create a split node in the given direction with equal-weight children for each pane.
fn split(direction: SplitDirection, panes: &[PaneId]) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        direction,
        panes.iter().map(|&pane| leaf(pane)).collect(),
    ))
}

/// A split whose children carry explicit primary constraints.
fn split_with(direction: SplitDirection, children: Vec<(PaneId, SizeWeight)>) -> LayoutNode {
    let mut node = SplitNode::with_equal_weights(
        direction,
        children.iter().map(|&(pane, _)| leaf(pane)).collect(),
    );
    node.weights = children.into_iter().map(|(_, weight)| weight).collect();
    LayoutNode::Split(node)
}

/// A left-leaning tree of `panes.len() - 1` splits whose direction alternates
/// horizontal/vertical by depth: `split(h, [p0, split(v, [p1, split(h, …)])])`.
/// Used to stress deeply nested solving and close ordering.
fn deep_alternating(panes: &[PaneId]) -> LayoutNode {
    let (&last, rest) = panes
        .split_last()
        .expect("deep_alternating needs at least one pane");
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

/// Verify that the solved panes fill the tab completely with no gaps, overlaps, or spillage.
fn assert_tiles_exactly(result: &SolveResult, tab: Rect) {
    assert_all_space_occupied(&result.panes, tab).unwrap();
    assert_no_overlap(&result.panes).unwrap();
    assert_no_outside(&result.panes, tab).unwrap();
}

#[test]
fn single_pane_fills_the_tab() {
    let pane = PaneId::new();
    let tab = rect(0, 0, 80, 24);
    let result = solve(&LayoutNode::Pane(pane), tab);
    assert_eq!(result.panes, [(pane, tab)]);
    assert!(result.suppressed.is_empty());
    assert!(!result.all_suppressed);
}

#[test]
fn horizontal_split_divides_columns() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tab = rect(0, 0, 80, 24);
    let result = solve(&split(SplitDirection::Horizontal, &[a, b]), tab);
    assert_eq!(
        result.panes,
        [(a, rect(0, 0, 40, 24)), (b, rect(40, 0, 40, 24))]
    );
}

#[test]
fn vertical_split_divides_rows() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tab = rect(0, 0, 80, 24);
    let result = solve(&split(SplitDirection::Vertical, &[a, b]), tab);
    assert_eq!(
        result.panes,
        [(a, rect(0, 0, 80, 12)), (b, rect(0, 12, 80, 12))]
    );
}

#[test]
fn odd_remainder_goes_to_the_trailing_pane_and_is_stable() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Horizontal, &[a, b]);
    let tab = rect(0, 0, 101, 24);

    let first = solve(&tree, tab);
    assert_eq!(
        first.panes,
        [(a, rect(0, 0, 50, 24)), (b, rect(50, 0, 51, 24))]
    );
    for _ in 0..10 {
        assert_eq!(solve(&tree, tab), first);
    }
}

#[test]
fn three_way_split_sums_to_the_full_width() {
    let panes = [PaneId::new(), PaneId::new(), PaneId::new()];
    let tab = rect(0, 0, 80, 24);
    let result = solve(&split(SplitDirection::Horizontal, &panes), tab);

    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [26, 27, 27]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn nested_tree_tiles_the_tab_exactly() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(inner)],
    ));
    let tab = rect(0, 0, 81, 25);

    let result = solve(&tree, tab);
    assert_tiles_exactly(&result, tab);
    assert_min_size_respected(&result.panes, Size { cols: 2, rows: 1 }).unwrap();
    assert_eq!(result.panes.len(), 3);
}

#[test]
fn fixed_then_percent_then_flex_distribution() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Fixed(10))),
            (b, SizeWeight::new(SizeConstraint::Percent(50))),
            (c, SizeWeight::new(SizeConstraint::Flex(1))),
        ],
    );
    let tab = rect(0, 0, 100, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [10, 50, 40]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn flex_weights_share_proportionally() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Flex(2))),
            (b, SizeWeight::new(SizeConstraint::Flex(1))),
        ],
    );
    let tab = rect(0, 0, 90, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [60, 30]);
}

#[test]
fn missing_weights_fall_back_to_the_default_share() {
    // Hand-built: a deserialized split can carry fewer weights than
    // children. The unweighted child takes the default share instead of
    // panicking the distribution.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![leaf(a), leaf(b)],
        weights: vec![SizeWeight::new(SizeConstraint::Flex(1))],
        active: 0,
    });
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [40, 40]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn an_out_of_range_percent_caps_at_the_whole_axis() {
    // Hand-built: validation rejects Percent above 100, but a raw tree can
    // carry one (via serde). Out-of-range values are capped at 100 to
    // prevent truncation when casting on wide axes.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Percent(255))),
            (b, SizeWeight::new(SizeConstraint::Flex(1))),
        ],
    );
    let tab = rect(0, 0, 40_000, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [39_996, 4]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn all_zero_flex_weights_solve_without_panicking() {
    // The validated constructors reject `Flex(0)`, but the variant stays
    // representable through serde and direct construction. The solver must
    // degrade to the leftover distribution instead of dividing by zero.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Flex(0))),
            (b, SizeWeight::new(SizeConstraint::Flex(0))),
        ],
    );
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn resize_deltas_shift_cells_between_siblings() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let grow = SizeWeight {
        resize_delta: 5,
        ..SizeWeight::default()
    };
    let shrink = SizeWeight {
        resize_delta: -5,
        ..SizeWeight::default()
    };
    let tree = split_with(SplitDirection::Horizontal, vec![(a, grow), (b, shrink)]);
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [45, 35]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn all_fixed_underfill_gives_slack_to_the_last_child() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Fixed(10))),
            (b, SizeWeight::new(SizeConstraint::Fixed(10))),
        ],
    );
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [10, 70]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn min_floor_is_honored_when_the_layout_fits() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let wide = SizeWeight::default().with_min(20).unwrap();
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, wide),
            (b, SizeWeight::default()),
            (c, SizeWeight::default()),
        ],
    );
    let tab = rect(0, 0, 30, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    // `a` holds its declared min of 20; the two default siblings split the
    // remaining 10 down to their border-inclusive floor of 4.
    assert_eq!(widths, [20, 6, 4]);
    assert_tiles_exactly(&result, tab);
    assert_min_size_respected(&result.panes, Size { cols: 2, rows: 1 }).unwrap();
}

#[test]
fn min_primary_acts_as_a_floor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Min(15))),
            (b, SizeWeight::new(SizeConstraint::Flex(1))),
        ],
    );
    let tab = rect(0, 0, 20, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [15, 5]);
}

#[test]
fn preferred_target_is_honored_when_slack_allows() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Preferred(30))),
            (b, SizeWeight::new(SizeConstraint::Flex(1))),
        ],
    );
    let tab = rect(0, 0, 100, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [30, 70]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn preferred_target_stops_at_the_donors_floor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Preferred(90))),
            (b, SizeWeight::default().with_min(20).unwrap()),
        ],
    );
    let tab = rect(0, 0, 100, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    // The donor gives down to its floor of 20; the target settles at 80.
    assert_eq!(widths, [80, 20]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn preferred_target_without_flexible_donors_stays_unmet() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Preferred(80))),
            (b, SizeWeight::new(SizeConstraint::Fixed(20))),
            (c, SizeWeight::new(SizeConstraint::Fixed(60))),
        ],
    );
    let tab = rect(0, 0, 100, 24);

    // A preference is only a hint: with nothing but exact-sized siblings,
    // there is no slack and the target is quietly unmet.
    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [20, 20, 60]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn floors_outrank_fixed_sizes_when_no_flexible_donor_remains() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::default().with_min(20).unwrap()),
            (b, SizeWeight::new(SizeConstraint::Fixed(30))),
        ],
    );
    let tab = rect(0, 0, 40, 24);

    // The fixed sibling claims 30 of 40 first, leaving the flexible child
    // at 10 — below its floor of 20. The clamp may tap fixed children, so
    // the floor wins and the fixed pane gives the difference back.
    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [20, 20]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn resize_deltas_clamp_at_zero_and_at_the_full_axis() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tab = rect(0, 0, 80, 24);

    // A runaway positive delta saturates at the axis, then the floor clamp
    // claws back the sibling's minimum.
    let grow = SizeWeight {
        resize_delta: 1000,
        ..SizeWeight::default()
    };
    let grown = solve(
        &split_with(
            SplitDirection::Horizontal,
            vec![(a, grow), (b, SizeWeight::default())],
        ),
        tab,
    );
    let widths: Vec<u16> = grown.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [76, 4]);
    assert_tiles_exactly(&grown, tab);

    // A runaway negative delta clamps to zero, and the floor clamp brings
    // the child back up to its minimum.
    let shrink = SizeWeight {
        resize_delta: -1000,
        ..SizeWeight::default()
    };
    let shrunk = solve(
        &split_with(
            SplitDirection::Horizontal,
            vec![(a, shrink), (b, SizeWeight::default())],
        ),
        tab,
    );
    let widths: Vec<u16> = shrunk.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [4, 76]);
    assert_tiles_exactly(&shrunk, tab);
}

#[test]
fn underfilled_percents_leave_the_remainder_to_flex() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Percent(30))),
            (b, SizeWeight::new(SizeConstraint::Percent(30))),
            (c, SizeWeight::new(SizeConstraint::Flex(1))),
        ],
    );
    let tab = rect(0, 0, 100, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [30, 30, 40]);
    assert_tiles_exactly(&result, tab);
}

#[test]
fn fits_accepts_a_layout_with_room_for_every_floor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Horizontal, &[a, b]);
    // Two bordered panes need a four-by-three box each: eight columns, three
    // rows side by side.
    assert!(fits(&tree, rect(0, 0, 8, 3), MIN_PANE_SIZE));
    assert!(fits(&tree, rect(0, 0, 80, 24), MIN_PANE_SIZE));
}

#[test]
fn fits_rejects_a_layout_whose_floors_exceed_the_rect() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Horizontal, &[a, b]);
    // Two panes at two columns each need four; three is one short.
    assert!(!fits(&tree, rect(0, 0, 3, 24), MIN_PANE_SIZE));
}

#[test]
fn fits_accounts_for_nested_axis_minimums() {
    let (a, b, c, d) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c), leaf(d)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(column)],
    ));

    // Each bordered pane needs a four-by-three box. The three-deep column
    // stacks to nine rows; the leaf beside it adds its four columns, so the
    // tree needs eight columns and nine rows.
    assert!(fits(&tree, rect(0, 0, 8, 9), MIN_PANE_SIZE));
    assert!(!fits(&tree, rect(0, 0, 8, 8), MIN_PANE_SIZE));
    assert!(!fits(&tree, rect(0, 0, 7, 9), MIN_PANE_SIZE));
}

#[test]
fn fits_uses_declared_floors_not_just_defaults() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let wide = SizeWeight::default().with_min(30).unwrap();
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![(a, wide), (b, SizeWeight::default())],
    );
    // `a`'s declared 30 plus the default sibling's border-inclusive floor of
    // 4 need 34 columns.
    assert!(fits(&tree, rect(0, 0, 34, 24), MIN_PANE_SIZE));
    assert!(!fits(&tree, rect(0, 0, 33, 24), MIN_PANE_SIZE));
}

#[test]
fn shrink_suppresses_trailing_panes_deterministically() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Horizontal, &[a, b, c]);
    // Three bordered panes need twelve columns; nine fit only the first two.
    let tab = rect(0, 0, 9, 24);

    let first = solve(&tree, tab);
    assert_eq!(first.suppressed, [c]);
    assert!(!first.all_suppressed);
    assert_eq!(
        first.panes,
        [
            (a, rect(0, 0, 4, 24)),
            (b, rect(4, 0, 5, 24)),
            (c, Rect::zero()),
        ]
    );
    assert_tiles_exactly(&first, tab);
    for _ in 0..10 {
        assert_eq!(solve(&tree, tab), first);
    }
}

#[test]
fn a_larger_min_suppresses_a_pane_that_fits_at_the_default_floor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Horizontal, &[a, b]);
    // Twelve columns hold two bordered panes at the 2-column default floor
    // (four each), so nothing suppresses.
    let tab = rect(0, 0, 12, 24);
    let default = solve_with_min(&tree, tab, MIN_PANE_SIZE);
    assert!(default.suppressed.is_empty());
    assert!(default.panes.iter().all(|(_, r)| !r.is_empty()));

    // Raising the content floor to eight columns needs ten per bordered pane —
    // twenty in all — so the same twelve columns now fit only the first, and the
    // second suppresses. This fails if the configured minimum stops reaching the
    // solver.
    let raised = solve_with_min(&tree, tab, Size { cols: 8, rows: 1 });
    assert_eq!(raised.suppressed, [b]);
    assert_eq!(raised.panes, [(a, rect(0, 0, 12, 24)), (b, Rect::zero())]);
}

#[test]
fn regrow_restores_suppressed_panes() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Horizontal, &[a, b, c]);

    let shrunk = solve(&tree, rect(0, 0, 9, 24));
    assert_eq!(shrunk.suppressed, [c]);

    let regrown = solve(&tree, rect(0, 0, 80, 24));
    assert!(regrown.suppressed.is_empty());
    assert!(regrown.panes.iter().all(|(_, r)| !r.is_empty()));
    assert_tiles_exactly(&regrown, rect(0, 0, 80, 24));
}

#[test]
fn a_tab_grown_shrunk_to_nothing_then_regrown_returns_the_exact_first_solve() {
    // Solving is stateless: a full big -> tiny -> big terminal-resize cycle
    // lands back on byte-identical geometry. The tiny step suppresses every
    // pane, yet regrowing to the original rect reproduces the first solve
    // exactly, deltas and remainders included.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    // A resize delta on the outer border makes the shape asymmetric, so the
    // exact-return check is meaningful and not just an even split.
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![leaf(a), LayoutChild::new(inner)],
        weights: vec![
            SizeWeight {
                resize_delta: 7,
                ..SizeWeight::default()
            },
            SizeWeight {
                resize_delta: -7,
                ..SizeWeight::default()
            },
        ],
        active: 0,
    });
    let big = rect(0, 0, 80, 24);

    let first = solve(&tree, big);
    assert_eq!(first.panes[0].1, rect(0, 0, 47, 24));

    // Shrink to a single cell: nothing fits, everything suppresses.
    let tiny = solve(&tree, rect(0, 0, 1, 1));
    assert!(tiny.all_suppressed);
    assert_eq!(tiny.suppressed, [a, b, c]);

    // Grow back to the original rect: identical placement.
    let regrown = solve(&tree, big);
    assert_eq!(regrown, first);
    assert_tiles_exactly(&regrown, big);
}

#[test]
fn all_panes_suppressed_is_flagged_for_the_overlay() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Horizontal, &[a, b]);

    let result = solve(&tree, rect(0, 0, 1, 1));
    assert_eq!(result.suppressed, [a, b]);
    assert!(result.all_suppressed);
    assert!(result.panes.iter().all(|(_, r)| r.is_empty()));
}

#[test]
fn cross_axis_too_small_suppresses_only_the_unfittable_subtree() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    // A pane beside a column of two, in a tab only three rows tall: the column
    // needs six bordered rows and cannot fit, the lone pane (needing three)
    // still can.
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(column)],
    ));
    let tab = rect(0, 0, 80, 3);

    let result = solve(&tree, tab);
    assert_eq!(result.suppressed, [b, c]);
    assert!(!result.all_suppressed);
    assert_eq!(result.panes[0], (a, tab));
    assert_tiles_exactly(&result, tab);
}

#[test]
fn suppression_never_overlaps_or_spills() {
    let panes: Vec<PaneId> = (0..6).map(|_| PaneId::new()).collect();
    let tree = split(SplitDirection::Horizontal, &panes);
    for cols in 1..14 {
        let tab = rect(0, 0, cols, 4);
        let result = solve(&tree, tab);
        assert_no_overlap(&result.panes).unwrap();
        assert_no_outside(&result.panes, tab).unwrap();
        assert_min_size_respected(&result.panes, MIN_PANE_SIZE).unwrap();
    }
}

#[test]
fn stack_gives_the_active_child_everything_above_the_headers() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b], 0));
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    assert_eq!(
        result.panes,
        [(a, rect(0, 0, 80, 23)), (b, rect(0, 23, 80, 1))]
    );
    // Collapsed members occupy a header strip, not suppression.
    assert!(result.suppressed.is_empty());
    assert!(!result.all_suppressed);
    assert_eq!(
        result.stack_headers,
        [StackHeader {
            pane: b,
            rect: rect(0, 23, 80, 1),
            position: 1,
            total: 2,
        }]
    );
    assert_tiles_exactly(&result, tab);
}

#[test]
fn five_member_stack_keeps_headers_in_layout_order_around_the_active_child() {
    let panes: Vec<PaneId> = (0..5).map(|_| PaneId::new()).collect();
    let tree = LayoutNode::Split(SplitNode::stack(panes.clone(), 2));
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    // Members 0 and 1 sit above as single rows, the active member gets
    // rows 2..22, members 3 and 4 sit below.
    assert_eq!(
        result.panes,
        [
            (panes[0], rect(0, 0, 80, 1)),
            (panes[1], rect(0, 1, 80, 1)),
            (panes[2], rect(0, 2, 80, 20)),
            (panes[3], rect(0, 22, 80, 1)),
            (panes[4], rect(0, 23, 80, 1)),
        ]
    );
    let positions: Vec<usize> = result.stack_headers.iter().map(|h| h.position).collect();
    assert_eq!(positions, [0, 1, 3, 4]);
    assert!(result.stack_headers.iter().all(|h| h.total == 5));
    assert_tiles_exactly(&result, tab);
    assert_min_size_respected(&result.panes, MIN_PANE_SIZE).unwrap();
}

#[test]
fn stack_header_metadata_is_stable_across_solves() {
    let panes: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let tree = LayoutNode::Split(SplitNode::stack(panes, 1));
    let tab = rect(0, 0, 80, 24);

    let first = solve(&tree, tab);
    for _ in 0..10 {
        assert_eq!(solve(&tree, tab), first);
    }
}

#[test]
fn stack_beside_a_pane_solves_inside_its_own_slot() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(stack)],
    ));
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    assert_eq!(
        result.panes,
        [
            (a, rect(0, 0, 40, 24)),
            (b, rect(40, 0, 40, 23)),
            (c, rect(40, 23, 40, 1)),
        ]
    );
    assert_eq!(result.stack_headers.len(), 1);
    assert_eq!(result.stack_headers[0].rect, rect(40, 23, 40, 1));
    assert_tiles_exactly(&result, tab);
}

#[test]
fn a_stack_too_small_for_its_active_child_suppresses_as_one_unit() {
    let panes: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let tree = LayoutNode::Split(SplitNode::stack(panes.clone(), 0));
    // Three members need two header rows plus one active row; two rows are
    // not enough, and a partial stack must never render.
    let tab = rect(0, 0, 80, 2);

    let result = solve(&tree, tab);
    assert_eq!(result.suppressed, panes);
    assert!(result.all_suppressed);
    assert!(result.stack_headers.is_empty());
    assert!(result.panes.iter().all(|(_, r)| r.is_empty()));
}

#[test]
fn an_out_of_bounds_active_index_still_suppresses_as_one_unit() {
    let panes: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    // Hand-built: the constructors and edits clamp `active`, but a
    // deserialized stack may carry an out-of-range index. The min-size
    // check must count the clamped active member, not skip it, or a
    // too-short rect renders headers over a zero-area active child.
    let mut stack = SplitNode::stack(panes.clone(), 0);
    stack.active = panes.len() + 4;
    let tree = LayoutNode::Split(stack);
    let tab = rect(0, 0, 80, 2);

    let result = solve(&tree, tab);
    assert_eq!(result.suppressed, panes);
    assert!(result.all_suppressed);
    assert!(result.stack_headers.is_empty());
}

#[test]
fn a_stack_narrower_than_its_members_suppresses_as_one_unit() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b], 0));

    let result = solve(&tree, rect(0, 0, 1, 24));
    assert_eq!(result.suppressed, [a, b]);
    assert!(result.all_suppressed);
    assert!(result.stack_headers.is_empty());
}

#[test]
fn an_active_subtree_splits_the_active_region() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    // Hand-built: a stack whose active member is itself a vertical pair.
    // The edits never create this shape, but the solver must stay sound.
    let pair = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    let stack = SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutChild {
                node: LayoutNode::Pane(a),
                collapsed: true,
            },
            LayoutChild {
                node: pair,
                collapsed: false,
            },
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 1,
    };
    let tab = rect(0, 0, 80, 25);

    let result = solve(&LayoutNode::Split(stack), tab);
    assert_eq!(
        result.panes,
        [
            (a, rect(0, 0, 80, 1)),
            (b, rect(0, 1, 80, 12)),
            (c, rect(0, 13, 80, 12)),
        ]
    );
    assert_tiles_exactly(&result, tab);
}

#[test]
fn border_inclusive_min_adds_one_cell_per_side() {
    let content = Size { cols: 2, rows: 1 };
    assert_eq!(
        border_inclusive_min(content, true),
        Size { cols: 4, rows: 3 }
    );
    assert_eq!(border_inclusive_min(content, false), content);
}

#[test]
fn a_layout_whose_content_mins_fit_but_borders_do_not_suppresses_trailing() {
    // Two panes fit at the bare (2,1) content floor in four columns, but each
    // is drawn inside a one-cell border, so together they truly need eight. At
    // seven the leading pane keeps its border-inclusive slot and the trailing
    // pane is suppressed rather than drawn under an overlapping border.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Horizontal, &[a, b]);
    let tab = rect(0, 0, 7, 3);

    let result = solve(&tree, tab);
    assert_eq!(result.suppressed, [b]);
    assert!(!result.all_suppressed);
    assert_eq!(result.panes, [(a, rect(0, 0, 7, 3)), (b, Rect::zero())]);
}

#[test]
fn every_visible_pane_insets_to_at_least_the_content_floor() {
    // The border-inclusive floor is the load-bearing guarantee: any pane the
    // solver leaves visible must, after the one-cell border inset, still hold
    // the (2,1) content minimum. Sweep tight tabs and check the inner rect of
    // every non-suppressed pane.
    let panes: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let tree = split(SplitDirection::Horizontal, &panes);
    for cols in 1..24 {
        for rows in 1..7 {
            let tab = rect(0, 0, cols, rows);
            let result = solve(&tree, tab);
            for (_, outer) in &result.panes {
                if outer.is_empty() {
                    continue;
                }
                let inner = outer.inner_with_border();
                assert!(
                    inner.size.cols >= 2 && inner.size.rows >= 1,
                    "visible pane {outer:?} insets to {inner:?}, below the content floor",
                );
            }
        }
    }
}

#[test]
fn zero_area_tab_solves_every_pane_to_zero_without_panicking() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let result = solve(&split(SplitDirection::Horizontal, &[a, b]), Rect::zero());
    assert!(result.panes.iter().all(|(_, r)| r.is_empty()));
}

#[test]
fn offset_tab_origin_is_respected() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tab = rect(5, 3, 40, 20);
    let result = solve(&split(SplitDirection::Horizontal, &[a, b]), tab);
    assert_eq!(
        result.panes,
        [(a, rect(5, 3, 20, 20)), (b, rect(25, 3, 20, 20))]
    );
    assert_tiles_exactly(&result, tab);
}

#[test]
fn an_empty_directional_split_solves_to_no_panes_without_panicking() {
    // Hand-built: a split with no children at all, representable directly
    // though the public edits never produce it.
    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    let result = solve(&empty, rect(0, 0, 80, 24));
    assert!(result.panes.is_empty());
    assert!(result.suppressed.is_empty());
    assert!(!result.all_suppressed);
    assert_eq!(min_size(&empty, MIN_PANE_SIZE), Size { cols: 0, rows: 0 });
}

#[test]
fn an_empty_stack_solves_to_no_panes_without_panicking() {
    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Stacked,
        Vec::new(),
    ));
    let result = solve(&empty, rect(0, 0, 80, 24));
    assert!(result.panes.is_empty());
    assert!(result.stack_headers.is_empty());
    assert_eq!(min_size(&empty, MIN_PANE_SIZE), Size { cols: 0, rows: 0 });
}

#[test]
fn single_member_stack_has_no_headers_and_fills_the_rect() {
    let a = PaneId::new();
    let tree = LayoutNode::Split(SplitNode::stack(vec![a], 0));
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    assert_eq!(result.panes, [(a, tab)]);
    assert!(result.stack_headers.is_empty());
    assert!(result.suppressed.is_empty());
}

#[test]
fn leftover_cells_distribute_to_multiple_trailing_flex_children() {
    // Four equal flex shares over 10 cells: 10/4 floors to 2 each with 2
    // left over, and both leftover cells go to the two trailing children.
    let weights = vec![SizeWeight::default(); 4];
    let floors = vec![0u16; 4];
    assert_eq!(distribute(&weights, &floors, 10), [2, 2, 3, 3]);
}

#[test]
fn a_fixed_child_is_raised_to_its_own_border_floor_by_a_flexible_donor() {
    // A Fixed(1) constraint claims only one cell in the primary pass, but
    // every leaf still carries its border-inclusive floor of four; the
    // floor clamp pulls the difference from the flexible sibling.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Fixed(1))),
            (b, SizeWeight::default()),
        ],
    );
    let tab = rect(0, 0, 20, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [4, 16]);
    assert_tiles_exactly(&result, tab);
    assert_min_size_respected(&result.panes, MIN_PANE_SIZE).unwrap();
}

#[test]
fn a_declared_min_overlay_outranks_a_smaller_min_primary() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let overlaid = SizeWeight::new(SizeConstraint::Min(10))
        .with_min(20)
        .unwrap();
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![(a, overlaid), (b, SizeWeight::new(SizeConstraint::Flex(1)))],
    );
    let tab = rect(0, 0, 30, 24);

    // The overlay's 20 wins over the primary's 10, so a holds 20 and b
    // takes the rest.
    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [20, 10]);
}

#[test]
fn fits_accepts_a_zero_rect_for_an_empty_split() {
    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    assert!(fits(&empty, Rect::zero(), MIN_PANE_SIZE));
}

#[test]
fn border_inclusive_min_saturates_at_u16_max() {
    let content = Size {
        cols: u16::MAX,
        rows: u16::MAX,
    };
    assert_eq!(
        border_inclusive_min(content, true),
        Size {
            cols: u16::MAX,
            rows: u16::MAX,
        }
    );
}

#[test]
fn a_single_pane_needs_three_rows_for_its_border() {
    // Wave 2 — starvation geometry. One cell of height is not enough: a bare
    // leaf still reserves a one-cell border on every side, so its floor is
    // (4, 3). One or two rows suppress it; three rows and four columns is the
    // exact smallest tab that keeps it visible.
    let a = PaneId::new();
    let tree = LayoutNode::Pane(a);

    let one_row = solve(&tree, rect(0, 0, 80, 1));
    assert_eq!(one_row.suppressed, [a]);
    assert!(one_row.all_suppressed);
    assert_eq!(one_row.panes, [(a, Rect::zero())]);

    let two_rows = solve(&tree, rect(0, 0, 80, 2));
    assert_eq!(two_rows.suppressed, [a]);
    assert!(two_rows.all_suppressed);

    let three_cols = solve(&tree, rect(0, 0, 3, 24));
    assert_eq!(three_cols.suppressed, [a]);
    assert!(three_cols.all_suppressed);

    // Exactly the floor: four columns by three rows keeps the pane visible.
    let floor = solve(&tree, rect(0, 0, 4, 3));
    assert_eq!(floor.panes, [(a, rect(0, 0, 4, 3))]);
    assert!(floor.suppressed.is_empty());
    assert!(!floor.all_suppressed);
}

#[test]
fn a_two_by_two_cell_tab_suppresses_every_pane() {
    // Wave 2 — starvation geometry. A 2x2 terminal cannot hold even one
    // bordered pane, so a four-pane grid drops entirely.
    let (a, b, c, d) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let left = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(a), leaf(b)],
    ));
    let right = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(c), leaf(d)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![LayoutChild::new(left), LayoutChild::new(right)],
    ));

    let result = solve(&tree, rect(0, 0, 2, 2));
    assert_eq!(result.suppressed, [a, b, c, d]);
    assert!(result.all_suppressed);
    assert!(result.panes.iter().all(|(_, r)| r.is_empty()));
}

#[test]
fn a_two_by_two_grid_tiles_a_normal_tab_exactly() {
    // Wave 2 — the same 2x2 grid at a real size lays four equal quadrants.
    let (a, b, c, d) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let left = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(a), leaf(b)],
    ));
    let right = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(c), leaf(d)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![LayoutChild::new(left), LayoutChild::new(right)],
    ));
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    assert_eq!(
        result.panes,
        [
            (a, rect(0, 0, 40, 12)),
            (b, rect(0, 12, 40, 12)),
            (c, rect(40, 0, 40, 12)),
            (d, rect(40, 12, 40, 12)),
        ]
    );
    assert_tiles_exactly(&result, tab);
}

#[test]
fn far_more_panes_than_fit_suppress_every_trailing_one() {
    // Wave 2 — starvation. A hundred equal columns in eighty cells: each
    // bordered pane needs four columns, so exactly the first twenty are kept
    // (twenty times four is eighty), and the remaining eighty suppress in
    // trailing order.
    let panes: Vec<PaneId> = (0..100).map(|_| PaneId::new()).collect();
    let tree = split(SplitDirection::Horizontal, &panes);
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    assert_eq!(result.suppressed, panes[20..].to_vec());
    assert!(!result.all_suppressed);
    // The kept twenty each take exactly their four-column floor, back to back.
    for (index, &pane) in panes.iter().take(20).enumerate() {
        let placed = result.panes.iter().find(|&&(id, _)| id == pane).unwrap().1;
        assert_eq!(placed, rect(index as u16 * 4, 0, 4, 24));
    }
    assert_tiles_exactly(&result, tab);
}

#[test]
fn exactly_enough_columns_for_every_pane_keeps_all_of_them() {
    // Wave 2 — the boundary: twenty bordered panes need exactly eighty
    // columns, and nothing suppresses.
    let panes: Vec<PaneId> = (0..20).map(|_| PaneId::new()).collect();
    let tree = split(SplitDirection::Horizontal, &panes);
    let tab = rect(0, 0, 80, 24);

    let result = solve(&tree, tab);
    assert!(result.suppressed.is_empty());
    assert!(result.panes.iter().all(|(_, r)| r.size.cols == 4));
    assert_tiles_exactly(&result, tab);

    // One column short and the last pane drops.
    let short = solve(&tree, rect(0, 0, 79, 24));
    assert_eq!(short.suppressed, [panes[19]]);
}

#[test]
fn odd_rows_send_the_extra_row_to_the_trailing_pane() {
    // Wave 2 — off-by-one. Twenty-five rows split two ways is 12 then 13.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Vertical, &[a, b]);
    let tab = rect(0, 0, 80, 25);

    let result = solve(&tree, tab);
    assert_eq!(
        result.panes,
        [(a, rect(0, 0, 80, 12)), (b, rect(0, 12, 80, 13))]
    );
    assert_tiles_exactly(&result, tab);
}

#[test]
fn odd_three_way_rows_give_the_remainder_to_the_last_pane() {
    // Wave 2 — off-by-one. Twenty-five rows three ways floors to 8 each with
    // one left over, which goes to the trailing child: 8, 8, 9.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Vertical, &[a, b, c]);
    let tab = rect(0, 0, 80, 25);

    let result = solve(&tree, tab);
    assert_eq!(
        result.panes,
        [
            (a, rect(0, 0, 80, 8)),
            (b, rect(0, 8, 80, 8)),
            (c, rect(0, 16, 80, 9)),
        ]
    );
    assert_tiles_exactly(&result, tab);
}

#[test]
fn nested_same_direction_splits_send_each_levels_remainder_trailing() {
    // Wave 2 — rounding accumulation. An unnormalized h(a, h(b, c)) over an
    // 81-column tab: the outer split rounds to 40 then 41, and the inner
    // split rounds its own 41 to 20 then 21. Each level's leftover cell lands
    // on that level's trailing child, and the whole thing still tiles.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(inner)],
    ));
    let tab = rect(0, 0, 81, 24);

    let result = solve(&tree, tab);
    assert_eq!(
        result.panes,
        [
            (a, rect(0, 0, 40, 24)),
            (b, rect(40, 0, 20, 24)),
            (c, rect(60, 0, 21, 24)),
        ]
    );
    assert_tiles_exactly(&result, tab);
}

#[test]
fn a_fifty_deep_alternating_tree_tiles_exactly_and_solves_deterministically() {
    // Wave 2 — deep nesting. Fifty alternating horizontal/vertical splits
    // nest fifty-one leaves. Over a tab large enough to hold every floor the
    // panes tile exactly, meet the minimum, and two solves agree.
    let panes: Vec<PaneId> = (0..51).map(|_| PaneId::new()).collect();
    let tree = deep_alternating(&panes);
    let tab = rect(0, 0, 1000, 1000);

    assert!(fits(&tree, tab, MIN_PANE_SIZE));
    let result = solve(&tree, tab);
    assert_eq!(result.panes.len(), 51);
    assert_eq!(
        result.panes.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
        panes
    );
    assert!(result.suppressed.is_empty());
    assert_tiles_exactly(&result, tab);
    assert_min_size_respected(&result.panes, MIN_PANE_SIZE).unwrap();
    assert_eq!(solve(&tree, tab), result);
}

#[test]
fn two_full_percent_children_over_the_total_share_donate_at_the_floor() {
    // Both children claim 100% of the axis; the second gets nothing from
    // the percent pass, then the floor clamp pulls its four cells back
    // from the first, which is not flexible but is still tapped once the
    // flexible-only donor pool comes up empty.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![
            (a, SizeWeight::new(SizeConstraint::Percent(100))),
            (b, SizeWeight::new(SizeConstraint::Percent(100))),
        ],
    );
    let tab = rect(0, 0, 100, 24);

    let result = solve(&tree, tab);
    let widths: Vec<u16> = result.panes.iter().map(|(_, r)| r.size.cols).collect();
    assert_eq!(widths, [96, 4]);
    assert_tiles_exactly(&result, tab);
    assert_min_size_respected(&result.panes, MIN_PANE_SIZE).unwrap();
}
