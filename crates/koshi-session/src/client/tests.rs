//! Client and ClientRegistry unit tests.
//!
//! Tests verify the server-set identity (origin, label, colour), client
//! state tracking (focus, viewport, lock mode, zoom, scrollback view,
//! highlights) and registry operations (attach, detach, lookup, mutation).

use std::time::SystemTime;

use koshi_core::command::{GridPos, Selection, SelectionKind};
use koshi_core::geometry::{PaneArea, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;

use super::{pane_viewport, Client, ClientOrigin, ClientRegistry};

/// Creates a test client with the given ID and active tab.
fn a_client_with(id: ClientId, active_tab: TabId) -> Client {
    Client::new(
        id,
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        None,
        active_tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    )
}

/// Creates a test client on an 80x24 viewport reporting `pane_area`.
fn a_client_reporting(active_tab: TabId, pane_area: Option<PaneArea>) -> Client {
    Client::new(
        ClientId::new(),
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        pane_area,
        active_tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    )
}

/// Creates a test client with a fresh ID and the given active tab.
fn a_client(active_tab: TabId) -> Client {
    a_client_with(ClientId::new(), active_tab)
}

#[test]
fn a_client_keeps_the_origin_label_and_colour_it_was_made_with() {
    for origin in [ClientOrigin::Local, ClientOrigin::Remote] {
        let client = Client::new(
            ClientId::new(),
            SessionId::new(),
            SystemTime::UNIX_EPOCH,
            Size { cols: 80, rows: 24 },
            None,
            TabId::new(),
            origin,
            "C-swift-otter".to_string(),
            3,
        );

        assert_eq!(client.origin(), origin);
        assert_eq!(client.label(), "C-swift-otter");
        assert_eq!(client.colour(), 3);
    }
}

#[test]
fn a_client_carries_where_it_connected_from_across_a_serde_round_trip() {
    for (origin, written) in [
        (ClientOrigin::Local, "Local"),
        (ClientOrigin::Remote, "Remote"),
    ] {
        let id = ClientId::new();
        let session_id = SessionId::new();
        let active_tab = TabId::new();
        let client = Client::new(
            id,
            session_id,
            SystemTime::UNIX_EPOCH,
            Size { cols: 80, rows: 24 },
            None,
            active_tab,
            origin,
            "C-swift-otter".to_string(),
            3,
        );

        let text = serde_json::to_string(&client).expect("the client encodes");
        let encoded: serde_json::Value =
            serde_json::from_str(&text).expect("the encoded client is json");
        assert_eq!(
            encoded["origin"],
            serde_json::Value::String(written.to_string())
        );
        assert_eq!(
            encoded.get("tier"),
            None,
            "a client record carries no authority key"
        );

        let read_back: Client = serde_json::from_str(&text).expect("the client decodes");
        assert_eq!(read_back.id(), id);
        assert_eq!(read_back.session_id(), session_id);
        assert_eq!(read_back.origin(), origin);
        assert_eq!(read_back.label(), "C-swift-otter");
        assert_eq!(read_back.colour(), 3);
        assert_eq!(read_back.active_tab(), active_tab);
    }
}

#[test]
fn a_new_client_starts_unlocked_with_no_focus() {
    let tab = TabId::new();
    let client = a_client(tab);

    assert_eq!(client.lock_mode(), LockMode::Normal);
    assert_eq!(client.active_tab(), tab);
    assert_eq!(client.focused_pane(tab), None);
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
fn focusing_another_pane_in_a_zoomed_tab_moves_the_zoom_to_it() {
    // Zoom follows focus: with a tab zoomed on one pane, focusing a different
    // pane there swaps the zoom onto it rather than dropping back to tiled.
    let tab = TabId::new();
    let (zoomed, next) = (PaneId::new(), PaneId::new());
    let mut client = a_client(tab);
    client.update_focused_pane(tab, zoomed);
    client.zoom_pane(tab, zoomed);
    assert_eq!(client.zoomed_pane(tab), Some(zoomed));

    let prior = client.update_focused_pane(tab, next);

    assert_eq!(prior, Some(zoomed));
    assert_eq!(client.zoomed_pane(tab), Some(next));
    assert_eq!(client.focused_pane(tab), Some(next));
    assert_eq!(
        client.layout_mode(tab),
        LayoutMode::Fullscreen { focused: next }
    );
}

#[test]
fn focusing_a_pane_in_a_tiled_tab_creates_no_zoom() {
    // Focusing a pane in a tab with no zoom must not invent one — the tab stays
    // tiled for this client.
    let tab = TabId::new();
    let mut client = a_client(tab);

    client.update_focused_pane(tab, PaneId::new());

    assert_eq!(client.zoomed_pane(tab), None);
    assert_eq!(client.layout_mode(tab), LayoutMode::Tiled);
}

#[test]
fn removing_a_tabs_focus_also_drops_its_zoom() {
    // Forgetting the focused pane in a tab drops any zoom there too: with no
    // focused pane there is no pane for a zoom to show.
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut client = a_client(tab);
    client.update_focused_pane(tab, pane);
    client.zoom_pane(tab, pane);

    client.remove_focused_pane(tab);

    assert_eq!(client.focused_pane(tab), None);
    assert_eq!(client.zoomed_pane(tab), None);
    assert_eq!(client.layout_mode(tab), LayoutMode::Tiled);
}

#[test]
fn zoom_is_tracked_independently_per_client() {
    // Two clients on the same tab zoom independently: one zooming a pane leaves
    // the other's tiled view untouched.
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut alice = a_client(tab);
    let bob = a_client(tab);

    alice.zoom_pane(tab, pane);

    assert_eq!(alice.zoomed_pane(tab), Some(pane));
    assert_eq!(bob.zoomed_pane(tab), None);
    assert_eq!(bob.layout_mode(tab), LayoutMode::Tiled);
}

#[test]
fn focusing_a_pane_in_another_tab_leaves_a_zoom_where_it_is() {
    // Zoom follows focus only inside the tab being focused: focusing in
    // `other_tab` must not move the zoom held in `zoomed_tab`.
    let (zoomed_tab, other_tab) = (TabId::new(), TabId::new());
    let (zoomed_pane, other_pane) = (PaneId::new(), PaneId::new());
    let mut client = a_client(zoomed_tab);
    client.update_focused_pane(zoomed_tab, zoomed_pane);
    client.zoom_pane(zoomed_tab, zoomed_pane);

    client.update_focused_pane(other_tab, other_pane);

    assert_eq!(client.zoomed_pane(zoomed_tab), Some(zoomed_pane));
    assert_eq!(client.zoomed_pane(other_tab), None);
    assert_eq!(
        client.layout_mode(zoomed_tab),
        LayoutMode::Fullscreen {
            focused: zoomed_pane
        }
    );
    assert_eq!(client.layout_mode(other_tab), LayoutMode::Tiled);
}

#[test]
fn removing_the_focus_of_a_never_focused_tab_changes_nothing() {
    let (focused_tab, untouched_tab) = (TabId::new(), TabId::new());
    let pane = PaneId::new();
    let mut client = a_client(focused_tab);
    client.update_focused_pane(focused_tab, pane);
    client.zoom_pane(focused_tab, pane);

    client.remove_focused_pane(untouched_tab);

    assert_eq!(client.focused_pane(focused_tab), Some(pane));
    assert_eq!(client.zoomed_pane(focused_tab), Some(pane));
    assert_eq!(client.focused_panes().len(), 1);
    assert_eq!(client.zoomed_panes().len(), 1);
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

/// A highlight, whose shape does not matter to the view rules under test.
fn a_selection() -> Selection {
    Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 0, col: 4 },
    }
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
fn set_scroll_offset_zero_clears_the_entry() {
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();

    client.set_scroll_offset(pane, 3);
    client.set_scroll_offset(pane, 0);
    assert_eq!(client.scroll_offset(pane), 0);
    assert_eq!(client.scroll_offsets().get(&pane), None);
}

#[test]
fn set_scroll_offset_zero_on_an_unscrolled_pane_adds_no_entry() {
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();

    client.set_scroll_offset(pane, 0);

    assert_eq!(client.scroll_offset(pane), 0);
    assert_eq!(client.scroll_offsets().len(), 0);
    assert!(!client.is_view_held(pane));
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
    let offsets: Vec<usize> = registry
        .list_attached()
        .map(|client| client.scroll_offset(pane))
        .collect();
    assert_eq!(offsets, vec![4, 4]);
}

// --- is_view_held: the two reasons a view is held ----------------------

#[test]
fn a_view_at_the_bottom_with_no_highlight_is_not_held() {
    let client = a_client(TabId::new());
    assert!(!client.is_view_held(PaneId::new()));
}

#[test]
fn a_scrolled_up_view_is_held() {
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();

    client.set_scroll_offset(pane, 1); // one line up is enough
    assert!(client.is_view_held(pane));
}

#[test]
fn a_highlight_holds_a_view_sitting_at_the_bottom() {
    // The state an offset alone cannot express: at the newest line and held.
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();

    client.set_selection(pane, a_selection());
    assert_eq!(client.scroll_offset(pane), 0);
    assert!(client.is_view_held(pane));
}

#[test]
fn a_highlight_holds_its_view_no_matter_where_it_is_scrolled() {
    // Both reasons at once: still held, and scrolling back to the bottom does not
    // release it while the highlight is up.
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();
    client.set_selection(pane, a_selection());

    client.set_scroll_offset(pane, 5);
    assert!(client.is_view_held(pane));

    client.set_scroll_offset(pane, 0); // scrolled back to the newest line
    assert!(client.is_view_held(pane));
}

#[test]
fn clearing_a_highlight_at_the_bottom_releases_the_view() {
    // Nothing has to remember to release it: the highlight was the only thing
    // holding it, so dropping the highlight is the release.
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();
    client.set_selection(pane, a_selection());
    assert!(client.is_view_held(pane));

    client.clear_selection(pane);
    assert!(!client.is_view_held(pane));
}

#[test]
fn clearing_a_highlight_leaves_a_scrolled_up_view_held() {
    // The other reason survives on its own: the view is still 3 lines up, so it
    // stays held until it is scrolled back to the bottom.
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();
    client.set_selection(pane, a_selection());
    client.set_scroll_offset(pane, 3);

    client.clear_selection(pane);
    assert!(client.is_view_held(pane));

    client.set_scroll_offset(pane, 0);
    assert!(!client.is_view_held(pane));
}

#[test]
fn a_highlight_holds_only_its_own_pane() {
    let mut client = a_client(TabId::new());
    let (held, other) = (PaneId::new(), PaneId::new());

    client.set_selection(held, a_selection());
    assert!(client.is_view_held(held));
    assert!(!client.is_view_held(other));
}

#[test]
fn highlighting_a_second_pane_leaves_the_first_panes_highlight_alone() {
    // One highlight per client, so starting one in `second` drops the one in
    // `first` — and `first` has nothing holding it any more, so it follows live
    // again. Nothing has to release it; the single `Option` is the whole rule.
    let mut client = a_client(TabId::new());
    let (first, second) = (PaneId::new(), PaneId::new());
    client.set_selection(first, a_selection());

    client.set_selection(second, a_selection());

    assert_eq!(client.selection(first), Some(a_selection()));
    assert_eq!(client.selection(second), Some(a_selection()));
    assert!(client.is_view_held(first));
    assert!(client.is_view_held(second));
}

#[test]
fn selection_reads_back_per_pane() {
    let mut client = a_client(TabId::new());
    let (pane, other) = (PaneId::new(), PaneId::new());
    assert_eq!(client.selection(pane), None);

    client.set_selection(pane, a_selection());
    assert_eq!(client.selection(pane), Some(a_selection()));
    assert_eq!(client.selection(other), None);
}

#[test]
fn setting_a_highlight_twice_in_one_pane_replaces_it() {
    // A drag re-issues the highlight as it grows; the pane holds the latest.
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();
    client.set_selection(pane, a_selection());

    let grown = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 2, col: 7 },
    };
    client.set_selection(pane, grown);
    assert_eq!(client.selection(pane), Some(grown));
}

