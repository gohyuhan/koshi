//! The session's half of keyboard input: writing a press the viewer did not
//! bind.
//!
//! **What a key means is decided by the viewer that received it.** The viewer
//! (`koshi-client`) holds the keymap, the input mode and any sequence being
//! typed. A binding the viewer resolves reaches the session as the command it
//! stands for, submitted like any other command. A press the viewer does not
//! bind reaches this module as a chord to write. Nothing here consults a
//! keymap. A raw chord that reaches the session
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
//! **A press reaches only a pane the client can see.** A focused pane the tab
//! draws no content for — suppressed for want of space, hidden behind a
//! fullscreen pane, collapsed to a stack header — takes nothing. The pane a
//! press may reach is the one `Server::find_typed_pane` names; when it names
//! none, the press is dropped.

use crate::runtime::snapshot::solve_tab_layout;
use crate::server::Server;
use koshi_core::ids::{ClientId, PaneId};
use koshi_core::key::KeyInput;
use koshi_input::keyboard::encode_key_input;
use koshi_layout::content::list_content_rects;

impl Server {
    /// React to input reaching `pane_id`'s child from `client_id`: drop the
    /// client's highlight in that pane, then return the client's view to live
    /// output.
    ///
    /// Three paths call it: a keystroke ([`Server::handle_key_input`]), pasted
    /// text ([`Server::handle_host_paste`]), and a `core:write-to-pane` write.
    /// A forwarded mouse report drops the highlight itself and does not call
    /// this.
    pub(crate) fn handle_input_reached_pane(&mut self, client_id: ClientId, pane_id: PaneId) {
        self.clear_session_recovery_notice_after_pane_input(pane_id);
        self.clear_selection_on_pane_input(client_id, pane_id);
        self.snap_view_to_bottom_on_input(client_id, pane_id);
    }

    /// Hide the recovery notice after a successful user write to a pane in its session.
    pub(crate) fn clear_session_recovery_notice_after_pane_input(&mut self, pane_id: PaneId) {
        if let Some(session) = self.get_session_for_pane_mut(pane_id) {
            if session.is_recovery_notice_visible {
                session.is_recovery_notice_visible = false;
                self.render_scheduler.invalidate();
            }
        }
    }

