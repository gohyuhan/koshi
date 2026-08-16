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
const ABOVE_RANGE: u32 = PROTOCOL_VERSION + 1;

/// A protocol version below every version this build speaks. Saturating, so
/// this stays a version rather than an overflow if the floor ever reaches 0.
const BELOW_RANGE: u32 = MIN_PROTOCOL_VERSION.saturating_sub(1);

/// The token this Koshi expects, as the gate under test holds it.
fn expected() -> ConnectionToken {
    ConnectionToken::new("k7QxSecret")
}

/// A gate for a fresh connection from this machine's own user, still closed.
fn gate() -> Handshake {
    Handshake::new(
        expected(),
        Peer::Local {
            same_user: true,
            other_users_allowed: false,
        },
    )
}

/// A gate for a fresh connection from another machine, still closed.
fn remote_gate() -> Handshake {
    Handshake::new(expected(), Peer::Remote)
}

/// A gate for a fresh connection from another user of this machine, with
/// `allow-other-users` set to `allowed`.
fn other_user_gate(allowed: bool) -> Handshake {
    Handshake::new(
        expected(),
        Peer::Local {
            same_user: false,
            other_users_allowed: allowed,
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

/// A Hello speaking `min` to `max` and presenting the right token.
fn hello_speaking(min: u32, max: u32) -> IpcRequestKind {
    IpcRequestKind::Hello {
        min_protocol_version: min,
        max_protocol_version: max,
        token: expected(),
        remote: false,
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
        token: expected(),
        remote: true,
    }
}

/// A Hello speaking this build's range and presenting a wrong token.
fn wrong_token_hello() -> IpcRequestKind {
    IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: ConnectionToken::new("wrongToken"),
        remote: false,
    }
}

/// The refusal an out-of-range Hello earns, spelled out.
fn version_refusal(min: u32, max: u32) -> IpcErrorPayload {
    IpcErrorPayload {
        code: IpcErrorCode::UnsupportedVersion,
        message: format!(
            "the caller speaks protocol versions {min} to {max}, \
             this Koshi speaks {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}"
        ),
    }
}

/// A submit-command request carrying one command with no arguments.
fn submit_command() -> IpcRequestKind {
    IpcRequestKind::SubmitCommand(Box::new(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::ExternalCli { session_id: None },
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )))
}

#[test]
fn a_hello_speaking_this_builds_range_is_accepted() {
    assert_eq!(gate().check(&good_hello()), Ok(()));
}

#[test]
fn a_closed_gate_has_settled_no_version() {
    assert_eq!(gate().agreed(), None);
}

#[test]
fn an_accepted_hello_settles_the_highest_version_both_sides_speak() {
    let mut gate = gate();

    gate.check(&hello_speaking(MIN_PROTOCOL_VERSION, ABOVE_RANGE))
        .expect("a range covering this build's is accepted");

    assert_eq!(
        gate.agreed(),
        Some(PROTOCOL_VERSION),
        "a caller reaching above this build settles on this build's highest"
    );
}

#[test]
fn a_caller_speaking_only_this_builds_lowest_settles_there() {
    let mut gate = gate();

    gate.check(&hello_speaking(MIN_PROTOCOL_VERSION, MIN_PROTOCOL_VERSION))
        .expect("a caller pinned to the floor is accepted");

    assert_eq!(gate.agreed(), Some(MIN_PROTOCOL_VERSION));
}

#[test]
fn an_accepted_hello_opens_the_gate_for_other_requests() {
    let mut gate = gate();

    gate.check(&good_hello()).expect("the Hello is accepted");

    assert_eq!(gate.check(&IpcRequestKind::Discovery), Ok(()));
    assert_eq!(gate.check(&submit_command()), Ok(()));
}

#[test]
fn a_hello_with_a_wrong_token_is_refused_as_bad_token() {
    assert_eq!(
        gate().check(&wrong_token_hello()),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        })
    );
}

#[test]
fn a_hello_from_another_machine_with_a_wrong_token_is_refused_as_bad_token() {
    assert_eq!(
        remote_gate().check(&wrong_token_hello()),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        })
    );
}

#[test]
fn a_hello_from_another_machine_with_the_right_token_is_accepted() {
    let mut gate = remote_gate();

    assert_eq!(gate.check(&good_hello()), Ok(()));
    assert_eq!(gate.agreed(), Some(PROTOCOL_VERSION));
}

#[test]
fn an_allowed_other_user_opens_the_gate_without_a_token() {
    let mut gate = other_user_gate(true);
    let hello = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: ConnectionToken::new(""),
        remote: false,
    };

    assert_eq!(gate.check(&hello), Ok(()));
    assert_eq!(gate.agreed(), Some(PROTOCOL_VERSION));
    assert_eq!(
        gate.check(&IpcRequestKind::Discovery),
        Ok(()),
        "the gate is open, so the requests after the Hello are served"
    );
}

