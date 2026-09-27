//! The attach-structure builder: copying a live [`Session`] into the
//! [`AttachedSessionStructureSnapshot`] the server sends a client on attach.
//!
//! [`build_session_structure_snapshot`] is a plain read-only mapping. It solves nothing and
//! checks nothing: the layout trees travel as they are. The client solves them
//! against its own terminal size, and the attach handler decides whether a
//! session is fit to attach to before it calls this.
//!
//! Tabs are sorted here, in display order (`Tab::index`).

use koshi_ipc::attach::{AttachedSessionStructureSnapshot, TabStructure};
use koshi_session::session::state::Session;

/// Copy `session`'s structure into the form a client attaches with.
///
/// Carries the session's id and name, and every tab with its unsolved layout
/// tree and focus history. Carries no pane content and no per-client state.
/// Tabs come out by display index.
#[must_use]
pub fn build_session_structure_snapshot(session: &Session) -> AttachedSessionStructureSnapshot {
    let mut tabs: Vec<TabStructure> = session
        .tabs
        .values()
        .map(|tab| TabStructure {
            tab_id: tab.get_tab_id(),
            tab_name: tab.get_tab_name().to_string(),
            tab_index: tab.get_tab_index(),
            layout: tab.get_layout_tree().clone(),
            focus_mru: tab.list_focus_mru().to_vec(),
        })
        .collect();
    tabs.sort_by_key(|tab| tab.tab_index);

    AttachedSessionStructureSnapshot {
        session_id: session.session_id,
        session_name: session.session_name.clone(),
        tabs,
    }
}

#[cfg(test)]
mod tests;