    /// Return this client's scrollback view of `pane_id` to the newest line.
    ///
    /// Moves the view only when the `scroll-on-input` setting is on and
    /// `pane_id`'s terminal engine is on the primary screen. A pane on the
    /// alternate screen, a pane with no terminal engine, and a view already at
    /// the newest line all move nothing.
    fn snap_view_to_bottom_on_input(&mut self, client_id: ClientId, pane_id: PaneId) {
        if self.client_config.scrollback.should_scroll_to_input
            && self
                .terminal_engine_by_pane_id
                .get(&pane_id)
                .is_some_and(|terminal_engine| {
                    terminal_engine
                        .get_terminal_state()
                        .is_primary_screen_active()
                })
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
    /// Nothing is written when `pasted_text` is empty, when the client's lock mode
    /// does not pass input to the pane
    /// (`LockMode::should_pass_unbound_input_to_pane`), or when
    /// `Server::find_typed_pane` names no pane. A write clears the client's
    /// highlight in that pane and returns the client's view to live output.
    pub fn handle_host_paste(&mut self, client_id: ClientId, pasted_text: &str) {
        if pasted_text.is_empty() {
            return;
        }
        let can_pass_input_to_pane = self
            .get_session_for_client(client_id)
            .and_then(|session| session.clients.get_client_by_id(client_id))
            .is_some_and(|attached_client| {
                attached_client
                    .get_lock_mode()
                    .should_pass_unbound_input_to_pane()
            });
        if !can_pass_input_to_pane {
            return;
        }
        let Some(pane_id) = self.find_typed_pane(client_id) else {
            return;
        };
        let is_bracketed_paste_enabled =
            self.terminal_engine_by_pane_id
                .get(&pane_id)
                .is_some_and(|terminal_engine| {
                    terminal_engine
                        .get_terminal_state()
                        .is_bracketed_paste_enabled()
                });
        let paste_output_bytes =
            crate::runtime::clipboard::build_paste_bytes(pasted_text, is_bracketed_paste_enabled);
        if self
            .get_pty_backend()
            .write_pane_input(pane_id, &paste_output_bytes)
            .is_ok()
        {
            self.handle_input_reached_pane(client_id, pane_id);
        }
    }

    /// The pane a keystroke from `client_id` types into: the pane it has focused
    /// in its active tab, when that pane can take a keystroke at all.
    ///
    /// Yields `None` for an unknown client, an active tab with no focused pane,
    /// a focused pane the session no longer holds, a missing tab, and a tab with
    /// no tab size.
    ///
    /// A focused pane this client draws no content for also yields `None` —
    /// suppressed for want of space, hidden behind a pane this client has
    /// zoomed, or collapsed to a stack header. Shrink the terminal until the
    /// focused pane is suppressed, type `l`, and the shell inside it stays
    /// untouched. The question is asked with [`list_content_rects`], the same
    /// function the renderer asks, in THIS client's layout mode: another
    /// client's zoom never silences this client's keys.
    ///
    /// The tab is solved against [`Session::get_tab_size`], the size every
    /// client viewing it shares: every viewer of the tab agrees on which panes
    /// are drawn, exactly as they agree on the frame.
    ///
    /// [`Session::get_tab_size`]: koshi_session::session::state::Session::get_tab_size
    pub(crate) fn find_typed_pane(&self, client_id: ClientId) -> Option<PaneId> {
        let session = self.get_session_for_client(client_id)?;
        let attached_client = session.clients.get_client_by_id(client_id)?;
        let tab_id = attached_client.get_active_tab_id();
        let pane_id = attached_client.get_focused_pane_id(tab_id)?;
        session.panes.get_pane_record_by_id(pane_id)?;

        let tab_record = session.tabs.get(&tab_id)?;
        let tab_size = session.get_tab_size(tab_id)?;
        list_content_rects(&solve_tab_layout(
            tab_record,
            attached_client.get_layout_mode(tab_id),
            tab_size,
            self.get_pane_sizing(),
        ))
        .into_iter()
        .any(|(candidate_pane_id, content_rect)| {
            candidate_pane_id == pane_id && content_rect.is_some()
        })
        .then_some(pane_id)
    }

    /// Write one key the viewer did not bind to the pane it is typing into,
    /// encoded for that pane's keyboard flags and cursor-key mode at this
    /// instant.
    ///
    /// Nothing is written when `Server::find_typed_pane` names no pane, and
    /// nothing is written when the pane asked not to receive this event, such
    /// as a key release into a pane that pushed no flag. A pane with no
    /// terminal engine encodes with no keyboard flags and application cursor
    /// keys off. A write clears the client's highlight in that pane and
    /// returns the client's view to live output.
    pub fn handle_key_input(&mut self, client_id: ClientId, key_input: &KeyInput) {
        let Some(pane_id) = self.find_typed_pane(client_id) else {
            return;
        };
        let (keyboard_flags, is_application_cursor_keys_enabled) = self
            .terminal_engine_by_pane_id
            .get(&pane_id)
            .map_or((0, false), |terminal_engine| {
                let terminal_state = terminal_engine.get_terminal_state();
                (
                    terminal_state.get_keyboard_flags(),
                    terminal_state.is_application_cursor_keys_enabled(),
                )
            });
        let key_bytes = encode_key_input(
            key_input,
            keyboard_flags,
            is_application_cursor_keys_enabled,
            self.config.terminal.extended_keys_mode,
        );
        if key_bytes.is_empty() {
            return;
        }
        if self
            .get_pty_backend()
            .write_pane_input(pane_id, &key_bytes)
            .is_ok()
        {
            self.handle_input_reached_pane(client_id, pane_id);
        }
    }
}

#[cfg(test)]
mod tests;
