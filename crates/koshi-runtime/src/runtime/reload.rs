//! The `koshi.kdl` app-settings transaction: swapping a changed app config
//! into the running session.
//!
//! Config lives in separate files in the koshi config directory —
//! `koshi.kdl` (app settings), a `themes/<name>.kdl` color theme,
//! `keybinding.kdl` (key bindings) — and each file is read on its own.
//! `koshi.kdl` is the only one carrying session-owned settings, and the only
//! one the session stores. Each viewer reads and validates the colors and the
//! bindings for itself. A file arrives here already deserialized into its
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
    /// Swap in a reloaded `koshi.kdl`: replace the app-settings layer with
    /// `reloaded_app_config` and recompute both effective configs from it, the session's
    /// own and the viewer copy. `koshi.kdl` carries sections both sides own.
    /// The next pane spawns with the new shell, size floor, and scrollback
    /// limits.
    ///
    /// `reloaded_app_config.theme` and `reloaded_app_config.keybindings` are set to `None` before
    /// the swap: the colors come from the theme file and the bindings from
    /// `keybinding.kdl`. Parsing `koshi.kdl` fills neither section; only a
    /// hand-built candidate carries one. The config loader resolves the theme
    /// `koshi.kdl` names and hands that file to the viewer.
    ///
    /// Returns one [`Event::ConfigReloaded`] per live session, in session-id
    /// order.
    pub fn reload_app_config(&mut self, mut reloaded_app_config: PartialKoshiConfig) -> Vec<Event> {
        reloaded_app_config.theme = None;
        reloaded_app_config.keybindings = None;
        self.app_layer = reloaded_app_config;
        self.config = merge_app_layer_into_server_config(&self.app_layer);
        self.client_config = merge_app_layer_into_client_config(&self.app_layer);
        self.config_reloaded_events()
    }

    /// Apply the `koshi.kdl` settings read at startup, before any session
    /// exists.
    ///
    /// `app` is `None` when the file is absent or failed to load; the built-in
    /// defaults then stand. No session exists yet, so the events
    /// [`reload_app_config`](Self::reload_app_config) returns are dropped.
    pub fn load_startup_config(&mut self, startup_app_config: Option<PartialKoshiConfig>) {
        if let Some(startup_app_config) = startup_app_config {
            let _ = self.reload_app_config(startup_app_config);
        }
    }

    /// One [`Event::ConfigReloaded`] per live session, in session-id order.
    fn config_reloaded_events(&self) -> Vec<Event> {
        let mut session_ids: Vec<SessionId> = self.session_by_id.keys().copied().collect();
        session_ids.sort_unstable();
        session_ids
            .into_iter()
            .map(|session_id| Event::ConfigReloaded(ConfigReloaded { session_id }))
            .collect()
    }
}

/// Merge the stored `koshi.kdl` layer onto the built-in defaults, keeping the
/// sections the session owns.
pub(crate) fn merge_app_layer_into_server_config(app_layer: &PartialKoshiConfig) -> ServerConfig {
    merge_server(ServerConfig::default(), vec![app_layer.clone()])
}

/// Merge the stored `koshi.kdl` layer onto the built-in defaults, keeping the
/// sections one viewer owns. This is the copy the session itself reads; each
/// viewer folds its own from its own files.
pub(crate) fn merge_app_layer_into_client_config(app_layer: &PartialKoshiConfig) -> ClientConfig {
    merge_client(ClientConfig::default(), vec![app_layer.clone()])
}

#[cfg(test)]
mod tests;
