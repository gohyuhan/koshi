//! Tests for the connection handshake gate: a Hello whose version range
//! overlaps this build's and whose token matches opens it, the settled version
//! is the highest both sides speak, every refusal carries its exact code and
//! message, and a refusal never changes what the gate lets through.

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

/// A gate for a fresh connection, still closed.
fn gate() -> Handshake {
    Handshake::new(expected())
}

/// A Hello speaking `min` to `max` and presenting the right token.
fn hello_speaking(min: u32, max: u32) -> IpcRequestKind {
    IpcRequestKind::Hello {
        min_protocol_version: min,
        max_protocol_version: max,
        token: expected(),
    }
}

/// A Hello speaking exactly this build's range, with the right token.
fn good_hello() -> IpcRequestKind {
    hello_speaking(MIN_PROTOCOL_VERSION, PROTOCOL_VERSION)
}

/// A Hello speaking this build's range and presenting a wrong token.
fn wrong_token_hello() -> IpcRequestKind {
    IpcRequestKind::Hello {
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: ConnectionToken::new("wrongToken"),
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
