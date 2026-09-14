//! Tests for the pane-supervisor protocol: every request, answer and event
//! keeps the exact bytes this version pins, the gate opens for a Hello whose
//! version range overlaps the supervisor's and whose token matches, a kind the
//! supervisor does not have is refused by name, and the link address sits
//! beside the session's own control socket.

use std::collections::BTreeMap;

use koshi_core::process::ShellKind;

use super::*;
use crate::protocol::IpcErrorCode;

#[cfg(unix)]
#[test]
fn a_working_directory_that_is_not_valid_utf8_has_no_encoding_on_this_wire() {
    // Encoding such a path fails, and a failed encode breaks the link rather
    // than one answer, so the supervisor answers `Cwd(None)` for it.
    use std::os::unix::ffi::OsStrExt;

    let invalid_working_directory_path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff"));
    let refused =
        serde_json::to_string(&SupervisorResult::Cwd(Some(invalid_working_directory_path)))
            .expect_err("a path that is not valid UTF-8 does not encode");

    assert_eq!(
        refused.to_string(),
        "path contains invalid UTF-8 characters"
    );
    assert_eq!(
        serde_json::to_string(&SupervisorResult::Cwd(None)).expect("no directory encodes"),
        r#"{"Cwd":null}"#
    );
}

/// The one UUID every fixed id below uses.
fn build_fixed_test_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// The pane every fixed request below names.
fn build_test_pane_id() -> PaneId {
    PaneId::from_uuid(build_fixed_test_uuid())
}

/// A token holding a fixed secret.
fn build_test_connection_token() -> ConnectionToken {
    ConnectionToken::from_secret("k7QxSecret")
}

/// The size every fixed request below names.
fn build_test_pty_size() -> PtySize {
    PtySize {
        column_count: 80,
        row_count: 24,
    }
}

/// A spawn spec at fixed values, so its encoding is byte-stable.
fn build_test_spawn_spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        arguments: vec!["-c".to_string(), "echo hi".to_string()],
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Other("sh".to_string()),
    }
}

/// Encode `message` as the exact bytes that go on the wire.
fn serialize_test_wire_message<T: Serialize>(message: &T) -> String {
    serde_json::to_string(message).expect("message encodes")
}

