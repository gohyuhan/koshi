//! Tests for the recent-event record: every variant is named, every id it
//! carries is the one its payload holds, and no payload content reaches it.

use super::*;

use std::collections::BTreeSet;
use std::time::Duration;

use crate::event::tests::list_event_cases;
use crate::event::{
    ConfigReloaded, PaneCommandFinished, PaneCreated, PaneFocused, QuitCause, TabFocused,
};

/// A fixed instant, so an assertion never races the clock.
fn build_occurred_at() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

#[test]
fn every_event_variant_records_the_name_it_reports() {
    for (event, event_name) in list_event_cases() {
        let recorded_event = record_event(&event, build_occurred_at());
        assert_eq!(recorded_event.event_name, Cow::Borrowed(event_name));
        assert_eq!(recorded_event.occurred_at, build_occurred_at());
    }
}

/// Every UUID `serialized_json_value` holds, anywhere inside it. Every typed id serializes as
/// a bare UUID string, so this finds each one an event or a record names.
fn collect_ids_from_json(serialized_json_value: &serde_json::Value) -> BTreeSet<String> {
    match serialized_json_value {
        serde_json::Value::String(text) => match uuid::Uuid::parse_str(text) {
            Ok(_) => BTreeSet::from([text.clone()]),
            Err(_) => BTreeSet::new(),
        },
        serde_json::Value::Array(serialized_json_values) => serialized_json_values
            .iter()
            .flat_map(collect_ids_from_json)
            .collect(),
        serde_json::Value::Object(serialized_json_fields) => serialized_json_fields
            .values()
            .flat_map(collect_ids_from_json)
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// The ids `event` names that its record deliberately leaves out: the place a
/// focus change came from. A record holds one id per kind, so the pane and tab
/// a client just left have nowhere to go.
fn list_omitted_event_ids(event: &Event) -> BTreeSet<String> {
    match event {
        Event::PaneFocused(payload) => payload
            .previous_pane_id
            .iter()
            .map(ToString::to_string)
            .map(|pane_id_text| pane_id_text.trim_start_matches("pane-").to_string())
            .collect(),
        Event::TabFocused(payload) => BTreeSet::from([payload
            .previous_tab_id
            .to_string()
            .trim_start_matches("tab-")
            .to_string()]),
        Event::PanePlacementCommitted(_) => {
            let named_event_ids =
                collect_ids_from_json(&serde_json::to_value(event).expect("event encodes"));
            let recorded_event_ids = collect_ids_from_json(
                &serde_json::to_value(record_event(event, build_occurred_at()))
                    .expect("recent event encodes"),
            );
            named_event_ids
                .difference(&recorded_event_ids)
                .cloned()
                .collect()
        }
        _ => BTreeSet::new(),
    }
}

#[test]
fn every_id_an_event_names_reaches_its_record_and_no_other_id_does() {
    for (event, event_name) in list_event_cases() {
        let recorded_event = record_event(&event, build_occurred_at());
        let event_ids = collect_ids_from_json(&serde_json::to_value(&event).unwrap());
        let recorded_ids = collect_ids_from_json(&serde_json::to_value(&recorded_event).unwrap());

        assert!(
            recorded_ids.is_subset(&event_ids),
            "{event_name} records an id its event never named: {:?}",
            recorded_ids.difference(&event_ids).collect::<Vec<_>>()
        );

        let dropped_event_ids: BTreeSet<String> =
            event_ids.difference(&recorded_ids).cloned().collect();
        assert_eq!(
            dropped_event_ids,
            list_omitted_event_ids(&event),
            "{event_name} drops the wrong ids"
        );
    }
}

#[test]
fn a_pane_created_records_its_pane_and_tab_and_nothing_else() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let recorded_event = record_event(
        &Event::PaneCreated(PaneCreated { pane_id, tab_id }),
        build_occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: build_occurred_at(),
            event_name: Cow::Borrowed("PaneCreated"),
            session_id: None,
            client_id: None,
            tab_id: Some(tab_id),
            pane_id: Some(pane_id),
            command_id: None,
        }
    );
}

