//! Tests for the parts of an exchange both peers do the same way: the version
//! check at the Hello, unwrapping an answer that may name a result this build
//! does not have, and the two failures that read the same for either peer.
//!
//! Each peer's own wording is pinned here word for word.

use super::*;

use koshi_core::compat::{CONTROL_PROTOCOL, SESSION_PROTOCOL};
use koshi_core::event::RejectReason;
use koshi_ipc::protocol::{IpcErrorCode, IpcResult};

/// The sentence a failure carries, for asserting on it exactly. Panics on any
/// [`CliError`] variant other than [`CliError::IpcUnavailable`].
fn extract_ipc_unavailable_detail(cli_error: CliError) -> String {
    match cli_error {
        CliError::IpcUnavailable { detail } => detail,
        unexpected_error => panic!("expected IpcUnavailable, got {unexpected_error:?}"),
    }
}

/// The sentence a refusal of the connection token carries, for asserting on it
/// exactly. Panics on any [`CliError`] variant other than
/// [`CliError::ConnectionTokenRefused`].
fn extract_connection_token_refused_detail(cli_error: CliError) -> String {
    match cli_error {
        CliError::ConnectionTokenRefused { detail } => detail,
        unexpected_error => panic!("expected ConnectionTokenRefused, got {unexpected_error:?}"),
    }
}

/// The refusal a session gives for `settled_protocol_version`, naming the
/// range of [`SESSION_PROTOCOL`].
fn format_session_version_refusal(settled_protocol_version: u32) -> String {
    format!(
        "the session settled on protocol version {settled_protocol_version}, which is outside \
         the {} to {} this koshi asked for",
        SESSION_PROTOCOL.minimum_version, SESSION_PROTOCOL.maximum_version
    )
}

/// The refusal a router gives for `settled_protocol_version`, naming the
/// range of [`CONTROL_PROTOCOL`].
fn format_router_version_refusal(settled_protocol_version: u32) -> String {
    format!(
        "the router settled on control-plane protocol version {settled_protocol_version}, which \
         is outside the {} to {} this koshi asked for",
        CONTROL_PROTOCOL.minimum_version, CONTROL_PROTOCOL.maximum_version
    )
}

#[test]
fn a_version_inside_the_range_this_build_sent_is_accepted() {
    for settled_protocol_version in [
        SESSION_PROTOCOL.minimum_version,
        SESSION_PROTOCOL.maximum_version,
    ] {
        SESSION_PEER_WORDS
            .validate_settled_protocol_version(settled_protocol_version)
            .expect("a session version inside the range opens");
    }
    for settled_protocol_version in [
        CONTROL_PROTOCOL.minimum_version,
        CONTROL_PROTOCOL.maximum_version,
    ] {
        ROUTER_PEER_WORDS
            .validate_settled_protocol_version(settled_protocol_version)
            .expect("a router version inside the range opens");
    }
}

#[test]
fn a_session_version_above_the_range_names_both_the_version_and_the_range() {
    let settled_protocol_version = SESSION_PROTOCOL.maximum_version + 1;

    let refusal = SESSION_PEER_WORDS
        .validate_settled_protocol_version(settled_protocol_version)
        .expect_err("a version above the range is refused");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        format_session_version_refusal(settled_protocol_version)
    );
}

#[test]
fn a_router_version_above_the_range_names_the_control_plane_in_its_own_words() {
    let settled_protocol_version = CONTROL_PROTOCOL.maximum_version + 1;

    let refusal = ROUTER_PEER_WORDS
        .validate_settled_protocol_version(settled_protocol_version)
        .expect_err("a version above the range is refused");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        format_router_version_refusal(settled_protocol_version)
    );
}

#[test]
fn a_version_below_the_floor_is_refused_the_same_way() {
    let settled_protocol_version = SESSION_PROTOCOL.minimum_version - 1;

    let refusal = SESSION_PEER_WORDS
        .validate_settled_protocol_version(settled_protocol_version)
        .expect_err("a version below the floor is refused");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        format_session_version_refusal(settled_protocol_version)
    );
}

