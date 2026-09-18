//! Tests for span overlap and directional neighbour selection.
//!
//! The fixture is the four-pane screen `A | B B` above `A | C D` over 120 by
//! 40 cells: A `(0, 0, 40, 40)`, B `(40, 0, 80, 20)`, C `(40, 20, 40, 20)`,
//! D `(80, 20, 40, 20)`.

use koshi_core::geometry::{Point, Size, SplitDirection};

use super::*;
use crate::size::{SizeConstraint, SizeWeight};
use crate::solver::solve_layout;
use crate::tree::{LayoutNode, SplitNode};

fn build_cell_rect(column: u16, row: u16, column_count: u16, row_count: u16) -> Rect {
    Rect::from_origin_and_size(
        Point { column, row },
        Size {
            column_count,
            row_count,
        },
    )
}

/// Four pane ids in ascending order: index 0 is the smallest id.
fn build_sorted_pane_ids() -> [PaneId; 4] {
    let mut pane_ids = [PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new()];
    pane_ids.sort_unstable();
    pane_ids
}

/// The fixture rects for A, B, C, D.
fn build_fixture_rects(pane_ids: [PaneId; 4]) -> [(PaneId, Rect); 4] {
    [
        (pane_ids[0], build_cell_rect(0, 0, 40, 40)),
        (pane_ids[1], build_cell_rect(40, 0, 80, 20)),
        (pane_ids[2], build_cell_rect(40, 20, 40, 20)),
        (pane_ids[3], build_cell_rect(80, 20, 40, 20)),
    ]
}

/// Every fixture rect except `source_pane_id`'s, plus the source rect.
fn split_source_from_candidates(
    fixture_rects: &[(PaneId, Rect); 4],
    source_pane_id: PaneId,
) -> (Rect, Vec<(PaneId, Rect)>) {
    let source_rect = fixture_rects
        .iter()
        .find(|(pane_id, _)| *pane_id == source_pane_id)
        .map(|(_, rect)| *rect)
        .expect("source is in the fixture");
    let candidate_pane_rects = fixture_rects
        .iter()
        .copied()
        .filter(|(pane_id, _)| *pane_id != source_pane_id)
        .collect();
    (source_rect, candidate_pane_rects)
}

fn build_flex_weight(share: u32) -> SizeWeight {
    SizeWeight::from_primary_constraint(SizeConstraint::Flex(share))
}

#[test]
fn span_overlap_measures_only_the_length_two_spans_share() {
    // `[0, 10)` and `[4, 10)` share `[4, 10)`: six cells.
    assert_eq!(compute_span_overlap(0, 10, 4, 6), 6);
    // Full containment answers the inner span's whole length, either way round.
    assert_eq!(compute_span_overlap(0, 10, 2, 3), 3);
    assert_eq!(compute_span_overlap(2, 3, 0, 10), 3);
    // Identical spans overlap along their whole length.
    assert_eq!(compute_span_overlap(5, 4, 5, 4), 4);
    // `[0, 5)` and `[5, 10)` touch end to end and share no cell.
    assert_eq!(compute_span_overlap(0, 5, 5, 5), 0);
    // Disjoint spans share nothing, in either order.
    assert_eq!(compute_span_overlap(0, 2, 7, 3), 0);
    assert_eq!(compute_span_overlap(7, 3, 0, 2), 0);
    // A zero-length span shares nothing, even inside the other span.
    assert_eq!(compute_span_overlap(0, 10, 5, 0), 0);
}

#[test]
fn up_from_d_selects_the_wide_pane_above_it() {
    let pane_ids = build_sorted_pane_ids();
    let fixture_rects = build_fixture_rects(pane_ids);
    let (source_rect, candidate_pane_rects) =
        split_source_from_candidates(&fixture_rects, pane_ids[3]);

    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Up),
        Some(pane_ids[1])
    );
}

