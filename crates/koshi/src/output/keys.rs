//! Renderers for `koshi keys`: binding lists, per-binding detail,
//! conflict reports, and keymap-file validation.

use super::*;

/// Column headers for a `keys list` row, matching [`key_binding_row`].
const KEYS_LIST_HEADERS: &[&str] = &["mode", "key", "action", "source"];

/// Field headers for a `keys describe` answer, matching [`key_detail_row`].
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
    mode: String,
    /// The key sequence, in the angle grammar.
    key: String,
    /// The action reference the key fires.
    action: String,
    /// The layer that authored the winning entry: `defaults`, `user`,
    /// `session`, or `layout` — or `defaults (unbound)` for a shipped
    /// binding a user surface displaced.
    source: String,
}

/// A whole `keys list` answer.
#[derive(Serialize)]
struct KeysList {
    /// True when a user keybinding file exists but was not admitted, so the
    /// listing shows the built-in defaults.
    reverted: bool,
    /// Every effective binding, then every displaced default.
    bindings: Vec<KeyBindingSummary>,
}

/// One binding as it appears in a `keys describe` answer.
#[derive(Serialize)]
struct KeyBindingDetail {
    /// The key sequence, in the angle grammar.
    key: String,
    /// The input mode this entry fires in.
    mode: String,
    /// The action reference the key fires.
    action: String,
    /// The action's human-facing name.
    display_name: String,
    /// The action's one-line description.
    description: String,
    /// How broad the action's effect is.
    scope: String,
    /// The preset arguments bound with the action, `null` when none.
    args: serde_json::Value,
    /// The layer that authored the winning entry.
    source: String,
    /// Whether the action re-arms its prefix when fired from a multi-chord
    /// binding.
    continuous: bool,
}

/// One conflict-detection finding as it appears in a `keys conflicts` or
/// `keys validate` answer.
#[derive(Serialize)]
struct ConflictFinding {
    /// The finding's weight: `warning`, `collision`, or `fatal`.
    severity: String,
    /// The user-facing message.
    finding: String,
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
    findings: Vec<ConflictFinding>,
}

/// A whole `keys validate` answer.
#[derive(Serialize)]
struct KeysValidation {
    /// True when the file parsed as valid keybinding KDL.
    valid: bool,
    /// True when a reload would apply the file.
    applies: bool,
    /// Parse problems, one per line, when the file did not parse.
    errors: Vec<String>,
    /// Conflict-detection findings, when the file parsed.
    findings: Vec<ConflictFinding>,
}

/// Render a `koshi keys list` answer from the offline keymap view.
#[must_use]
pub fn render_keys_list(
    view: &crate::keymap::KeymapView,
    mode: Option<&str>,
    scope: Option<ScopeArg>,
    format: FormatArg,
) -> String {
    let scope_label = scope.map(scope_arg_label);
    let mut bindings: Vec<KeyBindingSummary> = Vec::new();
    for (mode_name, merged) in &view.merged.modes {
        if mode.is_some_and(|wanted| wanted != mode_name.as_str()) {
            continue;
        }
        for (sequence, binding) in &merged.user_set {
            bindings.push(KeyBindingSummary {
                mode: mode_name.as_str().to_string(),
                key: sequence.to_string(),
                action: binding.bound.action.to_string(),
                source: binding.source.to_string(),
            });
        }
        for (sequence, bound) in &merged.defaults {
            bindings.push(KeyBindingSummary {
                mode: mode_name.as_str().to_string(),
                key: sequence.to_string(),
                action: bound.action.to_string(),
                source: "defaults".to_string(),
            });
        }
        for (sequence, bound) in &merged.unbound_defaults {
            bindings.push(KeyBindingSummary {
                mode: mode_name.as_str().to_string(),
                key: sequence.to_string(),
                action: bound.action.to_string(),
                source: "defaults (unbound)".to_string(),
            });
        }
    }
    if let Some(wanted) = scope_label {
        bindings.retain(|binding| binding.source == wanted);
    }
    bindings.sort_by(|a, b| (&a.mode, &a.key).cmp(&(&b.mode, &b.key)));
    match format {
        FormatArg::Json => json(&KeysList {
            reverted: view.reverted,
            bindings,
        }),
        FormatArg::Table => table(
            KEYS_LIST_HEADERS,
            bindings.iter().map(key_binding_row).collect(),
        ),
    }
}

