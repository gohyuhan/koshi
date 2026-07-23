//! Table and JSON renderers for the entity queries: the id-chain listings
//! (`list-sessions`, `list-tabs`, `list-panes`, `list-clients`) and the full
//! `inspect` records.

use super::*;

/// Render a `list-sessions` answer.
#[must_use]
pub fn render_sessions(sessions: &[SessionRow], format: FormatArg) -> String {
    listing(sessions, SESSION_ROW_HEADERS, session_row_cells, format)
}

/// Render an `inspect session` answer.
#[must_use]
pub fn render_session(session: &SessionInfo, format: FormatArg) -> String {
    record(session, SESSION_HEADERS, session_row, format)
}

/// Render a `list-tabs` answer.
#[must_use]
pub fn render_tabs(tabs: &[TabRow], format: FormatArg) -> String {
    listing(tabs, TAB_ROW_HEADERS, tab_row_cells, format)
}

/// Render an `inspect tab` answer.
#[must_use]
pub fn render_tab(tab: &TabInfo, format: FormatArg) -> String {
    record(tab, TAB_HEADERS, tab_row, format)
}

/// Render a `list-panes` answer.
#[must_use]
pub fn render_panes(panes: &[PaneRow], format: FormatArg) -> String {
    listing(panes, PANE_ROW_HEADERS, pane_row_cells, format)
}

/// Render an `inspect pane` answer.
#[must_use]
pub fn render_pane(pane: &PaneInfo, format: FormatArg) -> String {
    record(pane, PANE_HEADERS, pane_row, format)
}

/// Render a `list-clients` answer.
#[must_use]
pub fn render_clients(clients: &[ClientRow], format: FormatArg) -> String {
    listing(clients, CLIENT_ROW_HEADERS, client_row_cells, format)
}

/// Render an `inspect client` answer.
#[must_use]
pub fn render_client(client: &ClientInfo, format: FormatArg) -> String {
    record(client, CLIENT_HEADERS, client_row, format)
}

/// A listing answer: a JSON array of `rows`, or a table of one row per item
/// with `headers` above the cells `cells` produces.
fn listing<T: Serialize>(
    rows: &[T],
    headers: &[&str],
    cells: fn(&T) -> Vec<String>,
    format: FormatArg,
) -> String {
    match format {
        FormatArg::Json => json(&rows),
        FormatArg::Table => table(headers, rows.iter().map(cells).collect()),
    }
}

/// A single-item answer: `item` as a JSON object, or as one `field: value`
/// line per header, valued by `cells`.
fn record<T: Serialize>(
    item: &T,
    headers: &[&str],
    cells: fn(&T) -> Vec<String>,
    format: FormatArg,
) -> String {
    match format {
        FormatArg::Json => json(item),
        FormatArg::Table => fields(headers, cells(item)),
    }
}

/// Column headers for [`SessionRow`] listings, matching [`session_row_cells`].
const SESSION_ROW_HEADERS: &[&str] = &["id", "name"];

/// Column headers for [`TabRow`] listings, matching [`tab_row_cells`].
const TAB_ROW_HEADERS: &[&str] = &["id", "name", "session", "session_name"];

/// Column headers for [`PaneRow`] listings, matching [`pane_row_cells`].
const PANE_ROW_HEADERS: &[&str] = &["id", "name", "tab", "tab_name", "session", "session_name"];

/// Column headers for [`ClientRow`] listings, matching [`client_row_cells`].
const CLIENT_ROW_HEADERS: &[&str] = &["id", "session", "session_name"];

/// Field names for an `inspect session`, matching [`session_row`] order.
const SESSION_HEADERS: &[&str] = &["id", "name", "created_at", "clients", "panes"];

/// Field names for an `inspect tab`, matching [`tab_row`] order.
const TAB_HEADERS: &[&str] = &["id", "session", "name", "index", "active_pane", "panes"];

/// Field names for an `inspect pane`, matching [`pane_row`] order.
const PANE_HEADERS: &[&str] = &[
    "id",
    "tab",
    "session",
    "title",
    "cwd",
    "command",
    "state",
    "focused_by",
];

/// Field names for an `inspect client`, matching [`client_row`] order.
const CLIENT_HEADERS: &[&str] = &[
    "id",
    "session",
    "attached_at",
    "viewport",
    "active_tab",
    "focused_pane",
    "lock",
];

/// One [`SessionRow`] as table cells, in [`SESSION_ROW_HEADERS`] order.
fn session_row_cells(session: &SessionRow) -> Vec<String> {
    vec![session.id.to_string(), session.name.clone()]
}

/// One [`TabRow`] as table cells, in [`TAB_ROW_HEADERS`] order.
fn tab_row_cells(tab: &TabRow) -> Vec<String> {
    vec![
        tab.id.to_string(),
        tab.name.clone(),
        tab.session.to_string(),
        tab.session_name.clone(),
    ]
}

/// One [`PaneRow`] as table cells, in [`PANE_ROW_HEADERS`] order. A pane the
/// child never titled prints `-`.
fn pane_row_cells(pane: &PaneRow) -> Vec<String> {
    vec![
        pane.id.to_string(),
        opt_cell(pane.name.as_ref()),
        pane.tab.to_string(),
        pane.tab_name.clone(),
        pane.session.to_string(),
        pane.session_name.clone(),
    ]
}

/// One [`ClientRow`] as table cells, in [`CLIENT_ROW_HEADERS`] order.
fn client_row_cells(client: &ClientRow) -> Vec<String> {
    vec![
        client.id.to_string(),
        client.session.to_string(),
        client.session_name.clone(),
    ]
}

/// One [`SessionInfo`] as field values, in [`SESSION_HEADERS`] order.
fn session_row(session: &SessionInfo) -> Vec<String> {
    vec![
        session.id.to_string(),
        session.name.clone(),
        time_cell(session.created_at),
        session.attached_clients.len().to_string(),
        session.pane_count.to_string(),
    ]
}

/// One [`TabInfo`] as field values, in [`TAB_HEADERS`] order.
fn tab_row(tab: &TabInfo) -> Vec<String> {
    vec![
        tab.id.to_string(),
        tab.session_id.to_string(),
        tab.name.clone(),
        tab.index.to_string(),
        opt_cell(tab.active_pane.as_ref()),
        tab.pane_count.to_string(),
    ]
}

/// One [`PaneInfo`] as field values, in [`PANE_HEADERS`] order.
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
    ]
}

/// One [`ClientInfo`] as field values, in [`CLIENT_HEADERS`] order.
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
pub(super) fn time_cell(time: SystemTime) -> String {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs().to_string(),
        Err(_) => "-".to_string(),
    }
}

/// A size as a cell: `<cols>x<rows>`.
fn size_cell(size: Size) -> String {
    format!("{}x{}", size.cols, size.rows)
}

/// A pane state as a cell: its lowercase name, with the exit code appended
/// as `exited(<code>)` when one was observed and `exited(-)` when not.
pub(super) fn state_cell(state: PaneState) -> String {
    match state {
        PaneState::Spawning => "spawning".to_string(),
        PaneState::Running => "running".to_string(),
        PaneState::Exited { code: Some(code) } => format!("exited({code})"),
        PaneState::Exited { code: None } => "exited(-)".to_string(),
        PaneState::Closing => "closing".to_string(),
    }
}
