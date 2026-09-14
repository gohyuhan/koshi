//! Tests for the discovery serde forms: the timestamp epoch pair and lossy
//! path serialization.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use super::*;

/// The fixed UUID every fake id uses.
fn build_fixed_uuid() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// A session created at `created_at`, with fixed everything else.
fn build_session_discovery(created_at: SystemTime) -> SessionDiscovery {
    SessionDiscovery {
        session_id: SessionId::from_uuid(build_fixed_uuid()),
        session_name: "quiet-lake".to_string(),
        created_at,
        attached_client_ids: Vec::new(),
        pane_count: 0,
    }
}

/// A pane whose working directory is `working_directory`, with fixed everything else.
fn build_pane_discovery(working_directory: Option<PathBuf>) -> PaneDiscovery {
    PaneDiscovery {
        pane_id: PaneId::from_uuid(build_fixed_uuid()),
        tab_id: TabId::from_uuid(build_fixed_uuid()),
        session_id: SessionId::from_uuid(build_fixed_uuid()),
        pane_title: None,
        working_directory,
        command_argv: None,
        lifecycle: PaneLifecycle::Running,
        focused_by_client_ids: Vec::new(),
    }
}

/// A path containing bytes that are not valid UTF-8; its lossy form is
/// `/tmp/f\u{FFFD}oo` on every platform.
fn build_non_utf8_path() -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/f\x80oo".to_vec()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        // `/tmp/f` + an unpaired surrogate (invalid UTF-16) + `oo`.
        PathBuf::from(std::ffi::OsString::from_wide(&[
            0x2F, 0x74, 0x6D, 0x70, 0x2F, 0x66, 0xD800, 0x6F, 0x6F,
        ]))
    }
}

#[test]
fn time_serializes_as_its_flat_epoch_pair() {
    // 500 ns is a multiple of 100 ns — the resolution of a Windows `SystemTime`
    // (a `FILETIME`) — so the value survives on every platform.
    let session_discovery =
        build_session_discovery(SystemTime::UNIX_EPOCH + Duration::new(1234, 500));

    let serialized_discovery = serde_json::to_value(&session_discovery).expect("serializes");

    assert_eq!(
        serialized_discovery["created_at"],
        json!({"secs_since_epoch": 1234, "nanos_since_epoch": 500})
    );
}

#[test]
fn times_round_trip_through_json() {
    let session_discovery =
        build_session_discovery(SystemTime::UNIX_EPOCH + Duration::new(1234, 500));

    let serialized_discovery = serde_json::to_value(&session_discovery).expect("serializes");
    let decoded_discovery: SessionDiscovery =
        serde_json::from_value(serialized_discovery).expect("deserializes");

    assert_eq!(decoded_discovery, session_discovery);
}

#[test]
fn non_utf8_working_directory_serializes_as_its_lossy_string() {
    let pane_discovery = build_pane_discovery(Some(build_non_utf8_path()));

    let serialized_discovery = serde_json::to_value(&pane_discovery).expect("serializes");

    assert_eq!(serialized_discovery["cwd"], json!("/tmp/f\u{FFFD}oo"));
}

#[test]
fn absent_working_directory_serializes_as_null() {
    let pane_discovery = build_pane_discovery(None);

    let serialized_discovery = serde_json::to_value(&pane_discovery).expect("serializes");

    assert_eq!(serialized_discovery["cwd"], serde_json::Value::Null);
}

#[test]
fn valid_utf8_working_directory_serializes_as_its_plain_string() {
    let pane_discovery = build_pane_discovery(Some(PathBuf::from("/home/user/project")));

    let serialized_discovery = serde_json::to_value(&pane_discovery).expect("serializes");

    assert_eq!(serialized_discovery["cwd"], json!("/home/user/project"));
}

#[test]
fn pane_lifecycle_serializes_with_snake_case_names() {
    assert_eq!(
        serde_json::to_value(PaneLifecycle::Spawning).expect("serializes"),
        json!("spawning")
    );
    assert_eq!(
        serde_json::to_value(PaneLifecycle::Running).expect("serializes"),
        json!("running")
    );
    assert_eq!(
        serde_json::to_value(PaneLifecycle::Closing).expect("serializes"),
        json!("closing")
    );
    assert_eq!(
        serde_json::to_value(PaneLifecycle::Exited { exit_code: Some(1) }).expect("serializes"),
        json!({"exited": {"code": 1}})
    );
    assert_eq!(
        serde_json::to_value(PaneLifecycle::Exited { exit_code: None }).expect("serializes"),
        json!({"exited": {"code": null}})
    );
}

