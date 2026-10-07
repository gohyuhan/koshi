//! Client and ClientRegistry unit tests.
//!
//! Tests verify the server-set identity (origin, label, color), client
//! state tracking (focus, viewport, lock mode, zoom, scrollback view,
//! highlights, floating pane views) and registry operations (attach, detach,
//! lookup, mutation).

use std::collections::HashMap;
use std::time::SystemTime;

use koshi_core::command::{GridPosition, Selection, SelectionKind};
use koshi_core::geometry::{PaneArea, Point, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;

use super::{
    compute_default_pane_area_size, Client, ClientOrigin, ClientRegistry, FloatingPaneView,
};

/// A local test client with id `client_id` on an 80x24 viewport, viewing
/// `active_tab_id` and reporting `pane_area`.
fn build_test_client_from_parts(
    client_id: ClientId,
    active_tab_id: TabId,
    pane_area: Option<PaneArea>,
) -> Client {
    Client::from_attachment(
        client_id,
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        pane_area,
        active_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    )
}

/// [`build_test_client_from_parts`] with a fresh id, reporting `pane_area`.
fn build_test_client_with_pane_area(active_tab_id: TabId, pane_area: Option<PaneArea>) -> Client {
    build_test_client_from_parts(ClientId::new(), active_tab_id, pane_area)
}

/// [`build_test_client_from_parts`] with a fresh id, reporting no pane area.
fn build_test_client(active_tab_id: TabId) -> Client {
    build_test_client_from_parts(ClientId::new(), active_tab_id, None)
}

#[test]
fn a_client_keeps_the_origin_label_and_color_it_was_made_with() {
    for origin in [ClientOrigin::Local, ClientOrigin::Remote] {
        let client = Client::from_attachment(
            ClientId::new(),
            SessionId::new(),
            SystemTime::UNIX_EPOCH,
            Size {
                column_count: 80,
                row_count: 24,
            },
            None,
            TabId::new(),
            origin,
            "C-swift-otter".to_string(),
            3,
        );

        assert_eq!(client.get_origin(), origin);
        assert_eq!(client.get_label(), "C-swift-otter");
        assert_eq!(client.get_color_index(), 3);
    }
}

#[test]
fn a_client_carries_where_it_connected_from_across_a_serde_round_trip() {
    for (origin, expected_origin_wire_name) in [
        (ClientOrigin::Local, "Local"),
        (ClientOrigin::Remote, "Remote"),
    ] {
        let client_id = ClientId::new();
        let session_id = SessionId::new();
        let active_tab_id = TabId::new();
        let client = Client::from_attachment(
            client_id,
            session_id,
            SystemTime::UNIX_EPOCH,
            Size {
                column_count: 80,
                row_count: 24,
            },
            None,
            active_tab_id,
            origin,
            "C-swift-otter".to_string(),
            3,
        );

        let serialized_client_json = serde_json::to_string(&client).expect("the client encodes");
        let serialized_client_value: serde_json::Value =
            serde_json::from_str(&serialized_client_json).expect("the encoded JSON client is JSON");
        assert_eq!(
            serialized_client_value["origin"],
            serde_json::Value::String(expected_origin_wire_name.to_string())
        );
        assert_eq!(
            serialized_client_value.get("tier"),
            None,
            "a client record carries no authority key"
        );

        let decoded_client: Client =
            serde_json::from_str(&serialized_client_json).expect("the client decodes");
        assert_eq!(decoded_client.get_client_id(), client_id);
        assert_eq!(decoded_client.get_session_id(), session_id);
        assert_eq!(decoded_client.get_origin(), origin);
        assert_eq!(decoded_client.get_label(), "C-swift-otter");
        assert_eq!(decoded_client.get_color_index(), 3);
        assert_eq!(decoded_client.get_active_tab_id(), active_tab_id);
    }
}

#[test]
fn a_new_client_starts_unlocked_with_no_focus() {
    let tab_id = TabId::new();
    let client = build_test_client(tab_id);

    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert_eq!(client.get_active_tab_id(), tab_id);
    assert_eq!(client.get_focused_pane_id(tab_id), None);
}

#[test]
fn a_client_placement_revision_advances_once_and_refuses_wraparound() {
    let mut client = build_test_client(TabId::new());

    assert_eq!(client.get_placement_revision(), 0);
    assert!(client.can_advance_placement_revision());
    assert!(client.advance_placement_revision());
    assert_eq!(client.get_placement_revision(), 1);

    client.placement_revision = u64::MAX;
    assert!(!client.can_advance_placement_revision());
    assert!(!client.advance_placement_revision());
    assert_eq!(client.get_placement_revision(), u64::MAX);
}

#[test]
fn two_clients_focus_different_panes_in_the_same_tab() {
    let tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut first_client = build_test_client(tab_id);
    let mut second_client = build_test_client(tab_id);

    first_client.update_focused_pane(tab_id, first_pane_id);
    second_client.update_focused_pane(tab_id, second_pane_id);

    // Same tab, independent focus per client.
    assert_eq!(
        first_client.get_focused_pane_id(tab_id),
        Some(first_pane_id)
    );
    assert_eq!(
        second_client.get_focused_pane_id(tab_id),
        Some(second_pane_id)
    );
    assert_ne!(first_pane_id, second_pane_id);
}

#[test]
fn locking_one_client_leaves_another_unchanged() {
    let tab_id = TabId::new();
    let mut first_client = build_test_client(tab_id);
    let second_client = build_test_client(tab_id);

    first_client.update_lock_mode(LockMode::Locked);

    assert_eq!(first_client.get_lock_mode(), LockMode::Locked);
    assert_eq!(second_client.get_lock_mode(), LockMode::Normal);
}

#[test]
fn viewport_is_per_client() {
    let tab_id = TabId::new();
    let mut first_client = build_test_client(tab_id);
    let second_client = build_test_client(tab_id);

    first_client.update_viewport_size(Size {
        column_count: 120,
        row_count: 40,
    });

    assert_eq!(
        first_client.get_viewport_size(),
        Size {
            column_count: 120,
            row_count: 40
        }
    );
    assert_eq!(
        second_client.get_viewport_size(),
        Size {
            column_count: 80,
            row_count: 24
        }
    );
}

#[test]
fn focus_is_tracked_independently_per_tab() {
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(first_tab_id);

    client.update_focused_pane(first_tab_id, first_pane_id);
    client.update_active_tab_id(second_tab_id);
    client.update_focused_pane(second_tab_id, second_pane_id);
    // Switching back restores the focus held in the first tab.
    client.update_active_tab_id(first_tab_id);

    assert_eq!(client.get_active_tab_id(), first_tab_id);
    assert_eq!(
        client.get_focused_pane_id(first_tab_id),
        Some(first_pane_id)
    );
    assert_eq!(
        client.get_focused_pane_id(second_tab_id),
        Some(second_pane_id)
    );
}

#[test]
fn removing_a_tabs_focus_prunes_it() {
    let tab_id = TabId::new();
    let mut client = build_test_client(tab_id);
    client.update_focused_pane(tab_id, PaneId::new());

    client.remove_focused_pane(tab_id);

    assert_eq!(client.get_focused_pane_id(tab_id), None);
}

#[test]
fn updating_a_tabs_focus_returns_the_previous_pane() {
    let tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(tab_id);

    assert_eq!(client.update_focused_pane(tab_id, first_pane_id), None);
    assert_eq!(
        client.update_focused_pane(tab_id, second_pane_id),
        Some(first_pane_id)
    );
    assert_eq!(client.get_focused_pane_id(tab_id), Some(second_pane_id));
}

#[test]
fn focusing_another_pane_in_a_zoomed_tab_moves_the_zoom_to_it() {
    // Zoom follows focus: with a tab zoomed on one pane, focusing a different
    // pane there moves the zoom onto it. The tab stays zoomed.
    let tab_id = TabId::new();
    let (zoomed_pane_id, next_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(tab_id);
    client.update_focused_pane(tab_id, zoomed_pane_id);
    client.zoom_pane(tab_id, zoomed_pane_id);
    assert_eq!(client.get_zoomed_pane_id(tab_id), Some(zoomed_pane_id));

    let prior_pane_id = client.update_focused_pane(tab_id, next_pane_id);

    assert_eq!(prior_pane_id, Some(zoomed_pane_id));
    assert_eq!(client.get_zoomed_pane_id(tab_id), Some(next_pane_id));
    assert_eq!(client.get_focused_pane_id(tab_id), Some(next_pane_id));
    assert_eq!(
        client.get_layout_mode(tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: next_pane_id
        }
    );
}

#[test]
fn focusing_a_pane_in_a_tiled_tab_creates_no_zoom() {
    // Focusing a pane in a tab with no zoom leaves the tab tiled for this
    // client.
    let tab_id = TabId::new();
    let mut client = build_test_client(tab_id);

    client.update_focused_pane(tab_id, PaneId::new());

    assert_eq!(client.get_zoomed_pane_id(tab_id), None);
    assert_eq!(client.get_layout_mode(tab_id), LayoutMode::Tiled);
}

#[test]
fn removing_a_tabs_focus_also_drops_its_zoom() {
    // Forgetting the focused pane in a tab drops any zoom there too.
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut client = build_test_client(tab_id);
    client.update_focused_pane(tab_id, pane_id);
    client.zoom_pane(tab_id, pane_id);

    client.remove_focused_pane(tab_id);

    assert_eq!(client.get_focused_pane_id(tab_id), None);
    assert_eq!(client.get_zoomed_pane_id(tab_id), None);
    assert_eq!(client.get_layout_mode(tab_id), LayoutMode::Tiled);
}

#[test]
fn zoom_is_tracked_independently_per_client() {
    // Two clients on the same tab zoom independently: one zooming a pane leaves
    // the other's tiled view untouched.
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut first_client = build_test_client(tab_id);
    let second_client = build_test_client(tab_id);

    first_client.zoom_pane(tab_id, pane_id);

    assert_eq!(first_client.get_zoomed_pane_id(tab_id), Some(pane_id));
    assert_eq!(second_client.get_zoomed_pane_id(tab_id), None);
    assert_eq!(second_client.get_layout_mode(tab_id), LayoutMode::Tiled);
}

#[test]
fn focusing_a_pane_in_another_tab_leaves_a_zoom_where_it_is() {
    // Zoom follows focus only inside the tab being focused: focusing in
    // `other_tab_id` leaves the zoom in `zoomed_tab_id` where it is.
    let (zoomed_tab_id, other_tab_id) = (TabId::new(), TabId::new());
    let (zoomed_pane_id, other_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(zoomed_tab_id);
    client.update_focused_pane(zoomed_tab_id, zoomed_pane_id);
    client.zoom_pane(zoomed_tab_id, zoomed_pane_id);

    client.update_focused_pane(other_tab_id, other_pane_id);

    assert_eq!(
        client.get_zoomed_pane_id(zoomed_tab_id),
        Some(zoomed_pane_id)
    );
    assert_eq!(client.get_zoomed_pane_id(other_tab_id), None);
    assert_eq!(
        client.get_layout_mode(zoomed_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: zoomed_pane_id
        }
    );
    assert_eq!(client.get_layout_mode(other_tab_id), LayoutMode::Tiled);
}

#[test]
fn removing_the_focus_of_a_never_focused_tab_changes_nothing() {
    let (focused_tab_id, untouched_tab_id) = (TabId::new(), TabId::new());
    let pane_id = PaneId::new();
    let mut client = build_test_client(focused_tab_id);
    client.update_focused_pane(focused_tab_id, pane_id);
    client.zoom_pane(focused_tab_id, pane_id);

    client.remove_focused_pane(untouched_tab_id);

    assert_eq!(client.get_focused_pane_id(focused_tab_id), Some(pane_id));
    assert_eq!(client.get_zoomed_pane_id(focused_tab_id), Some(pane_id));
    assert_eq!(client.list_focused_pane_ids().len(), 1);
    assert_eq!(client.list_zoomed_pane_ids().len(), 1);
}

#[test]
fn a_new_registry_has_no_clients() {
    let client_registry = ClientRegistry::new();

    assert!(!client_registry.has_clients());
    assert_eq!(client_registry.count_clients(), 0);
    assert_eq!(client_registry.list_attached_clients().count(), 0);
}

#[test]
fn attaching_a_client_registers_it() {
    let mut client_registry = ClientRegistry::new();
    let client = build_test_client(TabId::new());
    let client_id = client.get_client_id();

    // A first attach displaces nothing.
    assert!(client_registry.attach_client(client).is_none());

    assert_eq!(client_registry.count_clients(), 1);
    assert!(client_registry.has_clients());
    assert_eq!(
        client_registry
            .get_client_by_id(client_id)
            .map(Client::get_client_id),
        Some(client_id)
    );
    assert_eq!(client_registry.list_attached_clients().count(), 1);
}

#[test]
fn detaching_a_client_removes_and_returns_it() {
    let mut client_registry = ClientRegistry::new();
    let client = build_test_client(TabId::new());
    let client_id = client.get_client_id();
    client_registry.attach_client(client);

    let detached_client = client_registry.detach_client(client_id);

    assert_eq!(
        detached_client.map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert!(client_registry.get_client_by_id(client_id).is_none());
    assert!(!client_registry.has_clients());
}

#[test]
fn detaching_an_unattached_client_returns_nothing() {
    let mut client_registry = ClientRegistry::new();

    assert!(client_registry.detach_client(ClientId::new()).is_none());
}

#[test]
fn update_attached_client_in_place_through_registry() {
    let mut client_registry = ClientRegistry::new();
    let client = build_test_client(TabId::new());
    let client_id = client.get_client_id();
    client_registry.attach_client(client);

    client_registry
        .get_client_mut_by_id(client_id)
        .expect("attached client")
        .update_lock_mode(LockMode::Locked);

    // The edit is visible through the registry.
    assert_eq!(
        client_registry
            .get_client_by_id(client_id)
            .map(Client::get_lock_mode),
        Some(LockMode::Locked)
    );
}

#[test]
fn re_attaching_the_same_id_replaces_and_returns_the_prior() {
    let mut client_registry = ClientRegistry::new();
    let client_id = ClientId::new();
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());

    assert!(client_registry
        .attach_client(build_test_client_from_parts(client_id, first_tab_id, None))
        .is_none());
    let replaced_client =
        client_registry.attach_client(build_test_client_from_parts(client_id, second_tab_id, None));

    // The prior record comes back; the registry holds exactly the new one.
    assert_eq!(
        replaced_client.map(|client| client.get_active_tab_id()),
        Some(first_tab_id)
    );
    assert_eq!(client_registry.count_clients(), 1);
    assert_eq!(
        client_registry
            .get_client_by_id(client_id)
            .map(Client::get_active_tab_id),
        Some(second_tab_id)
    );
}

/// A highlight, whose shape does not matter to the view rules under test.
fn build_test_selection() -> Selection {
    Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 4,
        },
    }
}

#[test]
fn scroll_offset_defaults_to_zero_for_an_unscrolled_pane() {
    let client = build_test_client(TabId::new());
    assert_eq!(client.get_scroll_offset(PaneId::new()), 0);
}

#[test]
fn set_scroll_offset_records_and_reads_back_per_pane() {
    let mut client = build_test_client(TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());

    client.set_scroll_offset(first_pane_id, 7);
    // Panes scroll independently; the second is untouched.
    assert_eq!(client.get_scroll_offset(first_pane_id), 7);
    assert_eq!(client.get_scroll_offset(second_pane_id), 0);
}

#[test]
fn set_scroll_offset_zero_clears_the_entry() {
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();

    client.set_scroll_offset(pane_id, 3);
    client.set_scroll_offset(pane_id, 0);
    assert_eq!(client.get_scroll_offset(pane_id), 0);
    assert_eq!(client.list_scroll_offsets().get(&pane_id), None);
}

#[test]
fn set_scroll_offset_zero_on_an_unscrolled_pane_adds_no_entry() {
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();

    client.set_scroll_offset(pane_id, 0);

    assert_eq!(client.get_scroll_offset(pane_id), 0);
    assert_eq!(client.list_scroll_offsets().len(), 0);
    assert!(!client.is_view_held(pane_id));
}

#[test]
fn list_attached_clients_mut_reaches_every_client() {
    let mut client_registry = ClientRegistry::new();
    let pane_id = PaneId::new();
    client_registry.attach_client(build_test_client(TabId::new()));
    client_registry.attach_client(build_test_client(TabId::new()));

    for client in client_registry.list_attached_clients_mut() {
        client.set_scroll_offset(pane_id, 4);
    }
    let scroll_offsets: Vec<usize> = client_registry
        .list_attached_clients()
        .map(|client| client.get_scroll_offset(pane_id))
        .collect();
    assert_eq!(scroll_offsets, vec![4, 4]);
}

// --- is_view_held: the two reasons a view is held ----------------------

#[test]
fn a_view_at_the_bottom_with_no_highlight_is_not_held() {
    let client = build_test_client(TabId::new());
    assert!(!client.is_view_held(PaneId::new()));
}

#[test]
fn a_scrolled_up_view_is_held() {
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();

    client.set_scroll_offset(pane_id, 1); // one line up is enough
    assert!(client.is_view_held(pane_id));
}

#[test]
fn a_highlight_holds_a_view_sitting_at_the_bottom() {
    // The state an offset alone cannot express: at the newest line and held.
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();

    client.set_selection(pane_id, build_test_selection());
    assert_eq!(client.get_scroll_offset(pane_id), 0);
    assert!(client.is_view_held(pane_id));
}

#[test]
fn a_highlight_holds_its_view_no_matter_where_it_is_scrolled() {
    // Both reasons at once: still held, and scrolling back to the bottom does not
    // release it while the highlight is up.
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();
    client.set_selection(pane_id, build_test_selection());

    client.set_scroll_offset(pane_id, 5);
    assert!(client.is_view_held(pane_id));

    client.set_scroll_offset(pane_id, 0); // scrolled back to the newest line
    assert!(client.is_view_held(pane_id));
}

#[test]
fn clearing_a_highlight_at_the_bottom_releases_the_view() {
    // The highlight is the only thing holding the view. Clearing it releases
    // the view.
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();
    client.set_selection(pane_id, build_test_selection());
    assert!(client.is_view_held(pane_id));

    client.clear_selection(pane_id);
    assert!(!client.is_view_held(pane_id));
}

#[test]
fn clearing_a_highlight_leaves_a_scrolled_up_view_held() {
    // The view is still 3 lines up: it stays held until it is scrolled back to
    // the bottom.
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();
    client.set_selection(pane_id, build_test_selection());
    client.set_scroll_offset(pane_id, 3);

    client.clear_selection(pane_id);
    assert!(client.is_view_held(pane_id));

    client.set_scroll_offset(pane_id, 0);
    assert!(!client.is_view_held(pane_id));
}

#[test]
fn a_highlight_holds_only_its_own_pane() {
    let mut client = build_test_client(TabId::new());
    let (highlighted_pane_id, other_pane_id) = (PaneId::new(), PaneId::new());

    client.set_selection(highlighted_pane_id, build_test_selection());
    assert!(client.is_view_held(highlighted_pane_id));
    assert!(!client.is_view_held(other_pane_id));
}

#[test]
fn highlighting_a_second_pane_leaves_the_first_panes_highlight_alone() {
    // Each pane keeps its own highlight: starting one in `second_pane_id`
    // leaves the one in `first_pane_id` up, and both views stay held.
    let mut client = build_test_client(TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    client.set_selection(first_pane_id, build_test_selection());

    client.set_selection(second_pane_id, build_test_selection());

    assert_eq!(
        client.get_selection(first_pane_id),
        Some(build_test_selection())
    );
    assert_eq!(
        client.get_selection(second_pane_id),
        Some(build_test_selection())
    );
    assert!(client.is_view_held(first_pane_id));
    assert!(client.is_view_held(second_pane_id));
}

#[test]
fn selection_reads_back_per_pane() {
    let mut client = build_test_client(TabId::new());
    let (pane_id, other_pane_id) = (PaneId::new(), PaneId::new());
    assert_eq!(client.get_selection(pane_id), None);

    client.set_selection(pane_id, build_test_selection());
    assert_eq!(client.get_selection(pane_id), Some(build_test_selection()));
    assert_eq!(client.get_selection(other_pane_id), None);
}

#[test]
fn setting_a_highlight_twice_in_one_pane_replaces_it() {
    // A drag re-issues the highlight as it grows; the pane holds the latest.
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();
    client.set_selection(pane_id, build_test_selection());

    let grown_selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 2,
            column_index: 7,
        },
    };
    client.set_selection(pane_id, grown_selection);
    assert_eq!(client.get_selection(pane_id), Some(grown_selection));
}

