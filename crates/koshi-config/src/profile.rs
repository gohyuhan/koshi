//! Profile file parsing: KDL text describing tabs, splits, and panes into a
//! [`ProfileTemplate`].
//!
//! A profile file is structural nodes holding config nodes. Structural
//! vocabulary: `tab`, `horizontal` (children side by side, left to right),
//! `vertical` (children top to bottom), `stack` (children share one
//! rectangle, one expanded), `pane` (terminal), `plugin "name"` (plugin
//! pane). Every setting is a child node — no properties: `pane { command
//! "nvim" "file"; cwd "~/proj"; env "K" "V"; size "60%"; focus }`.
//! Sizing (`size` cells or `"N%"`, `weight`, `min`, `preferred`) is valid only
//! on children of `horizontal`/`vertical`; `expanded` marks a stack's one
//! expanded member; `focus` marks the starting pane (one per tab) and, as a
//! direct `tab` child, the starting tab. A bare top-level `lock` node starts
//! the session's first client in locked input mode.
//!
//! Validation is all-or-nothing per file: every problem is collected as a
//! span-tagged [`ProfileDiagnostic`] and a file with any problem yields no
//! template.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlNode};
use koshi_core::geometry::SplitDirection;
use koshi_layout::size::{SizeConstraint, SizeWeight};
use koshi_layout::template::{
    CommandTemplate, LeafTemplate, PluginTemplate, ProfileTemplate, TabTemplate, TemplateNode,
    TemplateSplit, TerminalTemplate,
};
use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

use crate::error::{validate_config_schema_version, ConfigParseDiagnostic};
use crate::parser::{format_unknown_key, parse_kdl, parse_version_argument};

#[cfg(test)]
mod tests;

/// A profile file that could not be used.
#[derive(Debug, Error, Diagnostic)]
pub enum ProfileError {
    /// The file is not valid KDL syntax.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Syntax(#[from] ConfigParseDiagnostic),
    /// The file is valid KDL but violates the profile schema. Carries every
    /// problem found.
    #[error("invalid profile file {profile_path}")]
    #[diagnostic(code(koshi::config::profile))]
    Invalid {
        /// Path of the profile file, for the header line.
        profile_path: String,
        /// Every schema violation, each pointing at its own span.
        #[related]
        diagnostics: Vec<ProfileDiagnostic>,
    },
}

/// One schema violation in a profile file, rendered with a caret at the
/// offending node.
#[derive(Debug, Error, Diagnostic)]
#[error("{diagnostic_message}")]
#[diagnostic(code(koshi::config::profile))]
pub struct ProfileDiagnostic {
    /// What is wrong, in plain words.
    diagnostic_message: String,
    /// The profile file text, named by its path.
    #[source_code]
    profile_source: NamedSource<String>,
    /// Where in the file the problem sits.
    #[label]
    span: SourceSpan,
}

impl ProfileDiagnostic {
    /// The plain-words description of the violation.
    #[must_use]
    pub fn get_diagnostic_message(&self) -> &str {
        &self.diagnostic_message
    }

    /// Where in the file the problem sits, as the caret label's span.
    #[must_use]
    pub fn get_source_span(&self) -> SourceSpan {
        self.span
    }
}

/// Parses `profile_source_text`, the already-read contents of the profile
/// file at `profile_path`, into a [`ProfileTemplate`]. Does no file I/O:
/// discovery and reading happen in the caller.
///
/// # Errors
/// [`ProfileError::Syntax`] when the text is not valid KDL;
/// [`ProfileError::Invalid`] with every schema violation otherwise.
pub fn parse_profile(
    profile_path: &Path,
    profile_source_text: &str,
) -> Result<ProfileTemplate, ProfileError> {
    let profile_document = parse_kdl(profile_path, profile_source_text)?;
    let mut profile_walker = ProfileDocumentWalker {
        profile_path,
        profile_source_text,
        profile_diagnostics: Vec::new(),
        tab_leaf_count: 0,
        focused_tab_leaf_spans: Vec::new(),
    };
    let profile_template = profile_walker.parse_document(&profile_document);
    match profile_template {
        Some(profile_template) if profile_walker.profile_diagnostics.is_empty() => {
            Ok(profile_template)
        }
        _ => Err(ProfileError::Invalid {
            profile_path: profile_path.display().to_string(),
            diagnostics: profile_walker.profile_diagnostics,
        }),
    }
}

/// Where a structural node sits, deciding which config its children may
/// carry: sizing only under a directional split, `expanded` only in a stack.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProfileNodeContext {
    /// The single root slot of a `tab`.
    TabRoot,
    /// A child slot of `horizontal` or `vertical`.
    Directional,
    /// A member slot of `stack`.
    Stack,
}

/// Sizing config collected from one structural node's children, becoming
/// that node's [`SizeWeight`] in its parent split.
#[derive(Default)]
struct ProfileSizing {
    /// `size` or `weight` value, with the node span that set it.
    primary_constraint: Option<(SizeConstraint, SourceSpan)>,
    /// `min` overlay in cells, with its span.
    minimum_cell_count: Option<(u16, SourceSpan)>,
    /// `preferred` overlay in cells, with its span.
    preferred_cell_count: Option<(u16, SourceSpan)>,
}

