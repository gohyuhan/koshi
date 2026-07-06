//! Tests for the session state ops.
//!
//! [`rename_session`] tests assert the name write and the emitted event;
//! name generation and cross-session uniqueness are the runtime's.

use koshi_core::event::{Event, SessionRenamed};
use koshi_core::ids::SessionId;

use super::rename_session;
use crate::client::ClientRegistry;
use crate::session::state::Session;

/// A session named `name` with no tabs, panes, or clients.
fn bare_session(name: &str) -> Session {
    Session::new(SessionId::new(), name.to_owned(), ClientRegistry::new())
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
