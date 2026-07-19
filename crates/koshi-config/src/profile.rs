//! Profile file parsing: KDL text describing tabs, splits, and panes into a
//! [`ProfileTemplate`].
//!
//! A profile file is structural nodes holding config nodes. Structural
//! vocabulary: `tab`, `horizontal` (children side by side, left to right),
//! `vertical` (children top to bottom), `stack` (children share one
//! rectangle, one expanded), `pane` (terminal), `plugin "name"` (plugin
//! pane). Every setting is a child node — no properties: `pane { command
//! "nvim" "file"; cwd "~/proj"; env "K" "V"; size "60%"; focus }`. Sizing
//! (`size` cells or `"N%"`, `weight`, `min`, `preferred`) is valid only on
//! children of `horizontal`/`vertical`; `expanded` marks a stack's one
//! expanded member; `focus` marks the starting pane (one per tab) and, as a
//! direct `tab` child, the starting tab.
//!
//! Validation is all-or-nothing per file: every problem is collected as a
//! span-tagged [`ProfileDiagnostic`] and a file with any problem yields no
//! template. A profile instantiates real processes, so a half-applied file
//! (the editor pane silently dropped, its neighbors spawned anyway) would
//! be worse than a clean error and fallback.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlNode};
use koshi_core::geometry::SplitDirection;
use koshi_layout::size::{SizeConstraint, SizeWeight};
use koshi_layout::template::{
    CommandTemplate, LeafTemplate, PluginTemplate, ProfileTemplate, TabTemplate, TemplateChild,
    TemplateNode, TemplateSplit, TerminalTemplate,
};
use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

use crate::error::{check_version, ConfigParseDiagnostic};
use crate::parser::parse_kdl;

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
    /// problem found, so one read of the report fixes the whole file.
    #[error("invalid profile file {path}")]
    #[diagnostic(code(koshi::config::profile))]
    Invalid {
        /// Path of the profile file, for the header line.
        path: String,
        /// Every schema violation, each pointing at its own span.
        #[related]
        diagnostics: Vec<ProfileDiagnostic>,
    },
}

/// One schema violation in a profile file, rendered with a caret at the
/// offending node.
#[derive(Debug, Error, Diagnostic)]
#[error("{message}")]
#[diagnostic(code(koshi::config::profile))]
pub struct ProfileDiagnostic {
    /// What is wrong, in plain words.
    message: String,
    /// The profile file text, named by its path.
    #[source_code]
    src: NamedSource<String>,
    /// Where in the file the problem sits.
    #[label]
    span: SourceSpan,
}

impl ProfileDiagnostic {
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

/// Parses `source` — the already-read contents of the profile file at `path`
/// — into a [`ProfileTemplate`]. Does no file I/O: discovery and reading
/// happen in the caller.
///
/// # Errors
/// [`ProfileError::Syntax`] when the text is not valid KDL;
/// [`ProfileError::Invalid`] with every schema violation otherwise.
pub fn parse_profile(path: &Path, source: &str) -> Result<ProfileTemplate, ProfileError> {
    let doc = parse_kdl(path, source)?;
    let mut walker = Walker {
        path,
        source,
        diagnostics: Vec::new(),
        tab_leaves: 0,
        tab_focus: Vec::new(),
    };
    let template = walker.document(&doc);
    match template {
        Some(template) if walker.diagnostics.is_empty() => Ok(template),
        _ => Err(ProfileError::Invalid {
            path: path.display().to_string(),
            diagnostics: walker.diagnostics,
        }),
    }
}

/// Where a structural node sits, deciding which config its children may
/// carry: sizing only under a directional split, `expanded` only in a stack.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Context {
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
struct Sizing {
    /// `size` or `weight` value, with the node span that set it.
    primary: Option<(SizeConstraint, SourceSpan)>,
    /// `min` overlay in cells, with its span.
    min: Option<(u16, SourceSpan)>,
    /// `preferred` overlay in cells, with its span.
    preferred: Option<(u16, SourceSpan)>,
}

impl Sizing {
    /// The weight this sizing describes; an untouched sizing is the default
    /// equal share, matching what runtime pane insertion assigns.
    fn weight(&self) -> SizeWeight {
        SizeWeight {
            primary: self
                .primary
                .map_or(SizeConstraint::Flex(1), |(constraint, _)| constraint),
            min: self.min.map(|(cells, _)| cells),
            preferred: self.preferred.map(|(cells, _)| cells),
            resize_delta: 0,
        }
    }