#[test]
fn the_supervisor_link_wire_shape_belongs_to_this_protocol_version() {
    // Every request kind, every answer and every event, pinned byte for byte.
    //
    // A session server and the supervisor it reconnects to can be different
    // builds. The version in the Hello is the only thing that catches a pair
    // that does not agree on this shape. Round-trip tests cannot catch it: one
    // build encoding and decoding its own structs always agrees with itself.
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 1,
            request_kind: SupervisorRequestKind::Hello {
                min_protocol_version: 1,
                max_protocol_version: 1,
                connection_token: build_test_connection_token(),
            },
        }),
        r#"{"request_id":1,"kind":{"Hello":{"min_protocol_version":1,"max_protocol_version":1,"token":"k7QxSecret"}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 2,
            request_kind: SupervisorRequestKind::Spawn {
                pane_id: build_test_pane_id(),
                spawn_spec: build_test_spawn_spec(),
                pty_size: build_test_pty_size(),
            },
        }),
        r#"{"request_id":2,"kind":{"Spawn":{"pane_id":"00000000-0000-0000-0000-000000000001","spec":{"program":"/bin/sh","args":["-c","echo hi"],"cwd":null,"env":{},"shell_kind":{"Other":"sh"}},"size":{"cols":80,"rows":24}}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 3,
            request_kind: SupervisorRequestKind::Resize {
                pane_id: build_test_pane_id(),
                pty_size: build_test_pty_size(),
            },
        }),
        r#"{"request_id":3,"kind":{"Resize":{"pane_id":"00000000-0000-0000-0000-000000000001","size":{"cols":80,"rows":24}}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 4,
            request_kind: SupervisorRequestKind::Write {
                pane_id: build_test_pane_id(),
                input_bytes: vec![104, 105],
            },
        }),
        r#"{"request_id":4,"kind":{"Write":{"pane_id":"00000000-0000-0000-0000-000000000001","bytes":"aGk="}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 5,
            request_kind: SupervisorRequestKind::Kill {
                pane_id: build_test_pane_id(),
                kill_policy: KillPolicy::Tree,
            },
        }),
        r#"{"request_id":5,"kind":{"Kill":{"pane_id":"00000000-0000-0000-0000-000000000001","kill_policy":"Tree"}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 6,
            request_kind: SupervisorRequestKind::LiveCwd {
                pane_id: build_test_pane_id()
            },
        }),
        r#"{"request_id":6,"kind":{"LiveCwd":{"pane_id":"00000000-0000-0000-0000-000000000001"}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 7,
            request_kind: SupervisorRequestKind::ListPanes,
        }),
        r#"{"request_id":7,"kind":"ListPanes"}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 8,
            request_kind: SupervisorRequestKind::Shutdown,
        }),
        r#"{"request_id":8,"kind":"Shutdown"}"#
    );

    assert_eq!(
        serialize_test_wire_message(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(1),
                answer_result: SupervisorResult::Hello {
                    protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                },
            }
        )),
        r#"{"Response":{"request_id":1,"result":{"Hello":{"protocol_version":1}}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(2),
                answer_result: SupervisorResult::Spawned { process_id: 4242 },
            }
        )),
        r#"{"Response":{"request_id":2,"result":{"Spawned":{"pid":4242}}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(7),
                answer_result: SupervisorResult::Panes(vec![SupervisorPane {
                    pane_id: build_test_pane_id(),
                    process_id: 4242,
                    pty_size: build_test_pty_size(),
                }]),
            }
        )),
        r#"{"Response":{"request_id":7,"result":{"Panes":[{"pane_id":"00000000-0000-0000-0000-000000000001","pid":4242,"size":{"cols":80,"rows":24}}]}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(6),
                answer_result: SupervisorResult::Cwd(Some(PathBuf::from("/home/dev/api"))),
            }
        )),
        r#"{"Response":{"request_id":6,"result":{"Cwd":"/home/dev/api"}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(3),
                answer_result: SupervisorResult::Done,
            }
        )),
        r#"{"Response":{"request_id":3,"result":"Done"}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: None,
                answer_result: SupervisorResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::MalformedRequest,
                    message: "the request could not be read".to_string(),
                }),
            }
        )),
        r#"{"Response":{"request_id":null,"result":{"Error":{"code":"malformed_request","message":"the request could not be read"}}}}"#
    );

    assert_eq!(
        serialize_test_wire_message(&SupervisorMessage::<SupervisorResult, _>::Event(
            SupervisorEvent::Output {
                pane_id: build_test_pane_id(),
                output_bytes: vec![104, 105],
            }
        )),
        r#"{"Event":{"Output":{"pane_id":"00000000-0000-0000-0000-000000000001","bytes":"aGk="}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorMessage::<SupervisorResult, _>::Event(
            SupervisorEvent::Exited {
                pane_id: build_test_pane_id(),
                exit_status: ExitStatus::ExitCode(0),
            }
        )),
        r#"{"Event":{"Exited":{"pane_id":"00000000-0000-0000-0000-000000000001","status":{"ExitCode":0}}}}"#
    );
}

#[test]
fn the_output_hold_requests_travel_as_bare_names() {
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 9,
            request_kind: SupervisorRequestKind::PauseOutput,
        }),
        r#"{"request_id":9,"kind":"PauseOutput"}"#
    );
    assert_eq!(
        serialize_test_wire_message(&SupervisorRequest {
            request_id: 10,
            request_kind: SupervisorRequestKind::ResumeOutput,
        }),
        r#"{"request_id":10,"kind":"ResumeOutput"}"#
    );
}

#[test]
fn a_cwd_the_operating_system_cannot_answer_travels_as_null() {
    assert_eq!(
        serialize_test_wire_message(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(6),
                answer_result: SupervisorResult::Cwd(None),
            }
        )),
        r#"{"Response":{"request_id":6,"result":{"Cwd":null}}}"#
    );
}

#[test]
fn an_empty_write_travels_as_an_empty_string_and_reads_back_empty() {
    let request = SupervisorRequest {
        request_id: 4,
        request_kind: SupervisorRequestKind::Write {
            pane_id: build_test_pane_id(),
            input_bytes: Vec::new(),
        },
    };
    let serialized_request_json = serialize_test_wire_message(&request);

    assert_eq!(
        serialized_request_json,
        r#"{"request_id":4,"kind":{"Write":{"pane_id":"00000000-0000-0000-0000-000000000001","bytes":""}}}"#
    );
    let decoded: SupervisorRequest =
        serde_json::from_str(&serialized_request_json).expect("an empty write reads back");
    assert_eq!(decoded, request);
}

