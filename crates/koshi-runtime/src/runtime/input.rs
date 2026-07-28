//! The session's half of keyboard input: running a binding the viewer already
//! resolved, and writing a press the viewer did not bind.
//!
//! **What a key means is decided by the viewer that received it** — it holds
//! the keymap, the input mode and any sequence being typed (`koshi-client`).
//! It hands the session one of two things: a [`BoundAction`] to run, or a
//! chord to write. So nothing here consults a keymap, and a raw chord arriving
//! at the session belongs to no attached viewer and is dropped.
//!
//! Text the outer terminal pastes routes here too
//! ([`Server::handle_host_paste`]) — it is input for the same pane, delivered
//! as one block so none of it can fire a binding.
//!
//! **A chord becomes bytes here, not at the viewer.** Which bytes a pane
//! expects depends on the cursor-key mode that pane is in, and that mode lives
//! with the pane's terminal engine. Reading it at the instant of the write is
//! what stops the encoding going stale: a program that turns
//! application-cursor-keys on gets `ESC O A` for the very next `<Up>`, with no
//! frame in between for the answer to drift.
//!
//! **A press reaches only a pane the client can see, and only a terminal.** A
//! focused pane the tab draws no content for — suppressed for want of space,
//! hidden behind a fullscreen pane, collapsed to a stack header — takes
//! nothing, and neither does a plugin pane, which has no PTY to write to. The
//! pane a press may reach is the one `Server::typed_pane` names; when it names
//! none, the press is dropped.

use std::time::SystemTime;

use koshi_config::types::BoundAction;
use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::ids::{ClientId, CommandId, PaneId};
use koshi_core::key::KeyChord;
use koshi_core::lock::LockMode;
use koshi_core::resolve::{resolve_action, DispatchPlan};
use koshi_input::keyboard::encode;
use koshi_layout::content::content_rects;
use koshi_pane::pane::state::PaneKind;

use crate::runtime::render_schedule::InvalidationReason;
use crate::runtime::snapshot::solve_tab;
use crate::server::Server;

impl Server {
    /// Write one unconsumed press to the pane the client is typing into,
    fn write_press(&mut self, client_id: ClientId, chord: KeyChord) {
        let Some(pane_id) = self.typed_pane(client_id) else {
            return;
        };
        let app_cursor_keys = self
            .terminal_engines
            .get(&pane_id)
            .is_some_and(|engine| engine.state().app_cursor_keys());
        let bytes = encode(chord, app_cursor_keys);
        let _ = self.pty_backend().write(pane_id, &bytes);
        self.on_input_reached_pane(client_id, pane_id);
    }

    /// React to input reaching `pane_id`'s child from `client_id`: drop the
    /// client's highlight there — input replaces a selection, the way typing
    /// over one does — and follow the client's view back to live output. Every
    /// path that delivers input to a pane's child routes through here:
    /// keystrokes, pasted text, and `core:write-to-pane` writes.
    pub(crate) fn on_input_reached_pane(&mut self, client_id: ClientId, pane_id: PaneId) {
        self.clear_selection_on_pane_input(client_id, pane_id);
        self.snap_view_to_bottom_on_input(client_id, pane_id);
    }

    /// Return this client's scrollback view of `pane_id` to the newest line when
    /// the `scroll-on-input` setting is on. A no-op when the view already
    /// follows live output. The alternate screen keeps no scrollback of Koshi's,
    /// so its scroll position is left to the full-screen program that owns it —
    /// the snap only fires on the primary screen.
    fn snap_view_to_bottom_on_input(&mut self, client_id: ClientId, pane_id: PaneId) {
        if self.client_config.scrollback.scroll_on_input
            && self
                .terminal_engines
                .get(&pane_id)
                .is_some_and(|engine| engine.state().on_primary_screen())
        {
            self.scroll_to_bottom(client_id, pane_id);
        }
    }

    /// Write text the client's outer terminal pasted into the pane the client
    /// is typing into — the OS paste key, arriving as one block instead of a
    /// burst of keys, so no character of it can fire a keybinding (a pasted
    /// `Tab` lands in the shell instead of switching tabs).
    ///
    /// The pane reads it the way a terminal pastes: wrapped in bracketed-paste
    /// markers when the pane turned that mode on, raw bytes otherwise, line
    /// breaks as the byte the Enter key sends. Like a keystroke, it goes only
    /// to a visible terminal pane in a mode that passes input through, and —
    /// input reaching the pane's child — it clears the client's highlight
    /// there.
    pub fn handle_host_paste(&mut self, client_id: ClientId, text: &str) {
        if text.is_empty() {
            return;
        }
        let mode = self
            .session_for_client(client_id)
            .and_then(|session| session.clients.get(client_id))
            .map(koshi_session::client::Client::lock_mode);
        if !mode.is_some_and(transparent) {
            return;
        }
        let Some(pane_id) = self.typed_pane(client_id) else {
            return;
        };
        let bracketed = self
            .terminal_engines
            .get(&pane_id)
            .is_some_and(|engine| engine.state().bracketed_paste());
        let bytes = crate::runtime::clipboard::paste_bytes(text, bracketed);
        let _ = self.pty_backend().write(pane_id, &bytes);
        self.on_input_reached_pane(client_id, pane_id);
    }

