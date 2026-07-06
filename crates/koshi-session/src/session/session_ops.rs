//! Session operations: the pure state transitions for the session itself.
//!
//! Each operation mutates the session and returns the [`Event`]s describing
//! what changed, for the caller to emit. They draft events and edit state
//! only — never spawning or killing a process or touching a terminal.

use koshi_core::event::{Event, SessionRenamed};

use crate::session::state::Session;

/// Rename the session: set its display name.
///
/// A no-op (no event) when the name is unchanged. Session names address
/// sessions in attach and list, so they must be unique across the runtime;
/// the runtime supplies a generated name it has confirmed free. Tabs,
/// layout, focus, and PTYs are untouched. Returns
/// [`Event::SessionRenamed`].
#[must_use]
pub fn rename_session(session: &mut Session, new_name: String) -> Vec<Event> {
    if session.name == new_name {
        return Vec::new(); // unchanged, nothing to emit
    }
    let old_name = std::mem::replace(&mut session.name, new_name.clone());
    vec![Event::SessionRenamed(SessionRenamed {
        session_id: session.id,
        old_name,
        new_name,
    })]
}

#[cfg(test)]
mod tests;
