//! Output from an applied command.

use koshi_core::event::Event;

/// Render the ids created by `events`, in event order.
///
/// Events that did not create a tab or pane produce no output.
#[must_use]
pub fn render_created_events(events: &[Event]) -> String {
    let mut rendered = String::new();
    for event in events {
        match event {
            Event::TabCreated(created) => {
                rendered.push_str(&format!("[TAB ID]: {}\n", created.tab_id));
            }
            Event::PaneCreated(created) => {
                rendered.push_str(&format!("[PANE ID]: {}\n", created.pane_id));
            }
            _ => {}
        }
    }
    rendered
}

#[cfg(test)]
mod tests;
