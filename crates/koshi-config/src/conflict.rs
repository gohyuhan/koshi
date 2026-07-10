//! Keybinding conflict detection over ordered keymap layers.
//!
//! Bindings arrive in layers — the built-in defaults, then the user's own
//! surfaces (user file, project file, session, layout, manual `koshi keys`
//! edits), lowest first. Before the keymap-merge pass folds them into the
//! runtime lookup map, [`detect_conflicts`] inspects the layers and reports
//! every finding as a typed [`ConflictDiagnostic`]. The report's
//! [`verdict`](ConflictReport::verdict) tells the caller what to do with the
//! user keymap as a whole:
//!
//! - **Warnings** (ambiguous prefix, orphan action or mode, typeable keys,
//!   a binding shadowed by the reserved unlock) inform; the keymap applies.
//! - **A key collision** — the same key sequence claimed for different
//!   actions by two user-authored layers in one mode — makes one of those
//!   features unreachable, so the whole user keymap reverts to the built-in
//!   defaults ([`KeymapVerdict::RevertToDefaults`]). Non-keybinding config is
//!   unaffected. A user binding whose key is held only by the defaults layer
//!   is a *steal*, not a collision: the user's binding takes the key and the
//!   displaced default action becomes unbound.
//! - **A fatal finding** — the locked-mode unlock escape shadowed, missing,
//!   or typeable — refuses the keymap outright
//!   ([`KeymapVerdict::Reject`]): a config that can trap the user in locked
//!   mode never applies.
//!
//! Detection is pure: it reads the layers and writes nothing. Applying the
//! verdict — at load, new-session, or reload time — is the caller's step.
//! Detection runs on every config load and reload and on every plugin load
//! or unload (a plugin lifecycle change can orphan or un-orphan a binding).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use koshi_core::action::ActionRef;
use koshi_core::key::{KeyChord, KeySequence, ModFlags};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::ActionArgs;

use crate::key::Leader;
use crate::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};

