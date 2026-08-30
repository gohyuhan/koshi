//! Tests for the recent-event record: every variant is named, every id it
//! carries is the one its payload holds, and no payload content reaches it.

use super::*;

use std::collections::BTreeSet;
use std::time::Duration;

use crate::command::CopyTarget;
use crate::event::tests::event_cases;
use crate::event::{
    CommandRejected, ConfigReloaded, Copied, EventClass, KeybindingMatched, MouseDragged,
    MousePressed, MouseReleased, MouseScrolled, PaneCommandFinished, PaneCreated, PaneEnterPressed,
    PaneFocused, PaneTyped, PluginBroken, PluginDisabled, PluginDoctorCompleted, PluginEnabled,
    PluginInstalled, PluginLoadFailed, PluginReloaded, PluginUninstalled, PluginUnloaded,
    PluginUpdated, RejectReason, SubmittedLinePayload, SubscriberLagged, TabFocused, TypedPayload,
};
use crate::geometry::Point;
use crate::mouse::{MouseButton, ScrollDirection};

/// A fixed instant, so an assertion never races the clock.
fn at() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

#[test]
fn every_event_variant_records_the_name_it_reports() {
    for (event, name, _class) in event_cases() {
        let recorded = record(&event, at());
        assert_eq!(recorded.name, Cow::Borrowed(name));
        assert_eq!(recorded.at, at());
    }
}

/// Every UUID `value` holds, anywhere inside it. Every typed id serializes as
/// a bare UUID string, so this finds each one an event or a record names.
fn ids_in(value: &serde_json::Value) -> BTreeSet<String> {
    match value {
        serde_json::Value::String(text) => match uuid::Uuid::parse_str(text) {
            Ok(_) => BTreeSet::from([text.clone()]),
            Err(_) => BTreeSet::new(),
        },
        serde_json::Value::Array(items) => items.iter().flat_map(ids_in).collect(),
        serde_json::Value::Object(fields) => fields.values().flat_map(ids_in).collect(),
        _ => BTreeSet::new(),
    }
}

/// The ids `event` names that its record deliberately leaves out: the place a
/// focus change came from. A record holds one id per kind, so the pane and tab
/// a client just left have nowhere to go.
fn ids_left_out(event: &Event) -> BTreeSet<String> {
    match event {
        Event::PaneFocused(payload) => payload
            .prior_pane
            .iter()
            .map(ToString::to_string)
            .map(|id| id.trim_start_matches("pane-").to_string())
            .collect(),
        Event::TabFocused(payload) => BTreeSet::from([payload
            .prior_tab
            .to_string()
            .trim_start_matches("tab-")
            .to_string()]),
        _ => BTreeSet::new(),
    }
}

#[test]
fn every_id_an_event_names_reaches_its_record_and_no_other_id_does() {
    for (event, name, _class) in event_cases() {
        let recorded = record(&event, at());
        let in_event = ids_in(&serde_json::to_value(&event).unwrap());
        let in_record = ids_in(&serde_json::to_value(&recorded).unwrap());

        assert!(
            in_record.is_subset(&in_event),
            "{name} records an id its event never named: {:?}",
            in_record.difference(&in_event).collect::<Vec<_>>()
        );

        let dropped: BTreeSet<String> = in_event.difference(&in_record).cloned().collect();
        assert_eq!(dropped, ids_left_out(&event), "{name} drops the wrong ids");
    }
}

/// One instance per [`PluginEvent`] variant, with the plugin each names. The
/// array's length forces every variant to appear.
fn plugin_cases() -> [(PluginEvent, PluginId); 10] {
    let ids = [(); 10].map(|()| PluginId::new());
    [
        (
            PluginEvent::Installed(PluginInstalled { plugin_id: ids[0] }),
            ids[0],
        ),
        (
            PluginEvent::Uninstalled(PluginUninstalled { plugin_id: ids[1] }),
            ids[1],
        ),
        (
            PluginEvent::Enabled(PluginEnabled { plugin_id: ids[2] }),
            ids[2],
        ),
        (
            PluginEvent::Disabled(PluginDisabled { plugin_id: ids[3] }),
            ids[3],
        ),
        (
            PluginEvent::Updated(PluginUpdated { plugin_id: ids[4] }),
            ids[4],
        ),
        (
            PluginEvent::Reloaded(PluginReloaded { plugin_id: ids[5] }),
            ids[5],
        ),
        (
            PluginEvent::LoadFailed(PluginLoadFailed {
                plugin_id: ids[6],
                reason: "no such file".to_string(),
            }),
            ids[6],
        ),
        (
            PluginEvent::Unloaded(PluginUnloaded { plugin_id: ids[7] }),
            ids[7],
        ),
        (
            PluginEvent::Broken(PluginBroken {
                plugin_id: ids[8],
                reason: "wasm trap in activate".to_string(),
            }),
            ids[8],
        ),
        (
            PluginEvent::DoctorCompleted(PluginDoctorCompleted { plugin_id: ids[9] }),
            ids[9],
        ),
    ]
}

