//! Tests for the saved-view store: what a minted token takes back, that it
//! takes it back once, what an unminted token finds, when a filed view stops
//! standing, what happens past the record count, what a second save for one
//! client files, what `forget` leaves behind, and how an untouched view
//! round-trips.

use koshi_core::geometry::Size;
use koshi_core::ids::SessionId;
use koshi_session::client::ClientOrigin;

use super::*;

/// A fixed point on the clock, `secs` seconds after the epoch.
fn moment(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// A client with `id`, viewing `active_tab`, with no focused pane, no zoomed
/// pane and no pane scrolled up.
fn client(id: ClientId, active_tab: TabId) -> Client {
    Client::new(
        id,
        SessionId::new(),
        moment(0),
        Size { cols: 80, rows: 24 },
        active_tab,
        ClientOrigin::Local,
        "C-calm-otter".to_string(),
        1,
    )
}

#[test]
fn a_minted_token_takes_back_every_part_of_the_saved_view() {
    let tab = TabId::new();
    let other_tab = TabId::new();
    let focused = PaneId::new();
    let zoomed = PaneId::new();
    let scrolled = PaneId::new();

    let mut client = client(ClientId::new(), tab);
    client.update_focused_pane(tab, focused);
    client.update_focused_pane(other_tab, zoomed);
    client.zoom_pane(other_tab, zoomed);
    client.set_scroll_offset(scrolled, 7);

    let mut store = SavedViewStore::default();
    let token = store.mint(client.id());
    store.save(&client, moment(100));

    let view = store.take(&token, moment(101)).expect("the filed view");
    assert_eq!(view.active_tab, tab);
    assert_eq!(
        view.focus_by_tab,
        HashMap::from([(tab, focused), (other_tab, zoomed)])
    );
    assert_eq!(view.zoom_by_tab, HashMap::from([(other_tab, zoomed)]));
    assert_eq!(view.scroll_by_pane, HashMap::from([(scrolled, 7)]));
}

#[test]
fn presenting_the_same_token_twice_takes_the_view_back_once() {
    let tab = TabId::new();
    let client = client(ClientId::new(), tab);
    let mut store = SavedViewStore::default();
    let token = store.mint(client.id());
    store.save(&client, moment(100));

    assert_eq!(
        store.take(&token, moment(101)).map(|view| view.active_tab),
        Some(tab)
    );
    assert_eq!(store.take(&token, moment(102)), None);
}

#[test]
fn a_token_nobody_minted_takes_back_nothing() {
    let client = client(ClientId::new(), TabId::new());
    let mut store = SavedViewStore::default();
    store.mint(client.id());
    store.save(&client, moment(100));

    let stranger = ConnectionToken::generate();
    assert_eq!(store.take(&stranger, moment(101)), None);
}

#[test]
fn a_filed_view_stands_for_one_hundred_and_twenty_seconds() {
    let tab = TabId::new();
    let client = client(ClientId::new(), tab);

    let mut standing = SavedViewStore::default();
    let token = standing.mint(client.id());
    standing.save(&client, moment(100));
    assert_eq!(
        standing
            .take(&token, moment(219))
            .map(|view| view.active_tab),
        Some(tab)
    );

    let mut expired = SavedViewStore::default();
    let token = expired.mint(client.id());
    expired.save(&client, moment(100));
    assert_eq!(expired.take(&token, moment(221)), None);
}

#[test]
fn filing_a_thirty_third_view_drops_the_first_one_filed() {
    let mut store = SavedViewStore::default();
    let mut tokens = Vec::new();
    let mut tabs = Vec::new();
    for _ in 0..33 {
        let tab = TabId::new();
        let client = client(ClientId::new(), tab);
        tokens.push(store.mint(client.id()));
        tabs.push(tab);
        store.save(&client, moment(100));
    }
    assert_eq!(store.records.len(), 32);

    assert_eq!(
        store
            .take(&tokens[32], moment(101))
            .map(|view| view.active_tab),
        Some(tabs[32])
    );
    assert_eq!(store.take(&tokens[0], moment(101)), None);
    for index in 1..32 {
        assert_eq!(
            store
                .take(&tokens[index], moment(101))
                .map(|view| view.active_tab),
            Some(tabs[index])
        );
    }
}

#[test]
fn a_second_save_for_one_client_files_nothing() {
    let client = client(ClientId::new(), TabId::new());
    let mut store = SavedViewStore::default();
    store.mint(client.id());

    store.save(&client, moment(100));
    store.save(&client, moment(101));

    assert_eq!(store.records.len(), 1);
}

#[test]
fn minting_again_for_one_client_leaves_the_earlier_token_taking_back_nothing() {
    let tab = TabId::new();
    let client = client(ClientId::new(), tab);
    let mut store = SavedViewStore::default();
    let earlier = store.mint(client.id());
    let latest = store.mint(client.id());
    store.save(&client, moment(100));

    assert_eq!(store.records.len(), 1, "one save files one record");
    assert_eq!(store.take(&earlier, moment(101)), None);
    assert_eq!(
        store.take(&latest, moment(101)).map(|view| view.active_tab),
        Some(tab)
    );
}

#[test]
fn forgetting_a_client_leaves_its_minted_token_taking_back_nothing() {
    let client = client(ClientId::new(), TabId::new());
    let mut store = SavedViewStore::default();
    let token = store.mint(client.id());

    store.forget(client.id());
    store.save(&client, moment(100));

    assert!(store.records.is_empty());
    assert_eq!(store.take(&token, moment(101)), None);
}

/// The last moment this platform's clock holds, to within one second. Every
/// platform holds a different one, so it is found rather than written down:
/// the step starts wider than any clock's range and halves on every pass,
/// taken when the clock accepts it and skipped when it does not, which lands
/// inside a second after 63 passes.
fn the_end_of_the_clock() -> SystemTime {
    let mut at = SystemTime::UNIX_EPOCH;
    let mut step = Duration::from_secs(1 << 62);
    while step >= Duration::from_secs(1) {
        if let Some(later) = at.checked_add(step) {
            at = later;
        }
        step /= 2;
    }
    at
}

#[test]
fn a_clock_too_near_its_end_to_hold_the_lifetime_files_nothing_and_drops_the_hash() {
    let end = the_end_of_the_clock();
    assert_eq!(
        end.checked_add(LIFETIME),
        None,
        "the walk stopped short of the clock's end"
    );

    let client = client(ClientId::new(), TabId::new());
    let mut store = SavedViewStore::default();
    let token = store.mint(client.id());
    store.save(&client, end);

    assert!(store.records.is_empty());
    assert!(store.hash_by_client.is_empty());
    assert_eq!(store.take(&token, moment(100)), None);
}

#[test]
fn a_client_that_touched_nothing_takes_back_its_tab_and_three_empty_maps() {
    let tab = TabId::new();
    let client = client(ClientId::new(), tab);
    let mut store = SavedViewStore::default();
    let token = store.mint(client.id());
    store.save(&client, moment(100));

    assert_eq!(
        store.take(&token, moment(101)),
        Some(SavedView {
            active_tab: tab,
            focus_by_tab: HashMap::new(),
            zoom_by_tab: HashMap::new(),
            scroll_by_pane: HashMap::new(),
        })
    );
}
