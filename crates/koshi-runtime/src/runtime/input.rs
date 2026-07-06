//! Outer input routing: keystroke bytes from a client to its focused pane.
//!
//! A minimal passthrough — the raw bytes a client's terminal produced are
//! written straight to the focused pane's child stdin. Keybinding resolution
//! is a separate concern; this only carries bytes.

use koshi_core::ids::ClientId;

use crate::runtime::state::Runtime;

impl Runtime {
    /// Write outer-input bytes to `client_id`'s focused pane. Does nothing if
    /// the client is gone or has no focused pane in its active tab.
    pub fn handle_outer_input(&mut self, client_id: ClientId, bytes: &[u8]) {
        let pane_id = {
            let Some(session) = self.session_for_client(client_id) else {
                return;
            };
            let Some(client) = session.clients.get(client_id) else {
                return;
            };
            match client.focused_pane(client.active_tab()) {
                Some(pane_id) => pane_id,
                None => return,
            }
        };
        let _ = self.pty_backend().write(pane_id, bytes);
    }
}
