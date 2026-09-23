//! Keybinding conflict detection over ordered keymap layers.
//!
//! Bindings arrive in layers, lowest precedence first: the built-in
//! defaults, then the user's own surfaces (user file, session, layout).
//! [`detect_conflicts`] inspects the layers before the keymap-merge pass
//! folds them into the runtime lookup map, and reports every finding as a
//! typed [`ConflictDiagnostic`]. The report's
//! [`get_verdict`](ConflictReport::get_verdict) tells the caller what to do with the
//! user keymap as a whole:
//!
//! - **Warnings** (ambiguous prefix, orphan action or mode, a
//!   not-yet-implemented action, arguments the action cannot take, typeable
//!   keys, a binding shadowed by the reserved unlock, a sequence past the
//!   chord-depth cap) inform; the keymap applies.
//! - **A key collision** — the same key sequence bound to different actions
//!   by two user-authored layers in one mode — reverts the whole user keymap
//!   to the built-in defaults ([`KeymapVerdict::RevertToDefaults`]).
//!   Non-keybinding config is unaffected. A user binding whose key is held
//!   only by the defaults layer is a *steal*, not a collision: the user's
//!   binding takes the key and the displaced default action becomes unbound.
//! - **A fatal finding** — the locked-mode unlock escape is shadowed, missing,
//!   or typeable, or the `move-pane` mode has no live cancellation binding —
//!   refuses the keymap outright ([`KeymapVerdict::Reject`]).
//!
//! Every judgment above runs on **firing bindings only**. A binding fires
//! when the resolver accepts it as written AND a keypress can reach it. It
//! is dead when its sequence contains the reserved unlock chord (the chord
//! resolves the instant it is pressed, and the rest of the sequence is
//! unreachable), when it is longer than `max_chord_depth`, or when a higher
//! layer `remove`s its key. A dead binding is warned once per layer with the
//! most specific reason, claims no key in the collision scan, and steals
//! nothing. A binding voided by a `remove` gets no warning: removing a key
//! in one layer and rebinding it in a higher layer moves the key between
//! layers without a collision. A dead binding is judged again on every
//! config load or reload and on every plugin load or unload.
//!
//! Detection reads the layers and writes nothing. Applying the verdict is
//! the caller's step.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use koshi_core::action::ActionReference;
use koshi_core::geometry::Direction;
use koshi_core::key::{KeyChord, KeySequence};
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action, ActionArgs, ResolveError};

use crate::key::Leader;
use crate::types::{
    build_default_mode_bindings, BoundAction, KeybindingsConfig, ModeBindings, ModeName,
};

/// Which configuration surface authored a keymap layer, lowest precedence
/// first. Every origin except `Defaults` is user-authored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerOrigin {
    /// The built-in default binding table koshi ships.
    Defaults,
    /// The user's own keymap file (`keybinding.kdl` in the koshi config
    /// directory).
    User,
    /// Per-named-session overrides.
    Session,
    /// Bindings a layout file declares for itself.
    Layout,
}

impl LayerOrigin {
    /// True for every origin the user wrote; false only for the built-in
    /// defaults. Cross-layer collision and the typeable warning apply to
    /// user-authored layers only.
    #[must_use]
    pub fn is_user_authored(self) -> bool {
        !matches!(self, Self::Defaults)
    }
}

impl fmt::Display for LayerOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Defaults => "defaults",
            Self::User => "user",
            Self::Session => "session",
            Self::Layout => "layout",
        })
    }
}

/// One keymap layer: the surface that authored it plus its per-mode bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapLayer {
    /// The surface this layer came from.
    pub origin: LayerOrigin,
    /// The layer's bindings, grouped by input mode.
    pub mode_bindings_by_name: BTreeMap<ModeName, ModeBindings>,
}

impl KeymapLayer {
    /// On a user-authored layer, replaces every binding's arguments with
    /// [`ActionArgs::None`], keeping only the key → action mapping. Any
    /// argument a user file carries (an unexpected KDL property, a
    /// hand-edited node) is dropped. The defaults layer is returned
    /// untouched, arguments included. [`build_keymap_layers`] applies this to the
    /// user layer.
    #[must_use]
    pub fn strip_user_arguments(mut self) -> Self {
        if !self.origin.is_user_authored() {
            return self;
        }
        for mode_bindings in self.mode_bindings_by_name.values_mut() {
            for bound_action in mode_bindings.bound_action_by_key_sequence.values_mut() {
                bound_action.action_arguments = ActionArgs::None;
            }
        }
        self
    }
}

/// The ordered keymap layers: the built-in default binding table, plus the
/// user's `keybinding.kdl` modes when present.
///
/// The default table is built against `leader`: every leader-relative default
/// moves with it, and a user file setting `leader "alt"` turns the `<C-p>`
/// pane prefix into `<A-p>`. The user layer passes through
/// [`KeymapLayer::strip_user_arguments`], which drops the binding
/// arguments a user file carries.
#[must_use]
pub fn build_keymap_layers(
    user_modes: Option<BTreeMap<ModeName, ModeBindings>>,
    leader: Leader,
) -> Vec<KeymapLayer> {
    let mut layers = vec![KeymapLayer {
        origin: LayerOrigin::Defaults,
        mode_bindings_by_name: build_default_mode_bindings(leader),
    }];
    if let Some(user_mode_bindings) = user_modes {
        layers.push(
            KeymapLayer {
                origin: LayerOrigin::User,
                mode_bindings_by_name: user_mode_bindings,
            }
            .strip_user_arguments(),
        );
    }
    layers
}

