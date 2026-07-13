//! Tests for [`Session`] helpers.

use super::*;
use std::time::SystemTime;

/// Attach a client viewing `tab` with the given viewport.
fn viewer(session: &mut Session, tab: TabId, cols: u16, rows: u16) {
    let client = Client::new(
        ClientId::new(),
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols, rows },
        tab,
    );
    session.attach_client(client);
}

#[test]
fn tab_viewport_takes_the_per_axis_minimum_across_viewers() {
    let tab = TabId::new();
    let other_tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    // Two clients view `tab` with opposite aspect ratios.
    viewer(&mut session, tab, 80, 5);
    viewer(&mut session, tab, 40, 24);
    // A client on a different tab must not count.
    viewer(&mut session, other_tab, 10, 1);

    // Full-viewport minimum is 40×5; reserving two chrome rows leaves 40×3.
    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 40, rows: 3 }));
}

#[test]
fn tab_viewport_is_none_without_a_viewer() {
    let tab = TabId::new();
    let session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert_eq!(session.tab_viewport(tab), None);
}

#[test]
fn a_new_session_stores_the_supplied_creation_time() {
    let created_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234);
    let session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        created_at,
        ClientRegistry::new(),
    );

    assert_eq!(session.created_at, created_at);
}

#[test]
fn tab_viewport_with_exactly_one_viewer_returns_its_own_reserved_size() {
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    viewer(&mut session, tab, 100, 30);

    // A single viewer's own size, minus the two chrome rows, wins outright —
    // there is no second viewport to take a minimum against.
    assert_eq!(
        session.tab_viewport(tab),
        Some(Size {
            cols: 100,
            rows: 28
        })
    );
}

#[test]
fn tab_viewport_saturates_rather_than_panics_below_the_chrome_rows() {
    // A viewport with fewer rows than the two reserved chrome rows must not
    // underflow the `u16` row count; `1 - 2` would panic in debug builds
    // under plain subtraction, so the contract is `0`, not a crash.
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    viewer(&mut session, tab, 80, 1);

    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 80, rows: 0 }));
}

#[test]
fn attach_client_returns_the_client_it_displaced_on_reattach() {
    // A re-attach under the same id replaces in place; the caller needs the
    // displaced record back (e.g. to tear down its old view state).
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let id = ClientId::new();
    let first = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 0, rows: 0 },
        tab,
    );
    assert_eq!(session.attach_client(first).map(|c| c.id()), None);

    let second = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 40, rows: 10 },
        tab,
    );
    let displaced = session.attach_client(second);

    assert_eq!(
        displaced.map(|c| c.viewport()),
        Some(Size { cols: 0, rows: 0 })
    );
    assert_eq!(
        session.clients.get(id).map(Client::viewport),
        Some(Size { cols: 40, rows: 10 })
    );
}

#[test]
fn attaching_a_client_before_any_tab_leaves_the_session_starting() {
    // `ClientAttached` only revives a `Detaching` session; a session that
    // has not created its first tab yet is `Starting`, and attaching there
    // is not one of the legal moves out of `Starting` — the session stays
    // `Starting` until its first tab arrives.
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);

    let client = Client::new(
        ClientId::new(),
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 0, rows: 0 },
        tab,
    );
    session.attach_client(client);

    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detach_client_returns_the_exact_record_it_removed() {
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let id = ClientId::new();
    let client = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 12, rows: 3 },
        tab,
    );
    session.attach_client(client);

    let removed = session.detach_client(id);

    assert_eq!(removed.map(|c| c.id()), Some(id));
    assert!(session.clients.get(id).is_none());
}

#[test]
fn detach_client_on_an_unattached_id_returns_none() {
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert!(session.detach_client(ClientId::new()).is_none());
}

#[test]
fn request_stop_is_idempotent_once_already_stopping() {
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);

    // A second request from `Stopping` is rejected by the state machine and
    // silently ignored here; the session must not move or panic.
    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn complete_stop_before_a_stop_was_requested_is_a_noop() {
    // `StopCompleted` is only legal from `Stopping`; calling it on a fresh
    // (`Starting`) session is an illegal transition the wrapper swallows.
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    session.complete_stop();

    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detaching_the_last_client_of_a_stopping_session_does_not_revert_it() {
    // A session already winding down (`Stopping`) that loses its last client
    // must stay `Stopping`, never fall back to `Detaching` — that would be a
    // step backward in the shutdown sequence.
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let id = ClientId::new();
    let client = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 0, rows: 0 },
        tab,
    );
    session.attach_client(client);
    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);

    let removed = session.detach_client(id);

    assert_eq!(removed.map(|c| c.id()), Some(id));
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}
