//! Tests for the control-plane protocol: every request and answer keeps the
//! exact bytes this version pins, the gate opens for a Hello whose version
//! range overlaps the router's and whose token matches, and the router's
//! socket, endpoint and lock names sit where the trust checks accept them.

use std::time::{Duration, UNIX_EPOCH};

use koshi_core::ids::{ClientId, SessionId};

use super::*;
use crate::protocol::IpcErrorCode;

/// The one UUID every fixed id below uses.
fn build_fixed_test_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// A token holding a fixed secret.
fn build_test_connection_token() -> ConnectionToken {
    ConnectionToken::from_secret("k7QxSecret")
}

/// One session's address, at fixed values, so its encoding is byte-stable.
fn build_test_session_address() -> SessionAddress {
    SessionAddress {
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        session_name: "quiet-lake".to_string(),
        socket_address: "/run/koshi/session.sock".to_string(),
        process_id: 4242,
    }
}

/// One list row, at fixed ids and times, so its encoding is byte-stable.
fn build_test_session_discovery() -> SessionDiscovery {
    SessionDiscovery {
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        session_name: "quiet-lake".to_string(),
        created_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        attached_client_ids: vec![ClientId::from_uuid(build_fixed_test_uuid())],
        pane_count: 1,
    }
}

/// One remote access grant, at fixed values, so its encoding is byte-stable.
fn build_test_token_entry() -> TokenEntry {
    TokenEntry {
        identity: "build-box".to_string(),
        scope: TokenScope::HostWide,
        issued_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
    }
}

