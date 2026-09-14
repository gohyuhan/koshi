//! Tests for the connection handshake gate: a Hello whose version range
//! overlaps this build's and which meets its peer's token rule opens it, the
//! settled version is the highest both sides speak, every refusal carries its
//! exact code and message, and a refusal never changes what the gate lets
//! through.

use std::time::{Duration, UNIX_EPOCH};

use koshi_core::command::{Command, CommandEnvelope, CommandSource, ToggleLockModeArgs};
use koshi_core::ids::CommandId;

use super::*;

/// A protocol version above every version this build speaks.
const ABOVE_PROTOCOL_VERSION: u32 = PROTOCOL_VERSION + 1;

/// One below the lowest version this build speaks. Saturates at `0`.
const BELOW_PROTOCOL_VERSION: u32 = MIN_PROTOCOL_VERSION.saturating_sub(1);

/// The token this Koshi expects, as the gate under test holds it.
fn expected_connection_token() -> ConnectionToken {
    ConnectionToken::from_secret("k7QxSecret")
}

/// A gate for a fresh connection from this machine's own user, still closed.
fn build_test_handshake() -> Handshake {
    Handshake::from_expected_token_and_peer(
        expected_connection_token(),
        Peer::Local {
            is_same_user: true,
            is_other_user_access_allowed: false,
        },
    )
}

/// A gate for a fresh connection from another machine, still closed.
fn remote_build_test_handshake() -> Handshake {
    Handshake::from_expected_token_and_peer(expected_connection_token(), Peer::Remote)
}

/// A gate for a fresh connection from another user of this machine, with
/// `allow-other-users` set to `allowed`.
fn other_user_build_test_handshake(is_other_user_access_allowed: bool) -> Handshake {
    Handshake::from_expected_token_and_peer(
        expected_connection_token(),
        Peer::Local {
            is_same_user: false,
            is_other_user_access_allowed,
        },
    )
}

/// The refusal a Hello from another user earns while `allow-other-users` is
/// off, spelled out.
fn other_users_refusal() -> IpcErrorPayload {
    IpcErrorPayload {
        code: IpcErrorCode::OtherUsersOff,
        message: "this Koshi serves only the user who started it; \
                  set `allow-other-users #true` in koshi.kdl to let \
                  the other users of this machine in"
            .to_string(),
    }
}

/// The refusal a Hello presenting a token other than [`expected_connection_token`] earns,
/// spelled out.
fn bad_token_refusal() -> IpcErrorPayload {
    IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token presented does not match this Koshi's".to_string(),
    }
}

/// A Hello speaking `minimum_protocol_version` to `maximum_protocol_version` and presenting the right token.
fn hello_speaking(minimum_protocol_version: u32, maximum_protocol_version: u32) -> IpcRequestKind {
    IpcRequestKind::Hello {
        min_protocol_version: minimum_protocol_version,
        max_protocol_version: maximum_protocol_version,
        connection_token: expected_connection_token(),
        is_remote: false,
    }
}

/// A Hello speaking exactly this build's range, with the right token.
fn good_hello() -> IpcRequestKind {
    hello_speaking(MIN_PROTOCOL_VERSION, PROTOCOL_VERSION)
}

/// A Hello speaking this build's range, presenting the right token, and
/// saying `remote` — the shape the router sends for a caller it accepted over
/// TLS.
fn remote_hello() -> IpcRequestKind {
    IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: expected_connection_token(),
        is_remote: true,
    }
}

/// A Hello speaking this build's range and presenting a wrong token.
fn wrong_token_hello() -> IpcRequestKind {
    IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: ConnectionToken::from_secret("wrongToken"),
        is_remote: false,
    }
}

/// The refusal an out-of-range Hello earns, spelled out.
fn version_refusal(
    minimum_protocol_version: u32,
    maximum_protocol_version: u32,
) -> IpcErrorPayload {
    IpcErrorPayload {
        code: IpcErrorCode::UnsupportedVersion,
        message: format!(
            "the caller speaks protocol versions {minimum_protocol_version} to \
             {maximum_protocol_version}, \
             this Koshi speaks {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}"
        ),
    }
}

/// A submit-command request carrying one command with no arguments.
fn submit_command() -> IpcRequestKind {
    IpcRequestKind::SubmitCommand(Box::new(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::ExternalCli {
            session_id: None,
            target_client_id: None,
        },
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )))
}

