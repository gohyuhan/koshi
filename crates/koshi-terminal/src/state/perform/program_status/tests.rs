//! Tests for the OSC 7501 report parser: the grammar, each limit at its bound
//! and one byte over with both terminators, the ids, the base64 text rules,
//! and the keys that one state reads and another does not.

use koshi_core::program_status::{BlockedKind, ProgramStatusState};

use super::*;

/// `report_text` parsed as a report that ended with `ESC \`.
fn parse_string_terminated_report(report_text: &str) -> Option<ProgramStatusFact> {
    parse_program_status_report(report_text.as_bytes(), false)
}

/// A `Reported` fact for the root record with `state` and no other key.
fn build_root_report(program_status_state: ProgramStatusState) -> ProgramStatusFact {
    ProgramStatusFact::Reported {
        record_id: String::new(),
        program_status_state,
        blocked_kind: None,
        progress_percent: None,
        app_name: None,
        title: None,
        message: None,
    }
}

/// A `Reported` fact for the root record with `state` and `message`.
fn build_root_report_with_message(
    program_status_state: ProgramStatusState,
    message: &str,
) -> ProgramStatusFact {
    ProgramStatusFact::Reported {
        record_id: String::new(),
        program_status_state,
        blocked_kind: None,
        progress_percent: None,
        app_name: None,
        title: None,
        message: Some(message.to_string()),
    }
}

#[test]
fn the_protocol_example_gives_blocked_permission_terraform_apply() {
    assert_eq!(
        parse_string_terminated_report("state=blocked:kind=permission:app=terraform:msg=QXBwbHk/"),
        Some(ProgramStatusFact::Reported {
            record_id: String::new(),
            program_status_state: ProgramStatusState::Blocked,
            blocked_kind: Some(BlockedKind::Permission),
            progress_percent: None,
            app_name: Some("terraform".to_string()),
            title: None,
            message: Some("Apply?".to_string()),
        })
    );
}

#[test]
fn a_report_gives_every_key_it_names() {
    assert_eq!(
        parse_string_terminated_report(
            "state=blocked:kind=question:progress=42:id=tf/plan:app=terraform\
             :title=UGxhbg==:msg=QXBwbHk/"
        ),
        Some(ProgramStatusFact::Reported {
            record_id: "tf/plan".to_string(),
            program_status_state: ProgramStatusState::Blocked,
            blocked_kind: Some(BlockedKind::Question),
            progress_percent: Some(42),
            app_name: Some("terraform".to_string()),
            title: Some("Plan".to_string()),
            message: Some("Apply?".to_string()),
        })
    );
}

#[test]
fn each_state_name_gives_its_state() {
    for (program_status_state_text, expected_program_status_state) in [
        ("idle", ProgramStatusState::Idle),
        ("working", ProgramStatusState::Working),
        ("done", ProgramStatusState::Done),
        ("blocked", ProgramStatusState::Blocked),
        ("error", ProgramStatusState::Error),
    ] {
        assert_eq!(
            parse_string_terminated_report(&format!("state={program_status_state_text}")),
            Some(build_root_report(expected_program_status_state)),
            "{program_status_state_text}"
        );
    }
}

#[test]
fn a_bad_pair_is_skipped_and_the_rest_of_the_report_applies() {
    assert_eq!(
        parse_string_terminated_report("state=idle:mood=yes:oops"),
        Some(build_root_report(ProgramStatusState::Idle))
    );
    assert_eq!(
        parse_string_terminated_report(
            "garbage:=x:Bad=1:app=b@d:extra=yes:state=working:app=ok:::"
        ),
        Some(ProgramStatusFact::Reported {
            record_id: String::new(),
            program_status_state: ProgramStatusState::Working,
            blocked_kind: None,
            progress_percent: None,
            app_name: Some("ok".to_string()),
            title: None,
            message: None,
        })
    );
    assert_eq!(
        parse_string_terminated_report("state=idle:title=a@b:msg=a;b"),
        Some(build_root_report(ProgramStatusState::Idle))
    );
}

#[test]
fn spaces_and_tabs_around_keys_and_values_are_removed() {
    assert_eq!(
        parse_string_terminated_report(" state = working :\tapp=cargo\t: id = deploy "),
        Some(ProgramStatusFact::Reported {
            record_id: "deploy".to_string(),
            program_status_state: ProgramStatusState::Working,
            blocked_kind: None,
            progress_percent: None,
            app_name: Some("cargo".to_string()),
            title: None,
            message: None,
        })
    );
}