/// Encode `message` as the exact bytes that go on the wire.
fn serialize_test_wire_message<T: Serialize>(message: &T) -> String {
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
    // Shape as of control-plane protocol version 3. Round-trip tests cannot
    // catch this: one build encoding and decoding its own structs always
    // agrees with itself.
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 1,
            request_kind: RouterRequestKind::Hello {
                min_protocol_version: 3,
                max_protocol_version: 3,
                connection_token: build_test_connection_token(),
            },
        }),
        r#"{"request_id":1,"request_kind":{"Hello":{"min_protocol_version":3,"max_protocol_version":3,"connection_token":"k7QxSecret"}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::CreateSession {
                profile: None,
                working_directory: None,
                is_other_user_access_allowed: None,
            },
        }),
        r#"{"request_id":2,"request_kind":{"CreateSession":{"profile":null,"working_directory":null,"is_other_user_access_allowed":null}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::CreateSession {
                profile: Some("dev".to_string()),
                working_directory: None,
                is_other_user_access_allowed: None,
            },
        }),
        r#"{"request_id":2,"request_kind":{"CreateSession":{"profile":"dev","working_directory":null,"is_other_user_access_allowed":null}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::CreateSession {
                profile: Some("dev".to_string()),
                working_directory: Some(PathBuf::from("/home/dev/api")),
                is_other_user_access_allowed: None,
            },
        }),
        r#"{"request_id":2,"request_kind":{"CreateSession":{"profile":"dev","working_directory":"/home/dev/api","is_other_user_access_allowed":null}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::CreateSession {
                profile: None,
                working_directory: None,
                is_other_user_access_allowed: Some(true),
            },
        }),
        r#"{"request_id":2,"request_kind":{"CreateSession":{"profile":null,"working_directory":null,"is_other_user_access_allowed":true}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 3,
            request_kind: RouterRequestKind::AttachLookup {
                session_selector: SessionSelector::SessionId(SessionId::from_uuid(
                    build_fixed_test_uuid()
                )),
            },
        }),
        r#"{"request_id":3,"request_kind":{"AttachLookup":{"session_selector":{"SessionId":"00000000-0000-0000-0000-000000000001"}}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 4,
            request_kind: RouterRequestKind::ListSessions,
        }),
        r#"{"request_id":4,"request_kind":"ListSessions"}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 5,
            request_kind: RouterRequestKind::Restart,
        }),
        r#"{"request_id":5,"request_kind":"Restart"}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 6,
            request_kind: RouterRequestKind::GrantToken {
                identity: "build-box".to_string(),
                scope: TokenScope::HostWide,
                expires_in: Some(Duration::from_secs(3600)),
            },
        }),
        r#"{"request_id":6,"request_kind":{"GrantToken":{"identity":"build-box","scope":"HostWide","expires_in":{"secs":3600,"nanos":0}}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 6,
            request_kind: RouterRequestKind::GrantToken {
                identity: "build-box".to_string(),
                scope: TokenScope::Session(SessionId::from_uuid(build_fixed_test_uuid())),
                expires_in: None,
            },
        }),
        r#"{"request_id":6,"request_kind":{"GrantToken":{"identity":"build-box","scope":{"Session":"00000000-0000-0000-0000-000000000001"},"expires_in":null}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 7,
            request_kind: RouterRequestKind::RevokeToken {
                identity: "build-box".to_string(),
                scope: None,
            },
        }),
        r#"{"request_id":7,"request_kind":{"RevokeToken":{"identity":"build-box","scope":null}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 7,
            request_kind: RouterRequestKind::RevokeToken {
                identity: "build-box".to_string(),
                scope: Some(TokenScope::HostWide),
            },
        }),
        r#"{"request_id":7,"request_kind":{"RevokeToken":{"identity":"build-box","scope":"HostWide"}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 8,
            request_kind: RouterRequestKind::ListTokens { scope: None },
        }),
        r#"{"request_id":8,"request_kind":{"ListTokens":{"scope":null}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 8,
            request_kind: RouterRequestKind::ListTokens {
                scope: Some(TokenScope::Session(SessionId::from_uuid(
                    build_fixed_test_uuid()
                ))),
            },
        }),
        r#"{"request_id":8,"request_kind":{"ListTokens":{"scope":{"Session":"00000000-0000-0000-0000-000000000001"}}}}"#
    );

    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(1),
            answer_result: RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
                build_version: "0.9.9".to_string(),
            },
        }),
        r#"{"request_id":1,"answer_result":{"Hello":{"protocol_version":3,"build_version":"0.9.9"}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(2),
            answer_result: RouterResult::Created(build_test_session_address()),
        }),
        r#"{"request_id":2,"answer_result":{"Created":{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","socket_address":"/run/koshi/session.sock","process_id":4242}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(3),
            answer_result: RouterResult::Found(build_test_session_address()),
        }),
        r#"{"request_id":3,"answer_result":{"Found":{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","socket_address":"/run/koshi/session.sock","process_id":4242}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(4),
            answer_result: RouterResult::Sessions(vec![build_test_session_discovery()]),
        }),
        r#"{"request_id":4,"answer_result":{"Sessions":[{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","created_at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},"attached_client_ids":["00000000-0000-0000-0000-000000000001"],"pane_count":1}]}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(5),
            answer_result: RouterResult::Restarting,
        }),
        r#"{"request_id":5,"answer_result":"Restarting"}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(6),
            answer_result: RouterResult::Granted {
                connection_token: build_test_connection_token(),
                did_replace_active_grant: true,
            },
        }),
        r#"{"request_id":6,"answer_result":{"Granted":{"connection_token":"k7QxSecret","did_replace_active_grant":true}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(7),
            answer_result: RouterResult::Revoked(vec![
                TokenScope::HostWide,
                TokenScope::Session(SessionId::from_uuid(build_fixed_test_uuid())),
            ]),
        }),
        r#"{"request_id":7,"answer_result":{"Revoked":["HostWide",{"Session":"00000000-0000-0000-0000-000000000001"}]}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(8),
            answer_result: RouterResult::Tokens(vec![build_test_token_entry()]),
        }),
        r#"{"request_id":8,"answer_result":{"Tokens":[{"identity":"build-box","scope":"HostWide","issued_at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},"expires_at":null,"last_used_at":null,"revoked_at":null}]}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: None,
            answer_result: RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the request could not be read".to_string(),
            }),
        }),
        r#"{"request_id":null,"answer_result":{"Error":{"code":"MalformedRequest","message":"the request could not be read"}}}"#
    );

    assert_eq!(
        serialize_test_wire_message(&SessionServerReady {
            protocol_version: 3,
            socket_address: "/run/koshi/session.sock".to_string(),
        }),
        r#"{"protocol_version":3,"socket_address":"/run/koshi/session.sock"}"#
    );
}

