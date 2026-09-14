//! Table and JSON renderers for the entity queries: the id-chain listings
//! (`list-sessions`, `list-tabs`, `list-panes`, `list-clients`), the full
//! `inspect` records, and the `debug dump-state` dump of those same records
//! across every running session.

use super::*;
use koshi_core::geometry::PaneArea;

/// Render a `list-sessions` answer.
#[must_use]
pub fn render_sessions(session_rows: &[SessionRow], output_format: OutputFormat) -> String {
    render_listing(
        session_rows,
        SESSION_ROW_HEADERS,
        session_row_cells,
        output_format,
    )
}

/// Render an `inspect session` answer.
#[must_use]
pub fn render_session(session_discovery: &SessionDiscovery, output_format: OutputFormat) -> String {
    render_entity_record(
        session_discovery,
        SESSION_HEADERS,
        render_session_fields,
        output_format,
    )
}

/// Render a `list-tabs` answer.
#[must_use]
pub fn render_tabs(tab_rows: &[TabRow], output_format: OutputFormat) -> String {
    render_listing(tab_rows, TAB_ROW_HEADERS, tab_row_cells, output_format)
}

/// Render an `inspect tab` answer.
#[must_use]
pub fn render_tab(tab_discovery: &TabDiscovery, output_format: OutputFormat) -> String {
    render_entity_record(tab_discovery, TAB_HEADERS, render_tab_fields, output_format)
}

/// Render a `list-panes` answer.
#[must_use]
pub fn render_panes(pane_rows: &[PaneRow], output_format: OutputFormat) -> String {
    render_listing(pane_rows, PANE_ROW_HEADERS, pane_row_cells, output_format)
}

/// Render an `inspect pane` answer.
#[must_use]
pub fn render_pane(pane_discovery: &PaneDiscovery, output_format: OutputFormat) -> String {
    render_entity_record(
        pane_discovery,
        PANE_HEADERS,
        render_pane_fields,
        output_format,
    )
}

/// Render a `list-clients` answer.
#[must_use]
pub fn render_clients(client_rows: &[ClientRow], output_format: OutputFormat) -> String {
    render_listing(
        client_rows,
        CLIENT_ROW_HEADERS,
        client_row_cells,
        output_format,
    )
}

/// Render an `inspect client` answer.
#[must_use]
pub fn render_client(client_discovery: &ClientDiscovery, output_format: OutputFormat) -> String {
    render_entity_record(
        client_discovery,
        CLIENT_HEADERS,
        render_client_fields,
        output_format,
    )
}

/// Render a `debug dump-state` answer: every listed session's own record,
/// then its tabs, panes, and clients, as one table per section under its
/// name.
#[must_use]
pub fn render_dump_state(
    session_overviews: &[SessionOverview],
    output_format: OutputFormat,
) -> String {
    match output_format {
        OutputFormat::Json => render_json(&session_overviews),
        OutputFormat::Table => format!(
            "sessions\n{}\ntabs\n{}\npanes\n{}\nclients\n{}\n",
            render_table(
                SESSION_HEADERS,
                session_overviews
                    .iter()
                    .map(|session_overview| render_session_fields(&session_overview.session))
                    .collect(),
            ),
            render_table(
                TAB_HEADERS,
                session_overviews
                    .iter()
                    .flat_map(|session_overview| session_overview.tabs.iter())
                    .map(render_tab_fields)
                    .collect(),
            ),
            render_table(
                PANE_HEADERS,
                session_overviews
                    .iter()
                    .flat_map(|session_overview| session_overview.panes.iter())
                    .map(render_pane_fields)
                    .collect(),
            ),
            render_table(
                CLIENT_HEADERS,
                session_overviews
                    .iter()
                    .flat_map(|session_overview| session_overview.clients.iter())
                    .map(render_client_fields)
                    .collect(),
            ),
        ),
    }
}

