//! What a keypress means, decided by the viewer that received it.
//!
//! A viewer holds its own keymap, its own input mode, and its own open
//! sequence. It answers "is this key mine?" without asking the session. Two
//! viewers of one session read different `keybinding.kdl` files, and each one
//! gets its own answer.
//!
//! What the viewer does **not** decide is what a bound action *does*. It
//! resolves a chord to a [`BoundAction`] — a name plus its arguments — and
//! hands that to the session, which owns the action table and the state the
//! action mutates.
//!
//! **An open sequence captures the keyboard.** Once a chord opens a
//! multi-chord binding, every key belongs to koshi until the sequence
//! resolves: a key that continues it fires the binding, and a key that
//! continues nothing is discarded while the sequence stands. Nothing typed
//! into an open sequence reaches the pane. Three keys leave the context: a
//! continuation that completes a binding, `Esc`, and the reserved unlock
//! chord. One thing that is not a key leaves it too — a sequence that is both
//! a complete binding and a longer one's prefix closes on its ambiguity
//! deadline, firing the complete binding.

use std::time::{Duration, Instant};

use koshi_config::types::BoundAction;
use koshi_core::action::ActionReference;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey, PendingKeySequence};
use koshi_core::lock::LockMode;
use koshi_core::resolve::ActionArgs;

use crate::Client;

#[cfg(test)]
mod tests;

/// The chord that backs out of an open multi-chord sequence.
const ESCAPE_KEY_CHORD: KeyChord = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Esc));

/// What the viewer decided one keypress means.
///
/// Only [`Fire`](KeyOutcome::Fire) and [`PassThrough`](KeyOutcome::PassThrough)
/// leave the viewer; the other two are settled where the key was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// A binding completed. The session resolves the name against its action
    /// table and dispatches it.
    Fire(BoundAction),
    /// The chord opened or continued a multi-chord sequence, a held sequence
    /// swallowed a key that continues nothing, or an `Esc` closed one. The hint
    /// bar changes; nothing else does.
    Pending,
    /// Nothing bound the chord and the mode passes what it does not bind. The
    /// session encodes it for the focused pane, reading that pane's cursor-key
    /// mode at the instant it writes so the bytes cannot be stale.
    PassThrough(KeyChord),
    /// Consumed with nothing to do: no sequence is open, the chord binds
    /// nothing, and the mode is a modal one that owns the keyboard rather than
    /// passing what it does not bind.
    Discard,
}

impl Client {
    /// The viewer's current input mode.
    #[must_use]
    pub fn get_lock_mode(&self) -> LockMode {
        self.lock_mode
    }

    /// Set the input mode, dropping any open sequence with it.
    ///
    /// Called both when this viewer's own `core:lock` fires and when the
    /// session reports a mode change aimed at this viewer (`koshi lock
    /// --client`). Held chords were typed at koshi, so a mode change drops
    /// them and no pane ever sees them.
    pub fn set_lock_mode(&mut self, lock_mode: LockMode) {
        if self.lock_mode != lock_mode {
            self.lock_mode = lock_mode;
            self.pending_key_sequence = None;
        }
    }

    /// The chords of an open sequence, for the hint bar's breadcrumb.
    #[must_use]
    pub fn get_pending_key_sequence(&self) -> Option<&KeySequence> {
        self.pending_key_sequence
            .as_ref()
            .map(|pending_key_sequence| &pending_key_sequence.sequence)
    }

