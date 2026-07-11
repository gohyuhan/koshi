//! Outer keyboard routing: multi-chord keybinding resolution first, transparent
//! fallthrough to the focused terminal pane second.

use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::ids::{ClientId, CommandId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};
use koshi_core::lock::LockMode;
use koshi_core::resolve::{resolve_action, DispatchPlan};
use koshi_session::client::PendingKeySequence;

use crate::runtime::render_schedule::InvalidationReason;
use crate::runtime::state::Runtime;

/// The chord that backs out of an open multi-chord sequence.
const ESCAPE: KeyChord = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc));

impl Runtime {
    /// Resolve one normalized key against the client's current mode, buffering
    /// prefixes and writing only unconsumed bytes to the focused pane.
    pub fn handle_key_input(
        &mut self,
        client_id: ClientId,
        chord: KeyChord,
        raw_bytes: Vec<u8>,
        now: Instant,
    ) {
        let Some((mode, pending)) = self.take_pending(client_id) else {
            return;
        };
        let mut chords = pending
            .as_ref()
            .map(|pending| pending.sequence.chords().to_vec())
            .unwrap_or_default();
        let mut bytes = pending
            .as_ref()
            .map(|pending| pending.raw_bytes.clone())
            .unwrap_or_default();
        chords.push(chord);
        bytes.push(raw_bytes.clone());

        if chords.len() > self.keymap_hints.max_chord_depth() {
            self.flush_pending(client_id, mode, bytes);
            return;
        }

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
                let pending = PendingKeySequence {
                    sequence,
                    raw_bytes: bytes,
                    deadline,
                };
                if let Some(session) = self.session_for_client_mut(client_id) {
                    if let Some(client) = session.clients.get_mut(client_id) {
                        client.update_pending_key_sequence(Some(pending));
                    }
                }
                self.render_scheduler
                    .invalidate(InvalidationReason::StatusChanged);
            }
            (None, false) if pending.is_some() => {
                let held = pending.expect("guarded by the match arm");
                // Escape backs out of an open sequence: the buffered chords
                // are discarded, and the Escape itself is consumed.
                if chord == ESCAPE {
                    self.render_scheduler
                        .invalidate(InvalidationReason::StatusChanged);
                    return;
                }
                // A held sequence that was itself a complete binding fires
                // on the mismatch; otherwise its bytes fall through. The
                // mismatching chord then restarts resolution on its own.
                if let Some(bound) = self.keymap_hints.match_sequence(mode, &held.sequence).exact {
                    self.fire_binding(client_id, bound);
                } else {
                    self.flush_pending(client_id, mode, held.raw_bytes);
                }
                self.handle_key_input(client_id, chord, raw_bytes, now);
            }
            (None, false) => self.fall_through(client_id, mode, &raw_bytes),
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
            let matched = self.keymap_hints.match_sequence(mode, &pending.sequence);
            if let Some(bound) = matched.exact {
                self.fire_binding(client_id, bound.clone());
                self.rearm_continuous(client_id, &bound, &pending.sequence);
            } else {
                self.flush_pending(client_id, mode, pending.raw_bytes);
            }
            self.render_scheduler
                .invalidate(InvalidationReason::StatusChanged);
        }
    }

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

    /// Re-arm a continuous binding's prefix after it fires: the sequence
    /// minus its final chord goes back to pending, so the next chord alone
    /// fires the sibling binding (`<C-s> h h h` resizes three times). Only
    /// actions the registry marks `continuous` re-arm, and only multi-chord
    /// sequences have a prefix to hold. The re-armed pending carries no
    /// fallback bytes — the original press was consumed by the fired action,
    /// so abandoning the held prefix later writes nothing to the pane.
    fn rearm_continuous(
        &mut self,
        client_id: ClientId,
        bound: &koshi_config::types::BoundAction,
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
        let pending = PendingKeySequence {
            sequence: prefix,
            raw_bytes: Vec::new(),
            deadline: None,
        };
        if let Some(session) = self.session_for_client_mut(client_id) {
            if let Some(client) = session.clients.get_mut(client_id) {
                client.update_pending_key_sequence(Some(pending));
            }
        }
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
    }

    fn take_pending(
        &mut self,
        client_id: ClientId,
    ) -> Option<(LockMode, Option<PendingKeySequence>)> {
        let session = self.session_for_client_mut(client_id)?;
        let client = session.clients.get_mut(client_id)?;
        Some((client.lock_mode(), client.take_pending_key_sequence()))
    }

    fn flush_pending(&mut self, client_id: ClientId, mode: LockMode, bytes: Vec<Vec<u8>>) {
        if transparent(mode) {
            for raw in bytes {
                self.handle_outer_input(client_id, &raw);
            }
        }
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
    }

    fn fall_through(&mut self, client_id: ClientId, mode: LockMode, raw_bytes: &[u8]) {
        if transparent(mode) {
            self.handle_outer_input(client_id, raw_bytes);
        }
    }

    fn fire_binding(&mut self, client_id: ClientId, bound: koshi_config::types::BoundAction) {
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

fn transparent(mode: LockMode) -> bool {
    matches!(mode, LockMode::Normal | LockMode::Locked)
}

#[cfg(test)]
mod tests;
