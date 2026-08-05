//! Tests for the wire messages: every request and response variant survives a
//! round trip and keeps its own tag, a message carrying a field the build does
//! not know is refused, and the connection token neither prints nor compares
//! carelessly.

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use koshi_core::command::{Command, CommandSource, NewPaneArgs, ToggleLockModeArgs};
use koshi_core::discovery::{ClientInfo, PaneInfo, PaneState, SessionInfo, TabInfo};
use koshi_core::event::RejectReason;
use koshi_core::geometry::{Direction, Point, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{Key, ModFlags};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneKind;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;

use crate::attach::{PaneStructure, TabStructure};

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
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
}

/// An envelope carrying a `NewPane` with every optional field filled, at fixed
/// ids and times, so its encoding is byte-stable.
fn populated_envelope() -> CommandEnvelope {
    CommandEnvelope::new(
        CommandId::from_uuid(fixed_uuid()),
        CommandSource::InSessionCli {
            session_id: SessionId::from_uuid(fixed_uuid()),
            client_id: Some(ClientId::from_uuid(fixed_uuid())),
            pane_id: PaneId::from_uuid(fixed_uuid()),
            socket_path: PathBuf::from("/run/koshi.sock"),
        },
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        Command::NewPane(NewPaneArgs {
            source: Some(PaneId::from_uuid(fixed_uuid())),
            tab: Some(TabId::from_uuid(fixed_uuid())),
            direction: Direction::Down,
            stacked: true,
            cwd: Some(PathBuf::from("/home/user")),
            command: Some(SpawnSpec {
                program: PathBuf::from("/bin/zsh"),
                args: vec!["-l".to_string()],
                cwd: Some(PathBuf::from("/home/user")),
                env: BTreeMap::from([("KOSHI_PANE_ID".to_string(), "pane-1".to_string())]),
                shell_kind: ShellKind::Zsh,
            }),
            client: Some(ClientId::from_uuid(fixed_uuid())),
        }),
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

/// An overview of a session with one tab, one pane in it, and one attached
/// client, at fixed ids and times, so its encoding is byte-stable.
fn populated_overview() -> SessionOverview {
    let session_id = SessionId::from_uuid(fixed_uuid());
    let tab_id = TabId::from_uuid(fixed_uuid());
    let pane_id = PaneId::from_uuid(fixed_uuid());
    let client_id = ClientId::from_uuid(fixed_uuid());
    let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

    SessionOverview {
        session: SessionInfo {
            id: session_id,
            name: "quiet-lake".to_string(),
            created_at: at,
            attached_clients: vec![client_id],
            pane_count: 1,
        },
        tabs: vec![TabInfo {
            id: tab_id,
            session_id,
            name: "editor".to_string(),
            index: 0,
            active_pane: Some(pane_id),
            pane_count: 1,
        }],
        panes: vec![PaneInfo {
            id: pane_id,
            tab_id,
            session_id,
            title: Some("vim".to_string()),
            cwd: Some(PathBuf::from("/home/user")),
            command: None,
            state: PaneState::Running,
            focused_by_clients: vec![client_id],
        }],
        clients: vec![ClientInfo {
            id: client_id,
            session_id,
            attached_at: at,
            viewport_size: Size { cols: 80, rows: 24 },
            active_tab: tab_id,
            focused_pane: Some(pane_id),
            lock_state: LockMode::Normal,
        }],
    }
}

/// A session structure holding one tab and the one terminal pane in it, at
/// fixed ids, so its encoding is byte-stable.
fn populated_structure() -> AttachedSessionStructureSnapshot {
    let pane_id = PaneId::from_uuid(fixed_uuid());

    AttachedSessionStructureSnapshot {
        id: SessionId::from_uuid(fixed_uuid()),
        name: "quiet-lake".to_string(),
        tabs: vec![TabStructure {
            id: TabId::from_uuid(fixed_uuid()),
            name: "editor".to_string(),
            index: 0,
            layout: LayoutNode::Pane(pane_id),
            focus_mru: vec![pane_id],
        }],
        panes: vec![PaneStructure {
            id: pane_id,
            kind: PaneKind::Terminal,
        }],
    }
}

/// Every mouse action a round can carry, in the order the enum declares them,
/// at fixed ids.
fn every_mouse_action() -> Vec<WireMouseAction> {
    let pane = PaneId::from_uuid(fixed_uuid());

    vec![
        WireMouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        },
        WireMouseAction::Forward {
            pane,
            mouse: MouseInput {
                kind: MouseKind::Press(MouseButton::Left),
                at: Point { x: 10, y: 3 },
                mods: ModFlags::CTRL,
            },
        },
        WireMouseAction::AltScrollArrows {
            pane,
            up: false,
            count: 5,
        },
        WireMouseAction::Resize {
            pane,
            side: Direction::Left,
            step: -1,
            count: 2,
        },
        WireMouseAction::Command(Box::new(Command::ToggleLockMode(
            ToggleLockModeArgs::default(),
        ))),
    ]
}

/// The one UUID every fixed id above uses.
fn fixed_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// Encode `message` and decode it back.
fn round_trip<T: Serialize + DeserializeOwned>(message: &T) -> T {
    let encoded = serde_json::to_string(message).expect("message encodes");
    serde_json::from_str(&encoded).expect("message decodes")
}

/// The single tag an encoded enum variant carries, e.g. `"Overview"` for
/// `{"Overview": { … }}`.
fn tag_of(value: &serde_json::Value) -> String {
    let fields = value
        .as_object()
        .expect("a tagged variant encodes as an object");

    assert_eq!(fields.len(), 1, "expected exactly one tag in {value}");

    fields.keys().next().expect("one key").clone()
}

#[test]
fn the_protocol_version_this_build_speaks_is_two() {
    assert_eq!(PROTOCOL_VERSION, 2);
}

#[test]
fn the_overview_wire_shape_belongs_to_this_protocol_version() {
    // Every field of every struct a `Discovery` answer carries, pinned.
    //
    // Two builds only understand each other's bytes when they agree on this
    // shape, and the version in the Hello is the only thing that catches a
    // pair that does not. So a change here is a change to the wire: add,
    // remove, or rename anything below and `PROTOCOL_VERSION` goes up in the
    // same commit — otherwise a build at the old shape passes the handshake
    // and then fails to decode the answer, which reads to the user as a
    // session that is not running.
    //
    // Shape as of protocol version 2. Round-trip tests cannot catch this:
    // one build encoding and decoding its own structs always agrees with
    // itself.
    assert_eq!(
        serde_json::to_value(populated_overview()).expect("overview encodes"),
        json!({
            "session": {
                "id": "00000000-0000-0000-0000-000000000001",
                "name": "quiet-lake",
                "created_at": { "secs_since_epoch": 1_700_000_000, "nanos_since_epoch": 0 },
                "attached_clients": ["00000000-0000-0000-0000-000000000001"],
                "pane_count": 1
            },
            "tabs": [{
                "id": "00000000-0000-0000-0000-000000000001",
                "session_id": "00000000-0000-0000-0000-000000000001",
                "name": "editor",
                "index": 0,
                "active_pane": "00000000-0000-0000-0000-000000000001",
                "pane_count": 1
            }],
            "panes": [{
                "id": "00000000-0000-0000-0000-000000000001",
                "tab_id": "00000000-0000-0000-0000-000000000001",
                "session_id": "00000000-0000-0000-0000-000000000001",
                "title": "vim",
                "cwd": "/home/user",
                "command": null,
                "state": "running",
                "focused_by_clients": ["00000000-0000-0000-0000-000000000001"]
            }],
            "clients": [{
                "id": "00000000-0000-0000-0000-000000000001",
                "session_id": "00000000-0000-0000-0000-000000000001",
                "attached_at": { "secs_since_epoch": 1_700_000_000, "nanos_since_epoch": 0 },
                "viewport_size": { "cols": 80, "rows": 24 },
                "active_tab": "00000000-0000-0000-0000-000000000001",
                "focused_pane": "00000000-0000-0000-0000-000000000001",
                "lock_state": "Normal"
            }]
        })
    );
}

#[test]
fn the_submit_command_wire_shape_belongs_to_this_protocol_version() {
    // Every field of a command a CLI sends, pinned — the envelope, the source
    // it names, and the whole argument struct of the command inside it.
    //
    // The `Discovery` pin above covers only what a session ANSWERS. This one
    // covers what a caller SENDS, and the two travel opposite ways: a CLI at
    // the old shape passes the handshake and its command then fails to decode,
    // which reads to the user as a command that did nothing.
    //
    // So a change to `Command` or any `*Args` struct — add, remove, rename, or
    // retype a field — turns this red, and `PROTOCOL_VERSION` goes up in the
    // same commit. `direction` below is the worked example: it was
    // `Option<Direction>` and encoded `null` when unset; it is now a bare
    // `"Down"`, and a version-1 CLI's `null` no longer decodes.
    //
    // Shape as of protocol version 2. Round-trip tests cannot catch this: one
    // build encoding and decoding its own structs always agrees with itself.
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::SubmitCommand(Box::new(populated_envelope())),
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 2,
            "kind": {
                "SubmitCommand": {
                    "id": "00000000-0000-0000-0000-000000000001",
                    "source": {
                        "InSessionCli": {
                            "session_id": "00000000-0000-0000-0000-000000000001",
                            "client_id": "00000000-0000-0000-0000-000000000001",
                            "pane_id": "00000000-0000-0000-0000-000000000001",
                            "socket_path": "/run/koshi.sock"
                        }
                    },
                    "client_id": "00000000-0000-0000-0000-000000000001",
                    "issued_at": {
                        "secs_since_epoch": 1_700_000_000,
                        "nanos_since_epoch": 0
                    },
                    "command": {
                        "NewPane": {
                            "source": "00000000-0000-0000-0000-000000000001",
                            "tab": "00000000-0000-0000-0000-000000000001",
                            "direction": "Down",
                            "stacked": true,
                            "cwd": "/home/user",
                            "command": {
                                "program": "/bin/zsh",
                                "args": ["-l"],
                                "cwd": "/home/user",
                                "env": { "KOSHI_PANE_ID": "pane-1" },
                                "shell_kind": "Zsh"
                            },
                            "client": "00000000-0000-0000-0000-000000000001"
                        }
                    }
                }
            }
        })
    );
}

