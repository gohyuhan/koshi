//! Tests for the recent-event record: every variant is named, every id it
//! carries is the one its payload holds, and no payload content reaches it.

use super::*;

use std::collections::BTreeSet;
use std::time::Duration;

use crate::command::CopyTarget;
use crate::event::tests::list_event_cases;
use crate::event::{
    CommandRejected, ConfigReloaded, Copied, EventClass, KeybindingMatched, MouseDragged,
    MousePressed, MouseReleased, MouseScrolled, PaneCommandFinished, PaneCreated, PaneEnterPressed,
    PaneFocused, PaneTyped, PluginBroken, PluginDisabled, PluginDoctorCompleted, PluginEnabled,
    PluginInstalled, PluginLoadFailed, PluginReloaded, PluginUninstalled, PluginUnloaded,
    PluginUpdated, QuitCause, RejectReason, SubmittedLinePayload, SubscriberLagged, TabFocused,
    TypedPayload,
};
use crate::geometry::Point;
use crate::mouse::{MouseButton, ScrollDirection};

/// A fixed instant, so an assertion never races the clock.
fn occurred_at() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

#[test]
fn every_event_variant_records_the_name_it_reports() {
    for (event, event_name, _event_class) in list_event_cases() {
        let recorded_event = record_event(&event, occurred_at());
        assert_eq!(recorded_event.event_name, Cow::Borrowed(event_name));
        assert_eq!(recorded_event.occurred_at, occurred_at());
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
        _ => BTreeSet::new(),
    }
}