/// Every built-in input mode's name.
pub(crate) fn list_builtin_mode_names() -> BTreeSet<ModeName> {
    LockMode::ALL
        .iter()
        .map(|lock_mode| ModeName::from_text(lock_mode.get_keymap_name()))
        .collect()
}

/// How severe one finding is, mildest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictSeverity {
    /// Informational; the keymap still applies.
    Warning,
    /// A user-vs-user key collision; the user keymap reverts to defaults.
    Collision,
    /// The locked-mode unlock escape is compromised; the keymap is refused.
    Fatal,
}

/// What the caller does with the user keymap, decided by the worst finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapVerdict {
    /// No collision and nothing fatal: apply every layer.
    Apply,
    /// A key collision: drop every user-authored layer and run the built-in
    /// default bindings. Non-keybinding config is unaffected.
    RevertToDefaults,
    /// A fatal finding: refuse the keymap outright.
    Reject,
}

/// One finding from a detection run. `Display` gives the user-facing
/// message; [`get_severity`](Self::get_severity) gives its weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictDiagnostic {
    /// Two or more user-authored layers bind `key_sequence` in `mode_name` to
    /// different actions. `binding_claims` holds one entry per distinct bound
    /// action, in
    /// layer order.
    KeyCollision {
        /// The mode whose bindings collide.
        mode_name: ModeName,
        /// The key sequence both layers claim.
        key_sequence: KeySequence,
        /// Each distinct claim: the layer that made it and what it binds.
        binding_claims: Vec<(LayerOrigin, BoundAction)>,
    },
    /// `prefix` is bound, and so is a longer sequence starting with it.
    /// The prefix binding fires only on the chord timeout.
    AmbiguousPrefix {
        /// The mode holding both bindings.
        mode_name: ModeName,
        /// The shorter, fully-bound sequence.
        prefix_sequence: KeySequence,
        /// The action the shorter sequence triggers.
        prefix_action_reference: ActionReference,
        /// The longer sequence the prefix opens.
        longer_sequence: KeySequence,
        /// The action the longer sequence triggers.
        longer_action_reference: ActionReference,
    },
    /// The winning live locked-mode binding on the reserved unlock chord
    /// names an action other than `core:unlock`.
    ReservedUnlockShadowed {
        /// The layer whose binding won the reserved chord.
        layer_origin: LayerOrigin,
        /// The action bound in place of the working unlock.
        action_reference: ActionReference,
    },
    /// Locked mode has no binding from the reserved unlock chord to
    /// `core:unlock`.
    ReservedUnlockMissing {
        /// The chord that must map to `core:unlock`.
        reserved_unlock_chord: KeyChord,
    },
    /// `unlock_alternative` names a chord plain typing produces.
    UnlockAlternativeTypeable {
        /// The configured alternative chord.
        unlock_alternative_chord: KeyChord,
    },
    /// The effective `move-pane` map has no live `core:cancel-pane-move`
    /// binding, so an active placement cannot be cancelled from the keyboard.
    MovePaneCancelBindingMissing,
    /// A locked-mode sequence of two or more chords holds the reserved unlock
    /// chord. The chord resolves the instant it is pressed, ahead of the
    /// keymap and whether or not a sequence is open, and the sequence never
    /// fires. `<C-x> <C-l>` unlocks at the `<C-l>`.
    DeadUnderReservedUnlock {
        /// The layer that authored the dead binding.
        layer_origin: LayerOrigin,
        /// The sequence that can never fire.
        key_sequence: KeySequence,
        /// The action it would have triggered.
        action_reference: ActionReference,
    },
    /// A binding's sequence is longer than the `max_chord_depth` cap. No
    /// pending sequence grows long enough to reach it, and it never fires.
    ExceedsChordDepth {
        /// The layer holding the binding.
        layer_origin: LayerOrigin,
        /// The mode the binding lives in.
        mode_name: ModeName,
        /// The bound key sequence.
        key_sequence: KeySequence,
        /// The action it would have triggered.
        action_reference: ActionReference,
        /// The configured cap the sequence exceeds.
        max_chord_depth: u8,
    },
    /// A binding names a registered action the runtime does not implement in
    /// this build. The binding cannot fire.
    ComingSoonAction {
        /// The layer holding the binding.
        layer_origin: LayerOrigin,
        /// The mode the binding lives in.
        mode_name: ModeName,
        /// The bound key sequence.
        key_sequence: KeySequence,
        /// The not-yet-implemented action.
        action_reference: ActionReference,
    },
    /// A binding carries arguments its action cannot take, or names a macro
    /// the resolver refuses. The binding never fires as written.
    UnresolvableArgs {
        /// The layer holding the binding.
        layer_origin: LayerOrigin,
        /// The mode the binding lives in.
        mode_name: ModeName,
        /// The bound key sequence.
        key_sequence: KeySequence,
        /// The action whose arguments do not fit.
        action_reference: ActionReference,
    },
    /// A binding names an action the registry does not hold (for example,
    /// its plugin is not loaded). The binding is inactive until the action
    /// is registered.
    OrphanAction {
        /// The layer holding the binding.
        layer_origin: LayerOrigin,
        /// The mode the binding lives in.
        mode_name: ModeName,
        /// The bound key sequence.
        key_sequence: KeySequence,
        /// The unknown action reference.
        action_reference: ActionReference,
    },
    /// A layer declares bindings for a mode that is not registered. Those
    /// bindings are inactive until the mode is registered.
    OrphanMode {
        /// The layer declaring the mode.
        layer_origin: LayerOrigin,
        /// The unregistered mode name.
        mode_name: ModeName,
    },
    /// A user-authored binding opens with a chord plain typing produces,
    /// stealing that key from the pane whenever the client is not locked.
    TypeableBinding {
        /// The layer holding the binding.
        layer_origin: LayerOrigin,
        /// The mode the binding lives in.
        mode_name: ModeName,
        /// The bound key sequence.
        key_sequence: KeySequence,
        /// The action it triggers.
        action_reference: ActionReference,
    },
    /// The configured leader is reachable by plain typing. Every binding that
    /// starts with it steals a typeable key from the pane.
    TypeableLeader {
        /// The configured leader.
        leader: Leader,
    },
}

