//! Tests for the parts of an exchange both peers do the same way: the version
//! check at the Hello, unwrapping an answer that may name a result this build
//! does not have, and the two failures that read the same for either peer.
//!
//! Each peer's own wording is pinned here word for word.

use super::*;

use koshi_core::command::CliExitCode;
use koshi_core::event::RejectReason;
use koshi_ipc::protocol::{IpcErrorCode, IpcResult};

/// The sentence a failure carries, for asserting on it exactly. Panics on any
/// [`CliError`] variant other than [`CliError::IpcUnavailable`].
fn detail(error: CliError) -> String {
    match error {
        CliError::IpcUnavailable { detail } => detail,
        other => panic!("expected IpcUnavailable, got {other:?}"),
    }
}

#[test]
fn a_version_inside_the_range_this_build_sent_is_accepted() {
    SESSION
        .settled_version(3)
        .expect("3 is the only session version");
    ROUTER.settled_version(1).expect("1 is the router floor");
    ROUTER.settled_version(2).expect("2 is the router ceiling");
}

#[test]
fn a_session_version_above_the_range_names_both_the_version_and_the_range() {
    let refusal = SESSION
        .settled_version(4)
        .expect_err("4 is outside the 3 to 3 this build speaks");

    assert_eq!(
        detail(refusal),
        "the session settled on protocol version 4, which is outside the 3 to 3 this koshi \
         asked for"
    );
}

#[test]
fn a_router_version_above_the_range_names_the_control_plane_in_its_own_words() {
    let refusal = ROUTER
        .settled_version(3)
        .expect_err("3 is outside the 1 to 2 this build speaks");

    assert_eq!(
        detail(refusal),
        "the router settled on control-plane protocol version 3, which is outside the 1 to 2 \
         this koshi asked for"
    );
}

#[test]
fn a_version_below_the_floor_is_refused_the_same_way() {
    let refusal = SESSION
        .settled_version(2)
        .expect_err("2 is below the floor of 3");

    assert_eq!(
        detail(refusal),
        "the session settled on protocol version 2, which is outside the 3 to 3 this koshi \
         asked for"
    );
}

#[test]
fn a_router_version_below_the_floor_names_the_control_plane_range() {
    let refusal = ROUTER
        .settled_version(0)
        .expect_err("0 is below the router floor of 1");

    assert_eq!(
        detail(refusal),
        "the router settled on control-plane protocol version 0, which is outside the 1 to 2 \
         this koshi asked for"
    );
}

#[test]
fn the_largest_version_a_peer_can_name_is_outside_the_range() {
    let refusal = SESSION
        .settled_version(u32::MAX)
        .expect_err("4294967295 is outside the 3 to 3 this build speaks");

    assert_eq!(
        detail(refusal),
        "the session settled on protocol version 4294967295, which is outside the 3 to 3 this \
         koshi asked for"
    );
}

#[test]
fn a_known_result_comes_back_as_itself() {
    let response: Answer<MaybeKnown<IpcResult>> = Answer {
        request_id: Some(7),
        result: MaybeKnown::Known(IpcResult::Restarting),
    };

    assert_eq!(
        SESSION.take_result(response).expect("a known result"),
        IpcResult::Restarting
    );
}

#[test]
fn an_answer_that_names_no_request_still_hands_back_its_result() {
    let response: Answer<MaybeKnown<IpcResult>> = Answer {
        request_id: None,
        result: MaybeKnown::Known(IpcResult::Restarting),
    };

    assert_eq!(
        SESSION.take_result(response).expect("a known result"),
        IpcResult::Restarting
    );
}

#[test]
fn a_result_this_build_does_not_have_fails_naming_what_arrived() {
    let response: Answer<MaybeKnown<IpcResult>> = Answer {
        request_id: Some(7),
        result: MaybeKnown::Unknown {
            name: "Rehomed".to_string(),
        },
    };

    let refusal = SESSION
        .take_result(response)
        .expect_err("a result this build has no variant for");

    assert_eq!(
        detail(refusal),
        "the session answered with an unexpected Rehomed reply"
    );
}

#[test]
fn an_unexpected_reply_is_named_by_its_wire_name() {
    assert_eq!(
        detail(SESSION.unexpected_reply(&IpcResult::Restarting)),
        "the session answered with an unexpected Restarting reply"
    );
}

#[test]
fn a_reply_carrying_a_payload_is_named_by_its_variant_not_its_contents() {
    use koshi_core::ids::SessionId;
    use koshi_ipc::layout::SessionLayout;

    let layout = IpcResult::Layout(SessionLayout {
        id: SessionId::new(),
        name: "workspace".to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    });

    assert_eq!(
        detail(SESSION.unexpected_reply(&layout)),
        "the session answered with an unexpected Layout reply"
    );
}

