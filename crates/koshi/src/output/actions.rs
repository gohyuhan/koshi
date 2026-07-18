//! Renderers for `koshi actions`: the action list and per-action detail.

use super::*;

/// Column headers for an `actions list` row, matching [`action_summary_row`].
const ACTION_LIST_HEADERS: &[&str] = &["action", "command", "scope"];

/// Field headers for an `actions explain` answer, matching [`action_detail_row`].
const ACTION_DETAIL_HEADERS: &[&str] = &[
    "action",
    "display_name",
    "description",
    "scope",
    "targets",
    "command",
    "examples",
];

/// One action as it appears in an `actions list` answer.
#[derive(Serialize)]
struct ActionSummary {
    /// Canonical action reference, e.g. `core:new-pane`.
    action: String,
    /// The internal command the action dispatches.
    command: String,
    /// How broad the action's effect is.
    scope: String,
}

/// One action as it appears in an `actions explain` answer.
#[derive(Serialize)]
struct ActionDetail {
    /// Canonical action reference, e.g. `core:new-pane`.
    action: String,
    /// Human-facing name.
    display_name: String,
    /// One-line description.
    description: String,
    /// How broad the action's effect is.
    scope: String,
    /// Entity kinds the action can target.
    targets: Vec<String>,
    /// The internal command the action dispatches.
    command: String,
    /// Ways to invoke the action: its config reference and, when one exists,
    /// its CLI verb.
    examples: Vec<String>,
}

/// Render a `koshi actions list` answer over the supported actions in the
/// static table. Coming-soon actions are omitted until the runtime implements
/// them.
#[must_use]
pub fn render_actions_list(format: FormatArg) -> String {
    let summaries: Vec<ActionSummary> = core_action_seeds()
        .iter()
        .filter(|(_, metadata)| metadata.status == ActionStatus::Available)
        .map(action_summary)
        .collect();
    match format {
        FormatArg::Json => json(&summaries),
        FormatArg::Table => table(
            ACTION_LIST_HEADERS,
            summaries.iter().map(action_summary_row).collect(),
        ),
    }
}

/// Render a `koshi actions explain <action>` answer, or `None` when no
/// supported action matches `action` (accepted as a bare name or a full `core:`
/// reference). Coming-soon actions are hidden, so they resolve to `None` the
/// same as an unknown name.
#[must_use]
pub fn render_action_explain(action: &str, format: FormatArg) -> Option<String> {
    let seeds = core_action_seeds();
    let (action_ref, metadata) = seeds.iter().find(|(candidate, _)| {
        candidate.name.as_str() == action || candidate.to_string() == action
    })?;
    if metadata.status != ActionStatus::Available {
        return None;
    }
    let detail = action_detail(action_ref, metadata);
    Some(match format {
        FormatArg::Json => json(&detail),
        FormatArg::Table => fields(ACTION_DETAIL_HEADERS, action_detail_row(&detail)),
    })
}

/// One seed entry as an [`ActionSummary`].
fn action_summary((action, metadata): &(ActionRef, ActionMetadata)) -> ActionSummary {
    ActionSummary {
        action: action.to_string(),
        command: command_label(&metadata.handler),
        scope: scope_label(metadata.scope_class).to_string(),
    }
}

/// One [`ActionSummary`] as table cells, in [`ACTION_LIST_HEADERS`] order.
fn action_summary_row(summary: &ActionSummary) -> Vec<String> {
    vec![
        summary.action.clone(),
        summary.command.clone(),
        summary.scope.clone(),
    ]
}

/// One seed entry as an [`ActionDetail`].
fn action_detail(action: &ActionRef, metadata: &ActionMetadata) -> ActionDetail {
    ActionDetail {
        action: action.to_string(),
        display_name: metadata.display_name.clone(),
        description: metadata.description.clone(),
        scope: scope_label(metadata.scope_class).to_string(),
        targets: metadata
            .target_compat
            .iter()
            .map(|target| target_label(*target).to_string())
            .collect(),
        command: command_label(&metadata.handler),
        examples: examples_for(action),
    }
}

/// One [`ActionDetail`] as field cells, in [`ACTION_DETAIL_HEADERS`] order. The
/// list-valued `targets`/`examples` join with `, ` and print `-` when empty.
fn action_detail_row(detail: &ActionDetail) -> Vec<String> {
    vec![
        detail.action.clone(),
        detail.display_name.clone(),
        detail.description.clone(),
        detail.scope.clone(),
        join_cell(&detail.targets),
        detail.command.clone(),
        join_cell(&detail.examples),
    ]
}

/// A list of strings as one cell: `-` when empty, else the items joined by `, `.
pub(super) fn join_cell(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

/// An action scope as its kebab-case label.
pub(super) fn scope_label(scope: ActionScope) -> &'static str {
    match scope {
        ActionScope::PaneSession => "pane-session",
        ActionScope::Client => "client",
        ActionScope::Tab => "tab",
        ActionScope::Global => "global",
    }
}

/// A target kind as its lowercase label.
pub(super) fn target_label(target: TargetKind) -> &'static str {
    match target {
        TargetKind::Session => "session",
        TargetKind::Tab => "tab",
        TargetKind::Pane => "pane",
        TargetKind::Client => "client",
    }
}

/// The internal command an action dispatches, as a label: the core command's
/// name, `plugin-host` for a plugin call, or `sequence` for a macro.
pub(super) fn command_label(handler: &ActionHandlerRef) -> String {
    match handler {
        ActionHandlerRef::CoreCommand(kind) => format!("{kind:?}"),
        ActionHandlerRef::PluginHostCall(_) => "plugin-host".to_string(),
        ActionHandlerRef::Sequence(_) => "sequence".to_string(),
    }
}

/// The usage examples for an action: always its config reference
/// (`core:new-pane`), plus `koshi <verb>` when that runs the action on its own.
fn examples_for(action: &ActionRef) -> Vec<String> {
    let name = action.name.as_str();
    let mut examples = vec![action.to_string()];
    if cli_verb_runnable_bare(name) {
        examples.push(format!("koshi {name}"));
    }
    examples
}

/// Whether `koshi <name>` parses on its own — a top-level verb with no required
/// arguments. Verbs that need arguments (`run`, `resize-pane`) return false.
fn cli_verb_runnable_bare(name: &str) -> bool {
    use clap::Parser;

    crate::cli::Cli::try_parse_from(["koshi", name]).is_ok()
}

// --- Keybinding introspection ---