#[test]
fn a_router_version_below_the_floor_names_the_control_plane_range() {
    let refusal = ROUTER_PEER_WORDS
        .validate_settled_protocol_version(0)
        .expect_err("0 is below the router floor");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        format_router_version_refusal(0)
    );
}

#[test]
fn the_largest_version_a_peer_can_name_is_outside_the_range() {
    let refusal = SESSION_PEER_WORDS
        .validate_settled_protocol_version(u32::MAX)
        .expect_err("4294967295 is above the range");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        format_session_version_refusal(u32::MAX)
    );
}

#[test]
fn a_known_result_comes_back_as_itself() {
    let incoming_response: Answer<MaybeKnown<IpcResult>> = Answer {
        request_id: Some(7),
        answer_result: MaybeKnown::Known(IpcResult::Restarting),
    };

    assert_eq!(
        SESSION_PEER_WORDS
            .take_response_result(incoming_response)
            .expect("a known result"),
        IpcResult::Restarting
    );
}

#[test]
fn an_answer_that_names_no_request_still_hands_back_its_result() {
    let incoming_response: Answer<MaybeKnown<IpcResult>> = Answer {
        request_id: None,
        answer_result: MaybeKnown::Known(IpcResult::Restarting),
    };

    assert_eq!(
        SESSION_PEER_WORDS
            .take_response_result(incoming_response)
            .expect("a known result"),
        IpcResult::Restarting
    );
}

#[test]
fn a_result_this_build_does_not_have_fails_naming_what_arrived() {
    let incoming_response: Answer<MaybeKnown<IpcResult>> = Answer {
        request_id: Some(7),
        answer_result: MaybeKnown::Unknown {
            variant_name: "Rehomed".to_string(),
        },
    };

    let refusal = SESSION_PEER_WORDS
        .take_response_result(incoming_response)
        .expect_err("a result this build has no variant for");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        "the session answered with an unexpected Rehomed reply"
    );
}

#[test]
fn an_unexpected_reply_is_named_by_its_wire_name() {
    assert_eq!(
        extract_ipc_unavailable_detail(
            SESSION_PEER_WORDS.build_unexpected_reply_error(&IpcResult::Restarting)
        ),
        "the session answered with an unexpected Restarting reply"
    );
}

#[test]
fn a_reply_carrying_a_payload_is_named_by_its_variant_not_its_contents() {
    use koshi_core::ids::SessionId;
    use koshi_ipc::layout::SessionLayout;

    let layout = IpcResult::Layout(SessionLayout {
        session_id: SessionId::new(),
        session_name: "workspace".to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    });

    assert_eq!(
        extract_ipc_unavailable_detail(SESSION_PEER_WORDS.build_unexpected_reply_error(&layout)),
        "the session answered with an unexpected Layout reply"
    );
}

#[test]
fn each_peer_names_itself_in_the_unexpected_reply() {
    assert_eq!(
        extract_ipc_unavailable_detail(
            ROUTER_PEER_WORDS.build_unexpected_wire_name_error("Created")
        ),
        "the router answered with an unexpected Created reply"
    );
}

#[test]
fn a_transport_fault_carries_the_faults_own_words() {
    let fault = IpcError::NoListener {
        socket_address: "/nowhere.sock".to_string(),
    };
    let fault_detail = fault.to_string();

    assert_eq!(
        extract_ipc_unavailable_detail(build_ipc_unavailable_error(fault)),
        fault_detail
    );
}

#[test]
fn an_answer_from_a_server_of_koshi_0_4_0_or_older_names_the_step_that_moves_it() {
    let cli_error = build_ipc_unavailable_error(IpcError::PreviousReleaseAnswer);

    let CliError::PreviousReleaseServer { detail } = cli_error else {
        panic!("expected PreviousReleaseServer, got {cli_error:?}");
    };
    assert_eq!(
        detail,
        "the server answered in the format of koshi 0.4.0 or older, which this koshi cannot \
         talk to"
    );
}

