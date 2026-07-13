//! Keybinding file parsing: KDL text describing the keybindings section into
//! a [`PartialKeybindingsConfig`].
//!
//! The keybinding file is the whole keybindings section, one file. Top-level
//! setting nodes (`chord-timeout-ms 500`, `which-key-delay-ms 400`,
//! `max-chord-depth 4`, `leader "<C-p>"`, `unlock-alternative "<A-u>"`, an
//! optional `version 1`) sit beside `mode "name"` blocks holding the
//! bindings: `bind "<C-t>" "core:new-tab"` maps a key sequence to a full
//! action reference, and `remove "Tab"` clears the key in that mode, voiding
//! whatever a lower layer bound on it. A `bind` carries no arguments —
//! argument presets are authored by the built-in binding table, so a user
//! binding is the action reference alone.
//!
//! Key sequences use the angle grammar (`<C-p> n`); `<leader>` resolves
//! against this file's own `leader` node when present, the built-in leader
//! otherwise, wherever in the file the node sits. No chord-depth cap applies
//! at parse time: an overlong sequence is a liveness question, and conflict
//! detection reports it against the effective depth.
//!
//! Validation is all-or-nothing per file: every problem is collected as a
//! span-tagged [`KeybindingDiagnostic`] and a file with any problem yields
//! no layer. A half-applied keymap (some bindings live, the mistyped ones
//! silently dropped) would be worse than a clean error and the running map.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;

use kdl::{KdlDocument, KdlNode};
use koshi_core::action::ActionRef;
use koshi_core::key::KeySequence;
use koshi_core::resolve::ActionArgs;
use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

use crate::error::{check_version, ConfigParseDiagnostic};
use crate::key::{parse_chord, parse_leader, Leader};
use crate::key_sequence::parse_sequence;
use crate::layer::PartialKeybindingsConfig;
use crate::parser::parse_kdl;
use crate::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};

#[cfg(test)]
mod tests;

/// A keybinding file that could not be used.
#[derive(Debug, Error, Diagnostic)]
pub enum KeybindingParseError {
    /// The file is not valid KDL syntax.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Syntax(#[from] ConfigParseDiagnostic),
    /// The file is valid KDL but violates the keybinding schema. Carries
    /// every problem found, so one read of the report fixes the whole file.
    #[error("invalid keybinding file {path}")]
    #[diagnostic(code(koshi::config::keybinding))]
    Invalid {
        /// Path of the keybinding file, for the header line.
        path: String,
        /// Every schema violation, each pointing at its own span.
        #[related]
        diagnostics: Vec<KeybindingDiagnostic>,
    },
}

/// One schema violation in a keybinding file, rendered with a caret at the
/// offending node.
#[derive(Debug, Error, Diagnostic)]
#[error("{message}")]
#[diagnostic(code(koshi::config::keybinding))]
pub struct KeybindingDiagnostic {
    /// What is wrong, in plain words.
    message: String,
    /// The keybinding file text, named by its path.
    #[source_code]
    src: NamedSource<String>,
    /// Where in the file the problem sits.
    #[label]
    span: SourceSpan,
}

impl KeybindingDiagnostic {
    /// The plain-words description of the violation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Where in the file the problem sits, as the caret label's span.
    #[must_use]
    pub fn span(&self) -> SourceSpan {
        self.span
    }
}

/// Parses `source` — the already-read contents of the keybinding file at
/// `path` — into a [`PartialKeybindingsConfig`]. Does no file I/O: discovery
/// and reading happen in the caller.
///
/// # Errors
/// [`KeybindingParseError::Syntax`] when the text is not valid KDL;
/// [`KeybindingParseError::Invalid`] with every schema violation otherwise.
pub fn parse_keybindings(
    path: &Path,
    source: &str,
) -> Result<PartialKeybindingsConfig, KeybindingParseError> {
    let doc = parse_kdl(path, source)?;
    let mut walker = Walker {
        path,
        source,
        diagnostics: Vec::new(),
    };
    let partial = walker.document(&doc);
    if walker.diagnostics.is_empty() {
        Ok(partial)
    } else {
        Err(KeybindingParseError::Invalid {
            path: path.display().to_string(),
            diagnostics: walker.diagnostics,
        })
    }
}

/// Walks the parsed document, collecting the partial layer and every schema
/// violation.
struct Walker<'a> {
    /// Path of the file, naming diagnostic source code.
    path: &'a Path,
    /// The file text, embedded in each diagnostic for caret rendering.
    source: &'a str,
    /// Every schema violation found so far.
    diagnostics: Vec<KeybindingDiagnostic>,
}

