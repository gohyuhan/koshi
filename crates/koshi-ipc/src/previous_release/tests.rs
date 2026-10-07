//! The bytes this build sends a session of koshi 0.2.0 to 0.4.0, and the
//! answers such a session sends back, pinned word for word.

use super::*;

/// `answer_json` parsed as one answer of koshi 0.2.0 to 0.4.0.
fn parse_previous_release_answer(answer_json: &str) -> PreviousReleaseAnswer {
    serde_json::from_str(answer_json).expect("the answer parses")
}

#[test]
fn the_hello_travels_in_the_envelope_of_koshi_0_4_0_with_versions_2_to_3() {
    let hello_request = PreviousReleaseRequest {
        request_id: 1,
        request_kind: PreviousReleaseRequestKind::build_hello_request(
            ConnectionToken::from_secret("k7QxSecret"),
        ),
    };

    assert_eq!(
        serde_json::to_string(&hello_request).expect("the request serializes"),
        r#"{"request_id":1,"kind":{"Hello":{"min_protocol_version":2,"max_protocol_version":3,"token":"k7QxSecret"}}}"#
    );
}

#[test]
fn the_restart_travels_as_its_bare_name() {
    let restart_request = PreviousReleaseRequest {
        request_id: 2,
        request_kind: PreviousReleaseRequestKind::Restart,
    };

    assert_eq!(
        serde_json::to_string(&restart_request).expect("the request serializes"),
        r#"{"request_id":2,"kind":"Restart"}"#
    );
}

#[test]
fn the_hello_answer_of_koshi_0_4_0_reads_with_its_build() {
    assert_eq!(
        parse_previous_release_answer(
            r#"{"request_id":1,"result":{"Hello":{"protocol_version":3,"version":"0.4.0"}}}"#
        ),
        PreviousReleaseAnswer {
            answer_result: PreviousReleaseResult::Hello {
                build_version: "0.4.0".to_string(),
            },
        }
    );
}

#[test]
fn the_hello_answer_of_koshi_0_2_0_reads_with_an_empty_build() {
    assert_eq!(
        parse_previous_release_answer(
            r#"{"request_id":1,"result":{"Hello":{"protocol_version":2}}}"#
        ),
        PreviousReleaseAnswer {
            answer_result: PreviousReleaseResult::Hello {
                build_version: String::new(),
            },
        }
    );
}

#[test]
fn the_restart_answer_reads_as_restarting() {
    assert_eq!(
        parse_previous_release_answer(r#"{"request_id":2,"result":"Restarting"}"#),
        PreviousReleaseAnswer {
            answer_result: PreviousReleaseResult::Restarting,
        }
    );
}

#[test]
fn a_refusal_of_a_request_that_did_not_parse_reads_with_no_request_id() {
    assert_eq!(
        parse_previous_release_answer(
            r#"{"request_id":null,"result":{"Error":{"code":"malformed_request","message":"unknown field `request_kind`"}}}"#
        ),
        PreviousReleaseAnswer {
            answer_result: PreviousReleaseResult::Error(PreviousReleaseErrorPayload {
                code: PreviousReleaseErrorCode::MalformedRequest,
                message: "unknown field `request_kind`".to_string(),
            }),
        }
    );
}

#[test]
fn every_refusal_code_of_koshi_0_2_0_to_0_4_0_reads_as_its_variant() {
    let refusal_codes = [
        ("bad_token", PreviousReleaseErrorCode::BadToken),
        (
            "unsupported_version",
            PreviousReleaseErrorCode::UnsupportedVersion,
        ),
        (
            "unsupported_kind",
            PreviousReleaseErrorCode::UnsupportedKind,
        ),
        (
            "malformed_request",
            PreviousReleaseErrorCode::MalformedRequest,
        ),
        ("not_found", PreviousReleaseErrorCode::NotFound),
        ("hello_required", PreviousReleaseErrorCode::HelloRequired),
        ("other_users_off", PreviousReleaseErrorCode::OtherUsersOff),
        ("unknown", PreviousReleaseErrorCode::Unknown),
    ];

    for (wire_code, expected_error_code) in refusal_codes {
        let refusal_answer = parse_previous_release_answer(&format!(
            r#"{{"request_id":2,"result":{{"Error":{{"code":"{wire_code}","message":"refused"}}}}}}"#
        ));

        assert_eq!(
            refusal_answer,
            PreviousReleaseAnswer {
                answer_result: PreviousReleaseResult::Error(PreviousReleaseErrorPayload {
                    code: expected_error_code,
                    message: "refused".to_string(),
                }),
            },
            "{wire_code}"
        );
    }
}
