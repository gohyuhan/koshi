//! Table and JSON renderers for the entity listings: sessions, tabs,
//! panes, and clients.

use super::*;

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

/// A rectangle as a cell: `<cols>x<rows>@<x>,<y>`.
fn rect_cell(rect: Rect) -> String {
    format!(
        "{}x{}@{},{}",
        rect.size.cols, rect.size.rows, rect.origin.x, rect.origin.y
    )
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
