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
//! - **Faster than a person acts** — terminal content ticking over, a mouse
//!   moving, a key resolving to a command, a window edge being dragged. They go
//!   to the recent-events buffer (`koshi debug events`) instead.
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

use koshi_core::event::{Event, PluginEvent};

/// Write one log line for `event`, at the level its outcome deserves, or write
/// nothing when the event is one of the high-frequency kinds the [logging
/// policy](super#logging-policy) keeps out of the file.
///
/// Example: a `new-pane` binding commits [`Event::PaneCreated`] and
/// [`Event::PaneFocused`], which become two `info` lines carrying the pane and
/// tab ids. Typing `ls` into that pane commits [`Event::PaneTyped`] twice and
/// writes nothing.
pub fn log_event(event: &Event) {
    match event {
        // --- pane and tab lifecycle: one line per fact a person can point at.
        Event::PaneCreated(payload) => {
            tracing::info!(pane_id = %payload.pane_id, tab_id = %payload.tab_id, "pane created");
        }
        Event::PaneProcessExited(payload) => {
            // `exit_code` is `None` for a signal-terminated child; the line
            // then carries no `exit_code` field.
            tracing::info!(
                pane_id = %payload.pane_id,
                exit_code = payload.exit_code,
                "pane process exited"
            );
        }
        Event::PaneRemoved(payload) => {
            tracing::info!(pane_id = %payload.pane_id, tab_id = %payload.tab_id, "pane removed");
        }
        Event::PaneFocused(payload) => {
            tracing::info!(
                client_id = %payload.client_id,
                tab_id = %payload.tab_id,
                pane_id = %payload.pane_id,
                "pane focused"
            );
        }
        Event::TabCreated(payload) => {
            tracing::info!(tab_id = %payload.tab_id, "tab created");
        }
        Event::TabClosed(payload) => {
            tracing::info!(tab_id = %payload.tab_id, "tab closed");
        }
        Event::TabFocused(payload) => {
            tracing::info!(
                client_id = %payload.client_id,
                tab_id = %payload.tab_id,
                "tab focused"
            );
        }
        Event::TabMoved(payload) => {
            tracing::info!(
                tab_id = %payload.tab_id,
                old_index = payload.old_index,
                new_index = payload.new_index,
                "tab moved"
            );
        }

        // --- whole-screen visibility: one line on entering, one on leaving.
        Event::TerminalTooSmallEntered(payload) => {
            tracing::info!(
                client_id = %payload.client_id,
                cols = payload.size.cols,
                rows = payload.size.rows,
                pane_area = ?payload.pane_area,
                cause = ?payload.cause,
                "terminal too small; panes hidden"
            );
        }
        Event::TerminalTooSmallExited(payload) => {
            tracing::info!(
                client_id = %payload.client_id,
                cols = payload.size.cols,
                rows = payload.size.rows,
                "terminal big enough again; panes shown"
            );
        }

        // --- config.
        Event::ConfigReloaded(payload) => {
            tracing::info!(session_id = %payload.session_id, "config reloaded");
        }

        // --- input mode.
        Event::InputModeChanged(payload) => {
            tracing::info!(
                client_id = %payload.client_id,
                mode = ?payload.mode,
                "input mode changed"
            );
        }

        // --- mouse select.
        Event::MouseSelectChanged(payload) => {
            tracing::info!(
                client_id = %payload.client_id,
                on = payload.on,
                "mouse select changed"
            );
        }

        // --- copy: the byte count only; the copied text never reaches the file.
        Event::Copied(payload) => {
            tracing::info!(
                client_id = %payload.client_id,
                pane_id = %payload.pane_id,
                target = ?payload.target,
                byte_len = payload.byte_len,
                "copied"
            );
        }

        // --- delivery failures koshi has an answer for.
        Event::SubscriberLagged(payload) => {
            tracing::warn!(
                subscriber_id = %payload.subscriber_id,
                dropped_count = payload.dropped_count,
                event_class = ?payload.event_class,
                "subscriber queue overflowed; events dropped"
            );
        }
        // --- session end.
        Event::Quit => tracing::info!("session quitting"),

        // --- image swap: the session keeps running under a new process image.
        Event::Restarting => tracing::info!("session restarting into the binary on disk"),

        Event::Plugin(plugin_event) => log_plugin_event(plugin_event),

        // --- no line. The module doc names the three kinds left out.
        //
        // Faster than a person acts: `PaneOutputUpdated` fires per burst of
        // shell output; `PtyResized`, `LayoutChanged`, `PaneSuppressed` and
        // `PaneResumed` fire once per pane per frame of a window drag;
        // `PaneScrollbackTruncated` fires while a pane prints past its buffer;
        // `KeybindingMatched`, `PaneTyped`, `PaneEnterPressed`, the four mouse
        // events, `PaneMouseForwarded`, `PluginMouseInput` and
        // `SelectionChanged` fire once per keystroke or per mouse motion.
        //
        // Content: the typed events carry what the user typed.
        //
        // Announcements: `PaneClosing` starts the close `PaneRemoved`
        // completes. `CommandRejected` is written where the rejection is built.
        //
        // Not koshi's state: `PaneCommandStarted` and `PaneCommandFinished`
        // report what the shell inside a pane is doing.
        Event::PaneClosing(_)
        | Event::CommandRejected(_)
        | Event::PtyResized(_)
        | Event::PaneOutputUpdated(_)
        | Event::LayoutChanged(_)
        | Event::PaneSuppressed(_)
        | Event::PaneResumed(_)
        | Event::KeybindingMatched(_)
        | Event::PaneTyped(_)
        | Event::PaneEnterPressed(_)
        | Event::MousePressed(_)
        | Event::MouseReleased(_)
        | Event::MouseDragged(_)
        | Event::MouseScrolled(_)
        | Event::PaneMouseForwarded(_)
        | Event::PluginMouseInput(_)
        | Event::PaneCommandStarted(_)
        | Event::PaneCommandFinished(_)
        | Event::PaneScrollbackTruncated(_)
        | Event::SelectionChanged(_) => {}
    }
}