#[test]
fn the_starting_user_still_presents_the_token_while_other_users_are_allowed() {
    // Turning the setting on widens who may reach the socket, never what the
    // user who started the session has to present on it.
    let mut gate = Handshake::new(
        expected(),
        Peer::Local {
            same_user: true,
            other_users_allowed: true,
        },
    );

    assert_eq!(
        gate.check(&wrong_token_hello()),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        })
    );
    assert_eq!(gate.agreed(), None);
    assert_eq!(gate.check(&good_hello()), Ok(()));
}

#[test]
fn an_allowed_other_user_is_admitted_whatever_token_it_presents() {
    // The token proves a caller read the session's endpoint file, which
    // another user cannot; their Hello is not judged on the field they fill.
    let mut gate = other_user_gate(true);

    assert_eq!(gate.check(&wrong_token_hello()), Ok(()));
    assert_eq!(gate.agreed(), Some(PROTOCOL_VERSION));
}

#[test]
fn a_hello_from_another_machine_presenting_no_token_is_refused_as_bad_token() {
    // The empty token is what an admitted local user sends. Arriving from
    // another machine it earns the same refusal as any other wrong one.
    let hello = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: ConnectionToken::new(""),
        remote: false,
    };

    assert_eq!(
        remote_gate().check(&hello),
        Err(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        })
    );
}

