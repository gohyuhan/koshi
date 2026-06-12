use tile_core::geometry::{Point, SplitDirection};
use tile_test_support::layout_assert::{
    assert_all_space_occupied, assert_min_size_respected, assert_no_outside, assert_no_overlap,
};

use super::*;
use crate::tree::LayoutChild;

fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x, y }, Size { cols, rows })
}

fn leaf(pane: PaneId) -> LayoutChild {
    LayoutChild::new(LayoutNode::Pane(pane))
}

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
    assert_eq!(widths, [20, 8, 2]);
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
fn fits_accepts_a_layout_with_room_for_every_floor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = split(SplitDirection::Horizontal, &[a, b]);
    assert!(fits(&tree, rect(0, 0, 4, 1), MIN_PANE_SIZE));
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

    // The column needs three rows; the leaf beside it needs two columns.
    assert!(fits(&tree, rect(0, 0, 4, 3), MIN_PANE_SIZE));
    assert!(!fits(&tree, rect(0, 0, 4, 2), MIN_PANE_SIZE));
    assert!(!fits(&tree, rect(0, 0, 3, 3), MIN_PANE_SIZE));
}

#[test]
fn fits_uses_declared_floors_not_just_defaults() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let wide = SizeWeight::default().with_min(30).unwrap();
    let tree = split_with(
        SplitDirection::Horizontal,
        vec![(a, wide), (b, SizeWeight::default())],
    );
    assert!(fits(&tree, rect(0, 0, 32, 24), MIN_PANE_SIZE));
    assert!(!fits(&tree, rect(0, 0, 31, 24), MIN_PANE_SIZE));
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
