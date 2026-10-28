//! Event-emission transaction: the buffer a command handler accumulates its
//! [`Event`]s in before they are sealed as one ordered batch.
//!
//! A handler emits events into a [`TransactionScope`] as it mutates runtime
//! state, then [`TransactionScope::commit`] consumes the scope and turns the
//! batch into a [`CommandResult::Ok`] containing the same ordered [`Event`]s.
//! A scope dropped without committing reports nothing, so a handler that
//! fails partway leaves no events behind.
//!
//! Sealing is also where each event becomes a log line, via
//! [`koshi_observability::logging::event_log::log_event`], where it is added to
//! the server's recent-events ring, via [`RecentEventRing::record_event`], and
//! where the batch is published on the [`EventBus`]. Every committed event
//! passes through here; an uncommitted scope logs nothing, records nothing and
//! publishes nothing.

use koshi_core::{command::CommandResult, event::Event, ids::CommandId};
use koshi_observability::logging::event_log::log_event;
use koshi_observability::logging::recent_events::RecentEventRing;

use crate::runtime::bus::EventBus;

/// An ordered buffer of the [`Event`]s one command emits, sealed by
/// [`commit`](TransactionScope::commit) into a [`CommandResult`].
#[derive(Debug)]
pub(crate) struct TransactionScope {
    /// Buffered events, in emission order.
    emitted_events: Vec<Event>,
}

impl TransactionScope {
    /// An empty scope, holding no events.
    #[must_use]
    pub(crate) fn new() -> Self {
        TransactionScope {
            emitted_events: Vec::new(),
        }
    }

    /// Append `event` to the batch, after the events already emitted.
    pub(crate) fn emit(&mut self, event: Event) {
        self.emitted_events.push(event);
    }

    /// Consume the scope and seal its batch: write each buffered event to the
    /// log and to `recent_event_ring`, publish it on `event_bus`
    /// ([`EventBus::publish`] names the events that reach a subscriber), and
    /// report the same ordered events as an applied
    /// [`CommandResult::Ok`] keyed to `command_id`.
    #[must_use]
    pub(crate) fn commit(
        self,
        command_id: CommandId,
        event_bus: &mut EventBus,
        recent_event_ring: &RecentEventRing,
    ) -> CommandResult {
        for event in &self.emitted_events {
            log_event(event);
            recent_event_ring.record_event(event);
            event_bus.publish(event);
        }
        CommandResult::Ok {
            command_id,
            emitted_events: self.emitted_events,
        }
    }
}

#[cfg(test)]
mod tests;