#[test]
fn clear_selection_drops_only_that_panes_highlight() {
    let mut client = a_client(TabId::new());
    let (pane, other) = (PaneId::new(), PaneId::new());
    client.set_selection(pane, a_selection());
    client.set_selection(other, a_selection());

    client.clear_selection(other);

    assert_eq!(client.selection(pane), Some(a_selection()));
    assert!(client.is_view_held(pane));
    assert_eq!(client.selection(other), None);
    assert!(!client.is_view_held(other));
}

#[test]
fn clearing_a_pane_with_no_highlight_changes_nothing() {
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();

    client.clear_selection(pane);
    assert_eq!(client.selection(pane), None);
    assert!(!client.is_view_held(pane));
}

#[test]
fn one_clients_highlight_leaves_another_viewing_the_same_pane_alone() {
    // Two clients on one pane: the highlight is per-client, so one selecting must
    // not hold the other's view.
    let mut registry = ClientRegistry::new();
    let pane = PaneId::new();
    let (first, second) = (a_client(TabId::new()), a_client(TabId::new()));
    let (first_id, second_id) = (first.id(), second.id());
    registry.attach(first);
    registry.attach(second);

    registry
        .get_mut(first_id)
        .expect("the client was just attached")
        .set_selection(pane, a_selection());

    let first = registry.get(first_id).expect("attached");
    assert_eq!(first.selection(pane), Some(a_selection()));
    assert!(first.is_view_held(pane));

    let second = registry.get(second_id).expect("attached");
    assert_eq!(second.selection(pane), None);
    assert!(!second.is_view_held(pane));
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

// --- pane_area ---------------------------------------------------------

#[test]
fn a_client_that_reported_no_pane_area_sizes_as_its_viewport_minus_two_rows() {
    let client = a_client_reporting(TabId::new(), None);

    assert_eq!(client.pane_area(), Some(Size { cols: 80, rows: 22 }));
    assert_eq!(client.pane_area(), Some(pane_viewport(client.viewport())));
}

#[test]
fn a_reported_pane_area_is_clamped_to_the_viewport_per_axis() {
    // The viewport is 80x24 in every case.
    let wider_and_taller = a_client_reporting(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            cols: 200,
            rows: 50,
        })),
    );
    assert_eq!(
        wider_and_taller.pane_area(),
        Some(Size { cols: 80, rows: 24 })
    );

    let inside = a_client_reporting(
        TabId::new(),
        Some(PaneArea::Reported(Size { cols: 40, rows: 10 })),
    );
    assert_eq!(inside.pane_area(), Some(Size { cols: 40, rows: 10 }));

    let wider_only = a_client_reporting(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            cols: 100,
            rows: 10,
        })),
    );
    assert_eq!(wider_only.pane_area(), Some(Size { cols: 80, rows: 10 }));
}