/// A client row whose origin is `origin`, with fixed everything else.
fn build_client_discovery(origin: Option<ClientOrigin>) -> ClientDiscovery {
    ClientDiscovery {
        client_id: ClientId::from_uuid(build_fixed_uuid()),
        session_id: SessionId::from_uuid(build_fixed_uuid()),
        attached_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        viewport_size: Size {
            column_count: 80,
            row_count: 24,
        },
        active_tab_id: TabId::from_uuid(build_fixed_uuid()),
        focused_pane_id: None,
        lock_mode: LockMode::Normal,
        origin,
        pane_area: None,
    }
}

#[test]
fn client_discovery_json_without_pane_area_decodes_as_none() {
    let client_discovery = ClientDiscovery {
        pane_area: Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 22,
        })),
        ..build_client_discovery(None)
    };
    let mut client_json = serde_json::to_value(&client_discovery).expect("serialize");
    client_json
        .as_object_mut()
        .expect("a client row is a JSON object")
        .remove("pane_area")
        .expect("the row carries a `pane_area` field to remove");

    let decoded_client: ClientDiscovery = serde_json::from_value(client_json).expect("deserialize");

    assert_eq!(decoded_client, build_client_discovery(None));
}

#[test]
fn a_client_row_carrying_no_origin_field_reads_as_unanswered_never_as_local() {
    let mut client_json =
        serde_json::to_value(build_client_discovery(Some(ClientOrigin::Local))).expect("serialize");
    client_json
        .as_object_mut()
        .expect("a client row is a JSON object")
        .remove("origin")
        .expect("the row carries an `origin` field to remove");

    let decoded_client: ClientDiscovery = serde_json::from_value(client_json).expect("deserialize");

    assert_eq!(decoded_client.origin, None);
    assert_eq!(decoded_client, build_client_discovery(None));
}

#[test]
fn a_client_row_stating_its_origin_keeps_that_answer() {
    for origin in [ClientOrigin::Local, ClientOrigin::Remote] {
        let client_json =
            serde_json::to_value(build_client_discovery(Some(origin))).expect("serialize");
        let decoded_client: ClientDiscovery =
            serde_json::from_value(client_json).expect("deserialize");
        assert_eq!(decoded_client.origin, Some(origin), "{origin:?}");
    }
}

#[test]
fn pane_lifecycle_round_trips_through_json_for_every_variant() {
    for pane_lifecycle in [
        PaneLifecycle::Spawning,
        PaneLifecycle::Running,
        PaneLifecycle::Closing,
        PaneLifecycle::Exited {
            exit_code: Some(137),
        },
        PaneLifecycle::Exited { exit_code: None },
    ] {
        let lifecycle_json = serde_json::to_string(&pane_lifecycle).expect("serialize");
        let decoded_lifecycle: PaneLifecycle =
            serde_json::from_str(&lifecycle_json).expect("deserialize");
        assert_eq!(pane_lifecycle, decoded_lifecycle, "{lifecycle_json}");
    }
}

#[test]
fn a_non_utf8_working_directory_decodes_as_its_lossy_path() {
    let serialized_discovery =
        serde_json::to_value(build_pane_discovery(Some(build_non_utf8_path())))
            .expect("serializes");

    let decoded_pane_discovery: PaneDiscovery =
        serde_json::from_value(serialized_discovery).expect("deserializes");

    assert_eq!(
        decoded_pane_discovery.working_directory,
        Some(PathBuf::from("/tmp/f\u{FFFD}oo"))
    );
}

#[test]
fn pane_discovery_round_trips_with_every_optional_field_set() {
    let pane_discovery = PaneDiscovery {
        pane_title: Some("vim".to_string()),
        command_argv: Some(vec!["htop".to_string(), "-d".to_string()]),
        lifecycle: PaneLifecycle::Exited { exit_code: Some(0) },
        focused_by_client_ids: vec![ClientId::from_uuid(build_fixed_uuid())],
        ..build_pane_discovery(Some(PathBuf::from("/home/user/project")))
    };

    let pane_json = serde_json::to_string(&pane_discovery).expect("serializes");
    let decoded_discovery: PaneDiscovery = serde_json::from_str(&pane_json).expect("deserializes");

    assert_eq!(decoded_discovery, pane_discovery);
}