#[test]
fn answers_and_events_read_back_from_their_wire_text() {
    let hello: IncomingSupervisorMessage = serde_json::from_str(
        r#"{"Response":{"request_id":1,"result":{"Hello":{"protocol_version":1}}}}"#,
    )
    .expect("a Hello answer is this version's shape");
    let incoming_message: IncomingSupervisorMessage = serde_json::from_str(
        r#"{"Event":{"Output":{"pane_id":"00000000-0000-0000-0000-000000000001","bytes":"aGk="}}}"#,
    )
    .expect("an Output event is this version's shape");
    let exited: IncomingSupervisorMessage = serde_json::from_str(
        r#"{"Event":{"Exited":{"pane_id":"00000000-0000-0000-0000-000000000001","status":{"Signaled":9}}}}"#,
    )
    .expect("an Exited event is this version's shape");

    assert_eq!(
        hello,
        SupervisorMessage::Response(SupervisorResponse {
            request_id: Some(1),
            answer_result: MaybeKnown::Known(SupervisorResult::Hello {
                protocol_version: 1
            }),
        })
    );
    assert_eq!(
        incoming_message,
        SupervisorMessage::Event(MaybeKnown::Known(SupervisorEvent::Output {
            pane_id: build_test_pane_id(),
            output_bytes: vec![104, 105],
        }))
    );
    assert_eq!(
        exited,
        SupervisorMessage::Event(MaybeKnown::Known(SupervisorEvent::Exited {
            pane_id: build_test_pane_id(),
            exit_status: ExitStatus::Signaled(9),
        }))
    );
}

#[test]
fn a_hello_built_here_names_this_builds_range() {
    assert_eq!(
        SupervisorRequestKind::build_hello_request(build_test_connection_token()),
        SupervisorRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            connection_token: build_test_connection_token(),
        }
    );
}

#[test]
fn this_build_speaks_supervisor_link_version_one_only() {
    assert_eq!(SUPERVISOR_PROTOCOL_VERSION, 1);
    assert_eq!(MIN_SUPERVISOR_PROTOCOL_VERSION, 1);
}

#[test]
fn every_request_kind_names_itself_without_its_payload() {
    assert_eq!(
        SupervisorRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            connection_token: build_test_connection_token(),
        }
        .get_request_kind_name(),
        "Hello"
    );
    assert_eq!(
        SupervisorRequestKind::Spawn {
            pane_id: build_test_pane_id(),
            spawn_spec: build_test_spawn_spec(),
            pty_size: build_test_pty_size(),
        }
        .get_request_kind_name(),
        "Spawn"
    );
    assert_eq!(
        SupervisorRequestKind::Resize {
            pane_id: build_test_pane_id(),
            pty_size: build_test_pty_size(),
        }
        .get_request_kind_name(),
        "Resize"
    );
    assert_eq!(
        SupervisorRequestKind::Write {
            pane_id: build_test_pane_id(),
            input_bytes: vec![104],
        }
        .get_request_kind_name(),
        "Write"
    );
    assert_eq!(
        SupervisorRequestKind::Kill {
            pane_id: build_test_pane_id(),
            kill_policy: KillPolicy::Tree,
        }
        .get_request_kind_name(),
        "Kill"
    );
    assert_eq!(
        SupervisorRequestKind::LiveCwd {
            pane_id: build_test_pane_id()
        }
        .get_request_kind_name(),
        "LiveCwd"
    );
    assert_eq!(
        SupervisorRequestKind::ListPanes.get_request_kind_name(),
        "ListPanes"
    );
    assert_eq!(
        SupervisorRequestKind::PauseOutput.get_request_kind_name(),
        "PauseOutput"
    );
    assert_eq!(
        SupervisorRequestKind::ResumeOutput.get_request_kind_name(),
        "ResumeOutput"
    );
    assert_eq!(
        SupervisorRequestKind::Shutdown.get_request_kind_name(),
        "Shutdown"
    );
}