#[test]
fn a_viewport_with_no_room_for_the_chrome_rows_gives_a_zero_row_pane_area() {
    let client = Client::new(
        ClientId::new(),
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 1 },
        None,
        TabId::new(),
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );

    assert_eq!(client.pane_area(), Some(Size { cols: 80, rows: 0 }));
}

#[test]
fn shrinking_the_viewport_reclamps_a_reported_pane_area() {
    let mut client = a_client_reporting(
        TabId::new(),
        Some(PaneArea::Reported(Size { cols: 80, rows: 24 })),
    );
    assert_eq!(client.pane_area(), Some(Size { cols: 80, rows: 24 }));

    client.update_viewport(Size { cols: 40, rows: 10 });

    // The report is kept as reported; only the clamp against the new viewport
    // moves.
    assert_eq!(
        client.reported_pane_area(),
        Some(PaneArea::Reported(Size { cols: 80, rows: 24 }))
    );
    assert_eq!(client.pane_area(), Some(Size { cols: 40, rows: 10 }));
}

#[test]
fn a_reported_pane_area_equal_to_the_viewport_is_not_reduced() {
    let client = a_client_reporting(
        TabId::new(),
        Some(PaneArea::Reported(Size { cols: 80, rows: 24 })),
    );

    // The report stands as given: no chrome rows are taken off it.
    assert_eq!(client.pane_area(), Some(Size { cols: 80, rows: 24 }));
}