#[test]
fn an_attach_lookup_by_name_carries_the_name() {
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 3,
            request_kind: RouterRequestKind::AttachLookup {
                session_selector: SessionSelector::SessionName("quiet-lake".to_string()),
            },
        }),
        r#"{"request_id":3,"request_kind":{"AttachLookup":{"session_selector":{"SessionName":"quiet-lake"}}}}"#
    );
}

#[test]
fn the_remote_access_requests_travel_as_bare_names() {
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 9,
            request_kind: RouterRequestKind::RemoteStatus,
        }),
        r#"{"request_id":9,"request_kind":"RemoteStatus"}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterRequest {
            request_id: 10,
            request_kind: RouterRequestKind::EnableRemote,
        }),
        r#"{"request_id":10,"request_kind":"EnableRemote"}"#
    );
}

#[test]
fn the_remote_access_answers_keep_their_wire_bytes() {
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(9),
            answer_result: RouterResult::RemoteStatus {
                remote_listen_address: Some("0.0.0.0:7654".to_string()),
                is_remote_access_enabled: true,
                is_listening: false,
                certificate_fingerprint: Some("ab".repeat(32)),
                remote_connection_count: Some(2),
            },
        }),
        format!(
            r#"{{"request_id":9,"answer_result":{{"RemoteStatus":{{"remote_listen_address":"0.0.0.0:7654","is_remote_access_enabled":true,"is_listening":false,"certificate_fingerprint":"{}","remote_connection_count":2}}}}}}"#,
            "ab".repeat(32)
        )
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(9),
            answer_result: RouterResult::RemoteStatus {
                remote_listen_address: None,
                is_remote_access_enabled: false,
                is_listening: false,
                certificate_fingerprint: None,
                remote_connection_count: None,
            },
        }),
        r#"{"request_id":9,"answer_result":{"RemoteStatus":{"remote_listen_address":null,"is_remote_access_enabled":false,"is_listening":false,"certificate_fingerprint":null,"remote_connection_count":null}}}"#
    );
    assert_eq!(
        serialize_test_wire_message(&RouterResponse {
            request_id: Some(10),
            answer_result: RouterResult::RemoteEnabled {
                remote_listen_address: "0.0.0.0:7654".to_string(),
                certificate_fingerprint: "ab".repeat(32),
            },
        }),
        format!(
            r#"{{"request_id":10,"answer_result":{{"RemoteEnabled":{{"remote_listen_address":"0.0.0.0:7654","certificate_fingerprint":"{}"}}}}}}"#,
            "ab".repeat(32)
        )
    );
}

#[test]
fn a_remote_status_without_a_connection_count_decodes_with_none() {
    let response: RouterResponse = serde_json::from_str(
        r#"{"request_id":9,"answer_result":{"RemoteStatus":{"remote_listen_address":null,"is_remote_access_enabled":false,"is_listening":false,"certificate_fingerprint":null}}}"#,
    )
    .expect("a status without a count decodes");

    assert_eq!(
        response,
        RouterResponse {
            request_id: Some(9),
            answer_result: RouterResult::RemoteStatus {
                remote_listen_address: None,
                is_remote_access_enabled: false,
                is_listening: false,
                certificate_fingerprint: None,
                remote_connection_count: None,
            },
        }
    );
}

#[test]
fn this_build_speaks_control_plane_version_three_only() {
    // Version 3 is this build's own: a session the router does not have is
    // refused with NotFound.
    assert_eq!(ROUTER_PROTOCOL_VERSION, 3);
    assert_eq!(MIN_ROUTER_PROTOCOL_VERSION, 3);
}

