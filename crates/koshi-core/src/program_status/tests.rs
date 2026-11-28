//! Tests for the program status record type.

use super::*;

fn build_record(program_status_state: ProgramStatusState) -> ProgramStatusRecord {
    ProgramStatusRecord {
        record_id: "deploy".to_string(),
        program_status_state,
        blocked_kind: None,
        progress_percent: None,
        app_name: Some("cargo".to_string()),
        title: Some("US East".to_string()),
        message: Some("Apply?".to_string()),
        is_seen: false,
        updated_at: SystemTime::UNIX_EPOCH,
    }
}

#[test]
fn blocked_done_and_error_raise_their_reason_and_idle_and_working_raise_none() {
    let blocked_record = ProgramStatusRecord {
        blocked_kind: Some(BlockedKind::Permission),
        ..build_record(ProgramStatusState::Blocked)
    };
    assert_eq!(
        blocked_record.build_attention_reason(),
        Some(AttentionReason::ProgramBlocked {
            record_id: "deploy".to_string(),
            blocked_kind: Some(BlockedKind::Permission),
            message: Some("Apply?".to_string()),
        })
    );
    assert_eq!(
        build_record(ProgramStatusState::Done).build_attention_reason(),
        Some(AttentionReason::ProgramDone {
            record_id: "deploy".to_string(),
            message: Some("Apply?".to_string()),
        })
    );
    assert_eq!(
        build_record(ProgramStatusState::Error).build_attention_reason(),
        Some(AttentionReason::ProgramFailed {
            record_id: "deploy".to_string(),
            message: Some("Apply?".to_string()),
        })
    );
    assert_eq!(
        build_record(ProgramStatusState::Idle).build_attention_reason(),
        None
    );
    assert_eq!(
        build_record(ProgramStatusState::Working).build_attention_reason(),
        None
    );
}

#[test]
fn a_record_survives_a_json_round_trip() {
    let record = ProgramStatusRecord {
        blocked_kind: Some(BlockedKind::Auth),
        progress_percent: Some(40),
        is_seen: true,
        ..build_record(ProgramStatusState::Blocked)
    };
    let record_json = serde_json::to_string(&record).expect("the record serializes");
    assert_eq!(
        serde_json::from_str::<ProgramStatusRecord>(&record_json).expect("the record deserializes"),
        record
    );
}

#[test]
fn a_record_missing_an_optional_field_is_refused() {
    let record_json = serde_json::to_string(&build_record(ProgramStatusState::Done))
        .expect("the record serializes")
        .replacen(r#","app_name":"cargo""#, "", 1);
    let parse_error = serde_json::from_str::<ProgramStatusRecord>(&record_json)
        .expect_err("a record without app_name is refused");
    assert_eq!(
        parse_error.to_string(),
        format!(
            "missing field `app_name` at line 1 column {}",
            record_json.len()
        )
    );
}
