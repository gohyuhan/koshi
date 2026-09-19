//! Renderers for `koshi keys`: binding lists, per-binding detail,
//! conflict reports, and keymap-file validation.

use super::*;

/// Column headers for a `keys list` row, matching [`render_key_binding_cells`].
const KEYS_LIST_HEADERS: &[&str] = &["mode", "key", "action", "source"];

/// Field headers for a `keys describe` answer, matching [`render_key_detail_cells`].
const KEYS_DETAIL_HEADERS: &[&str] = &[
    "key",
    "mode",
    "action",
    "display_name",
    "description",
    "scope",
    "args",
    "source",
    "continuous",
];

/// Column headers for a `keys conflicts` finding row.
const KEYS_CONFLICTS_HEADERS: &[&str] = &["severity", "finding"];

/// Column headers for a `keys list --recommended` row.
const KEYS_RECOMMENDED_HEADERS: &[&str] = &["key", "action", "plugin"];

/// One effective binding as it appears in a `keys list` answer.
#[derive(Serialize)]
struct KeyBindingSummary {
    /// The input mode the binding fires in.
    input_mode: String,
    /// The key sequence, in the angle grammar.
    key_sequence: String,
    /// The action reference the key fires.
    action_reference: String,
    /// The layer that authored the winning entry: `defaults`, `user`,
    /// `session`, or `layout` — or `defaults (unbound)` for a shipped
    /// binding a user surface displaced.
    binding_source: String,
}

/// A whole `keys list` answer.
#[derive(Serialize)]
struct KeysList {
    /// True when a user keybinding file exists but was not admitted, so the
    /// listing shows the built-in defaults.
    is_reverted: bool,
    /// Every effective binding, then every displaced default.
    key_bindings: Vec<KeyBindingSummary>,
}

/// One binding as it appears in a `keys describe` answer.
#[derive(Serialize)]
struct KeyBindingDetail {
    /// The key sequence, in the angle grammar.
    key_sequence: String,
    /// The input mode this entry fires in.
    input_mode: String,
    /// The action reference the key fires.
    action_reference: String,
    /// The action's human-facing name.
    display_name: String,
    /// The action's one-line description.
    description: String,
    /// How broad the action's effect is.
    scope: String,
    /// The preset arguments bound with the action, `null` when none.
    action_arguments: serde_json::Value,
    /// The layer that authored the winning entry.
    binding_source: String,
    /// Whether the action re-arms its prefix when fired from a multi-chord
    /// binding.
    is_continuous: bool,
}

/// One conflict-detection finding as it appears in a `keys conflicts` or
/// `keys validate` answer.
#[derive(Serialize)]
struct ConflictFinding {
    /// The finding's weight: `warning`, `collision`, or `fatal`.
    severity: String,
    /// The user-facing message.
    finding_message: String,
}

/// A whole `keys conflicts` answer.
#[derive(Serialize)]
struct KeysConflicts {
    /// What a loader would do with the user keymap.
    verdict: String,
    /// Why the user keybinding file was ignored (it could not be read or
    /// parsed), `null` when it loaded. When set, the verdict and findings
    /// describe the built-in defaults, not the file.
    file_error: Option<String>,
    /// Every finding, warnings included.
    conflict_findings: Vec<ConflictFinding>,
}

/// A whole `keys validate` answer.
#[derive(Serialize)]
struct KeysValidation {
    /// True when the file parsed as valid keybinding KDL.
    is_valid: bool,
    /// True when a reload would apply the file.
    is_applicable: bool,
    /// Parse problems, one per line, when the file did not parse.
    parse_errors: Vec<String>,
    /// Conflict-detection findings, when the file parsed.
    conflict_findings: Vec<ConflictFinding>,
}

