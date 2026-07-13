//! Rendering for read-only query answers: discovery (`list-*`, `inspect`),
//! action introspection (`actions list`, `actions explain`), and keymap
//! introspection (the `keys` queries). Each prints as aligned columns
//! (`--format table`, the default) or JSON (`--format json`).
//!
//! List queries render every item as one table row; `inspect`, `actions
//! explain`, and `keys describe` render a single item as `field: value`
//! lines. JSON output is the serde form of the [`koshi_core::discovery`]
//! structs and this module's summary/detail structs — a JSON array for a
//! list, a JSON object for a single item — and is the stable scripting
//! surface. In table cells an absent value prints as `-`, an id list prints
//! as its count (full ids are in the JSON form), and a timestamp prints as
//! whole seconds since the Unix epoch.

use std::time::SystemTime;

use koshi_core::action::{
    core_action_seeds, ActionHandlerRef, ActionMetadata, ActionRef, ActionScope, ActionStatus,
    TargetKind,
};
use koshi_core::discovery::{ClientInfo, PaneInfo, PaneState, SessionInfo, TabInfo};
use koshi_core::geometry::{Rect, Size};
use serde::Serialize;

use crate::cli::{FormatArg, ScopeArg};

/// Render a `list-sessions` answer.
#[must_use]
pub fn render_sessions(sessions: &[SessionInfo], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&sessions),
        FormatArg::Table => table(SESSION_HEADERS, sessions.iter().map(session_row).collect()),
    }
}

/// Render an `inspect session` answer.
#[must_use]
pub fn render_session(session: &SessionInfo, format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(session),
        FormatArg::Table => fields(SESSION_HEADERS, session_row(session)),
    }
}

/// Render a `list-tabs` answer.
#[must_use]
pub fn render_tabs(tabs: &[TabInfo], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&tabs),
        FormatArg::Table => table(TAB_HEADERS, tabs.iter().map(tab_row).collect()),
    }
}

/// Render an `inspect tab` answer.
#[must_use]
pub fn render_tab(tab: &TabInfo, format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(tab),
        FormatArg::Table => fields(TAB_HEADERS, tab_row(tab)),
    }
}

/// Render a `list-panes` answer.
#[must_use]
pub fn render_panes(panes: &[PaneInfo], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&panes),
        FormatArg::Table => table(PANE_HEADERS, panes.iter().map(pane_row).collect()),
    }
}

/// Render an `inspect pane` answer.
#[must_use]
pub fn render_pane(pane: &PaneInfo, format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(pane),
        FormatArg::Table => fields(PANE_HEADERS, pane_row(pane)),
    }
}

/// Render a `list-clients` answer.
#[must_use]
pub fn render_clients(clients: &[ClientInfo], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&clients),
        FormatArg::Table => table(CLIENT_HEADERS, clients.iter().map(client_row).collect()),
    }
}

/// Render an `inspect client` answer.
#[must_use]
pub fn render_client(client: &ClientInfo, format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(client),
        FormatArg::Table => fields(CLIENT_HEADERS, client_row(client)),
    }
}

/// Column headers for [`SessionInfo`] rows, matching [`session_row`] order.
const SESSION_HEADERS: &[&str] = &["id", "name", "created_at", "clients", "panes"];

/// Column headers for [`TabInfo`] rows, matching [`tab_row`] order.
const TAB_HEADERS: &[&str] = &["id", "name", "index", "active_pane", "panes"];

/// Column headers for [`PaneInfo`] rows, matching [`pane_row`] order.
const PANE_HEADERS: &[&str] = &[
    "id",
    "tab",
    "session",
    "title",
    "cwd",
    "command",
    "state",
    "focused_by",
    "rect",
];

/// Column headers for [`ClientInfo`] rows, matching [`client_row`] order.
const CLIENT_HEADERS: &[&str] = &[
    "id",
    "session",
    "attached_at",
    "viewport",
    "active_tab",
    "focused_pane",
    "lock",
];

/// One [`SessionInfo`] as table cells, in [`SESSION_HEADERS`] order.
fn session_row(session: &SessionInfo) -> Vec<String> {
    vec![
        session.id.to_string(),
        session.name.clone(),
        time_cell(session.created_at),
        session.attached_clients.len().to_string(),
        session.pane_count.to_string(),
    ]
}

/// One [`TabInfo`] as table cells, in [`TAB_HEADERS`] order.
fn tab_row(tab: &TabInfo) -> Vec<String> {
    vec![
        tab.id.to_string(),
        tab.name.clone(),
        tab.index.to_string(),
        opt_cell(tab.active_pane.as_ref()),
        tab.pane_count.to_string(),
    ]
}

