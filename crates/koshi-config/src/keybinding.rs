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
use koshi_core::action::ActionReference;
use koshi_core::key::KeySequence;
use koshi_core::resolve::ActionArgs;
use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

use crate::error::{validate_config_schema_version, ConfigParseDiagnostic};
use crate::key::{parse_chord, parse_leader, Leader};
use crate::key_sequence::parse_sequence;
use crate::layer::PartialKeybindingsConfig;
use crate::parser::format_unknown_key;
use crate::parser::parse_kdl;
use crate::parser::parse_version_argument;
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
    #[error("invalid keybinding file {keybinding_path}")]
    #[diagnostic(code(koshi::config::keybinding))]
    Invalid {
        /// Path of the keybinding file, for the header line.
        keybinding_path: String,
        /// Every schema violation, each pointing at its own span.
        #[related]
        diagnostics: Vec<KeybindingDiagnostic>,
    },
}

/// One schema violation in a keybinding file, rendered with a caret at the
/// offending node or argument.
#[derive(Debug, Error, Diagnostic)]
#[error("{diagnostic_message}")]
#[diagnostic(code(koshi::config::keybinding))]
pub struct KeybindingDiagnostic {
    /// What is wrong, in plain words.
    diagnostic_message: String,
    /// The keybinding file text, named by its path.
    #[source_code]
    keybinding_source: NamedSource<String>,
    /// Where in the file the problem sits.
    #[label]
    diagnostic_span: SourceSpan,
}

impl KeybindingDiagnostic {
    /// The plain-words description of the violation.
    #[must_use]
    pub fn get_diagnostic_message(&self) -> &str {
        &self.diagnostic_message
    }

    /// Where in the file the problem sits, as the caret label's span.
    #[must_use]
    pub fn get_source_span(&self) -> SourceSpan {
        self.diagnostic_span
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
    keybinding_path: &Path,
    keybinding_source_text: &str,
) -> Result<PartialKeybindingsConfig, KeybindingParseError> {
    let keybinding_document = parse_kdl(keybinding_path, keybinding_source_text)?;
    let mut keybinding_document_walker = KeybindingDocumentWalker {
        keybinding_path,
        keybinding_source_text,
        diagnostics: Vec::new(),
    };
    let partial_keybindings_config =
        keybinding_document_walker.parse_document(&keybinding_document);
    if keybinding_document_walker.diagnostics.is_empty() {
        Ok(partial_keybindings_config)
    } else {
        Err(KeybindingParseError::Invalid {
            keybinding_path: keybinding_path.display().to_string(),
            diagnostics: keybinding_document_walker.diagnostics,
        })
    }
}

/// Walks the parsed document, collecting the partial layer and every schema
/// violation.
struct KeybindingDocumentWalker<'a> {
    /// Path of the file, naming diagnostic source code.
    keybinding_path: &'a Path,
    /// The file text, embedded in each diagnostic as its source code.
    keybinding_source_text: &'a str,
    /// Every schema violation found so far.
    diagnostics: Vec<KeybindingDiagnostic>,
}