#[test]
fn a_protocol_refusal_carries_the_sentence_the_peer_sent() {
    let refusal = IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token presented does not match this Koshi's".to_string(),
    };

    assert_eq!(
        extract_ipc_unavailable_detail(build_peer_refusal_error(&refusal)),
        "the token presented does not match this Koshi's"
    );
}

#[test]
fn a_hello_refused_for_its_token_is_a_connection_token_refusal() {
    let refusal = IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token presented does not match\u{1b}[2J the router's".to_string(),
    };

    assert_eq!(
        extract_connection_token_refused_detail(build_hello_refusal_error(&refusal)),
        "the token presented does not match[2J the router's"
    );
}

#[test]
fn a_hello_refused_for_its_version_is_a_protocol_version_refusal() {
    let refusal = IpcErrorPayload {
        code: IpcErrorCode::UnsupportedVersion,
        message: "this session speaks protocol versions 2..2; the caller speaks 3..3".to_string(),
    };

    let hello_refusal_error = build_hello_refusal_error(&refusal);

    let CliError::ProtocolVersionRefused { detail } = hello_refusal_error else {
        panic!("expected ProtocolVersionRefused, got {hello_refusal_error:?}");
    };
    assert_eq!(
        detail,
        "this session speaks protocol versions 2..2; the caller speaks 3..3"
    );
}

#[test]
fn a_hello_refused_for_any_other_reason_is_an_unavailable_peer() {
    let refusal = IpcErrorPayload {
        code: IpcErrorCode::MalformedRequest,
        message: "the bytes received are not a request".to_string(),
    };

    assert_eq!(
        extract_ipc_unavailable_detail(build_hello_refusal_error(&refusal)),
        "the bytes received are not a request"
    );
}

#[test]
fn a_version_refusal_is_protocol_version_refused_with_the_sentence_the_peer_sent() {
    let refusal = IpcErrorPayload {
        code: IpcErrorCode::UnsupportedVersion,
        message: "this session speaks protocol versions 2..2; the caller speaks 3..3".to_string(),
    };

    let peer_refusal_error = build_peer_refusal_error(&refusal);

    let CliError::ProtocolVersionRefused { detail } = peer_refusal_error else {
        panic!("expected ProtocolVersionRefused, got {peer_refusal_error:?}");
    };
    assert_eq!(
        detail,
        "this session speaks protocol versions 2..2; the caller speaks 3..3"
    );
}

#[test]
fn peer_text_reaches_the_message_filtered() {
    assert_eq!(
        extract_ipc_unavailable_detail(
            SESSION_PEER_WORDS.build_unexpected_wire_name_error("\u{1b}[2J\u{1b}[HRe\u{202e}homed")
        ),
        "the session answered with an unexpected [2J[HRehomed reply"
    );

    let refusal = IpcErrorPayload {
        code: IpcErrorCode::Unknown,
        message: "\u{1b}]0;pwned\u{7}refused".to_string(),
    };
    assert_eq!(
        extract_ipc_unavailable_detail(build_peer_refusal_error(&refusal)),
        "]0;pwnedrefused"
    );

    let long_error_payload = IpcErrorPayload {
        code: IpcErrorCode::Unknown,
        message: "a".repeat(100_000),
    };
    assert_eq!(
        extract_ipc_unavailable_detail(build_peer_refusal_error(&long_error_payload)).len(),
        koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT
    );
}

#[test]
fn each_peer_reads_its_range_from_the_versioned_surface_table() {
    assert_eq!(
        SESSION_PEER_WORDS.protocol_surface,
        koshi_core::compat::SESSION_PROTOCOL
    );
    assert_eq!(
        ROUTER_PEER_WORDS.protocol_surface,
        koshi_core::compat::CONTROL_PROTOCOL
    );
}

