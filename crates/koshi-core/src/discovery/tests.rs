//! Tests for the discovery serde forms: the timestamp epoch pair and lossy
//! path serialization.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use super::*;

/// The fixed UUID every fake id uses.
fn fixed_uuid() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// A session created at `created_at`, with fixed everything else.
fn session_info(created_at: SystemTime) -> SessionInfo {
    SessionInfo {
        id: SessionId::from_uuid(fixed_uuid()),
        name: "quiet-lake".to_string(),
        created_at,
        attached_clients: Vec::new(),
        pane_count: 0,
    }
}

/// A pane whose working directory is `cwd`, with fixed everything else.
fn pane_info(cwd: Option<PathBuf>) -> PaneInfo {
    PaneInfo {
        id: PaneId::from_uuid(fixed_uuid()),
        tab_id: TabId::from_uuid(fixed_uuid()),
        session_id: SessionId::from_uuid(fixed_uuid()),
        title: None,
        cwd,
        command: None,
        state: PaneState::Running,
        focused_by_clients: Vec::new(),
    }
}

/// A path containing bytes that are not valid UTF-8; its lossy form is
/// `/tmp/f\u{FFFD}oo` on every platform.
fn non_utf8_path() -> PathBuf {
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
    let info = session_info(SystemTime::UNIX_EPOCH + Duration::new(1234, 500));

    let value = serde_json::to_value(&info).expect("serializes");

    assert_eq!(
        value["created_at"],
        json!({"secs_since_epoch": 1234, "nanos_since_epoch": 500})
    );
}

#[test]
fn times_round_trip_through_json() {
    let info = session_info(SystemTime::UNIX_EPOCH + Duration::new(1234, 500));

    let value = serde_json::to_value(&info).expect("serializes");
    let back: SessionInfo = serde_json::from_value(value).expect("deserializes");

    assert_eq!(back, info);
}

#[test]
fn non_utf8_cwd_serializes_as_its_lossy_string() {
    let info = pane_info(Some(non_utf8_path()));

    let value = serde_json::to_value(&info).expect("serializes");

    assert_eq!(value["cwd"], json!("/tmp/f\u{FFFD}oo"));
}

#[test]
fn absent_cwd_serializes_as_null() {
    let info = pane_info(None);

    let value = serde_json::to_value(&info).expect("serializes");

    assert_eq!(value["cwd"], serde_json::Value::Null);
}

#[test]
fn valid_utf8_cwd_serializes_as_its_plain_string() {
    let info = pane_info(Some(PathBuf::from("/home/user/project")));

    let value = serde_json::to_value(&info).expect("serializes");

    assert_eq!(value["cwd"], json!("/home/user/project"));
}

#[test]
fn pane_state_serializes_with_snake_case_names() {
    assert_eq!(
        serde_json::to_value(PaneState::Spawning).expect("serializes"),
        json!("spawning")
    );
    assert_eq!(
        serde_json::to_value(PaneState::Running).expect("serializes"),
        json!("running")
    );
    assert_eq!(
        serde_json::to_value(PaneState::Closing).expect("serializes"),
        json!("closing")
    );
    assert_eq!(
        serde_json::to_value(PaneState::Exited { code: Some(1) }).expect("serializes"),
        json!({"exited": {"code": 1}})
    );
    assert_eq!(
        serde_json::to_value(PaneState::Exited { code: None }).expect("serializes"),
        json!({"exited": {"code": null}})
    );
}

/// A client row whose origin is `origin`, with fixed everything else.
fn client_info(origin: Option<ClientOrigin>) -> ClientInfo {
    ClientInfo {
        id: ClientId::from_uuid(fixed_uuid()),
        session_id: SessionId::from_uuid(fixed_uuid()),
        attached_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        viewport_size: Size { cols: 80, rows: 24 },
        active_tab: TabId::from_uuid(fixed_uuid()),
        focused_pane: None,
        lock_state: LockMode::Normal,
        origin,
        pane_area: None,
    }
}

#[test]
fn client_info_json_without_pane_area_decodes_as_none() {
    let reported = ClientInfo {
        pane_area: Some(PaneArea::Reported(Size { cols: 80, rows: 22 })),
        ..client_info(None)
    };
    let mut wire = serde_json::to_value(&reported).expect("serialize");
    wire.as_object_mut()
        .expect("a client row is a JSON object")
        .remove("pane_area")
        .expect("the row carries a `pane_area` field to remove");

    let decoded: ClientInfo = serde_json::from_value(wire).expect("deserialize");

    assert_eq!(decoded, client_info(None));
}

#[test]
fn a_client_row_carrying_no_origin_field_reads_as_unanswered_never_as_local() {
    let mut wire = serde_json::to_value(client_info(Some(ClientOrigin::Local))).expect("serialize");
    wire.as_object_mut()
        .expect("a client row is a JSON object")
        .remove("origin")
        .expect("the row carries an `origin` field to remove");

    let decoded: ClientInfo = serde_json::from_value(wire).expect("deserialize");

    assert_eq!(decoded.origin, None);
    assert_eq!(decoded, client_info(None));
}

#[test]
fn a_client_row_stating_its_origin_keeps_that_answer() {
    for origin in [ClientOrigin::Local, ClientOrigin::Remote] {
        let wire = serde_json::to_value(client_info(Some(origin))).expect("serialize");
        let decoded: ClientInfo = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(decoded.origin, Some(origin), "{origin:?}");
    }
}