#[test]
fn a_hello_speaking_this_builds_range_is_accepted() {
    assert_eq!(
        build_test_handshake().validate_request_kind(&good_hello()),
        Ok(())
    );
}

#[test]
fn a_closed_gate_has_settled_no_version() {
    assert_eq!(build_test_handshake().get_agreed_protocol_version(), None);
}

#[test]
fn an_accepted_hello_settles_the_highest_version_both_sides_speak() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&hello_speaking(
        MIN_PROTOCOL_VERSION,
        ABOVE_PROTOCOL_VERSION,
    ))
    .expect("a range covering this build's is accepted");

    assert_eq!(
        gate.get_agreed_protocol_version(),
        Some(PROTOCOL_VERSION),
        "a caller reaching above this build settles on this build's highest"
    );
}

#[test]
fn a_caller_speaking_only_this_builds_lowest_settles_there() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&hello_speaking(MIN_PROTOCOL_VERSION, MIN_PROTOCOL_VERSION))
        .expect("a caller pinned to the floor is accepted");

    assert_eq!(
        gate.get_agreed_protocol_version(),
        Some(MIN_PROTOCOL_VERSION)
    );
}

#[test]
fn an_accepted_hello_opens_the_gate_for_other_requests() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&good_hello())
        .expect("the Hello is accepted");

    assert_eq!(
        gate.validate_request_kind(&IpcRequestKind::Discovery),
        Ok(())
    );
    assert_eq!(gate.validate_request_kind(&submit_command()), Ok(()));
}

#[test]
fn a_hello_with_a_wrong_token_is_refused_as_bad_token() {
    assert_eq!(
        build_test_handshake().validate_request_kind(&wrong_token_hello()),
        Err(bad_token_refusal())
    );
}

#[test]
fn a_hello_from_another_machine_with_a_wrong_token_is_refused_as_bad_token() {
    assert_eq!(
        remote_build_test_handshake().validate_request_kind(&wrong_token_hello()),
        Err(bad_token_refusal())
    );
}

#[test]
fn a_hello_from_another_machine_with_the_right_token_is_accepted() {
    let mut gate = remote_build_test_handshake();

    assert_eq!(gate.validate_request_kind(&good_hello()), Ok(()));
    assert_eq!(gate.get_agreed_protocol_version(), Some(PROTOCOL_VERSION));
}

#[test]
fn an_allowed_other_user_opens_the_gate_without_a_token() {
    let mut gate = other_user_build_test_handshake(true);
    let hello = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: ConnectionToken::from_secret(""),
        is_remote: false,
    };

    assert_eq!(gate.validate_request_kind(&hello), Ok(()));
    assert_eq!(gate.get_agreed_protocol_version(), Some(PROTOCOL_VERSION));
    assert_eq!(
        gate.validate_request_kind(&IpcRequestKind::Discovery),
        Ok(()),
        "the gate is open, so the requests after the Hello are served"
    );
}

#[test]
fn the_starting_user_still_presents_the_token_while_other_users_are_allowed() {
    // With the setting on, the user who started the session still presents
    // the token.
    let mut gate = Handshake::from_expected_token_and_peer(
        expected_connection_token(),
        Peer::Local {
            is_same_user: true,
            is_other_user_access_allowed: true,
        },
    );

    assert_eq!(
        gate.validate_request_kind(&wrong_token_hello()),
        Err(bad_token_refusal())
    );
    assert_eq!(gate.get_agreed_protocol_version(), None);
    assert_eq!(gate.validate_request_kind(&good_hello()), Ok(()));
}

#[test]
fn an_allowed_other_user_is_admitted_whatever_token_it_presents() {
    // An allowed other user's Hello is not judged on its token.
    let mut gate = other_user_build_test_handshake(true);

    assert_eq!(gate.validate_request_kind(&wrong_token_hello()), Ok(()));
    assert_eq!(gate.get_agreed_protocol_version(), Some(PROTOCOL_VERSION));
}

#[test]
fn a_hello_from_another_machine_presenting_no_token_is_refused_as_bad_token() {
    // An empty token from another machine is refused like any other wrong
    // token.
    let hello = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: ConnectionToken::from_secret(""),
        is_remote: false,
    };

    assert_eq!(
        remote_build_test_handshake().validate_request_kind(&hello),
        Err(bad_token_refusal())
    );
}

