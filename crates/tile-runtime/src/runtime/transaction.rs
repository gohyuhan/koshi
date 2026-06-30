//! Event-emission transaction: the buffer a command handler accumulates its
//! [`Event`]s in before they are sealed as one ordered batch.
//!
//! A handler emits events into a [`TransactionScope`] as it mutates runtime
//! state, then [`TransactionScope::commit`] consumes the scope and turns the
//! batch into a [`CommandResult::Ok`], minting one [`EventId`] per event. A
//! scope dropped without committing reports nothing, so a handler that fails
//! partway leaves no events behind. Delivery of the events to subscribers is
//! layered on by the event bus; this module only buffers and seals the batch.

use tile_core::{
    command::CommandResult,
    event::Event,
    ids::{CommandId, EventId},
};

/// An ordered buffer of the [`Event`]s one command emits, sealed by
/// [`commit`](TransactionScope::commit) into a [`CommandResult`].
#[derive(Debug, Default)]
pub struct TransactionScope {
    /// Buffered events, in emission order.
    events: Vec<Event>,
}

impl TransactionScope {
    /// An empty scope, holding no events.
    #[must_use]
    pub fn new() -> Self {
        TransactionScope { events: Vec::new() }
    }

    /// The buffered events, in emission order.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Append `event` to the batch, after the events already emitted.
    pub fn emit(&mut self, event: Event) {
        self.events.push(event);
    }

    /// Consume the scope and seal its batch: mint one [`EventId`] per buffered
    /// event and report them as an applied [`CommandResult::Ok`] keyed to
    /// `command_id`. The events keep their emission order.
    ///
    /// Until the event bus exists, the event payloads are dropped once their ids
    /// are minted; only the ids reach the result.
    #[must_use]
    pub fn commit(self, command_id: CommandId) -> CommandResult {
        let emitted_events = self.events.iter().map(|_| EventId::new()).collect();
        CommandResult::Ok {
            command_id,
            emitted_events,
        }
    }
}

#[cfg(test)]
mod tests;