/// Render a `koshi keys list` answer from the offline keymap view.
#[must_use]
pub fn render_keys_list(
    keymap_view: &crate::keymap::KeymapView,
    requested_mode: Option<&str>,
    scope_filter: Option<KeymapScope>,
    output_format: OutputFormat,
) -> String {
    let scope_filter_label = scope_filter.map(format_scope_argument_label);
    let mut key_bindings: Vec<KeyBindingSummary> = Vec::new();
    for (mode_name, merged_mode_map) in &keymap_view.merged_keymap.mode_map_by_name {
        if requested_mode.is_some_and(|requested_mode| requested_mode != mode_name.get_name()) {
            continue;
        }
        for (key_sequence, merged_binding) in &merged_mode_map.user_bindings_by_key_sequence {
            key_bindings.push(KeyBindingSummary {
                input_mode: mode_name.get_name().to_string(),
                key_sequence: key_sequence.to_string(),
                action_reference: merged_binding.bound_action.action_reference.to_string(),
                binding_source: merged_binding.layer_origin.to_string(),
            });
        }
        for (key_sequence, default_binding) in &merged_mode_map.default_bindings_by_key_sequence {
            key_bindings.push(KeyBindingSummary {
                input_mode: mode_name.get_name().to_string(),
                key_sequence: key_sequence.to_string(),
                action_reference: default_binding.action_reference.to_string(),
                binding_source: "defaults".to_string(),
            });
        }
        for (key_sequence, displaced_default_binding) in
            &merged_mode_map.unbound_default_bindings_by_key_sequence
        {
            key_bindings.push(KeyBindingSummary {
                input_mode: mode_name.get_name().to_string(),
                key_sequence: key_sequence.to_string(),
                action_reference: displaced_default_binding.action_reference.to_string(),
                binding_source: "defaults (unbound)".to_string(),
            });
        }
    }
    if let Some(scope_filter_label) = scope_filter_label {
        key_bindings.retain(|binding| binding.binding_source == scope_filter_label);
    }
    key_bindings.sort_by(|left_binding, right_binding| {
        (&left_binding.input_mode, &left_binding.key_sequence)
            .cmp(&(&right_binding.input_mode, &right_binding.key_sequence))
    });
    match output_format {
        OutputFormat::Json => render_json(&KeysList {
            is_reverted: keymap_view.is_reverted_to_defaults,
            key_bindings,
        }),
        OutputFormat::Table => render_table(
            KEYS_LIST_HEADERS,
            key_bindings.iter().map(render_key_binding_cells).collect(),
        ),
    }
}

/// Render a `koshi keys list --recommended` answer. Plugin-recommended
/// bindings come from installed plugin manifests; none exist until plugins
/// do, so the listing is empty.
#[must_use]
pub fn render_keys_recommended(output_format: OutputFormat) -> String {
    let recommended_key_bindings: Vec<KeyBindingSummary> = Vec::new();
    match output_format {
        OutputFormat::Json => render_json(&recommended_key_bindings),
        OutputFormat::Table => render_table(KEYS_RECOMMENDED_HEADERS, Vec::new()),
    }
}

/// Render a `koshi keys describe <key-sequence>` answer: one detail block
/// per mode the sequence is bound in.
///
/// # Errors
/// The parser's message when `sequence` is not a valid key sequence; `Ok(None)`
/// when it parses but nothing is bound on it in any mode.
pub fn render_keys_describe(
    keymap_view: &crate::keymap::KeymapView,
    key_sequence_text: &str,
    output_format: OutputFormat,
) -> Result<Option<String>, String> {
    let parsed_key_sequence = koshi_config::key_sequence::parse_sequence(
        key_sequence_text,
        keymap_view.config.leader,
        keymap_view.config.max_chord_depth,
    )
    .map_err(|parse_error| parse_error.to_string())?;

    let mut key_binding_details: Vec<KeyBindingDetail> = Vec::new();
    for (mode_name, merged_mode_map) in &keymap_view.merged_keymap.mode_map_by_name {
        let (matched_binding, binding_source) = if let Some(merged_binding) = merged_mode_map
            .user_bindings_by_key_sequence
            .get(&parsed_key_sequence)
        {
            (
                &merged_binding.bound_action,
                merged_binding.layer_origin.to_string(),
            )
        } else if let Some(default_binding) = merged_mode_map
            .default_bindings_by_key_sequence
            .get(&parsed_key_sequence)
        {
            (default_binding, "defaults".to_string())
        } else {
            continue;
        };
        let action_metadata = keymap_view
            .registry
            .find_action_metadata(&matched_binding.action_reference);
        let action_arguments =
            if matched_binding.action_arguments == koshi_core::resolve::ActionArgs::None {
                serde_json::Value::Null
            } else {
                serde_json::to_value(&matched_binding.action_arguments)
                    .expect("action args serialize: plain enums and strings")
            };
        key_binding_details.push(KeyBindingDetail {
            key_sequence: parsed_key_sequence.to_string(),
            input_mode: mode_name.get_name().to_string(),
            action_reference: matched_binding.action_reference.to_string(),
            display_name: action_metadata
                .map_or(String::new(), |metadata| metadata.display_name.clone()),
            description: action_metadata
                .map_or(String::new(), |metadata| metadata.description.clone()),
            scope: action_metadata
                .map_or("-", |metadata| format_scope_label(metadata.scope))
                .to_string(),
            action_arguments,
            binding_source,
            is_continuous: action_metadata.is_some_and(|metadata| metadata.is_continuous),
        });
    }
    if key_binding_details.is_empty() {
        return Ok(None);
    }
    Ok(Some(match output_format {
        OutputFormat::Json => render_json(&key_binding_details),
        OutputFormat::Table => {
            let mut rendered_output = String::new();
            for (detail_index, key_binding_detail) in key_binding_details.iter().enumerate() {
                if detail_index > 0 {
                    rendered_output.push('\n');
                }
                rendered_output.push_str(&render_fields(
                    KEYS_DETAIL_HEADERS,
                    render_key_detail_cells(key_binding_detail),
                ));
            }
            rendered_output
        }
    }))
}

