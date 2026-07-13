//! Tests for the session state ops.
//!
//! [`rename_session`] tests assert the name write and the emitted event;
//! name generation and cross-session uniqueness are the runtime's.

use std::time::SystemTime;

use koshi_core::event::{Event, SessionRenamed};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_pane::pane::state::PaneRecord;

use super::rename_session;
use crate::client::{Client, ClientRegistry};
use crate::session::state::{Session, Tab};

/// A session named `name` with no tabs, panes, or clients.
fn bare_session(name: &str) -> Session {
    Session::new(
        SessionId::new(),
        name.to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

#[test]
fn rename_session_sets_the_name_and_emits() {
    let mut session = bare_session("main");
    let session_id = session.id;

    let events = rename_session(&mut session, "work".to_string());

    assert_eq!(session.name, "work");
    assert_eq!(
        events,
        vec![Event::SessionRenamed(SessionRenamed {
            session_id,
            old_name: "main".to_string(),
            new_name: "work".to_string(),
        })]
    );
}

#[test]
fn rename_session_to_its_current_name_is_a_no_op() {
    let mut session = bare_session("main");

    let events = rename_session(&mut session, "main".to_string());

    assert_eq!(events, Vec::new());
    assert_eq!(session.name, "main");
}

#[test]
fn renaming_twice_reports_the_immediately_prior_name_each_time() {
    // `old_name` on the second rename must be "work" (what the session was
    // just called), never "main" (the name from two renames ago) — a bug
    // here would surface as a rename event that looks like it undid the
    // first rename.
    let mut session = bare_session("main");
    let session_id = session.id;

    let first = rename_session(&mut session, "work".to_string());
    let second = rename_session(&mut session, "play".to_string());

    assert_eq!(
        first,
        vec![Event::SessionRenamed(SessionRenamed {
            session_id,
            old_name: "main".to_string(),
            new_name: "work".to_string(),
        })]
    );
    assert_eq!(
        second,
        vec![Event::SessionRenamed(SessionRenamed {
            session_id,
            old_name: "work".to_string(),
            new_name: "play".to_string(),
        })]
    );
    assert_eq!(session.name, "play");
}

#[test]
fn rename_session_accepts_an_empty_name() {
    // This layer performs no validation — the runtime supplies a
    // pre-confirmed generated name — so an empty string is stored verbatim
    // rather than rejected here.
    let mut session = bare_session("main");

    let events = rename_session(&mut session, String::new());

    assert_eq!(session.name, "");
    assert!(matches!(
        events.as_slice(),
        [Event::SessionRenamed(r)] if r.new_name.is_empty() && r.old_name == "main"
    ));
}

#[test]
fn rename_session_stores_a_multi_byte_generated_name_exactly() {
    // Generated names may be Japanese or Traditional Chinese
    // (`naming::generate_name`), e.g. `S-しずか-りす`; the rename path must
    // not truncate or mangle multi-byte UTF-8.
    let mut session = bare_session("main");

    let events = rename_session(&mut session, "S-快樂-書房".to_string());

    assert_eq!(session.name, "S-快樂-書房");
    assert!(matches!(
        events.as_slice(),
        [Event::SessionRenamed(r)] if r.new_name == "S-快樂-書房"
    ));
}

#[test]
fn rename_session_touches_only_the_name() {
    // Per the module doc: tabs, layout, focus, and PTYs are untouched by a
    // session rename. Build a session with a tab, a pane, and an attached,
    // focused client, rename it, and assert every one of those is bit-for-bit
    // unchanged.
    let mut session = bare_session("main");
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, "code".to_owned(), 0, pane_id));
    session
        .panes
        .insert(PaneRecord::new(pane_id, SystemTime::UNIX_EPOCH))
        .expect("unique pane id");
    let mut client = Client::new(
        ClientId::new(),
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        tab_id,
    );
    client.update_focused_pane(tab_id, pane_id);
    let client_id = client.id();
    session.attach_client(client);
    let layout_before = session.tabs[&tab_id].layout().clone();

    let _ = rename_session(&mut session, "work".to_string());

    assert_eq!(session.name, "work");
    assert_eq!(*session.tabs[&tab_id].layout(), layout_before);
    assert_eq!(session.tabs[&tab_id].name(), "code");
    assert!(session.panes.get(pane_id).is_some());
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        Some(pane_id)
    );
}