#[test]
fn every_request_kind_names_itself_without_its_payload() {
    assert_eq!(
        RouterRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            connection_token: build_test_connection_token(),
        }
        .get_request_kind_name(),
        "Hello"
    );
    assert_eq!(
        RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        }
        .get_request_kind_name(),
        "CreateSession"
    );
    assert_eq!(
        RouterRequestKind::AttachLookup {
            session_selector: SessionSelector::SessionName("quiet-lake".to_string()),
        }
        .get_request_kind_name(),
        "AttachLookup"
    );
    assert_eq!(
        RouterRequestKind::ListSessions.get_request_kind_name(),
        "ListSessions"
    );
    assert_eq!(
        RouterRequestKind::Restart.get_request_kind_name(),
        "Restart"
    );
    assert_eq!(
        RouterRequestKind::GrantToken {
            identity: "build-box".to_string(),
            scope: TokenScope::HostWide,
            expires_in: None,
        }
        .get_request_kind_name(),
        "GrantToken"
    );
    assert_eq!(
        RouterRequestKind::RevokeToken {
            identity: "build-box".to_string(),
            scope: None,
        }
        .get_request_kind_name(),
        "RevokeToken"
    );
    assert_eq!(
        RouterRequestKind::ListTokens { scope: None }.get_request_kind_name(),
        "ListTokens"
    );
    assert_eq!(
        RouterRequestKind::RemoteStatus.get_request_kind_name(),
        "RemoteStatus"
    );
    assert_eq!(
        RouterRequestKind::EnableRemote.get_request_kind_name(),
        "EnableRemote"
    );
}

/// Every answer this build writes names itself, and both wire lists hold one
/// entry per variant of their enum. A variant added without its `VARIANTS`
/// entry would arrive as unknown on the far side, so the two are pinned
/// together here.
#[test]
fn every_answer_names_itself_and_both_wire_lists_are_complete() {
    let kinds = [
        RouterRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            connection_token: build_test_connection_token(),
        },
        RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        },
        RouterRequestKind::AttachLookup {
            session_selector: SessionSelector::SessionName("quiet-lake".to_string()),
        },
        RouterRequestKind::ListSessions,
        RouterRequestKind::Restart,
        RouterRequestKind::GrantToken {
            identity: "build-box".to_string(),
            scope: TokenScope::HostWide,
            expires_in: None,
        },
        RouterRequestKind::RevokeToken {
            identity: "build-box".to_string(),
            scope: None,
        },
        RouterRequestKind::ListTokens { scope: None },
        RouterRequestKind::RemoteStatus,
        RouterRequestKind::EnableRemote,
    ];
    let results = [
        (
            RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
                build_version: "0.9.9".to_string(),
            },
            "Hello",
        ),
        (
            RouterResult::Created(build_test_session_address()),
            "Created",
        ),
        (RouterResult::Found(build_test_session_address()), "Found"),
        (
            RouterResult::Sessions(vec![build_test_session_discovery()]),
            "Sessions",
        ),
        (RouterResult::Restarting, "Restarting"),
        (
            RouterResult::Granted {
                connection_token: build_test_connection_token(),
                did_replace_active_grant: true,
            },
            "Granted",
        ),
        (RouterResult::Revoked(vec![TokenScope::HostWide]), "Revoked"),
        (
            RouterResult::Tokens(vec![build_test_token_entry()]),
            "Tokens",
        ),
        (
            RouterResult::RemoteStatus {
                remote_listen_address: Some("0.0.0.0:7654".to_string()),
                is_remote_access_enabled: true,
                is_listening: false,
                certificate_fingerprint: Some("ab".repeat(32)),
                remote_connection_count: Some(2),
            },
            "RemoteStatus",
        ),
        (
            RouterResult::RemoteEnabled {
                remote_listen_address: "0.0.0.0:7654".to_string(),
                certificate_fingerprint: "ab".repeat(32),
            },
            "RemoteEnabled",
        ),
        (
            RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the request could not be read".to_string(),
            }),
            "Error",
        ),
    ];

    for request_kind in &kinds {
        assert_eq!(
            request_kind.wire_name(),
            request_kind.get_request_kind_name()
        );
    }
    let request_kind_names: Vec<&str> = kinds.iter().map(RouterRequestKind::wire_name).collect();
    assert_eq!(request_kind_names, RouterRequestKind::VARIANTS);

    for (router_result, expected_wire_name) in &results {
        assert_eq!(router_result.wire_name(), *expected_wire_name);
    }
    let router_result_names: Vec<&str> = results
        .iter()
        .map(|(router_result, _)| router_result.wire_name())
        .collect();
    assert_eq!(router_result_names, RouterResult::VARIANTS);
}

