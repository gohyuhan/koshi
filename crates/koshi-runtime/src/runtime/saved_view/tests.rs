//! Tests for the saved-view store: what a minted token takes back, that it
//! takes it back once, what an unminted token finds, when a filed view stops
//! standing, what happens past the record count, what a second save for one
//! client files, what `forget` leaves behind, how an untouched view
//! round-trips, and which saved view records a save drops.

use koshi_core::geometry::Size;
use koshi_core::ids::SessionId;
use koshi_session::client::ClientOrigin;

use super::*;

/// A fixed point on the clock, `elapsed_seconds` seconds after the epoch.
fn time_at_seconds(elapsed_seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(elapsed_seconds)
}

/// A client with `client_id`, viewing `active_tab`, with no focused pane, no
/// zoomed pane and no pane scrolled up.
fn build_client(client_id: ClientId, active_tab_id: TabId) -> Client {
    Client::from_attachment(
        client_id,
        SessionId::new(),
        time_at_seconds(0),
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
    let tab = TabId::new();
    let other_tab = TabId::new();
    let focused_pane_id = PaneId::new();
    let zoomed_pane_id = PaneId::new();
    let scrolled_pane_id = PaneId::new();

    let mut client = build_client(ClientId::new(), tab);
    client.update_focused_pane(tab, focused_pane_id);
    client.update_focused_pane(other_tab, zoomed_pane_id);
    client.zoom_pane(other_tab, zoomed_pane_id);
    client.set_scroll_offset(scrolled_pane_id, 7);

    let mut saved_view_store = SavedViewStore::default();
    let token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, time_at_seconds(100));

    let saved_view = saved_view_store
        .take_saved_view(&token, time_at_seconds(101))
        .expect("the filed view");
    assert_eq!(saved_view.active_tab_id, tab);
    assert_eq!(
        saved_view.focused_pane_id_by_tab_id,
        HashMap::from([(tab, focused_pane_id), (other_tab, zoomed_pane_id)])
    );
    assert_eq!(
        saved_view.zoomed_pane_id_by_tab_id,
        HashMap::from([(other_tab, zoomed_pane_id)])
    );
    assert_eq!(
        saved_view.scroll_offset_by_pane_id,
        HashMap::from([(scrolled_pane_id, 7)])
    );
}

#[test]
fn presenting_the_same_token_twice_takes_the_view_back_once() {
    let tab = TabId::new();
    let client = build_client(ClientId::new(), tab);
    let mut saved_view_store = SavedViewStore::default();
    let token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, time_at_seconds(100));

    assert_eq!(
        saved_view_store
            .take_saved_view(&token, time_at_seconds(101))
            .map(|saved_view| saved_view.active_tab_id),
        Some(tab)
    );
    assert_eq!(
        saved_view_store.take_saved_view(&token, time_at_seconds(102)),
        None
    );
}

#[test]
fn a_token_nobody_minted_takes_back_nothing() {
    let client = build_client(ClientId::new(), TabId::new());
    let mut saved_view_store = SavedViewStore::default();
    saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, time_at_seconds(100));

    let stranger = ConnectionToken::generate();
    assert_eq!(
        saved_view_store.take_saved_view(&stranger, time_at_seconds(101)),
        None
    );
}

#[test]
fn a_filed_view_stands_for_one_hundred_and_twenty_seconds() {
    let tab = TabId::new();
    let client = build_client(ClientId::new(), tab);

    let mut standing_view_store = SavedViewStore::default();
    let token = standing_view_store.mint_resume_token(client.get_client_id());
    standing_view_store.save_client_view(&client, time_at_seconds(100));
    assert_eq!(
        standing_view_store
            .take_saved_view(&token, time_at_seconds(219))
            .map(|saved_view| saved_view.active_tab_id),
        Some(tab)
    );

    let mut expired_view_store = SavedViewStore::default();
    let token = expired_view_store.mint_resume_token(client.get_client_id());
    expired_view_store.save_client_view(&client, time_at_seconds(100));
    assert_eq!(
        expired_view_store.take_saved_view(&token, time_at_seconds(221)),
        None
    );
}