#[test]
fn every_event_names_itself_without_its_payload() {
    assert_eq!(
        SupervisorEvent::Output {
            pane_id: build_test_pane_id(),
            output_bytes: vec![104],
        }
        .get_event_name(),
        "Output"
    );
    assert_eq!(
        SupervisorEvent::Exited {
            pane_id: build_test_pane_id(),
            exit_status: ExitStatus::Signaled(9),
        }
        .get_event_name(),
        "Exited"
    );
}

#[test]
fn every_request_kind_and_answer_is_listed_as_a_name_this_build_has() {
    assert_eq!(
        SupervisorRequestKind::VARIANTS,
        [
            "Hello",
            "Spawn",
            "Resize",
            "Write",
            "Kill",
            "LiveCwd",
            "ListPanes",
            "PauseOutput",
            "ResumeOutput",
            "Shutdown",
        ]
    );
    assert_eq!(
        SupervisorResult::VARIANTS,
        ["Hello", "Spawned", "Panes", "Cwd", "Done", "Error"]
    );
    assert_eq!(SupervisorEvent::VARIANTS, ["Output", "Exited"]);
}

#[test]
fn every_answer_names_itself_and_the_wire_list_holds_each_name_in_order() {
    let results = [
        (
            SupervisorResult::Hello {
                protocol_version: 1,
            },
            "Hello",
        ),
        (SupervisorResult::Spawned { process_id: 4242 }, "Spawned"),
        (SupervisorResult::Panes(Vec::new()), "Panes"),
        (SupervisorResult::Cwd(None), "Cwd"),
        (SupervisorResult::Done, "Done"),
        (
            SupervisorResult::Error(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the request could not be read".to_string(),
            }),
            "Error",
        ),
    ];

    for (supervisor_result, expected_wire_name) in &results {
        assert_eq!(supervisor_result.wire_name(), *expected_wire_name);
    }
    let supervisor_result_names: Vec<&str> = results
        .iter()
        .map(|(supervisor_result, _)| supervisor_result.wire_name())
        .collect();
    assert_eq!(supervisor_result_names, SupervisorResult::VARIANTS);
}

#[test]
fn every_request_kind_and_event_travels_under_the_name_it_reports() {
    let kinds = [
        SupervisorRequestKind::build_hello_request(build_test_connection_token()),
        SupervisorRequestKind::Spawn {
            pane_id: build_test_pane_id(),
            spawn_spec: build_test_spawn_spec(),
            pty_size: build_test_pty_size(),
        },
        SupervisorRequestKind::Resize {
            pane_id: build_test_pane_id(),
            pty_size: build_test_pty_size(),
        },
        SupervisorRequestKind::Write {
            pane_id: build_test_pane_id(),
            input_bytes: vec![104],
        },
        SupervisorRequestKind::Kill {
            pane_id: build_test_pane_id(),
            kill_policy: KillPolicy::Force,
        },
        SupervisorRequestKind::LiveCwd {
            pane_id: build_test_pane_id(),
        },
        SupervisorRequestKind::ListPanes,
        SupervisorRequestKind::PauseOutput,
        SupervisorRequestKind::ResumeOutput,
        SupervisorRequestKind::Shutdown,
    ];
    let events = [
        SupervisorEvent::Output {
            pane_id: build_test_pane_id(),
            output_bytes: vec![104],
        },
        SupervisorEvent::Exited {
            pane_id: build_test_pane_id(),
            exit_status: ExitStatus::ExitCode(0),
        },
    ];

    for request_kind in &kinds {
        assert_eq!(
            request_kind.wire_name(),
            request_kind.get_request_kind_name()
        );
    }
    let request_kind_names: Vec<&str> =
        kinds.iter().map(SupervisorRequestKind::wire_name).collect();
    assert_eq!(request_kind_names, SupervisorRequestKind::VARIANTS);

    for event in &events {
        assert_eq!(event.wire_name(), event.get_event_name());
    }
    let event_names: Vec<&str> = events.iter().map(SupervisorEvent::wire_name).collect();
    assert_eq!(event_names, SupervisorEvent::VARIANTS);
}

