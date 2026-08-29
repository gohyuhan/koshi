//! `koshi debug events`: narrowing a session's remembered events to what the
//! flags asked for, then rendering one row per event — the moment the record
//! was stamped, which event it was, and the ids it named.

use super::*;

use std::time::Duration;

use koshi_core::ids::SessionId;
use koshi_core::recent_event::RecentEvent;

/// The oldest moment a `--since <length>` window keeps, counted back from
/// `now`. `None` keeps every event, and comes back both from an absent
/// `since` and from a window reaching further back than `now` can be counted.
///
/// Example: `oldest_kept(now, Some(Duration::from_secs(30)))` results in the
/// moment thirty seconds before `now`.
#[must_use]
pub fn oldest_kept(now: SystemTime, since: Option<Duration>) -> Option<SystemTime> {
    since.and_then(|window| now.checked_sub(window))
}

/// Keep the events recorded at or after `oldest_kept` whose name contains
/// `wanted`. `wanted` is matched ignoring case, so `pane` keeps `PaneCreated`.
/// A `None` on either side drops nothing for that side. Order is unchanged.
///
/// Example: `narrow(events, None, Some("tab"))` keeps `TabCreated` and
/// `TabMoved` and drops `PaneCreated`.
#[must_use]
pub fn narrow(
    events: Vec<RecentEvent>,
    oldest_kept: Option<SystemTime>,
    wanted: Option<&str>,
) -> Vec<RecentEvent> {
    let wanted = wanted.map(str::to_lowercase);
    events
        .into_iter()
        .filter(|event| oldest_kept.is_none_or(|oldest| event.at >= oldest))
        .filter(|event| {
            wanted
                .as_ref()
                .is_none_or(|wanted| event.name.to_lowercase().contains(wanted))
        })
        .collect()
}

/// One session's recent events, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionEvents {
    /// The session that remembered them.
    pub session: SessionId,
    /// That session's name.
    pub name: String,
    /// The events, oldest first.
    pub events: Vec<RecentEvent>,
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
pub fn render_recent_events(sessions: &[SessionEvents], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&sessions),
        FormatArg::Table => {
            let rows: Vec<Vec<String>> = sessions
                .iter()
                .flat_map(|session| {
                    session.events.iter().map(|event| {
                        vec![
                            session.session.to_string(),
                            session.name.clone(),
                            time_cell(event.at),
                            event.name.to_string(),
                            id_cells(event),
                        ]
                    })
                })
                .collect();
            table(&["session", "name", "at", "event", "ids"], rows)
        }
    }
}

/// Every id the event named, space separated, or `-` when it named none.
///
/// The ids keep this order: session, client, tab, pane, plugin, command,
/// subscriber. Each prints its own kind, so `client-… tab-… pane-…` needs no
/// column of its own to say which is which.
fn id_cells(event: &RecentEvent) -> String {
    let cells: Vec<String> = [
        event.session.map(|id| id.to_string()),
        event.client.map(|id| id.to_string()),
        event.tab.map(|id| id.to_string()),
        event.pane.map(|id| id.to_string()),
        event.plugin.map(|id| id.to_string()),
        event.command.map(|id| id.to_string()),
        event.subscriber.map(|id| id.to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();
    if cells.is_empty() {
        return "-".to_string();
    }
    cells.join(" ")
}
