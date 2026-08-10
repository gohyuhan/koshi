//! Tests for the control-plane protocol: every request and answer keeps the
//! exact bytes this version pins, the gate opens for a Hello whose version
//! range overlaps the router's and whose token matches, and the router's
//! socket, endpoint and lock names sit where the trust checks accept them.

use std::time::{Duration, UNIX_EPOCH};

use koshi_core::ids::{ClientId, SessionId};

use super::*;

/// The one UUID every fixed id below uses.
fn fixed_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// A token holding a fixed secret.
fn token() -> ConnectionToken {
    ConnectionToken::new("k7QxSecret")
}

/// One session's address, at fixed values, so its encoding is byte-stable.
fn address() -> SessionAddress {
    SessionAddress {
        id: SessionId::from_uuid(fixed_uuid()),
        name: "quiet-lake".to_string(),
        socket: "/run/koshi/session.sock".to_string(),
        pid: 4242,
    }
}

/// One list row, at fixed ids and times, so its encoding is byte-stable.
fn session_info() -> SessionInfo {
    SessionInfo {
        id: SessionId::from_uuid(fixed_uuid()),
        name: "quiet-lake".to_string(),
        created_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        attached_clients: vec![ClientId::from_uuid(fixed_uuid())],
        pane_count: 1,
    }
}

/// Encode `message` as the exact bytes that go on the wire.
fn encode<T: Serialize>(message: &T) -> String {
    serde_json::to_string(message).expect("message encodes")
}

#[test]
fn the_control_plane_wire_shape_belongs_to_this_protocol_version() {
    // Every request kind, every answer, and the session server's ready line,
    // pinned byte for byte.
    //
    // Two builds only understand each other's bytes when they agree on this
    // shape, and the version in the Hello is the only thing that catches a
    // pair that does not. The version moves once per release cycle, not once
    // per change, so a shape edit inside an unreleased cycle leaves it alone.
    //
    // Shape as of control-plane protocol version 1. Round-trip tests cannot
    // catch this: one build encoding and decoding its own structs always
    // agrees with itself.
    assert_eq!(
        encode(&RouterRequest {
            request_id: 1,
            kind: RouterRequestKind::Hello {
                min_protocol_version: 1,
                max_protocol_version: 1,
                token: token(),
            },
        }),
        r#"{"request_id":1,"kind":{"Hello":{"min_protocol_version":1,"max_protocol_version":1,"token":"k7QxSecret"}}}"#
    );
    assert_eq!(
        encode(&RouterRequest {
            request_id: 2,
            kind: RouterRequestKind::CreateSession {
                profile: None,
                cwd: None,
                allow_other_users: None,
            },
        }),
        r#"{"request_id":2,"kind":{"CreateSession":{"profile":null,"cwd":null,"allow_other_users":null}}}"#
    );
    assert_eq!(
        encode(&RouterRequest {
            request_id: 2,
            kind: RouterRequestKind::CreateSession {
                profile: Some("dev".to_string()),
                cwd: None,
                allow_other_users: None,
            },
        }),
        r#"{"request_id":2,"kind":{"CreateSession":{"profile":"dev","cwd":null,"allow_other_users":null}}}"#
    );
    assert_eq!(
        encode(&RouterRequest {
            request_id: 2,
            kind: RouterRequestKind::CreateSession {
                profile: Some("dev".to_string()),
                cwd: Some(PathBuf::from("/home/dev/api")),
                allow_other_users: None,
            },
        }),
        r#"{"request_id":2,"kind":{"CreateSession":{"profile":"dev","cwd":"/home/dev/api","allow_other_users":null}}}"#
    );
    assert_eq!(
        encode(&RouterRequest {
            request_id: 2,
            kind: RouterRequestKind::CreateSession {
                profile: None,
                cwd: None,
                allow_other_users: Some(true),
            },
        }),
        r#"{"request_id":2,"kind":{"CreateSession":{"profile":null,"cwd":null,"allow_other_users":true}}}"#
    );
    assert_eq!(
        encode(&RouterRequest {
            request_id: 3,
            kind: RouterRequestKind::AttachLookup {
                selector: SessionSelector::Id(SessionId::from_uuid(fixed_uuid())),
            },
        }),
        r#"{"request_id":3,"kind":{"AttachLookup":{"selector":{"Id":"00000000-0000-0000-0000-000000000001"}}}}"#
    );
    assert_eq!(
        encode(&RouterRequest {
            request_id: 4,
            kind: RouterRequestKind::ListSessions,
        }),
        r#"{"request_id":4,"kind":"ListSessions"}"#
    );

    assert_eq!(
        encode(&RouterResponse {
            request_id: Some(1),
            result: RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
            },
        }),
        r#"{"request_id":1,"result":{"Hello":{"protocol_version":1}}}"#
    );
    assert_eq!(
        encode(&RouterResponse {
            request_id: Some(2),
            result: RouterResult::Created(address()),
        }),
        r#"{"request_id":2,"result":{"Created":{"id":"00000000-0000-0000-0000-000000000001","name":"quiet-lake","socket":"/run/koshi/session.sock","pid":4242}}}"#
    );
    assert_eq!(
        encode(&RouterResponse {
            request_id: Some(3),
            result: RouterResult::Found(address()),
        }),
        r#"{"request_id":3,"result":{"Found":{"id":"00000000-0000-0000-0000-000000000001","name":"quiet-lake","socket":"/run/koshi/session.sock","pid":4242}}}"#
    );
    assert_eq!(
        encode(&RouterResponse {
            request_id: Some(4),
            result: RouterResult::Sessions(vec![session_info()]),
        }),
        r#"{"request_id":4,"result":{"Sessions":[{"id":"00000000-0000-0000-0000-000000000001","name":"quiet-lake","created_at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},"attached_clients":["00000000-0000-0000-0000-000000000001"],"pane_count":1}]}}"#
    );
    assert_eq!(
        encode(&RouterResponse {
            request_id: None,
            result: RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the request could not be read".to_string(),
            }),
        }),
        r#"{"request_id":null,"result":{"Error":{"code":"malformed_request","message":"the request could not be read"}}}"#
    );

    assert_eq!(
        encode(&SessionServerReady {
            protocol_version: 1,
            socket: "/run/koshi/session.sock".to_string(),
        }),
        r#"{"protocol_version":1,"socket":"/run/koshi/session.sock"}"#
    );
}

