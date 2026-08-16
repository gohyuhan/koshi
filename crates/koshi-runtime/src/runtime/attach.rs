//! The attach-structure builder: copying a live [`Session`] into the
//! [`AttachedSessionStructureSnapshot`] the server sends a client on attach.
//!
//! [`session_structure`] is a plain read-only mapping. It solves nothing and
//! checks nothing: the layout trees travel as they are. The client solves them
//! against its own terminal size, and the attach handler decides whether a
//! session is fit to attach to before it calls this.
//!
//! Both lists are sorted here. Tabs come out in display order (`Tab::index`),
//! so a client draws its tab bar straight from the list; panes come out by
//! [`PaneId`](koshi_core::ids::PaneId). Sorting is what makes the payload
//! deterministic —
//! [`PaneRegistry`](koshi_pane::registry::PaneRegistry) stores records in a
//! [`HashMap`](std::collections::HashMap), whose iteration order varies between
//! processes.

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

    let mut panes: Vec<PaneStructure> = session
        .panes
        .list()
        .map(|record| PaneStructure {
            id: record.id(),
            kind: record.kind().clone(),
        })
        .collect();
    panes.sort_by_key(|pane| pane.id);

    AttachedSessionStructureSnapshot {
        id: session.id,
        name: session.name.clone(),
        tabs,
        panes,
    }
}

#[cfg(test)]
mod tests;
