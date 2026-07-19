//! Outer keyboard routing: keybinding resolution first, transparent
//! fallthrough to the focused terminal pane second. Text the outer terminal
//! pastes routes here too ([`Runtime::handle_host_paste`]) — it is input for
//! the same pane, delivered as one block so none of it can fire a binding.
//!
//! A chord that no binding consumes becomes bytes here rather than back at the
//! decoder: the bytes a pane expects depend on which pane receives them and
//! what mode it is in, and the decoder, sitting at the host boundary, knows
//! neither.
//!
//! **An open sequence captures the keyboard.** Once a chord opens a multi-chord
//! binding, the client is inside that sequence's context, and every key belongs
//! to Koshi until the sequence resolves: a key that continues it fires the
//! binding, and a key that continues nothing is discarded while the sequence
//! stands. Nothing typed into an open sequence reaches the pane, so a mistyped
//! continuation cannot make the program underneath act on a key aimed at Koshi.
//! Three keys leave the context: a continuation that completes a binding, `Esc`,
//! and the reserved unlock chord. One thing that is not a key leaves it too — a
//! sequence that is both a complete binding and a longer one's prefix closes on
//! its ambiguity deadline, firing the complete binding.
//!
//! Because a buffered chord never reaches a pane, only an unconsumed press does
//! — and that press is written at the instant it is made, so the pane and its
//! cursor-key mode are both read then and cannot drift out from under it.
//!
//! **A press reaches only a pane the client can see, and only a terminal.** A
//! focused pane the tab draws no content for — suppressed for want of space,
//! hidden behind a fullscreen pane, collapsed to a stack header — takes
//! nothing, and neither does a plugin pane, which has no PTY to write to. The
//! pane a press may reach is the one `Runtime::typed_pane` names; when it names
//! none, the press is dropped.

use std::time::{Duration, Instant, SystemTime};

use koshi_config::types::BoundAction;
use koshi_core::action::ActionRef;
use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::ids::{ClientId, CommandId, PaneId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};
use koshi_core::lock::LockMode;
use koshi_core::resolve::{resolve_action, ActionArgs, DispatchPlan};
use koshi_input::keyboard::encode;
use koshi_layout::content::content_rects;
use koshi_pane::pane::state::PaneKind;
use koshi_session::client::PendingKeySequence;

use crate::runtime::render_schedule::InvalidationReason;
use crate::runtime::snapshot::solve_tab;
use crate::runtime::state::Runtime;

/// The chord that backs out of an open multi-chord sequence.
const ESCAPE: KeyChord = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc));

impl Runtime {
    /// Resolve one normalized key against the client's current mode: complete a
    /// binding, hold an open sequence, or write the press to the focused pane.
    pub fn handle_key_input(&mut self, client_id: ClientId, chord: KeyChord, now: Instant) {
        let Some((mode, pending)) = self.take_pending(client_id) else {
            return;
        };

        // The guaranteed escape from locked mode, resolved before the keymap
        // and before sequence buffering: whatever the client is in the middle
        // of, this chord unlocks it. Any held chords go with it — they were
        // typed at Koshi, and a client asking for Koshi back is not asking to
        // type them at the pane.
        if mode == LockMode::Locked && chord == self.keymap_hints.unlock_chord() {
            self.fire_binding(client_id, unlock());
            return;
        }

        let mut chords = pending
            .as_ref()
            .map(|pending| pending.sequence.chords().to_vec())
            .unwrap_or_default();
        chords.push(chord);

        let sequence = sequence(chords);
        let matched = self.keymap_hints.match_sequence(mode, &sequence);
        match (matched.exact, matched.prefix) {
            (Some(bound), false) => {
                self.fire_binding(client_id, bound.clone());
                self.rearm_continuous(client_id, &bound, &sequence);
            }
            (exact, true) => {
                // A prefix-only sequence waits for its next chord with no
                // deadline; only exact-plus-longer ambiguity arms one, and
                // reaching it fires the exact binding.
                let deadline = exact
                    .is_some()
                    .then(|| now + self.keymap_hints.chord_timeout());
                self.set_pending(client_id, PendingKeySequence { sequence, deadline });
            }
            (None, false) => match pending {
                // Escape leaves an open sequence: the held chords are dropped
                // and the Escape itself is consumed rather than typed at the
                // pane.
                Some(_) if chord == ESCAPE => {
                    self.render_scheduler
                        .invalidate(InvalidationReason::StatusChanged);
                }
                // A key that continues nothing is discarded, and the sequence
                // stands unchanged: the client is inside a Koshi context, so a
                // key that context cannot use goes nowhere rather than
                // surprising the program underneath. The sequence goes back
                // exactly as it was, deadline included, and nothing on screen
                // changed — so this restore does not repaint.
                Some(held) => self.hold_pending(client_id, held),
                // No sequence is open, so the key is the user's own to type.
                None => self.fall_through(client_id, mode, chord),
            },
        }
    }

    /// Earliest pending disambiguation deadline relative to `now`. Prefix-only
    /// sequences carry no deadline and never wake the loop.
    #[must_use]
    pub fn next_key_wakeup(&self, now: Instant) -> Option<Duration> {
        self.sessions
            .values()
            .flat_map(|session| session.clients.list_attached())
            .filter_map(|client| client.pending_key_sequence())
            .filter_map(|pending| pending.deadline)
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
    }