#[test]
fn a_request_kind_this_build_lacks_reads_as_unknown_carrying_its_name() {
    let decoded: RouterRequest<MaybeKnown<RouterRequestKind>> = serde_json::from_str(
        r#"{"request_id":9,"request_kind":{"RehomeToken":{"identity":"build-box"}}}"#,
    )
    .expect("a kind this build does not have still reads");

    assert_eq!(
        decoded,
        RouterRequest {
            request_id: 9,
            request_kind: MaybeKnown::Unknown {
                variant_name: "RehomeToken".to_string(),
            },
        }
    );
}

#[test]
fn an_unknown_kind_sent_as_a_bare_name_reads_as_unknown_carrying_its_name() {
    let decoded: RouterRequest<MaybeKnown<RouterRequestKind>> =
        serde_json::from_str(r#"{"request_id":9,"request_kind":"RehomeToken"}"#)
            .expect("a kind this build does not have still reads");

    assert_eq!(
        decoded,
        RouterRequest {
            request_id: 9,
            request_kind: MaybeKnown::Unknown {
                variant_name: "RehomeToken".to_string(),
            },
        }
    );
}

#[test]
fn a_kind_this_build_has_with_a_payload_it_cannot_read_keeps_the_decoding_error() {
    let decoded: Result<RouterRequest<MaybeKnown<RouterRequestKind>>, _> = serde_json::from_str(
        r#"{"request_id":3,"request_kind":{"AttachLookup":{"session_selector":7}}}"#,
    );

    assert_eq!(
        decoded
            .expect_err("a selector that names no session is not this version's shape")
            .to_string(),
        "expected value at line 1 column 37"
    );
}

#[test]
fn an_answer_this_build_does_not_have_reads_as_unknown_carrying_its_name() {
    let decoded: IncomingRouterResponse = serde_json::from_str(
        r#"{"request_id":9,"answer_result":{"Rehomed":{"identity":"build-box"}}}"#,
    )
    .expect("an answer this build does not have still reads");

    assert_eq!(
        decoded,
        RouterResponse {
            request_id: Some(9),
            answer_result: MaybeKnown::Unknown {
                variant_name: "Rehomed".to_string(),
            },
        }
    );
}

#[test]
fn a_request_missing_its_id_is_refused() {
    let decoded: Result<RouterRequest, _> =
        serde_json::from_str(r#"{"request_kind":"ListSessions"}"#);

    assert_eq!(
        decoded
            .expect_err("a request without an id is not this version's shape")
            .to_string(),
        "missing field `request_id` at line 1 column 31"
    );
}

#[test]
fn a_request_kind_carrying_a_field_this_build_does_not_know_still_reads() {
    let decoded: RouterRequest = serde_json::from_str(
        r#"{"request_id":2,"request_kind":{"CreateSession":{"profile":null,"working_directory":null,"is_other_user_access_allowed":null,"tab_count":3}}}"#,
    )
    .expect("a field this build lacks is passed over");

    assert_eq!(
        decoded,
        RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::CreateSession {
                profile: None,
                working_directory: None,
                is_other_user_access_allowed: None,
            },
        }
    );
}

#[test]
fn a_session_address_carrying_a_field_this_build_does_not_know_still_reads() {
    let decoded: SessionAddress = serde_json::from_str(
        r#"{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","socket_address":"/run/koshi/session.sock","process_id":4242,"uptime":7}"#,
    )
    .expect("a field this build lacks is passed over");

    assert_eq!(decoded, build_test_session_address());
}

#[test]
fn a_ready_line_carrying_a_field_this_build_does_not_know_still_reads() {
    let decoded: SessionServerReady = serde_json::from_str(
        r#"{"protocol_version":3,"socket_address":"/run/koshi/session.sock","process_id":4242}"#,
    )
    .expect("a field this build lacks is passed over");

    assert_eq!(
        decoded,
        SessionServerReady {
            protocol_version: 3,
            socket_address: "/run/koshi/session.sock".to_string(),
        }
    );
}

#[test]
fn printing_a_granted_answer_reveals_no_secret() {
    let printed = format!(
        "{:?}",
        RouterResult::Granted {
            connection_token: build_test_connection_token(),
            did_replace_active_grant: false,
        }
    );

    assert_eq!(
        printed,
        "Granted { connection_token: ConnectionToken(***), did_replace_active_grant: false }"
    );
}

#[test]
fn a_request_carrying_an_unknown_field_is_refused() {
    let decoded: Result<RouterRequest, _> =
        serde_json::from_str(r#"{"request_id":1,"request_kind":"ListSessions","junk":5}"#);

    assert_eq!(
        decoded
            .expect_err("an unknown field is not this version's shape")
            .to_string(),
        "unknown field `junk`, expected `request_id` or `request_kind` at line 1 column 52"
    );
}

#[test]
fn a_create_session_carrying_the_other_users_answer_decodes() {
    let decoded: RouterRequest = serde_json::from_str(
        r#"{"request_id":2,"request_kind":{"CreateSession":{"profile":null,"working_directory":null,"is_other_user_access_allowed":true}}}"#,
    )
    .expect("a create naming the other-users answer is this version's shape");

    assert_eq!(
        decoded,
        RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::CreateSession {
                profile: None,
                working_directory: None,
                is_other_user_access_allowed: Some(true),
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
        r#"{"request_id":2,"request_kind":{"CreateSession":{"profile":null,"working_directory":null}}}"#,
    )
    .expect("a create naming no other-users answer still reads");

    assert_eq!(
        decoded,
        RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::CreateSession {
                profile: None,
                working_directory: None,
                is_other_user_access_allowed: None,
            },
        }
    );
}

// A Hello from a build that predates the version field still decodes; the
// absent field reads as an empty string.
#[test]
fn a_hello_without_a_version_field_decodes_with_an_empty_version() {
    let response: RouterResponse = serde_json::from_str(
        r#"{"request_id":1,"answer_result":{"Hello":{"protocol_version":3}}}"#,
    )
    .expect("a version-less Hello decodes");
    assert_eq!(
        response,
        RouterResponse {
            request_id: Some(1),
            answer_result: RouterResult::Hello {
                protocol_version: 3,
                build_version: String::new(),
            },
        }
    );
}

#[test]
fn a_restart_and_its_answer_read_back_from_their_wire_text() {
    let request: RouterRequest =
        serde_json::from_str(r#"{"request_id":5,"request_kind":"Restart"}"#)
            .expect("a restart request is this version's shape");
    let response: RouterResponse =
        serde_json::from_str(r#"{"request_id":5,"answer_result":"Restarting"}"#)
            .expect("a restarting answer is this version's shape");

    assert_eq!(
        request,
        RouterRequest {
            request_id: 5,
            request_kind: RouterRequestKind::Restart,
        }
    );
    assert_eq!(
        response,
        RouterResponse {
            request_id: Some(5),
            answer_result: RouterResult::Restarting,
        }
    );
}

#[test]
fn a_session_address_missing_its_pid_is_refused() {
    // What a build that advertised no process id looks like here. Decoding
    // must fail rather than fill in a default, so the mismatch surfaces
    // instead of producing a row that names process 0.
    let decoded: Result<SessionAddress, _> = serde_json::from_str(
        r#"{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","socket_address":"/run/koshi/session.sock"}"#,
    );

    assert_eq!(
        decoded
            .expect_err("an address without a pid is not this version's shape")
            .to_string(),
        "missing field `process_id` at line 1 column 124"
    );
}

#[test]
fn a_hello_with_the_right_version_and_token_is_accepted() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
        }),
        Ok(())
    );
    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        Some(ROUTER_PROTOCOL_VERSION)
    );
}