#[test]
fn the_control_plane_version_this_build_speaks_is_one() {
    // Born in 0.2.0 and never released, so shape edits inside this cycle leave
    // it at 1.
    assert_eq!(ROUTER_PROTOCOL_VERSION, 1);
}

#[test]
fn every_request_kind_names_itself_without_its_payload() {
    assert_eq!(
        RouterRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            token: token(),
        }
        .name(),
        "Hello"
    );
    assert_eq!(
        RouterRequestKind::CreateSession {
            profile: None,
            cwd: None,
            allow_other_users: None,
        }
        .name(),
        "CreateSession"
    );
    assert_eq!(
        RouterRequestKind::AttachLookup {
            selector: SessionSelector::Name("quiet-lake".to_string()),
        }
        .name(),
        "AttachLookup"
    );
    assert_eq!(RouterRequestKind::ListSessions.name(), "ListSessions");
}

#[test]
fn a_request_carrying_an_unknown_field_is_refused() {
    let decoded: Result<RouterRequest, _> =
        serde_json::from_str(r#"{"request_id":1,"kind":"ListSessions","junk":5}"#);

    assert_eq!(
        decoded
            .expect_err("an unknown field is not this version's shape")
            .to_string(),
        "unknown field `junk`, expected `request_id` or `kind` at line 1 column 44"
    );
}

#[test]
fn a_create_session_carrying_the_other_users_answer_decodes() {
    let decoded: RouterRequest = serde_json::from_str(
        r#"{"request_id":2,"kind":{"CreateSession":{"profile":null,"cwd":null,"allow_other_users":true}}}"#,
    )
    .expect("a create naming the other-users answer is this version's shape");

    assert_eq!(
        decoded,
        RouterRequest {
            request_id: 2,
            kind: RouterRequestKind::CreateSession {
                profile: None,
                cwd: None,
                allow_other_users: Some(true),
            },
        }
    );
}

#[test]
fn a_create_session_naming_no_other_users_answer_leaves_it_to_the_session() {
    // What a build that asked for a session before this field existed looks
    // like here. It reads as "no answer given", which leaves the session's own
    // `koshi.kdl` to decide, so such a caller keeps the reachability it had.
    let decoded: RouterRequest = serde_json::from_str(
        r#"{"request_id":2,"kind":{"CreateSession":{"profile":null,"cwd":null}}}"#,
    )
    .expect("a create naming no other-users answer still reads");

    assert_eq!(
        decoded,
        RouterRequest {
            request_id: 2,
            kind: RouterRequestKind::CreateSession {
                profile: None,
                cwd: None,
                allow_other_users: None,
            },
        }
    );
}

#[test]
fn a_session_address_missing_its_pid_is_refused() {
    // What a build that advertised no process id looks like here. Decoding
    // must fail rather than fill in a default, so the mismatch surfaces
    // instead of producing a row that names process 0.
    let decoded: Result<SessionAddress, _> = serde_json::from_str(
        r#"{"id":"00000000-0000-0000-0000-000000000001","name":"quiet-lake","socket":"/run/koshi/session.sock"}"#,
    );

    assert_eq!(
        decoded
            .expect_err("an address without a pid is not this version's shape")
            .to_string(),
        "missing field `pid` at line 1 column 100"
    );
}

#[test]
fn a_hello_with_the_right_version_and_token_is_accepted() {
    let mut gate = RouterHandshake::new(token());

    assert_eq!(
        gate.check(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            token: token(),
        }),
        Ok(())
    );
}

