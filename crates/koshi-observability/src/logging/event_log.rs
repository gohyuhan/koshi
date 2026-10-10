//! Turning a committed runtime event into a log line.
//!
//! [`event_log::log_event`] runs once per event as a command's transaction is
//! sealed. The match is exhaustive: a new [`koshi_core::event::Event`] variant
//! does not compile until it is classified here.
//!
//! Levels mean what [the logging domain](super#what-each-level-means) says they
//! mean: `info` for a fact that landed, `warn` for one reporting a failure
//! koshi had an answer for, and no `error` at all.
//!
//! # Which events get a line
//!
//! Only the ones a person could point at: a pane opened, a tab closed, the
//! config applied, the lock mode changed.
//!
//! Four kinds are left out.
//!
//! - **Faster than a person acts** — a selection following the mouse, a
//!   window edge being dragged. They go to the recent-events buffer
//!   (`koshi debug events`) instead.
//! - **Announcements** — an event whose completion has its own event.
//!   [`koshi_core::event::Event::PaneClosing`] starts what
//!   [`koshi_core::event::Event::PaneRemoved`] finishes; only the second is
//!   written.
//! - **Content** — no line carries a display name. A pane title is content: the
//!   program in the pane can set it through the OSC 2 escape sequence. Lines
//!   carry ids only, as the [logging policy](super#logging-policy) says.
//! - **Not koshi's state** — [`koshi_core::event::Event::PaneCommandStarted`]
//!   and [`koshi_core::event::Event::PaneCommandFinished`] report what the
//!   shell inside a pane is doing.

use koshi_core::event::{Event, QuitCause};