/// Render a `koshi keys list --recommended` answer. Plugin-recommended
/// bindings come from installed plugin manifests; none exist until plugins
/// do, so the listing is empty.
#[must_use]
pub fn render_keys_recommended(format: FormatArg) -> String {
    let recommended: Vec<KeyBindingSummary> = Vec::new();
    match format {
        FormatArg::Json => json(&recommended),
        FormatArg::Table => table(KEYS_RECOMMENDED_HEADERS, Vec::new()),
    }
}

/// Render a `koshi keys describe <key-sequence>` answer: one detail block
/// per mode the sequence is bound in.
///
/// # Errors
/// The parser's message when `sequence` is not a valid key sequence; `Ok(None)`
/// when it parses but nothing is bound on it in any mode.
pub fn render_keys_describe(
    view: &crate::keymap::KeymapView,
    sequence: &str,
    format: FormatArg,
) -> Result<Option<String>, String> {
    let parsed = koshi_config::key_sequence::parse_sequence(
        sequence,
        view.config.leader,
        view.config.max_chord_depth,
    )
    .map_err(|err| err.to_string())?;

    let mut details: Vec<KeyBindingDetail> = Vec::new();
    for (mode_name, merged) in &view.merged.modes {
        let (bound, source) = if let Some(binding) = merged.user_set.get(&parsed) {
            (&binding.bound, binding.source.to_string())
        } else if let Some(bound) = merged.defaults.get(&parsed) {
            (bound, "defaults".to_string())
        } else {
            continue;
        };
        let metadata = view.registry.lookup(&bound.action);
        let args = if bound.args == koshi_core::resolve::ActionArgs::None {
            serde_json::Value::Null
        } else {
            serde_json::to_value(&bound.args)
                .expect("action args serialize: plain enums and strings")
        };
        details.push(KeyBindingDetail {
            key: parsed.to_string(),
            mode: mode_name.as_str().to_string(),
            action: bound.action.to_string(),
            display_name: metadata.map_or(String::new(), |m| m.display_name.clone()),
            description: metadata.map_or(String::new(), |m| m.description.clone()),
            scope: metadata
                .map_or("-", |m| scope_label(m.scope_class))
                .to_string(),
            args,
            source,
            continuous: metadata.is_some_and(|m| m.continuous),
        });
    }
    if details.is_empty() {
        return Ok(None);
    }
    Ok(Some(match format {
        FormatArg::Json => json(&details),
        FormatArg::Table => {
            let mut rendered = String::new();
            for (index, detail) in details.iter().enumerate() {
                if index > 0 {
                    rendered.push('\n');
                }
                rendered.push_str(&fields(KEYS_DETAIL_HEADERS, key_detail_row(detail)));
            }
            rendered
        }
    }))
}

/// Render a `koshi keys conflicts` answer from the offline keymap view. An
/// ignored user file (unreadable or unparseable) is part of the answer on
/// both formats, so a consumer reading only stdout never mistakes a
/// defaults-only "apply" for a clean file.
#[must_use]
pub fn render_keys_conflicts(view: &crate::keymap::KeymapView, format: FormatArg) -> String {
    let findings = conflict_findings(&view.report);
    let answer = KeysConflicts {
        verdict: verdict_label(view.report.verdict()).to_string(),
        file_error: view.file_error.clone(),
        findings,
    };
    match format {
        FormatArg::Json => json(&answer),
        FormatArg::Table => {
            let mut rendered = String::new();
            if let Some(error) = &answer.file_error {
                rendered.push_str("file: ignored (");
                rendered.push_str(error);
                rendered.push_str(")\n");
            }
            rendered.push_str(&format!("verdict: {}\n", answer.verdict));
            push_conflict_table(&mut rendered, &answer.findings);
            rendered
        }
    }
}

