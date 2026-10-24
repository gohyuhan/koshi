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
    "source",
    "displaced_by",
    "continuous",
];

/// Column headers for a `keys conflicts` finding row.
const KEYS_CONFLICTS_HEADERS: &[&str] = &["severity", "finding"];

/// One effective binding as it appears in a `keys list` answer.
#[derive(Serialize)]
struct KeyBindingSummary {
    /// The input mode the binding fires in.
    input_mode: String,
    /// The key sequence, in the angle grammar.
    key_sequence: String,
    /// The action reference the key fires.
    action_reference: String,
    /// The layer that authored the winning entry: `defaults` or `user` — or
    /// `defaults (unbound)` for a shipped binding the user displaced.
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
    /// The layer that authored the entry: `defaults`, `user`, or `defaults
    /// (unbound)` for a built-in binding the user displaced.
    binding_source: String,
    /// For a `defaults (unbound)` entry, the user entry that displaced it:
    /// `user bind` or `user remove`. `None` for every other entry.
    displaced_by: Option<String>,
    /// Whether the action re-arms its prefix when fired from a multi-chord
    /// binding.
    is_continuous: bool,
}

/// One conflict-detection finding as it appears in a `keys conflicts` or
/// `keys validate` answer.
#[derive(Serialize)]
struct ConflictFinding {
    /// The finding's weight: `warning` or `fatal`.
    severity: String,
    /// The user-facing message.
    finding_message: String,
}

/// A whole `keys conflicts` answer.
#[derive(Serialize)]
struct KeysConflicts {
    /// What a client does with the user keybinding file when it attaches:
    /// `apply` or `reject`. `apply` when no file exists.
    verdict: String,
    /// Why the user keybinding file was ignored (it could not be read or
    /// parsed), `null` when it loaded. When set, the verdict is `reject`.
    file_error: Option<String>,
    /// Every finding, warnings included.
    conflict_findings: Vec<ConflictFinding>,
}

/// A whole `keys validate` answer.
#[derive(Serialize)]
struct KeysValidation {
    /// True when the file parsed as valid keybinding KDL.
    is_valid: bool,
    /// True when a client would apply the file as its `keybinding.kdl`.
    is_applicable: bool,
    /// Parse problems, one per line, when the file did not parse.
    parse_errors: Vec<String>,
    /// Conflict-detection findings, when the file parsed.
    conflict_findings: Vec<ConflictFinding>,
}

/// Render a `koshi keys list` answer from the offline keymap view.
///
/// `scope_filter` keeps the rows one layer authored. [`KeymapScope::Default`]
/// keeps the surviving defaults and the `defaults (unbound)` rows of the
/// defaults a user binding or `remove` displaced. [`KeymapScope::User`]
/// keeps the user's bindings.
#[must_use]
pub fn render_keys_list(
    keymap_view: &crate::keymap::KeymapView,
    requested_input_mode_name: Option<&str>,
    scope_filter: Option<KeymapScope>,
    output_format: OutputFormat,
) -> String {
    let listed_layer_origin = scope_filter.map(resolve_scope_layer_origin);
    let is_layer_listed = |layer_origin: koshi_config::conflict::LayerOrigin| {
        listed_layer_origin.is_none_or(|listed_layer_origin| listed_layer_origin == layer_origin)
    };
    let are_defaults_listed = is_layer_listed(koshi_config::conflict::LayerOrigin::Defaults);
    let mut key_bindings: Vec<KeyBindingSummary> = Vec::new();
    for (mode_name, merged_mode_keymap) in &keymap_view.merged_keymap.mode_keymap_by_name {
        if requested_input_mode_name.is_some_and(|requested_input_mode_name| {
            requested_input_mode_name != mode_name.get_name()
        }) {
            continue;
        }
        for (key_sequence, merged_binding) in &merged_mode_keymap.user_bindings_by_key_sequence {
            if !is_layer_listed(merged_binding.layer_origin) {
                continue;
            }
            key_bindings.push(KeyBindingSummary {
                input_mode: mode_name.get_name().to_string(),
                key_sequence: key_sequence.to_string(),
                action_reference: merged_binding.bound_action.action_reference.to_string(),
                binding_source: merged_binding.layer_origin.to_string(),
            });
        }
        if !are_defaults_listed {
            continue;
        }
        for (key_sequence, default_binding) in &merged_mode_keymap.default_bindings_by_key_sequence
        {
            key_bindings.push(KeyBindingSummary {
                input_mode: mode_name.get_name().to_string(),
                key_sequence: key_sequence.to_string(),
                action_reference: default_binding.action_reference.to_string(),
                binding_source: "defaults".to_string(),
            });
        }
        for (key_sequence, displaced_default_binding) in
            &merged_mode_keymap.unbound_default_bindings_by_key_sequence
        {
            key_bindings.push(KeyBindingSummary {
                input_mode: mode_name.get_name().to_string(),
                key_sequence: key_sequence.to_string(),
                action_reference: displaced_default_binding
                    .bound_action
                    .action_reference
                    .to_string(),
                binding_source: "defaults (unbound)".to_string(),
            });
        }
    }
    key_bindings.sort_by(|left_binding, right_binding| {
        (&left_binding.input_mode, &left_binding.key_sequence)
            .cmp(&(&right_binding.input_mode, &right_binding.key_sequence))
    });
    match output_format {
        OutputFormat::Json => render_json(&KeysList {
            is_reverted: keymap_view.keymap_verdict != koshi_config::conflict::KeymapVerdict::Apply,
            key_bindings,
        }),
        OutputFormat::Table => render_table(
            KEYS_LIST_HEADERS,
            key_bindings.iter().map(render_key_binding_cells).collect(),
        ),
    }
}