#[test]
fn a_kind_this_build_does_not_have_reads_as_its_name_alone() {
    let request: IncomingSupervisorRequest =
        serde_json::from_str(r#"{"request_id":9,"kind":{"Rehome":{"pane_id":1}}}"#)
            .expect("a kind this build lacks still reads");

    assert_eq!(
        request,
        SupervisorRequest {
            request_id: 9,
            request_kind: MaybeKnown::Unknown {
                variant_name: "Rehome".to_string(),
            },
        }
    );
}

#[test]
fn an_answer_this_build_does_not_have_reads_as_its_name_alone() {
    let message: IncomingSupervisorMessage =
        serde_json::from_str(r#"{"Response":{"request_id":9,"result":{"Rehomed":{"pid":7}}}}"#)
            .expect("an answer this build lacks still reads");

    assert_eq!(
        message,
        SupervisorMessage::Response(SupervisorResponse {
            request_id: Some(9),
            answer_result: MaybeKnown::Unknown {
                variant_name: "Rehomed".to_string(),
            },
        })
    );
}

#[test]
fn an_event_this_build_does_not_have_reads_as_its_name_alone() {
    let message: IncomingSupervisorMessage =
        serde_json::from_str(r#"{"Event":{"Stalled":{"pane_id":1}}}"#)
            .expect("an event this build lacks still reads");

    assert_eq!(
        message,
        SupervisorMessage::Event(MaybeKnown::Unknown {
            variant_name: "Stalled".to_string(),
        })
    );
}

#[test]
fn a_request_carrying_an_unknown_field_is_refused() {
    let decoded: Result<SupervisorRequest, _> =
        serde_json::from_str(r#"{"request_id":7,"kind":"ListPanes","junk":5}"#);

    assert_eq!(
        decoded
            .expect_err("an unknown field is not this version's shape")
            .to_string(),
        "unknown field `junk`, expected `request_id` or `kind` at line 1 column 41"
    );
}

#[test]
fn a_request_missing_its_id_is_refused() {
    let decoded: Result<SupervisorRequest, _> = serde_json::from_str(r#"{"kind":"ListPanes"}"#);

    assert_eq!(
        decoded
            .expect_err("a request without an id is not this version's shape")
            .to_string(),
        "missing field `request_id` at line 1 column 20"
    );
}

#[test]
fn an_unknown_kind_sent_as_a_bare_name_reads_as_its_name_alone() {
    let request: IncomingSupervisorRequest =
        serde_json::from_str(r#"{"request_id":9,"kind":"Rehome"}"#)
            .expect("a kind this build lacks still reads");

    assert_eq!(
        request,
        SupervisorRequest {
            request_id: 9,
            request_kind: MaybeKnown::Unknown {
                variant_name: "Rehome".to_string(),
            },
        }
    );
}

#[test]
fn a_hello_sent_as_a_bare_name_keeps_the_decoding_error() {
    let decoded: Result<IncomingSupervisorRequest, _> =
        serde_json::from_str(r#"{"request_id":1,"kind":"Hello"}"#);

    assert_eq!(
        decoded
            .expect_err("a Hello with no payload is not this version's shape")
            .to_string(),
        "invalid type: unit variant, expected struct variant at line 1 column 31"
    );
}

#[test]
fn a_write_whose_bytes_are_not_base64_keeps_the_decoding_error() {
    let decoded: Result<IncomingSupervisorRequest, _> = serde_json::from_str(
        r#"{"request_id":4,"kind":{"Write":{"pane_id":"00000000-0000-0000-0000-000000000001","bytes":"a"}}}"#,
    );

    assert_eq!(
        decoded
            .expect_err("bytes that are not base64 are not this version's shape")
            .to_string(),
        "the base64 text length is not a multiple of four at line 1 column 70"
    );
}

#[test]
fn a_request_kind_carrying_a_field_this_build_does_not_know_still_reads() {
    let decoded: SupervisorRequest = serde_json::from_str(
        r#"{"request_id":3,"kind":{"Resize":{"pane_id":"00000000-0000-0000-0000-000000000001","size":{"cols":80,"rows":24},"priority":3}}}"#,
    )
    .expect("a field this build lacks is passed over");

    assert_eq!(
        decoded,
        SupervisorRequest {
            request_id: 3,
            request_kind: SupervisorRequestKind::Resize {
                pane_id: build_test_pane_id(),
                pty_size: build_test_pty_size(),
            },
        }
    );
}

#[test]
fn an_answer_carrying_a_field_this_build_does_not_know_still_reads() {
    let decoded: SupervisorResponse = serde_json::from_str(
        r#"{"request_id":7,"result":{"Panes":[{"pane_id":"00000000-0000-0000-0000-000000000001","pid":4242,"size":{"cols":80,"rows":24},"tty":"/dev/pts/3"}]}}"#,
    )
    .expect("a field this build lacks is passed over");

    assert_eq!(
        decoded,
        SupervisorResponse {
            request_id: Some(7),
            answer_result: SupervisorResult::Panes(vec![SupervisorPane {
                pane_id: build_test_pane_id(),
                process_id: 4242,
                pty_size: build_test_pty_size(),
            }]),
        }
    );
}

#[test]
fn an_event_carrying_a_field_this_build_does_not_know_still_reads() {
    let decoded: SupervisorMessage = serde_json::from_str(
        r#"{"Event":{"Exited":{"pane_id":"00000000-0000-0000-0000-000000000001","status":{"ExitCode":2},"seq":1}}}"#,
    )
    .expect("a field this build lacks is passed over");

    assert_eq!(
        decoded,
        SupervisorMessage::Event(SupervisorEvent::Exited {
            pane_id: build_test_pane_id(),
            exit_status: ExitStatus::ExitCode(2),
        })
    );
}

#[test]
fn a_hello_with_the_right_version_and_token_is_accepted() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Hello {
            min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
            max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
        }),
        Ok(())
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        Some(SUPERVISOR_PROTOCOL_VERSION)
    );
}

#[test]
fn an_accepted_hello_opens_the_gate_for_other_requests() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());

    gate.validate_request_kind(&SupervisorRequestKind::Hello {
        min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
        max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        connection_token: build_test_connection_token(),
    })
    .expect("the Hello is accepted");

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::ListPanes),
        Ok(())
    );
    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Shutdown),
        Ok(())
    );
}