impl ConflictDiagnostic {
    /// The weight of this finding; the report's verdict follows the worst.
    #[must_use]
    pub fn get_severity(&self) -> ConflictSeverity {
        match self {
            Self::KeyCollision { .. } => ConflictSeverity::Collision,
            Self::ReservedUnlockShadowed { .. }
            | Self::ReservedUnlockMissing { .. }
            | Self::UnlockAlternativeTypeable { .. }
            | Self::MovePaneCancelBindingMissing => ConflictSeverity::Fatal,
            Self::AmbiguousPrefix { .. }
            | Self::DeadUnderReservedUnlock { .. }
            | Self::ExceedsChordDepth { .. }
            | Self::ComingSoonAction { .. }
            | Self::UnresolvableArgs { .. }
            | Self::OrphanAction { .. }
            | Self::OrphanMode { .. }
            | Self::TypeableBinding { .. }
            | Self::TypeableLeader { .. } => ConflictSeverity::Warning,
        }
    }
}

impl fmt::Display for ConflictDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyCollision {
                mode_name,
                key_sequence,
                binding_claims,
            } => {
                write!(
                    f,
                    "key `{key_sequence}` in mode `{}` is bound",
                    mode_name.get_name()
                )?;
                for (claim_index, (layer_origin, bound_action)) in
                    binding_claims.iter().enumerate()
                {
                    if claim_index > 0 {
                        f.write_str(" and")?;
                    }
                    write!(
                        f,
                        " by {layer_origin} to `{}`",
                        bound_action.action_reference
                    )?;
                }
                // Claims that all name one action differ only in their
                // arguments. Fewer than two claims name no difference.
                let is_all_claims_for_same_action = binding_claims.len() >= 2
                    && binding_claims
                        .windows(2)
                        .all(|claim_pair| {
                            claim_pair[0].1.action_reference == claim_pair[1].1.action_reference
                        });
                if is_all_claims_for_same_action {
                    f.write_str(" with different arguments")?;
                }
                f.write_str("; all user keybindings revert to defaults")
            }
            Self::AmbiguousPrefix {
                mode_name,
                prefix_sequence,
                prefix_action_reference,
                longer_sequence,
                longer_action_reference,
            } => write!(
                f,
                "`{prefix_sequence}` (`{prefix_action_reference}`) is a prefix of `{longer_sequence}` (`{longer_action_reference}`) \
                 in mode `{}`; the shorter binding fires only on the chord timeout",
                mode_name.get_name()
            ),
            Self::ReservedUnlockShadowed {
                layer_origin,
                action_reference,
            } => write!(
                f,
                "the reserved unlock key is bound by {layer_origin} to `{action_reference}` in locked mode; \
                 declare `unlock_alternative` before rebinding it"
            ),
            Self::ReservedUnlockMissing {
                reserved_unlock_chord,
            } => write!(
                f,
                "locked mode has no binding from `{reserved_unlock_chord}` to `core:unlock`; \
                 the unlock escape would be unreachable"
            ),
            Self::UnlockAlternativeTypeable {
                unlock_alternative_chord,
            } => write!(
                f,
                "`unlock_alternative` `{unlock_alternative_chord}` is a key plain typing produces; \
                 hold Ctrl, Alt, or Super"
            ),
            Self::MovePaneCancelBindingMissing => write!(
                f,
                "the `move-pane` mode has no live `core:cancel-pane-move` binding; \
                 bind that action to a key before removing its last cancellation key"
            ),
            Self::DeadUnderReservedUnlock {
                layer_origin,
                key_sequence,
                action_reference,
            } => write!(
                f,
                "`{key_sequence}` ({layer_origin}, `{action_reference}`) in locked mode can never fire: \
                 it holds the reserved unlock chord, which resolves instantly \
                 wherever it is pressed"
            ),
            Self::ExceedsChordDepth {
                layer_origin,
                mode_name,
                key_sequence,
                action_reference,
                max_chord_depth,
            } => write!(
                f,
                "`{key_sequence}` in mode `{}` ({layer_origin}, `{action_reference}`) is {} chords, over the \
                 `max_chord_depth` cap of {max_chord_depth}; the binding can never fire",
                mode_name.get_name(),
                key_sequence.list_chords().len()
            ),
            Self::ComingSoonAction {
                layer_origin,
                mode_name,
                key_sequence,
                action_reference,
            } => write!(
                f,
                "`{key_sequence}` in mode `{}` ({layer_origin}) binds `{action_reference}`, which is not \
                 implemented yet; the binding cannot fire until it is",
                mode_name.get_name()
            ),
            Self::UnresolvableArgs {
                layer_origin,
                mode_name,
                key_sequence,
                action_reference,
            } => write!(
                f,
                "`{key_sequence}` in mode `{}` ({layer_origin}) binds `{action_reference}` with arguments it \
                 cannot take; the binding can never fire as written",
                mode_name.get_name()
            ),
            Self::OrphanAction {
                layer_origin,
                mode_name,
                key_sequence,
                action_reference,
            } => write!(
                f,
                "`{key_sequence}` in mode `{}` ({layer_origin}) names unknown action `{action_reference}`; \
                 the binding is inactive until the action is registered",
                mode_name.get_name()
            ),
            Self::OrphanMode {
                layer_origin,
                mode_name,
            } => write!(
                f,
                "the {layer_origin} keymap binds keys in unregistered mode `{}`; \
                 those bindings are inactive until the mode is registered",
                mode_name.get_name()
            ),
            Self::TypeableBinding {
                layer_origin,
                mode_name,
                key_sequence,
                action_reference,
            } => write!(
                f,
                "`{key_sequence}` in mode `{}` ({layer_origin}, `{action_reference}`) opens with a key plain typing \
                 produces; it steals that key from the pane",
                mode_name.get_name()
            ),
            Self::TypeableLeader { leader } => write!(
                f,
                "leader `{leader}` is reachable by plain typing; bindings that start with \
                 it steal those keys from panes"
            ),
        }
    }
}