#[test]
fn a_pane_focused_records_its_client_tab_and_pane_but_not_the_pane_it_left() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let previous_pane_id = PaneId::new();

    let recorded_event = record_event(
        &Event::PaneFocused(PaneFocused {
            client_id,
            tab_id,
            pane_id,
            previous_pane_id: Some(previous_pane_id),
        }),
        build_occurred_at(),
    );

    assert_eq!(recorded_event.client_id, Some(client_id));
    assert_eq!(recorded_event.tab_id, Some(tab_id));
    assert_eq!(recorded_event.pane_id, Some(pane_id));
    assert_ne!(recorded_event.pane_id, Some(previous_pane_id));
}

#[test]
fn a_tab_focused_records_its_client_and_tab_but_not_the_tab_it_left() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let previous_tab_id = TabId::new();

    let recorded_event = record_event(
        &Event::TabFocused(TabFocused {
            client_id,
            tab_id,
            previous_tab_id,
        }),
        build_occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: build_occurred_at(),
            event_name: Cow::Borrowed("TabFocused"),
            session_id: None,
            client_id: Some(client_id),
            tab_id: Some(tab_id),
            pane_id: None,
            command_id: None,
        }
    );
}

#[test]
fn a_finished_command_records_its_pane_and_no_exit_code() {
    let pane_id = PaneId::new();

    let recorded_event = record_event(
        &Event::PaneCommandFinished(PaneCommandFinished {
            pane_id,
            exit_code: Some(127),
        }),
        build_occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: build_occurred_at(),
            event_name: Cow::Borrowed("PaneCommandFinished"),
            session_id: None,
            client_id: None,
            tab_id: None,
            pane_id: Some(pane_id),
            command_id: None,
        }
    );
}

#[test]
fn a_config_reload_records_its_session() {
    let session_id = SessionId::new();

    let recorded_event = record_event(
        &Event::ConfigReloaded(ConfigReloaded { session_id }),
        build_occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: build_occurred_at(),
            event_name: Cow::Borrowed("ConfigReloaded"),
            session_id: Some(session_id),
            client_id: None,
            tab_id: None,
            pane_id: None,
            command_id: None,
        }
    );
}

#[test]
fn a_quit_records_a_name_and_no_id_at_all() {
    let recorded_event = record_event(&Event::Quit(QuitCause::Requested), build_occurred_at());

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: build_occurred_at(),
            event_name: Cow::Borrowed("Quit"),
            session_id: None,
            client_id: None,
            tab_id: None,
            pane_id: None,
            command_id: None,
        }
    );
}

#[test]
fn a_record_survives_the_wire_with_an_owned_name() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let recorded_event = record_event(
        &Event::PaneCreated(PaneCreated { pane_id, tab_id }),
        build_occurred_at(),
    );
    assert!(matches!(recorded_event.event_name, Cow::Borrowed(_)));

    let decoded_recent_event: RecentEvent =
        serde_json::from_str(&serde_json::to_string(&recorded_event).unwrap())
            .expect("a record this build wrote reads back");

    assert_eq!(decoded_recent_event, recorded_event);
    assert!(matches!(decoded_recent_event.event_name, Cow::Owned(_)));
}

#[test]
fn a_record_from_a_newer_koshi_reads_with_the_field_it_adds_ignored() {
    let serialized_recent_event_json = r#"{
        "occurred_at": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
        "event_name": "PaneOpenedSideways",
        "session_id": null,
        "client_id": null,
        "tab_id": null,
        "pane_id": null,
        "command_id": null,
        "workspace": "w-1"
    }"#;

    let decoded_recent_event: RecentEvent =
        serde_json::from_str(serialized_recent_event_json).expect("an added field is ignored");

    assert_eq!(
        decoded_recent_event.event_name,
        Cow::Borrowed("PaneOpenedSideways")
    );
    assert_eq!(decoded_recent_event.occurred_at, build_occurred_at());
}