#[test]
fn a_session_server_speaking_only_above_this_supervisor_is_refused_naming_both_ranges() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());
    let above = SUPERVISOR_PROTOCOL_VERSION + 1;

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            connection_token: build_test_connection_token(),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the session server speaks supervisor-link protocol versions {above} to {above}, \
                 this supervisor speaks {MIN_SUPERVISOR_PROTOCOL_VERSION} to \
                 {SUPERVISOR_PROTOCOL_VERSION}"
            ),
        })
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        None,
        "a refused Hello settles nothing"
    );
}

#[test]
fn a_session_server_reaching_above_this_supervisor_settles_on_the_supervisors_highest() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());

    gate.validate_request_kind(&SupervisorRequestKind::Hello {
        min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
        max_protocol_version: SUPERVISOR_PROTOCOL_VERSION + 3,
        connection_token: build_test_connection_token(),
    })
    .expect("a range covering this supervisor's is accepted");

    assert_eq!(
        gate.get_agreed_protocol_version(),
        Some(SUPERVISOR_PROTOCOL_VERSION)
    );
}

#[test]
fn a_session_server_speaking_only_below_this_supervisor_is_refused_naming_both_ranges() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());
    let below = MIN_SUPERVISOR_PROTOCOL_VERSION - 1;

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Hello {
            min_protocol_version: below,
            max_protocol_version: below,
            connection_token: build_test_connection_token(),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the session server speaks supervisor-link protocol versions {below} to {below}, \
                 this supervisor speaks {MIN_SUPERVISOR_PROTOCOL_VERSION} to \
                 {SUPERVISOR_PROTOCOL_VERSION}"
            ),
        })
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        None,
        "a refused Hello settles nothing"
    );
}

#[test]
fn a_session_server_reaching_below_and_above_this_supervisor_settles_on_the_supervisors_highest() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Hello {
            min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION - 1,
            max_protocol_version: SUPERVISOR_PROTOCOL_VERSION + 3,
            connection_token: build_test_connection_token(),
        }),
        Ok(())
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        Some(SUPERVISOR_PROTOCOL_VERSION)
    );
}

