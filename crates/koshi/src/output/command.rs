//! Output from an applied command, and the line `kill-session` prints.

use koshi_core::event::Event;

use crate::session_end::SessionEnding;
use koshi_core::text::format_counted_noun;

/// Render the ids created by `command_events`, in event order.
///
/// Events that did not create a tab or pane produce no output.
#[must_use]
pub fn render_created_events(command_events: &[Event]) -> String {
    let mut rendered_output = String::new();
    for command_event in command_events {
        match command_event {
            Event::TabCreated(created_tab) => {
                rendered_output.push_str(&format!("[TAB ID]: {}\n", created_tab.tab_id));
            }
            Event::PaneCreated(created_pane) => {
                rendered_output.push_str(&format!("[PANE ID]: {}\n", created_pane.pane_id));
            }
            _ => {}
        }
    }
    rendered_output
}

/// Render how `kill-session` ended a session, as one line, or nothing.
///
/// - [`SessionEnding::Quit`] with `0`: nothing.
/// - [`SessionEnding::Quit`] with `2`: `the session quit; koshi ended 2
///   processes it left running`.
/// - [`SessionEnding::Stopped`] with `IPC unavailable: the session did not
///   answer in time`, `5000` and `3`: `the session did not quit (IPC
///   unavailable: the session did not answer in time); koshi ended its process
///   5000 and 3 processes under it`.
#[must_use]
pub fn render_session_ending(session_ending: &SessionEnding) -> String {
    match session_ending {
        SessionEnding::Quit {
            stopped_process_count: 0,
        } => String::new(),
        SessionEnding::Quit {
            stopped_process_count,
        } => format!(
            "the session quit; koshi ended {} it left running\n",
            format_counted_noun(*stopped_process_count, "process", "processes")
        ),
        SessionEnding::Stopped {
            quit_failure,
            session_process_id,
            stopped_process_count,
        } => format!(
            "the session did not quit ({quit_failure}); koshi ended its process {session_process_id} and {} under it\n",
            format_counted_noun(*stopped_process_count, "process", "processes")
        ),
    }
}

#[cfg(test)]
mod tests;