/// A listing answer: a JSON array of `listing_rows`, or a table of one row per
/// row with `column_headers` above the cells `render_row_cells` produces.
fn render_listing<SerializableRow: Serialize>(
    listing_rows: &[SerializableRow],
    column_headers: &[&str],
    render_row_cells: fn(&SerializableRow) -> Vec<String>,
    output_format: OutputFormat,
) -> String {
    match output_format {
        OutputFormat::Json => render_json(&listing_rows),
        OutputFormat::Table => render_table(
            column_headers,
            listing_rows.iter().map(render_row_cells).collect(),
        ),
    }
}

/// A single-record answer: `serializable_record` as a JSON object, or as one
/// `field: value` line per header, valued by `field_values`.
fn render_entity_record<SerializableRecord: Serialize>(
    serializable_record: &SerializableRecord,
    field_headers: &[&str],
    render_field_values: fn(&SerializableRecord) -> Vec<String>,
    output_format: OutputFormat,
) -> String {
    match output_format {
        OutputFormat::Json => render_json(serializable_record),
        OutputFormat::Table => {
            render_fields(field_headers, render_field_values(serializable_record))
        }
    }
}

/// Column headers for [`SessionRow`] listings, matching [`session_row_cells`].
const SESSION_ROW_HEADERS: &[&str] = &["id", "name", "server"];

/// Column headers for [`TabRow`] listings, matching [`tab_row_cells`].
const TAB_ROW_HEADERS: &[&str] = &["id", "name", "session", "session_name"];

/// Column headers for [`PaneRow`] listings, matching [`pane_row_cells`].
const PANE_ROW_HEADERS: &[&str] = &["id", "name", "tab", "tab_name", "session", "session_name"];

/// Column headers for [`ClientRow`] listings, matching [`client_row_cells`].
const CLIENT_ROW_HEADERS: &[&str] = &["id", "session", "session_name"];

/// Field names for an `inspect session`, matching [`render_session_fields`] order.
const SESSION_HEADERS: &[&str] = &["id", "name", "created_at", "clients", "panes"];

/// Field names for an `inspect tab`, matching [`render_tab_fields`] order.
const TAB_HEADERS: &[&str] = &["id", "session", "name", "index", "active_pane", "panes"];

/// Field names for an `inspect pane`, matching [`render_pane_fields`] order.
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

/// Field names for an `inspect client`, matching [`render_client_fields`] order.
///
/// `pane_area` prints `-` for no report, `starving`, or `WxH`.
const CLIENT_HEADERS: &[&str] = &[
    "id",
    "session",
    "attached_at",
    "viewport",
    "pane_area",
    "active_tab",
    "focused_pane",
    "lock",
];

/// One [`SessionRow`] as table cells, in [`SESSION_ROW_HEADERS`] order. The
/// `server` cell is `local` for a session on this machine, else the saved
/// server the session runs on.
fn session_row_cells(session: &SessionRow) -> Vec<String> {
    vec![
        session.session_id.to_string(),
        session.session_name.clone(),
        session
            .server_name_or_address
            .clone()
            .unwrap_or_else(|| String::from("local")),
    ]
}

/// One [`TabRow`] as table cells, in [`TAB_ROW_HEADERS`] order.
fn tab_row_cells(tab: &TabRow) -> Vec<String> {
    vec![
        tab.tab_id.to_string(),
        tab.tab_name.clone(),
        tab.session_id.to_string(),
        tab.session_name.clone(),
    ]
}

/// One [`PaneRow`] as table cells, in [`PANE_ROW_HEADERS`] order. A pane the
/// child never titled prints `-`.
fn pane_row_cells(pane: &PaneRow) -> Vec<String> {
    vec![
        pane.pane_id.to_string(),
        format_optional_cell(pane.pane_name.as_ref()),
        pane.tab_id.to_string(),
        pane.tab_name.clone(),
        pane.session_id.to_string(),
        pane.session_name.clone(),
    ]
}

/// One [`ClientRow`] as table cells, in [`CLIENT_ROW_HEADERS`] order.
fn client_row_cells(client: &ClientRow) -> Vec<String> {
    vec![
        client.client_id.to_string(),
        client.session_id.to_string(),
        client.session_name.clone(),
    ]
}

