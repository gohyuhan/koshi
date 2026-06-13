use tile_core::constant::MAX_TAB_FOCUS_MRU;
use tile_core::ids::{PaneId, SessionId, TabId};
use tile_layout::mode::LayoutMode;
use tile_layout::tree::LayoutNode;
use tile_pane::registry::PaneRegistry;

use super::lifecycle::TabLifecycle;
use super::state::{Session, Tab};
use crate::client::ClientRegistry;

#[test]
fn a_new_session_starts_empty() {
    let id = SessionId::new();
    let session = Session::new(id, "main".to_owned(), ClientRegistry::new());

    assert_eq!(session.id, id);
    assert_eq!(session.name, "main");
    assert!(session.tabs.is_empty());
    assert!(session.plugin_runtime_ref.is_none());
    // The registries are part of the public shape, reachable as fields.
    let _: &PaneRegistry = &session.panes;
    let _: &ClientRegistry = &session.clients;
}

#[test]
fn a_new_tab_owns_its_layout_and_starts_unfocused() {
    let tab_id = TabId::new();
    let root = PaneId::new();
    let tab = Tab::new(tab_id, "code".to_owned(), 0, root);

    assert_eq!(tab.id, tab_id);
    assert_eq!(tab.name, "code");
    assert_eq!(tab.index, 0);
    // A fresh tab shows exactly its root pane, tiled, mid-creation, no focus yet.
    assert_eq!(tab.layout, LayoutNode::Pane(root));
    assert_eq!(tab.layout_mode, LayoutMode::Tiled);
    assert_eq!(tab.lifecycle, TabLifecycle::Creating);
    assert!(tab.focus_mru().is_empty());
}

#[test]
fn renaming_a_tab_changes_only_its_name() {
    let root = PaneId::new();
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, root);

    tab.name = "logs".to_owned();

    assert_eq!(tab.name, "logs");
    // Position and layout are untouched by a rename.
    assert_eq!(tab.index, 0);
    assert_eq!(tab.layout, LayoutNode::Pane(root));
}

#[test]
fn a_tab_index_can_be_reassigned() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());

    tab.index = 3;

    assert_eq!(tab.index, 3);
}

#[test]
fn record_focus_orders_newest_first() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());

    tab.record_focus_mru(a);
    tab.record_focus_mru(b);
    tab.record_focus_mru(c);

    assert_eq!(tab.focus_mru().to_vec(), vec![c, b, a]);
}

#[test]
fn re_focusing_moves_to_front_without_duplicating() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let (a, b) = (PaneId::new(), PaneId::new());

    tab.record_focus_mru(a);
    tab.record_focus_mru(b);
    tab.record_focus_mru(a);

    // `a` returns to the front; it is not stored twice.
    assert_eq!(tab.focus_mru().to_vec(), vec![a, b]);
}

#[test]
fn focus_mru_is_capped_dropping_the_oldest() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let cap = MAX_TAB_FOCUS_MRU as usize;

    // Record one more distinct pane than the cap allows.
    let panes: Vec<PaneId> = (0..=cap).map(|_| PaneId::new()).collect();
    for &pane in &panes {
        tab.record_focus_mru(pane);
    }

    let mru = tab.focus_mru();
    assert_eq!(mru.len(), cap);
    // Newest sits at the front; the first-recorded pane is evicted.
    assert_eq!(mru[0], *panes.last().unwrap());
    assert!(!mru.contains(&panes[0]));
}

#[test]
fn a_tab_survives_a_serde_round_trip() {
    let root = PaneId::new();
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 2, root);
    tab.record_focus_mru(root);

    let json = serde_json::to_string(&tab).expect("serialize");
    let restored: Tab = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(tab, restored);
}