#[test]
fn tab_discovery_round_trips_through_json() {
    let tab_discovery = TabDiscovery {
        tab_id: TabId::from_uuid(build_fixed_uuid()),
        session_id: SessionId::from_uuid(build_fixed_uuid()),
        tab_name: "amber-fox".to_string(),
        tab_index: 2,
        active_pane_id: Some(PaneId::from_uuid(build_fixed_uuid())),
        pane_count: 3,
    };

    let tab_json = serde_json::to_string(&tab_discovery).expect("serializes");
    let decoded_discovery: TabDiscovery = serde_json::from_str(&tab_json).expect("deserializes");

    assert_eq!(decoded_discovery, tab_discovery);
}

#[test]
fn client_discovery_round_trips_with_every_optional_field_set() {
    let client_discovery = ClientDiscovery {
        focused_pane_id: Some(PaneId::from_uuid(build_fixed_uuid())),
        lock_mode: LockMode::Locked,
        pane_area: Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 22,
        })),
        ..build_client_discovery(Some(ClientOrigin::Remote))
    };

    let client_json = serde_json::to_string(&client_discovery).expect("serializes");
    let decoded_discovery: ClientDiscovery =
        serde_json::from_str(&client_json).expect("deserializes");

    assert_eq!(decoded_discovery, client_discovery);
}

#[test]
fn a_starving_pane_area_round_trips_through_json() {
    let client_discovery = ClientDiscovery {
        pane_area: Some(PaneArea::Starving),
        ..build_client_discovery(None)
    };

    let client_json = serde_json::to_string(&client_discovery).expect("serializes");
    let decoded_discovery: ClientDiscovery =
        serde_json::from_str(&client_json).expect("deserializes");

    assert_eq!(decoded_discovery.pane_area, Some(PaneArea::Starving));
    assert_eq!(decoded_discovery, client_discovery);
}

#[test]
fn session_overview_round_trips_through_json() {
    let overview = SessionOverview {
        session: SessionDiscovery {
            attached_client_ids: vec![ClientId::from_uuid(build_fixed_uuid())],
            pane_count: 1,
            ..build_session_discovery(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        },
        tabs: vec![TabDiscovery {
            tab_id: TabId::from_uuid(build_fixed_uuid()),
            session_id: SessionId::from_uuid(build_fixed_uuid()),
            tab_name: "amber-fox".to_string(),
            tab_index: 0,
            active_pane_id: None,
            pane_count: 1,
        }],
        panes: vec![build_pane_discovery(None)],
        clients: vec![build_client_discovery(Some(ClientOrigin::Local))],
    };

    let overview_json = serde_json::to_string(&overview).expect("serializes");
    let decoded_overview: SessionOverview =
        serde_json::from_str(&overview_json).expect("deserializes");

    assert_eq!(decoded_overview, overview);
}

#[test]
fn an_empty_session_overview_round_trips_through_json() {
    let overview = SessionOverview {
        session: build_session_discovery(SystemTime::UNIX_EPOCH),
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    };

    let overview_json = serde_json::to_string(&overview).expect("serializes");
    let decoded_overview: SessionOverview =
        serde_json::from_str(&overview_json).expect("deserializes");

    assert_eq!(decoded_overview, overview);
}

#[test]
fn an_unknown_pane_state_name_is_rejected() {
    let lifecycle_parse_error =
        serde_json::from_value::<PaneLifecycle>(json!("sleeping")).expect_err("rejects");

    assert_eq!(
        lifecycle_parse_error.to_string(),
        "unknown variant `sleeping`, expected one of `spawning`, `running`, `exited`, `closing`"
    );
}

#[test]
fn an_exited_state_without_its_code_field_decodes_with_no_code() {
    let decoded_lifecycle: PaneLifecycle =
        serde_json::from_value(json!({"exited": {}})).expect("deserializes");

    assert_eq!(decoded_lifecycle, PaneLifecycle::Exited { exit_code: None });
}

#[test]
fn a_client_row_missing_its_id_is_rejected() {
    let mut client_json = serde_json::to_value(build_client_discovery(None)).expect("serialize");
    client_json
        .as_object_mut()
        .expect("a client row is a JSON object")
        .remove("id")
        .expect("the row carries an `id` field to remove");

    let client_parse_error =
        serde_json::from_value::<ClientDiscovery>(client_json).expect_err("rejects");

    assert_eq!(client_parse_error.to_string(), "missing field `id`");
}
