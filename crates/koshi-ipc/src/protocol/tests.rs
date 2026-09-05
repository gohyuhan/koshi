//! Tests for the wire messages: every request and response variant survives a
//! round trip and keeps its own tag, an unknown field is refused on the
//! envelope and ignored on the payload, and the connection token prints as
//! `***` and is equal only to a token holding the same bytes.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koshi_core::client::ClientOrigin;

use koshi_core::command::{Command, CommandSource, NewPaneArgs, ToggleLockModeArgs};
use koshi_core::discovery::{ClientInfo, PaneInfo, PaneState, SessionInfo, TabInfo};
use koshi_core::event::RejectReason;
use koshi_core::geometry::{Direction, PaneArea, Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{Key, ModFlags};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneKind;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

use crate::attach::{PaneStructure, TabStructure};
use crate::layout::{ClientFocus, SolvedPane, SolvedTab, TabLayout};
use crate::plane::Plane;
use crate::router::RouterRequestKind;
use crate::wire::{MaybeKnown, WireName, WireVariants};

use super::*;

/// A token holding a fixed secret.
fn token() -> ConnectionToken {
    ConnectionToken::new("k7QxSecret")
}

/// An envelope carrying one command with no arguments.
fn envelope() -> CommandEnvelope {
    CommandEnvelope::new(
        CommandId::new(),
        CommandSource::ExternalCli {
            session_id: None,
            target_client: None,
        },
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
}

/// An envelope carrying a `NewPane` with every optional field filled, at fixed
/// ids and times. Encodes to the same bytes on every call.
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

/// One remembered event: a pane created in a tab, stamped at the epoch.
fn recent() -> RecentEvent {
    koshi_core::recent_event::record(
        &koshi_core::event::Event::PaneCreated(koshi_core::event::PaneCreated {
            pane_id: PaneId::from_uuid(fixed_uuid()),
            tab_id: TabId::from_uuid(fixed_uuid()),
        }),
        std::time::SystemTime::UNIX_EPOCH,
    )
}

/// The layout of a session with one tab, holding one pane, that one client
/// views. The wire form of every field is pinned in `crate::layout::tests`.
fn layout() -> SessionLayout {
    let tab_id = TabId::from_uuid(fixed_uuid());
    let pane_id = PaneId::from_uuid(fixed_uuid());
    let client_id = ClientId::from_uuid(fixed_uuid());

    SessionLayout {
        id: SessionId::from_uuid(fixed_uuid()),
        name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            id: tab_id,
            name: "editor".to_string(),
            index: 0,
            tree: LayoutNode::Pane(pane_id),
            solved: vec![SolvedTab {
                client: client_id,
                viewport: Size { cols: 80, rows: 22 },
                mode: LayoutMode::Tiled,
                panes: vec![SolvedPane {
                    id: pane_id,
                    rect: Rect::at_origin(Size { cols: 80, rows: 22 }),
                }],
                suppressed: Vec::new(),
                all_suppressed: false,
                stack_headers: Vec::new(),
            }],
        }],
        clients: vec![ClientFocus {
            id: client_id,
            active_tab: tab_id,
            focused_pane: Some(pane_id),
        }],
    }
}

/// An overview of a session with one tab, one pane in it, and one attached
/// client, at fixed ids and times. Encodes to the same bytes on every call.
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
            origin: Some(ClientOrigin::Local),
            pane_area: None,
        }],
    }
}

/// A session structure holding one tab and the one terminal pane in it, at
/// fixed ids. Encodes to the same bytes on every call.
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

/// One request of every kind, in the order the enum declares them.
fn every_request_kind() -> Vec<IpcRequestKind> {
    vec![
        IpcRequestKind::hello(token()),
        IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: None,
            graphics: crate::protocol::GraphicsCapabilities::default(),
        },
        IpcRequestKind::KeyPress {
            chord: KeyChord::new(ModFlags::CTRL, Key::Char('c')),
        },
        IpcRequestKind::Resize {
            viewport: Size {
                cols: 120,
                rows: 40,
            },
            pane_area: None,
        },
        IpcRequestKind::Paste {
            text: "hello\nworld".to_string(),
        },
        IpcRequestKind::Mouse(every_mouse_action()),
        IpcRequestKind::SubmitCommand(Box::new(envelope())),
        IpcRequestKind::Discovery,
        IpcRequestKind::Layout { tab: None },
        IpcRequestKind::RecentEvents,
        IpcRequestKind::Restart,
        IpcRequestKind::Leaving,
    ]
}

/// One answer of every kind, in the order the enum declares them.
fn every_result() -> Vec<IpcResult> {
    vec![
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            version: "0.3.0".to_string(),
        },
        IpcResult::Attached {
            client_id: ClientId::from_uuid(fixed_uuid()),
            session_id: SessionId::from_uuid(fixed_uuid()),
            structure: populated_structure(),
            resume_token: None,
            pane_area: None,
        },
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: CommandId::from_uuid(fixed_uuid()),
            emitted_events: Vec::new(),
        }),
        IpcResult::Overview(overview()),
        IpcResult::Layout(layout()),
        IpcResult::RecentEvents(vec![recent()]),
        IpcResult::Restarting,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token does not match".to_string(),
        }),
    ]
}

/// The one UUID every fixed id in this file uses.
fn fixed_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// Encode `message` and decode it back.
fn round_trip<T: Serialize + DeserializeOwned>(message: &T) -> T {
    let encoded = serde_json::to_string(message).expect("message encodes");
    serde_json::from_str(&encoded).expect("message decodes")
}

