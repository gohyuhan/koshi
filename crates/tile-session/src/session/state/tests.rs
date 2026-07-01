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
    let mut session = Session::new(SessionId::new(), "s".to_owned(), ClientRegistry::new());

    // Two clients view `tab` with opposite aspect ratios.
    viewer(&mut session, tab, 80, 5);
    viewer(&mut session, tab, 40, 24);
    // A client on a different tab must not count.
    viewer(&mut session, other_tab, 10, 1);

    // Per-axis minimum: smallest cols (40) and smallest rows (5), independently —
    // not the lexicographically smaller viewport (which would keep 24 rows and
    // overflow the 5-row viewer).
    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 40, rows: 5 }));
}

#[test]
fn tab_viewport_is_none_without_a_viewer() {
    let tab = TabId::new();
    let session = Session::new(SessionId::new(), "s".to_owned(), ClientRegistry::new());

    assert_eq!(session.tab_viewport(tab), None);
}