/// One [`PaneInfo`] as table cells, in [`PANE_HEADERS`] order.
fn pane_row(pane: &PaneInfo) -> Vec<String> {
    vec![
        pane.id.to_string(),
        pane.tab_id.to_string(),
        pane.session_id.to_string(),
        opt_cell(pane.title.as_ref()),
        match &pane.cwd {
            Some(cwd) => cwd.display().to_string(),
            None => "-".to_string(),
        },
        match &pane.command {
            Some(argv) => argv.join(" "),
            None => "-".to_string(),
        },
        state_cell(pane.state),
        pane.focused_by_clients.len().to_string(),
        match pane.layout_rect {
            Some(rect) => rect_cell(rect),
            None => "-".to_string(),
        },
    ]
}

/// One [`ClientInfo`] as table cells, in [`CLIENT_HEADERS`] order.
fn client_row(client: &ClientInfo) -> Vec<String> {
    vec![
        client.id.to_string(),
        client.session_id.to_string(),
        time_cell(client.attached_at),
        size_cell(client.viewport_size),
        client.active_tab.to_string(),
        opt_cell(client.focused_pane.as_ref()),
        format!("{:?}", client.lock_state),
    ]
}

/// An optional value as a cell: its display form, or `-` when absent.
fn opt_cell<T: std::fmt::Display>(value: Option<&T>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "-".to_string(),
    }
}

/// A timestamp as a cell: whole seconds since the Unix epoch.
fn time_cell(time: SystemTime) -> String {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs().to_string(),
        Err(_) => "-".to_string(),
    }
}

/// A size as a cell: `<cols>x<rows>`.
fn size_cell(size: Size) -> String {
    format!("{}x{}", size.cols, size.rows)
}

/// A rectangle as a cell: `<cols>x<rows>@<x>,<y>`.
fn rect_cell(rect: Rect) -> String {
    format!(
        "{}x{}@{},{}",
        rect.size.cols, rect.size.rows, rect.origin.x, rect.origin.y
    )
}

/// A pane state as a cell: its lowercase name, with the exit code appended
/// as `exited(<code>)` when one was observed and `exited(-)` when not.
fn state_cell(state: PaneState) -> String {
    match state {
        PaneState::Spawning => "spawning".to_string(),
        PaneState::Running => "running".to_string(),
        PaneState::Exited { code: Some(code) } => format!("exited({code})"),
        PaneState::Exited { code: None } => "exited(-)".to_string(),
        PaneState::Closing => "closing".to_string(),
    }
}

/// The pretty-printed JSON form of `value`, ending in a newline.
fn json<T: Serialize>(value: &T) -> String {
    let mut rendered = serde_json::to_string_pretty(value).expect(
        "output structs serialize: strings are valid, paths render lossily, clocks post-epoch",
    );
    rendered.push('\n');
    rendered
}

/// Aligned columns: a header row, then one row per item, each column padded
/// to its widest cell and separated by two spaces, with no trailing spaces.
fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    // Each column's width is the widest cell in that column, starting from
    // the header's own width.
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut rendered = String::new();
    let header_cells: Vec<String> = headers.iter().map(|header| (*header).to_string()).collect();
    // Render the header first, then every data row, using the same padding logic.
    for row in std::iter::once(&header_cells).chain(rows.iter()) {
        let mut line = String::new();
        for (index, (cell, width)) in row.iter().zip(&widths).enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            let padding = width.saturating_sub(cell.chars().count());
            // Pad every cell except the last, whose trailing spaces get
            // trimmed off the line below anyway.
            if index < row.len() - 1 {
                line.extend(std::iter::repeat_n(' ', padding));
            }
        }
        rendered.push_str(line.trim_end());
        rendered.push('\n');
    }
    rendered
}

/// A single item as `field: value` lines, one per header.
fn fields(headers: &[&str], row: Vec<String>) -> String {
    let mut rendered = String::new();
    for (header, cell) in headers.iter().zip(row) {
        rendered.push_str(header);
        rendered.push_str(": ");
        rendered.push_str(&cell);
        rendered.push('\n');
    }
    rendered
}

// --- Action registry introspection ---

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
fn join_cell(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

/// An action scope as its kebab-case label.
fn scope_label(scope: ActionScope) -> &'static str {
    match scope {
        ActionScope::PaneSession => "pane-session",
        ActionScope::Client => "client",
        ActionScope::Tab => "tab",
        ActionScope::Global => "global",
    }
}

/// A target kind as its lowercase label.
fn target_label(target: TargetKind) -> &'static str {
    match target {
        TargetKind::Session => "session",
        TargetKind::Tab => "tab",
        TargetKind::Pane => "pane",
        TargetKind::Client => "client",
    }
}

/// The internal command an action dispatches, as a label: the core command's
/// name, `plugin-host` for a plugin call, or `sequence` for a macro.
fn command_label(handler: &ActionHandlerRef) -> String {
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
            if !answer.findings.is_empty() {
                rendered.push_str(&table(
                    KEYS_CONFLICTS_HEADERS,
                    answer.findings.iter().map(conflict_finding_row).collect(),
                ));
            }
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
                if !answer.findings.is_empty() {
                    rendered.push_str(&table(
                        KEYS_CONFLICTS_HEADERS,
                        answer.findings.iter().map(conflict_finding_row).collect(),
                    ));
                }
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

#[cfg(test)]
mod tests;