impl ProfileSizing {
    /// The weight this sizing describes. An untouched sizing yields
    /// `SizeConstraint::Flex(1)` with no `min`, no `preferred`, and a zero
    /// resize offset.
    fn build_size_weight(&self) -> SizeWeight {
        SizeWeight {
            primary_constraint: self
                .primary_constraint
                .map_or(SizeConstraint::Flex(1), |(constraint, _)| constraint),
            minimum_cell_count: self.minimum_cell_count.map(|(cell_count, _)| cell_count),
            preferred_cell_count: self.preferred_cell_count.map(|(cell_count, _)| cell_count),
            resize_delta: 0,
        }
    }

    /// The span of the first sizing node present, for "sizing not allowed
    /// here" reports. `None` when no sizing was given.
    fn find_first_sizing_span(&self) -> Option<SourceSpan> {
        self.primary_constraint
            .map(|(_, span)| span)
            .or(self.minimum_cell_count.map(|(_, span)| span))
            .or(self.preferred_cell_count.map(|(_, span)| span))
    }
}

/// Everything a parsed structural node hands back to its parent: the
/// subtree, its sizing, and whether a leaf marked itself `expanded`.
struct ProfileSlot {
    /// The parsed subtree.
    template_node: TemplateNode,
    /// Sizing config for this node's slot in a directional parent.
    slot_sizing: ProfileSizing,
    /// Span of an `expanded` marker, set only by leaves inside a stack.
    expanded_span: Option<SourceSpan>,
}

/// Recursive-descent walker over a profile document. Collects every
/// diagnostic instead of stopping at the first; every method that gives up
/// on a value (`None`, or a placeholder slot) records at least one
/// diagnostic explaining it first.
struct ProfileDocumentWalker<'a> {
    /// Profile file path, stamped onto every diagnostic.
    profile_path: &'a Path,
    /// Profile file text, stamped onto every diagnostic for span rendering.
    profile_source_text: &'a str,
    /// Every schema violation found so far.
    profile_diagnostics: Vec<ProfileDiagnostic>,
    /// Leaves assigned so far in the current tab, in layout order; the next
    /// leaf parsed gets this index.
    tab_leaf_count: usize,
    /// `focus`-marked leaves of the current tab: `(leaf index, span)`.
    focused_tab_leaf_spans: Vec<(usize, SourceSpan)>,
}

