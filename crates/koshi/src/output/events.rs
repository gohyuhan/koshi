//! `koshi debug events`: narrowing a session's remembered events to what the
//! flags asked for, then rendering one row per event — the moment the record
//! was stamped, which event it was, and the ids it named.

use super::*;

use std::time::Duration;

use koshi_core::ids::SessionId;
use koshi_core::recent_event::RecentEvent;

/// The oldest moment a `--since <length>` window keeps, counted back from
/// `current_time`. `None` keeps every event, and comes back both from an absent
/// `since_duration` and from a window reaching further back than `current_time` can be counted.
///
/// Example: `compute_oldest_event_time(current_time, Some(Duration::from_secs(30)))` results in the
/// moment thirty seconds before `current_time`.
#[must_use]
pub fn compute_oldest_event_time(
    current_time: SystemTime,
    since_duration: Option<Duration>,
) -> Option<SystemTime> {
    since_duration.and_then(|event_age_duration| current_time.checked_sub(event_age_duration))
}

/// Keep the events recorded at or after `oldest_event_time` whose name contains
/// `event_name_filter`. `event_name_filter` is matched ignoring case, so `pane` keeps
/// `PaneCreated`. A `None` on either side drops nothing for that side. Order is unchanged.
///
/// Example: `filter_recent_events(recent_events, None, Some("tab"))` keeps `TabCreated` and
/// `TabMoved` and drops `PaneCreated`.
#[must_use]
pub fn filter_recent_events(
    recent_events: Vec<RecentEvent>,
    oldest_event_time: Option<SystemTime>,
    event_name_filter: Option<&str>,
) -> Vec<RecentEvent> {
    let event_name_filter_lowercase = event_name_filter.map(str::to_lowercase);
    recent_events
        .into_iter()
        .filter(|recent_event| {
            oldest_event_time
                .is_none_or(|oldest_event_time| recent_event.occurred_at >= oldest_event_time)
        })
        .filter(|recent_event| {
            event_name_filter_lowercase
                .as_ref()
                .is_none_or(|event_name_filter| {
                    recent_event
                        .event_name
                        .to_lowercase()
                        .contains(event_name_filter)
                })
        })
        .collect()
}

/// One session's recent events, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvents {
    /// The session that remembered them.
    pub session_id: SessionId,
    /// That session's name.
    pub session_name: String,
    /// The events, oldest first.
    pub recent_events: Vec<RecentEvent>,
}

/// Render a `debug events` answer. The session cell carries the id and the
/// name, so two sessions sharing a name stay apart. One session that opened a
/// pane and focused it results in:
///
/// ```text
/// session                name        at                    event        ids
/// session-…1             quiet-lake  2024-12-31T00:00:01Z  PaneCreated  tab-… pane-…
/// session-…1             quiet-lake  2024-12-31T00:00:01Z  PaneFocused  client-… tab-… pane-…
/// ```
///
/// A session that remembered nothing contributes no row. An answer holding
/// only such sessions renders the header alone.
#[must_use]
pub fn render_recent_events(
    session_events: &[SessionEvents],
    output_format: OutputFormat,
) -> String {
    let session_event_reports: Vec<SessionEventsReport> = session_events
        .iter()
        .map(build_session_events_report)
        .collect();
    match output_format {
        OutputFormat::Json => render_json(&session_event_reports),
        OutputFormat::Table => {
            let event_rows: Vec<Vec<String>> = session_event_reports
                .iter()
                .flat_map(|session_events_report| {
                    session_events_report
                        .recent_events
                        .iter()
                        .map(|recent_event_row| {
                            vec![
                                session_events_report.session_id.clone(),
                                session_events_report.session_name.clone(),
                                recent_event_row.occurred_at.clone(),
                                recent_event_row.event_name.clone(),
                                render_event_identifier_cells(recent_event_row),
                            ]
                        })
                })
                .collect();
            render_table(&["session", "name", "at", "event", "ids"], event_rows)
        }
    }
}

/// One session's recent events as both formats of `debug events` print them.
#[derive(Serialize)]
struct SessionEventsReport {
    /// The session that remembered them, as `session-<uuid>`.
    session_id: String,
    /// That session's name.
    session_name: String,
    /// The events, oldest first.
    recent_events: Vec<RecentEventRow>,
}

/// One remembered event as both formats print it. Each id is in its prefixed
/// form, such as `pane-<uuid>`, or `None` when the event named no such id.
#[derive(Serialize)]
struct RecentEventRow {
    /// When the record was stamped, as [`format_utc_timestamp`] writes it.
    occurred_at: String,
    /// The event's name, e.g. `PaneCreated`.
    event_name: String,
    /// The session the event named.
    session_id: Option<String>,
    /// The client the event named.
    client_id: Option<String>,
    /// The tab the event named.
    tab_id: Option<String>,
    /// The pane the event named.
    pane_id: Option<String>,
    /// The command the event named.
    command_id: Option<String>,
}

/// One [`SessionEvents`] as both formats print it.
fn build_session_events_report(session_event_group: &SessionEvents) -> SessionEventsReport {
    SessionEventsReport {
        session_id: session_event_group.session_id.to_string(),
        session_name: session_event_group.session_name.clone(),
        recent_events: session_event_group
            .recent_events
            .iter()
            .map(|recent_event| RecentEventRow {
                occurred_at: format_utc_timestamp(recent_event.occurred_at),
                event_name: recent_event.event_name.to_string(),
                session_id: recent_event
                    .session_id
                    .map(|session_id| session_id.to_string()),
                client_id: recent_event
                    .client_id
                    .map(|client_id| client_id.to_string()),
                tab_id: recent_event.tab_id.map(|tab_id| tab_id.to_string()),
                pane_id: recent_event.pane_id.map(|pane_id| pane_id.to_string()),
                command_id: recent_event
                    .command_id
                    .map(|command_id| command_id.to_string()),
            })
            .collect(),
    }
}

/// Every id the event named, space separated, or `-` when it named none.
///
/// The ids keep this order: session, client, tab, pane, command. Each prints its own
/// kind, so `client-… tab-… pane-…` needs no column of its own to say which is which.
fn render_event_identifier_cells(recent_event_row: &RecentEventRow) -> String {
    let event_identifiers: Vec<&str> = [
        &recent_event_row.session_id,
        &recent_event_row.client_id,
        &recent_event_row.tab_id,
        &recent_event_row.pane_id,
        &recent_event_row.command_id,
    ]
    .into_iter()
    .flatten()
    .map(String::as_str)
    .collect();
    if event_identifiers.is_empty() {
        return "-".to_string();
    }
    event_identifiers.join(" ")
}
