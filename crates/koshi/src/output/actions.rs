//! Renderers for `koshi actions`: the action list and per-action detail.

use super::*;

/// Column headers for an `actions list` row, matching
/// [`render_action_summary_cells`].
const ACTION_LIST_HEADERS: &[&str] = &["action", "command", "scope"];

/// Field headers for an `actions explain` answer, matching
/// [`render_action_detail_cells`].
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
pub fn render_actions_list(output_format: OutputFormat) -> String {
    let summaries: Vec<ActionSummary> = build_core_action_seeds()
        .iter()
        .filter(|(_, metadata)| metadata.action_status == ActionStatus::Available)
        .map(build_action_summary)
        .collect();
    match output_format {
        OutputFormat::Json => render_json(&summaries),
        OutputFormat::Table => render_table(
            ACTION_LIST_HEADERS,
            summaries.iter().map(render_action_summary_cells).collect(),
        ),
    }
}

/// Render a `koshi actions explain <action>` answer, or `None` when no
/// supported action matches `action` (accepted as a bare name or a full `core:`
/// reference). Coming-soon actions are hidden, so they resolve to `None` the
/// same as an unknown name.
#[must_use]
pub fn render_action_explain(action_name: &str, output_format: OutputFormat) -> Option<String> {
    let seeds = build_core_action_seeds();
    let (action_reference, metadata) = seeds.iter().find(|(candidate, _)| {
        candidate.action_name.get_name() == action_name || candidate.to_string() == action_name
    })?;
    if metadata.action_status != ActionStatus::Available {
        return None;
    }
    let action_detail = build_action_detail(action_reference, metadata);
    Some(match output_format {
        OutputFormat::Json => render_json(&action_detail),
        OutputFormat::Table => render_fields(
            ACTION_DETAIL_HEADERS,
            render_action_detail_cells(&action_detail),
        ),
    })
}

/// One seed entry as an [`ActionSummary`].
fn build_action_summary(
    (action_reference, action_metadata): &(ActionReference, ActionMetadata),
) -> ActionSummary {
    ActionSummary {
        action: action_reference.to_string(),
        command: format_command_label(&action_metadata.handler),
        scope: format_scope_label(action_metadata.scope).to_string(),
    }
}

/// One [`ActionSummary`] as table cells, in [`ACTION_LIST_HEADERS`] order.
fn render_action_summary_cells(summary: &ActionSummary) -> Vec<String> {
    vec![
        summary.action.clone(),
        summary.command.clone(),
        summary.scope.clone(),
    ]
}

/// One seed entry as an [`ActionDetail`].
fn build_action_detail(
    action_reference: &ActionReference,
    action_metadata: &ActionMetadata,
) -> ActionDetail {
    ActionDetail {
        action: action_reference.to_string(),
        display_name: action_metadata.display_name.clone(),
        description: action_metadata.description.clone(),
        scope: format_scope_label(action_metadata.scope).to_string(),
        targets: action_metadata
            .target_kinds
            .iter()
            .map(|target_kind| format_target_label(*target_kind).to_string())
            .collect(),
        command: format_command_label(&action_metadata.handler),
        examples: build_action_examples(action_reference),
    }
}

/// One [`ActionDetail`] as field cells, in [`ACTION_DETAIL_HEADERS`] order. The
/// list-valued `targets`/`examples` join with `, ` and print `-` when empty.
fn render_action_detail_cells(detail: &ActionDetail) -> Vec<String> {
    vec![
        detail.action.clone(),
        detail.display_name.clone(),
        detail.description.clone(),
        detail.scope.clone(),
        render_joined_text_cell(&detail.targets),
        detail.command.clone(),
        render_joined_text_cell(&detail.examples),
    ]
}

/// A list of strings as one cell: `-` when empty, else the items joined by `, `.
pub(super) fn render_joined_text_cell(text_values: &[String]) -> String {
    if text_values.is_empty() {
        "-".to_string()
    } else {
        text_values.join(", ")
    }
}

/// An action scope as its kebab-case label.
pub(super) fn format_scope_label(action_scope: ActionScope) -> &'static str {
    match action_scope {
        ActionScope::PaneSession => "pane-session",
        ActionScope::Client => "client",
        ActionScope::Tab => "tab",
        ActionScope::Global => "global",
    }
}

/// A target kind as its lowercase label.
pub(super) fn format_target_label(target_kind: TargetKind) -> &'static str {
    match target_kind {
        TargetKind::Session => "session",
        TargetKind::Tab => "tab",
        TargetKind::Pane => "pane",
        TargetKind::Client => "client",
    }
}

/// The dispatch route an action uses: the core command's name, `client` for a
/// viewer-local action, `plugin-host` for a plugin call, or `sequence` for a
/// macro.
pub(super) fn format_command_label(action_handler: &ActionHandlerReference) -> String {
    match action_handler {
        ActionHandlerReference::CoreCommand(command_kind) => format!("{command_kind:?}"),
        ActionHandlerReference::CoreClient(_) => "client".to_string(),
        ActionHandlerReference::PluginHostCall(_) => "plugin-host".to_string(),
        ActionHandlerReference::Sequence(_) => "sequence".to_string(),
    }
}

/// The usage examples for an action: always its config reference
/// (`core:new-pane`), plus `koshi <verb>` when that runs the action on its own.
fn build_action_examples(action_reference: &ActionReference) -> Vec<String> {
    let action_name = action_reference.action_name.get_name();
    let mut action_examples = vec![action_reference.to_string()];
    if can_run_cli_verb_without_arguments(action_name) {
        action_examples.push(format!("koshi {action_name}"));
    }
    action_examples
}

/// Whether `koshi <name>` parses on its own — a top-level verb with no required
/// arguments. Verbs that need arguments (`run`, `resize-pane`) return false.
fn can_run_cli_verb_without_arguments(cli_verb_name: &str) -> bool {
    use clap::Parser;

    crate::cli::Cli::try_parse_from(["koshi", cli_verb_name]).is_ok()
}