#[test]
fn every_plugin_fact_records_the_plugin_its_own_payload_names() {
    for (plugin_event, expected) in plugin_cases() {
        let recorded = record(&Event::Plugin(plugin_event), at());
        assert_eq!(recorded.plugin, Some(expected));
    }
}

#[test]
fn a_plugin_fact_records_no_word_of_the_reason_it_carries() {
    let recorded = record(
        &Event::Plugin(PluginEvent::LoadFailed(PluginLoadFailed {
            plugin_id: PluginId::new(),
            reason: "/home/kim/.config/koshi/plugins/git.wasm is not a component".to_string(),
        })),
        at(),
    );

    let encoded = serde_json::to_string(&recorded).unwrap();
    assert!(!encoded.contains("git.wasm"), "{encoded}");
    assert!(!encoded.contains("kim"), "{encoded}");
}

#[test]
fn a_pane_created_records_its_pane_and_tab_and_nothing_else() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let recorded = record(&Event::PaneCreated(PaneCreated { pane_id, tab_id }), at());

    assert_eq!(
        recorded,
        RecentEvent {
            at: at(),
            name: Cow::Borrowed("PaneCreated"),
            session: None,
            client: None,
            tab: Some(tab_id),
            pane: Some(pane_id),
            plugin: None,
            command: None,
            subscriber: None,
        }
    );
}

#[test]
fn a_pane_focused_records_its_client_tab_and_pane_but_not_the_pane_it_left() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let prior_pane = PaneId::new();

    let recorded = record(
        &Event::PaneFocused(PaneFocused {
            client_id,
            tab_id,
            pane_id,
            prior_pane: Some(prior_pane),
        }),
        at(),
    );

    assert_eq!(recorded.client, Some(client_id));
    assert_eq!(recorded.tab, Some(tab_id));
    assert_eq!(recorded.pane, Some(pane_id));
    assert_ne!(recorded.pane, Some(prior_pane));
}

#[test]
fn a_mouse_press_outside_every_pane_records_no_pane() {
    let client_id = ClientId::new();

    let recorded = record(
        &Event::MousePressed(MousePressed {
            client_id,
            pane: None,
            position: Point { x: 0, y: 0 },
            button: MouseButton::Left,
        }),
        at(),
    );

    assert_eq!(recorded.client, Some(client_id));
    assert_eq!(recorded.pane, None);
}

#[test]
fn every_mouse_event_inside_a_pane_records_that_pane_and_its_client_and_nothing_else() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();
    let position = Point { x: 3, y: 4 };
    let events = [
        Event::MousePressed(MousePressed {
            client_id,
            pane: Some(pane_id),
            position,
            button: MouseButton::Left,
        }),
        Event::MouseReleased(MouseReleased {
            client_id,
            pane: Some(pane_id),
            position,
            button: MouseButton::Right,
        }),
        Event::MouseDragged(MouseDragged {
            client_id,
            pane: Some(pane_id),
            position,
            button: MouseButton::Middle,
        }),
        Event::MouseScrolled(MouseScrolled {
            client_id,
            pane: Some(pane_id),
            position,
            direction: ScrollDirection::Down,
        }),
    ];

    for event in &events {
        let recorded = record(event, at());
        assert_eq!(
            recorded,
            RecentEvent {
                at: at(),
                name: Cow::Borrowed(event.name()),
                session: None,
                client: Some(client_id),
                tab: None,
                pane: Some(pane_id),
                plugin: None,
                command: None,
                subscriber: None,
            },
            "{}",
            event.name()
        );
    }
}

#[test]
fn a_tab_focused_records_its_client_and_tab_but_not_the_tab_it_left() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let prior_tab = TabId::new();

    let recorded = record(
        &Event::TabFocused(TabFocused {
            client_id,
            tab_id,
            prior_tab,
        }),
        at(),
    );

    assert_eq!(
        recorded,
        RecentEvent {
            at: at(),
            name: Cow::Borrowed("TabFocused"),
            session: None,
            client: Some(client_id),
            tab: Some(tab_id),
            pane: None,
            plugin: None,
            command: None,
            subscriber: None,
        }
    );
}