#[test]
fn each_peer_names_itself_in_the_unexpected_reply() {
    assert_eq!(
        detail(ROUTER.unexpected_name("Created")),
        "the router answered with an unexpected Created reply"
    );
}

#[test]
fn a_transport_fault_carries_the_faults_own_words() {
    let fault = IpcError::NoListener {
        addr: "/nowhere.sock".to_string(),
    };
    let expected = fault.to_string();

    assert_eq!(detail(talk_failed(fault)), expected);
}

#[test]
fn a_protocol_refusal_carries_the_sentence_the_peer_sent() {
    let refusal = IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token presented does not match this Koshi's".to_string(),
    };

    assert_eq!(
        detail(refused(&refusal)),
        "the token presented does not match this Koshi's"
    );
}

#[test]
fn peer_text_reaches_the_message_filtered() {
    assert_eq!(
        detail(SESSION.unexpected_name("\u{1b}[2J\u{1b}[HRe\u{202e}homed")),
        "the session answered with an unexpected [2J[HRehomed reply"
    );

    let refusal = IpcErrorPayload {
        code: IpcErrorCode::Unknown,
        message: "\u{1b}]0;pwned\u{7}refused".to_string(),
    };
    assert_eq!(detail(refused(&refusal)), "]0;pwnedrefused");

    let long = IpcErrorPayload {
        code: IpcErrorCode::Unknown,
        message: "a".repeat(100_000),
    };
    assert_eq!(
        detail(refused(&long)).len(),
        koshi_core::text::MAX_REPORTED_TEXT_BYTES
    );
}

#[test]
fn each_peer_reads_its_range_from_the_versioned_surface_table() {
    assert_eq!(SESSION.surface, koshi_core::compat::SESSION_PROTOCOL);
    assert_eq!(ROUTER.surface, koshi_core::compat::CONTROL_PROTOCOL);
}

// --- Reading a Hello answer -------------------------------------------------

/// A session's answer carrying `result`, as the wire hands it to a caller.
fn session_answer(result: IpcResult) -> IncomingResponse {
    Answer {
        request_id: Some(1),
        result: MaybeKnown::Known(result),
    }
}

/// The router's answer carrying `result`, as the wire hands it to a caller.
fn router_answer(result: RouterResult) -> IncomingRouterResponse {
    Answer {
        request_id: Some(1),
        result: MaybeKnown::Known(result),
    }
}

#[test]
fn a_session_hello_hands_back_the_build_the_session_named() {
    let reply = session_answer(IpcResult::Hello {
        protocol_version: 3,
        version: "0.9.9".to_string(),
    });

    assert_eq!(
        session_hello_version(reply).expect("3 is the only version this build speaks"),
        (3, "0.9.9".to_string())
    );
}

#[test]
fn a_session_predating_the_build_field_hands_back_an_empty_string() {
    let reply = session_answer(IpcResult::Hello {
        protocol_version: 3,
        version: String::new(),
    });

    assert_eq!(
        session_hello_version(reply).expect("a build with no version field still opens"),
        (3, String::new())
    );
}

#[test]
fn a_session_hello_naming_a_version_outside_the_range_stops_the_exchange() {
    let reply = session_answer(IpcResult::Hello {
        protocol_version: 4,
        version: "0.9.9".to_string(),
    });

    let refusal = session_hello_version(reply).expect_err("4 is outside the 3 to 3");

    assert_eq!(
        detail(refusal),
        "the session settled on protocol version 4, which is outside the 3 to 3 this koshi \
         asked for"
    );
}

#[test]
fn a_session_refusing_the_hello_stops_the_exchange_with_its_own_sentence() {
    let reply = session_answer(IpcResult::Error(IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token presented does not match this Koshi's".to_string(),
    }));

    let refusal = session_hello_version(reply).expect_err("a refused Hello opens nothing");

    assert_eq!(
        detail(refusal),
        "the token presented does not match this Koshi's"
    );
}

#[test]
fn a_session_answering_no_hello_at_all_names_the_reply_that_arrived() {
    let reply = session_answer(IpcResult::Restarting);

    let refusal = session_hello_version(reply).expect_err("a Restarting is not a Hello");

    assert_eq!(
        detail(refusal),
        "the session answered with an unexpected Restarting reply"
    );
}

#[test]
fn a_hello_answer_this_build_cannot_name_stops_the_exchange() {
    let reply: IncomingResponse = Answer {
        request_id: Some(1),
        result: MaybeKnown::Unknown {
            name: "Rehomed".to_string(),
        },
    };

    let refusal = session_hello_version(reply).expect_err("this build has no Rehomed variant");

    assert_eq!(
        detail(refusal),
        "the session answered with an unexpected Rehomed reply"
    );
}

