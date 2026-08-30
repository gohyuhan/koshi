//! Keybinding file parsing: KDL text describing the keybindings section into
//! a [`PartialKeybindingsConfig`].
//!
//! The keybinding file is the whole keybindings section, one file. Top-level
//! setting nodes (`chord-timeout-ms 500`, `which-key-delay-ms 400`,
//! `max-chord-depth 4`, `leader "<C-p>"`, `unlock-alternative "<A-u>"`, a
//! required `version 1`) sit beside `mode "name"` blocks holding the
//! bindings: `bind "<C-y>" "core:new-tab"` maps a key sequence to a full
//! action reference, and `remove "<Tab>"` clears the key in that mode, voiding
//! whatever a lower layer bound on it. Named keys need their brackets — a bare
//! `Tab` is one chord per character, the three-chord sequence `T` `a` `b`. A
//! `bind` takes the action reference alone and carries no arguments: an
//! action choice with a fixed set of values is part of the action name, as in
//! `bind "<A-n>" "core:new-pane-left"`.
//!
//! Key sequences use the angle grammar (`<C-p> n`); `<leader>` resolves
//! against this file's own `leader` node when present, the built-in leader
//! otherwise, wherever in the file the node sits. The file's
//! `max-chord-depth` does not apply here: a sequence parses at any depth up
//! to 255 chords, and conflict detection reports an overlong one against the
//! effective depth.
//!
//! Validation is all-or-nothing per file: every problem is collected as a
//! span-tagged [`KeybindingDiagnostic`], and a file with any problem yields no
//! layer at all. The running map stays as it was.

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
use crate::parser::unknown_key;
use crate::types::{BoundAction, ModeBindings, ModeName};

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
    /// every problem found in the file.
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
/// offending node or argument.
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
    /// The file text, embedded in each diagnostic as its source code.
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

    /// Parses the whole document in two passes: the first reads the top-level
    /// setting nodes, the second parses the `mode` blocks against the leader
    /// the first pass resolved. A `leader` node applies to every `bind`
    /// wherever in the file the node sits.
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
                        unknown_key(
                            other,
                            &[
                                "version",
                                "chord-timeout-ms",
                                "which-key-delay-ms",
                                "max-chord-depth",
                                "leader",
                                "unlock-alternative",
                                "mode",
                            ],
                        ),
                    );
                }
            }
        }
        if !seen.contains("version") {
            self.error(doc.span(), "keybinding file must declare `version`");
        }

        // `<leader>` in a bind resolves against this file's own leader when
        // set, the built-in leader otherwise.
        let leader = partial.leader.unwrap_or_default();

        let mut modes: BTreeMap<ModeName, ModeBindings> = BTreeMap::new();
        for node in doc.nodes() {
            if node.name().value() == "mode" {
                self.mode(node, &leader, &mut modes);
            }
        }
        if !modes.is_empty() {
            partial.modes = Some(modes);
        }
        partial
    }

    /// Parses one top-level setting node into its partial field. `version`
    /// writes no field: it only checks the declared number against the
    /// supported schema version.
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
    /// Reports a duplicate `mode` block when `modes` already holds the name,
    /// keeping the first block's bindings.
    fn mode(
        &mut self,
        node: &KdlNode,
        leader: &Leader,
        modes: &mut BTreeMap<ModeName, ModeBindings>,
    ) {
        let Some((name, _)) = self.string_arg(node) else {
            return;
        };
        let mode_name = ModeName::new(name);
        if modes.contains_key(&mode_name) {
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
                        self.error(child.span(), unknown_key(other, &["bind", "remove"]));
                    }
                }
            }
        }
        modes.insert(mode_name, ModeBindings { keys, removed });
    }

    /// Parses one `bind "<seq>" "<action>"` node into `keys`, with
    /// [`ActionArgs::None`] as the arguments. Reports a violation when the
    /// parsed sequence is already a key of `keys`, keeping the first binding.
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
        let (key_entry, action_entry) = match node.entries() {
            [key, action] if key.name().is_none() && action.name().is_none() => (key, action),
            _ => {
                self.error(
                    node.span(),
                    "`bind` takes exactly two string arguments: a key sequence and an action \
                     reference",
                );
                return;
            }
        };
        let (Some(key_str), Some(action_str)) = (
            key_entry.value().as_string(),
            action_entry.value().as_string(),
        ) else {
            self.error(node.span(), "`bind` arguments must be strings");
            return;
        };

        // The widest cap: only a sequence past 255 chords is refused here.
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

    /// Parses one `remove "<seq>"` node into `removed`. Reports a violation
    /// when the parsed sequence is already in `removed`.
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
    /// `max`. Reports and returns `None` on any other shape, a child block
    /// included.
    fn integer_arg(&mut self, node: &KdlNode, max: u64) -> Option<u64> {
        if node.children().is_some() {
            self.error(
                node.span(),
                format!("`{}` takes no children", node.name().value()),
            );
            return None;
        }
        let entry = match node.entries() {
            [entry] if entry.name().is_none() => entry,
            _ => {
                self.error(
                    node.span(),
                    format!(
                        "`{}` takes exactly one integer argument",
                        node.name().value()
                    ),
                );
                return None;
            }
        };
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
    /// and returns `None` on any other shape. Does not look at children: a
    /// `mode` node carries a block, and each scalar setting rejects children
    /// in its own arm.
    fn string_arg<'n>(&mut self, node: &'n KdlNode) -> Option<(&'n str, SourceSpan)> {
        let entry = match node.entries() {
            [entry] if entry.name().is_none() => entry,
            _ => {
                self.error(
                    node.span(),
                    format!(
                        "`{}` takes exactly one string argument",
                        node.name().value()
                    ),
                );
                return None;
            }
        };
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
