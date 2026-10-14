//! Tests for the saved-view store: what a minted token takes back, that it
//! takes it back once, what an unminted token finds, when a filed view stops
//! standing, what happens past the record count, what a second save for one
//! client files, what `forget` leaves behind, how an untouched view
//! round-trips, which saved view records a save drops, and which attached
//! client a token finds.

use koshi_core::geometry::{Point, Size};
use koshi_core::ids::SessionId;
use koshi_session::client::{ClientOrigin, FloatingPanePosition};

use super::*;

/// A fixed point on the clock, `elapsed_seconds` seconds after the epoch.
fn build_system_time_at_seconds(elapsed_seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(elapsed_seconds)
}

/// A client with `client_id`, viewing `active_tab_id`, with no focused pane, no
/// zoomed pane and no pane scrolled up.
fn build_client(client_id: ClientId, active_tab_id: TabId) -> Client {
    Client::from_attachment(
        client_id,
        SessionId::new(),
        build_system_time_at_seconds(0),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        active_tab_id,
        ClientOrigin::Local,
        "C-calm-otter".to_string(),
        1,
    )
}

#[test]
fn a_minted_token_takes_back_every_part_of_the_saved_view() {
    let tab_id = TabId::new();
    let other_tab_id = TabId::new();
    let focused_pane_id = PaneId::new();
    let zoomed_pane_id = PaneId::new();
    let scrolled_pane_id = PaneId::new();
    let (minimized_floating_pane_id, focused_floating_pane_id) = (PaneId::new(), PaneId::new());

    let mut client = build_client(ClientId::new(), tab_id);
    client.update_focused_pane(tab_id, focused_pane_id);
    client.update_focused_pane(other_tab_id, zoomed_pane_id);
    client.zoom_pane(other_tab_id, zoomed_pane_id);
    client.set_scroll_offset(scrolled_pane_id, 7);
    assert!(client.focus_floating_pane(minimized_floating_pane_id));
    assert!(client.focus_floating_pane(focused_floating_pane_id));
    assert!(client.minimize_floating_pane(minimized_floating_pane_id));
    client.pin_floating_pane(focused_floating_pane_id, Point { column: 4, row: 2 });

    let mut saved_view_store = SavedViewStore::default();
    let resume_token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, build_system_time_at_seconds(100));

    let saved_view = saved_view_store
        .take_saved_view(&resume_token, build_system_time_at_seconds(101))
        .expect("the filed view");
    assert_eq!(saved_view.active_tab_id, tab_id);
    assert_eq!(
        saved_view.focused_pane_id_by_tab_id,
        HashMap::from([(tab_id, focused_pane_id), (other_tab_id, zoomed_pane_id)])
    );
    assert_eq!(
        saved_view.zoomed_pane_id_by_tab_id,
        HashMap::from([(other_tab_id, zoomed_pane_id)])
    );
    assert_eq!(
        saved_view.scroll_offset_by_pane_id,
        HashMap::from([(scrolled_pane_id, 7)])
    );
    assert_eq!(
        saved_view.floating_pane_view_by_pane_id,
        HashMap::from([
            (
                minimized_floating_pane_id,
                FloatingPaneView {
                    position: FloatingPanePosition::Default,
                    is_minimized: true,
                },
            ),
            (
                focused_floating_pane_id,
                FloatingPaneView {
                    position: FloatingPanePosition::Pinned(Point { column: 4, row: 2 }),
                    is_minimized: false,
                },
            ),
        ])
    );
    assert_eq!(
        saved_view.floating_pane_focus_order,
        [minimized_floating_pane_id, focused_floating_pane_id]
    );
    assert_eq!(
        saved_view.focused_floating_pane_id,
        Some(focused_floating_pane_id)
    );
}

#[test]
fn presenting_the_same_token_twice_takes_the_view_back_once() {
    let tab_id = TabId::new();
    let client = build_client(ClientId::new(), tab_id);
    let mut saved_view_store = SavedViewStore::default();
    let resume_token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, build_system_time_at_seconds(100));

    assert_eq!(
        saved_view_store
            .take_saved_view(&resume_token, build_system_time_at_seconds(101))
            .map(|saved_view| saved_view.active_tab_id),
        Some(tab_id)
    );
    assert_eq!(
        saved_view_store.take_saved_view(&resume_token, build_system_time_at_seconds(102)),
        None
    );
}

#[test]
fn a_token_nobody_minted_takes_back_nothing() {
    let client = build_client(ClientId::new(), TabId::new());
    let mut saved_view_store = SavedViewStore::default();
    saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, build_system_time_at_seconds(100));

    let unminted_resume_token = ConnectionToken::generate();
    assert_eq!(
        saved_view_store.take_saved_view(&unminted_resume_token, build_system_time_at_seconds(101)),
        None
    );
}