/// The tag an encoded enum variant carries: the single key of
/// `{"Overview": { … }}`, or the string itself for `"Restarting"`.
fn tag_of(value: &serde_json::Value) -> String {
    if let Some(name) = value.as_str() {
        return name.to_string();
    }

    let fields = value
        .as_object()
        .expect("a tagged variant encodes as a string or an object");

    assert_eq!(fields.len(), 1, "expected exactly one tag in {value}");

    fields.keys().next().expect("one key").clone()
}

#[test]
fn the_protocol_version_this_build_speaks_is_three() {
    assert_eq!(PROTOCOL_VERSION, 3);
}

#[test]
fn the_lowest_protocol_version_this_build_speaks_is_three() {
    assert_eq!(MIN_PROTOCOL_VERSION, 3);
}

#[test]
fn agreed_version_is_the_highest_version_both_ranges_hold() {
    assert_eq!(agreed_version(2, 4, 2, 2), Some(2));
    assert_eq!(agreed_version(2, 3, 2, 3), Some(3));
    assert_eq!(agreed_version(5, 6, 2, 2), None);
}

#[test]
fn agreed_version_settles_where_the_ranges_touch_at_one_version() {
    assert_eq!(agreed_version(1, 2, 2, 5), Some(2));
    assert_eq!(agreed_version(2, 5, 1, 2), Some(2));
}

#[test]
fn agreed_version_of_an_inverted_range_is_none() {
    assert_eq!(agreed_version(5, 2, 2, 5), None);
    assert_eq!(agreed_version(2, 5, 5, 2), None);
}

#[test]
fn the_session_plane_answers_a_refusal_as_an_error_result() {
    let payload = IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token does not match".to_string(),
    };

    assert_eq!(
        SessionPlane::refusal(payload.clone()),
        IpcResult::Error(payload)
    );
}

#[test]
fn the_session_plane_answers_a_hello_with_the_agreed_version_and_the_build() {
    assert_eq!(
        SessionPlane::hello(2, "0.3.0"),
        IpcResult::Hello {
            protocol_version: 2,
            version: "0.3.0".to_string(),
        }
    );
}

#[test]
fn the_overview_wire_shape_belongs_to_this_protocol_version() {
    // Every field of every struct a `Discovery` answer carries, as this build
    // writes it. A field renamed, retyped or repurposed changes these bytes
    // and moves `PROTOCOL_VERSION` in the same commit. A field added or
    // removed that both shapes still decode leaves the number in place, the
    // cadence rule in `koshi_core::compat`;
    // `a_client_row_decodes_across_the_shape_that_added_origin` pins the
    // decoding half of that.
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
                "lock_state": "Normal",
                "origin": "Local",
                "pane_area": null
            }]
        })
    );
}

#[test]
fn the_plane_a_remote_client_reaches_names_no_token_verb() {
    // `GrantToken`, `RevokeToken` and `ListTokens` are request kinds of the
    // router's plane and of no other. A remote client speaks the session
    // plane only.
    for verb in ["GrantToken", "RevokeToken", "ListTokens"] {
        assert!(
            RouterRequestKind::VARIANTS.contains(&verb),
            "{verb} is a control-plane verb"
        );
        assert!(
            !IpcRequestKind::VARIANTS.contains(&verb),
            "{verb} must stay off the plane a remote client speaks"
        );
    }
}

#[test]
fn a_client_row_decodes_across_the_shape_that_added_origin() {
    // A client row written without `origin` decodes with `origin: None`.
    let without_origin = json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "session_id": "00000000-0000-0000-0000-000000000001",
        "attached_at": { "secs_since_epoch": 1_700_000_000, "nanos_since_epoch": 0 },
        "viewport_size": { "cols": 80, "rows": 24 },
        "active_tab": "00000000-0000-0000-0000-000000000001",
        "focused_pane": null,
        "lock_state": "Normal"
    });
    let decoded: ClientInfo =
        serde_json::from_value(without_origin).expect("a row from a build without origin decodes");
    assert_eq!(
        decoded.origin, None,
        "a build that names no origin answered the question with nothing"
    );

    // The other direction: a row this build writes, read by a shape that has
    // no `origin` field. `OldClientInfo` is that shape.
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct OldClientInfo {
        id: ClientId,
        session_id: SessionId,
        attached_at: SystemTime,
        viewport_size: Size,
        active_tab: TabId,
        focused_pane: Option<PaneId>,
        lock_state: LockMode,
    }
    let mut written = populated_overview().clients.remove(0);
    written.origin = Some(ClientOrigin::Remote);
    let written = serde_json::to_value(written).expect("a client row encodes");
    let old: OldClientInfo =
        serde_json::from_value(written).expect("the older shape reads a row carrying origin");
    assert_eq!(old.lock_state, LockMode::Normal);
}