#[test]
fn filing_a_thirty_third_view_drops_the_first_one_filed() {
    let mut saved_view_store = SavedViewStore::default();
    let mut resume_tokens = Vec::new();
    let mut tab_ids = Vec::new();
    for _ in 0..33 {
        let tab = TabId::new();
        let client = build_client(ClientId::new(), tab);
        resume_tokens.push(saved_view_store.mint_resume_token(client.get_client_id()));
        tab_ids.push(tab);
        saved_view_store.save_client_view(&client, time_at_seconds(100));
    }
    assert_eq!(saved_view_store.saved_view_records.len(), 32);

    assert_eq!(
        saved_view_store
            .take_saved_view(&resume_tokens[32], time_at_seconds(101))
            .map(|saved_view| saved_view.active_tab_id),
        Some(tab_ids[32])
    );
    assert_eq!(
        saved_view_store.take_saved_view(&resume_tokens[0], time_at_seconds(101)),
        None
    );
    for record_index in 1..32 {
        assert_eq!(
            saved_view_store
                .take_saved_view(&resume_tokens[record_index], time_at_seconds(101))
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

    saved_view_store.save_client_view(&client, time_at_seconds(100));
    saved_view_store.save_client_view(&client, time_at_seconds(101));

    assert_eq!(saved_view_store.saved_view_records.len(), 1);
}

#[test]
fn minting_again_for_one_client_leaves_the_earlier_token_taking_back_nothing() {
    let tab = TabId::new();
    let client = build_client(ClientId::new(), tab);
    let mut saved_view_store = SavedViewStore::default();
    let earlier = saved_view_store.mint_resume_token(client.get_client_id());
    let latest = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, time_at_seconds(100));

    assert_eq!(
        saved_view_store.saved_view_records.len(),
        1,
        "one save files one record"
    );
    assert_eq!(
        saved_view_store.take_saved_view(&earlier, time_at_seconds(101)),
        None
    );
    assert_eq!(
        saved_view_store
            .take_saved_view(&latest, time_at_seconds(101))
            .map(|saved_view| saved_view.active_tab_id),
        Some(tab)
    );
}

#[test]
fn forgetting_a_client_leaves_its_minted_token_taking_back_nothing() {
    let client = build_client(ClientId::new(), TabId::new());
    let mut saved_view_store = SavedViewStore::default();
    let token = saved_view_store.mint_resume_token(client.get_client_id());

    saved_view_store.forget_client_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, time_at_seconds(100));

    assert!(saved_view_store.saved_view_records.is_empty());
    assert_eq!(
        saved_view_store.take_saved_view(&token, time_at_seconds(101)),
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
    let token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, clock_end);

    assert!(saved_view_store.saved_view_records.is_empty());
    assert!(saved_view_store
        .connection_token_hash_by_client_id
        .is_empty());
    assert_eq!(
        saved_view_store.take_saved_view(&token, time_at_seconds(100)),
        None
    );
}

#[test]
fn a_client_that_touched_nothing_takes_back_its_tab_and_three_empty_maps() {
    let tab = TabId::new();
    let client = build_client(ClientId::new(), tab);
    let mut saved_view_store = SavedViewStore::default();
    let token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, time_at_seconds(100));

    assert_eq!(
        saved_view_store.take_saved_view(&token, time_at_seconds(101)),
        Some(SavedView {
            active_tab_id: tab,
            focused_pane_id_by_tab_id: HashMap::new(),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            scroll_offset_by_pane_id: HashMap::new(),
        })
    );
}

#[test]
fn a_view_taken_at_the_exact_second_it_stops_standing_takes_back_nothing() {
    let client = build_client(ClientId::new(), TabId::new());
    let mut saved_view_store = SavedViewStore::default();
    let token = saved_view_store.mint_resume_token(client.get_client_id());
    saved_view_store.save_client_view(&client, time_at_seconds(100));

    assert_eq!(
        saved_view_store.take_saved_view(&token, time_at_seconds(220)),
        None
    );
    assert_eq!(saved_view_store.saved_view_records.len(), 0);
}

#[test]
fn filing_a_view_drops_the_records_that_stopped_standing() {
    let stale = build_client(ClientId::new(), TabId::new());
    let fresh_tab = TabId::new();
    let fresh = build_client(ClientId::new(), fresh_tab);
    let mut saved_view_store = SavedViewStore::default();

    let stale_token = saved_view_store.mint_resume_token(stale.get_client_id());
    saved_view_store.save_client_view(&stale, time_at_seconds(100));
    let fresh_token = saved_view_store.mint_resume_token(fresh.get_client_id());
    saved_view_store.save_client_view(&fresh, time_at_seconds(300));

    assert_eq!(saved_view_store.saved_view_records.len(), 1);
    assert_eq!(
        saved_view_store.take_saved_view(&stale_token, time_at_seconds(300)),
        None
    );
    assert_eq!(
        saved_view_store
            .take_saved_view(&fresh_token, time_at_seconds(300))
            .map(|saved_view| saved_view.active_tab_id),
        Some(fresh_tab)
    );
}
