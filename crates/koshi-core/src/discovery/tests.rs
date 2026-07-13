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
        layout_rect: None,
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
    let info = session_info(SystemTime::UNIX_EPOCH + Duration::new(1234, 56));

    let value = serde_json::to_value(&info).expect("serializes");

    assert_eq!(
        value["created_at"],
        json!({"secs_since_epoch": 1234, "nanos_since_epoch": 56})
    );
}

#[test]
fn times_round_trip_through_json() {
    let info = session_info(SystemTime::UNIX_EPOCH + Duration::new(1234, 56));

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
