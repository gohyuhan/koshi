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