#[test]
fn a_filed_view_stands_for_twenty_four_hours() {
    assert_eq!(SAVED_VIEW_LIFETIME_DURATION, Duration::from_secs(86_400));
    let tab_id = TabId::new();
    let client = build_client(ClientId::new(), tab_id);

    let mut standing_view_store = SavedViewStore::default();
    let resume_token = standing_view_store.mint_resume_token(client.get_client_id());
    standing_view_store.save_client_view(&client, build_system_time_at_seconds(100));
    assert_eq!(
        standing_view_store
            .take_saved_view(&resume_token, build_system_time_at_seconds(86_499))
            .map(|saved_view| saved_view.active_tab_id),
        Some(tab_id)
    );

    let mut expired_view_store = SavedViewStore::default();
    let resume_token = expired_view_store.mint_resume_token(client.get_client_id());
    expired_view_store.save_client_view(&client, build_system_time_at_seconds(100));
    assert_eq!(
        expired_view_store.take_saved_view(&resume_token, build_system_time_at_seconds(86_501)),
        None
    );
}

#[test]
fn filing_a_thirty_third_view_drops_the_first_one_filed() {
    let mut saved_view_store = SavedViewStore::default();
    let mut resume_tokens = Vec::new();
    let mut tab_ids = Vec::new();
    for _ in 0..33 {
        let tab_id = TabId::new();
        let client = build_client(ClientId::new(), tab_id);
        resume_tokens.push(saved_view_store.mint_resume_token(client.get_client_id()));
        tab_ids.push(tab_id);
        saved_view_store.save_client_view(&client, build_system_time_at_seconds(100));
    }
    assert_eq!(saved_view_store.saved_view_records.len(), 32);

    assert_eq!(
        saved_view_store
            .take_saved_view(&resume_tokens[32], build_system_time_at_seconds(101))
            .map(|saved_view| saved_view.active_tab_id),
        Some(tab_ids[32])
    );
    assert_eq!(
        saved_view_store.take_saved_view(&resume_tokens[0], build_system_time_at_seconds(101)),
        None
    );
    for record_index in 1..32 {
        assert_eq!(
            saved_view_store
                .take_saved_view(
                    &resume_tokens[record_index],
                    build_system_time_at_seconds(101)
                )
                .map(|saved_view| saved_view.active_tab_id),
            Some(tab_ids[record_index])
        );
    }
}

#[test]
fn a_second_save_for_one_client_files_nothing() {
    let client = build_client(ClientId::new(), TabId::new());
    let mut saved_view_store = SavedViewStore::default();
    saved_view_store.mint_resume_token(client.get_client_id());

    saved_view_store.save_client_view(&client, build_system_time_at_seconds(100));
    saved_view_store.save_client_view(&client, build_system_time_at_seconds(101));

    assert_eq!(saved_view_store.saved_view_records.len(), 1);
}

#[test]
fn minting_again_for_one_client_leaves_the_earlier_token_taking_back_nothing() {
    let tab_id = TabId::new();
    let client = build_client(ClientId::new(), tab_id);
    let mut saved_view_store = SavedViewStore::default();
    let earlier_resume_token = saved_view_store.mint_resume_token(client.get_client_id());
    let latest_resume_token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, build_system_time_at_seconds(100));

    assert_eq!(
        saved_view_store.saved_view_records.len(),
        1,
        "one save files one record"
    );
    assert_eq!(
        saved_view_store.take_saved_view(&earlier_resume_token, build_system_time_at_seconds(101)),
        None
    );
    assert_eq!(
        saved_view_store
            .take_saved_view(&latest_resume_token, build_system_time_at_seconds(101))
            .map(|saved_view| saved_view.active_tab_id),
        Some(tab_id)
    );
}

#[test]
fn forgetting_a_client_leaves_its_minted_token_taking_back_nothing() {
    let client = build_client(ClientId::new(), TabId::new());
    let mut saved_view_store = SavedViewStore::default();
    let resume_token = saved_view_store.mint_resume_token(client.get_client_id());

    saved_view_store.forget_client_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, build_system_time_at_seconds(100));

    assert!(saved_view_store.saved_view_records.is_empty());
    assert_eq!(
        saved_view_store.take_saved_view(&resume_token, build_system_time_at_seconds(101)),
        None
    );
}

/// The last time this platform's clock holds, to within one second. Every
/// platform holds a different one, so it is found rather than written down:
/// the step starts wider than any clock's range and halves on every pass,
/// taken when the clock accepts it and skipped when it does not, which lands
/// inside a second after 63 passes.
fn find_clock_end() -> SystemTime {
    let mut current_time = SystemTime::UNIX_EPOCH;
    let mut time_step = Duration::from_secs(1 << 62);
    while time_step >= Duration::from_secs(1) {
        if let Some(candidate_time) = current_time.checked_add(time_step) {
            current_time = candidate_time;
        }
        time_step /= 2;
    }
    current_time
}