/// Every finding from one detection run, in scan order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictReport {
    /// The findings, each carrying its own severity and message.
    pub diagnostics: Vec<ConflictDiagnostic>,
}

impl ConflictReport {
    /// The keymap decision the worst finding demands: any fatal finding
    /// rejects, any collision reverts to defaults, warnings alone apply.
    #[must_use]
    pub fn get_verdict(&self) -> KeymapVerdict {
        let worst_severity = self
            .diagnostics
            .iter()
            .map(ConflictDiagnostic::get_severity)
            .max();
        match worst_severity {
            Some(ConflictSeverity::Fatal) => KeymapVerdict::Reject,
            Some(ConflictSeverity::Collision) => KeymapVerdict::RevertToDefaults,
            Some(ConflictSeverity::Warning) | None => KeymapVerdict::Apply,
        }
    }
}

/// Inspects keybinding layers (ordered lowest precedence first) and reports
/// every conflict finding.
///
/// `leader`, `unlock_alternative`, and `max_chord_depth` come from the
/// merged keybindings config; `registry` is the live action table each
/// binding is resolved against for the liveness judgment. A binding whose mode
/// is not one of the [`LockMode`] names is skipped. The reserved unlock chord
/// is `unlock_alternative` when set, otherwise
/// [`KeybindingsConfig::RESERVED_UNLOCK`].
#[must_use]
pub fn detect_conflicts(
    layers: &[KeymapLayer],
    leader: Leader,
    unlock_alternative: Option<KeyChord>,
    max_chord_depth: u8,
    registry: &ActionRegistry,
) -> ConflictReport {
    let known_mode_names = &list_builtin_mode_names();
    let mut conflict_diagnostics = Vec::new();
    let reserved_unlock_chord = unlock_alternative.unwrap_or(KeybindingsConfig::RESERVED_UNLOCK);
    let locked_mode_name = ModeName::from_text("locked");

    if is_leader_typeable(leader) {
        conflict_diagnostics.push(ConflictDiagnostic::TypeableLeader { leader });
    }
    if let Some(unlock_alternative_chord) = unlock_alternative {
        if unlock_alternative_chord.is_typeable() {
            conflict_diagnostics.push(ConflictDiagnostic::UnlockAlternativeTypeable {
                unlock_alternative_chord,
            });
        }
    }

    let removal_layer_index_by_mode_and_key = build_removal_layer_index(layers, known_mode_names);
    let firing_rules = FiringRules {
        registry,
        reserved_unlock_chord,
        locked_mode_name: &locked_mode_name,
        max_chord_depth,
    };

    for (layer_index, layer) in layers
        .iter()
        .enumerate()
        .filter(|(_, layer)| layer.origin.is_user_authored())
    {
        scan_layer_bindings(
            layer,
            layer_index,
            &removal_layer_index_by_mode_and_key,
            known_mode_names,
            &firing_rules,
            &mut conflict_diagnostics,
        );
    }

    scan_key_collisions(
        layers,
        &removal_layer_index_by_mode_and_key,
        known_mode_names,
        &firing_rules,
        &mut conflict_diagnostics,
    );

    let effective_bindings_by_mode = build_effective_bindings(
        layers,
        &removal_layer_index_by_mode_and_key,
        known_mode_names,
        &firing_rules,
    );
    scan_ambiguous_prefixes(&effective_bindings_by_mode, &mut conflict_diagnostics);
    validate_reserved_unlock_binding(
        &effective_bindings_by_mode,
        reserved_unlock_chord,
        &locked_mode_name,
        &mut conflict_diagnostics,
    );
    validate_move_pane_cancel_binding(&effective_bindings_by_mode, &mut conflict_diagnostics);

    ConflictReport {
        diagnostics: conflict_diagnostics,
    }
}

