//! Unit tests for the text cursor and the DECSC/DECRC saved-cursor snapshot.

use super::*;

/// A cursor at a known position with no saved snapshot, used as the base for
/// the equality tests.
fn build_cursor_at(row_index: u16, column_index: u16) -> Cursor {
    Cursor {
        row: row_index,
        column: column_index,
        is_visible: true,
        is_wrap_pending: false,
        is_origin_mode_enabled: false,
        saved: None,
    }
}

#[test]
fn cursor_position_and_flags_match_assigned_values() {
    let cursor = build_cursor_at(4, 7);
    assert_eq!(cursor.row, 4);
    assert_eq!(cursor.column, 7);
    assert!(cursor.is_visible);
    assert!(!cursor.is_wrap_pending);
    assert_eq!(cursor.saved, None);
}

#[test]
fn two_cursors_differing_only_by_the_deferred_wrap_latch_are_not_equal() {
    let cursor_at_wrap_bound = build_cursor_at(2, 3);
    let mut cursor_with_wrap_pending = cursor_at_wrap_bound;
    cursor_with_wrap_pending.is_wrap_pending = true;
    assert_ne!(cursor_at_wrap_bound, cursor_with_wrap_pending);
}

#[test]
fn two_cursors_differing_only_by_visibility_are_not_equal() {
    let visible_cursor = build_cursor_at(1, 1);
    let mut hidden_cursor = visible_cursor;
    hidden_cursor.is_visible = false;
    assert_ne!(visible_cursor, hidden_cursor);
}

#[test]
fn copying_a_cursor_leaves_the_original_untouched() {
    let original_cursor = build_cursor_at(5, 6);
    let mut copied_cursor = original_cursor;
    copied_cursor.row = 9;
    assert_eq!(original_cursor.row, 5);
    assert_eq!(copied_cursor.row, 9);
}

#[test]
fn saved_cursor_carries_position_wrap_latch_and_render_snapshot() {
    let saved_cursor = SavedCursor {
        row: 3,
        column: 8,
        is_wrap_pending: true,
        is_origin_mode_enabled: false,
        render: RenderState::new(),
    };
    assert_eq!(saved_cursor.row, 3);
    assert_eq!(saved_cursor.column, 8);
    assert!(saved_cursor.is_wrap_pending);
    assert_eq!(saved_cursor.render, RenderState::new());
}

#[test]
fn saved_cursors_differing_only_by_their_render_snapshot_are_not_equal() {
    let saved_cursor_with_fresh_render = SavedCursor {
        row: 0,
        column: 0,
        is_wrap_pending: false,
        is_origin_mode_enabled: false,
        render: RenderState::new(),
    };
    let mut shifted_render_state = RenderState::new();
    shifted_render_state.gl = 1;
    let saved_cursor_with_shifted_render = SavedCursor {
        render: shifted_render_state,
        ..saved_cursor_with_fresh_render
    };
    assert_ne!(
        saved_cursor_with_fresh_render,
        saved_cursor_with_shifted_render
    );
}

#[test]
fn a_cursor_holding_a_saved_snapshot_differs_from_one_without() {
    let cursor_without_saved_snapshot = build_cursor_at(0, 0);
    let saved_cursor_snapshot = SavedCursor {
        row: 0,
        column: 0,
        is_wrap_pending: false,
        is_origin_mode_enabled: false,
        render: RenderState::new(),
    };
    let cursor_with_saved_snapshot = Cursor {
        saved: Some(saved_cursor_snapshot),
        ..cursor_without_saved_snapshot
    };
    assert_ne!(cursor_without_saved_snapshot, cursor_with_saved_snapshot);
}

#[test]
fn a_cursor_with_a_saved_snapshot_survives_a_serde_round_trip() {
    let mut saved_render_state = RenderState::new();
    saved_render_state.gl = 1;
    let cursor = Cursor {
        row: 3,
        column: 8,
        is_visible: false,
        is_wrap_pending: true,
        is_origin_mode_enabled: true,
        saved: Some(SavedCursor {
            row: 1,
            column: 2,
            is_wrap_pending: true,
            is_origin_mode_enabled: true,
            render: saved_render_state,
        }),
    };

    let cursor_json = serde_json::to_string(&cursor).expect("a cursor serializes");
    let restored_cursor: Cursor =
        serde_json::from_str(&cursor_json).expect("the same JSON deserializes");
    assert_eq!(restored_cursor, cursor);
}
