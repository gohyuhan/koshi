//! Client and ClientRegistry unit tests.
//!
//! Tests verify client state tracking (focus, viewport, lock mode, drag state) and
//! registry operations (attach, detach, lookup, mutation).

use std::time::SystemTime;

use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;

use super::{pane_viewport, Client, ClientRegistry, MouseState, ResizeDragState, TablineDragState};

/// Creates a test client with the given ID and active tab.
fn a_client_with(id: ClientId, active_tab: TabId) -> Client {
    Client::new(
        id,
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        active_tab,
    )
}

/// Creates a test client with a fresh ID and the given active tab.
fn a_client(active_tab: TabId) -> Client {
    a_client_with(ClientId::new(), active_tab)
}

#[test]
fn a_new_client_starts_unlocked_with_no_focus_and_no_drag() {
    let tab = TabId::new();
    let client = a_client(tab);

    assert_eq!(client.lock_mode(), LockMode::Normal);
    assert_eq!(client.active_tab(), tab);
    assert_eq!(client.focused_pane(tab), None);
    assert_eq!(client.mouse_state(), &MouseState);
    // A freshly attached client is never mid-drag.
    assert!(client.pending_resize_drag().is_none());
    // And it follows its active tab (no peek offset, no tabline drag).
    assert_eq!(client.tabline_offset(), None);
    assert_eq!(client.tabline_drag(), None);
}

#[test]
fn tabline_offset_and_drag_round_trip_and_reset_on_tab_switch() {
    let tab = TabId::new();
    let other = TabId::new();
    let mut client = a_client(tab);

    client.set_tabline_offset(Some(3));
    assert_eq!(client.tabline_offset(), Some(3));
    client.set_tabline_drag(Some(TablineDragState {
        anchor_x: 12,
        anchor_first_visible: 3,
    }));
    assert_eq!(
        client.tabline_drag(),
        Some(TablineDragState {
            anchor_x: 12,
            anchor_first_visible: 3,
        })
    );

    // Switching tabs reveals the new tab: the peek offset and the drag both clear.
    client.update_active_tab(other);
    assert_eq!(client.active_tab(), other);
    assert_eq!(client.tabline_offset(), None);
    assert_eq!(client.tabline_drag(), None);
}

#[test]
fn two_clients_focus_different_panes_in_the_same_tab() {
    let tab = TabId::new();
    let (pane_a, pane_b) = (PaneId::new(), PaneId::new());
    let mut alice = a_client(tab);
    let mut bob = a_client(tab);

    alice.update_focused_pane(tab, pane_a);
    bob.update_focused_pane(tab, pane_b);

    // Same tab, independent focus per client — they never share one cursor.
    assert_eq!(alice.focused_pane(tab), Some(pane_a));
    assert_eq!(bob.focused_pane(tab), Some(pane_b));
    assert_ne!(pane_a, pane_b);
}

#[test]
fn locking_one_client_leaves_another_unchanged() {
    let tab = TabId::new();
    let mut alice = a_client(tab);
    let bob = a_client(tab);

    alice.update_lock_mode(LockMode::Locked);

    assert_eq!(alice.lock_mode(), LockMode::Locked);
    assert_eq!(bob.lock_mode(), LockMode::Normal);
}

#[test]
fn viewport_is_per_client() {
    let tab = TabId::new();
    let mut alice = a_client(tab);
    let bob = a_client(tab);

    alice.update_viewport(Size {
        cols: 120,
        rows: 40,
    });

    assert_eq!(
        alice.viewport(),
        Size {
            cols: 120,
            rows: 40
        }
    );
    assert_eq!(bob.viewport(), Size { cols: 80, rows: 24 });
}

#[test]
fn focus_is_tracked_independently_per_tab() {
    let (tab_a, tab_b) = (TabId::new(), TabId::new());
    let (pane_a, pane_b) = (PaneId::new(), PaneId::new());
    let mut client = a_client(tab_a);

    client.update_focused_pane(tab_a, pane_a);
    client.update_active_tab(tab_b);
    client.update_focused_pane(tab_b, pane_b);
    // Switching back restores the focus held in tab_a; it is not lost.
    client.update_active_tab(tab_a);

    assert_eq!(client.active_tab(), tab_a);
    assert_eq!(client.focused_pane(tab_a), Some(pane_a));
    assert_eq!(client.focused_pane(tab_b), Some(pane_b));
}

#[test]
fn removing_a_tabs_focus_prunes_it() {
    let tab = TabId::new();
    let mut client = a_client(tab);
    client.update_focused_pane(tab, PaneId::new());

    client.remove_focused_pane(tab);

    assert_eq!(client.focused_pane(tab), None);
}

#[test]
fn updating_a_tabs_focus_returns_the_previous_pane() {
    let tab = TabId::new();
    let (first, second) = (PaneId::new(), PaneId::new());
    let mut client = a_client(tab);

    assert_eq!(client.update_focused_pane(tab, first), None);
    assert_eq!(client.update_focused_pane(tab, second), Some(first));
    assert_eq!(client.focused_pane(tab), Some(second));
}

#[test]
fn a_pending_resize_drag_can_be_set_and_cleared() {
    let mut client = a_client(TabId::new());

    client.update_pending_resize_drag(Some(ResizeDragState));
    assert_eq!(client.pending_resize_drag(), Some(&ResizeDragState));

    client.update_pending_resize_drag(None);
    assert!(client.pending_resize_drag().is_none());
}

