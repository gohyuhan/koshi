//! Renderers for `koshi actions`: the action list and per-action detail.

use super::*;

/// Column headers for an `actions list` row, matching
/// [`render_action_summary_cells`].
const ACTION_LIST_HEADERS: &[&str] = &["action", "command", "scope", "cli_invocation"];

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
    /// The `koshi` invocation that runs the action from a shell
    /// ([`format_cli_invocation`]).
    cli_invocation: String,
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
    /// Ways to invoke the action ([`list_action_examples`]).
    examples: Vec<String>,
}

/// Render a `koshi actions list` answer over every action in the static table.
#[must_use]
pub fn render_actions_list(output_format: OutputFormat) -> String {
    let action_registry = ActionRegistry::new();
    let action_summaries: Vec<ActionSummary> = build_core_action_seeds()
        .iter()
        .map(|action_seed| build_action_summary(action_seed, &action_registry))
        .collect();
    match output_format {
        OutputFormat::Json => render_json(&action_summaries),
        OutputFormat::Table => render_table(
            ACTION_LIST_HEADERS,
            action_summaries
                .iter()
                .map(render_action_summary_cells)
                .collect(),
        ),
    }
}

/// Render a `koshi actions explain <action>` answer, or `None` when no action
/// matches `action_reference_text` (accepted as a bare name or a full `core:`
/// reference, as [`parse_action_reference_text`] reads it).
#[must_use]
pub fn render_action_explain(
    action_reference_text: &str,
    output_format: OutputFormat,
) -> Option<String> {
    let action_reference = parse_action_reference_text(action_reference_text)?;
    let action_seeds = build_core_action_seeds();
    let (_, action_metadata) = action_seeds
        .iter()
        .find(|(candidate_action, _)| *candidate_action == action_reference)?;
    let action_detail =
        build_action_detail(&action_reference, action_metadata, &ActionRegistry::new());
    Some(match output_format {
        OutputFormat::Json => render_json(&action_detail),
        OutputFormat::Table => render_fields(
            ACTION_DETAIL_HEADERS,
            render_action_detail_cells(&action_detail),
        ),
    })
}

/// One seed entry as an [`ActionSummary`], with its CLI invocation read
/// from `action_registry`.
fn build_action_summary(
    (action_reference, action_metadata): &(ActionReference, ActionMetadata),
    action_registry: &ActionRegistry,
) -> ActionSummary {
    ActionSummary {
        action: action_reference.to_string(),
        command: format_command_label(&action_metadata.handler),
        scope: format_scope_label(action_metadata.scope).to_string(),
        cli_invocation: format_cli_invocation(action_reference, action_registry),
    }
}

/// One [`ActionSummary`] as table cells, in [`ACTION_LIST_HEADERS`] order.
fn render_action_summary_cells(summary: &ActionSummary) -> Vec<String> {
    vec![
        summary.action.clone(),
        summary.command.clone(),
        summary.scope.clone(),
        summary.cli_invocation.clone(),
    ]
}

/// One seed entry as an [`ActionDetail`], with its examples read from
/// `action_registry`.
fn build_action_detail(
    action_reference: &ActionReference,
    action_metadata: &ActionMetadata,
    action_registry: &ActionRegistry,
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
        examples: list_action_examples(action_reference, action_registry),
    }
}

/// One [`ActionDetail`] as field cells, in [`ACTION_DETAIL_HEADERS`] order. The
/// list-valued `targets`/`examples` join with `, ` and print `-` when empty.
fn render_action_detail_cells(action_detail: &ActionDetail) -> Vec<String> {
    vec![
        action_detail.action.clone(),
        action_detail.display_name.clone(),
        action_detail.description.clone(),
        action_detail.scope.clone(),
        render_joined_text_cell(&action_detail.targets),
        action_detail.command.clone(),
        render_joined_text_cell(&action_detail.examples),
    ]
}

/// A list of strings as one cell: `-` when empty, else the items joined by `, `.
pub(super) fn render_joined_text_cell(cell_text_values: &[String]) -> String {
    if cell_text_values.is_empty() {
        "-".to_string()
    } else {
        cell_text_values.join(", ")
    }
}

/// An action scope as its kebab-case label.
pub(super) fn format_scope_label(action_scope: ActionScope) -> &'static str {
    match action_scope {
        ActionScope::PaneSession => "pane-session",
        ActionScope::Client => "client",
        ActionScope::Tab => "tab",
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

/// The dispatch route an action uses: the core command's name, or `client` for
/// a viewer-local action.
pub(super) fn format_command_label(action_handler: &ActionHandlerReference) -> String {
    match action_handler {
        ActionHandlerReference::CoreCommand(command_kind) => format!("{command_kind:?}"),
        ActionHandlerReference::CoreClient(_) => "client".to_string(),
    }
}

/// The usage examples for an action: its config reference
/// (`core:new-pane`), its CLI invocation ([`format_cli_invocation`]), and
/// `koshi <name>` when that verb runs the action on its own and is not the CLI
/// invocation already. `core:new-pane` gives `core:new-pane`,
/// `koshi actions run core:new-pane` and `koshi new-pane`.
fn list_action_examples(
    action_reference: &ActionReference,
    action_registry: &ActionRegistry,
) -> Vec<String> {
    let action_name = action_reference.action_name.get_name();
    let cli_invocation = format_cli_invocation(action_reference, action_registry);
    let cli_verb_invocation = format!("koshi {action_name}");
    let has_own_cli_verb =
        cli_verb_invocation != cli_invocation && can_run_cli_verb_without_arguments(action_name);
    let mut action_examples = vec![action_reference.to_string(), cli_invocation];
    if has_own_cli_verb {
        action_examples.push(cli_verb_invocation);
    }
    action_examples
}

/// The `koshi` invocation that runs `action_reference` from a shell:
/// `koshi actions run core:new-tab`, or, for an action whose command takes
/// arguments, the verb that supplies them
/// ([`format_argument_verb_invocation`]): `koshi focus-tab`.
fn format_cli_invocation(
    action_reference: &ActionReference,
    action_registry: &ActionRegistry,
) -> String {
    match resolve_action(action_reference, action_registry, Direction::Right) {
        Err(ResolveError::ArgumentsRequired { .. }) => {
            format_argument_verb_invocation(action_reference)
        }
        Ok(_) | Err(ResolveError::Unregistered { .. }) => {
            format!("koshi actions run {action_reference}")
        }
    }
}

/// Whether `koshi <name>` parses on its own — a top-level verb with no required
/// arguments. Verbs that need arguments (`run`, `resize-pane`) return false.
fn can_run_cli_verb_without_arguments(cli_verb_name: &str) -> bool {
    use clap::Parser;

    crate::cli::Cli::try_parse_from(["koshi", cli_verb_name]).is_ok()
}