/// Render a `koshi keys conflicts` answer from the offline keymap view. An
/// ignored user file (unreadable or unparseable) is part of the answer on
/// both formats, so a consumer reading only stdout never mistakes a
/// defaults-only "apply" for a clean file.
#[must_use]
pub fn render_keys_conflicts(
    keymap_view: &crate::keymap::KeymapView,
    output_format: OutputFormat,
) -> String {
    let conflict_findings = build_conflict_findings(&keymap_view.report);
    let conflicts_answer = KeysConflicts {
        verdict: format_keymap_verdict_label(keymap_view.report.get_verdict()).to_string(),
        file_error: keymap_view.file_error_message.clone(),
        conflict_findings,
    };
    match output_format {
        OutputFormat::Json => render_json(&conflicts_answer),
        OutputFormat::Table => {
            let mut rendered_output = String::new();
            if let Some(file_error_message) = &conflicts_answer.file_error {
                rendered_output.push_str("file: ignored (");
                rendered_output.push_str(file_error_message);
                rendered_output.push_str(")\n");
            }
            rendered_output.push_str(&format!("verdict: {}\n", conflicts_answer.verdict));
            append_conflict_table(&mut rendered_output, &conflicts_answer.conflict_findings);
            rendered_output
        }
    }
}

/// Render a `koshi keys validate <path>` answer.
#[must_use]
pub fn render_keys_validate(
    validation_outcome: &crate::keymap::KeymapValidationOutcome,
    output_format: OutputFormat,
) -> String {
    let validation_answer = match validation_outcome {
        crate::keymap::KeymapValidationOutcome::ParseFailed(parse_errors) => KeysValidation {
            is_valid: false,
            is_applicable: false,
            parse_errors: parse_errors.clone(),
            conflict_findings: Vec::new(),
        },
        crate::keymap::KeymapValidationOutcome::Checked {
            report,
            is_applicable,
        } => KeysValidation {
            is_valid: true,
            is_applicable: *is_applicable,
            parse_errors: Vec::new(),
            conflict_findings: build_conflict_findings(report),
        },
    };
    match output_format {
        OutputFormat::Json => render_json(&validation_answer),
        OutputFormat::Table => {
            let mut rendered_output = String::new();
            if validation_answer.is_valid {
                rendered_output.push_str(if validation_answer.is_applicable {
                    "valid: a reload would apply this file\n"
                } else {
                    "invalid: a reload would keep the running keymap\n"
                });
                append_conflict_table(&mut rendered_output, &validation_answer.conflict_findings);
            } else {
                rendered_output.push_str("invalid: the file does not parse\n");
                for parse_error in &validation_answer.parse_errors {
                    rendered_output.push_str("error: ");
                    rendered_output.push_str(parse_error);
                    rendered_output.push('\n');
                }
            }
            rendered_output
        }
    }
}