#[test]
fn repeated_hellos_never_wear_down_a_setting_that_is_off() {
    let mut gate = other_user_build_test_handshake(false);

    for _ in 0..3 {
        assert_eq!(
            gate.validate_request_kind(&good_hello()),
            Err(other_users_refusal())
        );
    }

    assert_eq!(gate.get_agreed_protocol_version(), None);
    assert_eq!(
        gate.validate_request_kind(&IpcRequestKind::Discovery),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn another_user_is_refused_while_the_setting_is_off() {
    assert_eq!(
        other_user_build_test_handshake(false).validate_request_kind(&good_hello()),
        Err(other_users_refusal()),
        "the right token does not let another user in while the setting is off"
    );
    assert_eq!(
        other_user_build_test_handshake(false).validate_request_kind(&wrong_token_hello()),
        Err(other_users_refusal())
    );
}

#[test]
fn a_refused_other_user_leaves_the_gate_closed() {
    let mut gate = other_user_build_test_handshake(false);

    gate.validate_request_kind(&good_hello())
        .expect_err("the Hello is refused");

    assert_eq!(gate.get_agreed_protocol_version(), None);
    assert_eq!(
        gate.validate_request_kind(&IpcRequestKind::Discovery),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn an_out_of_range_hello_from_another_user_is_refused_for_the_version() {
    assert_eq!(
        other_user_build_test_handshake(false).validate_request_kind(&hello_speaking(
            ABOVE_PROTOCOL_VERSION,
            ABOVE_PROTOCOL_VERSION
        )),
        Err(version_refusal(
            ABOVE_PROTOCOL_VERSION,
            ABOVE_PROTOCOL_VERSION
        )),
        "the version is settled before the peer's own rule, for every peer"
    );
    assert_eq!(
        other_user_build_test_handshake(true).validate_request_kind(&hello_speaking(
            ABOVE_PROTOCOL_VERSION,
            ABOVE_PROTOCOL_VERSION
        )),
        Err(version_refusal(
            ABOVE_PROTOCOL_VERSION,
            ABOVE_PROTOCOL_VERSION
        ))
    );
    assert_eq!(
        remote_build_test_handshake().validate_request_kind(&hello_speaking(
            ABOVE_PROTOCOL_VERSION,
            ABOVE_PROTOCOL_VERSION
        )),
        Err(version_refusal(
            ABOVE_PROTOCOL_VERSION,
            ABOVE_PROTOCOL_VERSION
        ))
    );
}

#[test]
fn a_caller_speaking_only_above_this_build_is_refused_naming_both_ranges() {
    assert_eq!(
        build_test_handshake().validate_request_kind(&hello_speaking(
            ABOVE_PROTOCOL_VERSION,
            ABOVE_PROTOCOL_VERSION
        )),
        Err(version_refusal(
            ABOVE_PROTOCOL_VERSION,
            ABOVE_PROTOCOL_VERSION
        ))
    );
}

#[test]
fn a_caller_speaking_only_below_this_build_is_refused_naming_both_ranges() {
    assert_eq!(
        build_test_handshake().validate_request_kind(&hello_speaking(
            BELOW_PROTOCOL_VERSION,
            BELOW_PROTOCOL_VERSION
        )),
        Err(version_refusal(
            BELOW_PROTOCOL_VERSION,
            BELOW_PROTOCOL_VERSION
        ))
    );
}

#[test]
fn a_hello_whose_range_is_inverted_is_refused_for_the_version() {
    let mut gate = build_test_handshake();

    assert_eq!(
        gate.validate_request_kind(&hello_speaking(
            ABOVE_PROTOCOL_VERSION,
            BELOW_PROTOCOL_VERSION
        )),
        Err(version_refusal(
            ABOVE_PROTOCOL_VERSION,
            BELOW_PROTOCOL_VERSION
        )),
        "a range whose low end is above its high end shares no version"
    );
    assert_eq!(gate.get_agreed_protocol_version(), None);
}

#[test]
fn a_caller_speaking_every_version_settles_on_this_builds_highest() {
    let mut gate = build_test_handshake();

    assert_eq!(
        gate.validate_request_kind(&hello_speaking(0, u32::MAX)),
        Ok(())
    );
    assert_eq!(gate.get_agreed_protocol_version(), Some(PROTOCOL_VERSION));
}

#[test]
fn a_refused_version_settles_nothing() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&hello_speaking(
        ABOVE_PROTOCOL_VERSION,
        ABOVE_PROTOCOL_VERSION,
    ))
    .expect_err("the Hello is refused");

    assert_eq!(gate.get_agreed_protocol_version(), None);
}

#[test]
fn an_out_of_range_hello_with_a_wrong_token_is_refused_for_the_version() {
    let hello = IpcRequestKind::Hello {
        min_protocol_version: ABOVE_PROTOCOL_VERSION,
        max_protocol_version: ABOVE_PROTOCOL_VERSION,
        connection_token: ConnectionToken::from_secret("wrongToken"),
        is_remote: false,
    };

    assert_eq!(
        build_test_handshake().validate_request_kind(&hello),
        Err(version_refusal(
            ABOVE_PROTOCOL_VERSION,
            ABOVE_PROTOCOL_VERSION
        ))
    );
}

#[test]
fn a_request_before_any_hello_is_refused_as_hello_required() {
    assert_eq!(
        build_test_handshake().validate_request_kind(&IpcRequestKind::Discovery),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_hello_required_refusal_names_the_kind_without_its_payload() {
    assert_eq!(
        build_test_handshake().validate_request_kind(&submit_command()),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "SubmitCommand arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn an_unknown_kind_before_any_hello_is_refused_as_hello_required() {
    assert_eq!(
        build_test_handshake().build_unknown_request_kind_error("Floating"),
        IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Floating arrived before a Hello opened the connection".to_string(),
        },
        "a closed gate tells an unopened connection nothing about which kinds exist"
    );
}

#[test]
fn an_unknown_kind_on_an_open_gate_is_refused_by_name() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&good_hello())
        .expect("the Hello is accepted");

    assert_eq!(
        gate.build_unknown_request_kind_error("Floating"),
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this Koshi has no request kind named Floating".to_string(),
        }
    );
}

#[test]
fn a_refused_hello_leaves_the_gate_closed() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&wrong_token_hello())
        .expect_err("the Hello is refused");

    assert_eq!(
        gate.validate_request_kind(&IpcRequestKind::Discovery),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_good_hello_after_a_version_refusal_opens_the_build_test_handshake() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&hello_speaking(
        ABOVE_PROTOCOL_VERSION,
        ABOVE_PROTOCOL_VERSION,
    ))
    .expect_err("the Hello is refused");
    gate.validate_request_kind(&good_hello())
        .expect("the Hello is accepted");

    assert_eq!(
        gate.validate_request_kind(&IpcRequestKind::Discovery),
        Ok(())
    );
    assert_eq!(gate.get_agreed_protocol_version(), Some(PROTOCOL_VERSION));
}

