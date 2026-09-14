//! Tests for the two chrome-row inputs: assembling either one twice borrows
//! the same data both times and copies nothing behind it, and the compiled-in
//! region solve keeps one rectangle per region down to a zero-size viewport.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, SessionId, TabId};
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;

use crate::snapshot::{
    ClientSnapshot, CommittedRegions, HintBinding, PluginUiSnapshot, Reconnecting, RenderSnapshot,
    SessionSnapshot, TabMeta, TabSnapshot, ViewerChrome,
};

/// A frame of one session named `one`, holding one tab named `first` with no
/// panes in it.
fn build_render_snapshot() -> RenderSnapshot {
    let tab_id = TabId::new();

    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: "one".to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "first".to_string(),
                pane_slots: Vec::new(),
                effective_cell_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMeta {
                tab_id,
                tab_name: "first".to_string(),
                tab_index: 0,
                is_active: true,
            }],
        },
        pane_snapshots: Vec::new(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
            active_tab_id: tab_id,
            focused_pane_id: None,
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

/// Hints holding one binding: `<C-l>` labeled `Lock`.
fn build_keymap_hints() -> KeymapHints {
    KeymapHints {
        hint_bindings: Arc::new(vec![HintBinding {
            key_sequence: KeySequence::from_first_and_rest(
                KeyChord::from_parts(ModFlags::CTRL, Key::Char('l')),
                Vec::new(),
            ),
            action_display_name: "Lock".to_string(),
            is_user_authored: false,
            is_pinned: false,
        }]),
        prefix_labels: Arc::new(BTreeMap::new()),
        removed_key_sequences: Arc::new(BTreeSet::new()),
        is_reverted_to_defaults: false,
    }
}

#[test]
fn core_regions_commit_exact_chrome_rectangles_and_revision() {
    let committed_regions = CommittedRegions::core(
        Size {
            column_count: 80,
            row_count: 24,
        },
        7,
    );

    assert_eq!(
        committed_regions.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert_eq!(committed_regions.region_input_revision, 7);
    assert_eq!(
        committed_regions.solved_regions.region_rects,
        vec![
            Rect::from_origin_and_size(
                Point { column: 0, row: 0 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
            Rect::from_origin_and_size(
                Point { column: 0, row: 23 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
        ]
    );
    assert_eq!(
        committed_regions.solved_regions.pane_rect,
        Rect::from_origin_and_size(
            Point { column: 0, row: 1 },
            Size {
                column_count: 80,
                row_count: 22,
            },
        )
    );
}

#[test]
fn solve_core_regions_keeps_a_rectangle_per_region_on_short_viewports() {
    // Two rows: the tab row takes the first, the hint row the second, and no row
    // is left for panes.
    let two_row_regions = solve_core_regions(Size {
        column_count: 80,
        row_count: 2,
    });
    assert_eq!(
        two_row_regions.region_rects,
        vec![
            Rect::from_origin_and_size(
                Point { column: 0, row: 0 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
            Rect::from_origin_and_size(
                Point { column: 0, row: 1 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
        ]
    );
    assert_eq!(
        two_row_regions.pane_rect,
        Rect::from_origin_and_size(
            Point { column: 0, row: 1 },
            Size {
                column_count: 80,
                row_count: 0,
            },
        )
    );

    // One row: the tab row takes it and the hint row keeps a zero-height
    // rectangle at the row after it.
    let one_row_regions = solve_core_regions(Size {
        column_count: 80,
        row_count: 1,
    });
    assert_eq!(
        one_row_regions.region_rects,
        vec![
            Rect::from_origin_and_size(
                Point { column: 0, row: 0 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
            Rect::from_origin_and_size(
                Point { column: 0, row: 1 },
                Size {
                    column_count: 80,
                    row_count: 0,
                },
            ),
        ]
    );
    assert_eq!(
        one_row_regions.pane_rect,
        Rect::from_origin_and_size(
            Point { column: 0, row: 1 },
            Size {
                column_count: 80,
                row_count: 0,
            },
        )
    );

    // A zero-size viewport: both rectangles are empty at the origin.
    let zero_size_regions = solve_core_regions(Size {
        column_count: 0,
        row_count: 0,
    });
    assert_eq!(
        zero_size_regions.region_rects,
        vec![Rect::empty_at_origin(), Rect::empty_at_origin()]
    );
    assert_eq!(zero_size_regions.pane_rect, Rect::empty_at_origin());
}

#[test]
fn assembling_the_keybinding_row_input_twice_shares_every_allocation() {
    let keymap_hints = build_keymap_hints();
    let pending_key_sequence = KeySequence::from_first_and_rest(
        KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')),
        Vec::new(),
    );

    let first_statusline_inputs = StatuslineInputs {
        keymap_hints: &keymap_hints,
        pending_key_sequence: Some(&pending_key_sequence),
    };
    let second_statusline_inputs = StatuslineInputs {
        keymap_hints: &keymap_hints,
        pending_key_sequence: Some(&pending_key_sequence),
    };

    assert!(
        Arc::ptr_eq(
            &first_statusline_inputs.keymap_hints.hint_bindings,
            &second_statusline_inputs.keymap_hints.hint_bindings,
        ),
        "hint_bindings was copied"
    );
    assert!(
        Arc::ptr_eq(
            &first_statusline_inputs.keymap_hints.prefix_labels,
            &second_statusline_inputs.keymap_hints.prefix_labels,
        ),
        "prefix_labels was copied"
    );
    assert!(
        Arc::ptr_eq(
            &first_statusline_inputs.keymap_hints.removed_key_sequences,
            &second_statusline_inputs.keymap_hints.removed_key_sequences,
        ),
        "removed_key_sequences was copied"
    );
    assert!(
        std::ptr::eq(
            first_statusline_inputs.keymap_hints,
            second_statusline_inputs.keymap_hints,
        ),
        "keymap_hints was copied"
    );
    assert!(
        std::ptr::eq(
            first_statusline_inputs.pending_key_sequence.unwrap(),
            second_statusline_inputs.pending_key_sequence.unwrap(),
        ),
        "pending_key_sequence was copied"
    );
}

#[test]
fn assembling_the_tab_row_input_twice_borrows_each_shared_field() {
    let mut render_snapshot = build_render_snapshot();
    render_snapshot.client_snapshot.lock_mode = LockMode::Locked;
    render_snapshot.client_snapshot.is_mouse_selection_enabled = true;
    let viewer_chrome = ViewerChrome {
        reconnecting: Some(Reconnecting {
            attempt: 3,
            retry_in_seconds: 8,
        }),
        tabline_offset: Some(2),
        ..ViewerChrome::default()
    };

    let first_tabline_inputs = TablineInputs {
        session_name: &render_snapshot.session_snapshot.session_name,
        tabs_metadata: &render_snapshot.session_snapshot.tabs_metadata,
        lock_mode: render_snapshot.client_snapshot.lock_mode,
        is_mouse_selection_enabled: render_snapshot.client_snapshot.is_mouse_selection_enabled,
        reconnecting: viewer_chrome.reconnecting,
        tabline_offset: viewer_chrome.tabline_offset,
    };
    let second_tabline_inputs = TablineInputs {
        session_name: &render_snapshot.session_snapshot.session_name,
        tabs_metadata: &render_snapshot.session_snapshot.tabs_metadata,
        lock_mode: render_snapshot.client_snapshot.lock_mode,
        is_mouse_selection_enabled: render_snapshot.client_snapshot.is_mouse_selection_enabled,
        reconnecting: viewer_chrome.reconnecting,
        tabline_offset: viewer_chrome.tabline_offset,
    };

    assert_eq!(first_tabline_inputs.session_name, "one");
    assert_eq!(first_tabline_inputs.tabs_metadata[0].tab_name, "first");
    assert_eq!(first_tabline_inputs.lock_mode, LockMode::Locked);
    assert!(first_tabline_inputs.is_mouse_selection_enabled);
    assert_eq!(
        first_tabline_inputs.reconnecting,
        Some(Reconnecting {
            attempt: 3,
            retry_in_seconds: 8,
        })
    );
    assert_eq!(first_tabline_inputs.tabline_offset, Some(2));
    assert!(
        std::ptr::eq(
            first_tabline_inputs.session_name,
            second_tabline_inputs.session_name,
        ),
        "session name was copied"
    );
    assert!(
        std::ptr::eq(
            first_tabline_inputs.tabs_metadata,
            second_tabline_inputs.tabs_metadata,
        ),
        "tabs_metadata were copied"
    );
    assert_eq!(
        first_tabline_inputs.lock_mode,
        second_tabline_inputs.lock_mode
    );
    assert_eq!(
        first_tabline_inputs.is_mouse_selection_enabled,
        second_tabline_inputs.is_mouse_selection_enabled
    );
    assert_eq!(
        first_tabline_inputs.reconnecting,
        second_tabline_inputs.reconnecting
    );
    assert_eq!(
        first_tabline_inputs.tabline_offset,
        second_tabline_inputs.tabline_offset
    );

    // The frame yields the same value, field for field.
    let frame_layout = render_snapshot.build_frame_layout(viewer_chrome);
    assert_eq!(first_tabline_inputs, frame_layout.get_tabline_inputs());
}