#[test]
fn every_id_an_event_names_reaches_its_record_and_no_other_id_does() {
    for (event, event_name, _event_class) in list_event_cases() {
        let recorded_event = record_event(&event, occurred_at());
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

/// One instance per [`PluginEvent`] variant, with the plugin each names. The
/// array's length forces every variant to appear.
fn list_plugin_event_cases() -> [(PluginEvent, PluginId); 10] {
    let plugin_ids = [(); 10].map(|()| PluginId::new());
    [
        (
            PluginEvent::Installed(PluginInstalled {
                plugin_id: plugin_ids[0],
            }),
            plugin_ids[0],
        ),
        (
            PluginEvent::Uninstalled(PluginUninstalled {
                plugin_id: plugin_ids[1],
            }),
            plugin_ids[1],
        ),
        (
            PluginEvent::Enabled(PluginEnabled {
                plugin_id: plugin_ids[2],
            }),
            plugin_ids[2],
        ),
        (
            PluginEvent::Disabled(PluginDisabled {
                plugin_id: plugin_ids[3],
            }),
            plugin_ids[3],
        ),
        (
            PluginEvent::Updated(PluginUpdated {
                plugin_id: plugin_ids[4],
            }),
            plugin_ids[4],
        ),
        (
            PluginEvent::Reloaded(PluginReloaded {
                plugin_id: plugin_ids[5],
            }),
            plugin_ids[5],
        ),
        (
            PluginEvent::LoadFailed(PluginLoadFailed {
                plugin_id: plugin_ids[6],
                failure_reason: "no such file".to_string(),
            }),
            plugin_ids[6],
        ),
        (
            PluginEvent::Unloaded(PluginUnloaded {
                plugin_id: plugin_ids[7],
            }),
            plugin_ids[7],
        ),
        (
            PluginEvent::Broken(PluginBroken {
                plugin_id: plugin_ids[8],
                failure_reason: "wasm trap in activate".to_string(),
            }),
            plugin_ids[8],
        ),
        (
            PluginEvent::DoctorCompleted(PluginDoctorCompleted {
                plugin_id: plugin_ids[9],
            }),
            plugin_ids[9],
        ),
    ]
}

#[test]
fn every_plugin_fact_records_the_plugin_its_own_payload_names() {
    for (plugin_event, expected_plugin_id) in list_plugin_event_cases() {
        let recorded_event = record_event(&Event::Plugin(plugin_event), occurred_at());
        assert_eq!(recorded_event.plugin_id, Some(expected_plugin_id));
    }
}

#[test]
fn a_plugin_fact_records_no_word_of_the_reason_it_carries() {
    let recorded_event = record_event(
        &Event::Plugin(PluginEvent::LoadFailed(PluginLoadFailed {
            plugin_id: PluginId::new(),
            failure_reason: "/home/kim/.config/koshi/plugins/git.wasm is not a component"
                .to_string(),
        })),
        occurred_at(),
    );

    let encoded_json = serde_json::to_string(&recorded_event).unwrap();
    assert!(!encoded_json.contains("git.wasm"), "{encoded_json}");
    assert!(!encoded_json.contains("kim"), "{encoded_json}");
}

#[test]
fn a_pane_created_records_its_pane_and_tab_and_nothing_else() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let recorded_event = record_event(
        &Event::PaneCreated(PaneCreated { pane_id, tab_id }),
        occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: occurred_at(),
            event_name: Cow::Borrowed("PaneCreated"),
            session_id: None,
            client_id: None,
            tab_id: Some(tab_id),
            pane_id: Some(pane_id),
            plugin_id: None,
            command_id: None,
            subscriber_id: None,
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
        occurred_at(),
    );

    assert_eq!(recorded_event.client_id, Some(client_id));
    assert_eq!(recorded_event.tab_id, Some(tab_id));
    assert_eq!(recorded_event.pane_id, Some(pane_id));
    assert_ne!(recorded_event.pane_id, Some(previous_pane_id));
}

#[test]
fn a_mouse_press_outside_every_pane_records_no_pane() {
    let client_id = ClientId::new();

    let recorded_event = record_event(
        &Event::MousePressed(MousePressed {
            client_id,
            pane_id: None,
            position: Point { column: 0, row: 0 },
            button: MouseButton::Left,
        }),
        occurred_at(),
    );

    assert_eq!(recorded_event.client_id, Some(client_id));
    assert_eq!(recorded_event.pane_id, None);
}

#[test]
fn every_mouse_event_inside_a_pane_records_that_pane_and_its_client_and_nothing_else() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();
    let position = Point { column: 3, row: 4 };
    let mouse_events = [
        Event::MousePressed(MousePressed {
            client_id,
            pane_id: Some(pane_id),
            position,
            button: MouseButton::Left,
        }),
        Event::MouseReleased(MouseReleased {
            client_id,
            pane_id: Some(pane_id),
            position,
            button: MouseButton::Right,
        }),
        Event::MouseDragged(MouseDragged {
            client_id,
            pane_id: Some(pane_id),
            position,
            button: MouseButton::Middle,
        }),
        Event::MouseScrolled(MouseScrolled {
            client_id,
            pane_id: Some(pane_id),
            position,
            direction: ScrollDirection::Down,
        }),
    ];

    for event in &mouse_events {
        let recorded_event = record_event(event, occurred_at());
        assert_eq!(
            recorded_event,
            RecentEvent {
                occurred_at: occurred_at(),
                event_name: Cow::Borrowed(event.get_event_name()),
                session_id: None,
                client_id: Some(client_id),
                tab_id: None,
                pane_id: Some(pane_id),
                plugin_id: None,
                command_id: None,
                subscriber_id: None,
            },
            "{}",
            event.get_event_name()
        );
    }
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
        occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: occurred_at(),
            event_name: Cow::Borrowed("TabFocused"),
            session_id: None,
            client_id: Some(client_id),
            tab_id: Some(tab_id),
            pane_id: None,
            plugin_id: None,
            command_id: None,
            subscriber_id: None,
        }
    );
}

#[test]
fn a_keybinding_match_records_its_client_and_command() {
    let client_id = ClientId::new();
    let command_id = CommandId::new();

    let recorded_event = record_event(
        &Event::KeybindingMatched(KeybindingMatched {
            client_id,
            command_id,
        }),
        occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: occurred_at(),
            event_name: Cow::Borrowed("KeybindingMatched"),
            session_id: None,
            client_id: Some(client_id),
            tab_id: None,
            pane_id: None,
            plugin_id: None,
            command_id: Some(command_id),
            subscriber_id: None,
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
        occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: occurred_at(),
            event_name: Cow::Borrowed("PaneCommandFinished"),
            session_id: None,
            client_id: None,
            tab_id: None,
            pane_id: Some(pane_id),
            plugin_id: None,
            command_id: None,
            subscriber_id: None,
        }
    );
}