#[test]
fn a_good_hello_after_a_token_refusal_opens_the_build_test_handshake() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&wrong_token_hello())
        .expect_err("the Hello is refused");
    gate.validate_request_kind(&good_hello())
        .expect("the Hello is accepted");

    assert_eq!(
        gate.validate_request_kind(&IpcRequestKind::Discovery),
        Ok(())
    );
}

#[test]
fn a_repeated_hello_on_an_open_gate_gets_the_same_answer() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&good_hello())
        .expect("the Hello is accepted");

    assert_eq!(gate.validate_request_kind(&good_hello()), Ok(()));
    assert_eq!(gate.get_agreed_protocol_version(), Some(PROTOCOL_VERSION));
}

#[test]
fn a_refused_hello_on_an_open_gate_leaves_it_open_and_keeps_its_version() {
    let mut gate = build_test_handshake();

    gate.validate_request_kind(&good_hello())
        .expect("the Hello is accepted");
    gate.validate_request_kind(&wrong_token_hello())
        .expect_err("the Hello is refused");

    assert_eq!(
        gate.validate_request_kind(&IpcRequestKind::Discovery),
        Ok(())
    );
    assert_eq!(gate.get_agreed_protocol_version(), Some(PROTOCOL_VERSION));
}

#[test]
fn compute_agreed_protocol_version_uses_highest_shared_version() {
    assert_eq!(
        compute_agreed_protocol_version(2, 5, 2, 3),
        Some(3),
        "the caller reaches higher, so this build's highest wins"
    );
    assert_eq!(
        compute_agreed_protocol_version(2, 3, 2, 5),
        Some(3),
        "this build reaches higher, so the caller's highest wins"
    );
    assert_eq!(
        compute_agreed_protocol_version(4, 4, 4, 4),
        Some(4),
        "one shared version"
    );
    assert_eq!(
        compute_agreed_protocol_version(6, 7, 2, 5),
        None,
        "the caller is entirely above this build"
    );
    assert_eq!(
        compute_agreed_protocol_version(1, 1, 2, 5),
        None,
        "the caller is entirely below this build"
    );
}