#[test]
fn a_hello_whose_lowest_version_is_above_its_highest_is_refused_naming_both_ranges() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());
    let lowest = SUPERVISOR_PROTOCOL_VERSION;
    let highest = MIN_SUPERVISOR_PROTOCOL_VERSION - 1;

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Hello {
            min_protocol_version: lowest,
            max_protocol_version: highest,
            connection_token: build_test_connection_token(),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the session server speaks supervisor-link protocol versions {lowest} to \
                 {highest}, this supervisor speaks {MIN_SUPERVISOR_PROTOCOL_VERSION} to \
                 {SUPERVISOR_PROTOCOL_VERSION}"
            ),
        })
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        None,
        "a refused Hello settles nothing"
    );
}

#[test]
fn a_hello_with_a_wrong_token_is_refused_as_bad_build_test_connection_token() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Hello {
            min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
            max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret("wrongToken"),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the supervisor's".to_string(),
        })
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        None,
        "a refused Hello settles nothing"
    );
}

#[test]
fn an_out_of_range_hello_with_a_wrong_token_is_refused_for_the_version() {
    // The gate checks the version before the connection_token: the two faults together
    // get the version's refusal.
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());
    let above = SUPERVISOR_PROTOCOL_VERSION + 1;

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            connection_token: ConnectionToken::from_secret("wrongToken"),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the session server speaks supervisor-link protocol versions {above} to {above}, \
                 this supervisor speaks {MIN_SUPERVISOR_PROTOCOL_VERSION} to \
                 {SUPERVISOR_PROTOCOL_VERSION}"
            ),
        })
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        None,
        "a refused Hello settles nothing"
    );
}

#[test]
fn a_second_hello_on_an_open_link_settles_the_same_version_and_changes_nothing() {
    // A second Hello on an open link is accepted, settles the version again
    // from its own range, and leaves the gate open.
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());
    let hello = SupervisorRequestKind::build_hello_request(build_test_connection_token());

    gate.validate_request_kind(&hello)
        .expect("the first Hello is accepted");
    let settled = gate.get_agreed_protocol_version();

    assert_eq!(
        gate.validate_request_kind(&hello),
        Ok(()),
        "the second Hello is accepted"
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        settled,
        "and settles the same version"
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        Some(SUPERVISOR_PROTOCOL_VERSION)
    );
    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::ListPanes),
        Ok(()),
        "and the link keeps serving every other kind"
    );
}

#[test]
fn a_wrong_token_arriving_on_an_open_link_is_refused_and_leaves_it_open() {
    // A Hello refused on an open link changes nothing: the version stands and
    // the link keeps serving.
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());
    gate.validate_request_kind(&SupervisorRequestKind::build_hello_request(
        build_test_connection_token(),
    ))
    .expect("the first Hello is accepted");

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Hello {
            min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
            max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret("wrongToken"),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the supervisor's".to_string(),
        })
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        Some(SUPERVISOR_PROTOCOL_VERSION),
        "the version the link settled on stands"
    );
    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::ListPanes),
        Ok(())
    );
}

#[test]
fn a_version_range_arriving_on_an_open_link_that_misses_this_one_leaves_it_open() {
    // Same as a wrong connection_token: the Hello is refused, and the open link is
    // untouched.
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());
    gate.validate_request_kind(&SupervisorRequestKind::build_hello_request(
        build_test_connection_token(),
    ))
    .expect("the first Hello is accepted");
    let above = SUPERVISOR_PROTOCOL_VERSION + 1;

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            connection_token: build_test_connection_token(),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the session server speaks supervisor-link protocol versions {above} to {above}, \
                 this supervisor speaks {MIN_SUPERVISOR_PROTOCOL_VERSION} to \
                 {SUPERVISOR_PROTOCOL_VERSION}"
            ),
        })
    );
    assert_eq!(
        gate.get_agreed_protocol_version(),
        Some(SUPERVISOR_PROTOCOL_VERSION)
    );
    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::ListPanes),
        Ok(())
    );
}

#[test]
fn a_request_before_any_hello_is_refused_as_hello_required() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        gate.validate_request_kind(&SupervisorRequestKind::ListPanes),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "ListPanes arrived before a Hello opened the link".to_string(),
        })
    );
}