#[test]
fn the_attach_wire_shape_belongs_to_this_protocol_version() {
    // Both halves of the attach exchange, pinned: what a client SENDS to join
    // the session, and what the server ANSWERS.
    //
    // The answer is the one frame a client cannot recover from misreading: it
    // carries the ids the client names itself by afterwards and the structure
    // it draws its first frame from, and it arrives once. A client at the old
    // shape passes the handshake and then fails to decode this, which reads to
    // the user as a session that opens to a blank screen.
    //
    // So a change here — add, remove, rename, or retype anything below,
    // including inside `AttachedSessionStructureSnapshot` — turns this red,
    // and `PROTOCOL_VERSION` goes up in the same commit.
    //
    // Shape as of protocol version 2. Round-trip tests cannot catch this: one
    // build encoding and decoding its own structs always agrees with itself.
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
        },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 4,
            "kind": {
                "Attach": {
                    "viewport": { "cols": 80, "rows": 24 },
                    "filter": "All"
                }
            }
        })
    );

    let response = IpcResponse {
        request_id: Some(4),
        result: IpcResult::Attached {
            client_id: ClientId::from_uuid(fixed_uuid()),
            session_id: SessionId::from_uuid(fixed_uuid()),
            structure: populated_structure(),
        },
    };

    assert_eq!(
        serde_json::to_value(&response).expect("response encodes"),
        json!({
            "request_id": 4,
            "result": {
                "Attached": {
                    "client_id": "00000000-0000-0000-0000-000000000001",
                    "session_id": "00000000-0000-0000-0000-000000000001",
                    "structure": {
                        "id": "00000000-0000-0000-0000-000000000001",
                        "name": "quiet-lake",
                        "tabs": [{
                            "id": "00000000-0000-0000-0000-000000000001",
                            "name": "editor",
                            "index": 0,
                            "layout": { "Pane": "00000000-0000-0000-0000-000000000001" },
                            "focus_mru": ["00000000-0000-0000-0000-000000000001"]
                        }],
                        "panes": [{
                            "id": "00000000-0000-0000-0000-000000000001",
                            "kind": "Terminal"
                        }]
                    }
                }
            }
        })
    );
}

