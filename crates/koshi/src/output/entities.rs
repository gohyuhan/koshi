//! Table and JSON renderers for the entity queries: the id-chain listings
//! (`list-sessions`, `list-tabs`, `list-panes`, `list-clients`), the full
//! `inspect` records, and the `debug dump-state` dump of those same records
//! across every running session.
//!
//! Each answer is built once into the rows and reports below. The JSON form
//! serializes them, and the table cells read them.

use super::*;
use koshi_core::client::ClientOrigin;
use koshi_core::geometry::PaneArea;

/// Render a `list-sessions` answer.
#[must_use]
pub fn render_sessions(session_rows: &[SessionRow], output_format: OutputFormat) -> String {
    let session_listing_rows: Vec<SessionListingRow> =
        session_rows.iter().map(build_session_listing_row).collect();
    render_listing(
        &session_listing_rows,
        SESSION_ROW_HEADERS,
        format_session_row_cells,
        output_format,
    )
}

/// Render an `inspect session` answer.
#[must_use]
pub fn render_session(session_discovery: &SessionDiscovery, output_format: OutputFormat) -> String {
    render_entity_record(
        &build_session_report(session_discovery),
        SESSION_HEADERS,
        render_session_fields,
        output_format,
    )
}

/// Render a `list-tabs` answer.
#[must_use]
pub fn render_tabs(tab_rows: &[TabRow], output_format: OutputFormat) -> String {
    let tab_listing_rows: Vec<TabListingRow> = tab_rows.iter().map(build_tab_listing_row).collect();
    render_listing(
        &tab_listing_rows,
        TAB_ROW_HEADERS,
        format_tab_row_cells,
        output_format,
    )
}

/// Render an `inspect tab` answer.
#[must_use]
pub fn render_tab(tab_discovery: &TabDiscovery, output_format: OutputFormat) -> String {
    render_entity_record(
        &build_tab_report(tab_discovery),
        TAB_HEADERS,
        render_tab_fields,
        output_format,
    )
}

/// Render a `list-panes` answer.
#[must_use]
pub fn render_panes(pane_rows: &[PaneRow], output_format: OutputFormat) -> String {
    let pane_listing_rows: Vec<PaneListingRow> =
        pane_rows.iter().map(build_pane_listing_row).collect();
    render_listing(
        &pane_listing_rows,
        PANE_ROW_HEADERS,
        format_pane_row_cells,
        output_format,
    )
}

/// Render an `inspect pane` answer.
#[must_use]
pub fn render_pane(pane_discovery: &PaneDiscovery, output_format: OutputFormat) -> String {
    render_entity_record(
        &build_pane_report(pane_discovery),
        PANE_HEADERS,
        render_pane_fields,
        output_format,
    )
}

/// Render a `list-clients` answer.
#[must_use]
pub fn render_clients(client_rows: &[ClientRow], output_format: OutputFormat) -> String {
    let client_listing_rows: Vec<ClientListingRow> =
        client_rows.iter().map(build_client_listing_row).collect();
    render_listing(
        &client_listing_rows,
        CLIENT_ROW_HEADERS,
        format_client_row_cells,
        output_format,
    )
}