#[test]
fn the_submit_command_wire_shape_belongs_to_this_protocol_version() {
    // Every field of a command a CLI sends, as this build writes it: the
    // envelope, the source it names, and the whole argument struct of the
    // command inside it. Any field of `Command` or of an `*Args` struct that
    // is added, removed, renamed or retyped changes these bytes. A change an
    // older peer cannot decode, a rename or a retype among them, also moves
    // `PROTOCOL_VERSION` in the same commit, the cadence rule in
    // `koshi_core::compat`.
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
    // Both halves of the attach exchange, as this build writes them: what a
    // client sends to join the session, and what the server answers. Any
    // field added, removed, renamed or retyped below, inside
    // `AttachedSessionStructureSnapshot` included, changes these bytes. A
    // rename or retype also moves `PROTOCOL_VERSION` in the same commit; a
    // field added with `#[serde(default)]`, which an older peer decodes by
    // taking the default, does not.
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: None,
            graphics: crate::protocol::GraphicsCapabilities::default(),
        },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 4,
            "kind": {
                "Attach": {
                    "viewport": { "cols": 80, "rows": 24 },
                    "filter": "All",
                    "resume": null,
                    "resume_token": null,
                    "pane_area": null
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
            resume_token: None,
            pane_area: None,
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
                    },
                    "resume_token": null,
                    "pane_area": null
                }
            }
        })
    );
}

#[test]
fn attach_reports_positive_kitty_support_and_defaults_an_absent_report_to_false() {
    let supported = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: None,
            graphics: crate::protocol::GraphicsCapabilities { kitty: true },
        },
    };
    assert_eq!(
        serde_json::to_value(&supported).expect("the capability report encodes"),
        json!({
            "request_id": 4,
            "kind": {
                "Attach": {
                    "viewport": { "cols": 80, "rows": 24 },
                    "filter": "All",
                    "resume": null,
                    "resume_token": null,
                    "pane_area": null,
                    "graphics": { "kitty": true }
                }
            }
        })
    );

    let absent: IpcRequest = serde_json::from_value(json!({
        "request_id": 4,
        "kind": {
            "Attach": {
                "viewport": { "cols": 80, "rows": 24 },
                "filter": "All",
                "resume": null,
                "resume_token": null,
                "pane_area": null
            }
        }
    }))
    .expect("an attach without a graphics report decodes");
    assert_eq!(
        absent,
        IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::Attach {
                viewport: Size { cols: 80, rows: 24 },
                filter: EventFilterSpec::All,
                resume: None,
                resume_token: None,
                pane_area: None,
                graphics: crate::protocol::GraphicsCapabilities { kitty: false },
            },
        }
    );
}

#[test]
fn an_overview_missing_a_field_this_version_needs_is_refused() {
    // A tab record without `session_id` fails to decode; no default fills it
    // in.
    let mut encoded = serde_json::to_value(populated_overview()).expect("overview encodes");
    encoded["tabs"][0]
        .as_object_mut()
        .expect("a tab encodes as an object")
        .remove("session_id");

    let decoded: Result<SessionOverview, _> = serde_json::from_value(encoded);
    let error = decoded.expect_err("a tab without its session is not this version's shape");
    assert_eq!(error.to_string(), "missing field `session_id`");
}

#[test]
fn hello_request_round_trips() {
    let request = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: token(),
            remote: false,
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn hello_request_encodes_to_the_expected_shape() {
    let request = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 2,
            token: token(),
            remote: false,
        },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 1,
            "kind": {
                "Hello": {
                    "min_protocol_version": 1,
                    "max_protocol_version": 2,
                    "token": "k7QxSecret",
                    "remote": false
                }
            }
        })
    );
}

#[test]
fn a_hello_marking_a_remote_caller_round_trips_and_encodes_true() {
    let request = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: token(),
            remote: true,
        },
    };

    assert_eq!(round_trip(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes")["kind"]["Hello"]["remote"],
        json!(true)
    );
}

#[test]
fn a_hello_whose_token_is_not_a_string_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":1,"kind":{"Hello":{"min_protocol_version":2,"max_protocol_version":2,"token":5}}}"#,
    );

    let error = decoded.expect_err("a number where the token goes decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "invalid type: integer `5`, expected a string at line 1 column 92"
    );
}

#[test]
fn attach_request_round_trips() {
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: None,
            graphics: crate::protocol::GraphicsCapabilities::default(),
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn an_attach_request_naming_a_client_to_come_back_as_round_trips() {
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
            resume: Some(ClientId::from_uuid(fixed_uuid())),
            resume_token: None,
            pane_area: None,
            graphics: crate::protocol::GraphicsCapabilities::default(),
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn an_attach_request_written_without_the_resume_fields_decodes_as_no_claim() {
    // An attach written without `resume` and `resume_token` decodes with both
    // `None`.
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"All"}}}"#,
    )
    .expect("an attach without the resume fields decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::Attach {
                viewport: Size { cols: 80, rows: 24 },
                filter: EventFilterSpec::All,
                resume: None,
                resume_token: None,
                pane_area: None,
                graphics: crate::protocol::GraphicsCapabilities::default(),
            },
        }
    );
}

#[test]
fn an_attach_request_carrying_a_resume_token_keeps_the_secret_whole() {
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
            resume: Some(ClientId::from_uuid(fixed_uuid())),
            resume_token: Some(token()),
            pane_area: None,
            graphics: crate::protocol::GraphicsCapabilities::default(),
        },
    };

    let IpcRequestKind::Attach {
        resume_token: Some(carried),
        ..
    } = round_trip(&request).kind
    else {
        panic!("an attach carrying a resume token decodes as one");
    };

    assert_eq!(carried.expose(), token().expose());
}