#[test]
fn a_reported_pane_area_at_the_maximum_size_is_clamped_to_the_viewport() {
    let client = a_client_reporting(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            cols: u16::MAX,
            rows: u16::MAX,
        })),
    );

    assert_eq!(client.pane_area(), Some(Size { cols: 80, rows: 24 }));
}

#[test]
fn a_starving_client_has_no_pane_area() {
    let client = a_client_reporting(TabId::new(), Some(PaneArea::Starving));

    assert_eq!(client.pane_area(), None);
}

#[test]
fn reported_pane_area_returns_the_raw_report() {
    let reported = PaneArea::Reported(Size { cols: 40, rows: 10 });

    assert_eq!(
        a_client_reporting(TabId::new(), None).reported_pane_area(),
        None
    );
    assert_eq!(
        a_client_reporting(TabId::new(), Some(reported)).reported_pane_area(),
        Some(reported)
    );
    assert_eq!(
        a_client_reporting(TabId::new(), Some(PaneArea::Starving)).reported_pane_area(),
        Some(PaneArea::Starving)
    );
}

#[test]
fn update_pane_area_replaces_a_report_with_none() {
    let mut client = a_client_reporting(
        TabId::new(),
        Some(PaneArea::Reported(Size { cols: 40, rows: 10 })),
    );
    assert_eq!(client.pane_area(), Some(Size { cols: 40, rows: 10 }));

    client.update_pane_area(None);

    assert_eq!(client.reported_pane_area(), None);
    assert_eq!(client.pane_area(), Some(Size { cols: 80, rows: 22 }));
}