#[test]
fn a_hello_built_here_names_this_builds_range() {
    assert_eq!(
        RouterRequestKind::build_hello_request(build_test_connection_token()),
        RouterRequestKind::Hello {
            min_protocol_version: 3,
            max_protocol_version: 3,
            connection_token: build_test_connection_token(),
        }
    );
}

#[test]
fn an_accepted_hello_opens_the_gate_for_other_requests() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    router_handshake
        .validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
        })
        .expect("the Hello is accepted");

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        }),
        Ok(())
    );
    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::ListSessions),
        Ok(())
    );
}

#[test]
fn a_caller_speaking_only_above_this_router_is_refused_naming_both_ranges() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());
    let above = ROUTER_PROTOCOL_VERSION + 1;

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            connection_token: build_test_connection_token(),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the caller speaks control-plane protocol versions {above} to {above}, \
                 this router speaks {MIN_ROUTER_PROTOCOL_VERSION} to {ROUTER_PROTOCOL_VERSION}"
            ),
        })
    );
    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        None,
        "a refused Hello settles nothing"
    );
}

#[test]
fn a_caller_reaching_above_this_router_settles_on_the_routers_highest() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    router_handshake
        .validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION + 3,
            connection_token: build_test_connection_token(),
        })
        .expect("a range covering this router's is accepted");

    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        Some(ROUTER_PROTOCOL_VERSION)
    );
}