#[test]
fn an_attach_request_written_without_a_resume_token_beside_a_resume_decodes_as_no_token() {
    // An attach written with `resume` and without `resume_token` decodes with
    // `resume_token: None`.
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"All","resume":"00000000-0000-0000-0000-000000000001"}}}"#,
    )
    .expect("an attach without the resume token field decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::Attach {
                viewport: Size { cols: 80, rows: 24 },
                filter: EventFilterSpec::All,
                resume: Some(ClientId::from_uuid(fixed_uuid())),
                resume_token: None,
                pane_area: None,
                graphics: crate::protocol::GraphicsCapabilities::default(),
            },
        }
    );
}

#[test]
fn an_attach_request_written_without_a_pane_area_decodes_as_none() {
    // An attach written without `pane_area` decodes with `pane_area: None`.
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":1,"kind":{"Attach":{"viewport":{"cols":120,"rows":40},"filter":"All","resume":null,"resume_token":null}}}"#,
    )
    .expect("an attach without the pane area field decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 1,
            kind: IpcRequestKind::Attach {
                viewport: Size {
                    cols: 120,
                    rows: 40,
                },
                filter: EventFilterSpec::All,
                resume: None,
                resume_token: None,
                pane_area: None,
                graphics: crate::protocol::GraphicsCapabilities::default(),
            },
        }
    );
}

#[test]
fn an_attach_naming_an_unknown_pane_area_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":1,"kind":{"Attach":{"viewport":{"cols":120,"rows":40},"filter":"All","pane_area":"Bogus"}}}"#,
    );

    let error = decoded.expect_err("an unknown pane area decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown variant `Bogus`, expected `Reported` or `Starving` at line 1 column 102"
    );
}

#[test]
fn an_attach_naming_an_unknown_filter_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":4,"kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"Some"}}}"#,
    );

    let error = decoded.expect_err("an unknown filter decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown variant `Some`, expected `All` at line 1 column 82"
    );
}

#[test]
fn an_attach_request_reporting_a_pane_area_round_trips() {
    let reported = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Attach {
            viewport: Size {
                cols: 120,
                rows: 40,
            },
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: Some(PaneArea::Reported(Size {
                cols: 100,
                rows: 30,
            })),
            graphics: crate::protocol::GraphicsCapabilities::default(),
        },
    };

    assert_eq!(
        serde_json::to_value(&reported).expect("the attach encodes"),
        json!({
            "request_id": 1,
            "kind": {
                "Attach": {
                    "viewport": { "cols": 120, "rows": 40 },
                    "filter": "All",
                    "resume": null,
                    "resume_token": null,
                    "pane_area": { "Reported": { "cols": 100, "rows": 30 } }
                }
            }
        })
    );
    assert_eq!(round_trip(&reported), reported);

    let starving = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Attach {
            viewport: Size {
                cols: 120,
                rows: 40,
            },
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: Some(PaneArea::Starving),
            graphics: crate::protocol::GraphicsCapabilities::default(),
        },
    };

    assert_eq!(
        serde_json::to_value(&starving).expect("the attach encodes"),
        json!({
            "request_id": 1,
            "kind": {
                "Attach": {
                    "viewport": { "cols": 120, "rows": 40 },
                    "filter": "All",
                    "resume": null,
                    "resume_token": null,
                    "pane_area": "Starving"
                }
            }
        })
    );
    assert_eq!(round_trip(&starving), starving);
}

#[test]
fn restart_request_round_trips() {
    let request = IpcRequest {
        request_id: 5,
        kind: IpcRequestKind::Restart,
    };

    assert_eq!(round_trip(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 5, "kind": "Restart" })
    );
}

#[test]
fn restarting_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(5),
        result: IpcResult::Restarting,
    };

    assert_eq!(round_trip(&response), response);
    assert_eq!(
        serde_json::to_value(&response).expect("response encodes"),
        json!({ "request_id": 5, "result": "Restarting" })
    );
}

#[test]
fn attached_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(4),
        result: IpcResult::Attached {
            client_id: ClientId::new(),
            session_id: SessionId::new(),
            structure: populated_structure(),
            resume_token: None,
            pane_area: None,
        },
    };

    assert_eq!(round_trip(&response), response);
}

#[test]
fn an_attached_response_carrying_a_resume_token_keeps_the_secret_whole() {
    let response = IpcResponse {
        request_id: Some(4),
        result: IpcResult::Attached {
            client_id: ClientId::from_uuid(fixed_uuid()),
            session_id: SessionId::from_uuid(fixed_uuid()),
            structure: populated_structure(),
            resume_token: Some(token()),
            pane_area: None,
        },
    };

    let IpcResult::Attached {
        resume_token: Some(carried),
        ..
    } = round_trip(&response).result
    else {
        panic!("an attached answer carrying a resume token decodes as one");
    };

    assert_eq!(carried.expose(), token().expose());
}

#[test]
fn an_attached_response_written_without_the_resume_token_decodes_as_no_token() {
    // An attached answer written without `resume_token` decodes with
    // `resume_token: None`.
    let decoded: IpcResponse = serde_json::from_str(
        r#"{"request_id":4,"result":{"Attached":{"client_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000001","structure":{"id":"00000000-0000-0000-0000-000000000001","name":"quiet-lake","tabs":[],"panes":[]}}}}"#,
    )
    .expect("an attached answer without the resume token field decodes");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(4),
            result: IpcResult::Attached {
                client_id: ClientId::from_uuid(fixed_uuid()),
                session_id: SessionId::from_uuid(fixed_uuid()),
                structure: AttachedSessionStructureSnapshot {
                    id: SessionId::from_uuid(fixed_uuid()),
                    name: "quiet-lake".to_string(),
                    tabs: Vec::new(),
                    panes: Vec::new(),
                },
                resume_token: None,
                pane_area: None,
            },
        }
    );
}