#[test]
fn a_router_hello_hands_back_the_build_the_router_named() {
    let reply = router_answer(RouterResult::Hello {
        protocol_version: 2,
        version: "0.9.9".to_string(),
    });

    assert_eq!(
        router_hello_version(reply).expect("2 is inside the 1 to 2 this build speaks"),
        "0.9.9"
    );
}

#[test]
fn a_router_hello_naming_a_version_outside_the_range_stops_the_exchange() {
    let reply = router_answer(RouterResult::Hello {
        protocol_version: 3,
        version: "0.9.9".to_string(),
    });

    let refusal = router_hello_version(reply).expect_err("3 is outside the 1 to 2");

    assert_eq!(
        detail(refusal),
        "the router settled on control-plane protocol version 3, which is outside the 1 to 2 \
         this koshi asked for"
    );
}

#[test]
fn a_router_refusing_the_hello_stops_the_exchange_with_its_own_sentence() {
    let reply = router_answer(RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token presented does not match the router's".to_string(),
    }));

    let refusal = router_hello_version(reply).expect_err("a refused Hello opens nothing");

    assert_eq!(
        detail(refusal),
        "the token presented does not match the router's"
    );
}

#[test]
fn a_router_answering_no_hello_at_all_names_the_reply_that_arrived() {
    let reply = router_answer(RouterResult::Restarting);

    let refusal = router_hello_version(reply).expect_err("a Restarting is not a Hello");

    assert_eq!(
        detail(refusal),
        "the router answered with an unexpected Restarting reply"
    );
}

#[test]
fn the_target_client_refusal_names_the_version_and_the_release() {
    assert_eq!(TARGET_CLIENT_PROTOCOL, 3);

    let refusal = require_client_targeting(2, true).expect_err("a session settled on 2 is below 3");
    assert_eq!(
        detail(refusal),
        "this session speaks protocol 2; --client needs a session started by koshi 0.4.0 or \
         later"
    );

    let refusal = require_client_targeting(2, true).expect_err("a session settled on 2 is below 3");
    assert_eq!(CliExitCode::from(&refusal).code(), 4);

    let refusal = require_client_targeting(0, true).expect_err("a session settled on 0 is below 3");
    assert_eq!(
        detail(refusal),
        "this session speaks protocol 0; --client needs a session started by koshi 0.4.0 or \
         later"
    );
}

#[test]
fn a_settled_version_at_or_above_three_is_accepted_and_no_named_client_accepts_any() {
    require_client_targeting(3, true).expect("3 meets a floor of 3");
    require_client_targeting(4, true).expect("4 is above a floor of 3");
    // A command naming no client takes every settled version, including one
    // below 3.
    require_client_targeting(2, false).expect("no named client takes 2");
    require_client_targeting(0, false).expect("no named client takes 0");
}

#[test]
fn a_transport_failure_carrying_peer_bytes_is_filtered() {
    // `MalformedFrame` carries the decoder's message, which quotes the name
    // the peer sent.
    let hostile = format!("unknown variant `{}Rehomed`", "\u{1b}[2J");

    assert_eq!(
        detail(talk_failed(IpcError::MalformedFrame {
            detail: hostile.clone(),
        })),
        "ipc frame is not a readable message: unknown variant `[2JRehomed`"
    );
    assert!(
        !detail(talk_failed(IpcError::MalformedFrame { detail: hostile })).contains('\u{1b}'),
        "no escape byte reaches the sentence"
    );
}

#[test]
fn a_rejections_hint_is_filtered_and_an_applied_result_is_left_alone() {
    let command_id = koshi_core::ids::CommandId::new();
    let filtered = filter_rejection_hint(CommandResult::Rejected {
        command_id,
        reason: RejectReason::Unauthorized,
        help: Some("\u{1b}[2Jattach\u{7f} first".to_string()),
    });

    assert_eq!(
        filtered,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("[2Jattach first".to_string()),
        }
    );

    let no_hint = CommandResult::Rejected {
        command_id,
        reason: RejectReason::Unauthorized,
        help: None,
    };
    assert_eq!(filter_rejection_hint(no_hint.clone()), no_hint);

    let applied = CommandResult::Ok {
        command_id,
        emitted_events: Vec::new(),
    };
    assert_eq!(filter_rejection_hint(applied.clone()), applied);
}

#[test]
fn a_session_hello_filters_the_build_it_named() {
    // `koshi server-version` prints this string, and the session that answered
    // is another user's process or another machine's.
    let reply = session_answer(IpcResult::Hello {
        protocol_version: 3,
        version: "\u{1b}]0;pwned\u{7}0.9.9".to_string(),
    });

    assert_eq!(
        session_hello_version(reply).expect("3 is the only version this build speaks"),
        (3, "]0;pwned0.9.9".to_string())
    );
}