#[test]
fn a_new_registry_has_no_clients() {
    let registry = ClientRegistry::new();

    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert_eq!(registry.list_attached().count(), 0);
}

#[test]
fn attaching_a_client_registers_it() {
    let mut registry = ClientRegistry::new();
    let client = a_client(TabId::new());
    let id = client.id();

    // A first attach displaces nothing.
    assert!(registry.attach(client).is_none());

    assert_eq!(registry.len(), 1);
    assert!(!registry.is_empty());
    assert_eq!(registry.get(id).map(Client::id), Some(id));
    assert_eq!(registry.list_attached().count(), 1);
}

#[test]
fn detaching_a_client_removes_and_returns_it() {
    let mut registry = ClientRegistry::new();
    let client = a_client(TabId::new());
    let id = client.id();
    registry.attach(client);

    let detached = registry.detach(id);

    assert_eq!(detached.map(|c| c.id()), Some(id));
    assert!(registry.get(id).is_none());
    assert!(registry.is_empty());
}

#[test]
fn detaching_an_unattached_client_returns_nothing() {
    let mut registry = ClientRegistry::new();

    assert!(registry.detach(ClientId::new()).is_none());
}

#[test]
fn get_mut_edits_a_client_in_place() {
    let mut registry = ClientRegistry::new();
    let client = a_client(TabId::new());
    let id = client.id();
    registry.attach(client);

    registry
        .get_mut(id)
        .expect("attached client")
        .update_lock_mode(LockMode::Locked);

    // The edit is visible through the registry — it handed out a live handle.
    assert_eq!(
        registry.get(id).map(Client::lock_mode),
        Some(LockMode::Locked)
    );
}

#[test]
fn re_attaching_the_same_id_replaces_and_returns_the_prior() {
    let mut registry = ClientRegistry::new();
    let id = ClientId::new();
    let (tab_first, tab_second) = (TabId::new(), TabId::new());

    assert!(registry.attach(a_client_with(id, tab_first)).is_none());
    let replaced = registry.attach(a_client_with(id, tab_second));

    // The prior record comes back; the registry holds exactly the new one.
    assert_eq!(replaced.map(|c| c.active_tab()), Some(tab_first));
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.get(id).map(Client::active_tab), Some(tab_second));
}

#[test]
fn scroll_offset_defaults_to_zero_for_an_unscrolled_pane() {
    let client = a_client(TabId::new());
    assert_eq!(client.scroll_offset(PaneId::new()), 0);
}

#[test]
fn set_scroll_offset_records_and_reads_back_per_pane() {
    let mut client = a_client(TabId::new());
    let (first, second) = (PaneId::new(), PaneId::new());

    client.set_scroll_offset(first, 7);
    // Panes scroll independently; the second is untouched.
    assert_eq!(client.scroll_offset(first), 7);
    assert_eq!(client.scroll_offset(second), 0);
}

#[test]
fn set_scroll_offset_zero_clears_the_entry_restoring_live_following() {
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();

    client.set_scroll_offset(pane, 3);
    client.set_scroll_offset(pane, 0);
    assert_eq!(client.scroll_offset(pane), 0);
}

#[test]
fn list_attached_mut_reaches_every_client_for_in_place_updates() {
    let mut registry = ClientRegistry::new();
    let pane = PaneId::new();
    registry.attach(a_client(TabId::new()));
    registry.attach(a_client(TabId::new()));

    for client in registry.list_attached_mut() {
        client.set_scroll_offset(pane, 4);
    }
    assert!(registry.list_attached().all(|c| c.scroll_offset(pane) == 4));
}

// --- pane_viewport -----------------------------------------------------

#[test]
fn pane_viewport_reserves_the_tabline_and_hint_row() {
    // 80x24 minus one tabline row and one hint row leaves 80x22.
    assert_eq!(
        pane_viewport(Size { cols: 80, rows: 24 }),
        Size { cols: 80, rows: 22 }
    );
}

#[test]
fn pane_viewport_of_a_two_row_viewport_is_exactly_zero_rows() {
    // Exactly enough for the two chrome rows and nothing else: 2 - 2 = 0,
    // the boundary just above the saturating case below.
    assert_eq!(
        pane_viewport(Size { cols: 80, rows: 2 }),
        Size { cols: 80, rows: 0 }
    );
}

#[test]
fn pane_viewport_of_a_one_row_viewport_saturates_to_zero_rows() {
    // Fewer rows than the reserved chrome: plain subtraction would underflow
    // and panic (or wrap) on the u16 row count; the contract is saturation,
    // not a panic.
    assert_eq!(
        pane_viewport(Size { cols: 80, rows: 1 }),
        Size { cols: 80, rows: 0 }
    );
}

#[test]
fn pane_viewport_of_a_zero_row_viewport_stays_zero_rows() {
    assert_eq!(
        pane_viewport(Size { cols: 80, rows: 0 }),
        Size { cols: 80, rows: 0 }
    );
}

#[test]
fn pane_viewport_never_touches_the_column_count() {
    assert_eq!(
        pane_viewport(Size { cols: 0, rows: 24 }),
        Size { cols: 0, rows: 22 }
    );
}