#[test]
fn a_client_json_without_pane_area_decodes_as_none() {
    let client = a_client_reporting(TabId::new(), Some(PaneArea::Starving));
    let mut encoded: serde_json::Value = serde_json::to_value(&client).expect("the client encodes");
    assert!(
        encoded
            .as_object_mut()
            .expect("a client encodes as a json object")
            .remove("pane_area")
            .is_some(),
        "the encoded client carries a pane_area key"
    );

    let read_back: Client = serde_json::from_value(encoded).expect("the client decodes");

    assert_eq!(read_back.reported_pane_area(), None);
    assert_eq!(read_back.pane_area(), Some(Size { cols: 80, rows: 22 }));
}

#[test]
fn a_client_reported_pane_area_survives_a_serde_round_trip() {
    for reported in [
        PaneArea::Starving,
        PaneArea::Reported(Size { cols: 40, rows: 10 }),
    ] {
        let client = a_client_reporting(TabId::new(), Some(reported));

        let text = serde_json::to_string(&client).expect("the client encodes");
        let read_back: Client = serde_json::from_str(&text).expect("the client decodes");

        assert_eq!(read_back.reported_pane_area(), Some(reported));
    }
}

#[test]
fn a_reported_pane_area_of_zero_stays_zero() {
    let client = a_client_reporting(
        TabId::new(),
        Some(PaneArea::Reported(Size { cols: 0, rows: 0 })),
    );

    assert_eq!(client.pane_area(), Some(Size { cols: 0, rows: 0 }));
}

