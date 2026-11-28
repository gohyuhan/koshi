//! Tests for the pane attention types.

use super::*;

#[test]
fn each_reason_gives_its_kind() {
    assert_eq!(
        AttentionReason::Bell.get_reason_kind(),
        AttentionReasonKind::Bell
    );
    assert_eq!(
        AttentionReason::Notify {
            title: Some("Build".to_string()),
            body: "done".to_string(),
        }
        .get_reason_kind(),
        AttentionReasonKind::Notify
    );
    assert_eq!(
        AttentionReason::CommandFinished {
            exit_code: Some(101),
            elapsed_secs: Some(12),
        }
        .get_reason_kind(),
        AttentionReasonKind::CommandFinished
    );
    assert_eq!(
        AttentionReason::ProgramBlocked {
            record_id: String::new(),
            blocked_kind: Some(BlockedKind::Question),
            message: Some("Which branch?".to_string()),
        }
        .get_reason_kind(),
        AttentionReasonKind::ProgramBlocked
    );
    assert_eq!(
        AttentionReason::ProgramDone {
            record_id: "deploy".to_string(),
            message: None,
        }
        .get_reason_kind(),
        AttentionReasonKind::ProgramDone
    );
    assert_eq!(
        AttentionReason::ProgramFailed {
            record_id: "deploy/us-east".to_string(),
            message: Some("timeout".to_string()),
        }
        .get_reason_kind(),
        AttentionReasonKind::ProgramFailed
    );
}

#[test]
fn only_a_program_status_reason_names_its_record_and_state() {
    assert_eq!(
        AttentionReason::Bell.find_raising_program_status_record(),
        None
    );
    assert_eq!(
        AttentionReason::Notify {
            title: None,
            body: "done".to_string(),
        }
        .find_raising_program_status_record(),
        None
    );
    assert_eq!(
        AttentionReason::CommandFinished {
            exit_code: Some(0),
            elapsed_secs: Some(6),
        }
        .find_raising_program_status_record(),
        None
    );
    assert_eq!(
        AttentionReason::ProgramBlocked {
            record_id: String::new(),
            blocked_kind: None,
            message: None,
        }
        .find_raising_program_status_record(),
        Some(("", ProgramStatusState::Blocked))
    );
    assert_eq!(
        AttentionReason::ProgramDone {
            record_id: "deploy".to_string(),
            message: None,
        }
        .find_raising_program_status_record(),
        Some(("deploy", ProgramStatusState::Done))
    );
    assert_eq!(
        AttentionReason::ProgramFailed {
            record_id: "deploy/us-east".to_string(),
            message: None,
        }
        .find_raising_program_status_record(),
        Some(("deploy/us-east", ProgramStatusState::Error))
    );
}

#[test]
fn a_program_blocked_reason_missing_its_kind_is_refused() {
    let reason_json = r#"{"ProgramBlocked":{"record_id":"","message":null}}"#;
    let parse_error = serde_json::from_str::<AttentionReason>(reason_json)
        .expect_err("a reason without blocked_kind is refused");
    assert_eq!(
        parse_error.to_string(),
        "missing field `blocked_kind` at line 1 column 49"
    );
}

#[test]
fn attention_survives_a_json_round_trip() {
    let attention = Attention {
        raised_at: SystemTime::UNIX_EPOCH,
        reason: AttentionReason::Notify {
            title: None,
            body: "Waiting for input".to_string(),
        },
        withdrawable_notification_id: Some("build-1".to_string()),
    };
    let attention_json = serde_json::to_string(&attention).expect("attention serializes");
    assert_eq!(
        serde_json::from_str::<Attention>(&attention_json).expect("attention deserializes"),
        attention
    );
}

#[test]
fn attention_missing_its_withdrawable_id_is_refused() {
    let attention_json =
        r#"{"raised_at":{"secs_since_epoch":0,"nanos_since_epoch":0},"reason":"Bell"}"#;
    let parse_error = serde_json::from_str::<Attention>(attention_json)
        .expect_err("a record without withdrawable_notification_id is refused");
    assert_eq!(
        parse_error.to_string(),
        "missing field `withdrawable_notification_id` at line 1 column 74"
    );
}