#[test]
fn every_direction_from_every_fixture_pane_matches_the_screen() {
    let [pane_a, pane_b, pane_c, pane_d] = build_sorted_pane_ids();
    let fixture_rects = build_fixture_rects([pane_a, pane_b, pane_c, pane_d]);
    let expected_neighbors = [
        (pane_a, Direction::Left, None),
        (pane_a, Direction::Right, Some(pane_b)),
        (pane_a, Direction::Up, None),
        (pane_a, Direction::Down, None),
        (pane_b, Direction::Left, Some(pane_a)),
        (pane_b, Direction::Right, None),
        (pane_b, Direction::Up, None),
        (pane_b, Direction::Down, Some(pane_c)),
        (pane_c, Direction::Left, Some(pane_a)),
        (pane_c, Direction::Right, Some(pane_d)),
        (pane_c, Direction::Up, Some(pane_b)),
        (pane_c, Direction::Down, None),
        (pane_d, Direction::Left, Some(pane_c)),
        (pane_d, Direction::Right, None),
        (pane_d, Direction::Up, Some(pane_b)),
        (pane_d, Direction::Down, None),
    ];
    for (source_pane_id, direction, expected_neighbor) in expected_neighbors {
        let (source_rect, candidate_pane_rects) =
            split_source_from_candidates(&fixture_rects, source_pane_id);
        assert_eq!(
            select_directional_neighbor(source_rect, &candidate_pane_rects, direction),
            expected_neighbor,
            "{direction:?} from {source_pane_id}"
        );
    }
}

#[test]
fn down_from_b_prefers_the_larger_overlap_when_edges_tie() {
    // B spans columns 40..120; C covers 40 of those columns and D covers 40.
    // With C widened to 60 columns and D to 20, C's overlap is larger.
    let [pane_a, _, pane_c, pane_d] = build_sorted_pane_ids();
    let source_rect = build_cell_rect(40, 0, 80, 20);
    let candidate_pane_rects = [
        (pane_a, build_cell_rect(0, 0, 40, 40)),
        (pane_c, build_cell_rect(40, 20, 60, 20)),
        (pane_d, build_cell_rect(100, 20, 20, 20)),
    ];

    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Down),
        Some(pane_c)
    );
}

#[test]
fn equal_distance_and_overlap_pick_the_smaller_perpendicular_origin() {
    // Source (0, 0, 10, 10); X at rows 0..5 and Y at rows 5..10, both one
    // column to the right with a five-row overlap. X's row 0 wins over Y's
    // row 5, whichever id is smaller.
    let [smaller_id, larger_id, _, _] = build_sorted_pane_ids();
    let source_rect = build_cell_rect(0, 0, 10, 10);
    let upper_rect = build_cell_rect(10, 0, 5, 5);
    let lower_rect = build_cell_rect(10, 5, 5, 5);

    assert_eq!(
        select_directional_neighbor(
            source_rect,
            &[(smaller_id, upper_rect), (larger_id, lower_rect)],
            Direction::Right
        ),
        Some(smaller_id)
    );
    assert_eq!(
        select_directional_neighbor(
            source_rect,
            &[(larger_id, upper_rect), (smaller_id, lower_rect)],
            Direction::Right
        ),
        Some(larger_id)
    );
}

#[test]
fn identical_candidate_rects_pick_the_smaller_pane_id() {
    let [smaller_id, larger_id, _, _] = build_sorted_pane_ids();
    let source_rect = build_cell_rect(0, 0, 10, 10);
    let candidate_rect = build_cell_rect(10, 0, 10, 10);

    assert_eq!(
        select_directional_neighbor(
            source_rect,
            &[(larger_id, candidate_rect), (smaller_id, candidate_rect)],
            Direction::Right
        ),
        Some(smaller_id)
    );
}

#[test]
fn a_farther_pane_loses_to_a_nearer_one_across_a_gap() {
    let [near_id, far_id, _, _] = build_sorted_pane_ids();
    let source_rect = build_cell_rect(0, 0, 10, 10);
    let candidate_pane_rects = [
        (far_id, build_cell_rect(30, 0, 10, 10)),
        (near_id, build_cell_rect(12, 0, 10, 10)),
    ];

    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Right),
        Some(near_id)
    );
}

#[test]
fn a_diagonal_pane_with_no_shared_span_is_not_a_neighbor() {
    let [diagonal_id, _, _, _] = build_sorted_pane_ids();
    let source_rect = build_cell_rect(0, 0, 10, 10);
    let candidate_pane_rects = [(diagonal_id, build_cell_rect(10, 10, 10, 10))];

    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Right),
        None
    );
    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Down),
        None
    );
}

#[test]
fn an_overlapping_pane_on_the_near_side_of_the_edge_is_not_a_neighbor() {
    // The candidate starts at column 5, inside the source's 0..10 span.
    let [overlapping_id, _, _, _] = build_sorted_pane_ids();
    let source_rect = build_cell_rect(0, 0, 10, 10);
    let candidate_pane_rects = [(overlapping_id, build_cell_rect(5, 0, 10, 10))];

    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Right),
        None
    );
}

