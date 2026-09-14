//! Output from an applied command.

use koshi_core::event::Event;

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

#[cfg(test)]
mod tests;