/// Whether a rendered validation answer reports a file a reload would apply.
#[must_use]
pub fn does_validation_apply(validation_outcome: &crate::keymap::KeymapValidationOutcome) -> bool {
    match validation_outcome {
        crate::keymap::KeymapValidationOutcome::ParseFailed(_) => false,
        crate::keymap::KeymapValidationOutcome::Checked { is_applicable, .. } => *is_applicable,
    }
}

/// One [`KeyBindingSummary`] as table cells, in [`KEYS_LIST_HEADERS`] order.
fn render_key_binding_cells(binding: &KeyBindingSummary) -> Vec<String> {
    vec![
        binding.input_mode.clone(),
        binding.key_sequence.clone(),
        binding.action_reference.clone(),
        binding.binding_source.clone(),
    ]
}

/// One [`KeyBindingDetail`] as field cells, in [`KEYS_DETAIL_HEADERS`] order.
fn render_key_detail_cells(key_binding_detail: &KeyBindingDetail) -> Vec<String> {
    vec![
        key_binding_detail.key_sequence.clone(),
        key_binding_detail.input_mode.clone(),
        key_binding_detail.action_reference.clone(),
        key_binding_detail.display_name.clone(),
        key_binding_detail.description.clone(),
        key_binding_detail.scope.clone(),
        if key_binding_detail.action_arguments.is_null() {
            "-".to_string()
        } else {
            key_binding_detail.action_arguments.to_string()
        },
        key_binding_detail.binding_source.clone(),
        key_binding_detail.is_continuous.to_string(),
    ]
}

/// Append the findings table to `rendered`, or nothing when there are no
/// findings. The one table shape every keys-conflict renderer shares.
fn append_conflict_table(rendered_output: &mut String, conflict_findings: &[ConflictFinding]) {
    if !conflict_findings.is_empty() {
        rendered_output.push_str(&render_table(
            KEYS_CONFLICTS_HEADERS,
            conflict_findings
                .iter()
                .map(render_conflict_finding_cells)
                .collect(),
        ));
    }
}

/// One [`ConflictFinding`] as table cells, in [`KEYS_CONFLICTS_HEADERS`] order.
fn render_conflict_finding_cells(conflict_finding: &ConflictFinding) -> Vec<String> {
    vec![
        conflict_finding.severity.clone(),
        conflict_finding.finding_message.clone(),
    ]
}

/// Every report finding as a [`ConflictFinding`], in report order.
fn build_conflict_findings(
    report: &koshi_config::conflict::ConflictReport,
) -> Vec<ConflictFinding> {
    report
        .diagnostics
        .iter()
        .map(|diagnostic| ConflictFinding {
            severity: format_conflict_severity_label(diagnostic.get_severity()).to_string(),
            finding_message: diagnostic.to_string(),
        })
        .collect()
}

/// The stable label of one severity tier.
fn format_conflict_severity_label(
    conflict_severity: koshi_config::conflict::ConflictSeverity,
) -> &'static str {
    match conflict_severity {
        koshi_config::conflict::ConflictSeverity::Warning => "warning",
        koshi_config::conflict::ConflictSeverity::Collision => "collision",
        koshi_config::conflict::ConflictSeverity::Fatal => "fatal",
    }
}

/// The stable label of one keymap verdict.
fn format_keymap_verdict_label(
    keymap_verdict: koshi_config::conflict::KeymapVerdict,
) -> &'static str {
    match keymap_verdict {
        koshi_config::conflict::KeymapVerdict::Apply => "apply",
        koshi_config::conflict::KeymapVerdict::RevertToDefaults => "revert-to-defaults",
        koshi_config::conflict::KeymapVerdict::Reject => "reject",
    }
}

/// The `source` label a [`KeymapScope`] filter matches.
fn format_scope_argument_label(scope_argument: KeymapScope) -> &'static str {
    match scope_argument {
        KeymapScope::Default => "defaults",
        KeymapScope::User => "user",
        KeymapScope::Session => "session",
        KeymapScope::Layout => "layout",
    }
}
