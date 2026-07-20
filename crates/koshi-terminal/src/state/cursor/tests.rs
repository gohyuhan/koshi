//! Unit tests for the text cursor and the DECSC/DECRC saved-cursor snapshot.

use super::*;

/// A cursor at a known position with no saved snapshot, used as the base for
/// the equality tests.
fn cursor_at(row: u16, col: u16) -> Cursor {
    Cursor {
        row,
        col,
        is_visible: true,
        pending_wrap: false,
        saved: None,
    }
}

#[test]
fn cursor_fields_read_back_as_written() {
    let cursor = cursor_at(4, 7);
    assert_eq!(cursor.row, 4);
    assert_eq!(cursor.col, 7);
    assert!(cursor.is_visible);
    assert!(!cursor.pending_wrap);
    assert_eq!(cursor.saved, None);
}

#[test]
fn two_cursors_differing_only_by_the_deferred_wrap_latch_are_not_equal() {
    let parked = cursor_at(2, 3);
    let mut latched = parked;
    latched.pending_wrap = true;
    assert_ne!(parked, latched);
}

#[test]
fn two_cursors_differing_only_by_visibility_are_not_equal() {
    let shown = cursor_at(1, 1);
    let mut hidden = shown;
    hidden.is_visible = false;
    assert_ne!(shown, hidden);
}

#[test]
fn copying_a_cursor_leaves_the_original_untouched() {
    let original = cursor_at(5, 6);
    let mut copy = original;
    copy.row = 9;
    assert_eq!(original.row, 5);
    assert_eq!(copy.row, 9);
}

#[test]
fn saved_cursor_carries_position_wrap_latch_and_render_snapshot() {
    let saved = SavedCursor {
        row: 3,
        col: 8,
        pending_wrap: true,
        render: RenderState::fresh(),
    };
    assert_eq!(saved.row, 3);
    assert_eq!(saved.col, 8);
    assert!(saved.pending_wrap);
    assert_eq!(saved.render, RenderState::fresh());
}

#[test]
fn saved_cursors_differing_only_by_their_render_snapshot_are_not_equal() {
    let with_fresh = SavedCursor {
        row: 0,
        col: 0,
        pending_wrap: false,
        render: RenderState::fresh(),
    };
    let mut other_render = RenderState::fresh();
    other_render.gl = 1;
    let with_shifted = SavedCursor {
        render: other_render,
        ..with_fresh
    };
    assert_ne!(with_fresh, with_shifted);
}

#[test]
fn a_cursor_holding_a_saved_snapshot_differs_from_one_without() {
    let bare = cursor_at(0, 0);
    let snapshot = SavedCursor {
        row: 0,
        col: 0,
        pending_wrap: false,
        render: RenderState::fresh(),
    };
    let with_saved = Cursor {
        saved: Some(snapshot),
        ..bare
    };
    assert_ne!(bare, with_saved);
}
