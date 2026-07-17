//! Client and ClientRegistry unit tests.
//!
//! Tests verify client state tracking (focus, viewport, lock mode, drag state) and
//! registry operations (attach, detach, lookup, mutation).

use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{GridPos, Selection, SelectionKind};
use koshi_core::geometry::{Direction, Point, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseButton;

use super::{
    pane_viewport, ClickCount, Client, ClientRegistry, MouseState, ResizeDragState,
    SelectionDragState, TablineDragState,
};

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
    assert_eq!(client.mouse_state(), &MouseState::default());
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
fn switching_tabs_ends_a_pending_resize_drag() {
    let tab = TabId::new();
    let other = TabId::new();
    let mut client = a_client(tab);

    client.update_pending_resize_drag(Some(ResizeDragState {
        pane: PaneId::new(),
        side: Direction::Right,
        last: Point { x: 5, y: 5 },
    }));
    assert!(client.pending_resize_drag().is_some());

    // The grabbed border is no longer on the client's frame, so the drag ends.
    client.update_active_tab(other);
    assert!(client.pending_resize_drag().is_none());
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

    let drag = ResizeDragState {
        pane: PaneId::new(),
        side: Direction::Right,
        last: Point { x: 4, y: 2 },
    };
    client.update_pending_resize_drag(Some(drag));
    assert_eq!(client.pending_resize_drag(), Some(&drag));

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

// ============================================================================
// The run of clicks
// ============================================================================

/// The 400ms the runtime uses, so these read the way the real thing behaves.
const THRESHOLD: Duration = Duration::from_millis(400);

#[test]
fn a_first_press_is_a_single_click() {
    let mut state = MouseState::default();
    assert_eq!(
        state.press(MouseButton::Left, Instant::now(), THRESHOLD),
        ClickCount::Single
    );
}

#[test]
fn presses_inside_the_threshold_climb_to_double_then_triple() {
    let mut state = MouseState::default();
    let start = Instant::now();

    assert_eq!(
        state.press(MouseButton::Left, start, THRESHOLD),
        ClickCount::Single
    );
    assert_eq!(
        state.press(
            MouseButton::Left,
            start + Duration::from_millis(120),
            THRESHOLD
        ),
        ClickCount::Double
    );
    assert_eq!(
        state.press(
            MouseButton::Left,
            start + Duration::from_millis(260),
            THRESHOLD
        ),
        ClickCount::Triple
    );
}

#[test]
fn a_fourth_quick_press_starts_the_run_over() {
    let mut state = MouseState::default();
    let start = Instant::now();
    state.press(MouseButton::Left, start, THRESHOLD);
    state.press(
        MouseButton::Left,
        start + Duration::from_millis(100),
        THRESHOLD,
    );
    state.press(
        MouseButton::Left,
        start + Duration::from_millis(200),
        THRESHOLD,
    );

    assert_eq!(
        state.press(
            MouseButton::Left,
            start + Duration::from_millis(300),
            THRESHOLD
        ),
        ClickCount::Single,
        "a run tops out at three and begins again"
    );
}

#[test]
fn a_press_at_the_threshold_starts_a_new_run() {
    let mut state = MouseState::default();
    let start = Instant::now();
    state.press(MouseButton::Left, start, THRESHOLD);

    // Exactly 400ms: the gap is no longer inside the threshold.
    assert_eq!(
        state.press(MouseButton::Left, start + THRESHOLD, THRESHOLD),
        ClickCount::Single
    );
}

#[test]
fn a_slow_second_press_is_another_single_click() {
    let mut state = MouseState::default();
    let start = Instant::now();
    state.press(MouseButton::Left, start, THRESHOLD);

    assert_eq!(
        state.press(MouseButton::Left, start + Duration::from_secs(1), THRESHOLD),
        ClickCount::Single
    );
}

#[test]
fn a_different_button_starts_a_new_run() {
    let mut state = MouseState::default();
    let start = Instant::now();
    state.press(MouseButton::Left, start, THRESHOLD);

    // A quick right click after a left one is not a double click.
    assert_eq!(
        state.press(
            MouseButton::Right,
            start + Duration::from_millis(50),
            THRESHOLD
        ),
        ClickCount::Single
    );
    // And the run now belongs to the right button.
    assert_eq!(
        state.press(
            MouseButton::Right,
            start + Duration::from_millis(100),
            THRESHOLD
        ),
        ClickCount::Double
    );
}

#[test]
fn each_run_length_picks_its_selection_shape() {
    assert_eq!(
        ClickCount::Single.selection_kind(),
        SelectionKind::Character
    );
    assert_eq!(ClickCount::Double.selection_kind(), SelectionKind::Word);
    assert_eq!(ClickCount::Triple.selection_kind(), SelectionKind::Line);
}

#[test]
fn a_selection_drag_is_held_and_released() {
    let mut client = a_client(TabId::new());
    let pane = PaneId::new();
    assert_eq!(client.selection_drag(), None);

    let drag = SelectionDragState {
        pane,
        kind: SelectionKind::Character,
        anchor: GridPos { row: 3, col: 4 },
        at: Point { x: 10, y: 5 },
        scroll_at: None,
    };
    client.set_selection_drag(Some(drag));
    assert_eq!(client.selection_drag(), Some(drag));

    client.set_selection_drag(None);
    assert_eq!(client.selection_drag(), None);
}
