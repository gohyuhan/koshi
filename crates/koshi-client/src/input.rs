//! What a keypress means, decided by the viewer that received it.
//!
//! A viewer holds its own keymap, its own input mode, and its own open
//! sequence, so it can answer "is this key mine?" without asking the session.
//! That matters for two reasons: two viewers of one session read different
//! `keybinding.kdl` files and must each get their own answer, and over a
//! network a multi-chord binding would otherwise cost one round trip per
//! chord, with the disambiguation clock running on the far end of the link.
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
//! into an open sequence reaches the pane, so a mistyped continuation cannot
//! make the program underneath act on a key aimed at koshi. Three keys leave
//! the context — a continuation that completes a binding, `Esc`, and the
//! reserved unlock chord — and one thing that is not a key: a sequence that is
//! both a complete binding and a longer one's prefix closes on its ambiguity
//! deadline, firing the complete binding.

use std::time::{Duration, Instant};

use koshi_config::types::BoundAction;
use koshi_core::action::ActionRef;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey, PendingKeySequence};
use koshi_core::lock::LockMode;
use koshi_core::resolve::ActionArgs;

use crate::Client;

#[cfg(test)]
mod tests;

/// The chord that backs out of an open multi-chord sequence.
const ESCAPE: KeyChord = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc));

/// What the viewer decided one keypress means.
///
/// Only [`Fire`](KeyOutcome::Fire) and [`PassThrough`](KeyOutcome::PassThrough)
/// leave the viewer; the other two are settled where the key was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// A binding completed. The session resolves the name against its action
    /// table and dispatches it.
    Fire(BoundAction),
    /// The chord opened or continued a multi-chord sequence, or a held
    /// sequence swallowed a key that continues nothing. The hint bar changes;
    /// nothing else does.
    Pending,
    /// Nothing bound the chord and the mode passes what it does not bind. The
    /// session encodes it for the focused pane, reading that pane's cursor-key
    /// mode at the instant it writes so the bytes cannot be stale.
    PassThrough(KeyChord),
    /// Consumed with nothing to do: a modal mode that owns the keyboard, or an
    /// `Esc` that closed a sequence.
    Discard,
}

impl Client {
    /// The viewer's current input mode.
    #[must_use]
    pub fn lock_mode(&self) -> LockMode {
        self.lock_mode
    }

    /// Set the input mode, dropping any open sequence with it.
    ///
    /// Called both when this viewer's own `core:lock` fires and when the
    /// session reports a mode change aimed at this viewer (`koshi lock
    /// --client`). Held chords were typed at koshi, and a mode change is not a
    /// request to type them at the pane, so they are dropped rather than
    /// flushed.
    pub fn set_lock_mode(&mut self, mode: LockMode) {
        if self.lock_mode != mode {
            self.lock_mode = mode;
            self.pending = None;
        }
    }

    /// The chords of an open sequence, for the hint bar's breadcrumb.
    #[must_use]
    pub fn pending_sequence(&self) -> Option<&KeySequence> {
        self.pending.as_ref().map(|pending| &pending.sequence)
    }

    /// Drop any open sequence. A keymap change retires the bindings the held
    /// chords were reaching for, so they resolve to nothing and are dropped.
    pub fn clear_pending_sequence(&mut self) {
        self.pending = None;
    }