/// Whether one binding can fire, judged by handing it to action resolution,
/// the same code path a keypress takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingState {
    /// Resolution accepts the binding as written; it fires.
    Live,
    /// The action is not registered. Detection runs again on every plugin
    /// load or unload, and the binding fires once its action is registered.
    Orphan,
    /// The action is registered but not implemented in this build.
    ComingSoon,
    /// The arguments (or macro shape) can never resolve as written.
    Unresolvable,
}

/// For every `(mode_name, key_sequence)` some layer removes, the index of the
/// highest-precedence layer removing it. A binding at layer index
/// `layer_index` is voided when a removal for its key exists at a greater
/// index ([`is_removed_by_higher_layer`]). A layer's own remove never voids its own binding:
/// removing and rebinding a key in one layer keeps the rebind. Removals in
/// unregistered modes are skipped.
pub(crate) fn build_removal_layer_index<'a>(
    layers: &'a [KeymapLayer],
    known_mode_names: &BTreeSet<ModeName>,
) -> BTreeMap<(&'a ModeName, &'a KeySequence), usize> {
    let mut removal_layer_index_by_mode_and_key: BTreeMap<(&ModeName, &KeySequence), usize> =
        BTreeMap::new();
    for (layer_index, layer) in layers.iter().enumerate() {
        for (mode_name, mode_bindings) in &layer.mode_bindings_by_name {
            if !known_mode_names.contains(mode_name) {
                continue;
            }
            for key_sequence in &mode_bindings.removed_key_sequences {
                removal_layer_index_by_mode_and_key.insert((mode_name, key_sequence), layer_index);
            }
        }
    }
    removal_layer_index_by_mode_and_key
}

/// True when a layer above `layer_index` removes `(mode_name, key_sequence)`,
/// voiding any binding a layer at `layer_index` holds on it.
pub(crate) fn is_removed_by_higher_layer(
    removal_layer_index_by_mode_and_key: &BTreeMap<(&ModeName, &KeySequence), usize>,
    mode_name: &ModeName,
    key_sequence: &KeySequence,
    layer_index: usize,
) -> bool {
    removal_layer_index_by_mode_and_key
        .get(&(mode_name, key_sequence))
        .is_some_and(|&removal_layer_index| removal_layer_index > layer_index)
}

/// Classifies one binding with [`resolve_action`], the call a keypress makes.
fn classify_bound_action(bound_action: &BoundAction, registry: &ActionRegistry) -> BindingState {
    // Only whether the action resolves is read. The plan is dropped, and the
    // `Direction::Right` handed in reaches nothing.
    match resolve_action(
        &bound_action.action_reference,
        &bound_action.action_arguments,
        registry,
        Direction::Right,
    ) {
        Ok(_) => BindingState::Live,
        Err(ResolveError::Unregistered { .. }) => BindingState::Orphan,
        Err(ResolveError::ComingSoon { .. }) => BindingState::ComingSoon,
        Err(ResolveError::ArgsMismatch { .. } | ResolveError::SequenceTooDeep { .. }) => {
            BindingState::Unresolvable
        }
    }
}

/// True when `mode_name` is locked mode and `key_sequence` has two or more chords, one of
/// which is the reserved unlock chord. The input path resolves that chord the
/// instant it is pressed, ahead of the keymap and whether or not a sequence
/// is open, and a sequence holding it never fires.
///
/// Position does not matter. `<C-l> x` unlocks at the first chord; `<C-x>
/// <C-l>` opens and then unlocks at the second. A one-chord `<C-l>` is the
/// unlock binding itself and stays live.
fn is_sequence_dead_under_reserved_unlock(
    mode_name: &ModeName,
    key_sequence: &KeySequence,
    reserved_unlock_chord: KeyChord,
    locked_mode_name: &ModeName,
) -> bool {
    mode_name == locked_mode_name
        && key_sequence.list_chords().len() > 1
        && key_sequence.list_chords().contains(&reserved_unlock_chord)
}