/// Write one log line for a plugin lifecycle fact. Every variant writes a
/// line: `LoadFailed` and `Broken` are `warn`, the rest are `info`.
fn log_plugin_event(event: &PluginEvent) {
    match event {
        PluginEvent::Installed(payload) => {
            tracing::info!(plugin_id = %payload.plugin_id, "plugin installed");
        }
        PluginEvent::Uninstalled(payload) => {
            tracing::info!(plugin_id = %payload.plugin_id, "plugin uninstalled");
        }
        PluginEvent::Enabled(payload) => {
            tracing::info!(plugin_id = %payload.plugin_id, "plugin enabled");
        }
        PluginEvent::Disabled(payload) => {
            tracing::info!(plugin_id = %payload.plugin_id, "plugin disabled");
        }
        PluginEvent::Updated(payload) => {
            tracing::info!(plugin_id = %payload.plugin_id, "plugin updated");
        }
        PluginEvent::Reloaded(payload) => {
            tracing::info!(plugin_id = %payload.plugin_id, "plugin reloaded");
        }
        PluginEvent::Unloaded(payload) => {
            tracing::info!(plugin_id = %payload.plugin_id, "plugin unloaded");
        }
        PluginEvent::DoctorCompleted(payload) => {
            tracing::info!(plugin_id = %payload.plugin_id, "plugin diagnostic completed");
        }
        PluginEvent::LoadFailed(payload) => {
            tracing::warn!(
                plugin_id = %payload.plugin_id,
                reason = %payload.reason,
                "plugin failed to load; continuing without it"
            );
        }
        PluginEvent::Broken(payload) => {
            tracing::warn!(
                plugin_id = %payload.plugin_id,
                reason = %payload.reason,
                "plugin marked broken and disabled"
            );
        }
    }
}

#[cfg(test)]
mod tests;