    /// Fire the complete binding of every pending sequence whose ambiguity
    /// deadline elapsed. Only a sequence that is both a complete binding and
    /// a longer binding's prefix carries a deadline; a prefix-only sequence
    /// waits for its next chord indefinitely.
    pub fn expire_key_sequences(&mut self, now: Instant) {
        let expired: Vec<ClientId> = self
            .sessions
            .values()
            .flat_map(|session| session.clients.list_attached())
            .filter(|client| {
                client
                    .pending_key_sequence()
                    .and_then(|pending| pending.deadline)
                    .is_some_and(|deadline| deadline <= now)
            })
            .map(|client| client.id())
            .collect();

        for client_id in expired {
            let Some((mode, Some(pending))) = self.take_pending(client_id) else {
                continue;
            };
            // The deadline was armed because the sequence was itself a complete
            // binding, so it normally still is. A keybinding reload can retire
            // that binding while the sequence waits; the held chords then
            // resolve to nothing and are dropped, never typed at the pane.
            if let Some(bound) = self
                .keymap_hints
                .match_sequence(mode, &pending.sequence)
                .exact
            {
                self.fire_binding(client_id, bound.clone());
                self.rearm_continuous(client_id, &bound, &pending.sequence);
            }
            self.render_scheduler
                .invalidate(InvalidationReason::StatusChanged);
        }
    }

    /// Write one unconsumed press to the pane the client is typing into,
    /// encoded for the cursor-key mode that pane is in at this instant: a
    /// program in application-cursor-keys mode reads `<Up>` as `ESC O A`, one
    /// outside it reads `ESC [ A`. A pane with no terminal engine has turned
    /// nothing on, which reads the same as the mode being off.
    ///
    /// An opaque mode writes nothing: the mode owns the keyboard while it is
    /// held, and a key it does not bind is not the pane's to see. Nothing is
    /// written either when [`Self::typed_pane`] finds no pane to type at.
    ///
    /// A press that is written also drops this client's highlight in that pane:
    /// input reaching the pane's child leaves visual mode, the way typing
    /// replaces a selection in an editor.
    fn fall_through(&mut self, client_id: ClientId, mode: LockMode, chord: KeyChord) {
        if !transparent(mode) {
            return;
        }
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
        if self.config.scrollback.scroll_on_input
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
    /// The re-armed prefix is an open sequence like any other, and captures the
    /// keyboard like any other: a key that resizes nothing is discarded and the
    /// prefix stands, so repeated presses stay in the resize context until `Esc`
    /// leaves it.
    fn rearm_continuous(
        &mut self,
        client_id: ClientId,
        bound: &BoundAction,
        sequence: &KeySequence,
    ) {
        let continuous = self
            .action_registry
            .lookup(&bound.action)
            .is_some_and(|metadata| metadata.continuous);
        let chords = sequence.chords();
        if !continuous || chords.len() < 2 {
            return;
        }
        let prefix = KeySequence::new(chords[0], chords[1..chords.len() - 1].to_vec());
        self.set_pending(
            client_id,
            PendingKeySequence {
                sequence: prefix,
                deadline: None,
            },
        );
    }

    fn take_pending(
        &mut self,
        client_id: ClientId,
    ) -> Option<(LockMode, Option<PendingKeySequence>)> {
        let session = self.session_for_client_mut(client_id)?;
        let client = session.clients.get_mut(client_id)?;
        Some((client.lock_mode(), client.take_pending_key_sequence()))
    }

    /// Hold `pending` as the client's open sequence and repaint the hint bar,
    /// which draws the sequence's continuations while it stands.
    fn set_pending(&mut self, client_id: ClientId, pending: PendingKeySequence) {
        self.hold_pending(client_id, pending);
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
    }

    /// Hold `pending` without repainting: the caller is putting back a sequence
    /// the client already had, so the hint bar already draws it.
    fn hold_pending(&mut self, client_id: ClientId, pending: PendingKeySequence) {
        if let Some(session) = self.session_for_client_mut(client_id) {
            if let Some(client) = session.clients.get_mut(client_id) {
                client.update_pending_key_sequence(Some(pending));
            }
        }
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

fn sequence(chords: Vec<KeyChord>) -> KeySequence {
    let mut chords = chords.into_iter();
    let first = chords
        .next()
        .expect("key input always contributes one chord");
    KeySequence::new(first, chords.collect())
}

/// The binding the unlock chord fires. Built rather than read from the keymap:
/// the escape from locked mode is the one binding that must hold whatever any
/// layer above it says, so it does not depend on a lookup that a layer could
/// answer differently.
fn unlock() -> BoundAction {
    BoundAction {
        action: ActionRef::core("unlock")
            .expect("the reserved unlock action name satisfies the action-name grammar"),
        args: ActionArgs::None,
    }
}

/// Whether a key that binds nothing reaches the pane. Normal and locked mode
/// pass what they do not bind; the modal layers own the keyboard while they are
/// held and discard it. A host paste is gated the same way — pasted text is
/// input for the pane, and a mode that keeps keys from the pane keeps pastes
/// from it too.
fn transparent(mode: LockMode) -> bool {
    matches!(mode, LockMode::Normal | LockMode::Locked)
}

#[cfg(test)]
mod tests;