impl ProfileDocumentWalker<'_> {
    /// Records a schema violation at `diagnostic_span`.
    fn record_diagnostic(
        &mut self,
        diagnostic_span: SourceSpan,
        diagnostic_message: impl Into<String>,
    ) {
        self.profile_diagnostics.push(ProfileDiagnostic {
            diagnostic_message: diagnostic_message.into(),
            profile_source: NamedSource::new(
                self.profile_path.display().to_string(),
                self.profile_source_text.to_string(),
            ),
            span: diagnostic_span,
        });
    }

    /// Parses the whole document: a `version` node, one or more `tab` nodes,
    /// and an optional bare `lock` marker. Returns `None` when the file has no
    /// usable tab list.
    fn parse_document(&mut self, profile_document: &KdlDocument) -> Option<ProfileTemplate> {
        let mut has_version_node = false;
        let mut tab_templates = Vec::new();
        let mut focused_tab_index: Option<usize> = None;
        let mut starts_locked = false;
        let mut has_lock_node = false;
        for node in profile_document.nodes() {
            match node.name().value() {
                "version" => {
                    if has_version_node {
                        self.record_diagnostic(node.span(), "`version` is declared more than once");
                    } else {
                        has_version_node = true;
                        self.parse_version_node(node);
                    }
                }
                "tab" => {
                    let tab_index = tab_templates.len();
                    let (tab_template, is_tab_focused) = self.parse_tab_node(node);
                    tab_templates.push(tab_template);
                    if is_tab_focused {
                        match focused_tab_index {
                            None => focused_tab_index = Some(tab_index),
                            Some(_) => self.record_diagnostic(
                                node.span(),
                                "another tab already carries `focus`; only one tab starts focused",
                            ),
                        }
                    }
                }
                "lock" => {
                    let is_bare_marker = self.validate_marker(node, "lock");
                    if has_lock_node {
                        self.record_diagnostic(node.span(), "`lock` is declared more than once");
                    } else {
                        has_lock_node = true;
                        starts_locked = is_bare_marker;
                    }
                }
                other_node_name => {
                    self.record_diagnostic(
                        node.span(),
                        format_unknown_key(other_node_name, &["version", "tab", "lock"]),
                    );
                }
            }
        }
        if !has_version_node {
            self.record_diagnostic(
                profile_document.span(),
                "profile file must declare `version`",
            );
        }
        if tab_templates.is_empty() {
            self.record_diagnostic(
                profile_document.span(),
                "profile file must define at least one `tab`",
            );
            return None;
        }
        Some(ProfileTemplate {
            tabs: tab_templates,
            focused_tab_index: focused_tab_index.unwrap_or(0),
            is_locked: starts_locked,
        })
    }

    /// Validates the `version` node: one integer at least one, nothing else,
    /// no newer than this build's schema.
    fn parse_version_node(&mut self, version_node: &KdlNode) {
        match parse_version_argument(version_node) {
            Ok(schema_version) => {
                if let Err(version_error) = validate_config_schema_version(schema_version) {
                    self.record_diagnostic(version_node.span(), version_error.to_string());
                }
            }
            Err((diagnostic_span, diagnostic_message)) => {
                self.record_diagnostic(diagnostic_span, diagnostic_message);
            }
        }
    }

    /// Parses one `tab`: exactly one structural root plus an optional bare
    /// `focus` marker. Returns the tab and whether it starts focused. A tab
    /// with problems is still returned, over a placeholder shell root when
    /// no structural node parsed. With no leaf marked `focus`, the tab
    /// focuses its root's [`TemplateNode::find_first_visible_leaf_index`].
    fn parse_tab_node(&mut self, tab_node: &KdlNode) -> (TabTemplate, bool) {
        if !tab_node.entries().is_empty() {
            self.record_diagnostic(
                tab_node.span(),
                "`tab` takes no arguments or properties; its layout goes in the children block",
            );
        }
        self.tab_leaf_count = 0;
        self.focused_tab_leaf_spans = Vec::new();
        let mut root_slot: Option<ProfileSlot> = None;
        let mut is_tab_focused = false;
        if let Some(tab_children) = tab_node.children() {
            for child_node in tab_children.nodes() {
                match child_node.name().value() {
                    "focus" => {
                        if self.validate_marker(child_node, "focus") {
                            if is_tab_focused {
                                self.record_diagnostic(
                                    child_node.span(),
                                    "`focus` is declared more than once",
                                );
                            } else {
                                is_tab_focused = true;
                            }
                        }
                    }
                    node_name if is_structural_node_name(node_name) => {
                        let profile_slot =
                            self.parse_structural_node(child_node, ProfileNodeContext::TabRoot);
                        match root_slot {
                            None => root_slot = Some(profile_slot),
                            Some(_) => self.record_diagnostic(
                                child_node.span(),
                                "`tab` holds one root node; wrap multiple panes in \
                                 `horizontal`, `vertical`, or `stack`",
                            ),
                        }
                    }
                    other_node_name => self.record_diagnostic(
                        child_node.span(),
                        format_unknown_key(
                            &format!("tab.{other_node_name}"),
                            &[
                                "tab.focus",
                                "tab.pane",
                                "tab.plugin",
                                "tab.horizontal",
                                "tab.vertical",
                                "tab.stack",
                            ],
                        ),
                    ),
                }
            }
        }
        let root_slot = match root_slot {
            Some(profile_slot) => profile_slot,
            None => {
                self.record_diagnostic(
                    tab_node.span(),
                    "`tab` needs one layout node (`pane`, `plugin`, `horizontal`, \
                     `vertical`, or `stack`)",
                );
                ProfileSlot {
                    template_node: TemplateNode::Leaf(LeafTemplate::Terminal(
                        TerminalTemplate::default(),
                    )),
                    slot_sizing: ProfileSizing::default(),
                    expanded_span: None,
                }
            }
        };
        let extra_focus_spans: Vec<SourceSpan> = self
            .focused_tab_leaf_spans
            .iter()
            .skip(1)
            .map(|&(_, focus_span)| focus_span)
            .collect();
        for focus_span in extra_focus_spans {
            self.record_diagnostic(focus_span, "this tab already focuses another pane");
        }
        let focused_leaf_index = self.focused_tab_leaf_spans.first().map_or_else(
            || root_slot.template_node.find_first_visible_leaf_index(),
            |&(leaf_index, _)| leaf_index,
        );
        (
            TabTemplate {
                root: root_slot.template_node,
                focused_leaf_index,
            },
            is_tab_focused,
        )
    }

    /// Dispatches one structural node by name. `structural_node`'s name must
    /// pass [`is_structural_node_name`]; any other name panics. Always yields
    /// a slot: a node with problems is diagnosed and returned in degraded
    /// form, never dropped.
    fn parse_structural_node(
        &mut self,
        structural_node: &KdlNode,
        parent_context: ProfileNodeContext,
    ) -> ProfileSlot {
        match structural_node.name().value() {
            "pane" => self.parse_pane_node(structural_node, parent_context),
            "plugin" => self.parse_plugin_node(structural_node, parent_context),
            "horizontal" => {
                self.parse_split_node(structural_node, parent_context, SplitDirection::Horizontal)
            }
            "vertical" => {
                self.parse_split_node(structural_node, parent_context, SplitDirection::Vertical)
            }
            "stack" => self.parse_stack_node(structural_node, parent_context),
            other_node_name => {
                unreachable!("caller checked is_structural_node_name({other_node_name:?})")
            }
        }
    }

    /// Parses a `pane` leaf: optional `command`, `cwd`, repeated `env`,
    /// sizing, `focus`, and (in a stack) `expanded`, all as children.
    fn parse_pane_node(
        &mut self,
        pane_node: &KdlNode,
        parent_context: ProfileNodeContext,
    ) -> ProfileSlot {
        if !pane_node.entries().is_empty() {
            self.record_diagnostic(
                pane_node.span(),
                "`pane` takes no arguments or properties; its configuration goes in the \
                 children block",
            );
        }
        let mut command_template: Option<CommandTemplate> = None;
        let mut working_directory: Option<PathBuf> = None;
        let mut environment_variables: BTreeMap<String, String> = BTreeMap::new();
        let mut leaf_config = ProfileLeafConfig::default();
        if let Some(pane_children) = pane_node.children() {
            for child_node in pane_children.nodes() {
                match child_node.name().value() {
                    "command" => {
                        if command_template.is_some() {
                            self.record_diagnostic(
                                child_node.span(),
                                "`command` is declared more than once",
                            );
                        } else if let Some(parsed_command_template) =
                            self.parse_command_node(child_node)
                        {
                            command_template = Some(parsed_command_template);
                        }
                    }
                    "cwd" => {
                        if working_directory.is_some() {
                            self.record_diagnostic(
                                child_node.span(),
                                "`cwd` is declared more than once",
                            );
                        } else if let Some(working_directory_text) =
                            self.parse_single_string(child_node, "cwd")
                        {
                            working_directory = Some(PathBuf::from(working_directory_text));
                        }
                    }
                    "env" => self.parse_environment_node(child_node, &mut environment_variables),
                    _ => {
                        if !self.parse_leaf_config_node(
                            child_node,
                            parent_context,
                            &mut leaf_config,
                        ) {
                            let setting_key = format!("pane.{}", child_node.name().value());
                            self.record_diagnostic(
                                child_node.span(),
                                format_unknown_key(
                                    &setting_key,
                                    &[
                                        "pane.command",
                                        "pane.cwd",
                                        "pane.env",
                                        "pane.size",
                                        "pane.weight",
                                        "pane.min",
                                        "pane.preferred",
                                        "pane.focus",
                                        "pane.expanded",
                                    ],
                                ),
                            );
                        }
                    }
                }
            }
        }
        self.record_leaf(&leaf_config);
        ProfileSlot {
            template_node: TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate {
                command: command_template,
                working_directory,
                environment_variables,
            })),
            slot_sizing: leaf_config.leaf_sizing,
            expanded_span: leaf_config.expanded_span,
        }
    }

    /// Parses a `plugin "name"` leaf: the name as its one argument, plus
    /// optional sizing, `focus`, and (in a stack) `expanded` children. A name
    /// that cannot be read is reported and becomes an empty string.
    fn parse_plugin_node(
        &mut self,
        plugin_node: &KdlNode,
        parent_context: ProfileNodeContext,
    ) -> ProfileSlot {
        let plugin_name = match plugin_node.entries() {
            [plugin_argument] if plugin_argument.name().is_none() => {
                match plugin_argument.value().as_string() {
                    Some(plugin_name) if !plugin_name.is_empty() => Some(plugin_name.to_string()),
                    _ => {
                        self.record_diagnostic(
                            plugin_argument.span(),
                            "`plugin` takes one non-empty name string",
                        );
                        None
                    }
                }
            }
            _ => {
                self.record_diagnostic(
                    plugin_node.span(),
                    "`plugin` takes exactly one name string, like `plugin \"session-manager\"`",
                );
                None
            }
        };
        let mut leaf_config = ProfileLeafConfig::default();
        if let Some(plugin_children) = plugin_node.children() {
            for child_node in plugin_children.nodes() {
                if !self.parse_leaf_config_node(child_node, parent_context, &mut leaf_config) {
                    let setting_key = format!("plugin.{}", child_node.name().value());
                    self.record_diagnostic(
                        child_node.span(),
                        format_unknown_key(
                            &setting_key,
                            &[
                                "plugin.size",
                                "plugin.weight",
                                "plugin.min",
                                "plugin.preferred",
                                "plugin.focus",
                                "plugin.expanded",
                            ],
                        ),
                    );
                }
            }
        }
        self.record_leaf(&leaf_config);
        let plugin_name = plugin_name.unwrap_or_default();
        ProfileSlot {
            template_node: TemplateNode::Leaf(LeafTemplate::Plugin(PluginTemplate { plugin_name })),
            slot_sizing: leaf_config.leaf_sizing,
            expanded_span: leaf_config.expanded_span,
        }
    }

    /// Parses `horizontal` or `vertical`: its own sizing children plus at
    /// least two structural children.
    fn parse_split_node(
        &mut self,
        split_node: &KdlNode,
        parent_context: ProfileNodeContext,
        direction: SplitDirection,
    ) -> ProfileSlot {
        let split_name = split_node.name().value();
        if !split_node.entries().is_empty() {
            self.record_diagnostic(
                split_node.span(),
                format!("`{split_name}` takes no arguments or properties"),
            );
        }
        let mut split_sizing = ProfileSizing::default();
        let mut child_slots: Vec<ProfileSlot> = Vec::new();
        if let Some(split_children) = split_node.children() {
            for child_node in split_children.nodes() {
                let child_node_name = child_node.name().value();
                if is_structural_node_name(child_node_name) {
                    child_slots.push(
                        self.parse_structural_node(child_node, ProfileNodeContext::Directional),
                    );
                } else if !self.parse_sizing_node(child_node, &mut split_sizing) {
                    let setting_key = format!("{split_name}.{child_node_name}");
                    self.record_diagnostic(
                        child_node.span(),
                        format_unknown_key(
                            &setting_key,
                            &[
                                &format!("{split_name}.pane"),
                                &format!("{split_name}.plugin"),
                                &format!("{split_name}.horizontal"),
                                &format!("{split_name}.vertical"),
                                &format!("{split_name}.stack"),
                                &format!("{split_name}.size"),
                                &format!("{split_name}.weight"),
                                &format!("{split_name}.min"),
                                &format!("{split_name}.preferred"),
                            ],
                        ),
                    );
                }
            }
        }
        self.validate_sizing_context(&split_sizing, parent_context);
        if child_slots.len() < 2 {
            self.record_diagnostic(
                split_node.span(),
                format!("`{split_name}` needs at least two children to divide space between"),
            );
        }
        let child_weights = child_slots
            .iter()
            .map(|profile_slot| profile_slot.slot_sizing.build_size_weight())
            .collect();
        let template_children = child_slots
            .into_iter()
            .map(|profile_slot| profile_slot.template_node)
            .collect();
        ProfileSlot {
            template_node: TemplateNode::Split(TemplateSplit {
                direction,
                children: template_children,
                weights: child_weights,
                active_child_index: 0,
            }),
            slot_sizing: split_sizing,
            expanded_span: None,
        }
    }

    /// Parses `stack`: its own sizing children plus at least two leaf
    /// members (`pane`/`plugin`), at most one marked `expanded`.
    fn parse_stack_node(
        &mut self,
        stack_node: &KdlNode,
        parent_context: ProfileNodeContext,
    ) -> ProfileSlot {
        if !stack_node.entries().is_empty() {
            self.record_diagnostic(
                stack_node.span(),
                "`stack` takes no arguments or properties",
            );
        }
        let mut stack_sizing = ProfileSizing::default();
        let mut stack_members: Vec<ProfileSlot> = Vec::new();
        // One entry per member: the leaf index of a leaf member, or `None`
        // for an invalid-subtree placeholder.
        let mut member_leaf_indices: Vec<Option<usize>> = Vec::new();
        if let Some(stack_children) = stack_node.children() {
            for child_node in stack_children.nodes() {
                let child_node_name = child_node.name().value();
                if child_node_name == "pane" || child_node_name == "plugin" {
                    let leaf_index = self.tab_leaf_count;
                    let profile_slot =
                        self.parse_structural_node(child_node, ProfileNodeContext::Stack);
                    stack_members.push(profile_slot);
                    member_leaf_indices.push(Some(leaf_index));
                } else if is_structural_node_name(child_node_name) {
                    self.record_diagnostic(
                        child_node.span(),
                        format!(
                            "`{child_node_name}` cannot be a stack member; stack members are \
                             `pane` or `plugin`"
                        ),
                    );
                    // Parsed as a directional child: sizing on it and on its
                    // own children is accepted.
                    stack_members.push(
                        self.parse_structural_node(child_node, ProfileNodeContext::Directional),
                    );
                    member_leaf_indices.push(None);
                } else if !self.parse_sizing_node(child_node, &mut stack_sizing) {
                    self.record_diagnostic(
                        child_node.span(),
                        format_unknown_key(
                            &format!("stack.{child_node_name}"),
                            &[
                                "stack.pane",
                                "stack.plugin",
                                "stack.size",
                                "stack.weight",
                                "stack.min",
                                "stack.preferred",
                            ],
                        ),
                    );
                }
            }
        }
        self.validate_sizing_context(&stack_sizing, parent_context);
        if stack_members.len() < 2 {
            self.record_diagnostic(stack_node.span(), "`stack` needs at least two members");
        }
        let mut expanded_member_index: Option<usize> = None;
        for (member_index, profile_slot) in stack_members.iter().enumerate() {
            if let Some(expanded_span) = profile_slot.expanded_span {
                match expanded_member_index {
                    None => expanded_member_index = Some(member_index),
                    Some(_) => self.record_diagnostic(
                        expanded_span,
                        "another member is already `expanded`; a stack expands exactly one",
                    ),
                }
            }
        }
        let expanded_member_index = expanded_member_index.unwrap_or(0);
        let collapsed_focus_spans: Vec<SourceSpan> = member_leaf_indices
            .iter()
            .enumerate()
            .filter(|&(member_index, _)| member_index != expanded_member_index)
            .filter_map(|(_, &leaf_index)| leaf_index)
            .filter_map(|leaf_index| {
                self.focused_tab_leaf_spans
                    .iter()
                    .find(|&&(focus_leaf_index, _)| focus_leaf_index == leaf_index)
                    .map(|&(_, focus_span)| focus_span)
            })
            .collect();
        for focus_span in collapsed_focus_spans {
            self.record_diagnostic(
                focus_span,
                "a collapsed stack member cannot hold focus; mark it `expanded`",
            );
        }
        let stack_weights = vec![SizeWeight::default(); stack_members.len()];
        let template_children = stack_members
            .into_iter()
            .map(|profile_slot| profile_slot.template_node)
            .collect();
        ProfileSlot {
            template_node: TemplateNode::Split(TemplateSplit {
                direction: SplitDirection::Stacked,
                children: template_children,
                weights: stack_weights,
                active_child_index: expanded_member_index,
            }),
            slot_sizing: stack_sizing,
            expanded_span: None,
        }
    }

    /// Handles a config child shared by both leaf kinds: sizing, `focus`,
    /// `expanded`. Sizing outside a `Directional` slot is reported and
    /// discarded. Returns `false` when the node is none of the three.
    fn parse_leaf_config_node(
        &mut self,
        config_node: &KdlNode,
        parent_context: ProfileNodeContext,
        leaf_config: &mut ProfileLeafConfig,
    ) -> bool {
        match config_node.name().value() {
            "focus" => {
                if self.validate_marker(config_node, "focus") {
                    match leaf_config.focus_span {
                        None => leaf_config.focus_span = Some(config_node.span()),
                        Some(_) => self.record_diagnostic(
                            config_node.span(),
                            "`focus` is declared more than once",
                        ),
                    }
                }
                true
            }
            "expanded" => {
                if self.validate_marker(config_node, "expanded") {
                    if parent_context != ProfileNodeContext::Stack {
                        self.record_diagnostic(
                            config_node.span(),
                            "`expanded` applies only to members of a `stack`",
                        );
                    } else {
                        match leaf_config.expanded_span {
                            None => leaf_config.expanded_span = Some(config_node.span()),
                            Some(_) => self.record_diagnostic(
                                config_node.span(),
                                "`expanded` is declared more than once",
                            ),
                        }
                    }
                }
                true
            }
            _ => {
                if self.parse_sizing_node(config_node, &mut leaf_config.leaf_sizing) {
                    if parent_context != ProfileNodeContext::Directional {
                        self.validate_sizing_context(&leaf_config.leaf_sizing, parent_context);
                        leaf_config.leaf_sizing = ProfileSizing::default();
                    }
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Assigns the next leaf index of the current tab and records its
    /// `focus` marker, keeping leaf numbering aligned with
    /// [`TemplateNode::list_leaf_templates`] layout order.
    fn record_leaf(&mut self, leaf_config: &ProfileLeafConfig) {
        let leaf_index = self.tab_leaf_count;
        self.tab_leaf_count += 1;
        if let Some(focus_span) = leaf_config.focus_span {
            self.focused_tab_leaf_spans.push((leaf_index, focus_span));
        }
    }

    /// Reports sizing given where none is meaningful — anywhere but a child
    /// slot of `horizontal`/`vertical`.
    fn validate_sizing_context(
        &mut self,
        sizing: &ProfileSizing,
        parent_context: ProfileNodeContext,
    ) {
        if parent_context == ProfileNodeContext::Directional {
            return;
        }
        if let Some(sizing_span) = sizing.find_first_sizing_span() {
            self.record_diagnostic(
                sizing_span,
                "sizing applies only to children of `horizontal` or `vertical`",
            );
        }
    }

    /// Handles one sizing node (`size`, `weight`, `min`, `preferred`) into
    /// `sizing`. Returns `false` when the node is not a sizing node.
    fn parse_sizing_node(&mut self, sizing_node: &KdlNode, sizing: &mut ProfileSizing) -> bool {
        match sizing_node.name().value() {
            "size" => {
                if sizing.primary_constraint.is_some() {
                    self.record_diagnostic(
                        sizing_node.span(),
                        "this node already has `size` or `weight`; give one of the two, once",
                    );
                } else if let Some(size_constraint) = self.parse_size_constraint(sizing_node) {
                    sizing.primary_constraint = Some((size_constraint, sizing_node.span()));
                }
                true
            }
            "weight" => {
                if sizing.primary_constraint.is_some() {
                    self.record_diagnostic(
                        sizing_node.span(),
                        "this node already has `size` or `weight`; give one of the two, once",
                    );
                } else if let Some(weight_value) =
                    self.parse_cell_count_in_range(sizing_node, "weight", u32::MAX)
                {
                    match SizeConstraint::from_flex_weight(weight_value) {
                        Ok(size_constraint) => {
                            sizing.primary_constraint = Some((size_constraint, sizing_node.span()));
                        }
                        Err(size_error) => {
                            self.record_diagnostic(sizing_node.span(), size_error.to_string());
                        }
                    }
                }
                true
            }
            "min" => {
                if sizing.minimum_cell_count.is_some() {
                    self.record_diagnostic(sizing_node.span(), "`min` is declared more than once");
                } else if let Some(minimum_cell_count) =
                    self.parse_positive_cell_count(sizing_node, "min")
                {
                    sizing.minimum_cell_count = Some((minimum_cell_count, sizing_node.span()));
                }
                true
            }
            "preferred" => {
                if sizing.preferred_cell_count.is_some() {
                    self.record_diagnostic(
                        sizing_node.span(),
                        "`preferred` is declared more than once",
                    );
                } else if let Some(preferred_cell_count) =
                    self.parse_positive_cell_count(sizing_node, "preferred")
                {
                    sizing.preferred_cell_count = Some((preferred_cell_count, sizing_node.span()));
                }
                true
            }
            _ => false,
        }
    }

    /// Parses a `size` value: an integer argument is exact cells, a string
    /// like `"60%"` is a percentage of the parent's axis.
    fn parse_size_constraint(&mut self, size_node: &KdlNode) -> Option<SizeConstraint> {
        let size_argument = self.find_single_argument(size_node, "size")?;
        if let Some(cell_count_value) = size_argument.value().as_integer() {
            let Ok(cell_count) = u16::try_from(cell_count_value) else {
                self.record_diagnostic(
                    size_argument.span(),
                    format!("`size` cells must fit 1-{}", u16::MAX),
                );
                return None;
            };
            return match SizeConstraint::from_fixed_cell_count(cell_count) {
                Ok(size_constraint) => Some(size_constraint),
                Err(size_error) => {
                    self.record_diagnostic(size_argument.span(), size_error.to_string());
                    None
                }
            };
        }
        if let Some(percentage_text) = size_argument.value().as_string() {
            let Some(percentage) = percentage_text
                .strip_suffix('%')
                .and_then(|digits| digits.parse::<u8>().ok())
            else {
                self.record_diagnostic(
                    size_argument.span(),
                    "`size` is a cell count like `size 30` or a percentage like `size \"60%\"`",
                );
                return None;
            };
            return match SizeConstraint::from_percent(percentage) {
                Ok(size_constraint) => Some(size_constraint),
                Err(size_error) => {
                    self.record_diagnostic(size_argument.span(), size_error.to_string());
                    None
                }
            };
        }
        self.record_diagnostic(
            size_argument.span(),
            "`size` is a cell count like `size 30` or a percentage like `size \"60%\"`",
        );
        None
    }

    /// Parses a `min`/`preferred` value: one positive cell count.
    fn parse_positive_cell_count(
        &mut self,
        sizing_node: &KdlNode,
        sizing_name: &str,
    ) -> Option<u16> {
        let cell_count =
            self.parse_cell_count_in_range(sizing_node, sizing_name, u32::from(u16::MAX))?;
        let cell_count = u16::try_from(cell_count).expect("bounded by u16::MAX above");
        if cell_count == 0 {
            self.record_diagnostic(
                sizing_node.span(),
                format!("`{sizing_name}` must be at least one cell"),
            );
            return None;
        }
        Some(cell_count)
    }

    /// Parses one integer argument in `0..=max_cell_count` — the shared
    /// shape of `weight`, `min`, and `preferred` values. A non-integer, a
    /// negative value, or a value above the limit is reported and returns
    /// `None`. Zero passes; each caller rejects it with its own message.
    fn parse_cell_count_in_range(
        &mut self,
        sizing_node: &KdlNode,
        sizing_name: &str,
        max_cell_count: u32,
    ) -> Option<u32> {
        let sizing_argument = self.find_single_argument(sizing_node, sizing_name)?;
        match sizing_argument
            .value()
            .as_integer()
            .and_then(|integer_value| u32::try_from(integer_value).ok())
        {
            Some(cell_count) if cell_count <= max_cell_count => Some(cell_count),
            _ => {
                self.record_diagnostic(
                    sizing_argument.span(),
                    format!("`{sizing_name}` must be an integer between 1 and {max_cell_count}"),
                );
                None
            }
        }
    }

    /// Parses a `command` node: the program plus its arguments, all strings.
    /// The program must be non-empty; arguments may be empty strings. No word
    /// may hold a NUL character.
    fn parse_command_node(&mut self, command_node: &KdlNode) -> Option<CommandTemplate> {
        if command_node.children().is_some() {
            self.record_diagnostic(command_node.span(), "`command` takes no children");
            return None;
        }
        let mut command_words = Vec::with_capacity(command_node.entries().len());
        for command_argument in command_node.entries() {
            if command_argument.name().is_some() {
                self.record_diagnostic(
                    command_argument.span(),
                    "`command` takes arguments, not properties",
                );
                return None;
            }
            let Some(command_word) = command_argument.value().as_string() else {
                self.record_diagnostic(
                    command_argument.span(),
                    "`command` arguments must be strings",
                );
                return None;
            };
            if command_word.contains('\0') {
                self.record_diagnostic(
                    command_argument.span(),
                    "`command` program and arguments must not contain a NUL character",
                );
                return None;
            }
            command_words.push(command_word.to_string());
        }
        if command_words.is_empty() {
            self.record_diagnostic(
                command_node.span(),
                "`command` names a program, like `command \"nvim\" \"file.txt\"`",
            );
            return None;
        }
        if command_words[0].is_empty() {
            self.record_diagnostic(command_node.span(), "`command` program must not be empty");
            return None;
        }
        let program_path = PathBuf::from(command_words.remove(0));
        Some(CommandTemplate {
            program: program_path,
            arguments: command_words,
        })
    }

    /// Parses an `env "NAME" "value"` node into `environment_variables`.
    /// The name must be non-empty, hold no `=`, hold no NUL, and be set once;
    /// the value must hold no NUL. Names compare case-insensitively over ASCII:
    /// `env "Path" "/b"` after `env "PATH" "/a"` is a duplicate.
    fn parse_environment_node(
        &mut self,
        environment_node: &KdlNode,
        environment_variables: &mut BTreeMap<String, String>,
    ) {
        if environment_node.children().is_some() {
            self.record_diagnostic(environment_node.span(), "`env` takes no children");
            return;
        }
        let environment_values: Vec<&str> = environment_node
            .entries()
            .iter()
            .filter(|environment_entry| environment_entry.name().is_none())
            .filter_map(|environment_entry| environment_entry.value().as_string())
            .collect();
        let ([environment_variable_name, environment_variable_value], true) = (
            environment_values.as_slice(),
            environment_values.len() == environment_node.entries().len(),
        ) else {
            self.record_diagnostic(
                environment_node.span(),
                "`env` takes a name and a value, both strings, like `env \"RUST_LOG\" \"debug\"`",
            );
            return;
        };
        if environment_variable_name.is_empty() {
            self.record_diagnostic(environment_node.span(), "`env` name must not be empty");
            return;
        }
        if environment_variable_name.contains('=') {
            self.record_diagnostic(environment_node.span(), "`env` name must not contain `=`");
            return;
        }
        if environment_variable_name.contains('\0') || environment_variable_value.contains('\0') {
            self.record_diagnostic(
                environment_node.span(),
                "`env` name and value must not contain a NUL character",
            );
            return;
        }
        if let Some(existing_environment_name) =
            environment_variables.keys().find(|environment_name| {
                environment_name.eq_ignore_ascii_case(environment_variable_name)
            })
        {
            let diagnostic_message = if existing_environment_name == environment_variable_name {
                format!("`env` sets `{environment_variable_name}` more than once")
            } else {
                format!(
                    "`env` already sets `{existing_environment_name}`; env names match \
                     case-insensitively (Windows folds environment keys by case)"
                )
            };
            self.record_diagnostic(environment_node.span(), diagnostic_message);
            return;
        }
        environment_variables.insert(
            (*environment_variable_name).to_string(),
            (*environment_variable_value).to_string(),
        );
    }

    /// Parses a single-string-argument node (`cwd`). An empty string, and a
    /// string holding a NUL character, are each reported and yield `None`.
    fn parse_single_string(
        &mut self,
        setting_node: &KdlNode,
        setting_name: &str,
    ) -> Option<String> {
        let string_argument = self.find_single_argument(setting_node, setting_name)?;
        let string_value = match string_argument.value().as_string() {
            Some(string_value) if !string_value.is_empty() => string_value,
            _ => {
                self.record_diagnostic(
                    string_argument.span(),
                    format!("`{setting_name}` takes one non-empty string"),
                );
                return None;
            }
        };
        if string_value.contains('\0') {
            self.record_diagnostic(
                string_argument.span(),
                format!("`{setting_name}` must not contain a NUL character"),
            );
            return None;
        }
        Some(string_value.to_string())
    }

    /// Validates a node down to exactly one positional argument and no
    /// children, returning that argument's entry.
    fn find_single_argument<'k>(
        &mut self,
        setting_node: &'k KdlNode,
        setting_name: &str,
    ) -> Option<&'k kdl::KdlEntry> {
        if setting_node.children().is_some() {
            self.record_diagnostic(
                setting_node.span(),
                format!("`{setting_name}` takes no children"),
            );
            return None;
        }
        match setting_node.entries() {
            [argument_entry] if argument_entry.name().is_none() => Some(argument_entry),
            _ => {
                self.record_diagnostic(
                    setting_node.span(),
                    format!("`{setting_name}` takes exactly one value"),
                );
                None
            }
        }
    }

    /// Validates a bare marker node (`focus`, `expanded`): no arguments,
    /// no properties, no children. Returns whether the marker is usable.
    fn validate_marker(&mut self, marker_node: &KdlNode, marker_name: &str) -> bool {
        if marker_node.entries().is_empty() && marker_node.children().is_none() {
            true
        } else {
            self.record_diagnostic(
                marker_node.span(),
                format!("`{marker_name}` is a bare marker and takes no values or children"),
            );
            false
        }
    }
}

/// Focus/expanded/sizing markers collected while parsing one leaf.
#[derive(Default)]
struct ProfileLeafConfig {
    /// Sizing config from the leaf's children.
    leaf_sizing: ProfileSizing,
    /// Span of a `focus` marker, if any.
    focus_span: Option<SourceSpan>,
    /// Span of an `expanded` marker, if any.
    expanded_span: Option<SourceSpan>,
}

/// Whether `node_name` is a structural layout node, as opposed to a config
/// node.
fn is_structural_node_name(node_name: &str) -> bool {
    matches!(
        node_name,
        "pane" | "plugin" | "horizontal" | "vertical" | "stack"
    )
}
