//! The session's half of keyboard input: running a binding the viewer already
//! resolved, and writing a press the viewer did not bind.
//!
//! **What a key means is decided by the viewer that received it.** The viewer
//! (`koshi-client`) holds the keymap, the input mode and any sequence being
//! typed. It hands the session one of two things: a [`BoundAction`] to run,
//! carrying the split direction the viewer's own settings hold, or a chord to
//! write. Nothing here consults a keymap. A raw chord that reaches the session
//! belongs to no attached viewer and is dropped before this module sees it.
//!
//! Text the outer terminal pastes routes here too
//! ([`Server::handle_host_paste`]): input for the same pane, delivered as one
//! block, and no character of it fires a binding.
//!
//! **A chord becomes bytes here, not at the viewer.** Which bytes a pane
//! expects depends on the cursor-key mode that pane's terminal engine is in,
//! read at the instant of the write: a program that turns
//! application-cursor-keys on gets `ESC O A` for the very next `<Up>`.
//!
//! **A press reaches only a pane the client can see, and only a terminal.** A
//! focused pane the tab draws no content for — suppressed for want of space,
//! hidden behind a fullscreen pane, collapsed to a stack header — takes
//! nothing, and neither does a plugin pane, which has no PTY. The pane a press
//! may reach is the one `Server::typed_pane` names; when it names none, the
//! press is dropped.

use std::time::SystemTime;

use crate::runtime::snapshot::solve_tab;
use crate::server::Server;
use koshi_config::types::BoundAction;
use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, CommandId, PaneId};
use koshi_core::key::KeyChord;
use koshi_core::resolve::{resolve_action, DispatchPlan};
use koshi_input::keyboard::encode;
use koshi_layout::content::content_rects;
use koshi_pane::pane::state::PaneKind;

impl Server {
    /// React to input reaching `pane_id`'s child from `client_id`: drop the
    /// client's highlight in that pane, then return the client's view to live
    /// output.
    ///
    /// Three paths call it: a keystroke ([`Server::handle_key_press`]), pasted
    /// text ([`Server::handle_host_paste`]), and a `core:write-to-pane` write.
    /// A forwarded mouse report drops the highlight itself and does not call
    /// this.
    pub(crate) fn on_input_reached_pane(&mut self, client_id: ClientId, pane_id: PaneId) {
        self.clear_selection_on_pane_input(client_id, pane_id);
        self.snap_view_to_bottom_on_input(client_id, pane_id);
    }

    /// Return this client's scrollback view of `pane_id` to the newest line.
    ///
    /// Moves the view only when the `scroll-on-input` setting is on and
    /// `pane_id`'s terminal engine is on the primary screen. A pane on the
    /// alternate screen, a pane with no terminal engine, and a view already at
    /// the newest line all move nothing.
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
    /// burst of keys. No character of it fires a keybinding: a pasted `Tab`
    /// lands in the shell instead of switching tabs.
    ///
    /// The pane reads it the way a terminal pastes: wrapped in bracketed-paste
    /// markers when the pane turned that mode on, raw bytes otherwise, line
    /// breaks as the byte the Enter key sends.
    ///
    /// Nothing is written when `text` is empty, when the client's lock mode
    /// does not pass input to the pane (`LockMode::passes_to_pane`), or when
    /// `Server::typed_pane` names no pane. A write clears the client's
    /// highlight in that pane and returns the client's view to live output.
    pub fn handle_host_paste(&mut self, client_id: ClientId, text: &str) {
        if text.is_empty() {
            return;
        }
        let passes_to_pane = self
            .session_for_client(client_id)
            .and_then(|session| session.clients.get(client_id))
            .is_some_and(|client| client.lock_mode().passes_to_pane());
        if !passes_to_pane {
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
    /// Yields `None` for an unknown client, an active tab with no focused pane,
    /// a focused pane the session no longer holds, a missing tab, and a tab with
    /// no viewport. Two kinds of focused pane also yield `None`:
    ///
    /// - **A pane this client draws no content for** — suppressed for want of
    ///   space, hidden behind a pane this client has zoomed, or collapsed to a
    ///   stack header. Shrink the terminal until the focused pane is
    ///   suppressed, type `l`, and the shell inside it stays untouched. The
    ///   question is asked with [`content_rects`], the same function the
    ///   renderer asks, in THIS client's layout mode: another client's zoom
    ///   never silences this client's keys.
    /// - **A pane that is not a [`PaneKind::Terminal`]** — a plugin pane, which
    ///   has no PTY behind it.
    ///
    /// The tab is solved against [`Session::tab_viewport`], the size every
    /// client viewing it shares: every viewer of the tab agrees on which panes
    /// are drawn, exactly as they agree on the frame.
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
            self.pane_sizing(),
        ))
        .into_iter()
        .any(|(pane, content)| pane == pane_id && content.is_some())
        .then_some(pane_id)
    }

    /// Run the action a viewer's keypress resolved to: look `bound.action` up
    /// in the action table, turn it into commands, dispatch them, and mark the
    /// status line stale. The viewer decided which binding fired; the session
    /// decides what that binding does.
    ///
    /// An action that does not resolve — unregistered, not yet implemented,
    /// given arguments it does not accept, or nested past
    /// [`MAX_SEQUENCE_DEPTH`] — dispatches nothing and marks nothing stale.
    ///
    /// `new_pane_direction` is the viewer's own `layout.new-pane-direction`
    /// setting, handed in with the action: a pane-opening action that names no
    /// direction splits toward it. The session keeps no split direction of its
    /// own, and two viewers of one session each open panes their own way.
    ///
    /// [`MAX_SEQUENCE_DEPTH`]: koshi_core::resolve::MAX_SEQUENCE_DEPTH
    pub fn handle_bound_action(
        &mut self,
        client_id: ClientId,
        bound: BoundAction,
        new_pane_direction: Direction,
    ) {
        let Ok(plan) = resolve_action(
            &bound.action,
            &bound.args,
            &self.action_registry,
            new_pane_direction,
        ) else {
            return;
        };
        self.dispatch_plan(client_id, plan);
        self.render_scheduler.invalidate();
    }

    /// Write one key the viewer did not bind to the pane it is typing into,
    /// encoded for that pane's cursor-key mode at this instant.
    ///
    /// Nothing is written when `Server::typed_pane` names no pane. A pane with
    /// no terminal engine encodes with application cursor keys off. A write
    /// clears the client's highlight in that pane and returns the client's view
    /// to live output.
    pub fn handle_key_press(&mut self, client_id: ClientId, chord: KeyChord) {
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

    /// Dispatch every command `plan` names, in order, attributed to
    /// `client_id`'s keybinding. A command the dispatcher rejects does not stop
    /// the ones after it. A [`DispatchPlan::PluginHostCall`] runs nothing.
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