/// Render an `inspect client` answer.
#[must_use]
pub fn render_client(client_discovery: &ClientDiscovery, output_format: OutputFormat) -> String {
    render_entity_record(
        &build_client_report(client_discovery),
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
    let session_overview_reports: Vec<SessionOverviewReport> = session_overviews
        .iter()
        .map(|session_overview| SessionOverviewReport {
            session: build_session_report(&session_overview.session),
            tabs: session_overview.tabs.iter().map(build_tab_report).collect(),
            panes: session_overview
                .panes
                .iter()
                .map(build_pane_report)
                .collect(),
            clients: session_overview
                .clients
                .iter()
                .map(build_client_report)
                .collect(),
        })
        .collect();
    match output_format {
        OutputFormat::Json => render_json(&session_overview_reports),
        OutputFormat::Table => format!(
            "sessions\n{}\ntabs\n{}\npanes\n{}\nclients\n{}\n",
            render_table(
                SESSION_HEADERS,
                session_overview_reports
                    .iter()
                    .map(|session_overview_report| {
                        render_session_fields(&session_overview_report.session)
                    })
                    .collect(),
            ),
            render_table(
                TAB_HEADERS,
                session_overview_reports
                    .iter()
                    .flat_map(|session_overview_report| session_overview_report.tabs.iter())
                    .map(render_tab_fields)
                    .collect(),
            ),
            render_table(
                PANE_HEADERS,
                session_overview_reports
                    .iter()
                    .flat_map(|session_overview_report| session_overview_report.panes.iter())
                    .map(render_pane_fields)
                    .collect(),
            ),
            render_table(
                CLIENT_HEADERS,
                session_overview_reports
                    .iter()
                    .flat_map(|session_overview_report| session_overview_report.clients.iter())
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
/// `field: value` line per header, valued by `render_field_values`.
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

/// One `list-sessions` row, as both formats print it.
#[derive(Serialize)]
struct SessionListingRow {
    /// The session id, as `session-<uuid>`.
    session_id: String,
    /// The session's display name.
    session_name: String,
    /// The saved server the session runs on, by the name it was saved under
    /// or its `host:port` address. `None` for a session on this machine.
    server_name_or_address: Option<String>,
    /// The user id whose folder of the shared directory advertises the
    /// session, such as `1001`, or `None`.
    owner_user_id: Option<u32>,
}

/// One `list-tabs` row, as both formats print it.
#[derive(Serialize)]
struct TabListingRow {
    /// The tab id, as `tab-<uuid>`.
    tab_id: String,
    /// The tab's display name.
    tab_name: String,
    /// The id of the session holding the tab, as `session-<uuid>`.
    session_id: String,
    /// That session's display name.
    session_name: String,
}

/// One `list-panes` row, as both formats print it.
#[derive(Serialize)]
struct PaneListingRow {
    /// The pane id, as `pane-<uuid>`.
    pane_id: String,
    /// The pane's title, or `None` until the child sets one.
    pane_name: Option<String>,
    /// The id of the tab whose layout holds the pane, as `tab-<uuid>`, or
    /// `None` for a floating pane.
    tab_id: Option<String>,
    /// That tab's display name, or `None` for a floating pane.
    tab_name: Option<String>,
    /// The id of the session holding the pane, as `session-<uuid>`.
    session_id: String,
    /// That session's display name.
    session_name: String,
}

/// One `list-clients` row, as both formats print it.
#[derive(Serialize)]
struct ClientListingRow {
    /// The client id, as `client-<uuid>`.
    client_id: String,
    /// The id of the session the client is attached to, as `session-<uuid>`.
    session_id: String,
    /// That session's display name.
    session_name: String,
}

/// One session as `inspect session` and `debug dump-state` print it.
#[derive(Serialize)]
struct SessionReport {
    /// The session id, as `session-<uuid>`.
    session_id: String,
    /// The session's display name.
    session_name: String,
    /// When the session was created, as [`format_utc_timestamp`] writes it.
    created_at: String,
    /// The ids of the attached clients, each as `client-<uuid>`.
    attached_client_ids: Vec<String>,
    /// How many panes the session holds.
    pane_count: usize,
}

/// One tab as `inspect tab` and `debug dump-state` print it.
#[derive(Serialize)]
struct TabReport {
    /// The tab id, as `tab-<uuid>`.
    tab_id: String,
    /// The id of the session holding the tab, as `session-<uuid>`.
    session_id: String,
    /// The tab's display name.
    tab_name: String,
    /// The tab's position in the bar, starting at 0.
    tab_index: usize,
    /// The pane the tab most recently focused, as `pane-<uuid>`, or `None`.
    active_pane_id: Option<String>,
    /// How many panes the tab holds.
    pane_count: usize,
}

/// One pane as `inspect pane` and `debug dump-state` print it.
#[derive(Serialize)]
struct PaneReport {
    /// The pane id, as `pane-<uuid>`.
    pane_id: String,
    /// The id of the tab whose layout holds the pane, as `tab-<uuid>`, or
    /// `None` for a floating pane.
    tab_id: Option<String>,
    /// The id of the session holding the pane, as `session-<uuid>`.
    session_id: String,
    /// The pane's title, or `None` until the child sets one.
    pane_title: Option<String>,
    /// The pane's working directory as lossy UTF-8: bytes that are not valid
    /// UTF-8 become U+FFFD. `None` when unknown.
    working_directory: Option<String>,
    /// The command arguments, or `None` when unknown.
    command_argv: Option<Vec<String>>,
    /// Where the pane sits in its life, as [`format_pane_lifecycle`] names it.
    lifecycle: &'static str,
    /// The ids of the clients whose focus is on the pane, each as
    /// `client-<uuid>`.
    focused_by_client_ids: Vec<String>,
}

/// One client as `inspect client` and `debug dump-state` print it.
#[derive(Serialize)]
struct ClientReport {
    /// The client id, as `client-<uuid>`.
    client_id: String,
    /// The id of the session the client is attached to, as `session-<uuid>`.
    session_id: String,
    /// When the client attached, as [`format_utc_timestamp`] writes it.
    attached_at: String,
    /// The client's terminal viewport size.
    viewport_size: Size,
    /// The tab the client is viewing, as `tab-<uuid>`.
    active_tab_id: String,
    /// The pane that takes the client's input, as `pane-<uuid>`, or `None`
    /// until it has focused one.
    focused_pane_id: Option<String>,
    /// The client's input mode, by its keymap name: `normal`, `locked`,
    /// `pane-placement`, `pane-move-choice`, `floating-pane-move`,
    /// `dock-undock` or `floating-pane-tray`.
    lock_mode: &'static str,
    /// Where the client connected from, as [`format_client_origin`] names
    /// it, or `None` when the session did not say.
    origin: Option<&'static str>,
    /// The pane region the client reported, or `None` for no report.
    pane_area: Option<PaneAreaReport>,
}

/// The pane region one client reported. JSON writes `"starving"` for
/// [`Starving`](PaneAreaReport::Starving) and the size object for
/// [`Reported`](PaneAreaReport::Reported).
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum PaneAreaReport {
    /// The client reported that it can draw no panes.
    Starving,
    /// The client draws the tab's panes inside a region of this size.
    #[serde(untagged)]
    Reported(Size),
}

/// One session described in full, as `debug dump-state` prints it.
#[derive(Serialize)]
struct SessionOverviewReport {
    /// The session itself.
    session: SessionReport,
    /// The session's tabs, in tab-bar order.
    tabs: Vec<TabReport>,
    /// Every pane of the session: the panes of its tabs in tab-bar order, then
    /// its floating panes in the order they became floating.
    panes: Vec<PaneReport>,
    /// The clients attached to the session.
    clients: Vec<ClientReport>,
}

/// One [`SessionRow`] as both formats print it.
fn build_session_listing_row(session_row: &SessionRow) -> SessionListingRow {
    SessionListingRow {
        session_id: session_row.session_id.to_string(),
        session_name: session_row.session_name.clone(),
        server_name_or_address: session_row.server_name_or_address.clone(),
        owner_user_id: session_row.owner_user_id,
    }
}

/// One [`TabRow`] as both formats print it.
fn build_tab_listing_row(tab_row: &TabRow) -> TabListingRow {
    TabListingRow {
        tab_id: tab_row.tab_id.to_string(),
        tab_name: tab_row.tab_name.clone(),
        session_id: tab_row.session_id.to_string(),
        session_name: tab_row.session_name.clone(),
    }
}

/// One [`PaneRow`] as both formats print it.
fn build_pane_listing_row(pane_row: &PaneRow) -> PaneListingRow {
    PaneListingRow {
        pane_id: pane_row.pane_id.to_string(),
        pane_name: pane_row.pane_name.clone(),
        tab_id: pane_row.tab_id.map(|tab_id| tab_id.to_string()),
        tab_name: pane_row.tab_name.clone(),
        session_id: pane_row.session_id.to_string(),
        session_name: pane_row.session_name.clone(),
    }
}

/// One [`ClientRow`] as both formats print it.
fn build_client_listing_row(client_row: &ClientRow) -> ClientListingRow {
    ClientListingRow {
        client_id: client_row.client_id.to_string(),
        session_id: client_row.session_id.to_string(),
        session_name: client_row.session_name.clone(),
    }
}

/// One [`SessionDiscovery`] as both formats print it.
fn build_session_report(session_discovery: &SessionDiscovery) -> SessionReport {
    SessionReport {
        session_id: session_discovery.session_id.to_string(),
        session_name: session_discovery.session_name.clone(),
        created_at: format_utc_timestamp(session_discovery.created_at),
        attached_client_ids: session_discovery
            .attached_client_ids
            .iter()
            .map(ToString::to_string)
            .collect(),
        pane_count: session_discovery.pane_count,
    }
}

/// One [`TabDiscovery`] as both formats print it.
fn build_tab_report(tab_discovery: &TabDiscovery) -> TabReport {
    TabReport {
        tab_id: tab_discovery.tab_id.to_string(),
        session_id: tab_discovery.session_id.to_string(),
        tab_name: tab_discovery.tab_name.clone(),
        tab_index: tab_discovery.tab_index,
        active_pane_id: tab_discovery
            .active_pane_id
            .map(|active_pane_id| active_pane_id.to_string()),
        pane_count: tab_discovery.pane_count,
    }
}

/// One [`PaneDiscovery`] as both formats print it.
fn build_pane_report(pane_discovery: &PaneDiscovery) -> PaneReport {
    PaneReport {
        pane_id: pane_discovery.pane_id.to_string(),
        tab_id: pane_discovery.tab_id.map(|tab_id| tab_id.to_string()),
        session_id: pane_discovery.session_id.to_string(),
        pane_title: pane_discovery.pane_title.clone(),
        working_directory: pane_discovery
            .working_directory
            .as_ref()
            .map(|working_directory| working_directory.to_string_lossy().into_owned()),
        command_argv: pane_discovery.command_argv.clone(),
        lifecycle: format_pane_lifecycle(pane_discovery.lifecycle),
        focused_by_client_ids: pane_discovery
            .focused_by_client_ids
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
}

/// One [`ClientDiscovery`] as both formats print it.
fn build_client_report(client_discovery: &ClientDiscovery) -> ClientReport {
    ClientReport {
        client_id: client_discovery.client_id.to_string(),
        session_id: client_discovery.session_id.to_string(),
        attached_at: format_utc_timestamp(client_discovery.attached_at),
        viewport_size: client_discovery.viewport_size,
        active_tab_id: client_discovery.active_tab_id.to_string(),
        focused_pane_id: client_discovery
            .focused_pane_id
            .map(|focused_pane_id| focused_pane_id.to_string()),
        lock_mode: client_discovery.lock_mode.get_keymap_name(),
        origin: client_discovery.origin.map(format_client_origin),
        pane_area: client_discovery.pane_area.map(|pane_area| match pane_area {
            PaneArea::Starving => PaneAreaReport::Starving,
            PaneArea::Reported(size) => PaneAreaReport::Reported(size),
        }),
    }
}

/// Column headers for `list-sessions`, matching [`format_session_row_cells`].
const SESSION_ROW_HEADERS: &[&str] = &["id", "name", "server", "owner"];

/// Column headers for `list-tabs`, matching [`format_tab_row_cells`].
const TAB_ROW_HEADERS: &[&str] = &["id", "name", "session", "session_name"];

/// Column headers for `list-panes`, matching [`format_pane_row_cells`].
const PANE_ROW_HEADERS: &[&str] = &["id", "name", "tab", "tab_name", "session", "session_name"];

/// Column headers for `list-clients`, matching [`format_client_row_cells`].
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

/// One [`SessionListingRow`] as table cells, in [`SESSION_ROW_HEADERS`] order.
/// The `server` cell is `local` for a session on this machine, else the saved
/// server the session runs on. The `owner` cell is the user id of another
/// user whose folder of the shared directory advertises the session, such as
/// `1001`, and `-` for every other session.
fn format_session_row_cells(session: &SessionListingRow) -> Vec<String> {
    vec![
        session.session_id.clone(),
        session.session_name.clone(),
        session
            .server_name_or_address
            .clone()
            .unwrap_or_else(|| String::from("local")),
        format_optional_cell(session.owner_user_id.as_ref()),
    ]
}

/// One [`TabListingRow`] as table cells, in [`TAB_ROW_HEADERS`] order.
fn format_tab_row_cells(tab: &TabListingRow) -> Vec<String> {
    vec![
        tab.tab_id.clone(),
        tab.tab_name.clone(),
        tab.session_id.clone(),
        tab.session_name.clone(),
    ]
}

/// One [`PaneListingRow`] as table cells, in [`PANE_ROW_HEADERS`] order. A pane
/// the child never titled prints `-`. A floating pane prints `float` as its tab
/// and `-` as its tab name.
fn format_pane_row_cells(pane: &PaneListingRow) -> Vec<String> {
    vec![
        pane.pane_id.clone(),
        format_optional_cell(pane.pane_name.as_ref()),
        format_pane_tab_cell(pane.tab_id.as_ref()),
        format_optional_cell(pane.tab_name.as_ref()),
        pane.session_id.clone(),
        pane.session_name.clone(),
    ]
}

/// The tab cell of a pane: the id of the tab whose layout holds it, or `float`
/// for a floating pane.
fn format_pane_tab_cell(tab_id: Option<&String>) -> String {
    match tab_id {
        Some(tab_id) => tab_id.clone(),
        None => String::from("float"),
    }
}

/// One [`ClientListingRow`] as table cells, in [`CLIENT_ROW_HEADERS`] order.
fn format_client_row_cells(client: &ClientListingRow) -> Vec<String> {
    vec![
        client.client_id.clone(),
        client.session_id.clone(),
        client.session_name.clone(),
    ]
}

/// One [`SessionReport`] as field values, in [`SESSION_HEADERS`] order.
fn render_session_fields(session_report: &SessionReport) -> Vec<String> {
    vec![
        session_report.session_id.clone(),
        session_report.session_name.clone(),
        session_report.created_at.clone(),
        session_report.attached_client_ids.len().to_string(),
        session_report.pane_count.to_string(),
    ]
}

/// One [`TabReport`] as field values, in [`TAB_HEADERS`] order.
fn render_tab_fields(tab_report: &TabReport) -> Vec<String> {
    vec![
        tab_report.tab_id.clone(),
        tab_report.session_id.clone(),
        tab_report.tab_name.clone(),
        tab_report.tab_index.to_string(),
        format_optional_cell(tab_report.active_pane_id.as_ref()),
        tab_report.pane_count.to_string(),
    ]
}

/// One [`PaneReport`] as field values, in [`PANE_HEADERS`] order.
fn render_pane_fields(pane_report: &PaneReport) -> Vec<String> {
    vec![
        pane_report.pane_id.clone(),
        format_pane_tab_cell(pane_report.tab_id.as_ref()),
        pane_report.session_id.clone(),
        format_optional_cell(pane_report.pane_title.as_ref()),
        format_optional_cell(pane_report.working_directory.as_ref()),
        match &pane_report.command_argv {
            Some(argv) => argv.join(" "),
            None => "-".to_string(),
        },
        pane_report.lifecycle.to_string(),
        pane_report.focused_by_client_ids.len().to_string(),
    ]
}

/// One [`ClientReport`] as field values, in [`CLIENT_HEADERS`] order.
fn render_client_fields(client_report: &ClientReport) -> Vec<String> {
    vec![
        client_report.client_id.clone(),
        client_report.session_id.clone(),
        client_report.attached_at.clone(),
        format_size_cell(client_report.viewport_size),
        match &client_report.pane_area {
            None => "-".to_string(),
            Some(PaneAreaReport::Starving) => "starving".to_string(),
            Some(PaneAreaReport::Reported(size)) => format_size_cell(*size),
        },
        client_report.active_tab_id.clone(),
        format_optional_cell(client_report.focused_pane_id.as_ref()),
        client_report.lock_mode.to_string(),
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

/// Seconds in one day.
const SECONDS_PER_DAY: i128 = 86_400;

/// A moment as RFC 3339 UTC text, floored to the whole second:
/// `2025-01-01T00:00:00Z` for 1735689600 seconds after the Unix epoch, and
/// `1969-12-31T23:59:59Z` for half a second before it.
///
/// Dates follow the proleptic Gregorian calendar with year `0` before year
/// `1`. A year from `0` to `9999` prints as four digits. Any other year prints
/// in the ISO 8601 expanded form, a sign and at least five digits:
/// `+10000-01-01T00:00:00Z`, `-00001-12-31T23:59:59Z`. Every moment a
/// [`SystemTime`] holds formats.
pub(super) fn format_utc_timestamp(timestamp: SystemTime) -> String {
    let unix_seconds: i128 = match timestamp.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration_after_epoch) => i128::from(duration_after_epoch.as_secs()),
        Err(before_epoch_error) => {
            let duration_before_epoch = before_epoch_error.duration();
            let whole_seconds_before_epoch = i128::from(duration_before_epoch.as_secs());
            if duration_before_epoch.subsec_nanos() == 0 {
                -whole_seconds_before_epoch
            } else {
                -whole_seconds_before_epoch - 1
            }
        }
    };
    let second_of_day = unix_seconds.rem_euclid(SECONDS_PER_DAY);

    // Howard Hinnant's `civil_from_days`: the day count is shifted to start on
    // 0000-03-01, cut into 400-year eras of 146097 days, and each day of an era
    // maps to a year, a month counted from March, and a day.
    let day_count_from_year_zero_march = unix_seconds.div_euclid(SECONDS_PER_DAY) + 719_468;
    let era_index = day_count_from_year_zero_march.div_euclid(146_097);
    let day_of_era = day_count_from_year_zero_march.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index_from_march = (5 * day_of_year + 2) / 153;
    let day_of_month = day_of_year - (153 * month_index_from_march + 2) / 5 + 1;
    let (month, year) = if month_index_from_march < 10 {
        (month_index_from_march + 3, era_index * 400 + year_of_era)
    } else {
        (
            month_index_from_march - 9,
            era_index * 400 + year_of_era + 1,
        )
    };

    let year_text = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+06}")
    };
    format!(
        "{year_text}-{month:02}-{day_of_month:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        second_of_day / 60 % 60,
        second_of_day % 60
    )
}

/// A size as a cell: `<cols>x<rows>`.
pub(super) fn format_size_cell(size: Size) -> String {
    format!("{}x{}", size.column_count, size.row_count)
}

/// A pane's place in its life, as both formats name it: `running`.
fn format_pane_lifecycle(pane_lifecycle: PaneLifecycle) -> &'static str {
    match pane_lifecycle {
        PaneLifecycle::Running => "running",
    }
}

/// Where a client connected from, as JSON names it: `local` or `remote`.
fn format_client_origin(client_origin: ClientOrigin) -> &'static str {
    match client_origin {
        ClientOrigin::Local => "local",
        ClientOrigin::Remote => "remote",
    }
}