#[test]
fn clear_selection_drops_only_that_panes_highlight() {
    let mut client = build_test_client(TabId::new());
    let (pane_id, other_pane_id) = (PaneId::new(), PaneId::new());
    client.set_selection(pane_id, build_test_selection());
    client.set_selection(other_pane_id, build_test_selection());

    client.clear_selection(other_pane_id);

    assert_eq!(client.get_selection(pane_id), Some(build_test_selection()));
    assert!(client.is_view_held(pane_id));
    assert_eq!(client.get_selection(other_pane_id), None);
    assert!(!client.is_view_held(other_pane_id));
}

#[test]
fn clearing_a_pane_with_no_highlight_changes_nothing() {
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();

    client.clear_selection(pane_id);
    assert_eq!(client.get_selection(pane_id), None);
    assert!(!client.is_view_held(pane_id));
}

#[test]
fn one_clients_highlight_leaves_another_viewing_the_same_pane_alone() {
    // The highlight is per client: one client selecting in a pane leaves the
    // other client's view of that pane unheld.
    let mut client_registry = ClientRegistry::new();
    let pane_id = PaneId::new();
    let (first_client, second_client) = (
        build_test_client(TabId::new()),
        build_test_client(TabId::new()),
    );
    let (first_client_id, second_client_id) =
        (first_client.get_client_id(), second_client.get_client_id());
    client_registry.attach_client(first_client);
    client_registry.attach_client(second_client);

    client_registry
        .get_client_mut_by_id(first_client_id)
        .expect("the client was just attached")
        .set_selection(pane_id, build_test_selection());

    let attached_first_client = client_registry
        .get_client_by_id(first_client_id)
        .expect("attached");
    assert_eq!(
        attached_first_client.get_selection(pane_id),
        Some(build_test_selection())
    );
    assert!(attached_first_client.is_view_held(pane_id));

    let attached_second_client = client_registry
        .get_client_by_id(second_client_id)
        .expect("attached");
    assert_eq!(attached_second_client.get_selection(pane_id), None);
    assert!(!attached_second_client.is_view_held(pane_id));
}