/// Render a `koshi keys validate <path>` answer.
#[must_use]
pub fn render_keys_validate(
    outcome: &crate::keymap::ValidationOutcome,
    format: FormatArg,
) -> String {
    let answer = match outcome {
        crate::keymap::ValidationOutcome::ParseFailed(errors) => KeysValidation {
            valid: false,
            applies: false,
            errors: errors.clone(),
            findings: Vec::new(),
        },
        crate::keymap::ValidationOutcome::Checked { report, applies } => KeysValidation {
            valid: true,
            applies: *applies,
            errors: Vec::new(),
            findings: conflict_findings(report),
        },
    };
    match format {
        FormatArg::Json => json(&answer),
        FormatArg::Table => {
            let mut rendered = String::new();
            if answer.valid {
                rendered.push_str(if answer.applies {
                    "valid: a reload would apply this file\n"
                } else {
                    "invalid: a reload would keep the running keymap\n"
                });
                push_conflict_table(&mut rendered, &answer.findings);
            } else {
                rendered.push_str("invalid: the file does not parse\n");
                for error in &answer.errors {
                    rendered.push_str("error: ");
                    rendered.push_str(error);
                    rendered.push('\n');
                }
            }
            rendered
        }
    }
}

/// Whether a rendered validation answer reports a file a reload would apply.
#[must_use]
pub fn validation_applies(outcome: &crate::keymap::ValidationOutcome) -> bool {
    match outcome {
        crate::keymap::ValidationOutcome::ParseFailed(_) => false,
        crate::keymap::ValidationOutcome::Checked { applies, .. } => *applies,
    }
}

/// One [`KeyBindingSummary`] as table cells, in [`KEYS_LIST_HEADERS`] order.
fn key_binding_row(binding: &KeyBindingSummary) -> Vec<String> {
    vec![
        binding.mode.clone(),
        binding.key.clone(),
        binding.action.clone(),
        binding.source.clone(),
    ]
}

/// One [`KeyBindingDetail`] as field cells, in [`KEYS_DETAIL_HEADERS`] order.
fn key_detail_row(detail: &KeyBindingDetail) -> Vec<String> {
    vec![
        detail.key.clone(),
        detail.mode.clone(),
        detail.action.clone(),
        detail.display_name.clone(),
        detail.description.clone(),
        detail.scope.clone(),
        if detail.args.is_null() {
            "-".to_string()
        } else {
            detail.args.to_string()
        },
        detail.source.clone(),
        detail.continuous.to_string(),
    ]
}

/// Append the findings table to `rendered`, or nothing when there are no
/// findings. The one table shape every keys-conflict renderer shares.
fn push_conflict_table(rendered: &mut String, findings: &[ConflictFinding]) {
    if !findings.is_empty() {
        rendered.push_str(&table(
            KEYS_CONFLICTS_HEADERS,
            findings.iter().map(conflict_finding_row).collect(),
        ));
    }
}

/// One [`ConflictFinding`] as table cells, in [`KEYS_CONFLICTS_HEADERS`] order.
fn conflict_finding_row(finding: &ConflictFinding) -> Vec<String> {
    vec![finding.severity.clone(), finding.finding.clone()]
}

/// Every report finding as a [`ConflictFinding`], in report order.
fn conflict_findings(report: &koshi_config::conflict::ConflictReport) -> Vec<ConflictFinding> {
    report
        .diagnostics
        .iter()
        .map(|diagnostic| ConflictFinding {
            severity: severity_label(diagnostic.severity()).to_string(),
            finding: diagnostic.to_string(),
        })
        .collect()
}

/// The stable label of one severity tier.
fn severity_label(severity: koshi_config::conflict::ConflictSeverity) -> &'static str {
    match severity {
        koshi_config::conflict::ConflictSeverity::Warning => "warning",
        koshi_config::conflict::ConflictSeverity::Collision => "collision",
        koshi_config::conflict::ConflictSeverity::Fatal => "fatal",
    }
}

/// The stable label of one keymap verdict.
fn verdict_label(verdict: koshi_config::conflict::KeymapVerdict) -> &'static str {
    match verdict {
        koshi_config::conflict::KeymapVerdict::Apply => "apply",
        koshi_config::conflict::KeymapVerdict::RevertToDefaults => "revert-to-defaults",
        koshi_config::conflict::KeymapVerdict::Reject => "reject",
    }
}

/// The `source` label a [`ScopeArg`] filter matches.
fn scope_arg_label(scope: ScopeArg) -> &'static str {
    match scope {
        ScopeArg::Default => "defaults",
        ScopeArg::User => "user",
        ScopeArg::Session => "session",
        ScopeArg::Layout => "layout",
    }
}