#[test]
fn a_keybinding_match_records_its_client_and_command() {
    let client_id = ClientId::new();
    let command_id = CommandId::new();

    let recorded = record(
        &Event::KeybindingMatched(KeybindingMatched {
            client_id,
            command_id,
        }),
        at(),
    );

    assert_eq!(
        recorded,
        RecentEvent {
            at: at(),
            name: Cow::Borrowed("KeybindingMatched"),
            session: None,
            client: Some(client_id),
            tab: None,
            pane: None,
            plugin: None,
            command: Some(command_id),
            subscriber: None,
        }
    );
}

#[test]
fn a_finished_command_records_its_pane_and_no_exit_code() {
    let pane_id = PaneId::new();

    let recorded = record(
        &Event::PaneCommandFinished(PaneCommandFinished {
            pane_id,
            exit_code: Some(127),
        }),
        at(),
    );

    assert_eq!(
        recorded,
        RecentEvent {
            at: at(),
            name: Cow::Borrowed("PaneCommandFinished"),
            session: None,
            client: None,
            tab: None,
            pane: Some(pane_id),
            plugin: None,
            command: None,
            subscriber: None,
        }
    );
}

#[test]
fn a_config_reload_records_its_session() {
    let session_id = SessionId::new();

    let recorded = record(&Event::ConfigReloaded(ConfigReloaded { session_id }), at());

    assert_eq!(
        recorded,
        RecentEvent {
            at: at(),
            name: Cow::Borrowed("ConfigReloaded"),
            session: Some(session_id),
            client: None,
            tab: None,
            pane: None,
            plugin: None,
            command: None,
            subscriber: None,
        }
    );
}

#[test]
fn a_plugin_fact_records_the_plugin_it_names() {
    let plugin_id = PluginId::new();

    let recorded = record(
        &Event::Plugin(PluginEvent::Broken(PluginBroken {
            plugin_id,
            reason: "wasm trap in activate".to_string(),
        })),
        at(),
    );

    assert_eq!(recorded.name, Cow::Borrowed("Plugin"));
    assert_eq!(recorded.plugin, Some(plugin_id));
}

#[test]
fn a_lagged_subscriber_records_its_subscriber_id() {
    let subscriber_id = SubscriberId::new();

    let recorded = record(
        &Event::SubscriberLagged(SubscriberLagged {
            subscriber_id,
            dropped_count: 7,
            event_class: EventClass::Lossy,
        }),
        at(),
    );

    assert_eq!(recorded.subscriber, Some(subscriber_id));
    assert_eq!(recorded.client, None);
}

#[test]
fn a_rejected_command_records_its_command_id() {
    let id = CommandId::new();

    let recorded = record(
        &Event::CommandRejected(CommandRejected {
            id,
            reason: RejectReason::TargetNotFound,
        }),
        at(),
    );

    assert_eq!(recorded.command, Some(id));
}

#[test]
fn a_copy_records_the_client_and_pane_but_no_byte_count() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();

    let recorded = record(
        &Event::Copied(Copied {
            client_id,
            pane_id,
            target: CopyTarget::Osc52,
            byte_len: 4096,
        }),
        at(),
    );

    assert_eq!(
        recorded,
        RecentEvent {
            at: at(),
            name: Cow::Borrowed("Copied"),
            session: None,
            client: Some(client_id),
            tab: None,
            pane: Some(pane_id),
            plugin: None,
            command: None,
            subscriber: None,
        }
    );
}

#[test]
fn a_quit_records_a_name_and_no_id_at_all() {
    let recorded = record(&Event::Quit, at());

    assert_eq!(
        recorded,
        RecentEvent {
            at: at(),
            name: Cow::Borrowed("Quit"),
            session: None,
            client: None,
            tab: None,
            pane: None,
            plugin: None,
            command: None,
            subscriber: None,
        }
    );
}

#[test]
fn a_typed_character_records_its_ids_and_never_the_character() {
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    let recorded = record(
        &Event::PaneTyped(PaneTyped {
            pane_id,
            tab_id,
            session_id,
            client_id,
            payload: TypedPayload::SafePublic('q'),
            timestamp: at(),
        }),
        at(),
    );

    assert_eq!(recorded.session, Some(session_id));
    assert_eq!(recorded.client, Some(client_id));
    assert_eq!(recorded.tab, Some(tab_id));
    assert_eq!(recorded.pane, Some(pane_id));

    let encoded = serde_json::to_string(&recorded).unwrap();
    assert!(!encoded.contains('q'), "{encoded}");
    assert!(!encoded.contains("SafePublic"), "{encoded}");
}

