//! Tests for the two chrome-row inputs: assembling either one twice borrows
//! the same data both times, and copies nothing behind it.

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
fn snapshot() -> RenderSnapshot {
    let tab_id = TabId::new();

    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: "one".to_string(),
            active_tab: TabSnapshot {
                id: tab_id,
                name: "first".to_string(),
                layout_solved: Vec::new(),
                effective_size: Size { cols: 80, rows: 24 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
                gap: 0,
            },
            tabs_metadata: vec![TabMeta {
                id: tab_id,
                name: "first".to_string(),
                index: 0,
                active: true,
            }],
        },
        panes: Vec::new(),
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport: Size { cols: 80, rows: 24 },
            active_tab: tab_id,
            focused_pane: None,
            lock_mode: LockMode::Normal,
            mouse_select: false,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

/// Hints holding one binding: `<C-l>` labeled `Lock`.
fn hints() -> KeymapHints {
    KeymapHints {
        entries: Arc::new(vec![HintBinding {
            sequence: KeySequence::new(KeyChord::new(ModFlags::CTRL, Key::Char('l')), Vec::new()),
            label: "Lock".to_string(),
            user_set: false,
            pinned: false,
        }]),
        prefix_labels: Arc::new(BTreeMap::new()),
        removed: Arc::new(BTreeSet::new()),
        reverted: false,
    }
}

#[test]
fn core_regions_commit_exact_chrome_rectangles_and_revision() {
    let committed = CommittedRegions::core(Size { cols: 80, rows: 24 }, 7);

    assert_eq!(committed.viewport, Size { cols: 80, rows: 24 });
    assert_eq!(committed.input_revision, 7);
    assert_eq!(
        committed.solve.regions,
        vec![
            Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 1 }),
            Rect::new(Point { x: 0, y: 23 }, Size { cols: 80, rows: 1 }),
        ]
    );
    assert_eq!(
        committed.solve.pane_rect,
        Rect::new(Point { x: 0, y: 1 }, Size { cols: 80, rows: 22 })
    );
}

#[test]
fn assembling_the_keybinding_row_input_twice_shares_every_allocation() {
    let hints = hints();
    let theme = Theme::default();
    let pending = KeySequence::new(KeyChord::new(ModFlags::CTRL, Key::Char('p')), Vec::new());

    let first = StatuslineDto {
        hints: &hints,
        theme: &theme,
        pending: Some(&pending),
    };
    let second = StatuslineDto {
        hints: &hints,
        theme: &theme,
        pending: Some(&pending),
    };

    assert!(
        Arc::ptr_eq(&first.hints.entries, &second.hints.entries),
        "entries was copied"
    );
    assert!(
        Arc::ptr_eq(&first.hints.prefix_labels, &second.hints.prefix_labels),
        "prefix_labels was copied"
    );
    assert!(
        Arc::ptr_eq(&first.hints.removed, &second.hints.removed),
        "removed was copied"
    );
    assert!(std::ptr::eq(first.hints, second.hints), "hints was copied");
    assert!(std::ptr::eq(first.theme, second.theme), "theme was copied");
    assert!(
        std::ptr::eq(first.pending.unwrap(), second.pending.unwrap()),
        "pending was copied"
    );
}

#[test]
fn assembling_the_tab_row_input_twice_borrows_each_shared_field() {
    let mut snapshot = snapshot();
    snapshot.client.lock_mode = LockMode::Locked;
    snapshot.client.mouse_select = true;
    let theme = Theme::default();
    let viewer = ViewerChrome {
        reconnecting: Some(Reconnecting {
            attempt: 3,
            retry_in_seconds: 8,
        }),
        tabline_offset: Some(2),
        ..ViewerChrome::default()
    };

    let first = NavigatorDto {
        session_name: &snapshot.session.name,
        tabs: &snapshot.session.tabs_metadata,
        lock_mode: snapshot.client.lock_mode,
        mouse_select: snapshot.client.mouse_select,
        reconnecting: viewer.reconnecting,
        tabline_offset: viewer.tabline_offset,
        theme: &theme,
    };
    let second = NavigatorDto {
        session_name: &snapshot.session.name,
        tabs: &snapshot.session.tabs_metadata,
        lock_mode: snapshot.client.lock_mode,
        mouse_select: snapshot.client.mouse_select,
        reconnecting: viewer.reconnecting,
        tabline_offset: viewer.tabline_offset,
        theme: &theme,
    };

    assert_eq!(first.session_name, "one");
    assert_eq!(first.tabs[0].name, "first");
    assert_eq!(first.lock_mode, LockMode::Locked);
    assert!(first.mouse_select);
    assert_eq!(
        first.reconnecting,
        Some(Reconnecting {
            attempt: 3,
            retry_in_seconds: 8,
        })
    );
    assert_eq!(first.tabline_offset, Some(2));
    assert!(
        std::ptr::eq(first.session_name, second.session_name),
        "session name was copied"
    );
    assert!(std::ptr::eq(first.tabs, second.tabs), "tabs were copied");
    assert_eq!(first.lock_mode, second.lock_mode);
    assert_eq!(first.mouse_select, second.mouse_select);
    assert_eq!(first.reconnecting, second.reconnecting);
    assert_eq!(first.tabline_offset, second.tabline_offset);
    assert!(std::ptr::eq(first.theme, second.theme), "theme was copied");

    let layout = snapshot.layout(viewer);
    assert_eq!(first.inputs(), layout.navigator());
}