/// The inputs the firing judgment reads: the live action table, the
/// reserved unlock chord, the locked mode's name, and the chord-depth cap.
/// One value serves a whole detection or merge run.
pub(crate) struct FiringRules<'a> {
    /// The live action table each binding is resolved against.
    pub(crate) registry: &'a ActionRegistry,
    /// The reserved unlock chord.
    pub(crate) reserved_unlock_chord: KeyChord,
    /// The locked mode's name.
    pub(crate) locked_mode_name: &'a ModeName,
    /// The chord-depth cap a firing sequence must fit.
    pub(crate) max_chord_depth: u8,
}

/// True when the binding fires: the resolver accepts it as written, its
/// sequence does not hold the reserved unlock chord in locked mode, and its
/// sequence fits the chord-depth cap. Only firing bindings claim keys in the
/// collision scan or enter the effective map. Removal by a higher layer is a
/// separate check the callers make with [`is_removed_by_higher_layer`]. The keymap-merge
/// pass reads this same predicate.
pub(crate) fn is_bound_action_firing(
    mode_name: &ModeName,
    key_sequence: &KeySequence,
    bound_action: &BoundAction,
    firing_rules: &FiringRules<'_>,
) -> bool {
    classify_bound_action(bound_action, firing_rules.registry) == BindingState::Live
        && !is_sequence_dead_under_reserved_unlock(
            mode_name,
            key_sequence,
            firing_rules.reserved_unlock_chord,
            firing_rules.locked_mode_name,
        )
        && !is_over_chord_depth_limit(key_sequence, firing_rules.max_chord_depth)
}

/// True when the sequence holds more than `max_chord_depth` chords. The input
/// path grows a pending sequence only while a longer live binding starts with
/// it; with no live binding past the cap, no pending sequence grows past it,
/// and a binding past the cap is never reached.
fn is_over_chord_depth_limit(key_sequence: &KeySequence, max_chord_depth: u8) -> bool {
    key_sequence.list_chords().len() > usize::from(max_chord_depth)
}

/// True when the leader is reachable by plain typing: a chord leader that is
/// itself typeable, or a modifier-run leader whose modifiers plain typing
/// produces ([`koshi_core::key::ModFlags::is_typing`] — Shift alone merges into typed keys).
fn is_leader_typeable(leader: Leader) -> bool {
    match leader {
        Leader::Mods(modifier_flags) => modifier_flags.is_typing(),
        Leader::Chord(key_chord) => key_chord.is_typeable(),
    }
}

/// Per-layer warnings for one user-authored layer. An unregistered mode
/// warns once ([`ConflictDiagnostic::OrphanMode`]) and its bindings are
/// skipped. A binding a higher layer removes is skipped with no warning.
/// Each remaining binding gets at most one cannot-fire warning, most specific
/// reason first: the resolver's refusal, then the reserved unlock chord, then
/// the chord-depth cap. Only a firing binding is checked for a typeable
/// opening chord.
fn scan_layer_bindings(
    layer: &KeymapLayer,
    layer_index: usize,
    removal_layer_index_by_mode_and_key: &BTreeMap<(&ModeName, &KeySequence), usize>,
    known_mode_names: &BTreeSet<ModeName>,
    firing_rules: &FiringRules<'_>,
    conflict_diagnostics: &mut Vec<ConflictDiagnostic>,
) {
    for (mode_name, mode_bindings) in &layer.mode_bindings_by_name {
        if !known_mode_names.contains(mode_name) {
            conflict_diagnostics.push(ConflictDiagnostic::OrphanMode {
                layer_origin: layer.origin,
                mode_name: mode_name.clone(),
            });
            continue;
        }
        for (key_sequence, bound_action) in &mode_bindings.bound_action_by_key_sequence {
            if is_removed_by_higher_layer(
                removal_layer_index_by_mode_and_key,
                mode_name,
                key_sequence,
                layer_index,
            ) {
                continue;
            }
            match classify_bound_action(bound_action, firing_rules.registry) {
                BindingState::Live => {}
                BindingState::Orphan => {
                    conflict_diagnostics.push(ConflictDiagnostic::OrphanAction {
                        layer_origin: layer.origin,
                        mode_name: mode_name.clone(),
                        key_sequence: key_sequence.clone(),
                        action_reference: bound_action.action_reference.clone(),
                    });
                    continue;
                }
                BindingState::ComingSoon => {
                    conflict_diagnostics.push(ConflictDiagnostic::ComingSoonAction {
                        layer_origin: layer.origin,
                        mode_name: mode_name.clone(),
                        key_sequence: key_sequence.clone(),
                        action_reference: bound_action.action_reference.clone(),
                    });
                    continue;
                }
                BindingState::Unresolvable => {
                    conflict_diagnostics.push(ConflictDiagnostic::UnresolvableArgs {
                        layer_origin: layer.origin,
                        mode_name: mode_name.clone(),
                        key_sequence: key_sequence.clone(),
                        action_reference: bound_action.action_reference.clone(),
                    });
                    continue;
                }
            }
            if is_sequence_dead_under_reserved_unlock(
                mode_name,
                key_sequence,
                firing_rules.reserved_unlock_chord,
                firing_rules.locked_mode_name,
            ) {
                conflict_diagnostics.push(ConflictDiagnostic::DeadUnderReservedUnlock {
                    layer_origin: layer.origin,
                    key_sequence: key_sequence.clone(),
                    action_reference: bound_action.action_reference.clone(),
                });
                continue;
            }
            if is_over_chord_depth_limit(key_sequence, firing_rules.max_chord_depth) {
                conflict_diagnostics.push(ConflictDiagnostic::ExceedsChordDepth {
                    layer_origin: layer.origin,
                    mode_name: mode_name.clone(),
                    key_sequence: key_sequence.clone(),
                    action_reference: bound_action.action_reference.clone(),
                    max_chord_depth: firing_rules.max_chord_depth,
                });
                continue;
            }
            if key_sequence.list_chords()[0].is_typeable() {
                conflict_diagnostics.push(ConflictDiagnostic::TypeableBinding {
                    layer_origin: layer.origin,
                    mode_name: mode_name.clone(),
                    key_sequence: key_sequence.clone(),
                    action_reference: bound_action.action_reference.clone(),
                });
            }
        }
    }
}