#[test]
fn the_last_value_of_a_repeated_key_wins() {
    assert_eq!(
        parse_string_terminated_report("state=idle:app=a:state=done:app=b"),
        Some(ProgramStatusFact::Reported {
            record_id: String::new(),
            program_status_state: ProgramStatusState::Done,
            blocked_kind: None,
            progress_percent: None,
            app_name: Some("b".to_string()),
            title: None,
            message: None,
        })
    );
}

#[test]
fn a_missing_or_unknown_state_discards_the_report() {
    for report_text in [
        "",
        "state",
        "app=cargo",
        "state=",
        "state=sleeping",
        "state=IDLE",
        "state=idle!",
    ] {
        assert_eq!(
            parse_string_terminated_report(report_text),
            None,
            "{report_text}"
        );
    }
}

#[test]
fn a_key_over_16_bytes_discards_the_report_whether_known_or_not() {
    assert_eq!(
        parse_string_terminated_report("state=idle:abcdefghijklmnop=1"),
        Some(build_root_report(ProgramStatusState::Idle))
    );
    assert_eq!(
        parse_string_terminated_report("state=idle:abcdefghijklmnopq=1"),
        None
    );
    assert_eq!(
        parse_string_terminated_report("state=idle:ABCDEFGHIJKLMNOPQ=1"),
        None
    );
    assert_eq!(
        parse_string_terminated_report("state=idle:abcdefghijklmnopq"),
        Some(build_root_report(ProgramStatusState::Idle))
    );
}

#[test]
fn the_sequence_limit_counts_from_the_introducer_through_the_terminator_used() {
    let build_report_bytes = |report_byte_count: usize| {
        let mut report_bytes = b"state=idle:".to_vec();
        report_bytes.resize(report_byte_count, b'x');
        report_bytes
    };
    let idle_report = Some(build_root_report(ProgramStatusState::Idle));

    assert_eq!(
        parse_program_status_report(&build_report_bytes(4087), false),
        idle_report
    );
    assert_eq!(
        parse_program_status_report(&build_report_bytes(4088), false),
        None
    );
    assert_eq!(
        parse_program_status_report(&build_report_bytes(4088), true),
        idle_report
    );
    assert_eq!(
        parse_program_status_report(&build_report_bytes(4089), true),
        None
    );
}

#[test]
fn an_app_over_32_bytes_discards_the_report_even_when_another_app_follows_it() {
    let app_name_at_limit = "a".repeat(32);
    let app_name_over_limit = "a".repeat(33);

    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:app={app_name_at_limit}")),
        Some(ProgramStatusFact::Reported {
            record_id: String::new(),
            program_status_state: ProgramStatusState::Idle,
            blocked_kind: None,
            progress_percent: None,
            app_name: Some(app_name_at_limit.clone()),
            title: None,
            message: None,
        })
    );
    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:app={app_name_over_limit}")),
        None
    );
    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:app={app_name_over_limit}:app=cargo")),
        None
    );
    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:app=@{app_name_at_limit}")),
        None
    );
}

#[test]
fn an_app_that_is_not_a_name_reads_as_none() {
    for report_text in [
        "state=idle:app=a/b",
        "state=idle:app=a,b",
        "state=idle:app=",
    ] {
        assert_eq!(
            parse_string_terminated_report(report_text),
            Some(build_root_report(ProgramStatusState::Idle)),
            "{report_text}"
        );
    }
}

#[test]
fn an_id_that_follows_the_id_rule_names_its_record() {
    let longest_record_id = format!(
        "{}/{}/{}/{}",
        "a".repeat(32),
        "b".repeat(32),
        "c".repeat(31),
        "d".repeat(30)
    );
    assert_eq!(longest_record_id.len(), 128);
    for record_id in [
        "a",
        "build/test",
        "A-Z_0.9+",
        &"a".repeat(32),
        "a/b/c/d/e/f/g/h",
        &longest_record_id,
    ] {
        for is_bell_terminated in [false, true] {
            assert_eq!(
                parse_program_status_report(
                    format!("state=clear:id={record_id}").as_bytes(),
                    is_bell_terminated
                ),
                Some(ProgramStatusFact::Cleared {
                    record_id: record_id.to_string()
                }),
                "{record_id}"
            );
        }
    }
}