#[test]
fn a_config_reload_records_its_session() {
    let session_id = SessionId::new();

    let recorded_event = record_event(
        &Event::ConfigReloaded(ConfigReloaded { session_id }),
        occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: occurred_at(),
            event_name: Cow::Borrowed("ConfigReloaded"),
            session_id: Some(session_id),
            client_id: None,
            tab_id: None,
            pane_id: None,
            plugin_id: None,
            command_id: None,
            subscriber_id: None,
        }
    );
}

#[test]
fn a_plugin_fact_records_the_plugin_it_names() {
    let plugin_id = PluginId::new();

    let recorded_event = record_event(
        &Event::Plugin(PluginEvent::Broken(PluginBroken {
            plugin_id,
            failure_reason: "wasm trap in activate".to_string(),
        })),
        occurred_at(),
    );

    assert_eq!(recorded_event.event_name, Cow::Borrowed("Plugin"));
    assert_eq!(recorded_event.plugin_id, Some(plugin_id));
}

#[test]
fn a_lagged_subscriber_records_its_subscriber_id() {
    let subscriber_id = SubscriberId::new();

    let recorded_event = record_event(
        &Event::SubscriberLagged(SubscriberLagged {
            subscriber_id,
            dropped_event_count: 7,
            event_class: EventClass::Lossy,
        }),
        occurred_at(),
    );

    assert_eq!(recorded_event.subscriber_id, Some(subscriber_id));
    assert_eq!(recorded_event.client_id, None);
}

#[test]
fn a_rejected_command_records_its_command_id() {
    let command_id = CommandId::new();

    let recorded_event = record_event(
        &Event::CommandRejected(CommandRejected {
            command_id,
            rejection_reason: RejectReason::TargetNotFound,
        }),
        occurred_at(),
    );

    assert_eq!(recorded_event.command_id, Some(command_id));
}

#[test]
fn a_copy_records_the_client_and_pane_but_no_byte_count() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();

    let recorded_event = record_event(
        &Event::Copied(Copied {
            client_id,
            pane_id,
            clipboard_target: CopyTarget::Osc52,
            byte_count: 4096,
        }),
        occurred_at(),
    );

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: occurred_at(),
            event_name: Cow::Borrowed("Copied"),
            session_id: None,
            client_id: Some(client_id),
            tab_id: None,
            pane_id: Some(pane_id),
            plugin_id: None,
            command_id: None,
            subscriber_id: None,
        }
    );
}

#[test]
fn a_quit_records_a_name_and_no_id_at_all() {
    let recorded_event = record_event(&Event::Quit(QuitCause::Requested), occurred_at());

    assert_eq!(
        recorded_event,
        RecentEvent {
            occurred_at: occurred_at(),
            event_name: Cow::Borrowed("Quit"),
            session_id: None,
            client_id: None,
            tab_id: None,
            pane_id: None,
            plugin_id: None,
            command_id: None,
            subscriber_id: None,
        }
    );
}

#[test]
fn a_typed_character_records_its_ids_and_never_the_character() {
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    let recorded_event = record_event(
        &Event::PaneTyped(PaneTyped {
            pane_id,
            tab_id,
            session_id,
            client_id,
            typed_payload: TypedPayload::SafePublic('q'),
            accepted_at: occurred_at(),
        }),
        occurred_at(),
    );

    assert_eq!(recorded_event.session_id, Some(session_id));
    assert_eq!(recorded_event.client_id, Some(client_id));
    assert_eq!(recorded_event.tab_id, Some(tab_id));
    assert_eq!(recorded_event.pane_id, Some(pane_id));

    let recorded_json = serde_json::to_string(&recorded_event).unwrap();
    assert!(!recorded_json.contains('q'), "{recorded_json}");
    assert!(!recorded_json.contains("SafePublic"), "{recorded_json}");
}

#[test]
fn a_submitted_line_records_its_ids_and_never_the_line() {
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    let recorded_event = record_event(
        &Event::PaneEnterPressed(PaneEnterPressed {
            pane_id,
            tab_id,
            session_id,
            client_id,
            submitted_line: SubmittedLinePayload::SafePublic("mysql -u root -phunter2".to_string()),
            accepted_at: occurred_at(),
        }),
        occurred_at(),
    );

    assert_eq!(recorded_event.pane_id, Some(pane_id));

    let recorded_json = serde_json::to_string(&recorded_event).unwrap();
    assert!(!recorded_json.contains("hunter2"), "{recorded_json}");
    assert!(!recorded_json.contains("mysql"), "{recorded_json}");
}