#[test]
fn a_submitted_line_records_its_ids_and_never_the_line() {
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    let recorded = record(
        &Event::PaneEnterPressed(PaneEnterPressed {
            pane_id,
            tab_id,
            session_id,
            client_id,
            line: SubmittedLinePayload::SafePublic("mysql -u root -phunter2".to_string()),
            timestamp: at(),
        }),
        at(),
    );

    assert_eq!(recorded.pane, Some(pane_id));

    let encoded = serde_json::to_string(&recorded).unwrap();
    assert!(!encoded.contains("hunter2"), "{encoded}");
    assert!(!encoded.contains("mysql"), "{encoded}");
}

#[test]
fn a_record_survives_the_wire_with_an_owned_name() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let recorded = record(&Event::PaneCreated(PaneCreated { pane_id, tab_id }), at());
    assert!(matches!(recorded.name, Cow::Borrowed(_)));

    let decoded: RecentEvent = serde_json::from_str(&serde_json::to_string(&recorded).unwrap())
        .expect("a record this build wrote reads back");

    assert_eq!(decoded, recorded);
    assert!(matches!(decoded.name, Cow::Owned(_)));
}

#[test]
fn a_record_from_a_newer_koshi_reads_with_the_field_it_adds_ignored() {
    let encoded = r#"{
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

    let decoded: RecentEvent = serde_json::from_str(encoded).expect("an added field is ignored");

    assert_eq!(decoded.name, Cow::Borrowed("PaneOpenedSideways"));
    assert_eq!(decoded.at, at());
}

#[test]
fn a_record_whose_time_cannot_be_represented_is_refused_and_does_not_panic() {
    let encoded = r#"{
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

    let refusal = serde_json::from_str::<RecentEvent>(encoded)
        .expect_err("a time past the clock's range is refused");

    assert!(refusal.to_string().contains("SystemTime"), "{refusal}");
}

#[test]
fn a_record_missing_an_id_field_reads_it_as_absent() {
    let encoded = r#"{
        "at": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
        "name": "PaneCreated",
        "session": null,
        "client": null,
        "tab": null,
        "pane": null,
        "plugin": null,
        "command": null
    }"#;

    let decoded: RecentEvent =
        serde_json::from_str(encoded).expect("an absent id field reads as none");

    assert_eq!(decoded.subscriber, None);
    assert_eq!(decoded.name, "PaneCreated");
}

#[test]
fn a_record_missing_the_time_or_the_name_is_refused() {
    let no_time = r#"{"name": "Quit", "session": null, "client": null, "tab": null,
        "pane": null, "plugin": null, "command": null, "subscriber": null}"#;
    let no_name = r#"{"at": {"secs_since_epoch": 1, "nanos_since_epoch": 0}, "session": null,
        "client": null, "tab": null, "pane": null, "plugin": null, "command": null,
        "subscriber": null}"#;

    let refusal = serde_json::from_str::<RecentEvent>(no_time).expect_err("a record needs a time");
    assert!(refusal.to_string().contains("at"), "{refusal}");

    let refusal = serde_json::from_str::<RecentEvent>(no_name).expect_err("a record needs a name");
    assert!(refusal.to_string().contains("name"), "{refusal}");
}

#[test]
fn a_record_whose_name_is_not_an_event_this_build_has_still_reads() {
    let encoded = r#"{
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

    let decoded: RecentEvent = serde_json::from_str(encoded).expect("an unknown name still reads");

    assert_eq!(decoded.name, "PaneOpenedSideways");
}

#[test]
fn a_record_serializes_with_its_field_names_in_order() {
    let recorded = record(&Event::Quit, at());

    assert_eq!(
        serde_json::to_string(&recorded).unwrap(),
        r#"{"at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},"name":"Quit","session":null,"client":null,"tab":null,"pane":null,"plugin":null,"command":null,"subscriber":null}"#
    );
}

#[test]
fn a_record_whose_name_is_null_is_refused() {
    let encoded = r#"{
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

    let refusal = serde_json::from_str::<RecentEvent>(encoded).expect_err("a null name is refused");

    assert!(refusal.to_string().contains("null"), "{refusal}");
}