/// Render a `koshi keys describe <key-sequence>` answer: one detail block
/// per `koshi keys list` row whose key is the sequence or starts with it,
/// mode by mode in mode-name order, and in key order within a mode. A user
/// entry comes before the `defaults (unbound)` entry it displaced on the
/// same key.
///
/// - A prefix that binds nothing itself describes the bindings it opens:
///   `<C-t>` describes `<C-t> n` and `<C-t> x`.
/// - A `defaults (unbound)` entry names the user entry that displaced it in
///   `displaced_by`: `user bind` when a user binding took the key, `user
///   remove` when a `remove` cleared it. Every other entry prints `-` there
///   in a table and `null` in JSON.
///
/// # Errors
/// The parser's message when `key_sequence_text` is not a valid key sequence; `Ok(None)`
/// when it parses but no `koshi keys list` row in any mode has the sequence
/// as its key or as the start of its key.
pub fn render_keys_describe(
    keymap_view: &crate::keymap::KeymapView,
    key_sequence_text: &str,
    output_format: OutputFormat,
) -> Result<Option<String>, String> {
    let parsed_key_sequence = koshi_config::key_sequence::parse_sequence(
        key_sequence_text,
        keymap_view.keybindings_config.leader,
        keymap_view.keybindings_config.maximum_chord_depth,
    )
    .map_err(|parse_error| parse_error.to_string())?;

    let mut key_binding_details: Vec<KeyBindingDetail> = Vec::new();
    for (mode_name, merged_mode_keymap) in &keymap_view.merged_keymap.mode_keymap_by_name {
        let user_bindings = list_bindings_starting_with(
            &merged_mode_keymap.user_bindings_by_key_sequence,
            &parsed_key_sequence,
        )
        .map(|(key_sequence, merged_binding)| {
            (
                key_sequence,
                &merged_binding.bound_action,
                merged_binding.layer_origin.to_string(),
                None,
            )
        });
        let default_bindings = list_bindings_starting_with(
            &merged_mode_keymap.default_bindings_by_key_sequence,
            &parsed_key_sequence,
        )
        .map(|(key_sequence, default_binding)| {
            (key_sequence, default_binding, "defaults".to_string(), None)
        });
        let displaced_default_bindings = list_bindings_starting_with(
            &merged_mode_keymap.unbound_default_bindings_by_key_sequence,
            &parsed_key_sequence,
        )
        .map(|(key_sequence, displaced_default_binding)| {
            (
                key_sequence,
                &displaced_default_binding.bound_action,
                "defaults (unbound)".to_string(),
                Some(format_default_displacement_label(
                    displaced_default_binding.default_displacement,
                )),
            )
        });
        // A stable sort keeps a user entry ahead of the default it displaced.
        let mut mode_bindings: Vec<_> = user_bindings
            .chain(default_bindings)
            .chain(displaced_default_bindings)
            .collect();
        mode_bindings.sort_by_key(|(key_sequence, _, _, _)| *key_sequence);

        for (key_sequence, matched_binding, binding_source, displaced_by) in mode_bindings {
            let action_metadata = keymap_view
                .action_registry
                .find_action_metadata(&matched_binding.action_reference);
            key_binding_details.push(KeyBindingDetail {
                key_sequence: key_sequence.to_string(),
                input_mode: mode_name.get_name().to_string(),
                action_reference: matched_binding.action_reference.to_string(),
                display_name: action_metadata.map_or(String::new(), |action_metadata_entry| {
                    action_metadata_entry.display_name.clone()
                }),
                description: action_metadata.map_or(String::new(), |action_metadata_entry| {
                    action_metadata_entry.description.clone()
                }),
                scope: action_metadata
                    .map_or("-", |action_metadata_entry| {
                        format_scope_label(action_metadata_entry.scope)
                    })
                    .to_string(),
                binding_source,
                displaced_by,
                is_continuous: action_metadata
                    .is_some_and(|action_metadata_entry| action_metadata_entry.is_continuous),
            });
        }
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

/// Render a `koshi keys conflicts` answer from the offline keymap view: the
/// view's [`keymap_verdict`](crate::keymap::KeymapView::keymap_verdict) and
/// every finding. A user file that cannot be read or parsed adds its error on
/// both formats: the table opens with `file: ignored (<error>)`, and the
/// verdict reads `reject`.
#[must_use]
pub fn render_keys_conflicts(
    keymap_view: &crate::keymap::KeymapView,
    output_format: OutputFormat,
) -> String {
    let conflict_findings = build_conflict_findings(&keymap_view.conflict_report);
    let conflicts_answer = KeysConflicts {
        verdict: format_keymap_verdict_label(keymap_view.keymap_verdict).to_string(),
        file_error: keymap_view.keybinding_file_error_message.clone(),
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
        crate::keymap::KeymapValidationOutcome::Checked(conflict_report) => KeysValidation {
            is_valid: true,
            is_applicable: is_validation_applicable(validation_outcome),
            parse_errors: Vec::new(),
            conflict_findings: build_conflict_findings(conflict_report),
        },
    };
    match output_format {
        OutputFormat::Json => render_json(&validation_answer),
        OutputFormat::Table => {
            let mut rendered_output = String::new();
            if validation_answer.is_valid {
                rendered_output.push_str(if validation_answer.is_applicable {
                    "valid: koshi would apply this file as keybinding.kdl\n"
                } else {
                    "invalid: koshi would refuse this file as keybinding.kdl\n"
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

/// Whether a client would apply the validated file as its `keybinding.kdl`:
/// the file parsed and its conflict verdict is `apply`.
#[must_use]
pub fn is_validation_applicable(
    validation_outcome: &crate::keymap::KeymapValidationOutcome,
) -> bool {
    match validation_outcome {
        crate::keymap::KeymapValidationOutcome::ParseFailed(_) => false,
        crate::keymap::KeymapValidationOutcome::Checked(conflict_report) => {
            conflict_report.get_verdict() == koshi_config::conflict::KeymapVerdict::Apply
        }
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
        key_binding_detail.binding_source.clone(),
        key_binding_detail
            .displaced_by
            .clone()
            .unwrap_or_else(|| "-".to_string()),
        key_binding_detail.is_continuous.to_string(),
    ]
}

/// The `displaced_by` label of one displaced default: the displacing layer,
/// then `bind` or `remove`. A `remove` in the user layer is `user remove`.
fn format_default_displacement_label(
    default_displacement: koshi_config::keymap_merge::DefaultDisplacement,
) -> String {
    match default_displacement {
        koshi_config::keymap_merge::DefaultDisplacement::Rebound { layer_origin } => {
            format!("{layer_origin} bind")
        }
        koshi_config::keymap_merge::DefaultDisplacement::Removed { layer_origin } => {
            format!("{layer_origin} remove")
        }
    }
}

/// Append the findings table to `rendered_output`, or nothing when there are no
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
    conflict_report: &koshi_config::conflict::ConflictReport,
) -> Vec<ConflictFinding> {
    conflict_report
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
        koshi_config::conflict::ConflictSeverity::Fatal => "fatal",
    }
}

/// The stable label of one keymap verdict.
fn format_keymap_verdict_label(
    keymap_verdict: koshi_config::conflict::KeymapVerdict,
) -> &'static str {
    match keymap_verdict {
        koshi_config::conflict::KeymapVerdict::Apply => "apply",
        koshi_config::conflict::KeymapVerdict::Reject => "reject",
    }
}

/// The keymap layer a [`KeymapScope`] filter keeps.
fn resolve_scope_layer_origin(scope_argument: KeymapScope) -> koshi_config::conflict::LayerOrigin {
    match scope_argument {
        KeymapScope::Default => koshi_config::conflict::LayerOrigin::Defaults,
        KeymapScope::User => koshi_config::conflict::LayerOrigin::User,
    }
}

/// Every entry of `bindings_by_key_sequence` whose key is `key_sequence` or
/// starts with it, in key order. With `<C-t> n` and `<C-t> x` bound, `<C-t>`
/// yields both, and `<C-t> n` yields only itself.
///
/// The walk starts at `key_sequence` and stops at the first key that does not
/// start with it.
fn list_bindings_starting_with<'a, Binding>(
    bindings_by_key_sequence: &'a std::collections::BTreeMap<koshi_core::key::KeySequence, Binding>,
    key_sequence: &'a koshi_core::key::KeySequence,
) -> impl Iterator<Item = (&'a koshi_core::key::KeySequence, &'a Binding)> {
    bindings_by_key_sequence
        .range(key_sequence..)
        .take_while(|(candidate_key_sequence, _)| {
            candidate_key_sequence
                .list_chords()
                .starts_with(key_sequence.list_chords())
        })
}
