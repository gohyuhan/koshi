//! NewPane state transaction: the pure session-state op behind
//! `Command::NewPane` — split a tab's layout, register the new pane, and
//! auto-focus it, returning the events for the caller to emit.
//!
//! Like [`crate::session::tab_ops`], this layer drafts events and edits state
//! only — it never spawns a process or touches a terminal. The runtime attaches
//! the PTY off the returned [`Event::PaneCreated`] and advances the pane from
//! `Spawning` to `Running`. Every edit is gated behind a fit preflight, so a
//! rejected split leaves the session exactly as it was.

use std::path::PathBuf;
use std::time::SystemTime;

use tile_core::event::{Event, LayoutChanged, PaneCreated, PaneFocused};
use tile_core::geometry::{Direction, Point, Rect};
use tile_core::ids::{ClientId, PaneId, TabId};
use tile_core::process::SpawnSpec;
use tile_layout::edit::split_leaf;
use tile_layout::solver::{fits, MIN_PANE_SIZE};
use tile_pane::pane::state::PaneRecord;

use crate::session::state::Session;

/// Why a [`new_pane`] transaction was rejected. The caller maps each variant
/// onto the matching command rejection reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewPaneError {
    /// The source pane has no leaf in its tab's layout.
    SourceNotFound,
    /// The new pane could not fit the viewport at minimum size.
    WontFit,
}

/// What to record on a freshly created pane: its display title and the spawn
/// request (working directory and command) the PTY layer later honors. Bundled
/// so the requested program and cwd are never silently dropped — the new pane's
/// record is self-describing for restore and respawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewPaneSpec {
    /// Optional display title.
    pub name: Option<String>,
    /// Working directory; `None` inherits.
    pub cwd: Option<PathBuf>,
    /// Command to run; `None` launches the default shell.
    pub command: Option<SpawnSpec>,
}

/// Split `tab_id`'s layout next to `source_pane`, registering a fresh
/// `Spawning` terminal pane and focusing it for `focus_client` (when set).
///
/// `direction` is where the new pane lands; `spec` carries the title and spawn
/// request recorded on the new pane so the later PTY spawn (and any restore or
/// respawn) can recover what was requested. `created_at` is supplied by the
/// caller so the timestamp crosses the IPC boundary intact and tests stay
/// deterministic. When `focus_client` names a client with a viewport, the
/// post-split tree is preflighted against it and a split that cannot fit is
/// rejected before anything mutates.
///
/// Returns [`Event::PaneCreated`], [`Event::LayoutChanged`], and — only when a
/// `focus_client` applies — [`Event::PaneFocused`], in that order. On any error
/// the session is left exactly as it was.
///
/// # Errors
///
/// - [`NewPaneError::SourceNotFound`] when `source_pane` is not a leaf of
///   `tab_id`'s layout (or the tab is gone).
/// - [`NewPaneError::WontFit`] when the focus client's viewport cannot hold the
///   split at [`MIN_PANE_SIZE`].
pub fn new_pane(
    session: &mut Session,
    source_pane: PaneId,
    tab_id: TabId,
    direction: Direction,
    focus_client: Option<ClientId>,
    spec: NewPaneSpec,
    created_at: SystemTime,
) -> Result<Vec<Event>, NewPaneError> {
    let new_pane_id = PaneId::new();

    // Only a still-attached client can be focused. Resolving it once here keeps
    // the preflight, focus update, focus-MRU, and `PaneFocused` event in
    // agreement: a stale id focuses nothing, exactly like `None`.
    let focus = focus_client.filter(|client_id| session.clients.get(*client_id).is_some());

    // Build the post-split tree without touching the live one yet.
    let Some(tab) = session.tabs.get(&tab_id) else {
        return Err(NewPaneError::SourceNotFound);
    };
    let candidate = split_leaf(tab.layout(), source_pane, new_pane_id, direction)
        .map_err(|_| NewPaneError::SourceNotFound)?;

    // Preflight fit against the focus client's viewport, when one applies.
    if let Some(client_id) = focus {
        if let Some(client) = session.clients.get(client_id) {
            let rect = Rect::new(Point { x: 0, y: 0 }, client.viewport());
            if !fits(&candidate, rect, MIN_PANE_SIZE) {
                return Err(NewPaneError::WontFit);
            }
        }
    }

    // Past the gate — commit. Register the new pane, recording the spawn
    // request so the later PTY spawn can recover it.
    let mut record = PaneRecord::new(new_pane_id, created_at);
    record.title = spec.name;
    record.cwd = spec.cwd;
    record.command = spec.command;
    let _ = session.panes.insert(record);

    // Swap in the new tree and record focus history.
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
        tab.update_layout(candidate);
        if focus.is_some() {
            tab.record_focus_mru(new_pane_id);
        }
    }

    // Auto-focus the resolved client on the new pane.
    if let Some(client_id) = focus {
        if let Some(client) = session.clients.get_mut(client_id) {
            client.update_focused_pane(tab_id, new_pane_id);
        }
    }

    let mut events = vec![
        Event::PaneCreated(PaneCreated {
            pane_id: new_pane_id,
            tab_id,
        }),
        Event::LayoutChanged(LayoutChanged { tab_id }),
    ];
    if focus.is_some() {
        events.push(Event::PaneFocused(PaneFocused {
            pane_id: new_pane_id,
            tab_id,
        }));
    }
    Ok(events)
}

#[cfg(test)]
mod tests;