#[test]
fn pane_viewport_of_the_tallest_viewport_reserves_two_rows() {
    assert_eq!(
        pane_viewport(Size {
            cols: u16::MAX,
            rows: u16::MAX
        }),
        Size {
            cols: u16::MAX,
            rows: u16::MAX - 2
        }
    );
}

// --- mouse select ------------------------------------------------------

#[test]
fn mouse_select_starts_off_and_each_toggle_flips_it() {
    let mut client = a_client(TabId::new());
    assert!(!client.mouse_select());

    assert!(client.toggle_mouse_select());
    assert!(client.mouse_select());

    assert!(!client.toggle_mouse_select());
    assert!(!client.mouse_select());
}

#[test]
fn mouse_select_and_lock_mode_do_not_touch_each_other() {
    let mut client = a_client(TabId::new());

    client.toggle_mouse_select();
    assert_eq!(client.lock_mode(), LockMode::Normal);

    client.update_lock_mode(LockMode::Locked);
    assert!(client.mouse_select());
    assert_eq!(client.lock_mode(), LockMode::Locked);
}

#[test]
fn mouse_select_is_per_client() {
    let tab = TabId::new();
    let mut alice = a_client(tab);
    let bob = a_client(tab);

    alice.toggle_mouse_select();

    assert!(alice.mouse_select());
    assert!(!bob.mouse_select());
}

// --- zoom bookkeeping --------------------------------------------------

#[test]
fn zooming_a_second_pane_in_one_tab_replaces_the_zoom() {
    let tab = TabId::new();
    let (first, second) = (PaneId::new(), PaneId::new());
    let mut client = a_client(tab);

    client.zoom_pane(tab, first);
    client.zoom_pane(tab, second);

    assert_eq!(client.zoomed_pane(tab), Some(second));
    assert_eq!(
        client.layout_mode(tab),
        LayoutMode::Fullscreen { focused: second }
    );
}

#[test]
fn clear_zoom_drops_only_that_tabs_zoom() {
    let (tab_a, tab_b) = (TabId::new(), TabId::new());
    let (pane_a, pane_b) = (PaneId::new(), PaneId::new());
    let mut client = a_client(tab_a);
    client.zoom_pane(tab_a, pane_a);
    client.zoom_pane(tab_b, pane_b);

    client.clear_zoom(tab_a);

    assert_eq!(client.zoomed_pane(tab_a), None);
    assert_eq!(client.layout_mode(tab_a), LayoutMode::Tiled);
    assert_eq!(client.zoomed_pane(tab_b), Some(pane_b));
}

#[test]
fn clearing_the_zoom_of_a_tiled_tab_changes_nothing() {
    let tab = TabId::new();
    let mut client = a_client(tab);

    client.clear_zoom(tab);

    assert_eq!(client.zoomed_pane(tab), None);
    assert_eq!(client.zoomed_panes().len(), 0);
}

#[test]
fn clear_zoom_of_pane_drops_that_pane_in_every_tab_and_leaves_the_rest() {
    // The same pane zoomed in two tabs goes from both; a tab zoomed on another
    // pane keeps its zoom.
    let (tab_a, tab_b, tab_c) = (TabId::new(), TabId::new(), TabId::new());
    let (gone, kept) = (PaneId::new(), PaneId::new());
    let mut client = a_client(tab_a);
    client.zoom_pane(tab_a, gone);
    client.zoom_pane(tab_b, gone);
    client.zoom_pane(tab_c, kept);

    client.clear_zoom_of_pane(gone);

    assert_eq!(client.zoomed_pane(tab_a), None);
    assert_eq!(client.zoomed_pane(tab_b), None);
    assert_eq!(client.zoomed_pane(tab_c), Some(kept));
    assert_eq!(client.zoomed_panes().len(), 1);
}