/// Which configuration surface authored a keymap layer, lowest precedence
/// first. Every origin except `Defaults` is user-authored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerOrigin {
    /// The built-in default binding table koshi ships.
    Defaults,
    /// The user's own keymap file (`~/.config/koshi/keys.kdl`).
    User,
    /// A project-local keymap file (`.koshi/keys.kdl`).
    Project,
    /// Per-named-session overrides.
    Session,
    /// Bindings a layout file declares for itself.
    Layout,
    /// Runtime edits made through the `koshi keys` CLI.
    Manual,
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
            Self::Project => "project",
            Self::Session => "session",
            Self::Layout => "layout",
            Self::Manual => "manual",
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
///
/// This is the load / new-session decision. A live reload that fails keeps
/// the running keymap instead; that policy belongs to the reload
/// transaction, which reads the same report.
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
    /// The winning locked-mode binding on the reserved unlock chord is not
    /// the working unlock: it names another action, or `core:unlock` with
    /// arguments action resolution refuses to fire.
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
    /// `unlock_alternative` names a chord plain typing produces, which the
    /// unlock escape must never sit on.
    UnlockAlternativeTypeable {
        /// The configured alternative chord.
        chord: KeyChord,
    },
    /// A locked-mode sequence opens with the reserved unlock chord, which
    /// resolves instantly and never buffers, so the sequence cannot fire.
    DeadUnderReservedUnlock {
        /// The layer that authored the dead binding.
        origin: LayerOrigin,
        /// The sequence that can never fire.
        key: KeySequence,
        /// The action it would have triggered.
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
    /// The configured leader is reachable by plain typing, so every
    /// leader-opened binding steals a typeable key from the pane.
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
                 its first chord is the reserved unlock, which resolves instantly"
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
/// `leader` and `unlock_alternative` come from the merged keybindings
/// config; `registry` is the live action table for orphan checks;
/// `known_modes` holds every registered mode name (built-in and plugin).
/// The reserved unlock chord is `unlock_alternative` when set, otherwise
/// [`KeybindingsConfig::RESERVED_UNLOCK`].
#[must_use]
pub fn detect_conflicts(
    layers: &[KeyMapLayer],
    leader: Leader,
    unlock_alternative: Option<KeyChord>,
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

    for layer in layers.iter().filter(|l| l.origin.is_user_authored()) {
        scan_layer(layer, registry, known_modes, &mut diagnostics);
    }

    scan_collisions(layers, &mut diagnostics);

    let effective = effective_bindings(layers);
    scan_prefixes(&effective, reserved, &locked, &mut diagnostics);
    check_reserved_unlock(&effective, reserved, &locked, &mut diagnostics);

    ConflictReport { diagnostics }
}

/// True when the leader is reachable by plain typing: a chord leader that is
/// itself typeable, or a modifier-run leader holding none of Ctrl, Alt, or
/// Super (Shift alone merges into keys plain typing produces).
fn leader_is_typeable(leader: Leader) -> bool {
    const NON_TEXT: ModFlags = ModFlags::CTRL.union(ModFlags::ALT).union(ModFlags::SUPER);
    match leader {
        Leader::Mods(mods) => !mods.intersects(NON_TEXT),
        Leader::Chord(chord) => chord.is_typeable(),
    }
}

/// Per-layer warnings for one user-authored layer: unregistered modes,
/// bindings whose action the registry does not hold, and bindings that open
/// with a typeable chord.
fn scan_layer(
    layer: &KeyMapLayer,
    registry: &ActionRegistry,
    known_modes: &BTreeSet<ModeName>,
    out: &mut Vec<ConflictDiagnostic>,
) {
    for (mode, bindings) in &layer.modes {
        if !known_modes.contains(mode) {
            out.push(ConflictDiagnostic::OrphanMode {
                origin: layer.origin,
                mode: mode.clone(),
            });
        }
        for (key, bound) in &bindings.keys {
            if registry.lookup(&bound.action).is_none() {
                out.push(ConflictDiagnostic::OrphanAction {
                    origin: layer.origin,
                    mode: mode.clone(),
                    key: key.clone(),
                    action: bound.action.clone(),
                });
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
/// actions in several layers restate one intent and pass. The defaults
/// layer never collides — a user binding on a defaulted key is a steal.
fn scan_collisions(layers: &[KeyMapLayer], out: &mut Vec<ConflictDiagnostic>) {
    let mut claims: BTreeMap<(&ModeName, &KeySequence), Vec<(LayerOrigin, &BoundAction)>> =
        BTreeMap::new();
    for layer in layers.iter().filter(|l| l.origin.is_user_authored()) {
        for (mode, bindings) in &layer.modes {
            for (key, bound) in &bindings.keys {
                claims
                    .entry((mode, key))
                    .or_default()
                    .push((layer.origin, bound));
            }
        }
    }
    for ((mode, key), claimants) in claims {
        let mut distinct: Vec<(LayerOrigin, &BoundAction)> = Vec::new();
        for (origin, bound) in claimants {
            if !distinct.iter().any(|(_, held)| *held == bound) {
                distinct.push((origin, bound));
            }
        }
        if distinct.len() >= 2 {
            out.push(ConflictDiagnostic::KeyCollision {
                mode: mode.clone(),
                key: key.clone(),
                claims: distinct
                    .into_iter()
                    .map(|(origin, bound)| (origin, bound.clone()))
                    .collect(),
            });
        }
    }
}

/// The winning binding per `(mode, key)` after folding the layers in order:
/// a later layer's entry replaces a lower layer's on the same key.
fn effective_bindings(
    layers: &[KeyMapLayer],
) -> BTreeMap<&ModeName, BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>> {
    let mut effective: BTreeMap<&ModeName, BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>> =
        BTreeMap::new();
    for layer in layers {
        for (mode, bindings) in &layer.modes {
            let mode_map = effective.entry(mode).or_default();
            for (key, bound) in &bindings.keys {
                mode_map.insert(key, (layer.origin, bound));
            }
        }
    }
    effective
}

/// Ambiguous-prefix warnings over the winning bindings: within one mode, a
/// bound sequence that is a strict prefix of another bound sequence fires
/// only on the chord timeout. The locked-mode reserved unlock chord is
/// skipped here; sequences it opens are reported as dead by
/// [`check_reserved_unlock`], since the reserved chord resolves instantly.
fn scan_prefixes(
    effective: &BTreeMap<&ModeName, BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>>,
    reserved: KeyChord,
    locked: &ModeName,
    out: &mut Vec<ConflictDiagnostic>,
) {
    for (mode, bindings) in effective {
        for (short, (_, short_bound)) in bindings {
            if **mode == *locked && short.chords() == [reserved] {
                continue;
            }
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

/// The locked-mode unlock guarantee, checked on the winning bindings: the
/// reserved chord must carry exactly the working unlock binding —
/// `core:unlock` with no arguments, the one form action resolution fires
/// (shadowed or missing is fatal) — and any longer locked-mode sequence
/// opening with the reserved chord is dead, because the reserved chord
/// resolves instantly and never buffers.
fn check_reserved_unlock(
    effective: &BTreeMap<&ModeName, BTreeMap<&KeySequence, (LayerOrigin, &BoundAction)>>,
    reserved: KeyChord,
    locked: &ModeName,
    out: &mut Vec<ConflictDiagnostic>,
) {
    let unlock = BoundAction {
        action: ActionRef::core("unlock")
            .expect("the built-in unlock action name satisfies the action-name grammar"),
        args: ActionArgs::None,
    };
    let reserved_seq = KeySequence::from(reserved);
    let locked_map = effective.get(locked);

    match locked_map.and_then(|bindings| bindings.get(&reserved_seq)) {
        Some((origin, bound)) if **bound != unlock => {
            out.push(ConflictDiagnostic::ReservedUnlockShadowed {
                origin: *origin,
                action: bound.action.clone(),
            });
        }
        Some(_) => {}
        None => out.push(ConflictDiagnostic::ReservedUnlockMissing { reserved }),
    }

    if let Some(bindings) = locked_map {
        for (key, (origin, bound)) in bindings {
            if key.chords().len() > 1 && key.chords()[0] == reserved {
                out.push(ConflictDiagnostic::DeadUnderReservedUnlock {
                    origin: *origin,
                    key: (*key).clone(),
                    action: bound.action.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
