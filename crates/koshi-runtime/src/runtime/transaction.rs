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
//! the recent-events ring, via
//! [`koshi_observability::logging::recent_events::record`], and where the batch
//! is delivered to subscribers over the [`EventBus`]. Every committed event
//! passes through here; an uncommitted scope logs nothing, records nothing and
//! delivers nothing.

use koshi_core::{command::CommandResult, event::Event, ids::CommandId};
use koshi_observability::logging::event_log::log_event;
use koshi_observability::logging::recent_events;

use crate::runtime::bus::EventBus;

/// An ordered buffer of the [`Event`]s one command emits, sealed by
/// [`commit`](TransactionScope::commit) into a [`CommandResult`].
#[derive(Debug, Default)]
pub(crate) struct TransactionScope {
    /// Buffered events, in emission order.
    events: Vec<Event>,
}

impl TransactionScope {
    /// An empty scope, holding no events.
    #[must_use]
    pub(crate) fn new() -> Self {
        TransactionScope { events: Vec::new() }
    }

    /// The buffered events, in emission order.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn events(&self) -> &[Event] {
        &self.events
    }

    /// Append `event` to the batch, after the events already emitted.
    pub(crate) fn emit(&mut self, event: Event) {
        self.events.push(event);
    }

    /// Consume the scope and seal its batch: write each buffered event to the
    /// log and the recent-events ring, deliver it to every subscriber on
    /// `bus`, and report the same ordered events as an applied
    /// [`CommandResult::Ok`] keyed to `command_id`.
    #[must_use]
    pub(crate) fn commit(self, command_id: CommandId, bus: &mut EventBus) -> CommandResult {
        for event in &self.events {
            log_event(event);
            recent_events::record(event);
            bus.publish(event);
        }
        CommandResult::Ok {
            command_id,
            emitted_events: self.events,
        }
    }
}

#[cfg(test)]
mod tests;