    /// Decide what `chord` means in this viewer's current mode.
    ///
    /// `<C-l>` while locked yields `Fire(core:unlock)` whatever the keymap
    /// says; `<C-p>` in the default keymap yields `Pending` because it opens
    /// the pane group; a plain `a` with nothing bound yields
    /// `PassThrough('a')`.
    pub fn resolve_key(&mut self, chord: KeyChord, current_time: Instant) -> KeyOutcome {
        let lock_mode = self.lock_mode;
        let open_key_sequence = self.pending_key_sequence.take();

        // The guaranteed escape from locked mode, resolved before the keymap
        // and before sequence buffering: whatever the viewer is in the middle
        // of, this chord unlocks it.
        if lock_mode == LockMode::Locked && chord == self.keymap_catalog.get_unlock_chord() {
            return KeyOutcome::Fire(build_unlock_bound_action());
        }

        // The open sequence's chords with this one after them, or this one on
        // its own. One keypress allocates the chord list once and the sequence
        // once.
        let key_sequence = match open_key_sequence.as_ref() {
            Some(open_key_sequence) => {
                let held_chords = open_key_sequence.sequence.list_chords();
                let mut remaining_chords = Vec::with_capacity(held_chords.len());
                remaining_chords.extend_from_slice(&held_chords[1..]);
                remaining_chords.push(chord);
                KeySequence::from_first_and_rest(held_chords[0], remaining_chords)
            }
            None => KeySequence::from(chord),
        };

        let sequence_match = self.keymap_catalog.match_sequence(lock_mode, &key_sequence);
        match (
            sequence_match.exact_bound_action,
            sequence_match.has_longer_key_sequence,
        ) {
            (Some(bound_action), false) => {
                self.rearm_continuous(&bound_action, &key_sequence);
                KeyOutcome::Fire(bound_action)
            }
            (exact_bound_action, true) => {
                // A prefix-only sequence waits for its next chord with no
                // deadline; only exact-plus-longer ambiguity arms one, and
                // reaching it fires the exact binding.
                let deadline = exact_bound_action
                    .is_some()
                    .then(|| current_time + self.keymap_catalog.get_chord_timeout());
                self.pending_key_sequence = Some(PendingKeySequence {
                    sequence: key_sequence,
                    deadline,
                });
                KeyOutcome::Pending
            }
            (None, false) => match open_key_sequence {
                // Escape leaves an open sequence: the held chords are dropped
                // and the Escape itself is consumed rather than typed.
                Some(_) if chord == ESCAPE_KEY_CHORD => KeyOutcome::Pending,
                // A key that continues nothing is discarded and the sequence
                // stands unchanged, deadline included: the viewer is inside a
                // koshi context, so a key that context cannot use goes nowhere
                // rather than surprising the program underneath.
                Some(held_key_sequence) => {
                    self.pending_key_sequence = Some(held_key_sequence);
                    KeyOutcome::Pending
                }
                // No sequence is open, so the key is the user's own to type.
                None if lock_mode.should_pass_unbound_input_to_pane() => {
                    KeyOutcome::PassThrough(chord)
                }
                None => KeyOutcome::Discard,
            },
        }
    }

    /// How long until an open sequence's ambiguity deadline, so the event loop
    /// can wake for it. Prefix-only sequences carry no deadline and never wake
    /// it.
    #[must_use]
    pub fn next_key_wakeup(&self, current_time: Instant) -> Option<Duration> {
        self.pending_key_sequence
            .as_ref()
            .and_then(|pending_key_sequence| pending_key_sequence.deadline)
            .map(|deadline| deadline.saturating_duration_since(current_time))
    }

    /// Fire the open sequence's complete binding if its ambiguity deadline has
    /// passed.
    ///
    /// The deadline was armed because the sequence was itself a complete
    /// binding, so it normally still is. A keymap change can retire that
    /// binding while the sequence waits; the held chords then resolve to
    /// nothing and are dropped, never typed at the pane.
    pub fn expire_key_sequence(&mut self, current_time: Instant) -> Option<BoundAction> {
        let is_deadline_due = self
            .pending_key_sequence
            .as_ref()
            .and_then(|pending_key_sequence| pending_key_sequence.deadline)
            .is_some_and(|deadline| deadline <= current_time);
        if !is_deadline_due {
            return None;
        }
        let pending_key_sequence = self.pending_key_sequence.take()?;
        let bound_action = self
            .keymap_catalog
            .match_sequence(self.lock_mode, &pending_key_sequence.sequence)
            .exact_bound_action?;
        self.rearm_continuous(&bound_action, &pending_key_sequence.sequence);
        Some(bound_action)
    }

    /// Re-open the prefix of a sequence whose action the registry marks
    /// `continuous`, so a repeated final chord repeats the action: `<C-p> r →`
    /// leaves `<C-p> r` open, and each further `→` resizes again.
    ///
    /// Only multi-chord sequences have a prefix to hold. The re-armed prefix
    /// captures the keyboard like any other open sequence, so a key that
    /// resizes nothing is discarded and the prefix stands until `Esc` leaves
    /// it.
    fn rearm_continuous(&mut self, bound_action: &BoundAction, key_sequence: &KeySequence) {
        let is_continuous = self
            .registry
            .find_action_metadata(&bound_action.action_reference)
            .is_some_and(|action_metadata| action_metadata.is_continuous);
        let sequence_chords = key_sequence.list_chords();
        if !is_continuous || sequence_chords.len() < 2 {
            return;
        }
        self.pending_key_sequence = Some(PendingKeySequence {
            sequence: KeySequence::from_first_and_rest(
                sequence_chords[0],
                sequence_chords[1..sequence_chords.len() - 1].to_vec(),
            ),
            deadline: None,
        });
    }
}

/// The binding the unlock chord fires, built here and never looked up in the
/// keymap. The escape from locked mode holds whatever any config layer says.
fn build_unlock_bound_action() -> BoundAction {
    BoundAction {
        action_reference: ActionReference::from_core_action_name("unlock")
            .expect("the reserved unlock action name satisfies the action-name grammar"),
        action_arguments: ActionArgs::None,
    }
}
