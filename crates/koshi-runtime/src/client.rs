//! The client half of the in-process ownership split: one attached terminal's
//! view side.
//!
//! A [`Client`] owns what belongs to the terminal in front of the user — its
//! id, its own terminal size, the receiving end of its event subscription, and
//! the guard that restores the outer terminal when the process ends. It holds
//! no session state: everything authoritative lives in
//! [`Server`](crate::server::Server), reached only through
//! [`Server::submit_command`](crate::server::Server::submit_command) and the
//! event feed [`Server::subscribe`](crate::server::Server::subscribe) handed
//! this client at construction. Detaching a client therefore removes a view,
//! never a session.
//!
//! This is a different type from [`koshi_session::client::Client`], the
//! server's per-client record (focus, lock mode, viewport) inside a session's
//! registry: that record is session state about a client; this type is the
//! client itself.

use std::sync::mpsc::Receiver;

use koshi_core::{event::Event, geometry::Size, ids::ClientId};
use koshi_observability::cleanup::TerminalCleanupGuard;

/// One attached terminal's view side: its id, its own terminal size, its
/// event feed from the server, and the outer-terminal restore guard. The
/// binary's event loop drives it; it can never mutate session or pane data.
pub struct Client {
    /// This client's id, the one its input events and commands carry.
    id: ClientId,
    /// The client's own outer-terminal size in cells. Updated from resize
    /// events and reported to the server, which reconciles tab sizes from
    /// every viewer's report; this copy is the client's alone.
    viewport: Size,
    /// Receiving end of this client's event subscription, fed by the server's
    /// bounded fan-out.
    events: Receiver<Event>,
    /// Restores the outer terminal when the client ends or the process
    /// panics.
    cleanup_guard: TerminalCleanupGuard,
}

impl Client {
    /// Build a client from its id, its terminal's current size, the receiver
    /// [`Server::subscribe`](crate::server::Server::subscribe) handed out for
    /// it, and the outer-terminal cleanup guard.
    #[must_use]
    pub fn new(
        id: ClientId,
        viewport: Size,
        events: Receiver<Event>,
        cleanup_guard: TerminalCleanupGuard,
    ) -> Self {
        Client {
            id,
            viewport,
            events,
            cleanup_guard,
        }
    }

    /// This client's id.
    #[must_use]
    pub fn id(&self) -> ClientId {
        self.id
    }

    /// The client's own outer-terminal size in cells.
    #[must_use]
    pub fn viewport(&self) -> Size {
        self.viewport
    }

    /// Record the outer terminal's new size. The caller also reports the
    /// resize to the server, which owns the reconciled tab sizes.
    pub fn set_viewport(&mut self, viewport: Size) {
        self.viewport = viewport;
    }

    /// Take every event the subscription has delivered since the last drain,
    /// in emission order. Empty when nothing arrived.
    pub fn drain_events(&mut self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    /// Borrow the outer-terminal cleanup guard.
    #[must_use]
    pub fn cleanup_guard(&self) -> &TerminalCleanupGuard {
        &self.cleanup_guard
    }
}

#[cfg(test)]
mod tests;
