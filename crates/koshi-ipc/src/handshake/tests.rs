//! Tests for the connection handshake gate: a Hello with the right version
//! and token opens it, every refusal carries its exact code and message, and
//! a refusal never changes what the gate lets through.

use std::time::{Duration, UNIX_EPOCH};

use koshi_core::command::{Command, CommandEnvelope, CommandSource};
use koshi_core::ids::CommandId;

use super::*;

/// The token this Koshi expects, as the gate under test holds it.
fn expected() -> ConnectionToken {
    ConnectionToken::new("k7QxSecret")
}

/// A gate for a fresh connection, still closed.
fn gate() -> Handshake {
    Handshake::new(expected())
}

/// A Hello presenting the right version and the right token.
fn good_hello() -> IpcRequestKind {
    IpcRequestKind::Hello {
        protocol_version: PROTOCOL_VERSION,
        token: expected(),
    }
}

/// A Hello presenting the right version and a wrong token.
fn wrong_token_hello() -> IpcRequestKind {
    IpcRequestKind::Hello {
        protocol_version: PROTOCOL_VERSION,
        token: ConnectionToken::new("wrongToken"),
    }
}

/// A submit-command request carrying one command with no arguments.
fn submit_command() -> IpcRequestKind {
    IpcRequestKind::SubmitCommand(Box::new(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::ExternalCli { session_id: None },
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        Command::ToggleLockMode,
    )))
}

#[test]
fn a_hello_with_the_right_version_and_token_is_accepted() {
    assert_eq!(gate().check(&good_hello()), Ok(()));
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
fn a_hello_with_a_wrong_version_is_refused_naming_both_versions() {
    let hello = IpcRequestKind::Hello {
        protocol_version: 2,
        token: expected(),
    };

    assert_eq!(
        gate().check(&hello),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: "the caller speaks protocol version 2, this Koshi speaks 1".to_string(),
        })
    );
}

#[test]
fn a_hello_with_a_wrong_version_and_a_wrong_token_is_refused_for_the_version() {
    let hello = IpcRequestKind::Hello {
        protocol_version: 2,
        token: ConnectionToken::new("wrongToken"),
    };

    assert_eq!(
        gate().check(&hello),
        Err(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: "the caller speaks protocol version 2, this Koshi speaks 1".to_string(),
        })
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

    gate.check(&IpcRequestKind::Hello {
        protocol_version: 2,
        token: expected(),
    })
    .expect_err("the Hello is refused");
    gate.check(&good_hello()).expect("the Hello is accepted");

    assert_eq!(gate.check(&IpcRequestKind::Discovery), Ok(()));
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
}

#[test]
fn a_refused_hello_on_an_open_gate_leaves_it_open() {
    let mut gate = gate();

    gate.check(&good_hello()).expect("the Hello is accepted");
    gate.check(&wrong_token_hello())
        .expect_err("the Hello is refused");

    assert_eq!(gate.check(&IpcRequestKind::Discovery), Ok(()));
}
