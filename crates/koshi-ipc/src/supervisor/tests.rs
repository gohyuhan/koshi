//! Tests for the pane-supervisor protocol: every request, answer and event
//! keeps the exact bytes this version pins, the gate opens for a Hello whose
//! version range overlaps the supervisor's and whose token matches, a kind the
//! supervisor does not have is refused by name, and the link address sits
//! beside the session's own control socket.

use std::collections::BTreeMap;

use koshi_core::process::ShellKind;

use super::*;
use crate::protocol::IpcErrorCode;

/// The one UUID every fixed id below uses.
fn fixed_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// The pane every fixed request below names.
fn pane() -> PaneId {
    PaneId::from_uuid(fixed_uuid())
}

/// A token holding a fixed secret.
fn token() -> ConnectionToken {
    ConnectionToken::new("k7QxSecret")
}

/// The size every fixed request below names.
fn size() -> PtySize {
    PtySize { cols: 80, rows: 24 }
}

/// A spawn spec at fixed values, so its encoding is byte-stable.
fn spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), "echo hi".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Other("sh".to_string()),
    }
}

/// Encode `message` as the exact bytes that go on the wire.
fn encode<T: Serialize>(message: &T) -> String {
    serde_json::to_string(message).expect("message encodes")
}

#[test]
fn the_supervisor_link_wire_shape_belongs_to_this_protocol_version() {
    // Every request kind, every answer and every event, pinned byte for byte.
    //
    // A session server and the supervisor it reconnects to can be different
    // builds, because a supervisor keeps running the image it started from.
    // The version in the Hello is the only thing that catches a pair that does
    // not agree on this shape. Round-trip tests cannot catch it: one build
    // encoding and decoding its own structs always agrees with itself.
    assert_eq!(
        encode(&SupervisorRequest {
            request_id: 1,
            kind: SupervisorRequestKind::Hello {
                min_protocol_version: 1,
                max_protocol_version: 1,
                token: token(),
            },
        }),
        r#"{"request_id":1,"kind":{"Hello":{"min_protocol_version":1,"max_protocol_version":1,"token":"k7QxSecret"}}}"#
    );
    assert_eq!(
        encode(&SupervisorRequest {
            request_id: 2,
            kind: SupervisorRequestKind::Spawn {
                pane_id: pane(),
                spec: spec(),
                size: size(),
            },
        }),
        r#"{"request_id":2,"kind":{"Spawn":{"pane_id":"00000000-0000-0000-0000-000000000001","spec":{"program":"/bin/sh","args":["-c","echo hi"],"cwd":null,"env":{},"shell_kind":{"Other":"sh"}},"size":{"cols":80,"rows":24}}}}"#
    );
    assert_eq!(
        encode(&SupervisorRequest {
            request_id: 3,
            kind: SupervisorRequestKind::Resize {
                pane_id: pane(),
                size: size(),
            },
        }),
        r#"{"request_id":3,"kind":{"Resize":{"pane_id":"00000000-0000-0000-0000-000000000001","size":{"cols":80,"rows":24}}}}"#
    );
    assert_eq!(
        encode(&SupervisorRequest {
            request_id: 4,
            kind: SupervisorRequestKind::Write {
                pane_id: pane(),
                bytes: vec![104, 105],
            },
        }),
        r#"{"request_id":4,"kind":{"Write":{"pane_id":"00000000-0000-0000-0000-000000000001","bytes":"aGk="}}}"#
    );
    assert_eq!(
        encode(&SupervisorRequest {
            request_id: 5,
            kind: SupervisorRequestKind::Kill {
                pane_id: pane(),
                kill_policy: KillPolicy::Tree,
            },
        }),
        r#"{"request_id":5,"kind":{"Kill":{"pane_id":"00000000-0000-0000-0000-000000000001","kill_policy":"Tree"}}}"#
    );
    assert_eq!(
        encode(&SupervisorRequest {
            request_id: 6,
            kind: SupervisorRequestKind::LiveCwd { pane_id: pane() },
        }),
        r#"{"request_id":6,"kind":{"LiveCwd":{"pane_id":"00000000-0000-0000-0000-000000000001"}}}"#
    );
    assert_eq!(
        encode(&SupervisorRequest {
            request_id: 7,
            kind: SupervisorRequestKind::ListPanes,
        }),
        r#"{"request_id":7,"kind":"ListPanes"}"#
    );
    assert_eq!(
        encode(&SupervisorRequest {
            request_id: 8,
            kind: SupervisorRequestKind::Shutdown,
        }),
        r#"{"request_id":8,"kind":"Shutdown"}"#
    );

    assert_eq!(
        encode(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(1),
                result: SupervisorResult::Hello {
                    protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                },
            }
        )),
        r#"{"Response":{"request_id":1,"result":{"Hello":{"protocol_version":1}}}}"#
    );
    assert_eq!(
        encode(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(2),
                result: SupervisorResult::Spawned { pid: 4242 },
            }
        )),
        r#"{"Response":{"request_id":2,"result":{"Spawned":{"pid":4242}}}}"#
    );
    assert_eq!(
        encode(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(7),
                result: SupervisorResult::Panes(vec![SupervisorPane {
                    pane_id: pane(),
                    pid: 4242,
                    size: size(),
                }]),
            }
        )),
        r#"{"Response":{"request_id":7,"result":{"Panes":[{"pane_id":"00000000-0000-0000-0000-000000000001","pid":4242,"size":{"cols":80,"rows":24}}]}}}"#
    );
    assert_eq!(
        encode(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(6),
                result: SupervisorResult::Cwd(Some(PathBuf::from("/home/dev/api"))),
            }
        )),
        r#"{"Response":{"request_id":6,"result":{"Cwd":"/home/dev/api"}}}"#
    );
    assert_eq!(
        encode(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: Some(3),
                result: SupervisorResult::Done,
            }
        )),
        r#"{"Response":{"request_id":3,"result":"Done"}}"#
    );
    assert_eq!(
        encode(&SupervisorMessage::<_, SupervisorEvent>::Response(
            SupervisorResponse {
                request_id: None,
                result: SupervisorResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::MalformedRequest,
                    message: "the request could not be read".to_string(),
                }),
            }
        )),
        r#"{"Response":{"request_id":null,"result":{"Error":{"code":"malformed_request","message":"the request could not be read"}}}}"#
    );

    assert_eq!(
        encode(&SupervisorMessage::<SupervisorResult, _>::Event(
            SupervisorEvent::Output {
                pane_id: pane(),
                bytes: vec![104, 105],
            }
        )),
        r#"{"Event":{"Output":{"pane_id":"00000000-0000-0000-0000-000000000001","bytes":"aGk="}}}"#
    );
    assert_eq!(
        encode(&SupervisorMessage::<SupervisorResult, _>::Event(
            SupervisorEvent::Exited {
                pane_id: pane(),
                status: ExitStatus::ExitCode(0),
            }
        )),
        r#"{"Event":{"Exited":{"pane_id":"00000000-0000-0000-0000-000000000001","status":{"ExitCode":0}}}}"#
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
            token: token(),
        }
        .name(),
        "Hello"
    );
    assert_eq!(
        SupervisorRequestKind::Spawn {
            pane_id: pane(),
            spec: spec(),
            size: size(),
        }
        .name(),
        "Spawn"
    );
    assert_eq!(
        SupervisorRequestKind::Resize {
            pane_id: pane(),
            size: size(),
        }
        .name(),
        "Resize"
    );
    assert_eq!(
        SupervisorRequestKind::Write {
            pane_id: pane(),
            bytes: vec![104],
        }
        .name(),
        "Write"
    );
    assert_eq!(
        SupervisorRequestKind::Kill {
            pane_id: pane(),
            kill_policy: KillPolicy::Tree,
        }
        .name(),
        "Kill"
    );
    assert_eq!(
        SupervisorRequestKind::LiveCwd { pane_id: pane() }.name(),
        "LiveCwd"
    );
    assert_eq!(SupervisorRequestKind::ListPanes.name(), "ListPanes");
    assert_eq!(SupervisorRequestKind::PauseOutput.name(), "PauseOutput");
    assert_eq!(SupervisorRequestKind::ResumeOutput.name(), "ResumeOutput");
    assert_eq!(SupervisorRequestKind::Shutdown.name(), "Shutdown");
}

