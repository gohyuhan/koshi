//! One [`Event`] reduced to its name and the ids it named.
//!
//! [`RecentEvent`] is one line of `koshi debug events`: when the record was
//! stamped, which [`Event`] variant it was, and the ids that variant's payload
//! holds. It carries no payload content for any event class — no character a
//! user typed, no submitted line, no selection, no pane title, no plugin
//! failure message.
//!
//! [`record`] builds one. Its match has no wildcard arm: a new [`Event`]
//! variant does not compile until [`record`] names the ids it holds.

use std::borrow::Cow;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::event::{Event, PluginEvent};
use crate::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, SubscriberId, TabId};

/// One event as the recent-events ring remembers it.
///
/// Every id field is `None` when the event's payload names no id of that kind.
/// [`Event::PaneCreated`] fills [`pane`](Self::pane) and [`tab`](Self::tab) and
/// leaves the other five empty.
///
/// One id per kind. An event naming two ids of one kind records the one it
/// changed to: [`Event::PaneFocused`] records the pane focused and not its
/// `prior_pane`, and [`Event::TabFocused`] records the tab focused and not its
/// `prior_tab`.
///
/// Decoding ignores a field this build does not know, so a record from a newer
/// koshi still reads. An absent id field reads as `None`; an absent
/// [`at`](Self::at) or [`name`](Self::name) is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEvent {
    /// The moment the caller stamped the record with. `recent_events::record`
    /// in `koshi-observability` passes the wall clock reading of the moment it
    /// ran.
    pub at: SystemTime,
    /// The event variant's name, e.g. `"PaneCreated"` — the string
    /// [`Event::name`] returns. Borrowed while the record stays in the process
    /// that made it, owned once it is decoded from the wire.
    pub name: Cow<'static, str>,
    /// The session the event named.
    pub session: Option<SessionId>,
    /// The client the event named.
    pub client: Option<ClientId>,
    /// The tab the event named.
    pub tab: Option<TabId>,
    /// The pane the event named.
    pub pane: Option<PaneId>,
    /// The plugin the event named.
    pub plugin: Option<PluginId>,
    /// The command the event named.
    pub command: Option<CommandId>,
    /// The subscriber the event named.
    pub subscriber: Option<SubscriberId>,
}

/// Build the record for `event`, stamped `at`.
///
/// Reads the variant name and the ids its payload holds. Reads no payload
/// field carrying text or a measurement. The match has no wildcard arm: a new
/// [`Event`] variant does not compile until it names its ids here.
///
/// Example: `record(&Event::PaneCreated(PaneCreated { pane_id, tab_id }), at)`
/// results in a record whose `name` is `"PaneCreated"`, whose `pane` and `tab`
/// hold those two ids, and whose other five id fields are `None`.
#[must_use]
#[deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
pub fn record(event: &Event, at: SystemTime) -> RecentEvent {
    let blank = RecentEvent {
        at,
        name: Cow::Borrowed(event.name()),
        session: None,
        client: None,
        tab: None,
        pane: None,
        plugin: None,
        command: None,
        subscriber: None,
    };
    match event {
        Event::PaneCreated(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            tab: Some(payload.tab_id),
            ..blank
        },
        Event::PaneProcessExited(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::PaneClosing(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::PaneRemoved(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            tab: Some(payload.tab_id),
            ..blank
        },
        Event::PaneFocused(payload) => RecentEvent {
            client: Some(payload.client_id),
            tab: Some(payload.tab_id),
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::PtyResized(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::PaneOutputUpdated(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::LayoutChanged(payload) => RecentEvent {
            tab: Some(payload.tab_id),
            ..blank
        },
        Event::TabCreated(payload) => RecentEvent {
            tab: Some(payload.tab_id),
            ..blank
        },
        Event::TabClosed(payload) => RecentEvent {
            tab: Some(payload.tab_id),
            ..blank
        },
        Event::TabFocused(payload) => RecentEvent {
            client: Some(payload.client_id),
            tab: Some(payload.tab_id),
            ..blank
        },
        Event::TabMoved(payload) => RecentEvent {
            tab: Some(payload.tab_id),
            ..blank
        },
        Event::PaneSuppressed(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            tab: Some(payload.tab_id),
            ..blank
        },
        Event::PaneResumed(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            tab: Some(payload.tab_id),
            ..blank
        },
        Event::TerminalTooSmallEntered(payload) => RecentEvent {
            client: Some(payload.client_id),
            ..blank
        },
        Event::TerminalTooSmallExited(payload) => RecentEvent {
            client: Some(payload.client_id),
            ..blank
        },
        Event::ConfigReloaded(payload) => RecentEvent {
            session: Some(payload.session_id),
            ..blank
        },
        Event::InputModeChanged(payload) => RecentEvent {
            client: Some(payload.client_id),
            ..blank
        },
        Event::MouseSelectChanged(payload) => RecentEvent {
            client: Some(payload.client_id),
            ..blank
        },
        Event::KeybindingMatched(payload) => RecentEvent {
            client: Some(payload.client_id),
            command: Some(payload.command_id),
            ..blank
        },
        Event::PaneTyped(payload) => RecentEvent {
            session: Some(payload.session_id),
            client: Some(payload.client_id),
            tab: Some(payload.tab_id),
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::PaneEnterPressed(payload) => RecentEvent {
            session: Some(payload.session_id),
            client: Some(payload.client_id),
            tab: Some(payload.tab_id),
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::MousePressed(payload) => RecentEvent {
            client: Some(payload.client_id),
            pane: payload.pane,
            ..blank
        },
        Event::MouseReleased(payload) => RecentEvent {
            client: Some(payload.client_id),
            pane: payload.pane,
            ..blank
        },
        Event::MouseDragged(payload) => RecentEvent {
            client: Some(payload.client_id),
            pane: payload.pane,
            ..blank
        },
        Event::MouseScrolled(payload) => RecentEvent {
            client: Some(payload.client_id),
            pane: payload.pane,
            ..blank
        },
        Event::PaneMouseForwarded(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::PluginMouseInput(payload) => RecentEvent {
            plugin: Some(payload.plugin_id),
            ..blank
        },
        Event::PaneCommandStarted(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::PaneCommandFinished(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::PaneScrollbackTruncated(payload) => RecentEvent {
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::SubscriberLagged(payload) => RecentEvent {
            subscriber: Some(payload.subscriber_id),
            ..blank
        },
        Event::CommandRejected(payload) => RecentEvent {
            command: Some(payload.id),
            ..blank
        },
        Event::SelectionChanged(payload) => RecentEvent {
            client: Some(payload.client_id),
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::Copied(payload) => RecentEvent {
            client: Some(payload.client_id),
            pane: Some(payload.pane_id),
            ..blank
        },
        Event::Plugin(plugin_event) => RecentEvent {
            plugin: Some(plugin_id(plugin_event)),
            ..blank
        },
        Event::Quit | Event::Restarting => blank,
    }
}

/// The plugin `event` names. Every [`PluginEvent`] variant carries one.
#[deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
fn plugin_id(event: &PluginEvent) -> PluginId {
    match event {
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