#[test]
fn an_attached_reply_written_without_a_pane_area_decodes_as_none() {
    // An attached answer written without `pane_area` decodes with
    // `pane_area: None`.
    let decoded: IpcResponse = serde_json::from_str(
        r#"{"request_id":4,"result":{"Attached":{"client_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000001","structure":{"id":"00000000-0000-0000-0000-000000000001","name":"quiet-lake","tabs":[{"id":"00000000-0000-0000-0000-000000000001","name":"editor","index":0,"layout":{"Pane":"00000000-0000-0000-0000-000000000001"},"focus_mru":["00000000-0000-0000-0000-000000000001"]}],"panes":[{"id":"00000000-0000-0000-0000-000000000001","kind":"Terminal"}]},"resume_token":null}}}"#,
    )
    .expect("an attached answer without the pane area field decodes");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(4),
            result: IpcResult::Attached {
                client_id: ClientId::from_uuid(fixed_uuid()),
                session_id: SessionId::from_uuid(fixed_uuid()),
                structure: populated_structure(),
                resume_token: None,
                pane_area: None,
            },
        }
    );
}

#[test]
fn an_attach_envelope_carrying_an_authority_field_is_refused() {
    // The envelope's own fields are fixed: an attach frame that adds one beside
    // `request_id` and `kind` fails to decode.
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":4,"tier":"admin","kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"All"}}}"#,
    );

    // The same frame without `tier` decodes in
    // `an_attach_naming_its_own_authority_carries_none_of_it`.
    let error = decoded.expect_err("an unknown envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `tier`, expected `request_id` or `kind` at line 1 column 22"
    );
}

#[test]
fn an_attach_envelope_naming_where_it_connected_from_is_refused() {
    // An attach frame naming `origin` beside `request_id` and `kind` fails to
    // decode.
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":4,"origin":"Remote","kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"All"}}}"#,
    );

    let error = decoded.expect_err("an unknown envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `origin`, expected `request_id` or `kind` at line 1 column 24"
    );
}

#[test]
fn an_attach_naming_where_it_connected_from_carries_none_of_it() {
    // An `origin` inside the `Attach` payload is ignored. The decoded request
    // holds the viewport and filter and nothing of it.
    let with_origin: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"All","origin":"Remote"}}}"#,
    )
    .expect("an attach carrying an extra field still decodes");

    assert_eq!(
        with_origin,
        IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::Attach {
                viewport: Size { cols: 80, rows: 24 },
                filter: EventFilterSpec::All,
                resume: None,
                resume_token: None,
                pane_area: None,
                graphics: crate::protocol::GraphicsCapabilities::default(),
            },
        }
    );
}

#[test]
fn an_attach_naming_its_own_authority_carries_none_of_it() {
    // A field inside the `Attach` payload that this build does not have is
    // ignored. The decoded request holds the viewport and filter and nothing
    // of it.
    let with_tier: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"All","tier":"admin"}}}"#,
    )
    .expect("an attach carrying an extra field still decodes");

    let without_tier: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"kind":{"Attach":{"viewport":{"cols":80,"rows":24},"filter":"All"}}}"#,
    )
    .expect("the same attach without the extra field decodes");

    let expected = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: None,
            graphics: crate::protocol::GraphicsCapabilities::default(),
        },
    };

    assert_eq!(
        with_tier, expected,
        "the authority field left nothing behind in the decoded request"
    );
    assert_eq!(
        without_tier, expected,
        "the two frames decode to the same request"
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
            pane_area: None,
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn a_resize_request_reporting_a_starving_pane_area_round_trips() {
    let request = IpcRequest {
        request_id: 6,
        kind: IpcRequestKind::Resize {
            viewport: Size { cols: 2, rows: 2 },
            pane_area: Some(PaneArea::Starving),
        },
    };

    assert_eq!(round_trip(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 6,
            "kind": {
                "Resize": { "viewport": { "cols": 2, "rows": 2 }, "pane_area": "Starving" }
            }
        })
    );
}

#[test]
fn a_resize_request_written_without_a_pane_area_decodes_as_none() {
    // A resize written without `pane_area` decodes with `pane_area: None`.
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":6,"kind":{"Resize":{"viewport":{"cols":120,"rows":40}}}}"#,
    )
    .expect("a resize without the pane area field decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 6,
            kind: IpcRequestKind::Resize {
                viewport: Size {
                    cols: 120,
                    rows: 40,
                },
                pane_area: None,
            },
        }
    );
}

#[test]
fn every_mouse_action_round_trips() {
    for action in every_mouse_action() {
        assert_eq!(round_trip(&action), action);
    }
}