#[test]
fn an_unknown_kind_is_refused_by_name_once_the_gate_is_open() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        router_handshake.build_unknown_request_kind_error("Rehome"),
        IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Rehome arrived before a Hello opened the connection".to_string(),
        }
    );

    router_handshake
        .validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
        })
        .expect("the Hello is accepted");

    assert_eq!(
        router_handshake.build_unknown_request_kind_error("Rehome"),
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this router has no request kind named Rehome".to_string(),
        }
    );
}

#[test]
fn an_out_of_range_hello_with_a_wrong_token_is_refused_for_the_version() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());
    let above = ROUTER_PROTOCOL_VERSION + 1;

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: above,
            max_protocol_version: above,
            connection_token: ConnectionToken::from_secret("wrongToken"),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the caller speaks control-plane protocol versions {above} to {above}, \
                 this router speaks {MIN_ROUTER_PROTOCOL_VERSION} to {ROUTER_PROTOCOL_VERSION}"
            ),
        })
    );
    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        None,
        "a refused Hello settles nothing"
    );
}

#[test]
fn a_hello_with_a_wrong_token_is_refused_as_bad_token() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret("wrongToken"),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match the router's".to_string(),
        })
    );
    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        None,
        "a refused Hello settles nothing"
    );
}

#[test]
fn a_caller_speaking_only_below_this_router_is_refused_naming_both_ranges() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());
    let below = MIN_ROUTER_PROTOCOL_VERSION - 1;

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: below,
            max_protocol_version: below,
            connection_token: build_test_connection_token(),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the caller speaks control-plane protocol versions {below} to {below}, \
                 this router speaks {MIN_ROUTER_PROTOCOL_VERSION} to {ROUTER_PROTOCOL_VERSION}"
            ),
        })
    );
    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        None,
        "a refused Hello settles nothing"
    );
}

#[test]
fn a_caller_speaking_only_the_floor_settles_on_the_floor() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
        }),
        Ok(())
    );
    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        Some(MIN_ROUTER_PROTOCOL_VERSION)
    );
}

#[test]
fn a_second_hello_with_a_narrower_range_settles_the_version_again_from_that_range() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());
    router_handshake
        .validate_request_kind(&RouterRequestKind::build_hello_request(
            build_test_connection_token(),
        ))
        .expect("the first Hello is accepted");
    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        Some(ROUTER_PROTOCOL_VERSION)
    );

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
        }),
        Ok(())
    );
    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        Some(MIN_ROUTER_PROTOCOL_VERSION)
    );
    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::ListSessions),
        Ok(())
    );
}