/// Cross-layer key collisions: the same `(mode_name, key_sequence)` bound to different
/// [`BoundAction`]s by two or more user-authored layers. Identical bound
/// actions in several layers pass. The defaults layer never collides: a
/// user binding on a defaulted key is a steal.
///
/// Only firing claims count ([`is_bound_action_firing`]): a binding that cannot fire
/// claims no key, and [`scan_layer_bindings`] warns it instead. The collision appears
/// on the detection run where the binding turns live: at plugin registration
/// for an orphan action, at the first load of a build that implements a
/// coming-soon action. A claim a higher layer removes claims no key either:
/// removing a key and rebinding it in a higher layer takes the key without a
/// collision.
fn scan_key_collisions(
    layers: &[KeymapLayer],
    removal_layer_index_by_mode_and_key: &BTreeMap<(&ModeName, &KeySequence), usize>,
    known_mode_names: &BTreeSet<ModeName>,
    firing_rules: &FiringRules<'_>,
    conflict_diagnostics: &mut Vec<ConflictDiagnostic>,
) {
    let mut binding_claims_by_mode_and_key: BTreeMap<
        (&ModeName, &KeySequence),
        Vec<(LayerOrigin, &BoundAction)>,
    > = BTreeMap::new();
    for (layer_index, layer) in layers
        .iter()
        .enumerate()
        .filter(|(_, layer)| layer.origin.is_user_authored())
    {
        for (mode_name, mode_bindings) in &layer.mode_bindings_by_name {
            if !known_mode_names.contains(mode_name) {
                continue;
            }
            for (key_sequence, bound_action) in &mode_bindings.bound_action_by_key_sequence {
                if is_removed_by_higher_layer(
                    removal_layer_index_by_mode_and_key,
                    mode_name,
                    key_sequence,
                    layer_index,
                ) || !is_bound_action_firing(mode_name, key_sequence, bound_action, firing_rules)
                {
                    continue;
                }
                let binding_claimants = binding_claims_by_mode_and_key
                    .entry((mode_name, key_sequence))
                    .or_default();
                if !binding_claimants
                    .iter()
                    .any(|(_, existing_bound_action)| *existing_bound_action == bound_action)
                {
                    binding_claimants.push((layer.origin, bound_action));
                }
            }
        }
    }
    for ((mode_name, key_sequence), binding_claimants) in binding_claims_by_mode_and_key {
        if binding_claimants.len() >= 2 {
            conflict_diagnostics.push(ConflictDiagnostic::KeyCollision {
                mode_name: mode_name.clone(),
                key_sequence: key_sequence.clone(),
                binding_claims: binding_claimants
                    .into_iter()
                    .map(|(layer_origin, bound_action)| (layer_origin, bound_action.clone()))
                    .collect(),
            });
        }
    }
}

/// The winning **firing** binding per `(mode_name, key_sequence)` after folding the
/// layers in order: a higher layer's firing entry replaces a lower layer's
/// on the same key. A binding that cannot fire is transparent, and the firing
/// binding beneath it shows through. A binding a higher layer removes,
/// bindings in unregistered modes, locked-mode sequences holding the
/// reserved unlock chord, and sequences past the chord-depth cap never
/// enter. The map holds what a keypress reaches.
fn build_effective_bindings<'a>(
    layers: &'a [KeymapLayer],
    removal_layer_index_by_mode_and_key: &BTreeMap<(&'a ModeName, &'a KeySequence), usize>,
    known_mode_names: &BTreeSet<ModeName>,
    firing_rules: &FiringRules<'_>,
) -> BTreeMap<&'a ModeName, BTreeMap<&'a KeySequence, (LayerOrigin, &'a BoundAction)>> {
    let mut effective_bindings_by_mode: BTreeMap<
        &ModeName,
        BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>,
    > = BTreeMap::new();
    for (layer_index, layer) in layers.iter().enumerate() {
        for (mode_name, mode_bindings) in &layer.mode_bindings_by_name {
            if !known_mode_names.contains(mode_name) {
                continue;
            }
            let merged_mode_bindings = effective_bindings_by_mode.entry(mode_name).or_default();
            for (key_sequence, bound_action) in &mode_bindings.bound_action_by_key_sequence {
                if is_removed_by_higher_layer(
                    removal_layer_index_by_mode_and_key,
                    mode_name,
                    key_sequence,
                    layer_index,
                ) || !is_bound_action_firing(mode_name, key_sequence, bound_action, firing_rules)
                {
                    continue;
                }
                merged_mode_bindings.insert(key_sequence, (layer.origin, bound_action));
            }
        }
    }
    effective_bindings_by_mode
}