#[test]
fn pane_state_round_trips_through_json_for_every_variant() {
    for state in [
        PaneState::Spawning,
        PaneState::Running,
        PaneState::Closing,
        PaneState::Exited { code: Some(137) },
        PaneState::Exited { code: None },
    ] {
        let json = serde_json::to_string(&state).expect("serialize");
        let back: PaneState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, back, "{json}");
    }
}

#[test]
fn a_non_utf8_cwd_decodes_as_its_lossy_path() {
    let value = serde_json::to_value(pane_info(Some(non_utf8_path()))).expect("serializes");

    let decoded: PaneInfo = serde_json::from_value(value).expect("deserializes");

    assert_eq!(decoded.cwd, Some(PathBuf::from("/tmp/f\u{FFFD}oo")));
}

#[test]
fn pane_info_round_trips_with_every_optional_field_set() {
    let info = PaneInfo {
        title: Some("vim".to_string()),
        command: Some(vec!["htop".to_string(), "-d".to_string()]),
        state: PaneState::Exited { code: Some(0) },
        focused_by_clients: vec![ClientId::from_uuid(fixed_uuid())],
        ..pane_info(Some(PathBuf::from("/home/user/project")))
    };

    let json = serde_json::to_string(&info).expect("serializes");
    let back: PaneInfo = serde_json::from_str(&json).expect("deserializes");

    assert_eq!(back, info);
}

#[test]
fn tab_info_round_trips_through_json() {
    let info = TabInfo {
        id: TabId::from_uuid(fixed_uuid()),
        session_id: SessionId::from_uuid(fixed_uuid()),
        name: "amber-fox".to_string(),
        index: 2,
        active_pane: Some(PaneId::from_uuid(fixed_uuid())),
        pane_count: 3,
    };

    let json = serde_json::to_string(&info).expect("serializes");
    let back: TabInfo = serde_json::from_str(&json).expect("deserializes");

    assert_eq!(back, info);
}

#[test]
fn client_info_round_trips_with_every_optional_field_set() {
    let info = ClientInfo {
        focused_pane: Some(PaneId::from_uuid(fixed_uuid())),
        lock_state: LockMode::Locked,
        pane_area: Some(PaneArea::Reported(Size { cols: 80, rows: 22 })),
        ..client_info(Some(ClientOrigin::Remote))
    };

    let json = serde_json::to_string(&info).expect("serializes");
    let back: ClientInfo = serde_json::from_str(&json).expect("deserializes");

    assert_eq!(back, info);
}

#[test]
fn a_starving_pane_area_round_trips_through_json() {
    let info = ClientInfo {
        pane_area: Some(PaneArea::Starving),
        ..client_info(None)
    };

    let json = serde_json::to_string(&info).expect("serializes");
    let back: ClientInfo = serde_json::from_str(&json).expect("deserializes");

    assert_eq!(back.pane_area, Some(PaneArea::Starving));
    assert_eq!(back, info);
}

#[test]
fn session_overview_round_trips_through_json() {
    let overview = SessionOverview {
        session: SessionInfo {
            attached_clients: vec![ClientId::from_uuid(fixed_uuid())],
            pane_count: 1,
            ..session_info(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        },
        tabs: vec![TabInfo {
            id: TabId::from_uuid(fixed_uuid()),
            session_id: SessionId::from_uuid(fixed_uuid()),
            name: "amber-fox".to_string(),
            index: 0,
            active_pane: None,
            pane_count: 1,
        }],
        panes: vec![pane_info(None)],
        clients: vec![client_info(Some(ClientOrigin::Local))],
    };

    let json = serde_json::to_string(&overview).expect("serializes");
    let back: SessionOverview = serde_json::from_str(&json).expect("deserializes");

    assert_eq!(back, overview);
}

#[test]
fn an_empty_session_overview_round_trips_through_json() {
    let overview = SessionOverview {
        session: session_info(SystemTime::UNIX_EPOCH),
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    };

    let json = serde_json::to_string(&overview).expect("serializes");
    let back: SessionOverview = serde_json::from_str(&json).expect("deserializes");

    assert_eq!(back, overview);
}

#[test]
fn an_unknown_pane_state_name_is_rejected() {
    let err = serde_json::from_value::<PaneState>(json!("sleeping")).expect_err("rejects");

    assert_eq!(
        err.to_string(),
        "unknown variant `sleeping`, expected one of `spawning`, `running`, `exited`, `closing`"
    );
}

#[test]
fn an_exited_state_without_its_code_field_decodes_with_no_code() {
    let decoded: PaneState = serde_json::from_value(json!({"exited": {}})).expect("deserializes");

    assert_eq!(decoded, PaneState::Exited { code: None });
}

#[test]
fn a_client_row_missing_its_id_is_rejected() {
    let mut wire = serde_json::to_value(client_info(None)).expect("serialize");
    wire.as_object_mut()
        .expect("a client row is a JSON object")
        .remove("id")
        .expect("the row carries an `id` field to remove");

    let err = serde_json::from_value::<ClientInfo>(wire).expect_err("rejects");

    assert_eq!(err.to_string(), "missing field `id`");
}