#[test]
fn a_clock_too_near_its_end_to_hold_the_lifetime_files_nothing_and_drops_the_hash() {
    let clock_end = find_clock_end();
    assert_eq!(
        clock_end.checked_add(SAVED_VIEW_LIFETIME_DURATION),
        None,
        "the walk stopped short of the clock's end"
    );

    let client = build_client(ClientId::new(), TabId::new());
    let mut saved_view_store = SavedViewStore::default();
    let resume_token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, clock_end);

    assert!(saved_view_store.saved_view_records.is_empty());
    assert!(saved_view_store
        .connection_token_hash_by_client_id
        .is_empty());
    assert_eq!(
        saved_view_store.take_saved_view(&resume_token, build_system_time_at_seconds(100)),
        None
    );
}

#[test]
fn a_client_that_touched_nothing_takes_back_its_tab_and_an_empty_view() {
    let tab_id = TabId::new();
    let client = build_client(ClientId::new(), tab_id);
    let mut saved_view_store = SavedViewStore::default();
    let resume_token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, build_system_time_at_seconds(100));

    assert_eq!(
        saved_view_store.take_saved_view(&resume_token, build_system_time_at_seconds(101)),
        Some(SavedView {
            active_tab_id: tab_id,
            focused_pane_id_by_tab_id: HashMap::new(),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            scroll_offset_by_pane_id: HashMap::new(),
            floating_pane_view_by_pane_id: HashMap::new(),
            floating_pane_focus_order: Vec::new(),
            focused_floating_pane_id: None,
        })
    );
}

#[test]
fn a_view_taken_at_the_exact_second_it_stops_standing_takes_back_nothing() {
    let client = build_client(ClientId::new(), TabId::new());
    let mut saved_view_store = SavedViewStore::default();
    let resume_token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, build_system_time_at_seconds(100));

    assert_eq!(
        saved_view_store.take_saved_view(&resume_token, build_system_time_at_seconds(86_500)),
        None
    );
    assert_eq!(saved_view_store.saved_view_records.len(), 0);
}

#[test]
fn filing_a_view_drops_the_records_that_stopped_standing() {
    let stale_client = build_client(ClientId::new(), TabId::new());
    let fresh_tab_id = TabId::new();
    let fresh_client = build_client(ClientId::new(), fresh_tab_id);
    let mut saved_view_store = SavedViewStore::default();

    let stale_resume_token = saved_view_store.mint_resume_token(stale_client.get_client_id());
    saved_view_store.save_client_view(&stale_client, build_system_time_at_seconds(100));
    let fresh_resume_token = saved_view_store.mint_resume_token(fresh_client.get_client_id());
    saved_view_store.save_client_view(&fresh_client, build_system_time_at_seconds(86_500));

    assert_eq!(saved_view_store.saved_view_records.len(), 1);
    assert_eq!(
        saved_view_store.take_saved_view(&stale_resume_token, build_system_time_at_seconds(86_500)),
        None
    );
    assert_eq!(
        saved_view_store
            .take_saved_view(&fresh_resume_token, build_system_time_at_seconds(86_500))
            .map(|saved_view| saved_view.active_tab_id),
        Some(fresh_tab_id)
    );
}

#[test]
fn a_token_finds_the_still_attached_client_its_attach_minted_and_changes_nothing() {
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();
    let mut saved_view_store = SavedViewStore::default();
    let first_resume_token = saved_view_store.mint_resume_token(first_client_id);
    let second_resume_token = saved_view_store.mint_resume_token(second_client_id);

    assert_eq!(
        saved_view_store.find_client_id_by_resume_token(&first_resume_token),
        Some(first_client_id)
    );
    assert_eq!(
        saved_view_store.find_client_id_by_resume_token(&second_resume_token),
        Some(second_client_id)
    );
    assert_eq!(
        saved_view_store.find_client_id_by_resume_token(&first_resume_token),
        Some(first_client_id)
    );
    assert_eq!(saved_view_store.connection_token_hash_by_client_id.len(), 2);
}

#[test]
fn a_token_finds_no_client_once_unminted_replaced_filed_or_forgotten() {
    let mut saved_view_store = SavedViewStore::default();
    assert_eq!(
        saved_view_store.find_client_id_by_resume_token(&ConnectionToken::generate()),
        None
    );

    let replaced_client_id = ClientId::new();
    let replaced_resume_token = saved_view_store.mint_resume_token(replaced_client_id);
    saved_view_store.mint_resume_token(replaced_client_id);
    assert_eq!(
        saved_view_store.find_client_id_by_resume_token(&replaced_resume_token),
        None
    );

    let filed_client = build_client(ClientId::new(), TabId::new());
    let filed_resume_token = saved_view_store.mint_resume_token(filed_client.get_client_id());
    saved_view_store.save_client_view(&filed_client, build_system_time_at_seconds(100));
    assert_eq!(
        saved_view_store.find_client_id_by_resume_token(&filed_resume_token),
        None
    );

    let forgotten_client_id = ClientId::new();
    let forgotten_resume_token = saved_view_store.mint_resume_token(forgotten_client_id);
    saved_view_store.forget_client_resume_token(forgotten_client_id);
    assert_eq!(
        saved_view_store.find_client_id_by_resume_token(&forgotten_resume_token),
        None
    );
}