#[test]
fn every_event_names_itself_without_its_payload() {
    assert_eq!(
        SupervisorEvent::Output {
            pane_id: pane(),
            bytes: vec![104],
        }
        .name(),
        "Output"
    );
    assert_eq!(
        SupervisorEvent::Exited {
            pane_id: pane(),
            status: ExitStatus::Signaled(9),
        }
        .name(),
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
fn a_kind_this_build_does_not_have_reads_as_its_name_alone() {
    let request: IncomingSupervisorRequest =
        serde_json::from_str(r#"{"request_id":9,"kind":{"Rehome":{"pane_id":1}}}"#)
            .expect("a kind this build lacks still reads");

    assert_eq!(
        request,
        SupervisorRequest {
            request_id: 9,
            kind: MaybeKnown::Unknown {
                name: "Rehome".to_string(),
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
            result: MaybeKnown::Unknown {
                name: "Rehomed".to_string(),
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
            name: "Stalled".to_string(),
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
fn a_hello_with_the_right_version_and_token_is_accepted() {
    let mut gate = SupervisorHandshake::new(token());

    assert_eq!(
        gate.check(&SupervisorRequestKind::Hello {
            min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
            max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            token: token(),
        }),
        Ok(())
    );
    assert_eq!(gate.agreed(), Some(SUPERVISOR_PROTOCOL_VERSION));
}

#[test]
fn an_accepted_hello_opens_the_gate_for_other_requests() {
    let mut gate = SupervisorHandshake::new(token());

    gate.check(&SupervisorRequestKind::Hello {
        min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
        max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        token: token(),
    })
    .expect("the Hello is accepted");

    assert_eq!(gate.check(&SupervisorRequestKind::ListPanes), Ok(()));
    assert_eq!(gate.check(&SupervisorRequestKind::Shutdown), Ok(()));
}

#[test]
fn a_session_server_speaking_only_above_this_supervisor_is_refused_naming_both_ranges() {
    let mut gate = SupervisorHandshake::new(token());
    let above = SUPERVISOR_PROTOCOL_VERSION + 1;

    assert_eq!(
        gate.check(&SupervisorRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            token: token(),
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
    assert_eq!(gate.agreed(), None, "a refused Hello settles nothing");
}

#[test]
fn a_session_server_reaching_above_this_supervisor_settles_on_the_supervisors_highest() {
    let mut gate = SupervisorHandshake::new(token());

    gate.check(&SupervisorRequestKind::Hello {
        min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
        max_protocol_version: SUPERVISOR_PROTOCOL_VERSION + 3,
        token: token(),
    })
    .expect("a range covering this supervisor's is accepted");

    assert_eq!(gate.agreed(), Some(SUPERVISOR_PROTOCOL_VERSION));
}

#[test]
fn a_hello_with_a_wrong_token_is_refused_as_bad_token() {
    let mut gate = SupervisorHandshake::new(token());

    assert_eq!(
        gate.check(&SupervisorRequestKind::Hello {
            min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
            max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            token: ConnectionToken::new("wrongToken"),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the supervisor's".to_string(),
        })
    );
    assert_eq!(gate.agreed(), None, "a refused Hello settles nothing");
}

#[test]
fn an_out_of_range_hello_with_a_wrong_token_is_refused_for_the_version() {
    // The gate settles the version before it looks at the token, so the two
    // faults together earn the version's refusal.
    let mut gate = SupervisorHandshake::new(token());
    let above = SUPERVISOR_PROTOCOL_VERSION + 1;

    assert_eq!(
        gate.check(&SupervisorRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            token: ConnectionToken::new("wrongToken"),
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
    assert_eq!(gate.agreed(), None, "a refused Hello settles nothing");
}

#[test]
fn a_second_hello_on_an_open_link_settles_the_same_version_and_changes_nothing() {
    // A session server may send its Hello again — a link it is unsure of, or a
    // retry after a request it could not read. The link is already open, so the
    // second one has to answer the same and leave the gate as it was.
    let mut gate = SupervisorHandshake::new(token());
    let hello = SupervisorRequestKind::hello(token());

    gate.check(&hello).expect("the first Hello is accepted");
    let settled = gate.agreed();

    assert_eq!(gate.check(&hello), Ok(()), "the second Hello is accepted");
    assert_eq!(gate.agreed(), settled, "and settles the same version");
    assert_eq!(gate.agreed(), Some(SUPERVISOR_PROTOCOL_VERSION));
    assert_eq!(
        gate.check(&SupervisorRequestKind::ListPanes),
        Ok(()),
        "and the link keeps serving every other kind"
    );
}

#[test]
fn a_wrong_token_arriving_on_an_open_link_is_refused_and_leaves_it_open() {
    // The gate is one link's own, and a Hello it refuses changes nothing. A
    // refusal that shut the gate would end the panes' only route out over a
    // request that was already answered.
    let mut gate = SupervisorHandshake::new(token());
    gate.check(&SupervisorRequestKind::hello(token()))
        .expect("the first Hello is accepted");

    assert_eq!(
        gate.check(&SupervisorRequestKind::Hello {
            min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
            max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            token: ConnectionToken::new("wrongToken"),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the supervisor's".to_string(),
        })
    );
    assert_eq!(
        gate.agreed(),
        Some(SUPERVISOR_PROTOCOL_VERSION),
        "the version the link settled on stands"
    );
    assert_eq!(gate.check(&SupervisorRequestKind::ListPanes), Ok(()));
}

#[test]
fn a_version_range_arriving_on_an_open_link_that_misses_this_one_leaves_it_open() {
    // Same as a wrong token: the Hello is refused on its own, and the link the
    // panes are already being driven over is untouched.
    let mut gate = SupervisorHandshake::new(token());
    gate.check(&SupervisorRequestKind::hello(token()))
        .expect("the first Hello is accepted");
    let above = SUPERVISOR_PROTOCOL_VERSION + 1;

    assert_eq!(
        gate.check(&SupervisorRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            token: token(),
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
    assert_eq!(gate.agreed(), Some(SUPERVISOR_PROTOCOL_VERSION));
    assert_eq!(gate.check(&SupervisorRequestKind::ListPanes), Ok(()));
}

#[test]
fn a_request_before_any_hello_is_refused_as_hello_required() {
    let mut gate = SupervisorHandshake::new(token());

    assert_eq!(
        gate.check(&SupervisorRequestKind::ListPanes),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "ListPanes arrived before a Hello opened the link".to_string(),
        })
    );
}

#[test]
fn an_unknown_kind_is_refused_by_name_once_the_gate_is_open() {
    let mut gate = SupervisorHandshake::new(token());

    assert_eq!(
        gate.refuse_unknown("Rehome"),
        IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Rehome arrived before a Hello opened the link".to_string(),
        }
    );

    gate.check(&SupervisorRequestKind::Hello {
        min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
        max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        token: token(),
    })
    .expect("the Hello is accepted");

    assert_eq!(
        gate.refuse_unknown("Rehome"),
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this supervisor has no request kind named Rehome".to_string(),
        }
    );
}

#[test]
#[cfg(unix)]
fn the_supervisor_socket_sits_beside_the_sessions_own_socket() {
    let session = SessionId::from_uuid(fixed_uuid());

    assert_eq!(
        supervisor_socket_addr(Path::new("/run/user/1000/koshi"), session, 4821),
        "/run/user/1000/koshi/session-00000000-0000-0000-0000-000000000001-pty-4821.sock"
    );
    assert_ne!(
        supervisor_socket_addr(Path::new("/run/user/1000/koshi"), session, 4821),
        crate::endpoint::socket_addr(Path::new("/run/user/1000/koshi"), session)
    );
}

#[test]
#[cfg(windows)]
fn the_supervisor_pipe_sits_in_the_koshi_namespace_beside_the_sessions_own_pipe() {
    let session = SessionId::from_uuid(fixed_uuid());
    let runtime_dir = Path::new(r"C:\Users\u\AppData\Local\koshi");

    assert_eq!(
        supervisor_socket_addr(runtime_dir, session, 4821),
        "koshi-pty-session-00000000-0000-0000-0000-000000000001-4821"
    );
    assert_ne!(
        supervisor_socket_addr(runtime_dir, session, 4821),
        crate::endpoint::socket_addr(runtime_dir, session)
    );
}

/// Two supervisors of one session never listen at the same address, so the one
/// replacing the other never binds an address its predecessor still holds.
#[test]
fn two_supervisors_of_one_session_listen_at_different_addresses() {
    let session = SessionId::from_uuid(fixed_uuid());
    let runtime_dir = Path::new("/run/user/1000/koshi");

    assert_ne!(
        supervisor_socket_addr(runtime_dir, session, 4821),
        supervisor_socket_addr(runtime_dir, session, 4822)
    );
}
