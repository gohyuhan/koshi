//! Keybinding conflict detection over ordered keymap layers.
//!
//! Bindings arrive in layers, lowest precedence first: the built-in
//! defaults, then the user's own surfaces (user file, session, layout).
//! [`detect_conflicts`] inspects the layers before the keymap-merge pass
//! folds them into the runtime lookup map, and reports every finding as a
//! typed [`ConflictDiagnostic`]. The report's
//! [`verdict`](ConflictReport::verdict) tells the caller what to do with the
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
//! - **A fatal finding** — the locked-mode unlock escape shadowed, missing,
//!   or typeable — refuses the keymap outright ([`KeymapVerdict::Reject`]).
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

use koshi_core::action::ActionRef;
use koshi_core::geometry::Direction;
use koshi_core::key::{KeyChord, KeySequence};
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action, ActionArgs, ResolveError};

use crate::key::Leader;
use crate::types::{default_mode_bindings, BoundAction, KeybindingsConfig, ModeBindings, ModeName};

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
pub struct KeyMapLayer {
    /// The surface this layer came from.
    pub origin: LayerOrigin,
    /// The layer's bindings, grouped by input mode.
    pub modes: BTreeMap<ModeName, ModeBindings>,
}

impl KeyMapLayer {
    /// On a user-authored layer, replaces every binding's arguments with
    /// [`ActionArgs::None`], keeping only the key → action mapping. Any
    /// argument a user file carries (an unexpected KDL property, a
    /// hand-edited node) is dropped. The defaults layer is returned
    /// untouched, arguments included. [`keymap_layers`] applies this to the
    /// user layer.
    #[must_use]
    pub fn with_user_args_stripped(mut self) -> Self {
        if !self.origin.is_user_authored() {
            return self;
        }
        for bindings in self.modes.values_mut() {
            for bound in bindings.keys.values_mut() {
                bound.args = ActionArgs::None;
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
/// [`KeyMapLayer::with_user_args_stripped`], which drops the binding
/// arguments a user file carries.
#[must_use]
pub fn keymap_layers(
    user_modes: Option<BTreeMap<ModeName, ModeBindings>>,
    leader: Leader,
) -> Vec<KeyMapLayer> {
    let mut layers = vec![KeyMapLayer {
        origin: LayerOrigin::Defaults,
        modes: default_mode_bindings(leader),
    }];
    if let Some(modes) = user_modes {
        layers.push(
            KeyMapLayer {
                origin: LayerOrigin::User,
                modes,
            }
            .with_user_args_stripped(),
        );
    }
    layers
}

/// Every built-in input mode's name.
#[must_use]
pub fn built_in_modes() -> BTreeSet<ModeName> {
    LockMode::ALL
        .iter()
        .map(|mode| ModeName::new(mode.name()))
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
/// message; [`severity`](Self::severity) gives its weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictDiagnostic {
    /// Two or more user-authored layers bind `key` in `mode` to different
    /// actions. `claims` holds one entry per distinct bound action, in
    /// layer order.
    KeyCollision {
        /// The mode whose bindings collide.
        mode: ModeName,
        /// The key sequence both layers claim.
        key: KeySequence,
        /// Each distinct claim: the layer that made it and what it binds.
        claims: Vec<(LayerOrigin, BoundAction)>,
    },
    /// `prefix` is bound, and so is a longer sequence starting with it.
    /// The prefix binding fires only on the chord timeout.
    AmbiguousPrefix {
        /// The mode holding both bindings.
        mode: ModeName,
        /// The shorter, fully-bound sequence.
        prefix: KeySequence,
        /// The action the shorter sequence triggers.
        prefix_action: ActionRef,
        /// The longer sequence the prefix opens.
        longer: KeySequence,
        /// The action the longer sequence triggers.
        longer_action: ActionRef,
    },
    /// The winning live locked-mode binding on the reserved unlock chord
    /// names an action other than `core:unlock`.
    ReservedUnlockShadowed {
        /// The layer whose binding won the reserved chord.
        origin: LayerOrigin,
        /// The action bound in place of the working unlock.
        action: ActionRef,
    },
    /// Locked mode has no binding from the reserved unlock chord to
    /// `core:unlock`.
    ReservedUnlockMissing {
        /// The chord that must map to `core:unlock`.
        reserved: KeyChord,
    },
    /// `unlock_alternative` names a chord plain typing produces.
    UnlockAlternativeTypeable {
        /// The configured alternative chord.
        chord: KeyChord,
    },
    /// A locked-mode sequence of two or more chords holds the reserved unlock
    /// chord. The chord resolves the instant it is pressed, ahead of the
    /// keymap and whether or not a sequence is open, and the sequence never
    /// fires. `<C-x> <C-l>` unlocks at the `<C-l>`.
    DeadUnderReservedUnlock {
        /// The layer that authored the dead binding.
        origin: LayerOrigin,
        /// The sequence that can never fire.
        key: KeySequence,
        /// The action it would have triggered.
        action: ActionRef,
    },
    /// A binding's sequence is longer than the `max_chord_depth` cap. No
    /// pending sequence grows long enough to reach it, and it never fires.
    ExceedsChordDepth {
        /// The layer holding the binding.
        origin: LayerOrigin,
        /// The mode the binding lives in.
        mode: ModeName,
        /// The bound key sequence.
        key: KeySequence,
        /// The action it would have triggered.
        action: ActionRef,
        /// The configured cap the sequence exceeds.
        max_chord_depth: u8,
    },
    /// A binding names a registered action the runtime does not implement in
    /// this build. The binding cannot fire.
    ComingSoonAction {
        /// The layer holding the binding.
        origin: LayerOrigin,
        /// The mode the binding lives in.
        mode: ModeName,
        /// The bound key sequence.
        key: KeySequence,
        /// The not-yet-implemented action.
        action: ActionRef,
    },
    /// A binding carries arguments its action cannot take, or names a macro
    /// the resolver refuses. The binding never fires as written.
    UnresolvableArgs {
        /// The layer holding the binding.
        origin: LayerOrigin,
        /// The mode the binding lives in.
        mode: ModeName,
        /// The bound key sequence.
        key: KeySequence,
        /// The action whose arguments do not fit.
        action: ActionRef,
    },
    /// A binding names an action the registry does not hold (for example,
    /// its plugin is not loaded). The binding is inactive until the action
    /// is registered.
    OrphanAction {
        /// The layer holding the binding.
        origin: LayerOrigin,
        /// The mode the binding lives in.
        mode: ModeName,
        /// The bound key sequence.
        key: KeySequence,
        /// The unknown action reference.
        action: ActionRef,
    },
    /// A layer declares bindings for a mode that is not registered. Those
    /// bindings are inactive until the mode is registered.
    OrphanMode {
        /// The layer declaring the mode.
        origin: LayerOrigin,
        /// The unregistered mode name.
        mode: ModeName,
    },
    /// A user-authored binding opens with a chord plain typing produces,
    /// stealing that key from the pane whenever the client is not locked.
    TypeableBinding {
        /// The layer holding the binding.
        origin: LayerOrigin,
        /// The mode the binding lives in.
        mode: ModeName,
        /// The bound key sequence.
        key: KeySequence,
        /// The action it triggers.
        action: ActionRef,
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
    pub fn severity(&self) -> ConflictSeverity {
        match self {
            Self::KeyCollision { .. } => ConflictSeverity::Collision,
            Self::ReservedUnlockShadowed { .. }
            | Self::ReservedUnlockMissing { .. }
            | Self::UnlockAlternativeTypeable { .. } => ConflictSeverity::Fatal,
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
            Self::KeyCollision { mode, key, claims } => {
                write!(f, "key `{key}` in mode `{}` is bound", mode.as_str())?;
                for (i, (origin, bound)) in claims.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" and")?;
                    }
                    write!(f, " by {origin} to `{}`", bound.action)?;
                }
                // Claims that all name one action differ only in their
                // arguments.
                let same_action = claims
                    .windows(2)
                    .all(|pair| pair[0].1.action == pair[1].1.action);
                if same_action {
                    f.write_str(" with different arguments")?;
                }
                f.write_str("; all user keybindings revert to defaults")
            }
            Self::AmbiguousPrefix {
                mode,
                prefix,
                prefix_action,
                longer,
                longer_action,
            } => write!(
                f,
                "`{prefix}` (`{prefix_action}`) is a prefix of `{longer}` (`{longer_action}`) \
                 in mode `{}`; the shorter binding fires only on the chord timeout",
                mode.as_str()
            ),
            Self::ReservedUnlockShadowed { origin, action } => write!(
                f,
                "the reserved unlock key is bound by {origin} to `{action}` in locked mode; \
                 declare `unlock_alternative` before rebinding it"
            ),
            Self::ReservedUnlockMissing { reserved } => write!(
                f,
                "locked mode has no binding from `{reserved}` to `core:unlock`; \
                 the unlock escape would be unreachable"
            ),
            Self::UnlockAlternativeTypeable { chord } => write!(
                f,
                "`unlock_alternative` `{chord}` is a key plain typing produces; \
                 hold Ctrl, Alt, or Super"
            ),
            Self::DeadUnderReservedUnlock {
                origin,
                key,
                action,
            } => write!(
                f,
                "`{key}` ({origin}, `{action}`) in locked mode can never fire: \
                 it holds the reserved unlock chord, which resolves instantly \
                 wherever it is pressed"
            ),
            Self::ExceedsChordDepth {
                origin,
                mode,
                key,
                action,
                max_chord_depth,
            } => write!(
                f,
                "`{key}` in mode `{}` ({origin}, `{action}`) is {} chords, over the \
                 `max_chord_depth` cap of {max_chord_depth}; the binding can never fire",
                mode.as_str(),
                key.chords().len()
            ),
            Self::ComingSoonAction {
                origin,
                mode,
                key,
                action,
            } => write!(
                f,
                "`{key}` in mode `{}` ({origin}) binds `{action}`, which is not \
                 implemented yet; the binding cannot fire until it is",
                mode.as_str()
            ),
            Self::UnresolvableArgs {
                origin,
                mode,
                key,
                action,
            } => write!(
                f,
                "`{key}` in mode `{}` ({origin}) binds `{action}` with arguments it \
                 cannot take; the binding can never fire as written",
                mode.as_str()
            ),
            Self::OrphanAction {
                origin,
                mode,
                key,
                action,
            } => write!(
                f,
                "`{key}` in mode `{}` ({origin}) names unknown action `{action}`; \
                 the binding is inactive until the action is registered",
                mode.as_str()
            ),
            Self::OrphanMode { origin, mode } => write!(
                f,
                "the {origin} keymap binds keys in unregistered mode `{}`; \
                 those bindings are inactive until the mode is registered",
                mode.as_str()
            ),
            Self::TypeableBinding {
                origin,
                mode,
                key,
                action,
            } => write!(
                f,
                "`{key}` in mode `{}` ({origin}, `{action}`) opens with a key plain typing \
                 produces; it steals that key from the pane",
                mode.as_str()
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
    pub fn verdict(&self) -> KeymapVerdict {
        let worst = self
            .diagnostics
            .iter()
            .map(ConflictDiagnostic::severity)
            .max();
        match worst {
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
/// binding is resolved against for the liveness judgment; `known_modes`
/// holds every registered mode name (built-in and plugin).
/// The reserved unlock chord is `unlock_alternative` when set, otherwise
/// [`KeybindingsConfig::RESERVED_UNLOCK`].
#[must_use]
pub fn detect_conflicts(
    layers: &[KeyMapLayer],
    leader: Leader,
    unlock_alternative: Option<KeyChord>,
    max_chord_depth: u8,
    registry: &ActionRegistry,
    known_modes: &BTreeSet<ModeName>,
) -> ConflictReport {
    let mut diagnostics = Vec::new();
    let reserved = unlock_alternative.unwrap_or(KeybindingsConfig::RESERVED_UNLOCK);
    let locked = ModeName::new("locked");

    if leader_is_typeable(leader) {
        diagnostics.push(ConflictDiagnostic::TypeableLeader { leader });
    }
    if let Some(chord) = unlock_alternative {
        if chord.is_typeable() {
            diagnostics.push(ConflictDiagnostic::UnlockAlternativeTypeable { chord });
        }
    }

    let removals = removal_index(layers, known_modes);
    let rules = FiringRules {
        registry,
        reserved,
        locked: &locked,
        max_chord_depth,
    };

    for (index, layer) in layers
        .iter()
        .enumerate()
        .filter(|(_, layer)| layer.origin.is_user_authored())
    {
        scan_layer(
            layer,
            index,
            &removals,
            known_modes,
            &rules,
            &mut diagnostics,
        );
    }

    scan_collisions(layers, &removals, known_modes, &rules, &mut diagnostics);

    let effective = effective_bindings(layers, &removals, known_modes, &rules);
    scan_prefixes(&effective, &mut diagnostics);
    check_reserved_unlock(&effective, reserved, &locked, &mut diagnostics);

    ConflictReport { diagnostics }
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

/// For every `(mode, key)` some layer removes, the index of the
/// highest-precedence layer removing it. A binding at layer index `i` is
/// voided when a removal for its key exists at an index greater than `i`
/// ([`removed_above`]). A layer's own remove never voids its own binding:
/// removing and rebinding a key in one layer keeps the rebind. Removals in
/// unregistered modes are skipped.
pub(crate) fn removal_index<'a>(
    layers: &'a [KeyMapLayer],
    known_modes: &BTreeSet<ModeName>,
) -> BTreeMap<(&'a ModeName, &'a KeySequence), usize> {
    let mut removals: BTreeMap<(&ModeName, &KeySequence), usize> = BTreeMap::new();
    for (index, layer) in layers.iter().enumerate() {
        for (mode, bindings) in &layer.modes {
            if !known_modes.contains(mode) {
                continue;
            }
            for key in &bindings.removed {
                removals.insert((mode, key), index);
            }
        }
    }
    removals
}

/// True when a layer above `index` removes `(mode, key)`, voiding any
/// binding a layer at `index` holds on it.
pub(crate) fn removed_above(
    removals: &BTreeMap<(&ModeName, &KeySequence), usize>,
    mode: &ModeName,
    key: &KeySequence,
    index: usize,
) -> bool {
    removals.get(&(mode, key)).is_some_and(|&at| at > index)
}

/// Classifies one binding with [`resolve_action`], the call a keypress makes.
fn classify(bound: &BoundAction, registry: &ActionRegistry) -> BindingState {
    // Only whether the action resolves is read. The plan is dropped, and the
    // `Direction::Right` handed in reaches nothing.
    match resolve_action(&bound.action, &bound.args, registry, Direction::Right) {
        Ok(_) => BindingState::Live,
        Err(ResolveError::Unregistered { .. }) => BindingState::Orphan,
        Err(ResolveError::ComingSoon { .. }) => BindingState::ComingSoon,
        Err(ResolveError::ArgsMismatch { .. } | ResolveError::SequenceTooDeep { .. }) => {
            BindingState::Unresolvable
        }
    }
}

/// True when `mode` is locked mode and `key` has two or more chords, one of
/// which is the reserved unlock chord. The input path resolves that chord the
/// instant it is pressed, ahead of the keymap and whether or not a sequence
/// is open, and a sequence holding it never fires.
///
/// Position does not matter. `<C-l> x` unlocks at the first chord; `<C-x>
/// <C-l>` opens and then unlocks at the second. A one-chord `<C-l>` is the
/// unlock binding itself and stays live.
fn holds_reserved_unlock(
    mode: &ModeName,
    key: &KeySequence,
    reserved: KeyChord,
    locked: &ModeName,
) -> bool {
    mode == locked && key.chords().len() > 1 && key.chords().contains(&reserved)
}

/// The inputs the firing judgment reads: the live action table, the
/// reserved unlock chord, the locked mode's name, and the chord-depth cap.
/// One value serves a whole detection or merge run.
pub(crate) struct FiringRules<'a> {
    /// The live action table each binding is resolved against.
    pub(crate) registry: &'a ActionRegistry,
    /// The reserved unlock chord.
    pub(crate) reserved: KeyChord,
    /// The locked mode's name.
    pub(crate) locked: &'a ModeName,
    /// The chord-depth cap a firing sequence must fit.
    pub(crate) max_chord_depth: u8,
}

/// True when the binding fires: the resolver accepts it as written, its
/// sequence does not hold the reserved unlock chord in locked mode, and its
/// sequence fits the chord-depth cap. Only firing bindings claim keys in the
/// collision scan or enter the effective map. Removal by a higher layer is a
/// separate check the callers make with [`removed_above`]. The keymap-merge
/// pass reads this same predicate.
pub(crate) fn is_firing(
    mode: &ModeName,
    key: &KeySequence,
    bound: &BoundAction,
    rules: &FiringRules<'_>,
) -> bool {
    classify(bound, rules.registry) == BindingState::Live
        && !holds_reserved_unlock(mode, key, rules.reserved, rules.locked)
        && !exceeds_chord_depth(key, rules.max_chord_depth)
}

/// True when the sequence holds more than `max_chord_depth` chords. The input
/// path grows a pending sequence only while a longer live binding starts with
/// it; with no live binding past the cap, no pending sequence grows past it,
/// and a binding past the cap is never reached.
fn exceeds_chord_depth(key: &KeySequence, max_chord_depth: u8) -> bool {
    key.chords().len() > usize::from(max_chord_depth)
}

/// True when the leader is reachable by plain typing: a chord leader that is
/// itself typeable, or a modifier-run leader whose modifiers plain typing
/// produces ([`ModFlags::is_typing`] — Shift alone merges into typed keys).
fn leader_is_typeable(leader: Leader) -> bool {
    match leader {
        Leader::Mods(mods) => mods.is_typing(),
        Leader::Chord(chord) => chord.is_typeable(),
    }
}

/// Per-layer warnings for one user-authored layer. An unregistered mode
/// warns once ([`ConflictDiagnostic::OrphanMode`]) and its bindings are
/// skipped. A binding a higher layer removes is skipped with no warning.
/// Each remaining binding gets at most one cannot-fire warning, most specific
/// reason first: the resolver's refusal, then the reserved unlock chord, then
/// the chord-depth cap. Only a firing binding is checked for a typeable
/// opening chord.
fn scan_layer(
    layer: &KeyMapLayer,
    index: usize,
    removals: &BTreeMap<(&ModeName, &KeySequence), usize>,
    known_modes: &BTreeSet<ModeName>,
    rules: &FiringRules<'_>,
    out: &mut Vec<ConflictDiagnostic>,
) {
    for (mode, bindings) in &layer.modes {
        if !known_modes.contains(mode) {
            out.push(ConflictDiagnostic::OrphanMode {
                origin: layer.origin,
                mode: mode.clone(),
            });
            continue;
        }
        for (key, bound) in &bindings.keys {
            if removed_above(removals, mode, key, index) {
                continue;
            }
            match classify(bound, rules.registry) {
                BindingState::Live => {}
                BindingState::Orphan => {
                    out.push(ConflictDiagnostic::OrphanAction {
                        origin: layer.origin,
                        mode: mode.clone(),
                        key: key.clone(),
                        action: bound.action.clone(),
                    });
                    continue;
                }
                BindingState::ComingSoon => {
                    out.push(ConflictDiagnostic::ComingSoonAction {
                        origin: layer.origin,
                        mode: mode.clone(),
                        key: key.clone(),
                        action: bound.action.clone(),
                    });
                    continue;
                }
                BindingState::Unresolvable => {
                    out.push(ConflictDiagnostic::UnresolvableArgs {
                        origin: layer.origin,
                        mode: mode.clone(),
                        key: key.clone(),
                        action: bound.action.clone(),
                    });
                    continue;
                }
            }
            if holds_reserved_unlock(mode, key, rules.reserved, rules.locked) {
                out.push(ConflictDiagnostic::DeadUnderReservedUnlock {
                    origin: layer.origin,
                    key: key.clone(),
                    action: bound.action.clone(),
                });
                continue;
            }
            if exceeds_chord_depth(key, rules.max_chord_depth) {
                out.push(ConflictDiagnostic::ExceedsChordDepth {
                    origin: layer.origin,
                    mode: mode.clone(),
                    key: key.clone(),
                    action: bound.action.clone(),
                    max_chord_depth: rules.max_chord_depth,
                });
                continue;
            }
            if key.chords()[0].is_typeable() {
                out.push(ConflictDiagnostic::TypeableBinding {
                    origin: layer.origin,
                    mode: mode.clone(),
                    key: key.clone(),
                    action: bound.action.clone(),
                });
            }
        }
    }
}

/// Cross-layer key collisions: the same `(mode, key)` bound to different
/// [`BoundAction`]s by two or more user-authored layers. Identical bound
/// actions in several layers pass. The defaults layer never collides: a
/// user binding on a defaulted key is a steal.
///
/// Only firing claims count ([`is_firing`]): a binding that cannot fire
/// claims no key, and [`scan_layer`] warns it instead. The collision appears
/// on the detection run where the binding turns live: at plugin registration
/// for an orphan action, at the first load of a build that implements a
/// coming-soon action. A claim a higher layer removes claims no key either:
/// removing a key and rebinding it in a higher layer takes the key without a
/// collision.
fn scan_collisions(
    layers: &[KeyMapLayer],
    removals: &BTreeMap<(&ModeName, &KeySequence), usize>,
    known_modes: &BTreeSet<ModeName>,
    rules: &FiringRules<'_>,
    out: &mut Vec<ConflictDiagnostic>,
) {
    let mut claims: BTreeMap<(&ModeName, &KeySequence), Vec<(LayerOrigin, &BoundAction)>> =
        BTreeMap::new();
    for (index, layer) in layers
        .iter()
        .enumerate()
        .filter(|(_, layer)| layer.origin.is_user_authored())
    {
        for (mode, bindings) in &layer.modes {
            if !known_modes.contains(mode) {
                continue;
            }
            for (key, bound) in &bindings.keys {
                if removed_above(removals, mode, key, index) || !is_firing(mode, key, bound, rules)
                {
                    continue;
                }
                let claimants = claims.entry((mode, key)).or_default();
                if !claimants.iter().any(|(_, held)| *held == bound) {
                    claimants.push((layer.origin, bound));
                }
            }
        }
    }
    for ((mode, key), claimants) in claims {
        if claimants.len() >= 2 {
            out.push(ConflictDiagnostic::KeyCollision {
                mode: mode.clone(),
                key: key.clone(),
                claims: claimants
                    .into_iter()
                    .map(|(origin, bound)| (origin, bound.clone()))
                    .collect(),
            });
        }
    }
}

/// The winning **firing** binding per `(mode, key)` after folding the
/// layers in order: a higher layer's firing entry replaces a lower layer's
/// on the same key. A binding that cannot fire is transparent, and the firing
/// binding beneath it shows through. A binding a higher layer removes,
/// bindings in unregistered modes, locked-mode sequences holding the
/// reserved unlock chord, and sequences past the chord-depth cap never
/// enter. The map holds what a keypress reaches.
fn effective_bindings<'a>(
    layers: &'a [KeyMapLayer],
    removals: &BTreeMap<(&'a ModeName, &'a KeySequence), usize>,
    known_modes: &BTreeSet<ModeName>,
    rules: &FiringRules<'_>,
) -> BTreeMap<&'a ModeName, BTreeMap<&'a KeySequence, (LayerOrigin, &'a BoundAction)>> {
    let mut effective: BTreeMap<&ModeName, BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>> =
        BTreeMap::new();
    for (index, layer) in layers.iter().enumerate() {
        for (mode, bindings) in &layer.modes {
            if !known_modes.contains(mode) {
                continue;
            }
            let mode_map = effective.entry(mode).or_default();
            for (key, bound) in &bindings.keys {
                if removed_above(removals, mode, key, index) || !is_firing(mode, key, bound, rules)
                {
                    continue;
                }
                mode_map.insert(key, (layer.origin, bound));
            }
        }
    }
    effective
}

/// Ambiguous-prefix warnings over the winning firing bindings: within one
/// mode, a bound sequence that is a strict prefix of another bound sequence
/// fires only on the chord timeout. One warning per prefix pair. Locked-mode
/// sequences holding the reserved unlock chord are absent from the effective
/// map and never pair here; [`scan_layer`] warns them as dead.
fn scan_prefixes(
    effective: &BTreeMap<&ModeName, BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>>,
    out: &mut Vec<ConflictDiagnostic>,
) {
    for (mode, bindings) in effective {
        for (short, (_, short_bound)) in bindings {
            for (long, (_, long_bound)) in bindings {
                let is_strict_prefix = short.chords().len() < long.chords().len()
                    && long.chords().starts_with(short.chords());
                if is_strict_prefix {
                    out.push(ConflictDiagnostic::AmbiguousPrefix {
                        mode: (*mode).clone(),
                        prefix: (*short).clone(),
                        prefix_action: short_bound.action.clone(),
                        longer: (*long).clone(),
                        longer_action: long_bound.action.clone(),
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
fn check_reserved_unlock(
    effective: &BTreeMap<&ModeName, BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>>,
    reserved: KeyChord,
    locked: &ModeName,
    out: &mut Vec<ConflictDiagnostic>,
) {
    let unlock = ActionRef::core("unlock")
        .expect("the built-in unlock action name satisfies the action-name grammar");
    let reserved_seq = KeySequence::from(reserved);

    match effective
        .get(locked)
        .and_then(|bindings| bindings.get(&reserved_seq))
    {
        Some((origin, bound)) if bound.action != unlock => {
            out.push(ConflictDiagnostic::ReservedUnlockShadowed {
                origin: *origin,
                action: bound.action.clone(),
            });
        }
        Some(_) => {}
        None => out.push(ConflictDiagnostic::ReservedUnlockMissing { reserved }),
    }
}

#[cfg(test)]
mod tests;