#[test]
fn an_overview_missing_a_field_this_version_needs_is_refused() {
    // What an older build's answer looks like here: its tab records carry no
    // `session_id`. Decoding must fail rather than fill in a default, so the
    // mismatch surfaces instead of producing tab rows that claim to belong to
    // no session.
    let mut encoded = serde_json::to_value(populated_overview()).expect("overview encodes");
    encoded["tabs"][0]
        .as_object_mut()
        .expect("a tab encodes as an object")
        .remove("session_id");

    let decoded: Result<SessionOverview, _> = serde_json::from_value(encoded);
    let error = decoded.expect_err("a tab without its session is not this version's shape");
    assert!(
        error.to_string().contains("missing field `session_id`"),
        "unexpected error: {error}"
    );
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
fn attach_request_round_trips() {
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn attached_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(4),
        result: IpcResult::Attached {
            client_id: ClientId::new(),
            session_id: SessionId::new(),
            structure: populated_structure(),
        },
    };

    assert_eq!(round_trip(&response), response);
}

#[test]
fn an_attach_naming_its_own_authority_is_refused() {
    // A caller cannot say what it is allowed to do: the server decides that
    // from the connection it arrived on. An `Attach` carrying a `tier` is not
    // this build's shape, so it fails to decode and is answered on the
    // malformed-request path — the field never reaches code that would have to
    // remember to ignore it.
    let with_tier: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":4,"kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"All","tier":"admin"}}}"#,
    );

    let error = with_tier.expect_err("an authority field decoded instead of failing");
    assert!(
        error.to_string().contains("unknown field `tier`"),
        "unexpected error: {error}"
    );

    // The same frame without that one field decodes, so the refusal above is
    // the field's doing and not a typo elsewhere in the bytes.
    let without_tier: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"All"}}}"#,
    )
    .expect("the same attach without the extra field decodes");

    assert_eq!(
        without_tier,
        IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::Attach {
                viewport: Size { cols: 80, rows: 24 },
                filter: EventFilterSpec::All,
            },
        }
    );
}

