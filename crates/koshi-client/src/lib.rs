//! The viewer half of koshi: one attached terminal's own side of a session.
//!
//! A session is authoritative over tabs, panes, and the processes inside them.
//! A viewer owns what belongs to the terminal in front of the user — its size,
//! the settings it reads from its own config, and the colors it paints koshi's
//! own chrome with. The two talk only through the session's command door and
//! its event feed.
//!
//! Colors live here rather than with the session because two viewers of one
//! session each read their own `koshi.kdl`: the frame a session hands out says
//! *which pane is focused*, and each viewer looks up what "focused" should
//! look like in its own theme. So one session can be painted two ways at once.

pub mod theme;

use std::sync::mpsc::Receiver;

use koshi_config::types::ClientConfig;
use koshi_core::{event::Event, geometry::Size, ids::ClientId};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::theme::Theme;

#[cfg(test)]
mod tests;

/// One attached terminal's view side: its id, its own terminal size, its
/// event feed from the session, the settings it read from its own config, the
/// chrome colors resolved from them, and the outer-terminal restore guard.
///
/// The binary's event loop drives it; it can never mutate session or pane
/// data.
pub struct Client {
    /// This client's id, the one its input events and commands carry.
    id: ClientId,
    /// The client's own outer-terminal size in cells. Updated from resize
    /// events and reported to the session, which reconciles tab sizes from
    /// every viewer's report; this copy is the client's alone.
    viewport: Size,
    /// Receiving end of this client's event subscription, fed by the session's
    /// bounded fan-out.
    events: Receiver<Event>,
    /// The settings this viewer owns, folded from its own config files.
    config: ClientConfig,
    /// The chrome colors [`config`](Self::config)'s theme resolves to, kept
    /// resolved so a frame costs a borrow rather than a palette conversion.
    theme: Theme,
    /// Restores the outer terminal when the client ends or the process
    /// panics.
    cleanup_guard: TerminalCleanupGuard,
}

impl Client {
    /// Build a client from its id, its terminal's current size, the receiver
    /// the session handed out for it, the settings it read from its own
    /// config, and the outer-terminal cleanup guard.
    ///
    /// The chrome colors are resolved from `config` once here; a later config
    /// change goes through [`set_config`](Self::set_config).
    #[must_use]
    pub fn new(
        id: ClientId,
        viewport: Size,
        events: Receiver<Event>,
        config: ClientConfig,
        cleanup_guard: TerminalCleanupGuard,
    ) -> Self {
        let theme = theme::resolve(&config.theme);
        Client {
            id,
            viewport,
            events,
            config,
            theme,
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
    /// resize to the session, which owns the reconciled tab sizes.
    pub fn set_viewport(&mut self, viewport: Size) {
        self.viewport = viewport;
    }

    /// The settings this viewer owns.
    #[must_use]
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// The chrome colors every koshi-owned surface in this client's frames is
    /// painted with.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Swap in reloaded settings and re-resolve the chrome colors from them.
    ///
    /// A theme file the user edited reaches a frame this way: the new palette
    /// arrives in `config`, and the next frame this client paints uses it.
    pub fn set_config(&mut self, config: ClientConfig) {
        self.theme = theme::resolve(&config.theme);
        self.config = config;
    }

    /// Drop every event the subscription has delivered since the last call,
    /// returning how many were dropped. Keeps the bounded queue from filling
    /// while nothing consumes the feed; drops the events one by one without
    /// collecting them.
    pub fn discard_events(&mut self) -> usize {
        self.events.try_iter().count()
    }

    /// Borrow the outer-terminal cleanup guard.
    #[must_use]
    pub fn cleanup_guard(&self) -> &TerminalCleanupGuard {
        &self.cleanup_guard
    }
}