    /// The span of the first sizing node present, for "sizing not allowed
    /// here" reports. `None` when no sizing was given.
    fn first_span(&self) -> Option<SourceSpan> {
        self.primary
            .map(|(_, span)| span)
            .or(self.min.map(|(_, span)| span))
            .or(self.preferred.map(|(_, span)| span))
    }
}

/// Everything a parsed structural node hands back to its parent: the
/// subtree, its sizing, and whether a leaf marked itself `expanded`.
struct Slot {
    /// The parsed subtree.
    node: TemplateNode,
    /// Sizing config for this node's slot in a directional parent.
    sizing: Sizing,
    /// Span of an `expanded` marker, set only by leaves inside a stack.
    expanded: Option<SourceSpan>,
}

/// Recursive-descent walker over a profile document. Collects every
/// diagnostic instead of stopping at the first; every method that gives up
/// on a value (`None`, or a placeholder slot) records at least one
/// diagnostic explaining it first.
struct Walker<'a> {
    /// Profile file path, stamped onto every diagnostic.
    path: &'a Path,
    /// Profile file text, stamped onto every diagnostic for span rendering.
    source: &'a str,
    /// Every schema violation found so far.
    diagnostics: Vec<ProfileDiagnostic>,
    /// Leaves assigned so far in the current tab, in layout order; the next
    /// leaf parsed gets this index.
    tab_leaves: usize,
    /// `focus`-marked leaves of the current tab: `(leaf index, span)`.
    tab_focus: Vec<(usize, SourceSpan)>,
}

