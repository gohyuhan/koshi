//! Tests for the parts of an exchange both peers do the same way: the version
//! check at the Hello, unwrapping an answer that may name a result this build
//! does not have, and the two failures that read the same for either peer.
//!
//! Each peer's own wording is pinned here, because those sentences are what a
//! person reads when a verb fails.

use super::*;

use koshi_ipc::protocol::{IpcErrorCode, IpcResult};

/// The sentence a failure carries, for asserting on it exactly.
///
/// Every failure in this module is [`CliError::IpcUnavailable`]; any other
/// variant is the test's own failure, not a wording mismatch.
fn detail(error: CliError) -> String {
    match error {
        CliError::IpcUnavailable { detail } => detail,
        other => panic!("expected IpcUnavailable, got {other:?}"),
    }
}

#[test]
fn a_version_inside_the_range_this_build_sent_is_accepted() {
    assert!(SESSION.settled_version(2).is_ok());
    assert!(ROUTER.settled_version(1).is_ok());
    assert!(ROUTER.settled_version(2).is_ok());
}

#[test]
fn a_session_version_above_the_range_names_both_the_version_and_the_range() {
    let refusal = SESSION
        .settled_version(3)
        .expect_err("3 is outside the 2 to 2 this build speaks");

    assert_eq!(
        detail(refusal),
        "the session settled on protocol version 3, which is outside the 2 to 2 this koshi \
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
        .settled_version(1)
        .expect_err("1 is below the floor of 2");

    assert_eq!(
        detail(refusal),
        "the session settled on protocol version 1, which is outside the 2 to 2 this koshi \
         asked for"
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
fn each_peer_reads_its_range_from_the_versioned_surface_table() {
    // The two ranges are the table's, not a second copy that could drift from
    // it.
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
        protocol_version: 2,
        version: "0.9.9".to_string(),
    });

    assert_eq!(
        session_hello_version(reply).expect("2 is inside the 2 to 2 this build speaks"),
        "0.9.9"
    );
}

#[test]
fn a_session_predating_the_build_field_hands_back_an_empty_string() {
    let reply = session_answer(IpcResult::Hello {
        protocol_version: 2,
        version: String::new(),
    });

    assert_eq!(
        session_hello_version(reply).expect("a build with no version field still opens"),
        ""
    );
}

#[test]
fn a_session_hello_naming_a_version_outside_the_range_stops_the_exchange() {
    let reply = session_answer(IpcResult::Hello {
        protocol_version: 3,
        version: "0.9.9".to_string(),
    });

    let refusal = session_hello_version(reply).expect_err("3 is outside the 2 to 2");

    assert_eq!(
        detail(refusal),
        "the session settled on protocol version 3, which is outside the 2 to 2 this koshi \
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