// --- compute_default_pane_area_size -----------------------------------

#[test]
fn default_pane_area_size_reserves_the_tabline_and_hint_row() {
    // 80x24 minus one tabline row and one hint row leaves 80x22.
    assert_eq!(
        compute_default_pane_area_size(Size {
            column_count: 80,
            row_count: 24
        }),
        Size {
            column_count: 80,
            row_count: 22
        }
    );
}

#[test]
fn default_pane_area_size_of_a_two_row_viewport_is_exactly_zero_rows() {
    // Exactly enough for the tabline and hint rows and nothing else:
    // 2 - 2 = 0, the boundary just above the saturating case below.
    assert_eq!(
        compute_default_pane_area_size(Size {
            column_count: 80,
            row_count: 2
        }),
        Size {
            column_count: 80,
            row_count: 0
        }
    );
}

#[test]
fn default_pane_area_size_of_a_one_row_viewport_saturates_to_zero_rows() {
    // Fewer rows than the tabline and hint rows: the row count saturates at 0.
    assert_eq!(
        compute_default_pane_area_size(Size {
            column_count: 80,
            row_count: 1
        }),
        Size {
            column_count: 80,
            row_count: 0
        }
    );
}

#[test]
fn default_pane_area_size_of_a_zero_row_viewport_stays_zero_rows() {
    assert_eq!(
        compute_default_pane_area_size(Size {
            column_count: 80,
            row_count: 0
        }),
        Size {
            column_count: 80,
            row_count: 0
        }
    );
}