#[test]
fn an_id_that_breaks_the_id_rule_discards_the_report() {
    let record_id_over_limit = format!(
        "{}/{}/{}/{}",
        "a".repeat(32),
        "b".repeat(32),
        "c".repeat(31),
        "d".repeat(31)
    );
    assert_eq!(record_id_over_limit.len(), 129);
    for record_id in [
        "",
        "/",
        "a/",
        "/a",
        "a//b",
        "a,b",
        "a=b",
        "a@b",
        "a b",
        "a%20b",
        &"a".repeat(33),
        "a/b/c/d/e/f/g/h/i",
        &record_id_over_limit,
    ] {
        for is_bell_terminated in [false, true] {
            for report_text in [
                format!("state=clear:id={record_id}"),
                format!("state=clear:id={record_id}:id=a"),
                format!("state=idle:id={record_id}"),
            ] {
                assert_eq!(
                    parse_program_status_report(report_text.as_bytes(), is_bell_terminated),
                    None,
                    "{report_text}"
                );
            }
        }
    }
}

#[test]
fn clear_names_its_record_and_reads_no_other_key() {
    assert_eq!(
        parse_string_terminated_report("state=clear:id=deploy"),
        Some(ProgramStatusFact::Cleared {
            record_id: "deploy".to_string()
        })
    );
    assert_eq!(
        parse_string_terminated_report("state=clear"),
        Some(ProgramStatusFact::Cleared {
            record_id: String::new()
        })
    );
    assert_eq!(
        parse_string_terminated_report("state=clear:id=deploy:app=cargo:kind=auth:msg=QQ"),
        Some(ProgramStatusFact::Cleared {
            record_id: "deploy".to_string()
        })
    );
    assert_eq!(
        parse_string_terminated_report("state=clear:id=deploy:msg=a"),
        None
    );
}

#[test]
fn a_title_at_its_encoded_limit_applies_and_one_group_over_discards() {
    let title_at_limit = "QUFB".repeat(64);
    assert_eq!(title_at_limit.len(), 256);

    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:title={title_at_limit}")),
        Some(ProgramStatusFact::Reported {
            record_id: String::new(),
            program_status_state: ProgramStatusState::Idle,
            blocked_kind: None,
            progress_percent: None,
            app_name: None,
            title: Some("A".repeat(192)),
            message: None,
        })
    );
    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:title={title_at_limit}QUFB")),
        None
    );
    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:title=@{title_at_limit}")),
        None
    );
}

#[test]
fn a_message_at_its_decoded_limit_keeps_its_first_512_bytes_and_one_byte_over_discards() {
    let message_at_limit = format!("{}QUE", "QUFB".repeat(682));
    assert_eq!(message_at_limit.len(), 2731);
    let message_over_limit = "QUFB".repeat(683);
    assert_eq!(message_over_limit.len(), 2732);

    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:msg={message_at_limit}")),
        Some(build_root_report_with_message(
            ProgramStatusState::Idle,
            &"A".repeat(512)
        ))
    );
    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:msg={message_over_limit}")),
        None
    );
    assert_eq!(
        parse_string_terminated_report(&format!("state=idle:msg={message_over_limit}QUFB")),
        None
    );
}

#[test]
fn base64_padding_is_optional_and_partial_padding_discards_the_report() {
    for (message_text, expected_message) in [
        ("QXBwbHk/", "Apply?"),
        ("QXBwbHk", "Apply"),
        ("QXBwbHk=", "Apply"),
        ("QQ", "A"),
        ("QQ==", "A"),
    ] {
        assert_eq!(
            parse_string_terminated_report(&format!("state=done:msg={message_text}")),
            Some(build_root_report_with_message(
                ProgramStatusState::Done,
                expected_message
            )),
            "{message_text}"
        );
    }
    assert_eq!(parse_string_terminated_report("state=done:msg=QQ="), None);
}

#[test]
fn text_that_is_not_base64_or_not_utf8_or_holds_a_control_discards_the_report() {
    for report_text in [
        "state=done:msg=a",
        "state=done:title=a",
        "state=done:msg=QR==",
        "state=done:msg=YQpi",
        "state=done:title=YQpi",
        "state=done:msg=wps=",
        "state=done:msg=fw==",
        "state=done:msg=/w==",
        "state=done:msg=a:msg=QQ",
    ] {
        assert_eq!(
            parse_string_terminated_report(report_text),
            None,
            "{report_text}"
        );
    }
}

#[test]
fn text_that_is_empty_after_the_filter_reads_as_none() {
    assert_eq!(
        parse_string_terminated_report("state=done:msg=:title="),
        Some(build_root_report(ProgramStatusState::Done))
    );
    assert_eq!(
        parse_string_terminated_report("state=done:msg=4oCu"),
        Some(build_root_report(ProgramStatusState::Done))
    );
    assert_eq!(
        parse_string_terminated_report("state=done:msg=4oCub2s="),
        Some(build_root_report_with_message(
            ProgramStatusState::Done,
            "ok"
        ))
    );
    assert_eq!(
        parse_string_terminated_report("state=done:msg=5a6J5YWo"),
        Some(build_root_report_with_message(
            ProgramStatusState::Done,
            "安全"
        ))
    );
}