#[test]
fn clear_zoom_of_a_pane_no_tab_is_zoomed_on_changes_nothing() {
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut client = a_client(tab);
    client.zoom_pane(tab, pane);

    client.clear_zoom_of_pane(PaneId::new());

    assert_eq!(client.zoomed_pane(tab), Some(pane));
    assert_eq!(client.zoomed_panes().len(), 1);
}

#[test]
fn zoomed_panes_lists_every_zoom_keyed_by_tab() {
    let (tab_a, tab_b) = (TabId::new(), TabId::new());
    let (pane_a, pane_b) = (PaneId::new(), PaneId::new());
    let mut client = a_client(tab_a);

    client.zoom_pane(tab_a, pane_a);
    client.zoom_pane(tab_b, pane_b);

    let zoomed = client.zoomed_panes();
    assert_eq!(zoomed.len(), 2);
    assert_eq!(zoomed.get(&tab_a), Some(&pane_a));
    assert_eq!(zoomed.get(&tab_b), Some(&pane_b));
}

#[test]
fn a_tab_the_client_has_never_seen_is_tiled() {
    let client = a_client(TabId::new());

    assert_eq!(client.layout_mode(TabId::new()), LayoutMode::Tiled);
    assert_eq!(client.zoomed_pane(TabId::new()), None);
}

// --- focus and scroll map views ----------------------------------------

#[test]
fn focused_panes_lists_every_remembered_focus_keyed_by_tab() {
    let (tab_a, tab_b) = (TabId::new(), TabId::new());
    let (pane_a, pane_b) = (PaneId::new(), PaneId::new());
    let mut client = a_client(tab_a);

    client.update_focused_pane(tab_a, pane_a);
    client.update_focused_pane(tab_b, pane_b);

    let focused = client.focused_panes();
    assert_eq!(focused.len(), 2);
    assert_eq!(focused.get(&tab_a), Some(&pane_a));
    assert_eq!(focused.get(&tab_b), Some(&pane_b));

    client.remove_focused_pane(tab_a);
    assert_eq!(client.focused_panes().len(), 1);
    assert_eq!(client.focused_panes().get(&tab_b), Some(&pane_b));
}

#[test]
fn scroll_offsets_holds_only_the_scrolled_up_panes() {
    let mut client = a_client(TabId::new());
    let (scrolled, at_bottom) = (PaneId::new(), PaneId::new());

    client.set_scroll_offset(scrolled, 5);
    client.set_scroll_offset(at_bottom, 0);

    let offsets = client.scroll_offsets();
    assert_eq!(offsets.len(), 1);
    assert_eq!(offsets.get(&scrolled), Some(&5));
    assert_eq!(offsets.get(&at_bottom), None);
}

#[test]
fn set_scroll_offset_keeps_the_largest_offset() {
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();

    client.set_scroll_offset(pane, usize::MAX);

    assert_eq!(client.scroll_offset(pane), usize::MAX);
    assert!(client.is_view_held(pane));
}

#[test]
fn switching_tabs_keeps_every_highlight_and_scroll_position() {
    let (tab_a, tab_b) = (TabId::new(), TabId::new());
    let pane = PaneId::new();
    let mut client = a_client(tab_a);
    client.set_selection(pane, a_selection());
    client.set_scroll_offset(pane, 4);

    client.update_active_tab(tab_b);
    client.update_active_tab(tab_a);

    assert_eq!(client.active_tab(), tab_a);
    assert_eq!(client.selection(pane), Some(a_selection()));
    assert_eq!(client.scroll_offset(pane), 4);
}

// --- identity ----------------------------------------------------------

