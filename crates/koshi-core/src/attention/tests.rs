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

/// A record `record_id` in `program_status_state` with `app_name` and
/// `title`, and no kind, progress or message.
fn build_program_status_record(
    record_id: &str,
    program_status_state: ProgramStatusState,
    app_name: Option<&str>,
    title: Option<&str>,
) -> ProgramStatusRecord {
    ProgramStatusRecord {
        record_id: record_id.to_string(),
        program_status_state,
        blocked_kind: None,
        progress_percent: None,
        app_name: app_name.map(str::to_string),
        title: title.map(str::to_string),
        message: None,
        is_seen: false,
        updated_at: SystemTime::UNIX_EPOCH,
    }
}

#[test]
fn whole_seconds_read_in_seconds_then_minutes_then_hours() {
    for (elapsed_secs, expected_text) in [
        (0, "0s"),
        (45, "45s"),
        (59, "59s"),
        (60, "1m 0s"),
        (134, "2m 14s"),
        (3599, "59m 59s"),
        (3600, "1h 0m"),
        (3780, "1h 3m"),
        (u32::MAX, "1193046h 28m"),
    ] {
        assert_eq!(
            format_elapsed_seconds(elapsed_secs),
            expected_text,
            "{elapsed_secs}"
        );
    }
}

#[test]
fn each_reason_gives_the_severity_and_text_of_its_entry() {
    let notify = |title: Option<&str>, body: &str| AttentionReason::Notify {
        title: title.map(str::to_string),
        body: body.to_string(),
    };
    let finished =
        |exit_code: Option<i32>, elapsed_secs: Option<u32>| AttentionReason::CommandFinished {
            exit_code,
            elapsed_secs,
        };
    let root_records = [build_program_status_record(
        "",
        ProgramStatusState::Blocked,
        Some("codex"),
        None,
    )];
    for (reason, expected_entry) in [
        (AttentionReason::Bell, None),
        (
            notify(Some("Claude Code"), "Waiting for input"),
            Some((NotificationSeverity::Info, "Claude Code: Waiting for input")),
        ),
        (
            notify(None, "Build done"),
            Some((NotificationSeverity::Info, "Build done")),
        ),
        (
            notify(Some("Build"), ""),
            Some((NotificationSeverity::Info, "Build")),
        ),
        (notify(None, ""), Some((NotificationSeverity::Info, ""))),
        (
            finished(Some(0), Some(45)),
            Some((NotificationSeverity::Info, "finished after 45s")),
        ),
        (
            finished(None, None),
            Some((NotificationSeverity::Info, "finished")),
        ),
        (
            finished(Some(101), Some(134)),
            Some((
                NotificationSeverity::Warning,
                "failed — exit 101 after 2m 14s",
            )),
        ),
        (
            finished(Some(-1), None),
            Some((NotificationSeverity::Warning, "failed — exit -1")),
        ),
        (
            AttentionReason::ProgramBlocked {
                record_id: String::new(),
                blocked_kind: Some(BlockedKind::Question),
                message: Some("Which branch?".to_string()),
            },
            Some((
                NotificationSeverity::Warning,
                "codex: blocked (question) — Which branch?",
            )),
        ),
        (
            AttentionReason::ProgramBlocked {
                record_id: String::new(),
                blocked_kind: None,
                message: None,
            },
            Some((NotificationSeverity::Warning, "codex: blocked")),
        ),
        (
            AttentionReason::ProgramDone {
                record_id: String::new(),
                message: Some("Merged".to_string()),
            },
            Some((NotificationSeverity::Info, "codex: done — Merged")),
        ),
        (
            AttentionReason::ProgramFailed {
                record_id: String::new(),
                message: Some("Tests failed".to_string()),
            },
            Some((NotificationSeverity::Error, "codex: failed — Tests failed")),
        ),
    ] {
        let description = format!("{reason:?}");
        assert_eq!(
            reason.format_notification_entry(&root_records),
            expected_entry.map(|(severity, text)| (severity, text.to_string())),
            "{description}"
        );
    }
}

#[test]
fn a_record_label_takes_its_own_app_name_else_the_nearest_ancestor_one_then_its_title() {
    let records = [
        build_program_status_record("", ProgramStatusState::Working, Some("agent"), None),
        build_program_status_record("deploy", ProgramStatusState::Working, Some("deploy"), None),
        build_program_status_record(
            "deploy/eu-west",
            ProgramStatusState::Blocked,
            None,
            Some("EU West"),
        ),
        build_program_status_record(
            "deploy/us-east",
            ProgramStatusState::Blocked,
            Some("terraform"),
            None,
        ),
        build_program_status_record("build/linux/arm", ProgramStatusState::Blocked, None, None),
    ];
    let records_without_root = &records[1..];
    for (program_status_records, record_id, expected_text) in [
        (&records[..], "deploy/eu-west", "deploy / EU West: blocked"),
        (&records[..], "deploy/us-east", "terraform: blocked"),
        (&records[..], "build/linux/arm", "agent: blocked"),
        (&records[..], "", "agent: blocked"),
        (records_without_root, "build/linux/arm", "blocked"),
        (&[][..], "gone", "blocked"),
    ] {
        let reason = AttentionReason::ProgramBlocked {
            record_id: record_id.to_string(),
            blocked_kind: None,
            message: None,
        };
        assert_eq!(
            reason.format_notification_entry(program_status_records),
            Some((NotificationSeverity::Warning, expected_text.to_string())),
            "{record_id}"
        );
    }

    let title_only_records = [build_program_status_record(
        "job",
        ProgramStatusState::Done,
        None,
        Some("Nightly"),
    )];
    let done_reason = AttentionReason::ProgramDone {
        record_id: "job".to_string(),
        message: None,
    };
    assert_eq!(
        done_reason.format_notification_entry(&title_only_records),
        Some((NotificationSeverity::Info, "Nightly: done".to_string()))
    );
}