#[test]
fn default_pane_area_size_never_touches_the_column_count() {
    assert_eq!(
        compute_default_pane_area_size(Size {
            column_count: 0,
            row_count: 24
        }),
        Size {
            column_count: 0,
            row_count: 22
        }
    );
}

// --- pane_area ---------------------------------------------------------

#[test]
fn a_client_that_reported_no_pane_area_sizes_as_its_viewport_minus_two_rows() {
    let client = build_test_client_with_pane_area(TabId::new(), None);

    assert_eq!(
        client.get_pane_area(),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
    assert_eq!(
        client.get_pane_area(),
        Some(compute_default_pane_area_size(client.get_viewport_size()))
    );
}

#[test]
fn a_reported_pane_area_is_clamped_to_the_viewport_per_axis() {
    // The viewport is 80x24 in every case.
    let wider_and_taller_client = build_test_client_with_pane_area(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            column_count: 200,
            row_count: 50,
        })),
    );
    assert_eq!(
        wider_and_taller_client.get_pane_area(),
        Some(Size {
            column_count: 80,
            row_count: 24
        })
    );

    let inside_client = build_test_client_with_pane_area(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            column_count: 40,
            row_count: 10,
        })),
    );
    assert_eq!(
        inside_client.get_pane_area(),
        Some(Size {
            column_count: 40,
            row_count: 10
        })
    );

    let wider_only_client = build_test_client_with_pane_area(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            column_count: 100,
            row_count: 10,
        })),
    );
    assert_eq!(
        wider_only_client.get_pane_area(),
        Some(Size {
            column_count: 80,
            row_count: 10
        })
    );
}

#[test]
fn a_viewport_with_no_room_for_the_tabline_and_hint_rows_gives_a_zero_row_pane_area() {
    let client = Client::from_attachment(
        ClientId::new(),
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 1,
        },
        None,
        TabId::new(),
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );

    assert_eq!(
        client.get_pane_area(),
        Some(Size {
            column_count: 80,
            row_count: 0
        })
    );
}

#[test]
fn shrinking_the_viewport_reclamps_a_reported_pane_area() {
    let mut client = build_test_client_with_pane_area(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 24,
        })),
    );
    assert_eq!(
        client.get_pane_area(),
        Some(Size {
            column_count: 80,
            row_count: 24
        })
    );

    client.update_viewport_size(Size {
        column_count: 40,
        row_count: 10,
    });

    // The report is kept as reported; only the clamp against the new viewport
    // moves.
    assert_eq!(
        client.get_reported_pane_area(),
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 24
        }))
    );
    assert_eq!(
        client.get_pane_area(),
        Some(Size {
            column_count: 40,
            row_count: 10
        })
    );
}

#[test]
fn a_reported_pane_area_equal_to_the_viewport_is_not_reduced() {
    let client = build_test_client_with_pane_area(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 24,
        })),
    );

    // The report stands as given: no tabline or hint row is taken off it.
    assert_eq!(
        client.get_pane_area(),
        Some(Size {
            column_count: 80,
            row_count: 24
        })
    );
}

#[test]
fn a_reported_pane_area_at_the_maximum_size_is_clamped_to_the_viewport() {
    let client = build_test_client_with_pane_area(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            column_count: u16::MAX,
            row_count: u16::MAX,
        })),
    );

    assert_eq!(
        client.get_pane_area(),
        Some(Size {
            column_count: 80,
            row_count: 24
        })
    );
}

#[test]
fn a_starving_client_has_no_pane_area() {
    let client = build_test_client_with_pane_area(TabId::new(), Some(PaneArea::Starving));

    assert_eq!(client.get_pane_area(), None);
}

#[test]
fn reported_pane_area_returns_the_raw_report() {
    let reported_pane_area = PaneArea::Reported(Size {
        column_count: 40,
        row_count: 10,
    });

    assert_eq!(
        build_test_client_with_pane_area(TabId::new(), None).get_reported_pane_area(),
        None
    );
    assert_eq!(
        build_test_client_with_pane_area(TabId::new(), Some(reported_pane_area))
            .get_reported_pane_area(),
        Some(reported_pane_area)
    );
    assert_eq!(
        build_test_client_with_pane_area(TabId::new(), Some(PaneArea::Starving))
            .get_reported_pane_area(),
        Some(PaneArea::Starving)
    );
}

