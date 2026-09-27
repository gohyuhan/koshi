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
fn cursor_fields_read_back_as_written() {
    let cursor = build_cursor_at(4, 7);
    assert_eq!(cursor.row, 4);
    assert_eq!(cursor.column, 7);
    assert!(cursor.is_visible);
    assert!(!cursor.is_wrap_pending);
    assert_eq!(cursor.saved, None);
}

#[test]
fn two_cursors_differing_only_by_the_deferred_wrap_latch_are_not_equal() {
    let parked = build_cursor_at(2, 3);
    let mut latched = parked;
    latched.is_wrap_pending = true;
    assert_ne!(parked, latched);
}

#[test]
fn two_cursors_differing_only_by_visibility_are_not_equal() {
    let shown = build_cursor_at(1, 1);
    let mut hidden = shown;
    hidden.is_visible = false;
    assert_ne!(shown, hidden);
}

#[test]
fn copying_a_cursor_leaves_the_original_untouched() {
    let original = build_cursor_at(5, 6);
    let mut copy = original;
    copy.row = 9;
    assert_eq!(original.row, 5);
    assert_eq!(copy.row, 9);
}

#[test]
fn saved_cursor_carries_position_wrap_latch_and_render_snapshot() {
    let saved = SavedCursor {
        row: 3,
        column: 8,
        is_wrap_pending: true,
        is_origin_mode_enabled: false,
        render: RenderState::new(),
    };
    assert_eq!(saved.row, 3);
    assert_eq!(saved.column, 8);
    assert!(saved.is_wrap_pending);
    assert_eq!(saved.render, RenderState::new());
}

#[test]
fn saved_cursors_differing_only_by_their_render_snapshot_are_not_equal() {
    let with_fresh = SavedCursor {
        row: 0,
        column: 0,
        is_wrap_pending: false,
        is_origin_mode_enabled: false,
        render: RenderState::new(),
    };
    let mut other_render = RenderState::new();
    other_render.gl = 1;
    let with_shifted = SavedCursor {
        render: other_render,
        ..with_fresh
    };
    assert_ne!(with_fresh, with_shifted);
}

#[test]
fn a_cursor_holding_a_saved_snapshot_differs_from_one_without() {
    let bare = build_cursor_at(0, 0);
    let snapshot = SavedCursor {
        row: 0,
        column: 0,
        is_wrap_pending: false,
        is_origin_mode_enabled: false,
        render: RenderState::new(),
    };
    let with_saved = Cursor {
        saved: Some(snapshot),
        ..bare
    };
    assert_ne!(bare, with_saved);
}

#[test]
fn a_cursor_with_a_saved_snapshot_survives_a_serde_round_trip() {
    let mut other_render = RenderState::new();
    other_render.gl = 1;
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
            render: other_render,
        }),
    };

    let json = serde_json::to_string(&cursor).expect("a cursor serializes");
    let restored: Cursor = serde_json::from_str(&json).expect("the same JSON deserializes");
    assert_eq!(restored, cursor);
}
