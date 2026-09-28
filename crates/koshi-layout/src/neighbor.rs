//! Directional neighbour selection over solved pane rectangles.
//!
//! Given the pane rectangle a move starts from and the pane rectangles it may land on,
//! this module picks the one the user sees in a cardinal direction. It reads
//! screen geometry only: the layout tree, its nesting, and the order its
//! leaves are listed in play no part.

use std::cmp::Reverse;

use koshi_core::geometry::{Direction, Rect};
use koshi_core::ids::PaneId;

/// The overlap length of the spans `[first_span_start, first_span_start +
/// first_span_length)` and `[second_span_start, second_span_start +
/// second_span_length)`, `0` when they are disjoint. A span end past
/// `u16::MAX` saturates.
///
/// Spans `[0, 10)` and `[4, 10)` overlap by `6`; `[0, 5)` and `[5, 10)`
/// overlap by `0`.
fn compute_span_overlap(
    first_span_start: u16,
    first_span_length: u16,
    second_span_start: u16,
    second_span_length: u16,
) -> u16 {
    let overlap_start = first_span_start.max(second_span_start);
    let first_span_end = first_span_start.saturating_add(first_span_length);
    let second_span_end = second_span_start.saturating_add(second_span_length);
    first_span_end
        .min(second_span_end)
        .saturating_sub(overlap_start)
}

/// The pane in `direction` from `source_pane_rect`, chosen among
/// `candidate_pane_rects`.
///
/// A candidate qualifies when its whole rectangle lies beyond `source_pane_rect`'s
/// edge in `direction` and the two rectangles overlap on the perpendicular
/// axis by at least one cell. An empty rectangle never qualifies. The caller
/// leaves the source pane itself out of `candidate_pane_rects`.
///
/// Among qualifying candidates the smallest facing-edge distance wins, then
/// the largest perpendicular overlap, then the smallest origin on the
/// perpendicular axis (row for `Left`/`Right`, column for `Up`/`Down`), then
/// the smallest row, the smallest column, and the smallest pane id.
///
/// Returns `None` when no candidate qualifies. There is no wrap-around: `Up`
/// from a pane on the top edge is `None`.
///
/// Source `(0, 0, 10, 10)` with candidates `X (10, 0, 5, 5)` and `Y (10, 5,
/// 5, 5)`: `Right` picks `X`, whose origin row `0` is smaller than `Y`'s `5`.
#[must_use]
pub fn select_directional_neighbor(
    source_pane_rect: Rect,
    candidate_pane_rects: &[(PaneId, Rect)],
    direction: Direction,
) -> Option<PaneId> {
    candidate_pane_rects
        .iter()
        .filter(|(_, candidate_pane_rect)| !candidate_pane_rect.is_empty())
        .filter_map(|&(pane_id, candidate_pane_rect)| {
            let edge_distance =
                compute_facing_edge_distance(source_pane_rect, candidate_pane_rect, direction)?;
            let perpendicular_overlap =
                compute_perpendicular_overlap(source_pane_rect, candidate_pane_rect, direction);
            if perpendicular_overlap == 0 {
                return None;
            }
            let perpendicular_origin = match direction {
                Direction::Left | Direction::Right => candidate_pane_rect.origin.row,
                Direction::Up | Direction::Down => candidate_pane_rect.origin.column,
            };
            Some((
                (
                    edge_distance,
                    Reverse(perpendicular_overlap),
                    perpendicular_origin,
                    candidate_pane_rect.origin.row,
                    candidate_pane_rect.origin.column,
                    pane_id,
                ),
                pane_id,
            ))
        })
        .min_by_key(|(candidate_rank, _)| *candidate_rank)
        .map(|(_, pane_id)| pane_id)
}

/// The exclusive right edge of `pane_rect`: `origin.column + column_count`,
/// saturating at `u16::MAX`. `(40, 0, 80, 20)` gives `120`.
pub(crate) fn compute_right_edge(pane_rect: Rect) -> u16 {
    pane_rect
        .origin
        .column
        .saturating_add(pane_rect.size.column_count)
}

/// The exclusive bottom edge of `pane_rect`: `origin.row + row_count`, saturating
/// at `u16::MAX`. `(40, 0, 80, 20)` gives `20`.
pub(crate) fn compute_bottom_edge(pane_rect: Rect) -> u16 {
    pane_rect
        .origin
        .row
        .saturating_add(pane_rect.size.row_count)
}

/// The cells between `source_pane_rect`'s edge in `direction` and
/// `candidate_pane_rect`'s facing edge, or `None` when `candidate_pane_rect` is not
/// wholly beyond that edge.
///
/// Source pane `(0, 0, 10, 10)` and candidate pane `(12, 0, 5, 10)`: `Right` gives
/// `Some(2)`, `Left` gives `None`.
fn compute_facing_edge_distance(
    source_pane_rect: Rect,
    candidate_pane_rect: Rect,
    direction: Direction,
) -> Option<u16> {
    let source_right_edge = compute_right_edge(source_pane_rect);
    let source_bottom_edge = compute_bottom_edge(source_pane_rect);
    let candidate_right_edge = compute_right_edge(candidate_pane_rect);
    let candidate_bottom_edge = compute_bottom_edge(candidate_pane_rect);
    match direction {
        Direction::Left => (candidate_right_edge <= source_pane_rect.origin.column)
            .then(|| source_pane_rect.origin.column - candidate_right_edge),
        Direction::Right => (candidate_pane_rect.origin.column >= source_right_edge)
            .then(|| candidate_pane_rect.origin.column - source_right_edge),
        Direction::Up => (candidate_bottom_edge <= source_pane_rect.origin.row)
            .then(|| source_pane_rect.origin.row - candidate_bottom_edge),
        Direction::Down => (candidate_pane_rect.origin.row >= source_bottom_edge)
            .then(|| candidate_pane_rect.origin.row - source_bottom_edge),
    }
}

/// The cells `source_pane_rect` and `candidate_pane_rect` share on the axis
/// perpendicular to `direction`: rows for `Left`/`Right`, columns for
/// `Up`/`Down`.
fn compute_perpendicular_overlap(
    source_pane_rect: Rect,
    candidate_pane_rect: Rect,
    direction: Direction,
) -> u16 {
    match direction {
        Direction::Left | Direction::Right => compute_span_overlap(
            source_pane_rect.origin.row,
            source_pane_rect.size.row_count,
            candidate_pane_rect.origin.row,
            candidate_pane_rect.size.row_count,
        ),
        Direction::Up | Direction::Down => compute_span_overlap(
            source_pane_rect.origin.column,
            source_pane_rect.size.column_count,
            candidate_pane_rect.origin.column,
            candidate_pane_rect.size.column_count,
        ),
    }
}

#[cfg(test)]
mod tests;