// --- Reading a Hello answer -------------------------------------------------

/// A session's answer carrying `session_result`, as the wire hands it to a caller.
fn build_session_response(session_result: IpcResult) -> IncomingResponse {
    Answer {
        request_id: Some(1),
        answer_result: MaybeKnown::Known(session_result),
    }
}

/// The router's answer carrying `router_result`, as the wire hands it to a caller.
fn build_router_response(router_result: RouterResult) -> IncomingRouterResponse {
    Answer {
        request_id: Some(1),
        answer_result: MaybeKnown::Known(router_result),
    }
}

#[test]
fn a_session_hello_hands_back_the_build_the_session_named() {
    let incoming_response = build_session_response(IpcResult::Hello {
        protocol_version: SESSION_PROTOCOL.maximum_version,
        build_version: "0.9.9".to_string(),
    });

    assert_eq!(
        parse_session_hello_version(incoming_response).expect("a version inside the range opens"),
        (SESSION_PROTOCOL.maximum_version, "0.9.9".to_string())
    );
}

#[test]
fn a_session_hello_with_an_empty_build_version_hands_back_an_empty_string() {
    let incoming_response = build_session_response(IpcResult::Hello {
        protocol_version: SESSION_PROTOCOL.maximum_version,
        build_version: String::new(),
    });

    assert_eq!(
        parse_session_hello_version(incoming_response).expect("an empty build version still opens"),
        (SESSION_PROTOCOL.maximum_version, String::new())
    );
}

#[test]
fn a_session_hello_naming_a_version_outside_the_range_stops_the_exchange() {
    let incoming_response = build_session_response(IpcResult::Hello {
        protocol_version: SESSION_PROTOCOL.maximum_version + 1,
        build_version: "0.9.9".to_string(),
    });

    let refusal = parse_session_hello_version(incoming_response)
        .expect_err("a version above the range is refused");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        format_session_version_refusal(SESSION_PROTOCOL.maximum_version + 1)
    );
}

#[test]
fn a_session_refusing_the_hello_stops_the_exchange_with_its_own_sentence() {
    let incoming_response = build_session_response(IpcResult::Error(IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token presented does not match this Koshi's".to_string(),
    }));

    let refusal =
        parse_session_hello_version(incoming_response).expect_err("a refused Hello opens nothing");

    assert_eq!(
        extract_connection_token_refused_detail(refusal),
        "the token presented does not match this Koshi's"
    );
}

#[test]
fn a_session_answering_no_hello_at_all_names_the_reply_that_arrived() {
    let incoming_response = build_session_response(IpcResult::Restarting);

    let refusal =
        parse_session_hello_version(incoming_response).expect_err("a Restarting is not a Hello");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        "the session answered with an unexpected Restarting reply"
    );
}

#[test]
fn a_hello_answer_this_build_cannot_name_stops_the_exchange() {
    let incoming_response: IncomingResponse = Answer {
        request_id: Some(1),
        answer_result: MaybeKnown::Unknown {
            variant_name: "Rehomed".to_string(),
        },
    };

    let refusal = parse_session_hello_version(incoming_response)
        .expect_err("this build has no Rehomed variant");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        "the session answered with an unexpected Rehomed reply"
    );
}

#[test]
fn a_router_hello_hands_back_the_build_the_router_named() {
    let incoming_response = build_router_response(RouterResult::Hello {
        protocol_version: CONTROL_PROTOCOL.maximum_version,
        build_version: "0.9.9".to_string(),
    });

    assert_eq!(
        parse_router_hello_version(incoming_response).expect("a version inside the range opens"),
        "0.9.9"
    );
}

#[test]
fn a_router_hello_build_loses_its_control_characters() {
    let incoming_response = build_router_response(RouterResult::Hello {
        protocol_version: CONTROL_PROTOCOL.maximum_version,
        build_version: "0.9.9\u{1b}]0;title\u{7}".to_string(),
    });

    assert_eq!(
        parse_router_hello_version(incoming_response).expect("a version inside the range opens"),
        "0.9.9]0;title"
    );
}