#[test]
fn update_pane_area_replaces_a_report_with_none() {
    let mut client = build_test_client_with_pane_area(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            column_count: 40,
            row_count: 10,
        })),
    );
    assert_eq!(
        client.get_pane_area(),
        Some(Size {
            column_count: 40,
            row_count: 10
        })
    );

    client.update_pane_area(None);

    assert_eq!(client.get_reported_pane_area(), None);
    assert_eq!(
        client.get_pane_area(),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
}

#[test]
fn a_client_json_without_pane_area_decodes_as_none() {
    let client = build_test_client_with_pane_area(TabId::new(), Some(PaneArea::Starving));
    let mut encoded_client_json: serde_json::Value =
        serde_json::to_value(&client).expect("the client encodes");
    assert!(
        encoded_client_json
            .as_object_mut()
            .expect("a client encodes as a json object")
            .remove("pane_area")
            .is_some(),
        "the encoded JSON client carries a pane_area key"
    );

    let decoded_client: Client =
        serde_json::from_value(encoded_client_json).expect("the client decodes");

    assert_eq!(decoded_client.get_reported_pane_area(), None);
    assert_eq!(
        decoded_client.get_pane_area(),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
}

#[test]
fn a_client_reported_pane_area_survives_a_serde_round_trip() {
    for reported_pane_area in [
        PaneArea::Starving,
        PaneArea::Reported(Size {
            column_count: 40,
            row_count: 10,
        }),
    ] {
        let client = build_test_client_with_pane_area(TabId::new(), Some(reported_pane_area));

        let serialized_client_json = serde_json::to_string(&client).expect("the client encodes");
        let decoded_client: Client =
            serde_json::from_str(&serialized_client_json).expect("the client decodes");

        assert_eq!(
            decoded_client.get_reported_pane_area(),
            Some(reported_pane_area)
        );
    }
}

#[test]
fn a_reported_pane_area_of_zero_stays_zero() {
    let client = build_test_client_with_pane_area(
        TabId::new(),
        Some(PaneArea::Reported(Size {
            column_count: 0,
            row_count: 0,
        })),
    );

    assert_eq!(
        client.get_pane_area(),
        Some(Size {
            column_count: 0,
            row_count: 0
        })
    );
}

#[test]
fn default_pane_area_size_of_the_tallest_viewport_reserves_two_rows() {
    assert_eq!(
        compute_default_pane_area_size(Size {
            column_count: u16::MAX,
            row_count: u16::MAX
        }),
        Size {
            column_count: u16::MAX,
            row_count: u16::MAX - 2
        }
    );
}

// --- mouse select ------------------------------------------------------

#[test]
fn mouse_select_starts_off_and_each_toggle_flips_it() {
    let mut client = build_test_client(TabId::new());
    assert!(!client.is_mouse_selection_enabled());

    assert!(client.toggle_mouse_selection());
    assert!(client.is_mouse_selection_enabled());

    assert!(!client.toggle_mouse_selection());
    assert!(!client.is_mouse_selection_enabled());
}

#[test]
fn mouse_select_and_lock_mode_do_not_touch_each_other() {
    let mut client = build_test_client(TabId::new());

    client.toggle_mouse_selection();
    assert_eq!(client.get_lock_mode(), LockMode::Normal);

    client.update_lock_mode(LockMode::Locked);
    assert!(client.is_mouse_selection_enabled());
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
}

#[test]
fn mouse_select_is_per_client() {
    let tab_id = TabId::new();
    let mut first_client = build_test_client(tab_id);
    let second_client = build_test_client(tab_id);

    first_client.toggle_mouse_selection();

    assert!(first_client.is_mouse_selection_enabled());
    assert!(!second_client.is_mouse_selection_enabled());
}

// --- zoom bookkeeping --------------------------------------------------

#[test]
fn zooming_a_second_pane_in_one_tab_replaces_the_zoom() {
    let tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(tab_id);

    client.zoom_pane(tab_id, first_pane_id);
    client.zoom_pane(tab_id, second_pane_id);

    assert_eq!(client.get_zoomed_pane_id(tab_id), Some(second_pane_id));
    assert_eq!(
        client.get_layout_mode(tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: second_pane_id
        }
    );
}

#[test]
fn clear_zoom_drops_only_that_tabs_zoom() {
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(first_tab_id);
    client.zoom_pane(first_tab_id, first_pane_id);
    client.zoom_pane(second_tab_id, second_pane_id);

    client.clear_zoom(first_tab_id);

    assert_eq!(client.get_zoomed_pane_id(first_tab_id), None);
    assert_eq!(client.get_layout_mode(first_tab_id), LayoutMode::Tiled);
    assert_eq!(
        client.get_zoomed_pane_id(second_tab_id),
        Some(second_pane_id)
    );
}

#[test]
fn clearing_the_zoom_of_a_tiled_tab_changes_nothing() {
    let tab_id = TabId::new();
    let mut client = build_test_client(tab_id);

    client.clear_zoom(tab_id);

    assert_eq!(client.get_zoomed_pane_id(tab_id), None);
    assert_eq!(client.list_zoomed_pane_ids().len(), 0);
}

#[test]
fn clear_zoom_of_pane_drops_that_pane_in_every_tab_and_leaves_the_rest() {
    // The same pane zoomed in two tabs goes from both; a tab zoomed on another
    // pane keeps its zoom.
    let (first_tab_id, second_tab_id, third_tab_id) = (TabId::new(), TabId::new(), TabId::new());
    let (removed_pane_id, kept_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(first_tab_id);
    client.zoom_pane(first_tab_id, removed_pane_id);
    client.zoom_pane(second_tab_id, removed_pane_id);
    client.zoom_pane(third_tab_id, kept_pane_id);

    client.clear_zoom_of_pane(removed_pane_id);

    assert_eq!(client.get_zoomed_pane_id(first_tab_id), None);
    assert_eq!(client.get_zoomed_pane_id(second_tab_id), None);
    assert_eq!(client.get_zoomed_pane_id(third_tab_id), Some(kept_pane_id));
    assert_eq!(client.list_zoomed_pane_ids().len(), 1);
}

#[test]
fn clear_zoom_of_a_pane_no_tab_is_zoomed_on_changes_nothing() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut client = build_test_client(tab_id);
    client.zoom_pane(tab_id, pane_id);

    client.clear_zoom_of_pane(PaneId::new());

    assert_eq!(client.get_zoomed_pane_id(tab_id), Some(pane_id));
    assert_eq!(client.list_zoomed_pane_ids().len(), 1);
}

#[test]
fn list_zoomed_pane_ids_lists_every_zoom_keyed_by_tab() {
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(first_tab_id);

    client.zoom_pane(first_tab_id, first_pane_id);
    client.zoom_pane(second_tab_id, second_pane_id);

    let zoomed_pane_id_by_tab_id = client.list_zoomed_pane_ids();
    assert_eq!(zoomed_pane_id_by_tab_id.len(), 2);
    assert_eq!(
        zoomed_pane_id_by_tab_id.get(&first_tab_id),
        Some(&first_pane_id)
    );
    assert_eq!(
        zoomed_pane_id_by_tab_id.get(&second_tab_id),
        Some(&second_pane_id)
    );
}

#[test]
fn a_tab_the_client_has_never_seen_is_tiled() {
    let client = build_test_client(TabId::new());

    assert_eq!(client.get_layout_mode(TabId::new()), LayoutMode::Tiled);
    assert_eq!(client.get_zoomed_pane_id(TabId::new()), None);
}

// --- focus and scroll map views ----------------------------------------

#[test]
fn list_focused_pane_ids_lists_every_remembered_focus_keyed_by_tab() {
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(first_tab_id);

    client.update_focused_pane(first_tab_id, first_pane_id);
    client.update_focused_pane(second_tab_id, second_pane_id);

    let focused_pane_id_by_tab_id = client.list_focused_pane_ids();
    assert_eq!(focused_pane_id_by_tab_id.len(), 2);
    assert_eq!(
        focused_pane_id_by_tab_id.get(&first_tab_id),
        Some(&first_pane_id)
    );
    assert_eq!(
        focused_pane_id_by_tab_id.get(&second_tab_id),
        Some(&second_pane_id)
    );

    client.remove_focused_pane(first_tab_id);
    assert_eq!(client.list_focused_pane_ids().len(), 1);
    assert_eq!(
        client.list_focused_pane_ids().get(&second_tab_id),
        Some(&second_pane_id)
    );
}

#[test]
fn list_scroll_offsets_holds_only_the_scrolled_up_panes() {
    let mut client = build_test_client(TabId::new());
    let (scrolled_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new());

    client.set_scroll_offset(scrolled_pane_id, 5);
    client.set_scroll_offset(bottom_pane_id, 0);

    let scroll_offset_by_pane_id = client.list_scroll_offsets();
    assert_eq!(scroll_offset_by_pane_id.len(), 1);
    assert_eq!(scroll_offset_by_pane_id.get(&scrolled_pane_id), Some(&5));
    assert_eq!(scroll_offset_by_pane_id.get(&bottom_pane_id), None);
}

#[test]
fn set_scroll_offset_keeps_the_largest_offset() {
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();

    client.set_scroll_offset(pane_id, usize::MAX);

    assert_eq!(client.get_scroll_offset(pane_id), usize::MAX);
    assert!(client.is_view_held(pane_id));
}

#[test]
fn switching_tabs_keeps_every_highlight_and_scroll_position() {
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let pane_id = PaneId::new();
    let mut client = build_test_client(first_tab_id);
    client.set_selection(pane_id, build_test_selection());
    client.set_scroll_offset(pane_id, 4);

    client.update_active_tab_id(second_tab_id);
    client.update_active_tab_id(first_tab_id);

    assert_eq!(client.get_active_tab_id(), first_tab_id);
    assert_eq!(client.get_selection(pane_id), Some(build_test_selection()));
    assert_eq!(client.get_scroll_offset(pane_id), 4);
}

// --- identity ----------------------------------------------------------

#[test]
fn a_client_reads_back_the_session_and_attach_time_it_was_made_with() {
    let session_id = SessionId::new();
    let attached_at = SystemTime::UNIX_EPOCH;
    let client = Client::from_attachment(
        ClientId::new(),
        session_id,
        attached_at,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        TabId::new(),
        ClientOrigin::Local,
        "C-swift-otter".to_string(),
        3,
    );

    assert_eq!(client.get_session_id(), session_id);
    assert_eq!(client.get_attached_at(), attached_at);
}

#[test]
fn update_origin_replaces_where_the_client_connected_from() {
    let mut client = build_test_client(TabId::new());
    assert_eq!(client.get_origin(), ClientOrigin::Local);

    client.update_origin(ClientOrigin::Remote);
    assert_eq!(client.get_origin(), ClientOrigin::Remote);

    client.update_origin(ClientOrigin::Local);
    assert_eq!(client.get_origin(), ClientOrigin::Local);
}

#[test]
fn a_clients_whole_view_state_survives_a_serde_round_trip() {
    let (tab_id, other_tab_id) = (TabId::new(), TabId::new());
    let (pane_id, other_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_test_client(tab_id);
    client.update_lock_mode(LockMode::Locked);
    client.toggle_mouse_selection();
    client.update_focused_pane(tab_id, pane_id);
    client.zoom_pane(other_tab_id, other_pane_id);
    client.set_scroll_offset(pane_id, 9);
    client.set_selection(pane_id, build_test_selection());
    assert!(client.advance_placement_revision());

    let serialized_client_json = serde_json::to_string(&client).expect("the client encodes");
    let decoded_client: Client =
        serde_json::from_str(&serialized_client_json).expect("the client decodes");

    assert_eq!(decoded_client.get_lock_mode(), LockMode::Locked);
    assert!(decoded_client.is_mouse_selection_enabled());
    assert_eq!(decoded_client.get_focused_pane_id(tab_id), Some(pane_id));
    assert_eq!(
        decoded_client.get_zoomed_pane_id(other_tab_id),
        Some(other_pane_id)
    );
    assert_eq!(decoded_client.get_placement_revision(), 1);
    assert_eq!(
        decoded_client.get_layout_mode(other_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: other_pane_id
        }
    );
    assert_eq!(decoded_client.get_scroll_offset(pane_id), 9);
    assert_eq!(
        decoded_client.get_selection(pane_id),
        Some(build_test_selection())
    );
    assert!(decoded_client.is_view_held(pane_id));
}

// --- registry ordering -------------------------------------------------

#[test]
fn list_attached_clients_walks_clients_in_id_order() {
    let (first_client_id, second_client_id) = (ClientId::new(), ClientId::new());
    let (lower_client_id, higher_client_id) = (
        first_client_id.min(second_client_id),
        first_client_id.max(second_client_id),
    );
    let mut client_registry = ClientRegistry::new();

    // Attached highest first; the registry still yields lowest first.
    client_registry.attach_client(build_test_client_from_parts(
        higher_client_id,
        TabId::new(),
        None,
    ));
    client_registry.attach_client(build_test_client_from_parts(
        lower_client_id,
        TabId::new(),
        None,
    ));

    let ordered_client_ids: Vec<ClientId> = client_registry
        .list_attached_clients()
        .map(Client::get_client_id)
        .collect();
    assert_eq!(ordered_client_ids, vec![lower_client_id, higher_client_id]);

    let ordered_mutable_client_ids: Vec<ClientId> = client_registry
        .list_attached_clients_mut()
        .map(|client| client.get_client_id())
        .collect();
    assert_eq!(
        ordered_mutable_client_ids,
        vec![lower_client_id, higher_client_id]
    );
}

#[test]
fn editing_an_unattached_client_returns_nothing() {
    let mut client_registry = ClientRegistry::new();
    client_registry.attach_client(build_test_client(TabId::new()));

    assert!(client_registry
        .get_client_mut_by_id(ClientId::new())
        .is_none());
    assert_eq!(client_registry.count_clients(), 1);
}

#[test]
fn detaching_one_of_two_clients_leaves_the_other_attached() {
    let (staying_client_id, leaving_client_id) = (ClientId::new(), ClientId::new());
    let mut client_registry = ClientRegistry::new();
    client_registry.attach_client(build_test_client_from_parts(
        staying_client_id,
        TabId::new(),
        None,
    ));
    client_registry.attach_client(build_test_client_from_parts(
        leaving_client_id,
        TabId::new(),
        None,
    ));

    let detached_client = client_registry
        .detach_client(leaving_client_id)
        .expect("the client was attached");

    assert_eq!(detached_client.get_client_id(), leaving_client_id);
    assert_eq!(client_registry.count_clients(), 1);
    assert_eq!(
        client_registry
            .get_client_by_id(leaving_client_id)
            .map(Client::get_client_id),
        None
    );
    assert_eq!(
        client_registry
            .get_client_by_id(staying_client_id)
            .map(Client::get_client_id),
        Some(staying_client_id)
    );
    assert_eq!(
        client_registry
            .list_attached_clients()
            .map(Client::get_client_id)
            .collect::<Vec<ClientId>>(),
        vec![staying_client_id]
    );
}

#[test]
fn focusing_a_floating_pane_moves_exactly_that_pane_to_the_top() {
    let mut client = build_test_client(TabId::new());
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());

    for pane_id in [first_pane_id, second_pane_id, third_pane_id] {
        assert!(client.focus_floating_pane(pane_id));
    }

    assert_eq!(
        client.list_floating_pane_focus_order(),
        [first_pane_id, second_pane_id, third_pane_id]
    );
    assert_eq!(client.get_focused_floating_pane_id(), Some(third_pane_id));

    assert!(client.focus_floating_pane(second_pane_id));

    assert_eq!(
        client.list_floating_pane_focus_order(),
        [first_pane_id, third_pane_id, second_pane_id]
    );
    assert_eq!(client.get_focused_floating_pane_id(), Some(second_pane_id));
}

#[test]
fn pinning_a_floating_pane_keeps_the_focus_order_and_unpinning_drops_its_stored_view() {
    let mut client = build_test_client(TabId::new());
    let (pinned_pane_id, focused_pane_id) = (PaneId::new(), PaneId::new());
    assert!(client.focus_floating_pane(pinned_pane_id));
    assert!(client.focus_floating_pane(focused_pane_id));

    client.set_floating_pane_pinned(pinned_pane_id, true);

    assert_eq!(
        client.list_floating_pane_focus_order(),
        [pinned_pane_id, focused_pane_id]
    );
    assert_eq!(client.get_focused_floating_pane_id(), Some(focused_pane_id));
    assert_eq!(
        client.get_floating_pane_view(pinned_pane_id),
        FloatingPaneView {
            placement: None,
            is_pinned: true,
            is_minimized: false,
        }
    );

    client.set_floating_pane_pinned(pinned_pane_id, false);

    assert_eq!(client.list_floating_pane_views(), &HashMap::new());
    assert_eq!(
        client.list_floating_pane_focus_order(),
        [pinned_pane_id, focused_pane_id]
    );
}

#[test]
fn minimizing_the_focused_floating_pane_clears_the_focus_and_keeps_the_order() {
    let mut client = build_test_client(TabId::new());
    let (lower_pane_id, top_pane_id) = (PaneId::new(), PaneId::new());
    assert!(client.focus_floating_pane(lower_pane_id));
    assert!(client.focus_floating_pane(top_pane_id));

    client.minimize_floating_pane(top_pane_id);

    assert_eq!(
        client.list_floating_pane_focus_order(),
        [lower_pane_id, top_pane_id]
    );
    assert_eq!(client.get_focused_floating_pane_id(), None);
    assert_eq!(
        client.get_floating_pane_view(top_pane_id),
        FloatingPaneView {
            placement: None,
            is_pinned: false,
            is_minimized: true,
        }
    );
}

#[test]
fn minimizing_an_unfocused_floating_pane_keeps_the_focus_and_the_order() {
    let mut client = build_test_client(TabId::new());
    let (lower_pane_id, top_pane_id) = (PaneId::new(), PaneId::new());
    assert!(client.focus_floating_pane(lower_pane_id));
    assert!(client.focus_floating_pane(top_pane_id));

    client.minimize_floating_pane(lower_pane_id);

    assert_eq!(
        client.list_floating_pane_focus_order(),
        [lower_pane_id, top_pane_id]
    );
    assert_eq!(client.get_focused_floating_pane_id(), Some(top_pane_id));
}

#[test]
fn focusing_a_minimized_floating_pane_is_refused_and_changes_nothing() {
    let mut client = build_test_client(TabId::new());
    let (focused_pane_id, minimized_pane_id) = (PaneId::new(), PaneId::new());
    assert!(client.focus_floating_pane(focused_pane_id));
    client.minimize_floating_pane(minimized_pane_id);

    assert!(!client.focus_floating_pane(minimized_pane_id));

    assert_eq!(client.list_floating_pane_focus_order(), [focused_pane_id]);
    assert_eq!(client.get_focused_floating_pane_id(), Some(focused_pane_id));
    assert_eq!(
        client.list_floating_pane_views(),
        &HashMap::from([(
            minimized_pane_id,
            FloatingPaneView {
                placement: None,
                is_pinned: false,
                is_minimized: true,
            },
        )])
    );
}

#[test]
fn restoring_a_minimized_floating_pane_focuses_it_and_puts_it_on_top() {
    let mut client = build_test_client(TabId::new());
    let (restored_pane_id, focused_pane_id) = (PaneId::new(), PaneId::new());
    assert!(client.focus_floating_pane(restored_pane_id));
    assert!(client.focus_floating_pane(focused_pane_id));
    client.minimize_floating_pane(restored_pane_id);

    client.restore_floating_pane(restored_pane_id);

    assert_eq!(
        client.list_floating_pane_focus_order(),
        [focused_pane_id, restored_pane_id]
    );
    assert_eq!(
        client.get_focused_floating_pane_id(),
        Some(restored_pane_id)
    );
    assert_eq!(client.list_floating_pane_views(), &HashMap::new());
}

#[test]
fn restoring_a_floating_pane_this_client_never_focused_appends_and_focuses_it() {
    let mut client = build_test_client(TabId::new());
    let (focused_pane_id, never_focused_pane_id) = (PaneId::new(), PaneId::new());
    assert!(client.focus_floating_pane(focused_pane_id));
    client.minimize_floating_pane(never_focused_pane_id);
    assert_eq!(client.list_floating_pane_focus_order(), [focused_pane_id]);

    client.restore_floating_pane(never_focused_pane_id);

    assert_eq!(
        client.list_floating_pane_focus_order(),
        [focused_pane_id, never_focused_pane_id]
    );
    assert_eq!(
        client.get_focused_floating_pane_id(),
        Some(never_focused_pane_id)
    );
    assert_eq!(client.list_floating_pane_views(), &HashMap::new());
}

#[test]
fn restoring_a_floating_pane_keeps_its_placement_and_its_pin() {
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();
    let placement = Point { column: 3, row: 4 };
    assert!(client.set_floating_pane_placement(pane_id, placement));
    client.set_floating_pane_pinned(pane_id, true);
    client.minimize_floating_pane(pane_id);

    client.restore_floating_pane(pane_id);

    assert_eq!(
        client.get_floating_pane_view(pane_id),
        FloatingPaneView {
            placement: Some(placement),
            is_pinned: true,
            is_minimized: false,
        }
    );
}

#[test]
fn a_pinned_floating_pane_refuses_a_placement_and_an_unpinned_one_accepts_it() {
    let mut client = build_test_client(TabId::new());
    let pane_id = PaneId::new();
    let placement = Point { column: 3, row: 4 };
    client.set_floating_pane_pinned(pane_id, true);

    assert!(!client.set_floating_pane_placement(pane_id, placement));
    assert_eq!(
        client.get_floating_pane_view(pane_id),
        FloatingPaneView {
            placement: None,
            is_pinned: true,
            is_minimized: false,
        }
    );

    client.set_floating_pane_pinned(pane_id, false);

    assert!(client.set_floating_pane_placement(pane_id, placement));
    assert_eq!(
        client.get_floating_pane_view(pane_id),
        FloatingPaneView {
            placement: Some(placement),
            is_pinned: false,
            is_minimized: false,
        }
    );
}

#[test]
fn remove_floating_pane_view_drops_the_view_the_order_entry_and_a_matching_focus() {
    let mut client = build_test_client(TabId::new());
    let (lower_pane_id, top_pane_id) = (PaneId::new(), PaneId::new());
    assert!(client.focus_floating_pane(lower_pane_id));
    assert!(client.focus_floating_pane(top_pane_id));
    client.set_floating_pane_pinned(lower_pane_id, true);
    client.set_floating_pane_pinned(top_pane_id, true);

    client.remove_floating_pane_view(lower_pane_id);

    assert_eq!(client.list_floating_pane_focus_order(), [top_pane_id]);
    assert_eq!(client.get_focused_floating_pane_id(), Some(top_pane_id));
    assert_eq!(
        client.list_floating_pane_views(),
        &HashMap::from([(
            top_pane_id,
            FloatingPaneView {
                placement: None,
                is_pinned: true,
                is_minimized: false,
            },
        )])
    );

    client.remove_floating_pane_view(top_pane_id);

    assert_eq!(
        client.list_floating_pane_focus_order(),
        Vec::<PaneId>::new()
    );
    assert_eq!(client.get_focused_floating_pane_id(), None);
    assert_eq!(client.list_floating_pane_views(), &HashMap::new());
}

#[test]
fn a_client_floating_view_survives_a_serde_round_trip() {
    let mut client = build_test_client(TabId::new());
    let (minimized_pane_id, placed_pane_id) = (PaneId::new(), PaneId::new());
    assert!(client.focus_floating_pane(minimized_pane_id));
    assert!(client.focus_floating_pane(placed_pane_id));
    assert!(client.set_floating_pane_placement(placed_pane_id, Point { column: 3, row: 4 }));
    client.set_floating_pane_pinned(placed_pane_id, true);
    client.minimize_floating_pane(minimized_pane_id);

    let client_json = serde_json::to_value(&client).expect("the client encodes");
    let placed_pane_key = serde_json::to_value(placed_pane_id).expect("the pane id encodes");
    assert_eq!(
        client_json["floating_pane_view_by_pane_id"][placed_pane_key
            .as_str()
            .expect("a pane id encodes as a string")],
        serde_json::json!({
            "placement": {"column": 3, "row": 4},
            "is_pinned": true,
            "is_minimized": false
        })
    );
    assert_eq!(
        client_json["floating_pane_focus_order"],
        serde_json::json!([minimized_pane_id, placed_pane_id])
    );
    assert_eq!(
        client_json["focused_floating_pane_id"],
        serde_json::json!(placed_pane_id)
    );

    let decoded_client: Client =
        serde_json::from_value(client_json.clone()).expect("the client decodes");

    assert_eq!(
        decoded_client.list_floating_pane_views(),
        client.list_floating_pane_views()
    );
    assert_eq!(
        decoded_client.list_floating_pane_focus_order(),
        [minimized_pane_id, placed_pane_id]
    );
    assert_eq!(
        decoded_client.get_focused_floating_pane_id(),
        Some(placed_pane_id)
    );
    assert_eq!(
        serde_json::to_value(&decoded_client).expect("the decoded client encodes"),
        client_json
    );
}

#[test]
fn one_clients_floating_view_changes_leave_another_clients_bytes_unchanged() {
    let mut client_registry = ClientRegistry::new();
    let (moving_client_id, watching_client_id) = (ClientId::new(), ClientId::new());
    let pane_id = PaneId::new();
    for client_id in [moving_client_id, watching_client_id] {
        client_registry.attach_client(build_test_client_from_parts(client_id, TabId::new(), None));
        assert!(client_registry
            .get_client_mut_by_id(client_id)
            .expect("the client was just attached")
            .focus_floating_pane(pane_id));
    }
    let watching_client_bytes = serde_json::to_vec(
        client_registry
            .get_client_by_id(watching_client_id)
            .expect("the client is attached"),
    )
    .expect("the client encodes");

    let moving_client = client_registry
        .get_client_mut_by_id(moving_client_id)
        .expect("the client is attached");
    assert!(moving_client.set_floating_pane_placement(pane_id, Point { column: 1, row: 2 }));
    moving_client.set_floating_pane_pinned(pane_id, true);
    moving_client.minimize_floating_pane(pane_id);

    assert_eq!(
        serde_json::to_vec(
            client_registry
                .get_client_by_id(watching_client_id)
                .expect("the client is attached"),
        )
        .expect("the client encodes"),
        watching_client_bytes
    );
    let moving_client = client_registry
        .get_client_by_id(moving_client_id)
        .expect("the client is attached");
    assert_eq!(
        moving_client.get_floating_pane_view(pane_id),
        FloatingPaneView {
            placement: Some(Point { column: 1, row: 2 }),
            is_pinned: true,
            is_minimized: true,
        }
    );
    assert_eq!(moving_client.get_focused_floating_pane_id(), None);
}

#[test]
fn floating_view_fields_are_required_and_a_missing_floating_focus_decodes_as_none() {
    let client_json =
        serde_json::to_value(build_test_client(TabId::new())).expect("the client encodes");
    for required_field_name in ["floating_pane_view_by_pane_id", "floating_pane_focus_order"] {
        let mut incomplete_client_json = client_json.clone();
        incomplete_client_json
            .as_object_mut()
            .expect("a client encodes as a json object")
            .remove(required_field_name)
            .expect("the encoded client carries the field");

        assert_eq!(
            serde_json::from_value::<Client>(incomplete_client_json)
                .expect_err("a client without the field is refused")
                .to_string(),
            format!("missing field `{required_field_name}`")
        );
    }
    let mut client_json_without_floating_focus = client_json;
    client_json_without_floating_focus
        .as_object_mut()
        .expect("a client encodes as a json object")
        .remove("focused_floating_pane_id")
        .expect("the encoded client carries the floating focus");

    let decoded_client: Client = serde_json::from_value(client_json_without_floating_focus)
        .expect("a client without a floating focus decodes");

    assert_eq!(decoded_client.get_focused_floating_pane_id(), None);
}