impl KeybindingDocumentWalker<'_> {
    /// Records one schema violation at `span`.
    fn record_diagnostic(
        &mut self,
        diagnostic_span: SourceSpan,
        diagnostic_message: impl Into<String>,
    ) {
        self.diagnostics.push(KeybindingDiagnostic {
            diagnostic_message: diagnostic_message.into(),
            keybinding_source: NamedSource::new(
                self.keybinding_path.display().to_string(),
                self.keybinding_source_text.to_string(),
            ),
            diagnostic_span,
        });
    }

    /// Parses the whole document in two passes: the first reads the top-level
    /// setting nodes, the second parses the `mode` blocks against the leader
    /// the first pass resolved. A `leader` node applies to every `bind`
    /// wherever in the file the node sits.
    fn parse_document(&mut self, document: &KdlDocument) -> PartialKeybindingsConfig {
        let mut partial_keybindings_config = PartialKeybindingsConfig::default();
        let mut seen_setting_names: BTreeSet<&str> = BTreeSet::new();

        for node in document.nodes() {
            let node_name = node.name().value();
            match node_name {
                "version" | "chord-timeout-ms" | "which-key-delay-ms" | "max-chord-depth"
                | "leader" | "unlock-alternative" => {
                    if !seen_setting_names.insert(node_name) {
                        self.record_diagnostic(
                            node.span(),
                            format!("`{node_name}` is declared more than once"),
                        );
                        continue;
                    }
                    self.parse_setting_node(node, &mut partial_keybindings_config);
                }
                "mode" => {} // second pass
                unknown_node_name => {
                    self.record_diagnostic(
                        node.span(),
                        format_unknown_key(
                            unknown_node_name,
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
        if !seen_setting_names.contains("version") {
            self.record_diagnostic(document.span(), "keybinding file must declare `version`");
        }

        // `<leader>` in a bind resolves against this file's own leader when
        // set, the built-in leader otherwise.
        let leader = partial_keybindings_config.leader.unwrap_or_default();

        let mut mode_bindings_by_name: BTreeMap<ModeName, ModeBindings> = BTreeMap::new();
        for node in document.nodes() {
            if node.name().value() == "mode" {
                self.parse_mode_block(node, &leader, &mut mode_bindings_by_name);
            }
        }
        if !mode_bindings_by_name.is_empty() {
            partial_keybindings_config.mode_bindings_by_name = Some(mode_bindings_by_name);
        }
        partial_keybindings_config
    }

    /// Parses one top-level setting node into its partial field. `version`
    /// writes no field: it only checks the declared number against the
    /// supported schema version.
    fn parse_setting_node(
        &mut self,
        node: &KdlNode,
        partial_keybindings_config: &mut PartialKeybindingsConfig,
    ) {
        match node.name().value() {
            "version" => match parse_version_argument(node) {
                Ok(schema_version) => {
                    if let Err(version_error) = validate_config_schema_version(schema_version) {
                        self.record_diagnostic(node.span(), version_error.to_string());
                    }
                }
                Err((diagnostic_span, detail)) => self.record_diagnostic(diagnostic_span, detail),
            },
            "chord-timeout-ms" => {
                if let Some(integer_value) = self.parse_integer_argument(node, u64::from(u32::MAX))
                {
                    partial_keybindings_config.chord_timeout_ms =
                        Some(u32::try_from(integer_value).expect("bounded"));
                }
            }
            "which-key-delay-ms" => {
                if let Some(integer_value) = self.parse_integer_argument(node, u64::from(u32::MAX))
                {
                    partial_keybindings_config.which_key_delay_ms =
                        Some(u32::try_from(integer_value).expect("bounded"));
                }
            }
            "max-chord-depth" => {
                if let Some(integer_value) = self.parse_integer_argument(node, u64::from(u8::MAX)) {
                    partial_keybindings_config.max_chord_depth =
                        Some(u8::try_from(integer_value).expect("bounded"));
                }
            }
            "leader" => {
                if node.children().is_some() {
                    self.record_diagnostic(node.span(), "`leader` takes no children");
                    return;
                }
                if let Some((leader_text, diagnostic_span)) = self.parse_string_argument(node) {
                    match parse_leader(leader_text) {
                        Ok(leader) => partial_keybindings_config.leader = Some(leader),
                        Err(parse_error) => {
                            self.record_diagnostic(diagnostic_span, parse_error.to_string())
                        }
                    }
                }
            }
            "unlock-alternative" => {
                if node.children().is_some() {
                    self.record_diagnostic(node.span(), "`unlock-alternative` takes no children");
                    return;
                }
                if let Some((chord_text, diagnostic_span)) = self.parse_string_argument(node) {
                    match parse_chord(chord_text) {
                        Ok(chord) => {
                            partial_keybindings_config.unlock_alternative = Some(Some(chord))
                        }
                        Err(parse_error) => {
                            self.record_diagnostic(diagnostic_span, parse_error.to_string())
                        }
                    }
                }
            }
            _ => unreachable!("callers dispatch only setting names"),
        }
    }

    /// Parses one `mode "name" { bind/remove ... }` block into
    /// `mode_bindings_by_name`. Reports a duplicate `mode` block when
    /// `mode_bindings_by_name` already holds the name,
    /// keeping the first block's bindings.
    fn parse_mode_block(
        &mut self,
        node: &KdlNode,
        leader: &Leader,
        mode_bindings_by_name: &mut BTreeMap<ModeName, ModeBindings>,
    ) {
        let Some((mode_name_text, _)) = self.parse_string_argument(node) else {
            return;
        };
        let mode_name = ModeName::from_text(mode_name_text);
        if mode_bindings_by_name.contains_key(&mode_name) {
            self.record_diagnostic(
                node.span(),
                format!("duplicate `mode \"{mode_name_text}\"` block; one block per mode"),
            );
            return;
        }

        let mut bound_action_by_key_sequence: BTreeMap<KeySequence, BoundAction> = BTreeMap::new();
        let mut removed_key_sequences: BTreeSet<KeySequence> = BTreeSet::new();
        if let Some(children) = node.children() {
            for child in children.nodes() {
                match child.name().value() {
                    "bind" => {
                        self.parse_binding_node(child, leader, &mut bound_action_by_key_sequence)
                    }
                    "remove" => self.parse_removal_node(child, leader, &mut removed_key_sequences),
                    unknown_mode_child_name => {
                        self.record_diagnostic(
                            child.span(),
                            format_unknown_key(unknown_mode_child_name, &["bind", "remove"]),
                        );
                    }
                }
            }
        }
        mode_bindings_by_name.insert(
            mode_name,
            ModeBindings {
                bound_action_by_key_sequence,
                removed_key_sequences,
            },
        );
    }

    /// Parses one `bind "<seq>" "<action>"` node into `keys`, with
    /// [`ActionArgs::None`] as the arguments. Reports a violation when the
    /// parsed sequence is already a key of `keys`, keeping the first binding.
    fn parse_binding_node(
        &mut self,
        node: &KdlNode,
        leader: &Leader,
        bound_action_by_key_sequence: &mut BTreeMap<KeySequence, BoundAction>,
    ) {
        if node.children().is_some() {
            self.record_diagnostic(node.span(), "`bind` takes no children");
            return;
        }
        let (key_argument, action_argument) = match node.entries() {
            [key_entry, action_entry]
                if key_entry.name().is_none() && action_entry.name().is_none() =>
            {
                (key_entry, action_entry)
            }
            _ => {
                self.record_diagnostic(
                    node.span(),
                    "`bind` takes exactly two string arguments: a key sequence and an action \
                     reference",
                );
                return;
            }
        };
        let (Some(key_sequence_text), Some(action_reference_text)) = (
            key_argument.value().as_string(),
            action_argument.value().as_string(),
        ) else {
            self.record_diagnostic(node.span(), "`bind` arguments must be strings");
            return;
        };

        // The widest cap: only a sequence past 255 chords is refused here.
        let key_sequence = match parse_sequence(key_sequence_text, *leader, u8::MAX) {
            Ok(key_sequence) => key_sequence,
            Err(parse_error) => {
                self.record_diagnostic(key_argument.span(), parse_error.to_string());
                return;
            }
        };
        let action_reference = match ActionReference::from_str(action_reference_text) {
            Ok(action_reference) => action_reference,
            Err(parse_error) => {
                self.record_diagnostic(
                    action_argument.span(),
                    format!("{parse_error}; write the full reference, like `core:new-tab`"),
                );
                return;
            }
        };
        if bound_action_by_key_sequence.contains_key(&key_sequence) {
            self.record_diagnostic(
                node.span(),
                format!("`{key_sequence_text}` is already bound in this mode; one action per key"),
            );
            return;
        }
        bound_action_by_key_sequence.insert(
            key_sequence,
            BoundAction {
                action_reference,
                action_arguments: ActionArgs::None,
            },
        );
    }

    /// Parses one `remove "<seq>"` node into `removed`. Reports a violation
    /// when the parsed sequence is already in `removed`.
    fn parse_removal_node(
        &mut self,
        node: &KdlNode,
        leader: &Leader,
        removed_key_sequences: &mut BTreeSet<KeySequence>,
    ) {
        if node.children().is_some() {
            self.record_diagnostic(node.span(), "`remove` takes no children");
            return;
        }
        let Some((key_sequence_text, diagnostic_span)) = self.parse_string_argument(node) else {
            return;
        };
        let key_sequence = match parse_sequence(key_sequence_text, *leader, u8::MAX) {
            Ok(key_sequence) => key_sequence,
            Err(parse_error) => {
                self.record_diagnostic(diagnostic_span, parse_error.to_string());
                return;
            }
        };
        if !removed_key_sequences.insert(key_sequence) {
            self.record_diagnostic(
                node.span(),
                format!("duplicate `remove \"{key_sequence_text}\"`"),
            );
        }
    }

    /// Reads a node's single unnamed non-negative integer argument, at most
    /// `max`. Reports and returns `None` on any other shape, a child block
    /// included.
    fn parse_integer_argument(&mut self, node: &KdlNode, max_integer_value: u64) -> Option<u64> {
        if node.children().is_some() {
            self.record_diagnostic(
                node.span(),
                format!("`{}` takes no children", node.name().value()),
            );
            return None;
        }
        let argument_entry = match node.entries() {
            [argument_entry] if argument_entry.name().is_none() => argument_entry,
            _ => {
                self.record_diagnostic(
                    node.span(),
                    format!(
                        "`{}` takes exactly one integer argument",
                        node.name().value()
                    ),
                );
                return None;
            }
        };
        let integer_value = argument_entry
            .value()
            .as_integer()
            .and_then(|integer_value| u64::try_from(integer_value).ok());
        match integer_value {
            Some(integer_value) if integer_value <= max_integer_value => Some(integer_value),
            _ => {
                self.record_diagnostic(
                    argument_entry.span(),
                    format!(
                        "`{}` must be an integer from 0 to {max_integer_value}",
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
    fn parse_string_argument<'node>(
        &mut self,
        node: &'node KdlNode,
    ) -> Option<(&'node str, SourceSpan)> {
        let argument_entry = match node.entries() {
            [argument_entry] if argument_entry.name().is_none() => argument_entry,
            _ => {
                self.record_diagnostic(
                    node.span(),
                    format!(
                        "`{}` takes exactly one string argument",
                        node.name().value()
                    ),
                );
                return None;
            }
        };
        match argument_entry.value().as_string() {
            Some(string_value) => Some((string_value, argument_entry.span())),
            None => {
                self.record_diagnostic(
                    argument_entry.span(),
                    format!("`{}` argument must be a string", node.name().value()),
                );
                None
            }
        }
    }
}
