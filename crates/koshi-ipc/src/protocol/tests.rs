//! Tests for the wire messages: every request and response variant survives a
//! round trip, and the connection token neither prints nor compares carelessly.

use std::time::{Duration, UNIX_EPOCH};

use koshi_core::command::{Command, CommandSource};
use koshi_core::discovery::SessionInfo;
use koshi_core::event::RejectReason;
use koshi_core::ids::{CommandId, SessionId};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

use super::*;

/// A token holding a fixed secret.
fn token() -> ConnectionToken {
    ConnectionToken::new("k7QxSecret")
}

/// An envelope carrying one command with no arguments.
fn envelope() -> CommandEnvelope {
    CommandEnvelope::new(
        CommandId::new(),
        CommandSource::ExternalCli { session_id: None },
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        Command::ToggleLockMode,
    )
}

/// An overview of a session with no tabs, panes, or clients.
fn overview() -> SessionOverview {
    SessionOverview {
        session: SessionInfo {
            id: SessionId::new(),
            name: "quiet-lake".to_string(),
            created_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            attached_clients: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    }
}

/// Encode `message` and decode it back.
fn round_trip<T: Serialize + DeserializeOwned>(message: &T) -> T {
    let encoded = serde_json::to_string(message).expect("message encodes");
    serde_json::from_str(&encoded).expect("message decodes")
}

#[test]
fn hello_request_round_trips() {
    let request = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: token(),
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn hello_request_encodes_to_the_expected_shape() {
    let request = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            protocol_version: 1,
            token: token(),
        },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 1,
            "kind": { "Hello": { "protocol_version": 1, "token": "k7QxSecret" } }
        })
    );
}

#[test]
fn submit_command_request_round_trips() {
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::SubmitCommand(Box::new(envelope())),
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn discovery_request_round_trips() {
    let request = IpcRequest {
        request_id: 3,
        kind: IpcRequestKind::Discovery,
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn discovery_request_encodes_to_the_expected_shape() {
    let request = IpcRequest {
        request_id: 3,
        kind: IpcRequestKind::Discovery,
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 3, "kind": "Discovery" })
    );
}

#[test]
fn accepted_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(1),
        result: IpcResult::Accepted,
    };

    assert_eq!(round_trip(&response), response);
}

#[test]
fn applied_command_result_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(2),
        result: IpcResult::CommandResult(CommandResult::Ok {
            command_id: CommandId::new(),
            emitted_events: Vec::new(),
        }),
    };

    assert_eq!(round_trip(&response), response);
}

#[test]
fn rejected_command_result_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(2),
        result: IpcResult::CommandResult(CommandResult::Rejected {
            command_id: CommandId::new(),
            reason: RejectReason::TargetNotFound,
            help: Some("name a session with --session".to_string()),
        }),
    };

    assert_eq!(round_trip(&response), response);
}

#[test]
fn overview_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(3),
        result: IpcResult::Overview(overview()),
    };

    assert_eq!(round_trip(&response), response);
}

#[test]
fn error_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(1),
        result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: "this Koshi speaks protocol 1, the caller speaks 2".to_string(),
        }),
    };

    assert_eq!(round_trip(&response), response);
}

#[test]
fn error_response_encodes_its_code_in_snake_case() {
    let response = IpcResponse {
        request_id: Some(4),
        result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "open the connection first".to_string(),
        }),
    };

    assert_eq!(
        serde_json::to_value(&response).expect("response encodes"),
        json!({
            "request_id": 4,
            "result": {
                "Error": { "code": "hello_required", "message": "open the connection first" }
            }
        })
    );
}

#[test]
fn a_response_to_unreadable_bytes_names_no_request() {
    let response = IpcResponse {
        request_id: None,
        result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the request could not be read".to_string(),
        }),
    };

    assert_eq!(round_trip(&response), response);
    assert_eq!(
        serde_json::to_value(&response).expect("response encodes")["request_id"],
        json!(null)
    );
}

#[test]
fn token_encodes_as_a_bare_string() {
    assert_eq!(
        serde_json::to_value(token()).expect("token encodes"),
        json!("k7QxSecret")
    );
}

#[test]
fn token_debug_hides_the_secret() {
    assert_eq!(format!("{:?}", token()), "ConnectionToken(***)");
}

#[test]
fn token_display_hides_the_secret() {
    assert_eq!(token().to_string(), "***");
}

#[test]
fn nesting_a_token_in_a_request_keeps_it_out_of_debug_output() {
    let request = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            protocol_version: 1,
            token: token(),
        },
    };

    let printed = format!("{request:?}");

    assert!(
        !printed.contains("k7QxSecret"),
        "the secret reached debug output: {printed}"
    );
    assert!(printed.contains("ConnectionToken(***)"), "{printed}");
}

#[test]
fn tokens_holding_the_same_secret_are_equal() {
    assert_eq!(ConnectionToken::new("k7QxSecret"), token());
}

#[test]
fn tokens_differing_in_one_byte_are_not_equal() {
    assert_ne!(ConnectionToken::new("k7QxSecreT"), token());
}

#[test]
fn tokens_differing_in_the_first_byte_are_not_equal() {
    assert_ne!(ConnectionToken::new("K7QxSecret"), token());
}

#[test]
fn a_token_that_is_a_prefix_of_another_is_not_equal() {
    assert_ne!(ConnectionToken::new("k7QxSecre"), token());
}

#[test]
fn an_empty_token_is_not_equal_to_a_real_one() {
    assert_ne!(ConnectionToken::new(""), token());
}

#[test]
fn expose_returns_the_secret_for_writing_it_to_the_endpoint_file() {
    assert_eq!(token().expose(), "k7QxSecret");
}