#[test]
fn key_press_request_round_trips() {
    let request = IpcRequest {
        request_id: 5,
        kind: IpcRequestKind::KeyPress {
            chord: KeyChord::new(ModFlags::CTRL, Key::Char('c')),
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn resize_request_round_trips() {
    let request = IpcRequest {
        request_id: 6,
        kind: IpcRequestKind::Resize {
            viewport: Size {
                cols: 120,
                rows: 40,
            },
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn every_mouse_action_round_trips() {
    for action in every_mouse_action() {
        assert_eq!(round_trip(&action), action);
    }
}

#[test]
fn a_mouse_request_keeps_its_round_in_the_order_it_was_sent() {
    // Three actions that differ from one another, so any reordering — not just
    // a lost element — turns this red. The session runs them in this order.
    let pane = PaneId::from_uuid(fixed_uuid());
    let request = IpcRequest {
        request_id: 7,
        kind: IpcRequestKind::Mouse(vec![
            WireMouseAction::Scroll {
                pane,
                up: true,
                lines: 3,
            },
            WireMouseAction::Command(Box::new(Command::ToggleLockMode(
                ToggleLockModeArgs::default(),
            ))),
            WireMouseAction::Resize {
                pane,
                side: Direction::Left,
                step: -1,
                count: 2,
            },
        ]),
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn a_mouse_action_carrying_an_unknown_field_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":7,"kind":{"Mouse":[{"Scroll":{"pane":"00000000-0000-0000-0000-000000000001","up":true,"lines":3,"pixels":9}}]}}"#,
    );

    let error =
        decoded.expect_err("an unknown field inside a mouse action decoded instead of failing");
    assert!(
        error.to_string().contains("unknown field `pixels`"),
        "unexpected error: {error}"
    );

    // The same round without that one field decodes, so the refusal above is
    // the field's doing and not a typo elsewhere in the bytes.
    let without_it: IpcRequest = serde_json::from_str(
        r#"{"request_id":7,"kind":{"Mouse":[{"Scroll":{"pane":"00000000-0000-0000-0000-000000000001","up":true,"lines":3}}]}}"#,
    )
    .expect("the same round without the extra field decodes");

    assert_eq!(
        without_it,
        IpcRequest {
            request_id: 7,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane: PaneId::from_uuid(fixed_uuid()),
                up: true,
                lines: 3,
            }]),
        }
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
fn hello_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(1),
        result: IpcResult::Hello,
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

/// A caller branches on the refusal code, so each one keeps its own wire
/// spelling: a rejected token reads `bad_token`, never the name of another
/// refusal.
#[test]
fn every_refusal_code_encodes_to_its_own_wire_name() {
    // The match is exhaustive, so a refusal code added later does not compile
    // until its wire name is written here.
    let wire_name = |code: IpcErrorCode| match code {
        IpcErrorCode::BadToken => "bad_token",
        IpcErrorCode::UnsupportedVersion => "unsupported_version",
        IpcErrorCode::MalformedRequest => "malformed_request",
        IpcErrorCode::HelloRequired => "hello_required",
    };

    for code in [
        IpcErrorCode::BadToken,
        IpcErrorCode::UnsupportedVersion,
        IpcErrorCode::MalformedRequest,
        IpcErrorCode::HelloRequired,
    ] {
        assert_eq!(
            serde_json::to_value(code).expect("code encodes"),
            json!(wire_name(code)),
        );
    }
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
fn each_request_kind_is_tagged_with_its_own_name() {
    assert_eq!(
        tag_of(
            &serde_json::to_value(IpcRequestKind::Attach {
                viewport: Size { cols: 80, rows: 24 },
                filter: EventFilterSpec::All,
            })
            .unwrap()
        ),
        "Attach"
    );
    assert_eq!(
        tag_of(&serde_json::to_value(IpcRequestKind::SubmitCommand(Box::new(envelope()))).unwrap()),
        "SubmitCommand"
    );
    assert_eq!(
        serde_json::to_value(IpcRequestKind::Discovery).unwrap(),
        json!("Discovery")
    );
}

#[test]
fn each_result_is_tagged_with_its_own_name() {
    assert_eq!(
        serde_json::to_value(IpcResult::Hello).unwrap(),
        json!("Hello")
    );
    assert_eq!(
        tag_of(
            &serde_json::to_value(IpcResult::Attached {
                client_id: ClientId::new(),
                session_id: SessionId::new(),
                structure: populated_structure(),
            })
            .unwrap()
        ),
        "Attached"
    );
    assert_eq!(
        tag_of(
            &serde_json::to_value(IpcResult::CommandResult(CommandResult::Ok {
                command_id: CommandId::new(),
                emitted_events: Vec::new(),
            }))
            .unwrap()
        ),
        "CommandResult"
    );
    assert_eq!(
        tag_of(&serde_json::to_value(IpcResult::Overview(overview())).unwrap()),
        "Overview"
    );
    assert_eq!(
        tag_of(
            &serde_json::to_value(IpcResult::Error(IpcErrorPayload {
                code: IpcErrorCode::BadToken,
                message: "the token does not match".to_string(),
            }))
            .unwrap()
        ),
        "Error"
    );
}

#[test]
fn a_response_with_a_misspelled_request_id_is_refused() {
    let decoded: Result<IpcResponse, _> =
        serde_json::from_str(r#"{"requst_id":7,"result":"Hello"}"#);

    assert!(
        decoded.is_err(),
        "a misspelled field decoded instead of failing: {decoded:?}"
    );
}

#[test]
fn a_request_carrying_an_unknown_field_is_refused() {
    let decoded: Result<IpcRequest, _> =
        serde_json::from_str(r#"{"request_id":1,"kind":"Discovery","junk":5}"#);

    assert!(
        decoded.is_err(),
        "an unknown field decoded instead of failing: {decoded:?}"
    );
}

#[test]
fn a_hello_carrying_an_unknown_field_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":1,"kind":{"Hello":{"protocol_version":1,"token":"k7QxSecret","junk":5}}}"#,
    );

    assert!(
        decoded.is_err(),
        "an unknown field inside Hello decoded instead of failing: {decoded:?}"
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
fn every_request_kind_names_itself_without_its_payload() {
    assert_eq!(
        IpcRequestKind::Hello {
            protocol_version: 1,
            token: token(),
        }
        .name(),
        "Hello"
    );
    assert_eq!(
        IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
        }
        .name(),
        "Attach"
    );
    assert_eq!(
        IpcRequestKind::KeyPress {
            chord: KeyChord::new(ModFlags::CTRL, Key::Char('c')),
        }
        .name(),
        "KeyPress"
    );
    assert_eq!(
        IpcRequestKind::Resize {
            viewport: Size {
                cols: 120,
                rows: 40,
            },
        }
        .name(),
        "Resize"
    );
    assert_eq!(
        IpcRequestKind::Paste {
            text: String::from("hello\nworld"),
        }
        .name(),
        "Paste"
    );
    assert_eq!(IpcRequestKind::Mouse(every_mouse_action()).name(), "Mouse");
    assert_eq!(
        IpcRequestKind::SubmitCommand(Box::new(envelope())).name(),
        "SubmitCommand"
    );
    assert_eq!(IpcRequestKind::Discovery.name(), "Discovery");
}

/// Serializing is how the token reaches the endpoint file and the socket, so
/// it writes the real secret. Redacting here would break both.
#[test]
fn serializing_a_hello_writes_the_real_secret() {
    let request = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            protocol_version: 1,
            token: token(),
        },
    };

    let encoded = serde_json::to_string(&request).expect("request encodes");

    assert!(encoded.contains("k7QxSecret"), "{encoded}");
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

#[test]
fn a_generated_token_is_64_lowercase_hex_characters() {
    let token = ConnectionToken::generate();
    let secret = token.expose();
    assert_eq!(secret.len(), 64, "{secret}");
    assert!(
        secret
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{secret}"
    );
}

#[test]
fn two_generated_tokens_differ() {
    assert_ne!(ConnectionToken::generate(), ConnectionToken::generate());
}
