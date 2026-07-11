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