    /// The pane a keystroke from `client_id` types into: the pane it has focused
    /// in its active tab, when that pane can take a keystroke at all.
    ///
    /// Two focused panes take none, and both yield `None`:
    ///
    /// - **A pane this client draws no content for** — suppressed for want of
    ///   space, hidden behind a pane this client has zoomed, or collapsed to a
    ///   stack header. A keystroke is aimed at what the client can see, so a
    ///   pane it cannot see receives nothing: shrink the terminal until the
    ///   focused pane is suppressed, type `l`, and the shell inside it stays
    ///   untouched. The question is asked with [`content_rects`], the same
    ///   function the renderer asks, so what a client can type into and what it
    ///   can see cannot drift apart. It is asked in THIS client's layout mode —
    ///   zoom is per-client, so another client's zoom never silences this
    ///   client's keys.
    /// - **A plugin pane**, which has no PTY behind it. The bytes a chord
    ///   encodes are a terminal's to read; a plugin surface reads its input
    ///   through the plugin host.
    ///
    /// The tab is solved against [`Session::tab_viewport`] — the size every
    /// client viewing it shares — so all its viewers agree on which panes are
    /// drawn, exactly as they agree on the frame.
    ///
    /// [`Session::tab_viewport`]: koshi_session::session::state::Session::tab_viewport
    pub(crate) fn typed_pane(&self, client_id: ClientId) -> Option<PaneId> {
        let session = self.session_for_client(client_id)?;
        let client = session.clients.get(client_id)?;
        let tab_id = client.active_tab();
        let pane_id = client.focused_pane(tab_id)?;

        if !matches!(session.panes.get(pane_id)?.kind(), PaneKind::Terminal) {
            return None;
        }

        let tab = session.tabs.get(&tab_id)?;
        let viewport = session.tab_viewport(tab_id)?;
        content_rects(&solve_tab(
            tab,
            client.layout_mode(tab_id),
            viewport,
            self.effective_pane_min(),
        ))
        .into_iter()
        .any(|(pane, content)| pane == pane_id && content.is_some())
        .then_some(pane_id)
    }

    /// Re-arm a continuous binding's prefix after it fires: the sequence minus
    /// its final chord goes back to pending, so the next chord alone fires the
    /// sibling binding (`<C-s> h h h` resizes three times). Only actions the
    /// registry marks `continuous` re-arm, and only multi-chord sequences have
    /// a prefix to hold.
    ///
    /// what an action does.
    pub fn handle_bound_action(&mut self, client_id: ClientId, bound: BoundAction) {
        self.fire_binding(client_id, bound);
    }

    /// Write one key the viewer did not bind to the pane it is typing into,
    /// encoded for that pane's cursor-key mode at this instant.
    pub fn handle_key_press(&mut self, client_id: ClientId, chord: KeyChord) {
        self.write_press(client_id, chord);
    }

    fn fire_binding(&mut self, client_id: ClientId, bound: BoundAction) {
        let Ok(plan) = resolve_action(&bound.action, &bound.args, &self.action_registry) else {
            return;
        };
        self.dispatch_plan(client_id, plan);
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
    }

    fn dispatch_plan(&mut self, client_id: ClientId, plan: DispatchPlan) {
        match plan {
            DispatchPlan::Command(command) => {
                let envelope = CommandEnvelope::new(
                    CommandId::new(),
                    CommandSource::key_binding(client_id),
                    SystemTime::now(),
                    command,
                );
                let _ = self.dispatch(envelope);
            }
            DispatchPlan::Sequence(plans) => {
                for plan in plans {
                    self.dispatch_plan(client_id, plan);
                }
            }
            DispatchPlan::PluginHostCall { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests;

/// Whether input that binds nothing reaches the pane. Normal and locked mode
/// pass what they do not bind; the modal layers own the keyboard while they
/// are held and discard it. A host paste is gated the same way — pasted text is
/// input for the pane, and a mode that keeps keys from the pane keeps pastes
/// from it too.
fn transparent(mode: LockMode) -> bool {
    matches!(mode, LockMode::Normal | LockMode::Locked)
}
