//! Tests for the two chrome-row inputs: assembling either one twice borrows
//! the same data both times, and copies nothing behind it.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, SessionId, TabId};
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;

use crate::snapshot::{
    ClientSnapshot, HintBinding, PluginUiSnapshot, RenderSnapshot, SessionSnapshot, TabMeta,
    TabSnapshot, ViewerChrome,
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
fn assembling_the_tab_row_input_twice_borrows_the_same_frame() {
    let snapshot = snapshot();
    let theme = Theme::default();

    let first = NavigatorDto {
        frame: snapshot.layout(ViewerChrome::default()),
        theme: &theme,
    };
    let second = NavigatorDto {
        frame: snapshot.layout(ViewerChrome::default()),
        theme: &theme,
    };

    assert!(
        std::ptr::eq(first.frame.session, second.frame.session),
        "session was copied"
    );
    assert!(
        std::ptr::eq(first.frame.client, second.frame.client),
        "client was copied"
    );
    assert!(std::ptr::eq(first.theme, second.theme), "theme was copied");
}