#[test]
fn compute_agreed_protocol_version_rejects_inverted_u32_ranges() {
    assert_eq!(
        compute_agreed_protocol_version(0, u32::MAX, 0, u32::MAX),
        Some(u32::MAX),
        "two full ranges settle on the highest version there is"
    );
    assert_eq!(
        compute_agreed_protocol_version(u32::MAX, 0, 2, 5),
        None,
        "a caller range whose low end is above its high end shares nothing"
    );
    assert_eq!(
        compute_agreed_protocol_version(2, 5, u32::MAX, 0),
        None,
        "a build range whose low end is above its high end shares nothing"
    );
}

#[test]
fn a_hello_that_says_nothing_leaves_the_connection_local() {
    let mut gate = build_test_handshake();

    assert_eq!(gate.validate_request_kind(&good_hello()), Ok(()));

    assert!(!gate.is_remote_caller());
}

#[test]
fn a_hello_saying_remote_marks_the_connection() {
    let mut gate = build_test_handshake();

    assert_eq!(gate.validate_request_kind(&remote_hello()), Ok(()));

    assert!(gate.is_remote_caller());
}

#[test]
fn a_second_hello_cannot_clear_the_remote_mark() {
    let mut gate = build_test_handshake();
    assert_eq!(gate.validate_request_kind(&remote_hello()), Ok(()));

    assert_eq!(gate.validate_request_kind(&good_hello()), Ok(()));

    assert!(
        gate.is_remote_caller(),
        "a later Hello saying local left the connection marked remote"
    );
}

#[test]
fn a_second_hello_can_still_set_the_remote_mark() {
    let mut gate = build_test_handshake();
    assert_eq!(gate.validate_request_kind(&good_hello()), Ok(()));

    assert_eq!(gate.validate_request_kind(&remote_hello()), Ok(()));

    assert!(gate.is_remote_caller());
}

#[test]
fn a_refused_hello_saying_remote_does_not_mark_the_connection() {
    let mut gate = build_test_handshake();
    let refused = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: ConnectionToken::from_secret("wrongToken"),
        is_remote: true,
    };

    assert_eq!(
        gate.validate_request_kind(&refused),
        Err(bad_token_refusal())
    );

    assert!(!gate.is_remote_caller());
    assert_eq!(gate.validate_request_kind(&good_hello()), Ok(()));
    assert!(
        !gate.is_remote_caller(),
        "a Hello that never passed its token check marked the connection"
    );
}

#[test]
fn a_refused_hello_saying_remote_leaves_the_gate_closed() {
    let mut gate = build_test_handshake();
    let refused = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: ConnectionToken::from_secret("wrongToken"),
        is_remote: true,
    };

    assert_eq!(
        gate.validate_request_kind(&refused),
        Err(bad_token_refusal())
    );

    assert_eq!(gate.get_agreed_protocol_version(), None);
}

#[test]
fn a_remote_hello_from_another_machine_marks_the_connection() {
    let mut gate = remote_build_test_handshake();

    assert_eq!(gate.validate_request_kind(&remote_hello()), Ok(()));

    assert!(gate.is_remote_caller());
    assert_eq!(gate.get_agreed_protocol_version(), Some(PROTOCOL_VERSION));
}

#[test]
fn an_allowed_other_users_hello_saying_remote_marks_the_connection() {
    let mut gate = other_user_build_test_handshake(true);
    let hello = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: ConnectionToken::from_secret(""),
        is_remote: true,
    };

    assert_eq!(gate.validate_request_kind(&hello), Ok(()));

    assert!(gate.is_remote_caller());
    assert_eq!(gate.get_agreed_protocol_version(), Some(PROTOCOL_VERSION));
}

#[test]
fn a_refused_other_users_hello_saying_remote_does_not_mark_the_connection() {
    let mut gate = other_user_build_test_handshake(false);
    let refused = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: expected_connection_token(),
        is_remote: true,
    };

    assert_eq!(
        gate.validate_request_kind(&refused),
        Err(other_users_refusal())
    );

    assert!(!gate.is_remote_caller());
    assert_eq!(gate.get_agreed_protocol_version(), None);
}

#[test]
fn the_gate_trait_names_only_a_hello_as_the_hello() {
    use crate::plane::Gate as _;

    assert!(Handshake::is_hello(&good_hello()));
    assert!(Handshake::is_hello(&wrong_token_hello()));
    assert!(!Handshake::is_hello(&IpcRequestKind::Discovery));
    assert!(!Handshake::is_hello(&submit_command()));
}