#[test]
fn every_other_kind_is_refused_by_name_before_a_hello_and_served_after_one() {
    let kinds = [
        (
            RouterRequestKind::CreateSession {
                profile: None,
                working_directory: None,
                is_other_user_access_allowed: None,
            },
            "CreateSession",
        ),
        (
            RouterRequestKind::AttachLookup {
                session_selector: SessionSelector::SessionName("quiet-lake".to_string()),
            },
            "AttachLookup",
        ),
        (RouterRequestKind::ListSessions, "ListSessions"),
        (RouterRequestKind::Restart, "Restart"),
        (
            RouterRequestKind::GrantToken {
                identity: "build-box".to_string(),
                scope: TokenScope::HostWide,
                expires_in: None,
            },
            "GrantToken",
        ),
        (
            RouterRequestKind::RevokeToken {
                identity: "build-box".to_string(),
                scope: None,
            },
            "RevokeToken",
        ),
        (RouterRequestKind::ListTokens { scope: None }, "ListTokens"),
        (RouterRequestKind::RemoteStatus, "RemoteStatus"),
        (RouterRequestKind::EnableRemote, "EnableRemote"),
    ];
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    for (request_kind, request_kind_name) in &kinds {
        assert_eq!(
            router_handshake.validate_request_kind(request_kind),
            Err(IpcErrorPayload {
                code: IpcErrorCode::HelloRequired,
                message: format!(
                    "{request_kind_name} arrived before a Hello opened the connection"
                ),
            })
        );
    }
    assert_eq!(
        router_handshake.get_agreed_protocol_version(),
        None,
        "a refused request kind opens nothing"
    );

    router_handshake
        .validate_request_kind(&RouterRequestKind::build_hello_request(
            build_test_connection_token(),
        ))
        .expect("the Hello is accepted");

    for (request_kind, request_kind_name) in &kinds {
        assert_eq!(
            router_handshake.validate_request_kind(request_kind),
            Ok(()),
            "{request_kind_name} is served on an open connection"
        );
    }
}

#[test]
fn a_request_before_any_hello_is_refused_as_hello_required() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "CreateSession arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_hello_required_refusal_names_the_kind_without_its_payload() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::AttachLookup {
            session_selector: SessionSelector::SessionName("quiet-lake".to_string()),
        }),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "AttachLookup arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_restart_is_refused_before_a_hello_and_served_after_one() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::Restart),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Restart arrived before a Hello opened the connection".to_string(),
        })
    );

    router_handshake
        .validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
        })
        .expect("the Hello is accepted");

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::Restart),
        Ok(())
    );
}

#[test]
fn a_refused_hello_leaves_the_gate_closed() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    router_handshake
        .validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret("wrongToken"),
        })
        .expect_err("the Hello is refused");

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::ListSessions),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "ListSessions arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_good_hello_after_a_refusal_opens_the_gate() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    router_handshake
        .validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret("wrongToken"),
        })
        .expect_err("the Hello is refused");
    router_handshake
        .validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
        })
        .expect("the Hello is accepted");

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        }),
        Ok(())
    );
}

#[test]
fn a_refused_hello_on_an_open_gate_leaves_it_open() {
    let mut router_handshake =
        RouterHandshake::from_connection_token(build_test_connection_token());

    router_handshake
        .validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
        })
        .expect("the Hello is accepted");
    router_handshake
        .validate_request_kind(&RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret("wrongToken"),
        })
        .expect_err("the Hello is refused");

    assert_eq!(
        router_handshake.validate_request_kind(&RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        }),
        Ok(())
    );
}

#[test]
#[cfg(unix)]
fn the_router_socket_sits_directly_inside_the_runtime_directory() {
    assert_eq!(
        compute_router_socket_address(Path::new("/run/user/1000/koshi")),
        "/run/user/1000/koshi/router.sock"
    );
}

#[test]
#[cfg(windows)]
fn each_runtime_directory_gets_its_own_router_pipe_in_the_koshi_namespace() {
    const PREFIX: &str = "koshi-router-";

    let pipe = compute_router_socket_address(Path::new(r"C:\Users\u\AppData\Local\koshi"));
    let pipe_again = compute_router_socket_address(Path::new(r"C:\Users\u\AppData\Local\koshi"));
    let other_pipe =
        compute_router_socket_address(Path::new(r"C:\Users\u\AppData\Local\koshi-test"));

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
    let runtime_directory = tempfile::tempdir().expect("a temporary directory is created");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            runtime_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("the runtime directory mode is set");
    }

    crate::validate::validate_socket_address(
        &compute_router_socket_address(runtime_directory.path()),
        runtime_directory.path(),
    )
    .expect("the router socket sits where the trust check accepts it");
}

#[test]
fn the_router_endpoint_and_lock_files_sit_beside_the_socket() {
    let runtime_directory = Path::new("/run/user/1000/koshi");

    assert_eq!(
        resolve_router_endpoint_path(runtime_directory),
        PathBuf::from("/run/user/1000/koshi/router.json")
    );
    assert_eq!(
        resolve_router_lock_path(runtime_directory),
        PathBuf::from("/run/user/1000/koshi/router.lock")
    );
}