    /// Decide what `chord` means in this viewer's current mode.
    ///
    /// `<C-l>` while locked yields `Fire(core:unlock)` whatever the keymap
    /// says; `<C-p>` in the default keymap yields `Pending` because it opens
    /// the pane group; a plain `a` with nothing bound yields
    /// `PassThrough('a')`.
    pub fn resolve_key(&mut self, chord: KeyChord, now: Instant) -> KeyOutcome {
        let mode = self.lock_mode;
        let pending = self.pending.take();

        // The guaranteed escape from locked mode, resolved before the keymap
        // and before sequence buffering: whatever the viewer is in the middle
        // of, this chord unlocks it.
        if mode == LockMode::Locked && chord == self.keymap.unlock_chord() {
            return KeyOutcome::Fire(unlock());
        }

        let mut chords = pending
            .as_ref()
            .map(|pending| pending.sequence.chords().to_vec())
            .unwrap_or_default();
        chords.push(chord);
        let sequence = sequence(chords);

        let matched = self.keymap.match_sequence(mode, &sequence);
        match (matched.exact, matched.prefix) {
            (Some(bound), false) => {
                self.rearm_continuous(&bound, &sequence);
                KeyOutcome::Fire(bound)
            }
            (exact, true) => {
                // A prefix-only sequence waits for its next chord with no
                // deadline; only exact-plus-longer ambiguity arms one, and
                // reaching it fires the exact binding.
                let deadline = exact.is_some().then(|| now + self.keymap.chord_timeout());
                self.pending = Some(PendingKeySequence { sequence, deadline });
                KeyOutcome::Pending
            }
            (None, false) => match pending {
                // Escape leaves an open sequence: the held chords are dropped
                // and the Escape itself is consumed rather than typed.
                Some(_) if chord == ESCAPE => KeyOutcome::Pending,
                // A key that continues nothing is discarded and the sequence
                // stands unchanged, deadline included: the viewer is inside a
                // koshi context, so a key that context cannot use goes nowhere
                // rather than surprising the program underneath.
                Some(held) => {
                    self.pending = Some(held);
                    KeyOutcome::Pending
                }
                // No sequence is open, so the key is the user's own to type.
                None if transparent(mode) => KeyOutcome::PassThrough(chord),
                None => KeyOutcome::Discard,
            },
        }
    }

    /// How long until an open sequence's ambiguity deadline, so the event loop
    /// can wake for it. Prefix-only sequences carry no deadline and never wake
    /// it.
    #[must_use]
    pub fn next_key_wakeup(&self, now: Instant) -> Option<Duration> {
        self.pending
            .as_ref()
            .and_then(|pending| pending.deadline)
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Fire the open sequence's complete binding if its ambiguity deadline has
    /// passed.
    ///
    /// The deadline was armed because the sequence was itself a complete
    /// binding, so it normally still is. A keymap change can retire that
    /// binding while the sequence waits; the held chords then resolve to
    /// nothing and are dropped, never typed at the pane.
    pub fn expire_key_sequence(&mut self, now: Instant) -> Option<BoundAction> {
        let due = self
            .pending
            .as_ref()
            .and_then(|pending| pending.deadline)
            .is_some_and(|deadline| deadline <= now);
        if !due {
            return None;
        }
        let pending = self.pending.take()?;
        let bound = self
            .keymap
            .match_sequence(self.lock_mode, &pending.sequence)
            .exact?;
        self.rearm_continuous(&bound, &pending.sequence);
        Some(bound)
    }

    /// Re-open the prefix of a sequence whose action the registry marks
    /// `continuous`, so a repeated final chord repeats the action: `<C-p> r →`
    /// leaves `<C-p> r` open, and each further `→` resizes again.
    ///
    /// Only multi-chord sequences have a prefix to hold. The re-armed prefix
    /// captures the keyboard like any other open sequence, so a key that
    /// resizes nothing is discarded and the prefix stands until `Esc` leaves
    /// it.
    fn rearm_continuous(&mut self, bound: &BoundAction, sequence: &KeySequence) {
        let continuous = self
            .registry
            .lookup(&bound.action)
            .is_some_and(|metadata| metadata.continuous);
        let chords = sequence.chords();
        if !continuous || chords.len() < 2 {
            return;
        }
        self.pending = Some(PendingKeySequence {
            sequence: KeySequence::new(chords[0], chords[1..chords.len() - 1].to_vec()),
            deadline: None,
        });
    }
}

/// One sequence from the chords pressed into it, in press order.
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
/// pass what they do not bind; the modal layers own the keyboard while they
/// are held and discard it.
fn transparent(mode: LockMode) -> bool {
    matches!(mode, LockMode::Normal | LockMode::Locked)
}