/// Ambiguous-prefix warnings over the winning firing bindings: within one
/// mode, a bound sequence that is a strict prefix of another bound sequence
/// fires only on the chord timeout. One warning per prefix pair. Locked-mode
/// sequences holding the reserved unlock chord are absent from the effective
/// map and never pair here; [`scan_layer`] warns them as dead.
fn scan_ambiguous_prefixes(
    effective_bindings_by_mode: &BTreeMap<
        &ModeName,
        BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>,
    >,
    conflict_diagnostics: &mut Vec<ConflictDiagnostic>,
) {
    for (mode_name, mode_bindings) in effective_bindings_by_mode {
        for (shorter_key_sequence, (_, shorter_bound_action)) in mode_bindings {
            for (longer_key_sequence, (_, longer_bound_action)) in mode_bindings {
                let is_strict_prefix = shorter_key_sequence.list_chords().len()
                    < longer_key_sequence.list_chords().len()
                    && longer_key_sequence
                        .list_chords()
                        .starts_with(shorter_key_sequence.list_chords());
                if is_strict_prefix {
                    conflict_diagnostics.push(ConflictDiagnostic::AmbiguousPrefix {
                        mode_name: (*mode_name).clone(),
                        prefix_sequence: (*shorter_key_sequence).clone(),
                        prefix_action_reference: shorter_bound_action.action_reference.clone(),
                        longer_sequence: (*longer_key_sequence).clone(),
                        longer_action_reference: longer_bound_action.action_reference.clone(),
                    });
                }
            }
        }
    }
}

/// The locked-mode unlock check on the winning firing bindings. The firing
/// binding on the reserved chord in locked mode must name `core:unlock`:
/// another action is [`ConflictDiagnostic::ReservedUnlockShadowed`], and no
/// firing binding is [`ConflictDiagnostic::ReservedUnlockMissing`]. A dead
/// binding on the reserved chord is transparent and cannot shadow the
/// escape. The action alone is compared: the map holds firing bindings only,
/// and `core:unlock` resolves only with [`ActionArgs::None`].
fn validate_reserved_unlock_binding(
    effective_bindings_by_mode: &BTreeMap<
        &ModeName,
        BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>,
    >,
    reserved_unlock_chord: KeyChord,
    locked_mode_name: &ModeName,
    conflict_diagnostics: &mut Vec<ConflictDiagnostic>,
) {
    let unlock_action_reference = ActionReference::from_core_action_name("unlock")
        .expect("the built-in unlock action name satisfies the action-name grammar");
    let reserved_unlock_sequence = KeySequence::from(reserved_unlock_chord);

    match effective_bindings_by_mode
        .get(locked_mode_name)
        .and_then(|mode_bindings| mode_bindings.get(&reserved_unlock_sequence))
    {
        Some((layer_origin, bound_action))
            if bound_action.action_reference != unlock_action_reference =>
        {
            conflict_diagnostics.push(ConflictDiagnostic::ReservedUnlockShadowed {
                layer_origin: *layer_origin,
                action_reference: bound_action.action_reference.clone(),
            });
        }
        Some(_) => {}
        None => conflict_diagnostics.push(ConflictDiagnostic::ReservedUnlockMissing {
            reserved_unlock_chord,
        }),
    }
}

/// The effective `move-pane` map must keep one live cancellation action so a
/// keyboard placement always has a reachable exit.
fn validate_move_pane_cancel_binding(
    effective_bindings_by_mode: &BTreeMap<
        &ModeName,
        BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>,
    >,
    conflict_diagnostics: &mut Vec<ConflictDiagnostic>,
) {
    let move_pane_mode_name = ModeName::from_text("move-pane");
    let cancel_action_reference = ActionReference::from_core_action_name("cancel-pane-move")
        .expect(
            "the built-in pane-move cancellation action name satisfies the action-name grammar",
        );
    let has_cancel_binding = effective_bindings_by_mode
        .get(&move_pane_mode_name)
        .is_some_and(|mode_bindings| {
            mode_bindings
                .values()
                .any(|(_, bound_action)| bound_action.action_reference == cancel_action_reference)
        });

    if !has_cancel_binding {
        conflict_diagnostics.push(ConflictDiagnostic::MovePaneCancelBindingMissing);
    }
}

#[cfg(test)]
mod tests;