#[test]
fn an_accepted_hello_opens_the_gate_for_other_requests() {
    let mut gate = RouterHandshake::new(token());

    gate.check(&RouterRequestKind::Hello {
        min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
        max_protocol_version: ROUTER_PROTOCOL_VERSION,
        token: token(),
    })
    .expect("the Hello is accepted");

    assert_eq!(
        gate.check(&RouterRequestKind::CreateSession {
            profile: None,
            cwd: None,
            allow_other_users: None,
        }),
        Ok(())
    );
    assert_eq!(gate.check(&RouterRequestKind::ListSessions), Ok(()));
}

#[test]
fn a_caller_speaking_only_above_this_router_is_refused_naming_both_ranges() {
    let mut gate = RouterHandshake::new(token());
    let above = ROUTER_PROTOCOL_VERSION + 1;

    assert_eq!(
        gate.check(&RouterRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            token: token(),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the caller speaks control-plane protocol versions {above} to {above}, \
                 this router speaks {MIN_ROUTER_PROTOCOL_VERSION} to {ROUTER_PROTOCOL_VERSION}"
            ),
        })
    );
    assert_eq!(gate.agreed(), None, "a refused Hello settles nothing");
}

#[test]
fn a_caller_reaching_above_this_router_settles_on_the_routers_highest() {
    let mut gate = RouterHandshake::new(token());

    gate.check(&RouterRequestKind::Hello {
        min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
        max_protocol_version: ROUTER_PROTOCOL_VERSION + 3,
        token: token(),
    })
    .expect("a range covering this router's is accepted");

    assert_eq!(gate.agreed(), Some(ROUTER_PROTOCOL_VERSION));
}

#[test]
fn an_unknown_kind_is_refused_by_name_once_the_gate_is_open() {
    let mut gate = RouterHandshake::new(token());

    assert_eq!(
        gate.refuse_unknown("Rehome"),
        IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Rehome arrived before a Hello opened the connection".to_string(),
        }
    );

    gate.check(&RouterRequestKind::Hello {
        min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
        max_protocol_version: ROUTER_PROTOCOL_VERSION,
        token: token(),
    })
    .expect("the Hello is accepted");

    assert_eq!(
        gate.refuse_unknown("Rehome"),
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this router has no request kind named Rehome".to_string(),
        }
    );
}

#[test]
fn an_out_of_range_hello_with_a_wrong_token_is_refused_for_the_version() {
    let mut gate = RouterHandshake::new(token());
    let above = ROUTER_PROTOCOL_VERSION + 1;

    assert_eq!(
        gate.check(&RouterRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            token: ConnectionToken::new("wrongToken"),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the caller speaks control-plane protocol versions {above} to {above}, \
                 this router speaks {MIN_ROUTER_PROTOCOL_VERSION} to {ROUTER_PROTOCOL_VERSION}"
            ),
        })
    );
}

#[test]
fn a_hello_with_a_wrong_token_is_refused_as_bad_token() {
    let mut gate = RouterHandshake::new(token());

    assert_eq!(
        gate.check(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            token: ConnectionToken::new("wrongToken"),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the router's".to_string(),
        })
    );
}