impl Walker<'_> {
    /// Records a schema violation at `span`.
    fn error(&mut self, span: SourceSpan, message: impl Into<String>) {
        self.diagnostics.push(ProfileDiagnostic {
            message: message.into(),
            src: NamedSource::new(self.path.display().to_string(), self.source.to_string()),
            span,
        });
    }

    /// Parses the whole document: a `version` node plus one or more `tab`
    /// nodes. Returns `None` when the file has no usable tab list.
    fn document(&mut self, doc: &KdlDocument) -> Option<ProfileTemplate> {
        let mut version_seen = false;
        let mut tabs = Vec::new();
        let mut focused_tab: Option<(usize, SourceSpan)> = None;
        for node in doc.nodes() {
            match node.name().value() {
                "version" => {
                    if version_seen {
                        self.error(node.span(), "`version` is declared more than once");
                    } else {
                        version_seen = true;
                        self.version(node);
                    }
                }
                "tab" => {
                    let index = tabs.len();
                    let (tab, focused) = self.tab(node);
                    tabs.push(tab);
                    if focused {
                        match focused_tab {
                            None => focused_tab = Some((index, node.span())),
                            Some(_) => self.error(
                                node.span(),
                                "another tab already carries `focus`; only one tab starts focused",
                            ),
                        }
                    }
                }
                other => self.error(
                    node.span(),
                    format!("unknown node `{other}`; a profile holds `version` and `tab` nodes"),
                ),
            }
        }
        if !version_seen {
            self.error(doc.span(), "profile file must declare `version`");
        }
        if tabs.is_empty() {
            self.error(doc.span(), "profile file must define at least one `tab`");
            return None;
        }
        Some(ProfileTemplate {
            tabs,
            focused_tab: focused_tab.map_or(0, |(index, _)| index),
        })
    }

    /// Validates the `version` node: one integer argument, nothing else,
    /// no newer than this build's schema.
    fn version(&mut self, node: &KdlNode) {
        if node.children().is_some() {
            self.error(node.span(), "`version` takes no children");
            return;
        }
        let [entry] = node.entries() else {
            self.error(node.span(), "`version` takes exactly one integer argument");
            return;
        };
        if entry.name().is_some() {
            self.error(node.span(), "`version` takes exactly one integer argument");
            return;
        }
        let Some(found) = entry
            .value()
            .as_integer()
            .and_then(|v| u32::try_from(v).ok())
        else {
            self.error(entry.span(), "`version` must be a non-negative integer");
            return;
        };
        if let Err(err) = check_version(found) {
            self.error(node.span(), err.to_string());
        }
    }

    /// Parses one `tab`: exactly one structural root plus an optional bare
    /// `focus` marker. Returns the tab and whether it starts focused. A tab
    /// with problems is still returned (over a placeholder root when none
    /// parsed), so its own diagnostic is the only one reported.
    fn tab(&mut self, node: &KdlNode) -> (TabTemplate, bool) {
        if !node.entries().is_empty() {
            self.error(
                node.span(),
                "`tab` takes no arguments or properties; its layout goes in the children block",
            );
        }
        self.tab_leaves = 0;
        self.tab_focus = Vec::new();
        let mut root: Option<Slot> = None;
        let mut focused = false;
        let mut focus_span: Option<SourceSpan> = None;
        if let Some(children) = node.children() {
            for child in children.nodes() {
                match child.name().value() {
                    "focus" => {
                        if self.marker(child, "focus") {
                            match focus_span {
                                None => {
                                    focused = true;
                                    focus_span = Some(child.span());
                                }
                                Some(_) => {
                                    self.error(child.span(), "`focus` is declared more than once")
                                }
                            }
                        }
                    }
                    name if is_structural(name) => {
                        let slot = self.structural(child, Context::TabRoot);
                        match root {
                            None => root = Some(slot),
                            Some(_) => self.error(
                                child.span(),
                                "`tab` holds one root node; wrap multiple panes in \
                                 `horizontal`, `vertical`, or `stack`",
                            ),
                        }
                    }
                    other => self.error(
                        child.span(),
                        format!(
                            "unknown node `{other}` in `tab`; expected `focus` or a layout \
                             node (`pane`, `plugin`, `horizontal`, `vertical`, `stack`)"
                        ),
                    ),
                }
            }
        }
        let root = match root {
            Some(slot) => slot,
            None => {
                self.error(
                    node.span(),
                    "`tab` needs one layout node (`pane`, `plugin`, `horizontal`, \
                     `vertical`, or `stack`)",
                );
                // Placeholder so the tab still exists; the diagnostic above
                // already rejects the file.
                Slot {
                    node: TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate::default())),
                    sizing: Sizing::default(),
                    expanded: None,
                }
            }
        };
        let extra_focus: Vec<SourceSpan> = self
            .tab_focus
            .iter()
            .skip(1)
            .map(|&(_, span)| span)
            .collect();
        for span in extra_focus {
            self.error(span, "this tab already focuses another pane");
        }
        // Default focus is the first visible leaf — at a stacked node only
        // the active member is visible, so initial focus always lands on a
        // pane the user can see.
        let focused_leaf = self
            .tab_focus
            .first()
            .map_or_else(|| root.node.first_visible_leaf(), |&(index, _)| index);
        (
            TabTemplate {
                root: root.node,
                focused_leaf,
            },
            focused,
        )
    }

    /// Dispatches one structural node by name. Callers guarantee the name
    /// passed [`is_structural`]. Always yields a slot — a node with problems
    /// is diagnosed and returned in degraded form, never dropped, so parents
    /// report no follow-on errors for it.
    fn structural(&mut self, node: &KdlNode, context: Context) -> Slot {
        match node.name().value() {
            "pane" => self.pane(node, context),
            "plugin" => self.plugin(node, context),
            "horizontal" => self.split(node, context, SplitDirection::Horizontal),
            "vertical" => self.split(node, context, SplitDirection::Vertical),
            "stack" => self.stack(node, context),
            other => unreachable!("caller checked is_structural({other:?})"),
        }
    }

    /// Parses a `pane` leaf: optional `command`, `cwd`, repeated `env`,
    /// sizing, `focus`, and (in a stack) `expanded`, all as children.
    fn pane(&mut self, node: &KdlNode, context: Context) -> Slot {
        if !node.entries().is_empty() {
            self.error(
                node.span(),
                "`pane` takes no arguments or properties; its configuration goes in the \
                 children block",
            );
        }
        let mut command: Option<(CommandTemplate, SourceSpan)> = None;
        let mut cwd: Option<(PathBuf, SourceSpan)> = None;
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        let mut leaf = LeafSlot::default();
        if let Some(children) = node.children() {
            for child in children.nodes() {
                match child.name().value() {
                    "command" => {
                        if command.is_some() {
                            self.error(child.span(), "`command` is declared more than once");
                        } else if let Some(parsed) = self.command(child) {
                            command = Some((parsed, child.span()));
                        }
                    }
                    "cwd" => {
                        if cwd.is_some() {
                            self.error(child.span(), "`cwd` is declared more than once");
                        } else if let Some(parsed) = self.single_string(child, "cwd") {
                            cwd = Some((PathBuf::from(parsed), child.span()));
                        }
                    }
                    "env" => self.env(child, &mut env),
                    _ => {
                        if !self.leaf_config(child, context, &mut leaf) {
                            self.error(
                                child.span(),
                                format!(
                                    "unknown node `{}` in `pane`; expected `command`, `cwd`, \
                                     `env`, `size`, `weight`, `min`, `preferred`, `focus`, or \
                                     `expanded`",
                                    child.name().value()
                                ),
                            );
                        }
                    }
                }
            }
        }
        self.finish_leaf(&leaf);
        Slot {
            node: TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate {
                command: command.map(|(parsed, _)| parsed),
                cwd: cwd.map(|(path, _)| path),
                env,
            })),
            sizing: leaf.sizing,
            expanded: leaf.expanded,
        }
    }

    /// Parses a `plugin "name"` leaf: the name as its one argument, plus
    /// optional sizing, `focus`, and (in a stack) `expanded` children.
    fn plugin(&mut self, node: &KdlNode, context: Context) -> Slot {
        let name = match node.entries() {
            [entry] if entry.name().is_none() => match entry.value().as_string() {
                Some(name) if !name.is_empty() => Some(name.to_string()),
                _ => {
                    self.error(entry.span(), "`plugin` takes one non-empty name string");
                    None
                }
            },
            _ => {
                self.error(
                    node.span(),
                    "`plugin` takes exactly one name string, like `plugin \"session-manager\"`",
                );
                None
            }
        };
        let mut leaf = LeafSlot::default();
        if let Some(children) = node.children() {
            for child in children.nodes() {
                if !self.leaf_config(child, context, &mut leaf) {
                    self.error(
                        child.span(),
                        format!(
                            "unknown node `{}` in `plugin`; expected `size`, `weight`, `min`, \
                             `preferred`, `focus`, or `expanded`",
                            child.name().value()
                        ),
                    );
                }
            }
        }
        self.finish_leaf(&leaf);
        // A bad name was already diagnosed; the empty placeholder never
        // escapes because the file is rejected.
        let name = name.unwrap_or_default();
        Slot {
            node: TemplateNode::Leaf(LeafTemplate::Plugin(PluginTemplate { name })),
            sizing: leaf.sizing,
            expanded: leaf.expanded,
        }
    }

    /// Parses `horizontal` or `vertical`: its own sizing children plus at
    /// least two structural children.
    fn split(&mut self, node: &KdlNode, context: Context, direction: SplitDirection) -> Slot {
        let name = node.name().value();
        if !node.entries().is_empty() {
            self.error(
                node.span(),
                format!("`{name}` takes no arguments or properties"),
            );
        }
        let mut sizing = Sizing::default();
        let mut slots: Vec<Slot> = Vec::new();
        if let Some(children) = node.children() {
            for child in children.nodes() {
                let child_name = child.name().value();
                if is_structural(child_name) {
                    let slot = self.structural(child, Context::Directional);
                    slots.push(slot);
                } else if self.sizing_config(child, &mut sizing) {
                    // recorded into `sizing`
                } else {
                    self.error(
                        child.span(),
                        format!(
                            "unknown node `{child_name}` in `{name}`; expected a layout node \
                             (`pane`, `plugin`, `horizontal`, `vertical`, `stack`) or sizing \
                             (`size`, `weight`, `min`, `preferred`)"
                        ),
                    );
                }
            }
        }
        self.check_sizing_context(&sizing, context);
        if slots.len() < 2 {
            self.error(
                node.span(),
                format!("`{name}` needs at least two children to divide space between"),
            );
        }
        let weights = slots.iter().map(|slot| slot.sizing.weight()).collect();
        let children = slots
            .into_iter()
            .map(|slot| TemplateChild {
                node: slot.node,
                collapsed: false,
            })
            .collect();
        Slot {
            node: TemplateNode::Split(TemplateSplit {
                direction,
                children,
                weights,
                active: 0,
            }),
            sizing,
            expanded: None,
        }
    }

    /// Parses `stack`: its own sizing children plus at least two leaf
    /// members (`pane`/`plugin`), at most one marked `expanded`.
    fn stack(&mut self, node: &KdlNode, context: Context) -> Slot {
        if !node.entries().is_empty() {
            self.error(node.span(), "`stack` takes no arguments or properties");
        }
        let mut sizing = Sizing::default();
        let mut members: Vec<Slot> = Vec::new();
        // One entry per member: the leaf index of a genuine leaf member, or
        // `None` for an invalid-subtree placeholder, which the focus check
        // below must not judge.
        let mut member_leaves: Vec<Option<usize>> = Vec::new();
        if let Some(children) = node.children() {
            for child in children.nodes() {
                let child_name = child.name().value();
                if child_name == "pane" || child_name == "plugin" {
                    let leaf_index = self.tab_leaves;
                    let slot = self.structural(child, Context::Stack);
                    members.push(slot);
                    member_leaves.push(Some(leaf_index));
                } else if is_structural(child_name) {
                    self.error(
                        child.span(),
                        format!(
                            "`{child_name}` cannot be a stack member; stack members are \
                             `pane` or `plugin`"
                        ),
                    );
                    // Placeholder member so the invalid node is the only
                    // diagnostic; the file is already rejected.
                    members.push(self.structural(child, Context::Directional));
                    member_leaves.push(None);
                } else if self.sizing_config(child, &mut sizing) {
                    // recorded into `sizing`
                } else {
                    self.error(
                        child.span(),
                        format!(
                            "unknown node `{child_name}` in `stack`; expected `pane`, \
                             `plugin`, or sizing (`size`, `weight`, `min`, `preferred`)"
                        ),
                    );
                }
            }
        }
        self.check_sizing_context(&sizing, context);
        if members.len() < 2 {
            self.error(node.span(), "`stack` needs at least two members");
        }
        let mut active: Option<usize> = None;
        for (index, member) in members.iter().enumerate() {
            if let Some(span) = member.expanded {
                match active {
                    None => active = Some(index),
                    Some(_) => self.error(
                        span,
                        "another member is already `expanded`; a stack expands exactly one",
                    ),
                }
            }
        }
        let active = active.unwrap_or(0);
        let collapsed_focus: Vec<SourceSpan> = member_leaves
            .iter()
            .enumerate()
            .filter(|&(member_index, _)| member_index != active)
            .filter_map(|(_, &leaf_index)| leaf_index)
            .filter_map(|leaf_index| {
                self.tab_focus
                    .iter()
                    .find(|&&(focus_leaf, _)| focus_leaf == leaf_index)
                    .map(|&(_, span)| span)
            })
            .collect();
        for span in collapsed_focus {
            self.error(
                span,
                "a collapsed stack member cannot hold focus; mark it `expanded`",
            );
        }
        let weights = members.iter().map(|_| SizeWeight::default()).collect();
        let children = members
            .into_iter()
            .enumerate()
            .map(|(index, slot)| TemplateChild {
                node: slot.node,
                collapsed: index != active,
            })
            .collect();
        Slot {
            node: TemplateNode::Split(TemplateSplit {
                direction: SplitDirection::Stacked,
                children,
                weights,
                active,
            }),
            sizing,
            expanded: None,
        }
    }

    /// Handles a config child shared by both leaf kinds: sizing, `focus`,
    /// `expanded`. Returns `false` when the node is none of them, so the
    /// caller reports it with its own allowed-node list.
    fn leaf_config(&mut self, child: &KdlNode, context: Context, leaf: &mut LeafSlot) -> bool {
        match child.name().value() {
            "focus" => {
                if self.marker(child, "focus") {
                    match leaf.focus {
                        None => leaf.focus = Some(child.span()),
                        Some(_) => self.error(child.span(), "`focus` is declared more than once"),
                    }
                }
                true
            }
            "expanded" => {
                if self.marker(child, "expanded") {
                    if context != Context::Stack {
                        self.error(
                            child.span(),
                            "`expanded` applies only to members of a `stack`",
                        );
                    } else {
                        match leaf.expanded {
                            None => leaf.expanded = Some(child.span()),
                            Some(_) => {
                                self.error(child.span(), "`expanded` is declared more than once")
                            }
                        }
                    }
                }
                true
            }
            _ => {
                if self.sizing_config(child, &mut leaf.sizing) {
                    if context != Context::Directional {
                        self.check_sizing_context(&leaf.sizing, context);
                        leaf.sizing = Sizing::default();
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
    /// [`TemplateNode::leaves`] layout order.
    fn finish_leaf(&mut self, leaf: &LeafSlot) {
        let index = self.tab_leaves;
        self.tab_leaves += 1;
        if let Some(span) = leaf.focus {
            self.tab_focus.push((index, span));
        }
    }

    /// Reports sizing given where none is meaningful — anywhere but a child
    /// slot of `horizontal`/`vertical`.
    fn check_sizing_context(&mut self, sizing: &Sizing, context: Context) {
        if context == Context::Directional {
            return;
        }
        if let Some(span) = sizing.first_span() {
            self.error(
                span,
                "sizing applies only to children of `horizontal` or `vertical`",
            );
        }
    }

    /// Handles one sizing node (`size`, `weight`, `min`, `preferred`) into
    /// `sizing`. Returns `false` when the node is not a sizing node.
    fn sizing_config(&mut self, child: &KdlNode, sizing: &mut Sizing) -> bool {
        match child.name().value() {
            "size" => {
                if sizing.primary.is_some() {
                    self.error(
                        child.span(),
                        "this node already has `size` or `weight`; give one of the two, once",
                    );
                } else if let Some(constraint) = self.size(child) {
                    sizing.primary = Some((constraint, child.span()));
                }
                true
            }
            "weight" => {
                if sizing.primary.is_some() {
                    self.error(
                        child.span(),
                        "this node already has `size` or `weight`; give one of the two, once",
                    );
                } else if let Some(weight) = self.cells(child, "weight", u32::MAX) {
                    match SizeConstraint::flex(weight) {
                        Ok(constraint) => sizing.primary = Some((constraint, child.span())),
                        Err(err) => self.error(child.span(), err.to_string()),
                    }
                }
                true
            }
            "min" => {
                if sizing.min.is_some() {
                    self.error(child.span(), "`min` is declared more than once");
                } else if let Some(cells) = self.cell_count(child, "min") {
                    sizing.min = Some((cells, child.span()));
                }
                true
            }
            "preferred" => {
                if sizing.preferred.is_some() {
                    self.error(child.span(), "`preferred` is declared more than once");
                } else if let Some(cells) = self.cell_count(child, "preferred") {
                    sizing.preferred = Some((cells, child.span()));
                }
                true
            }
            _ => false,
        }
    }

    /// Parses a `size` value: an integer argument is exact cells, a string
    /// like `"60%"` is a percentage of the parent's axis.
    fn size(&mut self, node: &KdlNode) -> Option<SizeConstraint> {
        let entry = self.single_argument(node, "size")?;
        if let Some(value) = entry.value().as_integer() {
            let Ok(cells) = u16::try_from(value) else {
                self.error(
                    entry.span(),
                    format!("`size` cells must fit 1-{}", u16::MAX),
                );
                return None;
            };
            return match SizeConstraint::fixed(cells) {
                Ok(constraint) => Some(constraint),
                Err(err) => {
                    self.error(entry.span(), err.to_string());
                    None
                }
            };
        }
        if let Some(value) = entry.value().as_string() {
            let Some(percent) = value
                .strip_suffix('%')
                .and_then(|digits| digits.parse::<u8>().ok())
            else {
                self.error(
                    entry.span(),
                    "`size` is a cell count like `size 30` or a percentage like `size \"60%\"`",
                );
                return None;
            };
            return match SizeConstraint::percent(percent) {
                Ok(constraint) => Some(constraint),
                Err(err) => {
                    self.error(entry.span(), err.to_string());
                    None
                }
            };
        }
        self.error(
            entry.span(),
            "`size` is a cell count like `size 30` or a percentage like `size \"60%\"`",
        );
        None
    }

    /// Parses a `min`/`preferred` value: one positive cell count.
    fn cell_count(&mut self, node: &KdlNode, name: &str) -> Option<u16> {
        let cells = self.cells(node, name, u32::from(u16::MAX))?;
        let cells = u16::try_from(cells).expect("bounded by u16::MAX above");
        if cells == 0 {
            self.error(node.span(), format!("`{name}` must be at least one cell"));
            return None;
        }
        Some(cells)
    }

    /// Parses one integer argument in `1..=max` — the shared shape of
    /// `weight`, `min`, and `preferred` values.
    fn cells(&mut self, node: &KdlNode, name: &str, max: u32) -> Option<u32> {
        let entry = self.single_argument(node, name)?;
        match entry
            .value()
            .as_integer()
            .and_then(|value| u32::try_from(value).ok())
        {
            Some(value) if value <= max => Some(value),
            _ => {
                self.error(
                    entry.span(),
                    format!("`{name}` must be an integer between 1 and {max}"),
                );
                None
            }
        }
    }

    /// Parses a `command` node: the program plus its arguments, all strings.
    /// The program must be non-empty (an empty program has nothing to
    /// execute); arguments may be empty strings.
    fn command(&mut self, node: &KdlNode) -> Option<CommandTemplate> {
        if node.children().is_some() {
            self.error(node.span(), "`command` takes no children");
            return None;
        }
        let mut words = Vec::with_capacity(node.entries().len());
        for entry in node.entries() {
            if entry.name().is_some() {
                self.error(entry.span(), "`command` takes arguments, not properties");
                return None;
            }
            let Some(word) = entry.value().as_string() else {
                self.error(entry.span(), "`command` arguments must be strings");
                return None;
            };
            words.push(word.to_string());
        }
        if words.is_empty() {
            self.error(
                node.span(),
                "`command` names a program, like `command \"nvim\" \"file.txt\"`",
            );
            return None;
        }
        if words[0].is_empty() {
            self.error(node.span(), "`command` program must not be empty");
            return None;
        }
        let program = PathBuf::from(words.remove(0));
        Some(CommandTemplate {
            program,
            args: words,
        })
    }

    /// Parses an `env "NAME" "value"` node into `env`. The name must be
    /// spawnable: non-empty, no `=` (environment blocks encode entries as
    /// `NAME=value`, so `env "A=B" "x"` would reach the child as variable
    /// `A` with value `B=x`), no NUL in name or value, and set once —
    /// names compare case-insensitively, since Windows folds environment
    /// keys by case at spawn and one of two case variants would silently
    /// win there.
    fn env(&mut self, node: &KdlNode, env: &mut BTreeMap<String, String>) {
        if node.children().is_some() {
            self.error(node.span(), "`env` takes no children");
            return;
        }
        let values: Vec<&str> = node
            .entries()
            .iter()
            .filter(|entry| entry.name().is_none())
            .filter_map(|entry| entry.value().as_string())
            .collect();
        let ([name, value], true) = (values.as_slice(), values.len() == node.entries().len())
        else {
            self.error(
                node.span(),
                "`env` takes a name and a value, both strings, like `env \"RUST_LOG\" \"debug\"`",
            );
            return;
        };
        if name.is_empty() {
            self.error(node.span(), "`env` name must not be empty");
            return;
        }
        if name.contains('=') {
            self.error(node.span(), "`env` name must not contain `=`");
            return;
        }
        if name.contains('\0') || value.contains('\0') {
            self.error(
                node.span(),
                "`env` name and value must not contain a NUL character",
            );
            return;
        }
        if let Some(existing) = env.keys().find(|key| key.eq_ignore_ascii_case(name)) {
            let message = if existing == name {
                format!("`env` sets `{name}` more than once")
            } else {
                format!(
                    "`env` already sets `{existing}`; env names match case-insensitively \
                     (Windows folds environment keys by case)"
                )
            };
            self.error(node.span(), message);
            return;
        }
        env.insert((*name).to_string(), (*value).to_string());
    }

    /// Parses a single-string-argument node (`cwd`).
    fn single_string(&mut self, node: &KdlNode, name: &str) -> Option<String> {
        let entry = self.single_argument(node, name)?;
        match entry.value().as_string() {
            Some(value) if !value.is_empty() => Some(value.to_string()),
            _ => {
                self.error(entry.span(), format!("`{name}` takes one non-empty string"));
                None
            }
        }
    }

    /// Validates a node down to exactly one positional argument and no
    /// children, returning that argument's entry.
    fn single_argument<'k>(&mut self, node: &'k KdlNode, name: &str) -> Option<&'k kdl::KdlEntry> {
        if node.children().is_some() {
            self.error(node.span(), format!("`{name}` takes no children"));
            return None;
        }
        match node.entries() {
            [entry] if entry.name().is_none() => Some(entry),
            _ => {
                self.error(node.span(), format!("`{name}` takes exactly one value"));
                None
            }
        }
    }

    /// Validates a bare marker node (`focus`, `expanded`): no arguments,
    /// no properties, no children. Returns whether the marker is usable.
    fn marker(&mut self, node: &KdlNode, name: &str) -> bool {
        if node.entries().is_empty() && node.children().is_none() {
            true
        } else {
            self.error(
                node.span(),
                format!("`{name}` is a bare marker and takes no values or children"),
            );
            false
        }
    }
}

/// Focus/expanded/sizing markers collected while parsing one leaf.
#[derive(Default)]
struct LeafSlot {
    /// Sizing config from the leaf's children.
    sizing: Sizing,
    /// Span of a `focus` marker, if any.
    focus: Option<SourceSpan>,
    /// Span of an `expanded` marker, if any.
    expanded: Option<SourceSpan>,
}

/// Whether `name` is a structural layout node, as opposed to a config node.
fn is_structural(name: &str) -> bool {
    matches!(
        name,
        "pane" | "plugin" | "horizontal" | "vertical" | "stack"
    )
}