/// One [`SessionDiscovery`] as field values, in [`SESSION_HEADERS`] order.
fn render_session_fields(session_discovery: &SessionDiscovery) -> Vec<String> {
    vec![
        session_discovery.session_id.to_string(),
        session_discovery.session_name.clone(),
        format_time_cell(session_discovery.created_at),
        session_discovery.attached_client_ids.len().to_string(),
        session_discovery.pane_count.to_string(),
    ]
}

/// One [`TabDiscovery`] as field values, in [`TAB_HEADERS`] order.
fn render_tab_fields(tab_discovery: &TabDiscovery) -> Vec<String> {
    vec![
        tab_discovery.tab_id.to_string(),
        tab_discovery.session_id.to_string(),
        tab_discovery.tab_name.clone(),
        tab_discovery.tab_index.to_string(),
        format_optional_cell(tab_discovery.active_pane_id.as_ref()),
        tab_discovery.pane_count.to_string(),
    ]
}

/// One [`PaneDiscovery`] as field values, in [`PANE_HEADERS`] order.
fn render_pane_fields(pane_discovery: &PaneDiscovery) -> Vec<String> {
    vec![
        pane_discovery.pane_id.to_string(),
        pane_discovery.tab_id.to_string(),
        pane_discovery.session_id.to_string(),
        format_optional_cell(pane_discovery.pane_title.as_ref()),
        match &pane_discovery.working_directory {
            Some(working_directory) => working_directory.display().to_string(),
            None => "-".to_string(),
        },
        match &pane_discovery.command_argv {
            Some(argv) => argv.join(" "),
            None => "-".to_string(),
        },
        format_pane_state_cell(pane_discovery.lifecycle),
        pane_discovery.focused_by_client_ids.len().to_string(),
    ]
}

/// One [`ClientDiscovery`] as field values, in [`CLIENT_HEADERS`] order.
fn render_client_fields(client_discovery: &ClientDiscovery) -> Vec<String> {
    vec![
        client_discovery.client_id.to_string(),
        client_discovery.session_id.to_string(),
        format_time_cell(client_discovery.attached_at),
        format_size_cell(client_discovery.viewport_size),
        match client_discovery.pane_area {
            None => "-".to_string(),
            Some(PaneArea::Starving) => "starving".to_string(),
            Some(PaneArea::Reported(size)) => format_size_cell(size),
        },
        client_discovery.active_tab_id.to_string(),
        format_optional_cell(client_discovery.focused_pane_id.as_ref()),
        format!("{:?}", client_discovery.lock_mode),
    ]
}

/// An optional value as a cell: its display form, or `-` when absent.
pub(super) fn format_optional_cell<DisplayValue: std::fmt::Display>(
    optional_value: Option<&DisplayValue>,
) -> String {
    match optional_value {
        Some(display_value) => display_value.to_string(),
        None => "-".to_string(),
    }
}

/// A timestamp as a cell: whole seconds since the Unix epoch, or `-` for a
/// moment before that epoch.
pub(super) fn format_time_cell(timestamp: SystemTime) -> String {
    match timestamp.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs().to_string(),
        Err(_) => "-".to_string(),
    }
}

/// A timestamp that may be absent as a cell: [`format_time_cell`] when present, `-`
/// when absent.
pub(super) fn format_optional_time_cell(timestamp: Option<SystemTime>) -> String {
    timestamp.map_or_else(|| "-".to_string(), format_time_cell)
}

/// A size as a cell: `<cols>x<rows>`.
pub(super) fn format_size_cell(cell_size: Size) -> String {
    format!("{}x{}", cell_size.column_count, cell_size.row_count)
}

/// A pane state as a cell: its lowercase name, with the exit code appended
/// as `exited(<exit_code>)` when one was observed and `exited(-)` when not.
pub(super) fn format_pane_state_cell(pane_lifecycle: PaneLifecycle) -> String {
    match pane_lifecycle {
        PaneLifecycle::Spawning => "spawning".to_string(),
        PaneLifecycle::Running => "running".to_string(),
        PaneLifecycle::Exited {
            exit_code: Some(exit_code),
        } => format!("exited({exit_code})"),
        PaneLifecycle::Exited { exit_code: None } => "exited(-)".to_string(),
        PaneLifecycle::Closing => "closing".to_string(),
    }
}