#[test]
fn kind_reads_with_blocked_only_and_an_unknown_kind_reads_as_none() {
    for (kind_text, expected_blocked_kind) in [
        ("permission", Some(BlockedKind::Permission)),
        ("question", Some(BlockedKind::Question)),
        ("auth", Some(BlockedKind::Auth)),
        ("dance", None),
        ("AUTH", None),
    ] {
        assert_eq!(
            parse_string_terminated_report(&format!("state=blocked:kind={kind_text}")),
            Some(ProgramStatusFact::Reported {
                record_id: String::new(),
                program_status_state: ProgramStatusState::Blocked,
                blocked_kind: expected_blocked_kind,
                progress_percent: None,
                app_name: None,
                title: None,
                message: None,
            }),
            "{kind_text}"
        );
    }
    for program_status_state_text in ["idle", "working", "done", "error"] {
        let Some(ProgramStatusFact::Reported { blocked_kind, .. }) =
            parse_string_terminated_report(&format!("state={program_status_state_text}:kind=auth"))
        else {
            panic!("state={program_status_state_text}:kind=auth gives a report");
        };
        assert_eq!(blocked_kind, None, "{program_status_state_text}");
    }
}

#[test]
fn progress_reads_a_whole_number_from_0_to_100_with_working_and_blocked_only() {
    for (progress_text, expected_progress_percent) in [
        ("0", Some(0)),
        ("40", Some(40)),
        ("100", Some(100)),
        ("007", Some(7)),
        ("101", None),
        ("256", None),
        ("99999999999999999999", None),
        ("-1", None),
        ("+1", None),
        ("4.5", None),
        ("1_0", None),
        ("", None),
    ] {
        for program_status_state in [ProgramStatusState::Working, ProgramStatusState::Blocked] {
            let program_status_state_text = match program_status_state {
                ProgramStatusState::Working => "working",
                _ => "blocked",
            };
            assert_eq!(
                parse_string_terminated_report(&format!(
                    "state={program_status_state_text}:progress={progress_text}"
                )),
                Some(ProgramStatusFact::Reported {
                    record_id: String::new(),
                    program_status_state,
                    blocked_kind: None,
                    progress_percent: expected_progress_percent,
                    app_name: None,
                    title: None,
                    message: None,
                }),
                "{program_status_state_text} {progress_text}"
            );
        }
    }
    for program_status_state_text in ["idle", "done", "error"] {
        let Some(ProgramStatusFact::Reported {
            progress_percent, ..
        }) = parse_string_terminated_report(&format!(
            "state={program_status_state_text}:progress=40"
        ))
        else {
            panic!("state={program_status_state_text}:progress=40 gives a report");
        };
        assert_eq!(progress_percent, None, "{program_status_state_text}");
    }
}

#[test]
fn a_value_byte_outside_the_value_bytes_skips_its_pair_and_discards_an_id() {
    assert_eq!(
        parse_string_terminated_report("state=idle:app=cårgo"),
        Some(build_root_report(ProgramStatusState::Idle))
    );
    assert_eq!(
        parse_program_status_report(b"state=idle:app=\xff", false),
        Some(build_root_report(ProgramStatusState::Idle))
    );
    assert_eq!(parse_string_terminated_report("state=idle:id=dé"), None);
    assert_eq!(
        parse_program_status_report(b"state=idle:id=\xff", false),
        None
    );
}

#[test]
fn text_that_decodes_to_an_escape_sequence_discards_the_report() {
    for report_text in [
        "state=blocked:msg=G10wO2V2aWwH",
        "state=blocked:title=G1sySg==",
    ] {
        assert_eq!(
            parse_string_terminated_report(report_text),
            None,
            "{report_text}"
        );
    }
}

#[test]
fn text_keeps_right_to_left_letters_and_emoji_joiners() {
    assert_eq!(
        parse_string_terminated_report("state=done:msg=16nXnNeV150g8J+RqeKAjfCfkrs="),
        Some(build_root_report_with_message(
            ProgramStatusState::Done,
            "\u{5e9}\u{5dc}\u{5d5}\u{5dd}\u{20}\u{1f469}\u{200d}\u{1f4bb}"
        ))
    );
}
