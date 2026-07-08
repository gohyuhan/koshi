//! Rendering for read-only query answers: discovery (`list-*`, `inspect`) and
//! action introspection (`actions list`, `actions explain`). Each prints as
//! aligned columns (`--format table`, the default) or JSON (`--format json`).
//!
//! List queries render every item as one table row; `inspect` and `actions
//! explain` render a single item as `field: value` lines. JSON output is the
//! serde form of the [`koshi_core::discovery`] structs and the action
//! summary/detail structs — a JSON array for a list, a JSON object for a single
//! item — and is the stable scripting surface. In table cells an absent value
//! prints as `-`, an id list prints as its count (full ids are in the JSON
//! form), and a timestamp prints as whole seconds since the Unix epoch.

use std::time::SystemTime;

use koshi_core::action::{
    core_action_seeds, ActionHandlerRef, ActionMetadata, ActionRef, ActionScope, ActionStatus,
    TargetKind,
};
use koshi_core::discovery::{ClientInfo, PaneInfo, PaneState, SessionInfo, TabInfo};
use koshi_core::geometry::{Rect, Size};
use serde::Serialize;

use crate::cli::FormatArg;

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
    for row in std::iter::once(&header_cells).chain(rows.iter()) {
        let mut line = String::new();
        for (index, (cell, width)) in row.iter().zip(&widths).enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            let padding = width.saturating_sub(cell.chars().count());
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

#[cfg(test)]
mod tests;
