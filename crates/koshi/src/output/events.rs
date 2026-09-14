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
    since_duration.and_then(|window| current_time.checked_sub(window))
}

/// Keep the events recorded at or after `oldest_event_time` whose name contains
/// `event_name_filter`. `event_name_filter` is matched ignoring case, so `pane` keeps `PaneCreated`.
/// A `None` on either side drops nothing for that side. Order is unchanged.
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
        .filter(|event| {
            oldest_event_time.is_none_or(|oldest_event_time| event.occurred_at >= oldest_event_time)
        })
        .filter(|event| {
            event_name_filter_lowercase
                .as_ref()
                .is_none_or(|event_name_filter| {
                    event.event_name.to_lowercase().contains(event_name_filter)
                })
        })
        .collect()
}

/// One session's recent events, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionEvents {
    /// The session that remembered them.
    #[serde(rename = "session")]
    pub session_id: SessionId,
    /// That session's name.
    #[serde(rename = "name")]
    pub session_name: String,
    /// The events, oldest first.
    #[serde(rename = "events")]
    pub recent_events: Vec<RecentEvent>,
}

/// Render a `debug events` answer. The session cell carries the id and the
/// name, so two sessions sharing a name stay apart. One session that opened a
/// pane and focused it results in:
///
/// ```text
/// session                name        at          event        ids
/// session-…1             quiet-lake  1735600001  PaneCreated  tab-… pane-…
/// session-…1             quiet-lake  1735600001  PaneFocused  client-… tab-… pane-…
/// ```
///
/// A session that remembered nothing contributes no row. An answer holding
/// only such sessions renders the header alone.
#[must_use]
pub fn render_recent_events(
    session_events: &[SessionEvents],
    output_format: OutputFormat,
) -> String {
    match output_format {
        OutputFormat::Json => render_json(&session_events),
        OutputFormat::Table => {
            let event_rows: Vec<Vec<String>> = session_events
                .iter()
                .flat_map(|session_event_group| {
                    session_event_group
                        .recent_events
                        .iter()
                        .map(|recent_event| {
                            vec![
                                session_event_group.session_id.to_string(),
                                session_event_group.session_name.clone(),
                                format_time_cell(recent_event.occurred_at),
                                recent_event.event_name.to_string(),
                                render_event_identifier_cells(recent_event),
                            ]
                        })
                })
                .collect();
            render_table(&["session", "name", "at", "event", "ids"], event_rows)
        }
    }
}

/// Every id the event named, space separated, or `-` when it named none.
///
/// The ids keep this order: session, client, tab, pane, plugin, command,
/// subscriber. Each prints its own kind, so `client-… tab-… pane-…` needs no
/// column of its own to say which is which.
fn render_event_identifier_cells(recent_event: &RecentEvent) -> String {
    let event_identifiers: Vec<String> = [
        recent_event
            .session_id
            .map(|session_id| session_id.to_string()),
        recent_event
            .client_id
            .map(|client_id| client_id.to_string()),
        recent_event.tab_id.map(|tab_id| tab_id.to_string()),
        recent_event.pane_id.map(|pane_id| pane_id.to_string()),
        recent_event
            .plugin_id
            .map(|plugin_id| plugin_id.to_string()),
        recent_event
            .command_id
            .map(|command_id| command_id.to_string()),
        recent_event
            .subscriber_id
            .map(|subscriber_id| subscriber_id.to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();
    if event_identifiers.is_empty() {
        return "-".to_string();
    }
    event_identifiers.join(" ")
}