#[test]
fn a_client_reads_back_the_session_and_attach_time_it_was_made_with() {
    let session_id = SessionId::new();
    let attached_at = SystemTime::UNIX_EPOCH;
    let client = Client::new(
        ClientId::new(),
        session_id,
        attached_at,
        Size { cols: 80, rows: 24 },
        None,
        TabId::new(),
        ClientOrigin::Local,
        "C-swift-otter".to_string(),
        3,
    );

    assert_eq!(client.session_id(), session_id);
    assert_eq!(client.attached_at(), attached_at);
}

#[test]
fn update_origin_replaces_where_the_client_connected_from() {
    let mut client = a_client(TabId::new());
    assert_eq!(client.origin(), ClientOrigin::Local);

    client.update_origin(ClientOrigin::Remote);
    assert_eq!(client.origin(), ClientOrigin::Remote);

    client.update_origin(ClientOrigin::Local);
    assert_eq!(client.origin(), ClientOrigin::Local);
}

#[test]
fn a_clients_whole_view_state_survives_a_serde_round_trip() {
    let (tab, other_tab) = (TabId::new(), TabId::new());
    let (pane, other_pane) = (PaneId::new(), PaneId::new());
    let mut client = a_client(tab);
    client.update_lock_mode(LockMode::Locked);
    client.toggle_mouse_select();
    client.update_focused_pane(tab, pane);
    client.zoom_pane(other_tab, other_pane);
    client.set_scroll_offset(pane, 9);
    client.set_selection(pane, a_selection());

    let text = serde_json::to_string(&client).expect("the client encodes");
    let read_back: Client = serde_json::from_str(&text).expect("the client decodes");

    assert_eq!(read_back.lock_mode(), LockMode::Locked);
    assert!(read_back.mouse_select());
    assert_eq!(read_back.focused_pane(tab), Some(pane));
    assert_eq!(read_back.zoomed_pane(other_tab), Some(other_pane));
    assert_eq!(
        read_back.layout_mode(other_tab),
        LayoutMode::Fullscreen {
            focused: other_pane
        }
    );
    assert_eq!(read_back.scroll_offset(pane), 9);
    assert_eq!(read_back.selection(pane), Some(a_selection()));
    assert!(read_back.is_view_held(pane));
}

// --- registry ordering -------------------------------------------------

#[test]
fn list_attached_walks_clients_in_id_order() {
    let (one, two) = (ClientId::new(), ClientId::new());
    let (lower, higher) = (one.min(two), one.max(two));
    let mut registry = ClientRegistry::new();

    // Attached highest first; the registry still yields lowest first.
    registry.attach(a_client_with(higher, TabId::new()));
    registry.attach(a_client_with(lower, TabId::new()));

    let order: Vec<ClientId> = registry.list_attached().map(Client::id).collect();
    assert_eq!(order, vec![lower, higher]);

    let mutable_order: Vec<ClientId> = registry.list_attached_mut().map(|c| c.id()).collect();
    assert_eq!(mutable_order, vec![lower, higher]);
}

#[test]
fn get_mut_of_an_unattached_client_returns_nothing() {
    let mut registry = ClientRegistry::new();
    registry.attach(a_client(TabId::new()));

    assert!(registry.get_mut(ClientId::new()).is_none());
    assert_eq!(registry.len(), 1);
}

#[test]
fn detaching_one_of_two_clients_leaves_the_other_attached() {
    let (staying, leaving) = (ClientId::new(), ClientId::new());
    let mut registry = ClientRegistry::new();
    registry.attach(a_client_with(staying, TabId::new()));
    registry.attach(a_client_with(leaving, TabId::new()));

    let detached = registry.detach(leaving).expect("the client was attached");

    assert_eq!(detached.id(), leaving);
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.get(leaving).map(Client::id), None);
    assert_eq!(registry.get(staying).map(Client::id), Some(staying));
    assert_eq!(
        registry.list_attached().map(Client::id).collect::<Vec<_>>(),
        vec![staying]
    );
}
