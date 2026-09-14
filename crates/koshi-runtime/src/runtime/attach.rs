//! The attach-structure builder: copying a live [`Session`] into the
//! [`AttachedSessionStructureSnapshot`] the server sends a client on attach.
//!
//! [`build_session_structure_snapshot`] is a plain read-only mapping. It solves nothing and
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

    // `PaneRegistry::list` walks in id order, so the snapshot is already sorted.
    let pane_structures: Vec<PaneStructure> = session
        .panes
        .list_pane_records()
        .map(|pane_record| PaneStructure {
            pane_id: pane_record.get_pane_id(),
            pane_kind: *pane_record.get_pane_kind(),
        })
        .collect();

    AttachedSessionStructureSnapshot {
        session_id: session.session_id,
        session_name: session.session_name.clone(),
        tabs,
        panes: pane_structures,
    }
}

#[cfg(test)]
mod tests;