#[test]
fn an_empty_candidate_rect_never_qualifies() {
    let [empty_id, real_id, _, _] = build_sorted_pane_ids();
    let source_rect = build_cell_rect(0, 0, 10, 10);
    let candidate_pane_rects = [
        (empty_id, build_cell_rect(10, 0, 0, 10)),
        (real_id, build_cell_rect(10, 0, 10, 10)),
    ];

    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Right),
        Some(real_id)
    );
    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects[..1], Direction::Right),
        None
    );
}

#[test]
fn no_candidates_gives_no_neighbor() {
    let source_rect = build_cell_rect(0, 0, 10, 10);
    assert_eq!(
        select_directional_neighbor(source_rect, &[], Direction::Left),
        None
    );
}

/// Three tree encodings of the fixture screen. Each solves to the same four
/// rects over `(0, 0, 120, 40)`.
fn build_fixture_encodings(pane_ids: [PaneId; 4]) -> Vec<LayoutNode> {
    let [pane_a, pane_b, pane_c, pane_d] = pane_ids;
    let right_column = |bottom_row: LayoutNode| {
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            vec![LayoutNode::Pane(pane_b), bottom_row],
        ))
    };
    let canonical = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            LayoutNode::Pane(pane_a),
            right_column(LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Horizontal,
                vec![LayoutNode::Pane(pane_c), LayoutNode::Pane(pane_d)],
            ))),
        ],
        weights: vec![build_flex_weight(1), build_flex_weight(2)],
        active_child_index: 0,
    });
    // C wrapped in a single-child split.
    let wrapped_c = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            LayoutNode::Pane(pane_a),
            right_column(LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Horizontal,
                vec![
                    LayoutNode::Split(SplitNode::with_equal_weights(
                        SplitDirection::Horizontal,
                        vec![LayoutNode::Pane(pane_c)],
                    )),
                    LayoutNode::Pane(pane_d),
                ],
            ))),
        ],
        weights: vec![build_flex_weight(1), build_flex_weight(2)],
        active_child_index: 0,
    });
    // The same shape with weights 20 and 40 in place of 1 and 2.
    let rescaled = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            LayoutNode::Pane(pane_a),
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![
                    LayoutNode::Pane(pane_b),
                    LayoutNode::Split(SplitNode::with_equal_weights(
                        SplitDirection::Horizontal,
                        vec![LayoutNode::Pane(pane_c), LayoutNode::Pane(pane_d)],
                    )),
                ],
            )),
        ],
        weights: vec![build_flex_weight(20), build_flex_weight(40)],
        active_child_index: 0,
    });
    vec![canonical, wrapped_c, rescaled]
}

#[test]
fn equivalent_tree_encodings_select_the_same_neighbor() {
    let pane_ids = build_sorted_pane_ids();
    let [_, pane_b, pane_c, pane_d] = pane_ids;
    let tab_rect = build_cell_rect(0, 0, 120, 40);
    let expected_rects: Vec<(PaneId, Rect)> = build_fixture_rects(pane_ids).to_vec();

    for layout_tree in build_fixture_encodings(pane_ids) {
        let solved = solve_layout(&layout_tree, tab_rect);
        let mut solved_rects = solved.pane_rects.clone();
        solved_rects.sort_by_key(|(pane_id, _)| *pane_id);
        assert_eq!(solved_rects, expected_rects, "encoding {layout_tree:?}");

        let source_rect = solved_rects[3].1;
        let candidate_pane_rects: Vec<(PaneId, Rect)> = solved
            .pane_rects
            .iter()
            .copied()
            .filter(|(pane_id, _)| *pane_id != pane_d)
            .collect();
        assert_eq!(
            select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Up),
            Some(pane_b)
        );
        assert_eq!(
            select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Left),
            Some(pane_c)
        );
    }
}

#[test]
fn edge_sums_past_u16_max_saturate_and_still_rank() {
    // Both spans start at 65530 and would end at 65540; the ends saturate at
    // 65535, leaving five shared cells.
    assert_eq!(compute_span_overlap(65530, 10, 65530, 10), 5);

    let [near_pane_id, _, _, _] = build_sorted_pane_ids();
    let source_rect = build_cell_rect(65520, 0, 10, 10);
    let candidate_pane_rects = [(near_pane_id, build_cell_rect(65530, 0, 10, 10))];
    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Right),
        Some(near_pane_id)
    );
    assert_eq!(
        select_directional_neighbor(source_rect, &candidate_pane_rects, Direction::Left),
        None
    );
}
