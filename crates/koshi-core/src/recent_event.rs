//! One [`Event`] reduced to its name and the ids it named.
//!
//! [`RecentEvent`] is one line of `koshi debug events`: when the record was
//! stamped, which [`Event`] variant it was, and the ids that variant's payload
//! holds. It carries no payload content for any event class — no character a
//! user typed, no submitted line, no selection, no pane title, no plugin
//! failure message.
//!
//! [`record_event`] builds one. Its match has no wildcard arm: a new [`Event`]
//! variant does not compile until [`record_event`] names the ids it holds.

use std::borrow::Cow;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::event::{Event, PluginEvent};
use crate::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, SubscriberId, TabId};

/// One event as the recent-events ring remembers it.
///
/// Every id field is `None` when the event's payload names no id of that kind.
/// [`Event::PaneCreated`] fills [`pane_id`](Self::pane_id) and [`tab_id`](Self::tab_id) and
/// leaves the other five empty.
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
    /// The plugin the event named.
    pub plugin_id: Option<PluginId>,
    /// The command the event named.
    pub command_id: Option<CommandId>,
    /// The subscriber the event named.
    pub subscriber_id: Option<SubscriberId>,
}

/// Build the record for `event`, stamped `occurred_at`.
///
/// Reads the variant name and the ids its payload holds. Reads no payload
/// field carrying text or a measurement. The match has no wildcard arm: a new
/// [`Event`] variant does not compile until it names its ids here.
///
/// Example: `record_event(&Event::PaneCreated(PaneCreated { pane_id, tab_id }), occurred_at)`
/// results in a record whose `event_name` is `"PaneCreated"`, whose `pane_id`
/// and `tab_id`
/// hold those two ids, and whose other five id fields are `None`.
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
        plugin_id: None,
        command_id: None,
        subscriber_id: None,
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
        Event::PaneOutputUpdated(payload) => RecentEvent {
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
        Event::PaneSuppressed(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            tab_id: Some(payload.tab_id),
            ..empty_recent_event
        },
        Event::PaneResumed(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            tab_id: Some(payload.tab_id),
            ..empty_recent_event
        },
        Event::TerminalTooSmallEntered(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            ..empty_recent_event
        },
        Event::TerminalTooSmallExited(payload) => RecentEvent {
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
        Event::KeybindingMatched(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            command_id: Some(payload.command_id),
            ..empty_recent_event
        },
        Event::PaneTyped(payload) => RecentEvent {
            session_id: Some(payload.session_id),
            client_id: Some(payload.client_id),
            tab_id: Some(payload.tab_id),
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::PaneEnterPressed(payload) => RecentEvent {
            session_id: Some(payload.session_id),
            client_id: Some(payload.client_id),
            tab_id: Some(payload.tab_id),
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::MousePressed(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            pane_id: payload.pane_id,
            ..empty_recent_event
        },
        Event::MouseReleased(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            pane_id: payload.pane_id,
            ..empty_recent_event
        },
        Event::MouseDragged(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            pane_id: payload.pane_id,
            ..empty_recent_event
        },
        Event::MouseScrolled(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            pane_id: payload.pane_id,
            ..empty_recent_event
        },
        Event::PaneMouseForwarded(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::PluginMouseInput(payload) => RecentEvent {
            plugin_id: Some(payload.plugin_id),
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
        Event::PaneScrollbackTruncated(payload) => RecentEvent {
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::SubscriberLagged(payload) => RecentEvent {
            subscriber_id: Some(payload.subscriber_id),
            ..empty_recent_event
        },
        Event::CommandRejected(payload) => RecentEvent {
            command_id: Some(payload.command_id),
            ..empty_recent_event
        },
        Event::SelectionChanged(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::Copied(payload) => RecentEvent {
            client_id: Some(payload.client_id),
            pane_id: Some(payload.pane_id),
            ..empty_recent_event
        },
        Event::Plugin(plugin_event) => RecentEvent {
            plugin_id: Some(get_plugin_id(plugin_event)),
            ..empty_recent_event
        },
        Event::Quit(_) | Event::Restarting => empty_recent_event,
    }
}

/// The plugin event names. Every [`PluginEvent`] variant carries one.
#[deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
fn get_plugin_id(plugin_event: &PluginEvent) -> PluginId {
    match plugin_event {
        PluginEvent::Installed(payload) => payload.plugin_id,
        PluginEvent::Uninstalled(payload) => payload.plugin_id,
        PluginEvent::Enabled(payload) => payload.plugin_id,
        PluginEvent::Disabled(payload) => payload.plugin_id,
        PluginEvent::Updated(payload) => payload.plugin_id,
        PluginEvent::Reloaded(payload) => payload.plugin_id,
        PluginEvent::LoadFailed(payload) => payload.plugin_id,
        PluginEvent::Unloaded(payload) => payload.plugin_id,
        PluginEvent::Broken(payload) => payload.plugin_id,
        PluginEvent::DoctorCompleted(payload) => payload.plugin_id,
    }
}

#[cfg(test)]
mod tests;