#[test]
fn a_request_before_any_hello_is_refused_as_hello_required() {
    let mut gate = RouterHandshake::new(token());

    assert_eq!(
        gate.check(&RouterRequestKind::CreateSession {
            profile: None,
            cwd: None,
            allow_other_users: None,
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "CreateSession arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_hello_required_refusal_names_the_kind_without_its_payload() {
    let mut gate = RouterHandshake::new(token());

    assert_eq!(
        gate.check(&RouterRequestKind::AttachLookup {
            selector: SessionSelector::Name("quiet-lake".to_string()),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "AttachLookup arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_refused_hello_leaves_the_gate_closed() {
    let mut gate = RouterHandshake::new(token());

    gate.check(&RouterRequestKind::Hello {
        min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
        max_protocol_version: ROUTER_PROTOCOL_VERSION,
        token: ConnectionToken::new("wrongToken"),
    })
    .expect_err("the Hello is refused");

    assert_eq!(
        gate.check(&RouterRequestKind::ListSessions),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "ListSessions arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_good_hello_after_a_refusal_opens_the_gate() {
    let mut gate = RouterHandshake::new(token());

    gate.check(&RouterRequestKind::Hello {
        min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
        max_protocol_version: ROUTER_PROTOCOL_VERSION,
        token: ConnectionToken::new("wrongToken"),
    })
    .expect_err("the Hello is refused");
    gate.check(&RouterRequestKind::Hello {
        min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
        max_protocol_version: ROUTER_PROTOCOL_VERSION,
        token: token(),
    })
    .expect("the Hello is accepted");

    assert_eq!(
        gate.check(&RouterRequestKind::CreateSession {
            profile: None,
            cwd: None,
            allow_other_users: None,
        }),
        Ok(())
    );
}

#[test]
fn a_refused_hello_on_an_open_gate_leaves_it_open() {
    let mut gate = RouterHandshake::new(token());

    gate.check(&RouterRequestKind::Hello {
        min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
        max_protocol_version: ROUTER_PROTOCOL_VERSION,
        token: token(),
    })
    .expect("the Hello is accepted");
    gate.check(&RouterRequestKind::Hello {
        min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
        max_protocol_version: ROUTER_PROTOCOL_VERSION,
        token: ConnectionToken::new("wrongToken"),
    })
    .expect_err("the Hello is refused");

    assert_eq!(
        gate.check(&RouterRequestKind::CreateSession {
            profile: None,
            cwd: None,
            allow_other_users: None,
        }),
        Ok(())
    );
}

#[test]
#[cfg(unix)]
fn the_router_socket_sits_directly_inside_the_runtime_directory() {
    assert_eq!(
        router_socket_addr(Path::new("/run/user/1000/koshi")),
        "/run/user/1000/koshi/router.sock"
    );
}

#[test]
#[cfg(windows)]
fn each_runtime_directory_gets_its_own_router_pipe_in_the_koshi_namespace() {
    const PREFIX: &str = "koshi-router-";

    let pipe = router_socket_addr(Path::new(r"C:\Users\u\AppData\Local\koshi"));
    let pipe_again = router_socket_addr(Path::new(r"C:\Users\u\AppData\Local\koshi"));
    let other_pipe = router_socket_addr(Path::new(r"C:\Users\u\AppData\Local\koshi-test"));

    assert_eq!(pipe, pipe_again);
    assert_ne!(pipe, other_pipe);
    assert_eq!(&pipe[..PREFIX.len()], PREFIX);
    assert_eq!(pipe.len(), PREFIX.len() + 16);
    assert_eq!(
        pipe[PREFIX.len()..].trim_matches(|c: char| c.is_ascii_hexdigit()),
        ""
    );
}

#[test]
fn the_router_socket_address_passes_the_trust_check() {
    let runtime_dir = tempfile::tempdir().expect("a temporary directory is created");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(runtime_dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("the runtime directory mode is set");
    }

    crate::validate::validate_socket_addr(
        &router_socket_addr(runtime_dir.path()),
        runtime_dir.path(),
    )
    .expect("the router socket sits where the trust check accepts it");
}

#[test]
fn the_router_endpoint_and_lock_files_sit_beside_the_socket() {
    let runtime_dir = Path::new("/run/user/1000/koshi");

    assert_eq!(
        router_endpoint_path(runtime_dir),
        PathBuf::from("/run/user/1000/koshi/router.json")
    );
    assert_eq!(
        router_lock_path(runtime_dir),
        PathBuf::from("/run/user/1000/koshi/router.lock")
    );
}
