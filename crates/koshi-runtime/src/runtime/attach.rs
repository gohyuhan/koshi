//! The attach-structure builder: copying a live [`Session`] into the
//! [`AttachedSessionStructureSnapshot`] the server sends a client on attach.
//!
//! [`session_structure`] is a plain read-only mapping. It solves nothing and
//! checks nothing: the layout trees travel as they are. The client solves them
//! against its own terminal size, and the attach handler decides whether a
//! session is fit to attach to before it calls this.
//!
//! Both lists are sorted here: tabs in display order (`Tab::index`), panes
//! ascending by [`PaneId`](koshi_core::ids::PaneId).

use koshi_ipc::attach::{AttachedSessionStructureSnapshot, PaneStructure, TabStructure};
use koshi_session::session::state::Session;

/// Copy `session`'s structure into the form a client attaches with.
///
/// Carries the session's id and name, every tab with its unsolved layout tree
/// and focus history, and every pane's id and kind. Carries no pane content and
/// no per-client state. Tabs come out by display index, panes by id.
#[must_use]
pub fn session_structure(session: &Session) -> AttachedSessionStructureSnapshot {
    let mut tabs: Vec<TabStructure> = session
        .tabs
        .values()
        .map(|tab| TabStructure {
            id: tab.id(),
            name: tab.name().to_string(),
            index: tab.index(),
            layout: tab.layout().clone(),
            focus_mru: tab.focus_mru().to_vec(),
        })
        .collect();
    tabs.sort_by_key(|tab| tab.index);

    // `PaneRegistry::list` walks in id order, so the snapshot is already sorted.
    let panes: Vec<PaneStructure> = session
        .panes
        .list()
        .map(|record| PaneStructure {
            id: record.id(),
            kind: *record.kind(),
        })
        .collect();

    AttachedSessionStructureSnapshot {
        id: session.id,
        name: session.name.clone(),
        tabs,
        panes,
    }
}

#[cfg(test)]
mod tests;
