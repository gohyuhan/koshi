//! The `koshi.kdl` app-settings transaction: swapping a changed app config
//! into the running session.
//!
//! Config lives in separate files in the koshi config directory —
//! `koshi.kdl` (app settings), a `themes/<name>.kdl` color theme,
//! `keybinding.kdl` (key bindings) — and each file is read on its own.
//! `koshi.kdl` is the only one carrying session-owned settings, so it is the
//! only one the session stores; each viewer reads and validates the colors and
//! the bindings for itself. A file arrives here already deserialized into its
//! partial config layer; discovering, reading, and deserializing the files is
//! the config loader's job.
//!
//! App settings are typed values and always apply. The transaction yields one
//! [`Event::ConfigReloaded`] per live session.

use koshi_config::layer::{merge_client, merge_server, PartialKoshiConfig};
use koshi_config::types::{ClientConfig, ServerConfig};
use koshi_core::event::{ConfigReloaded, Event};
use koshi_core::ids::SessionId;

use crate::server::Server;

impl Server {
    /// Swap in a reloaded `koshi.kdl`: replace the app-settings layer and
    /// recompute both effective configs, so the next pane spawns with the new
    /// shell, size floor, and scrollback limits.
    ///
    /// The candidate's theme and keybinding sections are dropped; the colors
    /// come from the theme file and the bindings from `keybinding.kdl`.
    /// Parsing `koshi.kdl` cannot fill either section, so the drop bites only
    /// on a hand-built candidate. The theme `koshi.kdl` *names* is resolved by
    /// the config loader, which reads that file and hands it to the viewer.
    ///
    /// Returns one [`Event::ConfigReloaded`] per live session.
    pub fn reload_app_config(&mut self, mut candidate: PartialKoshiConfig) -> Vec<Event> {
        candidate.theme = None;
        candidate.keybindings = None;
        self.app_layer = candidate;
        // `koshi.kdl` carries sections both sides own, so both are recomputed.
        self.config = fold_server(&self.app_layer);
        self.client_config = fold_client(&self.app_layer);
        self.config_reloaded_events()
    }

    /// Apply the `koshi.kdl` settings read at startup, before any session
    /// exists.
    ///
    /// `app` is `None` when the file is absent or failed to load; the built-in
    /// defaults then stand. No session exists yet, so the events
    /// [`reload_app_config`](Self::reload_app_config) returns are dropped.
    pub fn load_startup_config(&mut self, app: Option<PartialKoshiConfig>) {
        if let Some(app) = app {
            let _ = self.reload_app_config(app);
        }
    }

    /// One [`Event::ConfigReloaded`] per live session, in session-id order.
    fn config_reloaded_events(&self) -> Vec<Event> {
        let mut session_ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        session_ids.sort_unstable();
        session_ids
            .into_iter()
            .map(|session_id| Event::ConfigReloaded(ConfigReloaded { session_id }))
            .collect()
    }
}

/// Fold the stored `koshi.kdl` layer onto the built-in defaults, keeping the
/// sections the session owns.
pub(crate) fn fold_server(app_layer: &PartialKoshiConfig) -> ServerConfig {
    merge_server(ServerConfig::default(), vec![app_layer.clone()])
}

/// Fold the stored `koshi.kdl` layer onto the built-in defaults, keeping the
/// sections one viewer owns. This is the copy the session itself reads; each
/// viewer folds its own from its own files.
pub(crate) fn fold_client(app_layer: &PartialKoshiConfig) -> ClientConfig {
    merge_client(ClientConfig::default(), vec![app_layer.clone()])
}

#[cfg(test)]
mod tests;
