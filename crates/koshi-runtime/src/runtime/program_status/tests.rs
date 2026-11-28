//! Tests for [`apply_program_status_fact`]: how each OSC 7501 fact changes a
//! pane's program status records, and which report raises attention.

use std::time::Duration;

use koshi_core::program_status::BlockedKind;

use super::*;

/// `seconds` after the Unix epoch.
fn build_wall_time(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

/// A report for `record_id` in `program_status_state`, with no kind, no
/// progress, no title and no message, and the app name `cargo`.
fn build_report(record_id: &str, program_status_state: ProgramStatusState) -> ProgramStatusFact {
    ProgramStatusFact::Reported {
        record_id: record_id.to_string(),
        program_status_state,
        blocked_kind: None,
        progress_percent: None,
        app_name: Some("cargo".to_string()),
        title: None,
        message: None,
    }
}

/// The record `build_report(record_id, program_status_state)` writes at
/// `updated_at` with `is_seen`.
fn build_record(
    record_id: &str,
    program_status_state: ProgramStatusState,
    is_seen: bool,
    updated_at: SystemTime,
) -> ProgramStatusRecord {
    ProgramStatusRecord {
        record_id: record_id.to_string(),
        program_status_state,
        blocked_kind: None,
        progress_percent: None,
        app_name: Some("cargo".to_string()),
        title: None,
        message: None,
        is_seen,
        updated_at,
    }
}

/// Apply `program_status_fact` read at `build_wall_time(read_seconds)`, not
/// seen, with no outstanding reason.
fn apply_unseen_fact(
    program_status_records: &mut Vec<ProgramStatusRecord>,
    program_status_fact: ProgramStatusFact,
    read_seconds: u64,
) -> Option<AttentionReason> {
    apply_program_status_fact(
        program_status_records,
        program_status_fact,
        build_wall_time(read_seconds),
        false,
        None,
    )
}

/// The record ids of `program_status_records`, in order.
fn list_record_ids(program_status_records: &[ProgramStatusRecord]) -> Vec<&str> {
    program_status_records
        .iter()
        .map(|program_status_record| program_status_record.record_id.as_str())
        .collect()
}

#[test]
fn a_report_for_a_new_id_adds_its_record_at_the_end_with_its_read_time_and_seen_flag() {
    let mut program_status_records = vec![build_record(
        "deploy",
        ProgramStatusState::Working,
        false,
        build_wall_time(1),
    )];

    let raised_reason = apply_program_status_fact(
        &mut program_status_records,
        ProgramStatusFact::Reported {
            record_id: String::new(),
            program_status_state: ProgramStatusState::Working,
            blocked_kind: None,
            progress_percent: Some(40),
            app_name: Some("cargo".to_string()),
            title: Some("Build".to_string()),
            message: Some("compiling".to_string()),
        },
        build_wall_time(2),
        true,
        None,
    );

    assert_eq!(raised_reason, None);
    assert_eq!(
        program_status_records,
        [
            build_record(
                "deploy",
                ProgramStatusState::Working,
                false,
                build_wall_time(1)
            ),
            ProgramStatusRecord {
                record_id: String::new(),
                program_status_state: ProgramStatusState::Working,
                blocked_kind: None,
                progress_percent: Some(40),
                app_name: Some("cargo".to_string()),
                title: Some("Build".to_string()),
                message: Some("compiling".to_string()),
                is_seen: true,
                updated_at: build_wall_time(2),
            },
        ]
    );
}

#[test]
fn a_report_replaces_its_record_whole_and_moves_it_to_the_end() {
    let mut program_status_records = Vec::new();
    apply_unseen_fact(
        &mut program_status_records,
        ProgramStatusFact::Reported {
            record_id: "deploy".to_string(),
            program_status_state: ProgramStatusState::Blocked,
            blocked_kind: Some(BlockedKind::Auth),
            progress_percent: Some(10),
            app_name: Some("deploy".to_string()),
            title: Some("US East".to_string()),
            message: Some("Token?".to_string()),
        },
        1,
    );
    apply_unseen_fact(
        &mut program_status_records,
        build_report("build", ProgramStatusState::Working),
        2,
    );

    apply_unseen_fact(
        &mut program_status_records,
        ProgramStatusFact::Reported {
            record_id: "deploy".to_string(),
            program_status_state: ProgramStatusState::Working,
            blocked_kind: None,
            progress_percent: None,
            app_name: None,
            title: None,
            message: None,
        },
        3,
    );

    assert_eq!(
        program_status_records,
        [
            build_record(
                "build",
                ProgramStatusState::Working,
                false,
                build_wall_time(2)
            ),
            ProgramStatusRecord {
                record_id: "deploy".to_string(),
                program_status_state: ProgramStatusState::Working,
                blocked_kind: None,
                progress_percent: None,
                app_name: None,
                title: None,
                message: None,
                is_seen: false,
                updated_at: build_wall_time(3),
            },
        ]
    );
}

#[test]
fn a_new_id_beyond_the_limit_removes_the_record_written_longest_ago() {
    let mut program_status_records = Vec::new();
    for record_index in 0..PROGRAM_STATUS_RECORDS_MAX {
        apply_unseen_fact(
            &mut program_status_records,
            build_report(&format!("task-{record_index}"), ProgramStatusState::Working),
            0,
        );
    }
    apply_unseen_fact(
        &mut program_status_records,
        build_report("task-0", ProgramStatusState::Working),
        0,
    );

    apply_unseen_fact(
        &mut program_status_records,
        build_report("task-64", ProgramStatusState::Working),
        0,
    );

    let mut expected_record_ids: Vec<String> = (2..PROGRAM_STATUS_RECORDS_MAX)
        .map(|record_index| format!("task-{record_index}"))
        .collect();
    expected_record_ids.push("task-0".to_string());
    expected_record_ids.push("task-64".to_string());
    assert_eq!(program_status_records.len(), PROGRAM_STATUS_RECORDS_MAX);
    assert_eq!(
        list_record_ids(&program_status_records),
        expected_record_ids
    );
}

#[test]
fn a_clear_removes_its_record_and_every_record_beneath_it() {
    let mut program_status_records = Vec::new();
    for record_id in ["", "deploy", "deploy/us-east", "deploy-old", "build/deploy"] {
        apply_unseen_fact(
            &mut program_status_records,
            build_report(record_id, ProgramStatusState::Done),
            1,
        );
    }

    let raised_reason = apply_unseen_fact(
        &mut program_status_records,
        ProgramStatusFact::Cleared {
            record_id: "deploy".to_string(),
        },
        2,
    );

    assert_eq!(raised_reason, None);
    assert_eq!(
        list_record_ids(&program_status_records),
        ["", "deploy-old", "build/deploy"]
    );
}

#[test]
fn a_clear_with_no_id_removes_every_record_and_a_clear_of_an_unknown_id_removes_none() {
    let mut program_status_records = Vec::new();
    for record_id in ["", "deploy"] {
        apply_unseen_fact(
            &mut program_status_records,
            build_report(record_id, ProgramStatusState::Error),
            1,
        );
    }

    apply_unseen_fact(
        &mut program_status_records,
        ProgramStatusFact::Cleared {
            record_id: "dep".to_string(),
        },
        2,
    );
    assert_eq!(list_record_ids(&program_status_records), ["", "deploy"]);

    apply_unseen_fact(
        &mut program_status_records,
        ProgramStatusFact::Cleared {
            record_id: String::new(),
        },
        3,
    );
    assert_eq!(program_status_records, Vec::new());
}

#[test]
fn a_prompt_start_keeps_only_done_and_error_records() {
    let mut program_status_records = Vec::new();
    for (record_id, program_status_state) in [
        ("idle", ProgramStatusState::Idle),
        ("working", ProgramStatusState::Working),
        ("done", ProgramStatusState::Done),
        ("blocked", ProgramStatusState::Blocked),
        ("error", ProgramStatusState::Error),
    ] {
        apply_unseen_fact(
            &mut program_status_records,
            build_report(record_id, program_status_state),
            1,
        );
    }

    let raised_reason = apply_unseen_fact(
        &mut program_status_records,
        ProgramStatusFact::PromptStarted,
        2,
    );

    assert_eq!(raised_reason, None);
    assert_eq!(list_record_ids(&program_status_records), ["done", "error"]);
}

#[test]
fn a_terminal_reset_removes_every_record() {
    let mut program_status_records = Vec::new();
    for record_id in ["", "deploy"] {
        apply_unseen_fact(
            &mut program_status_records,
            build_report(record_id, ProgramStatusState::Done),
            1,
        );
    }

    let raised_reason = apply_unseen_fact(
        &mut program_status_records,
        ProgramStatusFact::TerminalReset,
        2,
    );

    assert_eq!(raised_reason, None);
    assert_eq!(program_status_records, Vec::new());
}

#[test]
fn a_move_into_blocked_done_or_error_raises_its_reason() {
    for (program_status_state, expected_reason) in [
        (
            ProgramStatusState::Blocked,
            AttentionReason::ProgramBlocked {
                record_id: "deploy".to_string(),
                blocked_kind: None,
                message: None,
            },
        ),
        (
            ProgramStatusState::Done,
            AttentionReason::ProgramDone {
                record_id: "deploy".to_string(),
                message: None,
            },
        ),
        (
            ProgramStatusState::Error,
            AttentionReason::ProgramFailed {
                record_id: "deploy".to_string(),
                message: None,
            },
        ),
    ] {
        let mut new_record_records = Vec::new();
        assert_eq!(
            apply_unseen_fact(
                &mut new_record_records,
                build_report("deploy", program_status_state),
                1
            ),
            Some(expected_reason.clone()),
            "{program_status_state:?} from no record"
        );

        let mut working_records = Vec::new();
        apply_unseen_fact(
            &mut working_records,
            build_report("deploy", ProgramStatusState::Working),
            1,
        );
        assert_eq!(
            apply_unseen_fact(
                &mut working_records,
                build_report("deploy", program_status_state),
                2
            ),
            Some(expected_reason),
            "{program_status_state:?} from working"
        );
    }
}

#[test]
fn a_report_into_idle_or_working_raises_nothing() {
    for program_status_state in [ProgramStatusState::Idle, ProgramStatusState::Working] {
        let mut program_status_records = Vec::new();
        apply_unseen_fact(
            &mut program_status_records,
            build_report("deploy", ProgramStatusState::Blocked),
            1,
        );

        assert_eq!(
            apply_unseen_fact(
                &mut program_status_records,
                build_report("deploy", program_status_state),
                2
            ),
            None,
            "{program_status_state:?}"
        );
    }
}

#[test]
fn a_rewrite_in_the_same_state_raises_only_for_the_record_of_the_outstanding_reason() {
    let mut program_status_records = Vec::new();
    for record_id in ["deploy", "build"] {
        apply_unseen_fact(
            &mut program_status_records,
            build_report(record_id, ProgramStatusState::Blocked),
            1,
        );
    }
    let outstanding_reason = AttentionReason::ProgramBlocked {
        record_id: "deploy".to_string(),
        blocked_kind: None,
        message: None,
    };
    let rewritten_blocked_report = ProgramStatusFact::Reported {
        record_id: "deploy".to_string(),
        program_status_state: ProgramStatusState::Blocked,
        blocked_kind: Some(BlockedKind::Permission),
        progress_percent: None,
        app_name: None,
        title: None,
        message: Some("Overwrite file?".to_string()),
    };

    assert_eq!(
        apply_program_status_fact(
            &mut program_status_records,
            rewritten_blocked_report.clone(),
            build_wall_time(2),
            false,
            Some(&outstanding_reason),
        ),
        Some(AttentionReason::ProgramBlocked {
            record_id: "deploy".to_string(),
            blocked_kind: Some(BlockedKind::Permission),
            message: Some("Overwrite file?".to_string()),
        })
    );
    assert_eq!(
        apply_program_status_fact(
            &mut program_status_records,
            build_report("build", ProgramStatusState::Blocked),
            build_wall_time(3),
            false,
            Some(&outstanding_reason),
        ),
        None
    );
    assert_eq!(
        apply_program_status_fact(
            &mut program_status_records,
            rewritten_blocked_report,
            build_wall_time(4),
            false,
            Some(&AttentionReason::Bell),
        ),
        None
    );
    assert_eq!(
        apply_program_status_fact(
            &mut program_status_records,
            build_report("deploy", ProgramStatusState::Blocked),
            build_wall_time(5),
            false,
            Some(&AttentionReason::ProgramDone {
                record_id: "deploy".to_string(),
                message: None,
            }),
        ),
        None
    );
}