impl Walker<'_> {
    /// Records one schema violation at `span`.
    fn error(&mut self, span: SourceSpan, message: impl Into<String>) {
        self.diagnostics.push(KeybindingDiagnostic {
            message: message.into(),
            src: NamedSource::new(self.path.display().to_string(), self.source.to_string()),
            span,
        });
    }

    /// Parses the whole document: a first pass reads the top-level setting
    /// nodes (so `leader` applies to every `bind` regardless of node order),
    /// a second pass parses the `mode` blocks with the resolved leader.
    fn document(&mut self, doc: &KdlDocument) -> PartialKeybindingsConfig {
        let mut partial = PartialKeybindingsConfig::default();
        let mut seen: BTreeSet<&str> = BTreeSet::new();

        for node in doc.nodes() {
            let name = node.name().value();
            match name {
                "version" | "chord-timeout-ms" | "which-key-delay-ms" | "max-chord-depth"
                | "leader" | "unlock-alternative" => {
                    if !seen.insert(name) {
                        self.error(node.span(), format!("duplicate `{name}` node"));
                        continue;
                    }
                    self.setting(node, &mut partial);
                }
                "mode" => {} // second pass
                other => {
                    self.error(
                        node.span(),
                        format!(
                            "unknown node `{other}`; expected a setting \
                             (`chord-timeout-ms`, `which-key-delay-ms`, \
                             `max-chord-depth`, `leader`, `unlock-alternative`, \
                             `version`) or a `mode` block"
                        ),
                    );
                }
            }
        }

        // `<leader>` in a bind resolves against this file's own leader when
        // set, the built-in leader otherwise.
        let leader = partial
            .leader
            .unwrap_or_else(|| KeybindingsConfig::default().leader);

        let mut modes: BTreeMap<ModeName, ModeBindings> = BTreeMap::new();
        let mut seen_modes: BTreeSet<String> = BTreeSet::new();
        for node in doc.nodes() {
            if node.name().value() == "mode" {
                self.mode(node, &leader, &mut modes, &mut seen_modes);
            }
        }
        if !modes.is_empty() {
            partial.modes = Some(modes);
        }
        partial
    }

    /// Parses one top-level setting node into its partial field.
    fn setting(&mut self, node: &KdlNode, partial: &mut PartialKeybindingsConfig) {
        match node.name().value() {
            "version" => {
                if let Some(found) = self.integer_arg(node, u64::from(u32::MAX)) {
                    // The bound above keeps the value in u32 range.
                    let found = u32::try_from(found).expect("bounded by integer_arg");
                    if let Err(err) = check_version(found) {
                        self.error(node.span(), err.to_string());
                    }
                }
            }
            "chord-timeout-ms" => {
                if let Some(v) = self.integer_arg(node, u64::from(u32::MAX)) {
                    partial.chord_timeout_ms = Some(u32::try_from(v).expect("bounded"));
                }
            }
            "which-key-delay-ms" => {
                if let Some(v) = self.integer_arg(node, u64::from(u32::MAX)) {
                    partial.which_key_delay_ms = Some(u32::try_from(v).expect("bounded"));
                }
            }
            "max-chord-depth" => {
                if let Some(v) = self.integer_arg(node, u64::from(u8::MAX)) {
                    partial.max_chord_depth = Some(u8::try_from(v).expect("bounded"));
                }
            }
            "leader" => {
                if node.children().is_some() {
                    self.error(node.span(), "`leader` takes no children");
                    return;
                }
                if let Some((value, span)) = self.string_arg(node) {
                    match parse_leader(value) {
                        Ok(leader) => partial.leader = Some(leader),
                        Err(err) => self.error(span, err.to_string()),
                    }
                }
            }
            "unlock-alternative" => {
                if node.children().is_some() {
                    self.error(node.span(), "`unlock-alternative` takes no children");
                    return;
                }
                if let Some((value, span)) = self.string_arg(node) {
                    match parse_chord(value) {
                        Ok(chord) => partial.unlock_alternative = Some(Some(chord)),
                        Err(err) => self.error(span, err.to_string()),
                    }
                }
            }
            _ => unreachable!("callers dispatch only setting names"),
        }
    }

    /// Parses one `mode "name" { bind/remove ... }` block into `modes`.
    fn mode(
        &mut self,
        node: &KdlNode,
        leader: &Leader,
        modes: &mut BTreeMap<ModeName, ModeBindings>,
        seen_modes: &mut BTreeSet<String>,
    ) {
        let Some((name, _)) = self.string_arg(node) else {
            return;
        };
        if !seen_modes.insert(name.to_string()) {
            self.error(
                node.span(),
                format!("duplicate `mode \"{name}\"` block; one block per mode"),
            );
            return;
        }

        let mut keys: BTreeMap<KeySequence, BoundAction> = BTreeMap::new();
        let mut removed: BTreeSet<KeySequence> = BTreeSet::new();
        if let Some(children) = node.children() {
            for child in children.nodes() {
                match child.name().value() {
                    "bind" => self.bind(child, leader, &mut keys),
                    "remove" => self.remove(child, leader, &mut removed),
                    other => {
                        self.error(
                            child.span(),
                            format!(
                                "unknown node `{other}` in `mode`; expected `bind` or `remove`"
                            ),
                        );
                    }
                }
            }
        }
        modes.insert(ModeName::new(name), ModeBindings { keys, removed });
    }

    /// Parses one `bind "<seq>" "<action>"` node into `keys`.
    fn bind(
        &mut self,
        node: &KdlNode,
        leader: &Leader,
        keys: &mut BTreeMap<KeySequence, BoundAction>,
    ) {
        if node.children().is_some() {
            self.error(node.span(), "`bind` takes no children");
            return;
        }
        let [key_entry, action_entry] = node.entries() else {
            self.error(
                node.span(),
                "`bind` takes exactly two string arguments: a key sequence and an action reference",
            );
            return;
        };
        if key_entry.name().is_some() || action_entry.name().is_some() {
            self.error(
                node.span(),
                "`bind` takes exactly two string arguments: a key sequence and an action reference",
            );
            return;
        }
        let (Some(key_str), Some(action_str)) = (
            key_entry.value().as_string(),
            action_entry.value().as_string(),
        ) else {
            self.error(node.span(), "`bind` arguments must be strings");
            return;
        };

        // No chord-depth cap at parse time — an overlong sequence stays a
        // conflict-detection warning against the effective depth.
        let sequence = match parse_sequence(key_str, *leader, u8::MAX) {
            Ok(sequence) => sequence,
            Err(err) => {
                self.error(key_entry.span(), err.to_string());
                return;
            }
        };
        let action = match ActionRef::from_str(action_str) {
            Ok(action) => action,
            Err(err) => {
                self.error(
                    action_entry.span(),
                    format!("{err}; write the full reference, like `core:new-tab`"),
                );
                return;
            }
        };
        if keys.contains_key(&sequence) {
            self.error(
                node.span(),
                format!("`{key_str}` is already bound in this mode; one action per key"),
            );
            return;
        }
        keys.insert(
            sequence,
            BoundAction {
                action,
                args: ActionArgs::None,
            },
        );
    }

    /// Parses one `remove "<seq>"` node into `removed`.
    fn remove(&mut self, node: &KdlNode, leader: &Leader, removed: &mut BTreeSet<KeySequence>) {
        if node.children().is_some() {
            self.error(node.span(), "`remove` takes no children");
            return;
        }
        let Some((key_str, span)) = self.string_arg(node) else {
            return;
        };
        let sequence = match parse_sequence(key_str, *leader, u8::MAX) {
            Ok(sequence) => sequence,
            Err(err) => {
                self.error(span, err.to_string());
                return;
            }
        };
        if !removed.insert(sequence) {
            self.error(node.span(), format!("duplicate `remove \"{key_str}\"`"));
        }
    }

    /// Reads a node's single unnamed non-negative integer argument, at most
    /// `max`. Reports and returns `None` on any other shape.
    fn integer_arg(&mut self, node: &KdlNode, max: u64) -> Option<u64> {
        if node.children().is_some() {
            self.error(
                node.span(),
                format!("`{}` takes no children", node.name().value()),
            );
            return None;
        }
        let [entry] = node.entries() else {
            self.error(
                node.span(),
                format!(
                    "`{}` takes exactly one integer argument",
                    node.name().value()
                ),
            );
            return None;
        };
        if entry.name().is_some() {
            self.error(
                node.span(),
                format!(
                    "`{}` takes exactly one integer argument",
                    node.name().value()
                ),
            );
            return None;
        }
        let value = entry
            .value()
            .as_integer()
            .and_then(|v| u64::try_from(v).ok());
        match value {
            Some(v) if v <= max => Some(v),
            _ => {
                self.error(
                    entry.span(),
                    format!(
                        "`{}` must be an integer from 0 to {max}",
                        node.name().value()
                    ),
                );
                None
            }
        }
    }

    /// Reads a node's single unnamed string argument and its span. Reports
    /// and returns `None` on any other shape. Children are left to the
    /// caller — `mode` carries a block, the scalar settings must not.
    fn string_arg<'n>(&mut self, node: &'n KdlNode) -> Option<(&'n str, SourceSpan)> {
        let [entry] = node.entries() else {
            self.error(
                node.span(),
                format!(
                    "`{}` takes exactly one string argument",
                    node.name().value()
                ),
            );
            return None;
        };
        if entry.name().is_some() {
            self.error(
                node.span(),
                format!(
                    "`{}` takes exactly one string argument",
                    node.name().value()
                ),
            );
            return None;
        }
        match entry.value().as_string() {
            Some(value) => Some((value, entry.span())),
            None => {
                self.error(
                    entry.span(),
                    format!("`{}` argument must be a string", node.name().value()),
                );
                None
            }
        }
    }
}