#[test]
fn repeated_hellos_never_wear_down_a_setting_that_is_off() {
    let mut gate = other_user_gate(false);

    for _ in 0..3 {
        assert_eq!(gate.check(&good_hello()), Err(other_users_refusal()));
    }

    assert_eq!(gate.agreed(), None);
    assert_eq!(
        gate.check(&IpcRequestKind::Discovery),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn another_user_is_refused_while_the_setting_is_off() {
    assert_eq!(
        other_user_gate(false).check(&good_hello()),
        Err(other_users_refusal()),
        "the right token does not let another user in while the setting is off"
    );
    assert_eq!(
        other_user_gate(false).check(&wrong_token_hello()),
        Err(other_users_refusal())
    );
}

#[test]
fn a_refused_other_user_leaves_the_gate_closed() {
    let mut gate = other_user_gate(false);

    gate.check(&good_hello()).expect_err("the Hello is refused");

    assert_eq!(gate.agreed(), None);
    assert_eq!(
        gate.check(&IpcRequestKind::Discovery),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn an_out_of_range_hello_from_another_user_is_refused_for_the_version() {
    assert_eq!(
        other_user_gate(false).check(&hello_speaking(ABOVE_RANGE, ABOVE_RANGE)),
        Err(version_refusal(ABOVE_RANGE, ABOVE_RANGE)),
        "the version is settled before the peer's own rule, for every peer"
    );
    assert_eq!(
        other_user_gate(true).check(&hello_speaking(ABOVE_RANGE, ABOVE_RANGE)),
        Err(version_refusal(ABOVE_RANGE, ABOVE_RANGE))
    );
    assert_eq!(
        remote_gate().check(&hello_speaking(ABOVE_RANGE, ABOVE_RANGE)),
        Err(version_refusal(ABOVE_RANGE, ABOVE_RANGE))
    );
}

#[test]
fn a_caller_speaking_only_above_this_build_is_refused_naming_both_ranges() {
    assert_eq!(
        gate().check(&hello_speaking(ABOVE_RANGE, ABOVE_RANGE)),
        Err(version_refusal(ABOVE_RANGE, ABOVE_RANGE))
    );
}

#[test]
fn a_caller_speaking_only_below_this_build_is_refused_naming_both_ranges() {
    assert_eq!(
        gate().check(&hello_speaking(BELOW_RANGE, BELOW_RANGE)),
        Err(version_refusal(BELOW_RANGE, BELOW_RANGE))
    );
}

#[test]
fn a_refused_version_settles_nothing() {
    let mut gate = gate();

    gate.check(&hello_speaking(ABOVE_RANGE, ABOVE_RANGE))
        .expect_err("the Hello is refused");

    assert_eq!(gate.agreed(), None);
}

#[test]
fn an_out_of_range_hello_with_a_wrong_token_is_refused_for_the_version() {
    let hello = IpcRequestKind::Hello {
        min_protocol_version: ABOVE_RANGE,
        max_protocol_version: ABOVE_RANGE,
        token: ConnectionToken::new("wrongToken"),
        remote: false,
    };

    assert_eq!(
        gate().check(&hello),
        Err(version_refusal(ABOVE_RANGE, ABOVE_RANGE))
    );
}

#[test]
fn a_request_before_any_hello_is_refused_as_hello_required() {
    assert_eq!(
        gate().check(&IpcRequestKind::Discovery),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_hello_required_refusal_names_the_kind_without_its_payload() {
    assert_eq!(
        gate().check(&submit_command()),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "SubmitCommand arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn an_unknown_kind_before_any_hello_is_refused_as_hello_required() {
    assert_eq!(
        gate().refuse_unknown("Floating"),
        IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Floating arrived before a Hello opened the connection".to_string(),
        },
        "a closed gate tells an unopened connection nothing about which kinds exist"
    );
}

#[test]
fn an_unknown_kind_on_an_open_gate_is_refused_by_name() {
    let mut gate = gate();

    gate.check(&good_hello()).expect("the Hello is accepted");

    assert_eq!(
        gate.refuse_unknown("Floating"),
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this Koshi has no request kind named Floating".to_string(),
        }
    );
}

#[test]
fn a_refused_hello_leaves_the_gate_closed() {
    let mut gate = gate();

    gate.check(&wrong_token_hello())
        .expect_err("the Hello is refused");

    assert_eq!(
        gate.check(&IpcRequestKind::Discovery),
        Err(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        })
    );
}

#[test]
fn a_good_hello_after_a_version_refusal_opens_the_gate() {
    let mut gate = gate();

    gate.check(&hello_speaking(ABOVE_RANGE, ABOVE_RANGE))
        .expect_err("the Hello is refused");
    gate.check(&good_hello()).expect("the Hello is accepted");

    assert_eq!(gate.check(&IpcRequestKind::Discovery), Ok(()));
    assert_eq!(gate.agreed(), Some(PROTOCOL_VERSION));
}

#[test]
fn a_good_hello_after_a_token_refusal_opens_the_gate() {
    let mut gate = gate();

    gate.check(&wrong_token_hello())
        .expect_err("the Hello is refused");
    gate.check(&good_hello()).expect("the Hello is accepted");

    assert_eq!(gate.check(&IpcRequestKind::Discovery), Ok(()));
}

#[test]
fn a_repeated_hello_on_an_open_gate_gets_the_same_answer() {
    let mut gate = gate();

    gate.check(&good_hello()).expect("the Hello is accepted");

    assert_eq!(gate.check(&good_hello()), Ok(()));
    assert_eq!(gate.agreed(), Some(PROTOCOL_VERSION));
}

#[test]
fn a_refused_hello_on_an_open_gate_leaves_it_open_and_keeps_its_version() {
    let mut gate = gate();

    gate.check(&good_hello()).expect("the Hello is accepted");
    gate.check(&wrong_token_hello())
        .expect_err("the Hello is refused");

    assert_eq!(gate.check(&IpcRequestKind::Discovery), Ok(()));
    assert_eq!(gate.agreed(), Some(PROTOCOL_VERSION));
}

#[test]
fn the_agreed_version_is_the_highest_both_sides_speak() {
    assert_eq!(
        agreed_version(2, 5, 2, 3),
        Some(3),
        "the caller reaches higher, so this build's highest wins"
    );
    assert_eq!(
        agreed_version(2, 3, 2, 5),
        Some(3),
        "this build reaches higher, so the caller's highest wins"
    );
    assert_eq!(agreed_version(4, 4, 4, 4), Some(4), "one shared version");
    assert_eq!(
        agreed_version(6, 7, 2, 5),
        None,
        "the caller is entirely above this build"
    );
    assert_eq!(
        agreed_version(1, 1, 2, 5),
        None,
        "the caller is entirely below this build"
    );
}

#[test]
fn a_hello_that_says_nothing_leaves_the_connection_local() {
    let mut gate = gate();

    assert_eq!(gate.check(&good_hello()), Ok(()));

    assert!(!gate.remote_caller());
}

#[test]
fn a_hello_saying_remote_marks_the_connection() {
    let mut gate = gate();

    assert_eq!(gate.check(&remote_hello()), Ok(()));

    assert!(gate.remote_caller());
}

#[test]
fn a_second_hello_cannot_clear_the_remote_mark() {
    let mut gate = gate();
    assert_eq!(gate.check(&remote_hello()), Ok(()));

    assert_eq!(gate.check(&good_hello()), Ok(()));

    assert!(
        gate.remote_caller(),
        "a later Hello saying local left the connection marked remote"
    );
}

#[test]
fn a_second_hello_can_still_set_the_remote_mark() {
    let mut gate = gate();
    assert_eq!(gate.check(&good_hello()), Ok(()));

    assert_eq!(gate.check(&remote_hello()), Ok(()));

    assert!(gate.remote_caller());
}

#[test]
fn a_refused_hello_saying_remote_does_not_mark_the_connection() {
    let mut gate = gate();
    let refused = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: ConnectionToken::new("wrongToken"),
        remote: true,
    };

    assert!(gate.check(&refused).is_err());

    assert!(!gate.remote_caller());
    assert_eq!(gate.check(&good_hello()), Ok(()));
    assert!(
        !gate.remote_caller(),
        "a Hello that never passed its token check marked the connection"
    );
}

#[test]
fn a_refused_hello_saying_remote_leaves_the_gate_closed() {
    let mut gate = gate();
    let refused = IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: ConnectionToken::new("wrongToken"),
        remote: true,
    };

    assert!(gate.check(&refused).is_err());

    assert_eq!(gate.agreed(), None);
}