#[test]
fn a_record_survives_the_wire_with_an_owned_name() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let recorded_event = record_event(
        &Event::PaneCreated(PaneCreated { pane_id, tab_id }),
        occurred_at(),
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
        "at": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
        "name": "PaneOpenedSideways",
        "session": null,
        "client": null,
        "tab": null,
        "pane": null,
        "plugin": null,
        "command": null,
        "subscriber": null,
        "workspace": "w-1"
    }"#;

    let decoded_recent_event: RecentEvent =
        serde_json::from_str(serialized_recent_event_json).expect("an added field is ignored");

    assert_eq!(
        decoded_recent_event.event_name,
        Cow::Borrowed("PaneOpenedSideways")
    );
    assert_eq!(decoded_recent_event.occurred_at, occurred_at());
}

#[test]
fn a_record_whose_time_cannot_be_represented_is_refused_and_does_not_panic() {
    let serialized_recent_event_json = r#"{
        "at": {"secs_since_epoch": 18446744073709551615, "nanos_since_epoch": 999999999},
        "name": "PaneCreated",
        "session": null,
        "client": null,
        "tab": null,
        "pane": null,
        "plugin": null,
        "command": null,
        "subscriber": null
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
        "at": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
        "name": "PaneCreated",
        "session": null,
        "client": null,
        "tab": null,
        "pane": null,
        "plugin": null,
        "command": null
    }"#;

    let decoded_recent_event: RecentEvent = serde_json::from_str(serialized_recent_event_json)
        .expect("an absent id field reads as none");

    assert_eq!(decoded_recent_event.subscriber_id, None);
    assert_eq!(decoded_recent_event.event_name, "PaneCreated");
}

#[test]
fn a_record_missing_the_time_or_the_name_is_refused() {
    let missing_time_json = r#"{"name": "Quit", "session": null, "client": null, "tab": null,
        "pane": null, "plugin": null, "command": null, "subscriber": null}"#;
    let missing_name_json = r#"{"at": {"secs_since_epoch": 1, "nanos_since_epoch": 0}, "session": null,
        "client": null, "tab": null, "pane": null, "plugin": null, "command": null,
        "subscriber": null}"#;

    let missing_time_parse_error =
        serde_json::from_str::<RecentEvent>(missing_time_json).expect_err("a record needs a time");
    assert!(
        missing_time_parse_error.to_string().contains("at"),
        "{missing_time_parse_error}"
    );

    let missing_name_parse_error =
        serde_json::from_str::<RecentEvent>(missing_name_json).expect_err("a record needs a name");
    assert!(
        missing_name_parse_error.to_string().contains("name"),
        "{missing_name_parse_error}"
    );
}

#[test]
fn a_record_whose_name_is_not_an_event_this_build_has_still_reads() {
    let serialized_recent_event_json = r#"{
        "at": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
        "name": "PaneOpenedSideways",
        "session": null,
        "client": null,
        "tab": null,
        "pane": null,
        "plugin": null,
        "command": null,
        "subscriber": null
    }"#;

    let decoded_recent_event: RecentEvent =
        serde_json::from_str(serialized_recent_event_json).expect("an unknown name still reads");

    assert_eq!(decoded_recent_event.event_name, "PaneOpenedSideways");
}

#[test]
fn a_record_serializes_with_its_field_names_in_order() {
    let recorded_event = record_event(&Event::Quit(QuitCause::Requested), occurred_at());

    assert_eq!(
        serde_json::to_string(&recorded_event).unwrap(),
        r#"{"at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},"name":"Quit","session":null,"client":null,"tab":null,"pane":null,"plugin":null,"command":null,"subscriber":null}"#
    );
}

#[test]
fn a_record_whose_name_is_null_is_refused() {
    let serialized_recent_event_json = r#"{
        "at": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
        "name": null,
        "session": null,
        "client": null,
        "tab": null,
        "pane": null,
        "plugin": null,
        "command": null,
        "subscriber": null
    }"#;

    let null_name_parse_error = serde_json::from_str::<RecentEvent>(serialized_recent_event_json)
        .expect_err("a null name is refused");

    assert!(
        null_name_parse_error.to_string().contains("null"),
        "{null_name_parse_error}"
    );
}