/// Write one log line for `runtime_event`, at the level its outcome deserves, or write
/// nothing when the event is one of the high-frequency kinds the [logging
/// policy](super#logging-policy) keeps out of the file.
///
/// Example: a `new-pane` binding commits [`Event::PaneCreated`] and
/// [`Event::PaneFocused`], which become two `info` lines carrying the pane and
/// tab ids. An [`Event::SelectionChanged`] writes nothing.
pub fn log_event(runtime_event: &Event) {
    match runtime_event {
        // --- pane and tab lifecycle: one line per fact a person can point at.
        // A floating pane has no tab: its `tab_id` writes no field.
        Event::PaneCreated(event_payload) => {
            tracing::info!(
                pane_id = %event_payload.pane_id,
                tab_id = event_payload.tab_id.map(tracing::field::display),
                "pane created"
            );
        }
        Event::PaneProcessExited(event_payload) => {
            // Exactly one of `exit_code` and `signal` is `Some`; a `None`
            // writes no field. Exit code `0` logs at info; every other code,
            // and a signal, logs at warn.
            if event_payload.is_failure() {
                tracing::warn!(
                    pane_id = %event_payload.pane_id,
                    exit_code = event_payload.exit_code,
                    signal = event_payload.signal,
                    "pane process exited"
                );
            } else {
                tracing::info!(
                    pane_id = %event_payload.pane_id,
                    exit_code = event_payload.exit_code,
                    signal = event_payload.signal,
                    "pane process exited"
                );
            }
        }
        Event::PaneRemoved(event_payload) => {
            tracing::info!(
                pane_id = %event_payload.pane_id,
                tab_id = event_payload.tab_id.map(tracing::field::display),
                "pane removed"
            );
        }
        Event::PaneFocused(event_payload) => {
            tracing::info!(
                client_id = %event_payload.client_id,
                tab_id = event_payload.tab_id.map(tracing::field::display),
                pane_id = %event_payload.pane_id,
                "pane focused"
            );
        }
        Event::PanePlacementCommitted(event_payload) => {
            tracing::info!(
                command_id = %event_payload.command_id,
                source_pane_id = %event_payload.source_pane_id,
                source_tab_id = event_payload.source_tab_id.map(tracing::field::display),
                destination_tab_id = event_payload.destination_tab_id.map(tracing::field::display),
                placement_target = ?event_payload.placement_target,
                "pane placement committed"
            );
        }
        Event::TabCreated(event_payload) => {
            tracing::info!(tab_id = %event_payload.tab_id, "tab created");
        }
        Event::TabClosed(event_payload) => {
            tracing::info!(tab_id = %event_payload.tab_id, "tab closed");
        }
        Event::TabFocused(event_payload) => {
            tracing::info!(
                client_id = %event_payload.client_id,
                tab_id = %event_payload.tab_id,
                "tab focused"
            );
        }
        Event::TabMoved(event_payload) => {
            tracing::info!(
                tab_id = %event_payload.tab_id,
                previous_tab_index = event_payload.previous_tab_index,
                new_tab_index = event_payload.new_tab_index,
                "tab moved"
            );
        }

        // --- whole-screen visibility: one line on entering, one on leaving.
        Event::TerminalTooSmallEntered(event_payload) => {
            tracing::info!(
                client_id = %event_payload.client_id,
                column_count = event_payload.viewport_size.column_count,
                row_count = event_payload.viewport_size.row_count,
                pane_area = ?event_payload.pane_area,
                cause = ?event_payload.cause,
                "terminal too small; panes hidden"
            );
        }

        // --- config.
        Event::ConfigReloaded(event_payload) => {
            tracing::info!(session_id = %event_payload.session_id, "config reloaded");
        }

        // --- input mode.
        Event::InputModeChanged(event_payload) => {
            tracing::info!(
                client_id = %event_payload.client_id,
                mode = ?event_payload.lock_mode,
                "input mode changed"
            );
        }

        // --- mouse select.
        Event::MouseSelectChanged(event_payload) => {
            tracing::info!(
                client_id = %event_payload.client_id,
                is_enabled = event_payload.is_enabled,
                "mouse select changed"
            );
        }

        // --- floating pane view: one client's move or pin of a floating pane.
        Event::FloatingPaneMoved(event_payload) => {
            tracing::info!(
                client_id = %event_payload.client_id,
                pane_id = %event_payload.pane_id,
                column = event_payload.to.column,
                row = event_payload.to.row,
                "floating pane moved"
            );
        }
        Event::PanePinChanged(event_payload) => {
            tracing::info!(
                client_id = %event_payload.client_id,
                pane_id = %event_payload.pane_id,
                is_pinned = event_payload.is_pinned,
                "floating pane pin changed"
            );
        }

        // --- session end: `cause` is `requested` or `last-tab-closed`. A
        // last-tab close names its tab, and the pane exit that emptied it when
        // one did. Only a close that followed a failed exit logs at warn.
        Event::Quit(QuitCause::Requested) => {
            tracing::info!(cause = "requested", "session quitting");
        }
        Event::Quit(QuitCause::LastTabClosed {
            tab_id,
            pane_exit: None,
        }) => {
            tracing::info!(cause = "last-tab-closed", tab_id = %tab_id, "session quitting");
        }
        Event::Quit(QuitCause::LastTabClosed {
            tab_id,
            pane_exit: Some(pane_exit),
        }) => {
            if pane_exit.is_failure() {
                tracing::warn!(
                    cause = "last-tab-closed",
                    tab_id = %tab_id,
                    pane_id = %pane_exit.pane_id,
                    exit_code = pane_exit.exit_code,
                    signal = pane_exit.signal,
                    "session quitting"
                );
            } else {
                tracing::info!(
                    cause = "last-tab-closed",
                    tab_id = %tab_id,
                    pane_id = %pane_exit.pane_id,
                    exit_code = pane_exit.exit_code,
                    signal = pane_exit.signal,
                    "session quitting"
                );
            }
        }

        // --- image swap: the session keeps running under a new process image.
        Event::Restarting => tracing::info!("session restarting into the binary on disk"),

        // --- no line. The module doc names the kinds left out.
        //
        // Faster than a person acts: `PtyResized` and `LayoutChanged` fire
        // once per pane per frame of a window drag; `SelectionChanged` fires
        // once per mouse motion.
        //
        // Announcements: `PaneClosing` starts the close `PaneRemoved`
        // completes.
        //
        // Not koshi's state: `PaneCommandStarted` and `PaneCommandFinished`
        // report what the shell inside a pane is doing.
        Event::PaneClosing(_)
        | Event::PtyResized(_)
        | Event::LayoutChanged(_)
        | Event::PaneCommandStarted(_)
        | Event::PaneCommandFinished(_)
        | Event::SelectionChanged(_) => {}
    }
}

#[cfg(test)]
mod tests;