#[test]
fn every_other_kind_is_refused_by_name_before_a_hello_and_served_after_one() {
    let kinds = [
        (
            SupervisorRequestKind::Spawn {
                pane_id: build_test_pane_id(),
                spawn_spec: build_test_spawn_spec(),
                pty_size: build_test_pty_size(),
            },
            "Spawn",
        ),
        (
            SupervisorRequestKind::Resize {
                pane_id: build_test_pane_id(),
                pty_size: build_test_pty_size(),
            },
            "Resize",
        ),
        (
            SupervisorRequestKind::Write {
                pane_id: build_test_pane_id(),
                input_bytes: vec![104],
            },
            "Write",
        ),
        (
            SupervisorRequestKind::Kill {
                pane_id: build_test_pane_id(),
                kill_policy: KillPolicy::Force,
            },
            "Kill",
        ),
        (
            SupervisorRequestKind::LiveCwd {
                pane_id: build_test_pane_id(),
            },
            "LiveCwd",
        ),
        (SupervisorRequestKind::ListPanes, "ListPanes"),
        (SupervisorRequestKind::PauseOutput, "PauseOutput"),
        (SupervisorRequestKind::ResumeOutput, "ResumeOutput"),
        (SupervisorRequestKind::Shutdown, "Shutdown"),
    ];
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());

    for (request_kind, request_kind_name) in &kinds {
        assert_eq!(
            gate.validate_request_kind(request_kind),
            Err(IpcErrorPayload {
                code: IpcErrorCode::HelloRequired,
                message: format!("{request_kind_name} arrived before a Hello opened the link"),
            })
        );
    }
    assert_eq!(
        gate.get_agreed_protocol_version(),
        None,
        "a refused request kind opens nothing"
    );

    gate.validate_request_kind(&SupervisorRequestKind::build_hello_request(
        build_test_connection_token(),
    ))
    .expect("the Hello is accepted");

    for (request_kind, request_kind_name) in &kinds {
        assert_eq!(
            gate.validate_request_kind(request_kind),
            Ok(()),
            "{request_kind_name} is served on an open link"
        );
    }
}

#[test]
fn an_unknown_kind_is_refused_by_name_once_the_gate_is_open() {
    let mut gate = SupervisorHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        gate.build_unknown_request_kind_error("Rehome"),
        IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Rehome arrived before a Hello opened the link".to_string(),
        }
    );

    gate.validate_request_kind(&SupervisorRequestKind::Hello {
        min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
        max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        connection_token: build_test_connection_token(),
    })
    .expect("the Hello is accepted");

    assert_eq!(
        gate.build_unknown_request_kind_error("Rehome"),
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this supervisor has no request kind named Rehome".to_string(),
        }
    );
}

#[test]
#[cfg(unix)]
fn the_supervisor_socket_sits_beside_the_sessions_own_socket() {
    let session = SessionId::from_uuid(build_fixed_test_uuid());

    assert_eq!(
        compute_supervisor_socket_address(Path::new("/run/user/1000/koshi"), session, 4821),
        "/run/user/1000/koshi/session-00000000-0000-0000-0000-000000000001-pty-4821.sock"
    );
    assert_ne!(
        compute_supervisor_socket_address(Path::new("/run/user/1000/koshi"), session, 4821),
        crate::endpoint::compute_socket_address(Path::new("/run/user/1000/koshi"), session)
    );
}

#[test]
#[cfg(windows)]
fn the_supervisor_pipe_sits_in_the_koshi_namespace_beside_the_sessions_own_pipe() {
    let session = SessionId::from_uuid(build_fixed_test_uuid());
    let runtime_directory = Path::new(r"C:\Users\u\AppData\Local\koshi");

    assert_eq!(
        compute_supervisor_socket_address(runtime_directory, session, 4821),
        "koshi-pty-session-00000000-0000-0000-0000-000000000001-4821"
    );
    assert_ne!(
        compute_supervisor_socket_address(runtime_directory, session, 4821),
        crate::endpoint::compute_socket_address(runtime_directory, session)
    );
}

/// Two supervisors of one session with different process ids listen at
/// different addresses.
#[test]
fn two_supervisors_of_one_session_listen_at_different_addresses() {
    let session = SessionId::from_uuid(build_fixed_test_uuid());
    let runtime_directory = Path::new("/run/user/1000/koshi");

    assert_ne!(
        compute_supervisor_socket_address(runtime_directory, session, 4821),
        compute_supervisor_socket_address(runtime_directory, session, 4822)
    );
}