#[test]
fn a_record_whose_time_cannot_be_represented_is_refused_and_does_not_panic() {
    let serialized_recent_event_json = r#"{
        "occurred_at": {"secs_since_epoch": 18446744073709551615, "nanos_since_epoch": 999999999},
        "event_name": "PaneCreated",
        "session_id": null,
        "client_id": null,
        "tab_id": null,
        "pane_id": null,
        "command_id": null
    }"#;

    let recent_event_parse_error =
        serde_json::from_str::<RecentEvent>(serialized_recent_event_json)
            .expect_err("a time past the clock's range is refused");

    assert!(
        recent_event_parse_error.to_string().contains("SystemTime"),
        "{recent_event_parse_error}"
    );
}

#[test]
fn a_record_missing_an_id_field_reads_it_as_absent() {
    let serialized_recent_event_json = r#"{
        "occurred_at": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
        "event_name": "PaneCreated",
        "session_id": null,
        "client_id": null,
        "tab_id": null,
        "pane_id": null
    }"#;

    let decoded_recent_event: RecentEvent = serde_json::from_str(serialized_recent_event_json)
        .expect("an absent id field reads as none");

    assert_eq!(decoded_recent_event.command_id, None);
    assert_eq!(decoded_recent_event.event_name, "PaneCreated");
}

#[test]
fn a_record_missing_the_time_or_the_name_is_refused() {
    let missing_time_json = r#"{"event_name": "Quit", "session_id": null, "client_id": null, "tab_id": null,
        "pane_id": null, "command_id": null}"#;
    let missing_name_json = r#"{"occurred_at": {"secs_since_epoch": 1, "nanos_since_epoch": 0}, "session_id": null,
        "client_id": null, "tab_id": null, "pane_id": null, "command_id": null}"#;

    let missing_time_parse_error =
        serde_json::from_str::<RecentEvent>(missing_time_json).expect_err("a record needs a time");
    assert!(
        missing_time_parse_error.to_string().contains("occurred_at"),
        "{missing_time_parse_error}"
    );

    let missing_name_parse_error =
        serde_json::from_str::<RecentEvent>(missing_name_json).expect_err("a record needs a name");
    assert!(
        missing_name_parse_error.to_string().contains("event_name"),
        "{missing_name_parse_error}"
    );
}

#[test]
fn a_record_whose_name_is_not_an_event_this_build_has_still_reads() {
    let serialized_recent_event_json = r#"{
        "occurred_at": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
        "event_name": "PaneOpenedSideways",
        "session_id": null,
        "client_id": null,
        "tab_id": null,
        "pane_id": null,
        "command_id": null
    }"#;

    let decoded_recent_event: RecentEvent =
        serde_json::from_str(serialized_recent_event_json).expect("an unknown name still reads");

    assert_eq!(decoded_recent_event.event_name, "PaneOpenedSideways");
}

#[test]
fn a_record_serializes_with_its_field_names_in_order() {
    let recorded_event = record_event(&Event::Quit(QuitCause::Requested), build_occurred_at());

    assert_eq!(
        serde_json::to_string(&recorded_event).unwrap(),
        r#"{"occurred_at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},"event_name":"Quit","session_id":null,"client_id":null,"tab_id":null,"pane_id":null,"command_id":null}"#
    );
}

#[test]
fn a_record_whose_name_is_null_is_refused() {
    let serialized_recent_event_json = r#"{
        "occurred_at": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
        "event_name": null,
        "session_id": null,
        "client_id": null,
        "tab_id": null,
        "pane_id": null,
        "command_id": null
    }"#;

    let null_name_parse_error = serde_json::from_str::<RecentEvent>(serialized_recent_event_json)
        .expect_err("a null name is refused");

    assert!(
        null_name_parse_error.to_string().contains("null"),
        "{null_name_parse_error}"
    );
}
