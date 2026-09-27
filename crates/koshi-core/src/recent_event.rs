//! One [`Event`] reduced to its name and the ids it named.
//!
//! [`RecentEvent`] is one line of `koshi debug events`: when the record was
//! stamped, which [`Event`] variant it was, and the ids that variant's payload
//! holds. It carries no payload content for any event class — no character a
//! user typed, no submitted line, no selection, no pane title.
//!
//! [`record_event`] builds one. Its match has no wildcard arm: a new [`Event`]
//! variant does not compile until [`record_event`] names the ids it holds.

use std::borrow::Cow;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::ids::{ClientId, CommandId, PaneId, SessionId, TabId};

/// One event as the recent-events ring remembers it.
///
/// Every id field is `None` when the event's payload names no id of that kind.
/// [`Event::PaneCreated`] fills [`pane_id`](Self::pane_id) and [`tab_id`](Self::tab_id) and
/// leaves the other three empty.
///
/// One id per kind. An event naming two ids of one kind records the one it
/// changed to: [`Event::PaneFocused`] records the pane focused and not its
/// previous pane, and [`Event::TabFocused`] records the tab focused and not its
/// previous tab.
///
/// Decoding ignores a field this build does not know, so a record from a newer
/// koshi still reads. An absent id field reads as `None`; an absent
/// [`occurred_at`](Self::occurred_at) or [`event_name`](Self::event_name) is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEvent {
    /// Wall-clock time supplied by the caller. `recent_events::record` in
    /// `koshi-observability` supplies the clock reading when it runs.
    pub occurred_at: SystemTime,
    /// The event variant's name, e.g. `"PaneCreated"` — the string
    /// [`Event::get_event_name`] returns. Borrowed while the record stays in the process
    /// that made it, owned once it is decoded from the wire.
    pub event_name: Cow<'static, str>,
    /// The session the event named.
    pub session_id: Option<SessionId>,
    /// The client the event named.
    pub client_id: Option<ClientId>,
    /// The tab the event named.
    pub tab_id: Option<TabId>,
    /// The pane the event named.
    pub pane_id: Option<PaneId>,
    /// The command the event named.
    pub command_id: Option<CommandId>,
}

/// Build the record for `event`, stamped `occurred_at`.
///
/// Reads the variant name and the ids its payload holds. Reads no payload
/// field carrying text or a measurement. The match has no wildcard arm: a new
/// [`Event`] variant does not compile until it names its ids here.
///
/// Example: `record_event(&Event::PaneCreated(PaneCreated { pane_id, tab_id }), occurred_at)`
/// results in a record whose `event_name` is `"PaneCreated"`, whose `pane_id`
/// and `tab_id` hold those two ids, and whose other three id fields are `None`.
#[must_use]
#[deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
pub fn record_event(event: &Event, occurred_at: SystemTime) -> RecentEvent {
    let empty_recent_event = RecentEvent {
        occurred_at,
        event_name: Cow::Borrowed(event.get_event_name()),
        session_id: None,
        client_id: None,
        tab_id: None,
        pane_id: None,
        command_id: None,
    };
    match event {
        Event::PaneCreated(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            tab_id: Some(payload.tab_id),
            ..empty_recent_event
        },
        Event::PaneProcessExited(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::PaneClosing(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::PaneRemoved(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            tab_id: Some(payload.tab_id),
            ..empty_recent_event
        },
        Event::PaneFocused(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            tab_id: Some(payload.tab_id),
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::PtyResized(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::LayoutChanged(payload) => RecentEvent {
            tab_id: Some(payload.tab_id),
            ..empty_recent_event
        },
        Event::PanePlacementCommitted(payload) => RecentEvent {
            tab_id: Some(payload.destination_tab_id),
            pane_id: Some(payload.source_pane_id),
            command_id: Some(payload.command_id),
            ..empty_recent_event
        },
        Event::TabCreated(payload) => RecentEvent {
            tab_id: Some(payload.tab_id),
            ..empty_recent_event
        },
        Event::TabClosed(payload) => RecentEvent {
            tab_id: Some(payload.tab_id),
            ..empty_recent_event
        },
        Event::TabFocused(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            tab_id: Some(payload.tab_id),
            ..empty_recent_event
        },
        Event::TabMoved(payload) => RecentEvent {
            tab_id: Some(payload.tab_id),
            ..empty_recent_event
        },
        Event::TerminalTooSmallEntered(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            ..empty_recent_event
        },
        Event::ConfigReloaded(payload) => RecentEvent {
            session_id: Some(payload.session_id),
            ..empty_recent_event
        },
        Event::InputModeChanged(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            ..empty_recent_event
        },
        Event::MouseSelectChanged(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            ..empty_recent_event
        },
        Event::PaneCommandStarted(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::PaneCommandFinished(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::SelectionChanged(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::Quit(_) | Event::Restarting => empty_recent_event,
    }
}

#[cfg(test)]
mod tests;