#[test]
fn a_router_hello_naming_a_version_outside_the_range_stops_the_exchange() {
    let incoming_response = build_router_response(RouterResult::Hello {
        protocol_version: CONTROL_PROTOCOL.maximum_version + 1,
        build_version: "0.9.9".to_string(),
    });

    let refusal = parse_router_hello_version(incoming_response)
        .expect_err("a version above the range is refused");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        format_router_version_refusal(CONTROL_PROTOCOL.maximum_version + 1)
    );
}

#[test]
fn a_router_refusing_the_hello_stops_the_exchange_with_its_own_sentence() {
    let incoming_response = build_router_response(RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token presented does not match the router's".to_string(),
    }));

    let refusal =
        parse_router_hello_version(incoming_response).expect_err("a refused Hello opens nothing");

    assert_eq!(
        extract_connection_token_refused_detail(refusal),
        "the token presented does not match the router's"
    );
}

#[test]
fn a_router_answering_no_hello_at_all_names_the_reply_that_arrived() {
    let incoming_response = build_router_response(RouterResult::Restarting);

    let refusal =
        parse_router_hello_version(incoming_response).expect_err("a Restarting is not a Hello");

    assert_eq!(
        extract_ipc_unavailable_detail(refusal),
        "the router answered with an unexpected Restarting reply"
    );
}

#[test]
fn a_transport_failure_carrying_peer_bytes_is_filtered() {
    // `MalformedFrame` carries the decoder's message, which quotes the name
    // the peer sent.
    let hostile_wire_text = format!("unknown variant `{}Rehomed`", "\u{1b}[2J");

    assert_eq!(
        extract_ipc_unavailable_detail(build_ipc_unavailable_error(IpcError::MalformedFrame {
            error_detail: hostile_wire_text.clone(),
        })),
        "ipc frame is not a readable message: unknown variant `[2JRehomed`"
    );
    assert!(
        !extract_ipc_unavailable_detail(build_ipc_unavailable_error(IpcError::MalformedFrame {
            error_detail: hostile_wire_text
        }))
        .contains('\u{1b}'),
        "no escape byte reaches the sentence"
    );
}

#[test]
fn a_rejections_hint_is_filtered_and_an_applied_result_is_left_alone() {
    let command_id = koshi_core::ids::CommandId::new();
    let filtered_rejection_result = filter_rejection_hint(CommandResult::Rejected {
        command_id,
        reason: RejectReason::Unauthorized,
        help: Some("\u{1b}[2Jattach\u{7f} first".to_string()),
    });

    assert_eq!(
        filtered_rejection_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("[2Jattach first".to_string()),
        }
    );

    let rejection_without_hint = CommandResult::Rejected {
        command_id,
        reason: RejectReason::Unauthorized,
        help: None,
    };
    assert_eq!(
        filter_rejection_hint(rejection_without_hint.clone()),
        rejection_without_hint,
    );

    let accepted_command_result = CommandResult::Ok {
        command_id,
        emitted_events: Vec::new(),
    };
    assert_eq!(
        filter_rejection_hint(accepted_command_result.clone()),
        accepted_command_result,
    );
}

#[test]
fn a_session_hello_filters_the_build_it_named() {
    // The control characters of the build version are dropped.
    let incoming_response = build_session_response(IpcResult::Hello {
        protocol_version: SESSION_PROTOCOL.maximum_version,
        build_version: "\u{1b}]0;pwned\u{7}0.9.9".to_string(),
    });

    assert_eq!(
        parse_session_hello_version(incoming_response).expect("a version inside the range opens"),
        (
            SESSION_PROTOCOL.maximum_version,
            "]0;pwned0.9.9".to_string()
        )
    );
}