#[test]
fn a_mouse_request_keeps_its_round_in_the_order_it_was_sent() {
    // Three actions that differ from one another: a reordered or dropped one
    // changes the decoded round.
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
fn a_mouse_action_carrying_an_unknown_field_ignores_it() {
    let with_pixels: IpcRequest = serde_json::from_str(
        r#"{"request_id":7,"kind":{"Mouse":[{"Scroll":{"pane":"00000000-0000-0000-0000-000000000001","up":true,"lines":3,"pixels":9}}]}}"#,
    )
    .expect("a field this build does not know is ignored");

    let without_it: IpcRequest = serde_json::from_str(
        r#"{"request_id":7,"kind":{"Mouse":[{"Scroll":{"pane":"00000000-0000-0000-0000-000000000001","up":true,"lines":3}}]}}"#,
    )
    .expect("the same round without the extra field decodes");

    assert_eq!(
        with_pixels, without_it,
        "the extra field left nothing behind in the decoded round"
    );
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
fn paste_request_round_trips_its_text_whole() {
    let request = IpcRequest {
        request_id: 8,
        kind: IpcRequestKind::Paste {
            text: "hello\nworld\u{1b}[A\ttab \u{0} 日本語 🐚".to_string(),
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn recent_events_request_round_trips() {
    let request = IpcRequest {
        request_id: 9,
        kind: IpcRequestKind::RecentEvents,
    };

    assert_eq!(round_trip(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 9, "kind": "RecentEvents" })
    );
}

#[test]
fn leaving_request_round_trips() {
    let request = IpcRequest {
        request_id: 10,
        kind: IpcRequestKind::Leaving,
    };

    assert_eq!(round_trip(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 10, "kind": "Leaving" })
    );
}

#[test]
fn hello_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(1),
        result: IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            version: "0.3.0".to_string(),
        },
    };

    assert_eq!(round_trip(&response), response);
}

#[test]
fn a_hello_response_written_without_the_build_version_decodes_as_empty() {
    // A Hello answer written without `version` decodes with `version` empty.
    let decoded: IpcResponse =
        serde_json::from_str(r#"{"request_id":1,"result":{"Hello":{"protocol_version":2}}}"#)
            .expect("a hello answer without the build version decodes");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(1),
            result: IpcResult::Hello {
                protocol_version: 2,
                version: String::new(),
            },
        }
    );
}

#[test]
fn a_hello_answer_carrying_an_unknown_field_ignores_it() {
    let decoded: IpcResponse = serde_json::from_str(
        r#"{"request_id":1,"result":{"Hello":{"protocol_version":2,"version":"0.3.0","build_date":"2026-01-01"}}}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(1),
            result: IpcResult::Hello {
                protocol_version: 2,
                version: "0.3.0".to_string(),
            },
        }
    );
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
fn a_layout_request_naming_one_tab_round_trips() {
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Layout {
            tab: Some(TabId::from_uuid(fixed_uuid())),
        },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn a_layout_request_naming_no_tab_round_trips() {
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Layout { tab: None },
    };

    assert_eq!(round_trip(&request), request);
}

#[test]
fn a_layout_request_encodes_to_the_expected_shape() {
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Layout {
            tab: Some(TabId::from_uuid(fixed_uuid())),
        },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 4,
            "kind": { "Layout": { "tab": "00000000-0000-0000-0000-000000000001" } }
        })
    );
}

#[test]
fn a_layout_request_for_every_tab_encodes_a_null_tab() {
    let request = IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::Layout { tab: None },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 4, "kind": { "Layout": { "tab": null } } })
    );
}

#[test]
fn a_layout_request_carrying_an_unknown_field_ignores_it() {
    let decoded: IpcRequest =
        serde_json::from_str(r#"{"request_id":4,"kind":{"Layout":{"tab":null,"junk":5}}}"#)
            .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::Layout { tab: None },
        }
    );
}

#[test]
fn a_layout_request_written_without_a_tab_decodes_as_every_tab() {
    let decoded: IpcRequest = serde_json::from_str(r#"{"request_id":4,"kind":{"Layout":{}}}"#)
        .expect("a layout request naming no tab decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::Layout { tab: None },
        }
    );
}

#[test]
fn a_request_envelope_carrying_an_unknown_field_is_still_refused() {
    // A misspelled `request_id` beside a correct one is refused.
    let decoded: Result<IpcRequest, _> =
        serde_json::from_str(r#"{"request_id":4,"requst_id":9,"kind":"Discovery"}"#);

    let error = decoded.expect_err("an unknown envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `requst_id`, expected `request_id` or `kind` at line 1 column 27"
    );
}

#[test]
fn layout_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(4),
        result: IpcResult::Layout(layout()),
    };

    assert_eq!(round_trip(&response), response);
}

#[test]
fn recent_events_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(9),
        result: IpcResult::RecentEvents(vec![recent()]),
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
fn a_refusal_naming_the_other_users_setting_round_trips() {
    let response = IpcResponse {
        request_id: Some(7),
        result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::OtherUsersOff,
            message: "this Koshi serves only the user who started it".to_string(),
        }),
    };

    assert_eq!(round_trip(&response), response);
    assert_eq!(
        serde_json::to_value(&response).expect("response encodes"),
        json!({
            "request_id": 7,
            "result": {
                "Error": {
                    "code": "other_users_off",
                    "message": "this Koshi serves only the user who started it"
                }
            }
        })
    );
}

/// A refusal code this build has no name for decodes as
/// `IpcErrorCode::Unknown`. The message beside it decodes whole.
#[test]
fn a_refusal_code_this_build_cannot_name_reads_as_unknown() {
    let payload: IpcErrorPayload =
        serde_json::from_str(r#"{"code":"rate_limited","message":"too many attach requests"}"#)
            .expect("payload decodes");

    assert_eq!(
        payload,
        IpcErrorPayload {
            code: IpcErrorCode::Unknown,
            message: "too many attach requests".to_string(),
        }
    );
}

#[test]
fn a_refusal_written_without_a_code_reads_as_unknown() {
    let payload: IpcErrorPayload =
        serde_json::from_str(r#"{"message":"too many attach requests"}"#)
            .expect("a refusal without a code decodes");

    assert_eq!(
        payload,
        IpcErrorPayload {
            code: IpcErrorCode::Unknown,
            message: "too many attach requests".to_string(),
        }
    );
}

#[test]
fn a_refusal_written_without_a_message_is_refused() {
    let decoded: Result<IpcErrorPayload, _> = serde_json::from_str(r#"{"code":"bad_token"}"#);

    let error = decoded.expect_err("a refusal without a message decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "missing field `message` at line 1 column 20"
    );
}

/// Each refusal code encodes to its own snake_case wire name: `BadToken`
/// reads `bad_token`.
#[test]
fn every_refusal_code_encodes_to_its_own_wire_name() {
    // The match is exhaustive: a refusal code missing from it does not
    // compile.
    let wire_name = |code: IpcErrorCode| match code {
        IpcErrorCode::BadToken => "bad_token",
        IpcErrorCode::UnsupportedVersion => "unsupported_version",
        IpcErrorCode::UnsupportedKind => "unsupported_kind",
        IpcErrorCode::MalformedRequest => "malformed_request",
        IpcErrorCode::NotFound => "not_found",
        IpcErrorCode::HelloRequired => "hello_required",
        IpcErrorCode::OtherUsersOff => "other_users_off",
        IpcErrorCode::Unknown => "unknown",
    };

    for code in [
        IpcErrorCode::BadToken,
        IpcErrorCode::UnsupportedVersion,
        IpcErrorCode::UnsupportedKind,
        IpcErrorCode::MalformedRequest,
        IpcErrorCode::NotFound,
        IpcErrorCode::HelloRequired,
        IpcErrorCode::OtherUsersOff,
        IpcErrorCode::Unknown,
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
                resume: None,
                resume_token: None,
                pane_area: None,
                graphics: crate::protocol::GraphicsCapabilities::default(),
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
    assert_eq!(
        tag_of(&serde_json::to_value(IpcRequestKind::Layout { tab: None }).unwrap()),
        "Layout"
    );
    assert_eq!(
        serde_json::to_value(IpcRequestKind::RecentEvents).unwrap(),
        json!("RecentEvents")
    );
    assert_eq!(
        serde_json::to_value(IpcRequestKind::Restart).unwrap(),
        json!("Restart")
    );
}

#[test]
fn each_result_is_tagged_with_its_own_name() {
    assert_eq!(
        tag_of(
            &serde_json::to_value(IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                version: "0.3.0".to_string(),
            })
            .unwrap()
        ),
        "Hello"
    );
    assert_eq!(
        tag_of(
            &serde_json::to_value(IpcResult::Attached {
                client_id: ClientId::new(),
                session_id: SessionId::new(),
                structure: populated_structure(),
                resume_token: None,
                pane_area: None,
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
        tag_of(&serde_json::to_value(IpcResult::Layout(layout())).unwrap()),
        "Layout"
    );
    assert_eq!(
        tag_of(&serde_json::to_value(IpcResult::RecentEvents(vec![recent()])).unwrap()),
        "RecentEvents"
    );
    assert_eq!(
        serde_json::to_value(IpcResult::Restarting).unwrap(),
        json!("Restarting")
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
fn every_request_kind_is_tagged_with_its_name() {
    for kind in every_request_kind() {
        let encoded = serde_json::to_value(&kind).expect("request kind encodes");

        assert_eq!(tag_of(&encoded), kind.name(), "{kind:?}");
    }
}

#[test]
fn every_result_is_tagged_with_its_wire_name() {
    for result in every_result() {
        let encoded = serde_json::to_value(&result).expect("result encodes");

        assert_eq!(tag_of(&encoded), result.wire_name(), "{result:?}");
    }
}

#[test]
fn variants_lists_every_request_kind_in_declaration_order() {
    let names: Vec<&str> = every_request_kind()
        .iter()
        .map(IpcRequestKind::name)
        .collect();

    assert_eq!(names, IpcRequestKind::VARIANTS);
}

#[test]
fn variants_lists_every_result_in_declaration_order() {
    let names: Vec<&str> = every_result().iter().map(IpcResult::wire_name).collect();

    assert_eq!(names, IpcResult::VARIANTS);
}

#[test]
fn a_request_naming_a_kind_this_build_does_not_have_reads_as_unknown() {
    let decoded: IncomingRequest =
        serde_json::from_str(r#"{"request_id":9,"kind":{"Floating":{"pane":3}}}"#)
            .expect("a kind this build does not have decodes as unknown");

    assert_eq!(
        decoded,
        IncomingRequest {
            request_id: 9,
            kind: MaybeKnown::Unknown {
                name: "Floating".to_string(),
            },
        }
    );
}

#[test]
fn a_request_naming_a_kind_this_build_has_reads_as_known() {
    let decoded: IncomingRequest =
        serde_json::from_str(r#"{"request_id":9,"kind":{"Layout":{"tab":null}}}"#)
            .expect("a kind this build has decodes as known");

    assert_eq!(
        decoded,
        IncomingRequest {
            request_id: 9,
            kind: MaybeKnown::Known(IpcRequestKind::Layout { tab: None }),
        }
    );
}

#[test]
fn a_response_naming_a_result_this_build_does_not_have_reads_as_unknown() {
    let decoded: IncomingResponse = serde_json::from_str(r#"{"request_id":9,"result":"Rebooted"}"#)
        .expect("a result this build does not have decodes as unknown");

    assert_eq!(
        decoded,
        IncomingResponse {
            request_id: Some(9),
            result: MaybeKnown::Unknown {
                name: "Rebooted".to_string(),
            },
        }
    );
}

#[test]
fn a_response_naming_a_result_this_build_has_reads_as_known() {
    let decoded: IncomingResponse =
        serde_json::from_str(r#"{"request_id":9,"result":"Restarting"}"#)
            .expect("a result this build has decodes as known");

    assert_eq!(
        decoded,
        IncomingResponse {
            request_id: Some(9),
            result: MaybeKnown::Known(IpcResult::Restarting),
        }
    );
}

/// A response carrying a field beside `request_id` and `result` is refused. An
/// absent `request_id` means the request could not be read.
#[test]
fn a_response_with_a_misspelled_request_id_is_refused() {
    // The result decodes on its own in
    // `a_response_envelope_this_build_reads_decodes`; the misspelled field is
    // the only fault in these bytes.
    let decoded: Result<IpcResponse, _> = serde_json::from_str(
        r#"{"requst_id":7,"result":{"Hello":{"protocol_version":2}},"request_id":7}"#,
    );

    let error = decoded.expect_err("a misspelled envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `requst_id`, expected `request_id` or `result` at line 1 column 12"
    );
}

#[test]
fn a_response_envelope_this_build_reads_decodes() {
    let decoded: IpcResponse =
        serde_json::from_str(r#"{"request_id":7,"result":{"Hello":{"protocol_version":2}}}"#)
            .expect("the same bytes without the misspelling decode");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(7),
            result: IpcResult::Hello {
                protocol_version: 2,
                version: String::new(),
            },
        }
    );
}

#[test]
fn a_request_carrying_an_unknown_field_is_refused() {
    let decoded: Result<IpcRequest, _> =
        serde_json::from_str(r#"{"request_id":1,"kind":"Discovery","junk":5}"#);

    let error = decoded.expect_err("an unknown envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `junk`, expected `request_id` or `kind` at line 1 column 41"
    );
}

/// A field inside the Hello payload that this build does not have is ignored.
/// The envelope around it refuses one.
#[test]
fn a_hello_carrying_an_unknown_field_ignores_it() {
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":1,"kind":{"Hello":{"min_protocol_version":2,"max_protocol_version":2,"token":"k7QxSecret","junk":5}}}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 1,
            kind: IpcRequestKind::Hello {
                min_protocol_version: 2,
                max_protocol_version: 2,
                token: token(),
                remote: false,
            },
        }
    );
}

/// A Hello without `min_protocol_version` is refused; no default fills it in.
#[test]
fn a_hello_missing_a_version_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":1,"kind":{"Hello":{"max_protocol_version":2,"token":"k7QxSecret"}}}"#,
    );

    let error = decoded.expect_err("a Hello missing a version decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "missing field `min_protocol_version` at line 1 column 79"
    );
}

/// `IpcRequestKind::hello` fills `min_protocol_version` with
/// `MIN_PROTOCOL_VERSION` and `max_protocol_version` with `PROTOCOL_VERSION`,
/// carries the token through, and sets `remote` to `false`.
#[test]
fn the_hello_this_build_sends_carries_the_range_it_speaks() {
    let IpcRequestKind::Hello {
        min_protocol_version,
        max_protocol_version,
        token: carried,
        remote: false,
    } = IpcRequestKind::hello(token())
    else {
        panic!("the constructor builds a Hello");
    };

    assert_eq!(min_protocol_version, MIN_PROTOCOL_VERSION);
    assert_eq!(max_protocol_version, PROTOCOL_VERSION);
    assert!(
        min_protocol_version <= max_protocol_version,
        "the lowest version this build speaks is not above its highest"
    );
    assert_eq!(carried, token(), "the endpoint's token is carried through");
}

#[test]
fn token_encodes_as_a_bare_string() {
    assert_eq!(
        serde_json::to_value(token()).expect("token encodes"),
        json!("k7QxSecret")
    );
}

#[test]
fn token_decodes_from_a_bare_string() {
    let decoded: ConnectionToken =
        serde_json::from_str(r#""k7QxSecret""#).expect("a bare string decodes as a token");

    assert_eq!(decoded, token());
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
            min_protocol_version: 1,
            max_protocol_version: 1,
            token: token(),
            remote: false,
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
            min_protocol_version: 1,
            max_protocol_version: 1,
            token: token(),
            remote: false,
        }
        .name(),
        "Hello"
    );
    assert_eq!(
        IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: None,
            graphics: crate::protocol::GraphicsCapabilities::default(),
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
            pane_area: None,
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
    assert_eq!(IpcRequestKind::Layout { tab: None }.name(), "Layout");
    assert_eq!(
        IpcRequestKind::Layout {
            tab: Some(TabId::from_uuid(fixed_uuid())),
        }
        .name(),
        "Layout"
    );
    assert_eq!(IpcRequestKind::RecentEvents.name(), "RecentEvents");
    assert_eq!(IpcRequestKind::Restart.name(), "Restart");
    assert_eq!(IpcRequestKind::Leaving.name(), "Leaving");
}

/// Serializing a Hello writes the real secret.
#[test]
fn serializing_a_hello_writes_the_real_secret() {
    let request = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            token: token(),
            remote: false,
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
fn two_empty_tokens_are_equal() {
    assert_eq!(ConnectionToken::new(""), ConnectionToken::new(""));
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
