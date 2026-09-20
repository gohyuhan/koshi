//! Tests for the wire messages: every request and response variant survives a
//! round trip and keeps its own tag, an unknown field is refused on the
//! envelope and ignored on the payload, and the connection token prints as
//! `***` and is equal only to a token holding the same bytes.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koshi_core::client::ClientOrigin;

use crate::attach::{PaneStructure, TabStructure};
use crate::layout::{ClientFocus, SolvedPane, SolvedTab, TabLayout};
use crate::plane::Plane;
use crate::router::RouterRequestKind;
use crate::wire::{MaybeKnown, WireName, WireVariants};
use koshi_core::command::{
    Command, CommandSource, MovePaneArgs, NewPaneArgs, PanePlacementAnchor, PanePlacementTarget,
    PlacePaneArgs, ScrollPaneArgs, SwapPanesArgs, ToggleLockModeArgs,
};
use koshi_core::discovery::{
    ClientDiscovery, PaneDiscovery, PaneLifecycle, SessionDiscovery, TabDiscovery,
};
use koshi_core::event::RejectReason;
use koshi_core::geometry::{Direction, PaneArea, Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{Key, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags, ModFlags};
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

use super::*;

/// A token holding a fixed secret.
fn build_test_connection_token() -> ConnectionToken {
    ConnectionToken::from_secret("k7QxSecret")
}

/// An envelope carrying one command with no arguments.
fn build_test_command_envelope() -> CommandEnvelope {
    CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::ExternalCli {
            session_id: None,
            target_client_id: None,
        },
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
}

/// An envelope carrying a `NewPane` with every optional field filled, at fixed
/// ids and times. Encodes to the same bytes on every call.
fn build_populated_test_command_envelope() -> CommandEnvelope {
    build_command_envelope_with_fixed_in_session_source(Command::NewPane(NewPaneArgs {
        source_pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
        tab_id: Some(TabId::from_uuid(build_fixed_test_uuid())),
        direction: Direction::Down,
        should_stack: true,
        working_directory: Some(PathBuf::from("/home/user")),
        spawn_spec: Some(SpawnSpec {
            program: PathBuf::from("/bin/zsh"),
            arguments: vec!["-l".to_string()],
            working_directory: Some(PathBuf::from("/home/user")),
            environment_variables: BTreeMap::from([(
                "KOSHI_PANE_ID".to_string(),
                "pane-1".to_string(),
            )]),
            shell_kind: ShellKind::Zsh,
        }),
        client_id: Some(ClientId::from_uuid(build_fixed_test_uuid())),
    }))
}

/// An envelope carrying a `PlacePane` split at fixed ids and times. Encodes to
/// the same bytes on every call.
fn build_populated_place_pane_command_envelope() -> CommandEnvelope {
    build_command_envelope_with_fixed_in_session_source(Command::PlacePane(PlacePaneArgs {
        source_pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
        placement_target: PanePlacementTarget::Split {
            destination_tab_id: TabId::from_uuid(build_fixed_test_uuid()),
            anchor: PanePlacementAnchor::Tab,
            direction: Direction::Down,
        },
        expected_placement_revision: None,
    }))
}

/// Build a command envelope with the fixed in-session source used by the wire
/// shape tests.
fn build_command_envelope_with_fixed_in_session_source(command: Command) -> CommandEnvelope {
    CommandEnvelope::from_parts(
        CommandId::from_uuid(build_fixed_test_uuid()),
        CommandSource::InSessionCli {
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            client_id: Some(ClientId::from_uuid(build_fixed_test_uuid())),
            pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
            socket_path: PathBuf::from("/run/koshi.sock"),
        },
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        command,
    )
}

/// An overview of a session with no tabs, panes, or clients.
fn build_empty_session_overview() -> SessionOverview {
    SessionOverview {
        session: SessionDiscovery {
            session_id: SessionId::new(),
            session_name: "quiet-lake".to_string(),
            created_at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            attached_client_ids: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    }
}

/// One remembered event: a pane created in a tab, stamped at the epoch.
fn build_test_recent_event() -> RecentEvent {
    koshi_core::recent_event::record_event(
        &koshi_core::event::Event::PaneCreated(koshi_core::event::PaneCreated {
            pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
            tab_id: TabId::from_uuid(build_fixed_test_uuid()),
        }),
        std::time::SystemTime::UNIX_EPOCH,
    )
}

/// The layout of a session with one tab, holding one pane, that one client
/// views. The wire form of every field is pinned in `crate::layout::tests`.
fn build_test_session_layout() -> SessionLayout {
    let tab_id = TabId::from_uuid(build_fixed_test_uuid());
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());

    SessionLayout {
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        session_name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            tab_id,
            tab_name: "editor".to_string(),
            tab_index: 0,
            layout_tree: LayoutNode::Pane(pane_id),
            solved_tabs: vec![SolvedTab {
                client_id,
                viewport_size: Size {
                    column_count: 80,
                    row_count: 22,
                },
                layout_mode: LayoutMode::Tiled,
                pane_rects: vec![SolvedPane {
                    pane_id,
                    outer_rect: Rect::from_size_at_origin(Size {
                        column_count: 80,
                        row_count: 22,
                    }),
                }],
                suppressed_pane_ids: Vec::new(),
                is_every_pane_suppressed: false,
                stack_headers: Vec::new(),
            }],
        }],
        clients: vec![ClientFocus {
            client_id,
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
        }],
    }
}

/// An overview of a session with one tab, one pane in it, and one attached
/// client, at fixed ids and times. Encodes to the same bytes on every call.
fn build_populated_test_session_overview() -> SessionOverview {
    let session_id = SessionId::from_uuid(build_fixed_test_uuid());
    let tab_id = TabId::from_uuid(build_fixed_test_uuid());
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());
    let event_timestamp = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

    SessionOverview {
        session: SessionDiscovery {
            session_id,
            session_name: "quiet-lake".to_string(),
            created_at: event_timestamp,
            attached_client_ids: vec![client_id],
            pane_count: 1,
        },
        tabs: vec![TabDiscovery {
            tab_id,
            session_id,
            tab_name: "editor".to_string(),
            tab_index: 0,
            active_pane_id: Some(pane_id),
            pane_count: 1,
        }],
        panes: vec![PaneDiscovery {
            pane_id,
            tab_id,
            session_id,
            pane_title: Some("vim".to_string()),
            working_directory: Some(PathBuf::from("/home/user")),
            command_argv: None,
            lifecycle: PaneLifecycle::Running,
            focused_by_client_ids: vec![client_id],
        }],
        clients: vec![ClientDiscovery {
            client_id,
            session_id,
            attached_at: event_timestamp,
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            origin: Some(ClientOrigin::Local),
            pane_area: None,
        }],
    }
}

/// A session structure holding one tab and the one terminal pane in it, at
/// fixed ids. Encodes to the same bytes on every call.
fn populated_structure() -> AttachedSessionStructureSnapshot {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());

    AttachedSessionStructureSnapshot {
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        session_name: "quiet-lake".to_string(),
        tabs: vec![TabStructure {
            tab_id: TabId::from_uuid(build_fixed_test_uuid()),
            tab_name: "editor".to_string(),
            tab_index: 0,
            layout: LayoutNode::Pane(pane_id),
            focus_mru: vec![pane_id],
        }],
        panes: vec![PaneStructure {
            pane_id,
            pane_kind: PaneKind::Terminal,
        }],
    }
}

/// Every mouse action a round can carry, in the order the enum declares them,
/// at fixed ids.
fn every_mouse_action() -> Vec<WireMouseAction> {
    let pane = PaneId::from_uuid(build_fixed_test_uuid());

    vec![
        WireMouseAction::Scroll {
            pane_id: pane,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
        WireMouseAction::Forward {
            pane_id: pane,
            mouse_input: MouseInput {
                mouse_kind: MouseKind::Press(MouseButton::Left),
                position: Point { column: 10, row: 3 },
                modifier_flags: ModFlags::CTRL,
            },
        },
        WireMouseAction::AltScrollArrows {
            pane_id: pane,
            is_scrolling_up: false,
            arrow_count: 5,
        },
        WireMouseAction::Resize {
            pane_id: pane,
            border_side: Direction::Left,
            resize_step: -1,
            requested_cell_count: 2,
        },
        WireMouseAction::Command(Box::new(Command::ToggleLockMode(
            ToggleLockModeArgs::default(),
        ))),
    ]
}

/// One request of every kind, in the order the enum declares them.
fn every_request_kind() -> Vec<IpcRequestKind> {
    vec![
        IpcRequestKind::build_hello_request(build_test_connection_token()),
        IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
        IpcRequestKind::Keyboard {
            key_input: build_control_c_key_input(),
        },
        IpcRequestKind::Resize {
            viewport: Size {
                column_count: 120,
                row_count: 40,
            },
            pane_area: None,
            cell_size: None,
        },
        IpcRequestKind::CellSize {
            cell_size: koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
                .expect("nonzero cell size"),
        },
        IpcRequestKind::Paste {
            pasted_text: "hello\nworld".to_string(),
        },
        IpcRequestKind::Mouse(every_mouse_action()),
        IpcRequestKind::SubmitCommand(Box::new(build_test_command_envelope())),
        IpcRequestKind::Discovery,
        IpcRequestKind::Layout { tab_id: None },
        IpcRequestKind::RecentEvents,
        IpcRequestKind::Restart,
        IpcRequestKind::Leaving,
    ]
}

/// One answer of every kind, in the order the enum declares them.
fn list_all_ipc_results() -> Vec<IpcResult> {
    vec![
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: "0.3.0".to_string(),
        },
        IpcResult::Attached {
            client_id: ClientId::from_uuid(build_fixed_test_uuid()),
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            session_structure: populated_structure(),
            resume_token: None,
            pane_area: None,
        },
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: CommandId::from_uuid(build_fixed_test_uuid()),
            emitted_events: Vec::new(),
        }),
        IpcResult::Overview(build_empty_session_overview()),
        IpcResult::Layout(build_test_session_layout()),
        IpcResult::RecentEvents(vec![build_test_recent_event()]),
        IpcResult::Restarting,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token does not match".to_string(),
        }),
    ]
}

/// The one UUID every fixed id in this file uses.
fn build_fixed_test_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// Encode `message` and decode it back.
fn round_trip_wire_message<T: Serialize + DeserializeOwned>(message: &T) -> T {
    let encoded_json = serde_json::to_string(message).expect("message encodes");
    serde_json::from_str(&encoded_json).expect("message decodes")
}

/// The tag an encoded JSON enum variant carries: the single key of
/// `{"Overview": { … }}`, or the string itself for `"Restarting"`.
fn tag_of(encoded_json: &serde_json::Value) -> String {
    if let Some(tag_name) = encoded_json.as_str() {
        return tag_name.to_string();
    }

    let object_fields = encoded_json
        .as_object()
        .expect("a tagged variant encodes as a string or an object");

    assert_eq!(
        object_fields.len(),
        1,
        "expected exactly one tag in {encoded_json}"
    );

    object_fields.keys().next().expect("one key").clone()
}

#[test]
fn the_protocol_version_this_build_speaks_is_four() {
    assert_eq!(PROTOCOL_VERSION, 4);
}

#[test]
fn the_lowest_protocol_version_this_build_speaks_is_four() {
    assert_eq!(MIN_PROTOCOL_VERSION, 4);
}

#[test]
fn compute_agreed_protocol_version_uses_highest_shared_version() {
    assert_eq!(compute_agreed_protocol_version(2, 4, 2, 2), Some(2));
    assert_eq!(compute_agreed_protocol_version(2, 3, 2, 3), Some(3));
    assert_eq!(compute_agreed_protocol_version(5, 6, 2, 2), None);
}

#[test]
fn compute_agreed_protocol_version_accepts_one_shared_version() {
    assert_eq!(compute_agreed_protocol_version(1, 2, 2, 5), Some(2));
    assert_eq!(compute_agreed_protocol_version(2, 5, 1, 2), Some(2));
}

#[test]
fn compute_agreed_protocol_version_rejects_inverted_ranges() {
    assert_eq!(compute_agreed_protocol_version(5, 2, 2, 5), None);
    assert_eq!(compute_agreed_protocol_version(2, 5, 5, 2), None);
}

#[test]
fn the_session_plane_answers_a_refusal_as_an_error_result() {
    let ipc_error_payload = IpcErrorPayload {
        code: IpcErrorCode::BadToken,
        message: "the token does not match".to_string(),
    };

    assert_eq!(
        SessionPlane::build_refusal_response(ipc_error_payload.clone()),
        IpcResult::Error(ipc_error_payload)
    );
}

#[test]
fn the_session_plane_answers_a_hello_with_the_agreed_version_and_the_build() {
    assert_eq!(
        SessionPlane::build_hello_response(2, "0.3.0"),
        IpcResult::Hello {
            protocol_version: 2,
            build_version: "0.3.0".to_string(),
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
        serde_json::to_value(build_populated_test_session_overview()).expect("overview encodes"),
        json!({
            "session": {
                "session_id": "00000000-0000-0000-0000-000000000001",
                "session_name": "quiet-lake",
                "created_at": { "secs_since_epoch": 1_700_000_000, "nanos_since_epoch": 0 },
                "attached_client_ids": ["00000000-0000-0000-0000-000000000001"],
                "pane_count": 1
            },
            "tabs": [{
                "tab_id": "00000000-0000-0000-0000-000000000001",
                "session_id": "00000000-0000-0000-0000-000000000001",
                "tab_name": "editor",
                "tab_index": 0,
                "active_pane_id": "00000000-0000-0000-0000-000000000001",
                "pane_count": 1
            }],
            "panes": [{
                "pane_id": "00000000-0000-0000-0000-000000000001",
                "tab_id": "00000000-0000-0000-0000-000000000001",
                "session_id": "00000000-0000-0000-0000-000000000001",
                "pane_title": "vim",
                "working_directory": "/home/user",
                "command_argv": null,
                "lifecycle": "Running",
                "focused_by_client_ids": ["00000000-0000-0000-0000-000000000001"]
            }],
            "clients": [{
                "client_id": "00000000-0000-0000-0000-000000000001",
                "session_id": "00000000-0000-0000-0000-000000000001",
                "attached_at": { "secs_since_epoch": 1_700_000_000, "nanos_since_epoch": 0 },
                "viewport_size": { "column_count": 80, "row_count": 24 },
                "active_tab_id": "00000000-0000-0000-0000-000000000001",
                "focused_pane_id": "00000000-0000-0000-0000-000000000001",
                "lock_mode": "Normal",
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
        "client_id": "00000000-0000-0000-0000-000000000001",
        "session_id": "00000000-0000-0000-0000-000000000001",
        "attached_at": { "secs_since_epoch": 1_700_000_000, "nanos_since_epoch": 0 },
        "viewport_size": { "column_count": 80, "row_count": 24 },
        "active_tab_id": "00000000-0000-0000-0000-000000000001",
        "focused_pane_id": null,
        "lock_mode": "Normal"
    });
    let decoded: ClientDiscovery =
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
        client_id: ClientId,
        session_id: SessionId,
        attached_at: SystemTime,
        viewport_size: Size,
        active_tab_id: TabId,
        focused_pane_id: Option<PaneId>,
        lock_mode: LockMode,
    }
    let mut written = build_populated_test_session_overview().clients.remove(0);
    written.origin = Some(ClientOrigin::Remote);
    let written = serde_json::to_value(written).expect("a client row encodes");
    let legacy_client_info: OldClientInfo =
        serde_json::from_value(written).expect("the older shape reads a row carrying origin");
    assert_eq!(legacy_client_info.lock_mode, LockMode::Normal);
}

#[test]
fn the_submit_command_wire_shape_belongs_to_this_protocol_version() {
    // Every field of a command a CLI sends, as this build writes it: the
    // envelope, the source it names, and the whole argument struct of the
    // command inside it. Any field of `Command` or of an `*Args` struct that
    // is added, removed, renamed or retyped changes these bytes. This fixture
    // pins the command vocabulary under the protocol version named by
    // `koshi_core::compat`.
    let request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(
            build_populated_test_command_envelope(),
        )),
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 2,
            "request_kind": {
                "SubmitCommand": {
                    "command_id": "00000000-0000-0000-0000-000000000001",
                    "command_source": {
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
                            "source_pane_id": "00000000-0000-0000-0000-000000000001",
                            "tab_id": "00000000-0000-0000-0000-000000000001",
                            "direction": "Down",
                            "should_stack": true,
                            "working_directory": "/home/user",
                            "spawn_spec": {
                                "program": "/bin/zsh",
                                "arguments": ["-l"],
                                "working_directory": "/home/user",
                                "environment_variables": { "KOSHI_PANE_ID": "pane-1" },
                                "shell_kind": "Zsh"
                            },
                            "client_id": "00000000-0000-0000-0000-000000000001"
                        }
                    }
                }
            }
        })
    );
}

#[test]
fn the_place_pane_command_wire_shape_belongs_to_this_protocol_version() {
    let request = IpcRequest {
        request_id: 5,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(
            build_populated_place_pane_command_envelope(),
        )),
    };
    let encoded_request = serde_json::to_value(&request).expect("request encodes");

    assert_eq!(
        encoded_request["request_kind"]["SubmitCommand"]["command"],
        json!({
            "PlacePane": {
                "source_pane_id": "00000000-0000-0000-0000-000000000001",
                "placement_target": {
                    "Split": {
                        "destination_tab_id": "00000000-0000-0000-0000-000000000001",
                        "anchor": "Tab",
                        "direction": "Down"
                    }
                },
                "expected_placement_revision": null
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
        request_kind: IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 4,
            "request_kind": {
                "Attach": {
                    "viewport": { "column_count": 80, "row_count": 24 },
                    "event_filter": "All",
                    "resume_client_id": null,
                    "resume_token": null,
                    "pane_area": null
                }
            }
        })
    );

    let response = IpcResponse {
        request_id: Some(4),
        answer_result: IpcResult::Attached {
            client_id: ClientId::from_uuid(build_fixed_test_uuid()),
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            session_structure: populated_structure(),
            resume_token: None,
            pane_area: None,
        },
    };

    assert_eq!(
        serde_json::to_value(&response).expect("response encodes"),
        json!({
            "request_id": 4,
            "answer_result": {
                "Attached": {
                    "client_id": "00000000-0000-0000-0000-000000000001",
                    "session_id": "00000000-0000-0000-0000-000000000001",
                    "session_structure": {
                        "session_id": "00000000-0000-0000-0000-000000000001",
                        "session_name": "quiet-lake",
                        "tabs": [{
                            "tab_id": "00000000-0000-0000-0000-000000000001",
                            "tab_name": "editor",
                            "tab_index": 0,
                            "layout": { "Pane": "00000000-0000-0000-0000-000000000001" },
                            "focus_mru": ["00000000-0000-0000-0000-000000000001"]
                        }],
                        "panes": [{
                            "pane_id": "00000000-0000-0000-0000-000000000001",
                            "pane_kind": "Terminal"
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
        request_kind: IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities {
                supports_kitty: true,
                supports_iterm: false,
                supports_sixel: false,
            },
            cell_size: None,
        },
    };
    assert_eq!(
        serde_json::to_value(&supported).expect("the capability report encodes"),
        json!({
            "request_id": 4,
            "request_kind": {
                "Attach": {
                    "viewport": { "column_count": 80, "row_count": 24 },
                    "event_filter": "All",
                    "resume_client_id": null,
                    "resume_token": null,
                    "pane_area": null,
                    "graphics_capabilities": { "supports_kitty": true, "supports_iterm": false, "supports_sixel": false }
                }
            }
        })
    );

    let absent: IpcRequest = serde_json::from_value(json!({
        "request_id": 4,
        "request_kind": {
            "Attach": {
                "viewport": { "column_count": 80, "row_count": 24 },
                "event_filter": "All",
                "resume_client_id": null,
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
            request_kind: IpcRequestKind::Attach {
                viewport: Size {
                    column_count: 80,
                    row_count: 24
                },
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: crate::protocol::GraphicsCapabilities {
                    supports_kitty: false,
                    supports_iterm: false,
                    supports_sixel: false,
                },
                cell_size: None,
            },
        }
    );
}

#[test]
fn graphics_capabilities_default_and_native_detection_cover_each_protocol() {
    assert!(!GraphicsCapabilities::default().has_native_image_protocol());
    assert!(GraphicsCapabilities {
        supports_kitty: true,
        supports_iterm: false,
        supports_sixel: false,
    }
    .has_native_image_protocol());
    assert!(GraphicsCapabilities {
        supports_kitty: false,
        supports_iterm: true,
        supports_sixel: false,
    }
    .has_native_image_protocol());
    assert!(GraphicsCapabilities {
        supports_kitty: false,
        supports_iterm: false,
        supports_sixel: true,
    }
    .has_native_image_protocol());
}

#[test]
fn graphics_capabilities_ignore_unknown_fields_and_default_new_fields() {
    let decoded: GraphicsCapabilities = serde_json::from_value(json!({
        "supports_kitty": true,
        "vendor_extension": "ignored"
    }))
    .expect("unknown capability fields are ignored");

    assert_eq!(
        decoded,
        GraphicsCapabilities {
            supports_kitty: true,
            supports_iterm: false,
            supports_sixel: false,
        }
    );
}

#[test]
fn graphics_capabilities_reject_retired_field_names() {
    for retired_field_name in ["kitty", "iterm", "sixel"] {
        let capability_json = format!(r#"{{"{retired_field_name}":true}}"#);
        let retired_field_error = serde_json::from_str::<GraphicsCapabilities>(&capability_json)
            .expect_err("a retired capability field is refused");

        assert_eq!(
            retired_field_error.to_string(),
            format!(
                "unknown field `{retired_field_name}`, expected one of `supports_kitty`, \
                 `supports_iterm`, `supports_sixel` at line 1 column 8"
            )
        );
    }
}

#[test]
fn attach_rejects_retired_graphics_capability_fields() {
    let attach_request_json = r#"{"request_id":4,"request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"All","resume_client_id":null,"resume_token":null,"pane_area":null,"graphics_capabilities":{"kitty":true}}}}"#;
    let retired_field_error = serde_json::from_str::<IpcRequest>(attach_request_json)
        .expect_err("an attach request cannot use a retired capability field");

    assert_eq!(
        retired_field_error.to_string(),
        "unknown field `kitty`, expected one of `supports_kitty`, `supports_iterm`, \
         `supports_sixel` at line 1 column 202"
    );
}

#[test]
fn an_overview_missing_a_field_this_version_needs_is_refused() {
    // A tab record without `session_id` fails to decode; no default fills it
    // in.
    let mut encoded_json =
        serde_json::to_value(build_populated_test_session_overview()).expect("overview encodes");
    encoded_json["tabs"][0]
        .as_object_mut()
        .expect("a tab encodes as an object")
        .remove("session_id");

    let decoded: Result<SessionOverview, _> = serde_json::from_value(encoded_json);
    let error = decoded.expect_err("a tab without its session is not this version's shape");
    assert_eq!(error.to_string(), "missing field `session_id`");
}

#[test]
fn hello_request_round_trips() {
    let request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
            is_remote: false,
        },
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn hello_request_encodes_to_the_expected_shape() {
    let request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 2,
            connection_token: build_test_connection_token(),
            is_remote: false,
        },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 1,
            "request_kind": {
                "Hello": {
                    "min_protocol_version": 1,
                    "max_protocol_version": 2,
                    "connection_token": "k7QxSecret",
                    "is_remote": false
                }
            }
        })
    );
}

#[test]
fn a_hello_marking_a_remote_caller_round_trips_and_encodes_true() {
    let request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token: build_test_connection_token(),
            is_remote: true,
        },
    };

    assert_eq!(round_trip_wire_message(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes")["request_kind"]["Hello"]
            ["is_remote"],
        json!(true)
    );
}

#[test]
fn a_hello_whose_token_is_not_a_string_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":1,"request_kind":{"Hello":{"min_protocol_version":2,"max_protocol_version":2,"connection_token":5}}}"#,
    );

    let error = decoded.expect_err("a number where the token goes decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "invalid type: integer `5`, expected a string at line 1 column 111"
    );
}

#[test]
fn attach_request_round_trips() {
    let request = IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn attach_and_resize_keep_cell_measurements_and_default_old_wire_frames() {
    let cell_size = koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
        .expect("positive cell dimensions");
    let attach = IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: Some(cell_size),
        },
    };
    let resize = IpcRequest {
        request_id: 6,
        request_kind: IpcRequestKind::Resize {
            viewport: Size {
                column_count: 120,
                row_count: 40,
            },
            pane_area: None,
            cell_size: Some(cell_size),
        },
    };

    assert_eq!(round_trip_wire_message(&attach), attach);
    assert_eq!(round_trip_wire_message(&resize), resize);
    assert_eq!(
        serde_json::to_value(&attach).expect("attach encodes")["request_kind"]["Attach"]
            ["cell_size"],
        json!({ "pixel_width": 10, "pixel_height": 20 })
    );
    assert_eq!(
        serde_json::from_str::<IpcRequest>(
            r#"{"request_id":4,"request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"All"}}}"#,
        )
        .expect("legacy attach decodes"),
        IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Attach {
                viewport: Size { column_count: 80, row_count: 24 },
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        }
    );
    assert_eq!(
        serde_json::from_str::<IpcRequest>(
            r#"{"request_id":6,"request_kind":{"Resize":{"viewport":{"column_count":120,"row_count":40}}}}"#,
        )
        .expect("legacy resize decodes"),
        IpcRequest {
            request_id: 6,
            request_kind: IpcRequestKind::Resize {
                viewport: Size {
                    column_count: 120,
                    row_count: 40,
                },
                pane_area: None,
                cell_size: None,
            },
        }
    );
}

#[test]
fn an_attach_request_naming_a_client_to_come_back_as_round_trips() {
    let request = IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: Some(ClientId::from_uuid(build_fixed_test_uuid())),
            resume_token: None,
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn an_attach_request_written_without_the_resume_fields_decodes_as_no_claim() {
    // An attach written without `resume` and `resume_token` decodes with both
    // `None`.
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"All"}}}"#,
    )
    .expect("an attach without the resume fields decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Attach {
                viewport: Size {
                    column_count: 80,
                    row_count: 24
                },
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        }
    );
}

#[test]
fn an_attach_request_carrying_a_resume_token_keeps_the_secret_whole() {
    let request = IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: Some(ClientId::from_uuid(build_fixed_test_uuid())),
            resume_token: Some(build_test_connection_token()),
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };

    let IpcRequestKind::Attach {
        resume_token: Some(carried),
        ..
    } = round_trip_wire_message(&request).request_kind
    else {
        panic!("an attach carrying a resume token decodes as one");
    };

    assert_eq!(carried.expose(), build_test_connection_token().expose());
}

#[test]
fn an_attach_request_written_without_a_resume_token_beside_a_resume_decodes_as_no_token() {
    // An attach written with `resume` and without `resume_token` decodes with
    // `resume_token: None`.
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"All","resume_client_id":"00000000-0000-0000-0000-000000000001"}}}"#,
    )
    .expect("an attach without the resume token field decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Attach {
                viewport: Size {
                    column_count: 80,
                    row_count: 24
                },
                event_filter: EventFilterSpec::All,
                resume_client_id: Some(ClientId::from_uuid(build_fixed_test_uuid())),
                resume_token: None,
                pane_area: None,
                graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        }
    );
}

#[test]
fn an_attach_request_written_without_a_pane_area_decodes_as_none() {
    // An attach written without `pane_area` decodes with `pane_area: None`.
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":1,"request_kind":{"Attach":{"viewport":{"column_count":120,"row_count":40},"event_filter":"All","resume_client_id":null,"resume_token":null}}}"#,
    )
    .expect("an attach without the pane area field decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Attach {
                viewport: Size {
                    column_count: 120,
                    row_count: 40,
                },
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        }
    );
}

#[test]
fn an_attach_naming_an_unknown_pane_area_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":1,"request_kind":{"Attach":{"viewport":{"column_count":120,"row_count":40},"event_filter":"All","pane_area":"Bogus"}}}"#,
    );

    let error = decoded.expect_err("an unknown pane area decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown variant `Bogus`, expected `Reported` or `Starving` at line 1 column 129"
    );
}

#[test]
fn an_attach_naming_an_unknown_filter_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":4,"request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"Some"}}}"#,
    );

    let error = decoded.expect_err("an unknown filter decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown variant `Some`, expected `All` at line 1 column 109"
    );
}

#[test]
fn an_attach_request_reporting_a_pane_area_round_trips() {
    let reported = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Attach {
            viewport: Size {
                column_count: 120,
                row_count: 40,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: Some(PaneArea::Reported(Size {
                column_count: 100,
                row_count: 30,
            })),
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };

    assert_eq!(
        serde_json::to_value(&reported).expect("the attach encodes"),
        json!({
            "request_id": 1,
            "request_kind": {
                "Attach": {
                    "viewport": { "column_count": 120, "row_count": 40 },
                    "event_filter": "All",
                    "resume_client_id": null,
                    "resume_token": null,
                    "pane_area": { "Reported": { "column_count": 100, "row_count": 30 } }
                }
            }
        })
    );
    assert_eq!(round_trip_wire_message(&reported), reported);

    let starving = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Attach {
            viewport: Size {
                column_count: 120,
                row_count: 40,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: Some(PaneArea::Starving),
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };

    assert_eq!(
        serde_json::to_value(&starving).expect("the attach encodes"),
        json!({
            "request_id": 1,
            "request_kind": {
                "Attach": {
                    "viewport": { "column_count": 120, "row_count": 40 },
                    "event_filter": "All",
                    "resume_client_id": null,
                    "resume_token": null,
                    "pane_area": "Starving"
                }
            }
        })
    );
    assert_eq!(round_trip_wire_message(&starving), starving);
}

#[test]
fn restart_request_round_trips() {
    let request = IpcRequest {
        request_id: 5,
        request_kind: IpcRequestKind::Restart,
    };

    assert_eq!(round_trip_wire_message(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 5, "request_kind": "Restart" })
    );
}

#[test]
fn restarting_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(5),
        answer_result: IpcResult::Restarting,
    };

    assert_eq!(round_trip_wire_message(&response), response);
    assert_eq!(
        serde_json::to_value(&response).expect("response encodes"),
        json!({ "request_id": 5, "answer_result": "Restarting" })
    );
}

#[test]
fn attached_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(4),
        answer_result: IpcResult::Attached {
            client_id: ClientId::new(),
            session_id: SessionId::new(),
            session_structure: populated_structure(),
            resume_token: None,
            pane_area: None,
        },
    };

    assert_eq!(round_trip_wire_message(&response), response);
}

#[test]
fn an_attached_response_carrying_a_resume_token_keeps_the_secret_whole() {
    let response = IpcResponse {
        request_id: Some(4),
        answer_result: IpcResult::Attached {
            client_id: ClientId::from_uuid(build_fixed_test_uuid()),
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            session_structure: populated_structure(),
            resume_token: Some(build_test_connection_token()),
            pane_area: None,
        },
    };

    let IpcResult::Attached {
        resume_token: Some(carried),
        ..
    } = round_trip_wire_message(&response).answer_result
    else {
        panic!("an attached answer carrying a resume token decodes as one");
    };

    assert_eq!(carried.expose(), build_test_connection_token().expose());
}

#[test]
fn an_attached_response_written_without_the_resume_token_decodes_as_no_token() {
    // An attached answer written without `resume_token` decodes with
    // `resume_token: None`.
    let decoded: IpcResponse = serde_json::from_str(
        r#"{"request_id":4,"answer_result":{"Attached":{"client_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000001","session_structure":{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","tabs":[],"panes":[]}}}}"#,
    )
    .expect("an attached answer without the resume token field decodes");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(4),
            answer_result: IpcResult::Attached {
                client_id: ClientId::from_uuid(build_fixed_test_uuid()),
                session_id: SessionId::from_uuid(build_fixed_test_uuid()),
                session_structure: AttachedSessionStructureSnapshot {
                    session_id: SessionId::from_uuid(build_fixed_test_uuid()),
                    session_name: "quiet-lake".to_string(),
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
        r#"{"request_id":4,"answer_result":{"Attached":{"client_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000001","session_structure":{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","tabs":[{"tab_id":"00000000-0000-0000-0000-000000000001","tab_name":"editor","tab_index":0,"layout":{"Pane":"00000000-0000-0000-0000-000000000001"},"focus_mru":["00000000-0000-0000-0000-000000000001"]}],"panes":[{"pane_id":"00000000-0000-0000-0000-000000000001","pane_kind":"Terminal"}]},"resume_token":null}}}"#,
    )
    .expect("an attached answer without the pane area field decodes");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(4),
            answer_result: IpcResult::Attached {
                client_id: ClientId::from_uuid(build_fixed_test_uuid()),
                session_id: SessionId::from_uuid(build_fixed_test_uuid()),
                session_structure: populated_structure(),
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
        r#"{"request_id":4,"tier":"admin","request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"All"}}}"#,
    );

    // The same frame without `tier` decodes in
    // `an_attach_naming_its_own_authority_carries_none_of_it`.
    let error = decoded.expect_err("an unknown envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `tier`, expected `request_id` or `request_kind` at line 1 column 22"
    );
}

#[test]
fn an_attach_envelope_naming_where_it_connected_from_is_refused() {
    // An attach frame naming `origin` beside `request_id` and `kind` fails to
    // decode.
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":4,"origin":"Remote","request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"All"}}}"#,
    );

    let error = decoded.expect_err("an unknown envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `origin`, expected `request_id` or `request_kind` at line 1 column 24"
    );
}

#[test]
fn an_attach_naming_where_it_connected_from_carries_none_of_it() {
    // An `origin` inside the `Attach` payload is ignored. The decoded request
    // holds the viewport and filter and nothing of it.
    let with_origin: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"All","origin":"Remote"}}}"#,
    )
    .expect("an attach carrying an extra field still decodes");

    assert_eq!(
        with_origin,
        IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Attach {
                viewport: Size {
                    column_count: 80,
                    row_count: 24
                },
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
                cell_size: None,
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
        r#"{"request_id":4,"request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"All","tier":"admin"}}}"#,
    )
    .expect("an attach carrying an extra field still decodes");

    let without_tier: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"request_kind":{"Attach":{"viewport":{"column_count":80,"row_count":24},"event_filter":"All"}}}"#,
    )
    .expect("the same attach without the extra field decodes");

    let expected_attach_request = IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };

    assert_eq!(
        with_tier, expected_attach_request,
        "the authority field left nothing behind in the decoded request"
    );
    assert_eq!(
        without_tier, expected_attach_request,
        "the two frames decode to the same request"
    );
}

#[test]
fn keyboard_request_round_trips() {
    let request = IpcRequest {
        request_id: 5,
        request_kind: IpcRequestKind::Keyboard {
            key_input: build_control_c_key_input(),
        },
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn a_keyboard_request_carries_every_reported_field() {
    let all_modifier_flags = KeyModifierFlags::from_bits(0b1111_1111);
    let request = IpcRequest {
        request_id: 5,
        request_kind: IpcRequestKind::Keyboard {
            key_input: KeyInput {
                key: KeyIdentity::Key(Key::Char('1')),
                key_event_kind: KeyEventKind::Release,
                shifted_key: Some('!'),
                base_layout_key: Some('q'),
                associated_text: "e\u{301}".to_string(),
                modifier_flags: all_modifier_flags,
            },
        },
    };

    let decoded_request = round_trip_wire_message(&request);

    let IpcRequestKind::Keyboard { key_input } = decoded_request.request_kind else {
        panic!("expected a Keyboard request");
    };
    assert_eq!(key_input.key, KeyIdentity::Key(Key::Char('1')));
    assert_eq!(key_input.key_event_kind, KeyEventKind::Release);
    assert_eq!(key_input.shifted_key, Some('!'));
    assert_eq!(key_input.base_layout_key, Some('q'));
    assert_eq!(key_input.associated_text, "e\u{301}");
    assert_eq!(key_input.modifier_flags, all_modifier_flags);
}

/// The complete key event a viewer reads for `Ctrl+c`: a press, with no
/// alternative keys and no associated text.
fn build_control_c_key_input() -> KeyInput {
    KeyInput {
        key: KeyIdentity::Key(Key::Char('c')),
        key_event_kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags: KeyModifierFlags::CTRL,
    }
}

#[test]
fn resize_request_round_trips() {
    let request = IpcRequest {
        request_id: 6,
        request_kind: IpcRequestKind::Resize {
            viewport: Size {
                column_count: 120,
                row_count: 40,
            },
            pane_area: None,
            cell_size: None,
        },
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn a_resize_request_reporting_a_starving_pane_area_round_trips() {
    let request = IpcRequest {
        request_id: 6,
        request_kind: IpcRequestKind::Resize {
            viewport: Size {
                column_count: 2,
                row_count: 2,
            },
            pane_area: Some(PaneArea::Starving),
            cell_size: None,
        },
    };

    assert_eq!(round_trip_wire_message(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 6,
            "request_kind": {
                "Resize": { "viewport": { "column_count": 2, "row_count": 2 }, "pane_area": "Starving" }
            }
        })
    );
}

#[test]
fn a_resize_request_written_without_a_pane_area_decodes_as_none() {
    // A resize written without `pane_area` decodes with `pane_area: None`.
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":6,"request_kind":{"Resize":{"viewport":{"column_count":120,"row_count":40}}}}"#,
    )
    .expect("a resize without the pane area field decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 6,
            request_kind: IpcRequestKind::Resize {
                viewport: Size {
                    column_count: 120,
                    row_count: 40,
                },
                pane_area: None,
                cell_size: None,
            },
        }
    );
}

#[test]
fn every_mouse_action_round_trips() {
    for action in every_mouse_action() {
        assert_eq!(round_trip_wire_message(&action), action);
    }
}

#[test]
fn a_mouse_request_keeps_its_round_in_the_order_it_was_sent() {
    // Three actions that differ from one another: a reordered or dropped one
    // changes the decoded round.
    let pane = PaneId::from_uuid(build_fixed_test_uuid());
    let request = IpcRequest {
        request_id: 7,
        request_kind: IpcRequestKind::Mouse(vec![
            WireMouseAction::Scroll {
                pane_id: pane,
                is_scrolling_up: true,
                scroll_line_count: 3,
            },
            WireMouseAction::Command(Box::new(Command::ToggleLockMode(
                ToggleLockModeArgs::default(),
            ))),
            WireMouseAction::Resize {
                pane_id: pane,
                border_side: Direction::Left,
                resize_step: -1,
                requested_cell_count: 2,
            },
        ]),
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn a_mouse_action_carrying_an_unknown_field_ignores_it() {
    let with_pixels: IpcRequest = serde_json::from_str(
        r#"{"request_id":7,"request_kind":{"Mouse":[{"Scroll":{"pane_id":"00000000-0000-0000-0000-000000000001","is_scrolling_up":true,"scroll_line_count":3,"pixels":9}}]}}"#,
    )
    .expect("a field this build does not know is ignored");

    let without_it: IpcRequest = serde_json::from_str(
        r#"{"request_id":7,"request_kind":{"Mouse":[{"Scroll":{"pane_id":"00000000-0000-0000-0000-000000000001","is_scrolling_up":true,"scroll_line_count":3}}]}}"#,
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
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
                is_scrolling_up: true,
                scroll_line_count: 3,
            }]),
        }
    );
}

#[test]
fn pane_command_requests_round_trip_without_a_protocol_change() {
    let commands = [
        Command::MovePane(MovePaneArgs {
            pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
            direction: Direction::Left,
        }),
        Command::SwapPanes(SwapPanesArgs {
            source_pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
            target_pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
        }),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
            placement_target: PanePlacementTarget::Split {
                destination_tab_id: TabId::from_uuid(build_fixed_test_uuid()),
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Down,
            },
            expected_placement_revision: None,
        }),
        Command::ScrollPane(ScrollPaneArgs {
            pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
            scroll_line_count: -7,
        }),
    ];

    for (request_id, command) in commands.into_iter().enumerate() {
        let request = IpcRequest {
            request_id: request_id as u64,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(CommandEnvelope::from_parts(
                CommandId::from_uuid(build_fixed_test_uuid()),
                CommandSource::ExternalCli {
                    session_id: None,
                    target_client_id: None,
                },
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                command,
            ))),
        };
        assert_eq!(round_trip_wire_message(&request), request);
    }
}

#[test]
fn submit_command_request_round_trips() {
    let request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(build_test_command_envelope())),
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn discovery_request_round_trips() {
    let request = IpcRequest {
        request_id: 3,
        request_kind: IpcRequestKind::Discovery,
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn discovery_request_encodes_to_the_expected_shape() {
    let request = IpcRequest {
        request_id: 3,
        request_kind: IpcRequestKind::Discovery,
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 3, "request_kind": "Discovery" })
    );
}

#[test]
fn paste_request_round_trips_its_text_whole() {
    let request = IpcRequest {
        request_id: 8,
        request_kind: IpcRequestKind::Paste {
            pasted_text: "hello\nworld\u{1b}[A\ttab \u{0} 日本語 🐚".to_string(),
        },
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn recent_events_request_round_trips() {
    let request = IpcRequest {
        request_id: 9,
        request_kind: IpcRequestKind::RecentEvents,
    };

    assert_eq!(round_trip_wire_message(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 9, "request_kind": "RecentEvents" })
    );
}

#[test]
fn leaving_request_round_trips() {
    let request = IpcRequest {
        request_id: 10,
        request_kind: IpcRequestKind::Leaving,
    };

    assert_eq!(round_trip_wire_message(&request), request);
    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 10, "request_kind": "Leaving" })
    );
}

#[test]
fn hello_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(1),
        answer_result: IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: "0.3.0".to_string(),
        },
    };

    assert_eq!(round_trip_wire_message(&response), response);
}

#[test]
fn a_hello_response_written_without_the_build_version_decodes_as_empty() {
    // A Hello answer written without `build_version` decodes with `build_version` empty.
    let decoded: IpcResponse = serde_json::from_str(
        r#"{"request_id":1,"answer_result":{"Hello":{"protocol_version":2}}}"#,
    )
    .expect("a hello answer without the build version decodes");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(1),
            answer_result: IpcResult::Hello {
                protocol_version: 2,
                build_version: String::new(),
            },
        }
    );
}

#[test]
fn a_hello_answer_carrying_an_unknown_field_ignores_it() {
    let decoded: IpcResponse = serde_json::from_str(
        r#"{"request_id":1,"answer_result":{"Hello":{"protocol_version":2,"build_version":"0.3.0","build_date":"2026-01-01"}}}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(1),
            answer_result: IpcResult::Hello {
                protocol_version: 2,
                build_version: "0.3.0".to_string(),
            },
        }
    );
}

#[test]
fn applied_command_result_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(2),
        answer_result: IpcResult::CommandResult(CommandResult::Ok {
            command_id: CommandId::new(),
            emitted_events: Vec::new(),
        }),
    };

    assert_eq!(round_trip_wire_message(&response), response);
}

#[test]
fn rejected_command_result_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(2),
        answer_result: IpcResult::CommandResult(CommandResult::Rejected {
            command_id: CommandId::new(),
            reason: RejectReason::TargetNotFound,
            help: Some("name a session with --session".to_string()),
        }),
    };

    assert_eq!(round_trip_wire_message(&response), response);
}

#[test]
fn overview_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(3),
        answer_result: IpcResult::Overview(build_empty_session_overview()),
    };

    assert_eq!(round_trip_wire_message(&response), response);
}

#[test]
fn a_layout_request_naming_one_tab_round_trips() {
    let request = IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Layout {
            tab_id: Some(TabId::from_uuid(build_fixed_test_uuid())),
        },
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn a_layout_request_naming_no_tab_round_trips() {
    let request = IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Layout { tab_id: None },
    };

    assert_eq!(round_trip_wire_message(&request), request);
}

#[test]
fn a_layout_request_encodes_to_the_expected_shape() {
    let request = IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Layout {
            tab_id: Some(TabId::from_uuid(build_fixed_test_uuid())),
        },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({
            "request_id": 4,
            "request_kind": { "Layout": { "tab_id": "00000000-0000-0000-0000-000000000001" } }
        })
    );
}

#[test]
fn a_layout_request_for_every_tab_encodes_a_null_tab() {
    let request = IpcRequest {
        request_id: 4,
        request_kind: IpcRequestKind::Layout { tab_id: None },
    };

    assert_eq!(
        serde_json::to_value(&request).expect("request encodes"),
        json!({ "request_id": 4, "request_kind": { "Layout": { "tab_id": null } } })
    );
}

#[test]
fn a_layout_request_carrying_an_unknown_field_ignores_it() {
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":4,"request_kind":{"Layout":{"tab_id":null,"junk":5}}}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Layout { tab_id: None },
        }
    );
}

#[test]
fn a_layout_request_written_without_a_tab_decodes_as_every_tab() {
    let decoded: IpcRequest =
        serde_json::from_str(r#"{"request_id":4,"request_kind":{"Layout":{}}}"#)
            .expect("a layout request naming no tab decodes");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 4,
            request_kind: IpcRequestKind::Layout { tab_id: None },
        }
    );
}

#[test]
fn a_request_envelope_carrying_an_unknown_field_is_still_refused() {
    // A misspelled `request_id` beside a correct one is refused.
    let decoded: Result<IpcRequest, _> =
        serde_json::from_str(r#"{"request_id":4,"requst_id":9,"request_kind":"Discovery"}"#);

    let error = decoded.expect_err("an unknown envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `requst_id`, expected `request_id` or `request_kind` at line 1 column 27"
    );
}

#[test]
fn layout_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(4),
        answer_result: IpcResult::Layout(build_test_session_layout()),
    };

    assert_eq!(round_trip_wire_message(&response), response);
}

#[test]
fn recent_events_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(9),
        answer_result: IpcResult::RecentEvents(vec![build_test_recent_event()]),
    };

    assert_eq!(round_trip_wire_message(&response), response);
}

#[test]
fn error_response_round_trips() {
    let response = IpcResponse {
        request_id: Some(1),
        answer_result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: "this Koshi speaks protocol 1, the caller speaks 2".to_string(),
        }),
    };

    assert_eq!(round_trip_wire_message(&response), response);
}

#[test]
fn error_response_encodes_its_code_in_snake_case() {
    let response = IpcResponse {
        request_id: Some(4),
        answer_result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "open the connection first".to_string(),
        }),
    };

    assert_eq!(
        serde_json::to_value(&response).expect("response encodes"),
        json!({
            "request_id": 4,
            "answer_result": {
                "Error": { "code": "HelloRequired", "message": "open the connection first" }
            }
        })
    );
}

#[test]
fn a_refusal_naming_the_other_users_setting_round_trips() {
    let response = IpcResponse {
        request_id: Some(7),
        answer_result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::OtherUsersOff,
            message: "this Koshi serves only the user who started it".to_string(),
        }),
    };

    assert_eq!(round_trip_wire_message(&response), response);
    assert_eq!(
        serde_json::to_value(&response).expect("response encodes"),
        json!({
            "request_id": 7,
            "answer_result": {
                "Error": {
                    "code": "OtherUsersOff",
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
    let ipc_error_payload: IpcErrorPayload =
        serde_json::from_str(r#"{"code":"rate_limited","message":"too many attach requests"}"#)
            .expect("error payload decodes");

    assert_eq!(
        ipc_error_payload,
        IpcErrorPayload {
            code: IpcErrorCode::Unknown,
            message: "too many attach requests".to_string(),
        }
    );
}

#[test]
fn a_refusal_written_without_a_code_reads_as_unknown() {
    let ipc_error_payload: IpcErrorPayload =
        serde_json::from_str(r#"{"message":"too many attach requests"}"#)
            .expect("a refusal without a code decodes");

    assert_eq!(
        ipc_error_payload,
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
        IpcErrorCode::BadToken => "BadToken",
        IpcErrorCode::UnsupportedVersion => "UnsupportedVersion",
        IpcErrorCode::UnsupportedKind => "UnsupportedKind",
        IpcErrorCode::MalformedRequest => "MalformedRequest",
        IpcErrorCode::NotFound => "NotFound",
        IpcErrorCode::HelloRequired => "HelloRequired",
        IpcErrorCode::OtherUsersOff => "OtherUsersOff",
        IpcErrorCode::Unknown => "Unknown",
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
        answer_result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the request could not be read".to_string(),
        }),
    };

    assert_eq!(round_trip_wire_message(&response), response);
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
                viewport: Size {
                    column_count: 80,
                    row_count: 24
                },
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            })
            .unwrap()
        ),
        "Attach"
    );
    assert_eq!(
        tag_of(
            &serde_json::to_value(IpcRequestKind::SubmitCommand(Box::new(
                build_test_command_envelope()
            )))
            .unwrap()
        ),
        "SubmitCommand"
    );
    assert_eq!(
        serde_json::to_value(IpcRequestKind::Discovery).unwrap(),
        json!("Discovery")
    );
    assert_eq!(
        tag_of(&serde_json::to_value(IpcRequestKind::Layout { tab_id: None }).unwrap()),
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
                build_version: "0.3.0".to_string(),
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
                session_structure: populated_structure(),
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
        tag_of(&serde_json::to_value(IpcResult::Overview(build_empty_session_overview())).unwrap()),
        "Overview"
    );
    assert_eq!(
        tag_of(&serde_json::to_value(IpcResult::Layout(build_test_session_layout())).unwrap()),
        "Layout"
    );
    assert_eq!(
        tag_of(
            &serde_json::to_value(IpcResult::RecentEvents(vec![build_test_recent_event()]))
                .unwrap()
        ),
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
    for request_kind in every_request_kind() {
        let encoded_json = serde_json::to_value(&request_kind).expect("request kind encodes");

        assert_eq!(
            tag_of(&encoded_json),
            request_kind.get_request_kind_name(),
            "{request_kind:?}"
        );
    }
}

#[test]
fn every_ipc_result_variant_has_its_wire_name() {
    for ipc_result in list_all_ipc_results() {
        let encoded_json = serde_json::to_value(&ipc_result).expect("the IPC result encodes");

        assert_eq!(
            tag_of(&encoded_json),
            ipc_result.wire_name(),
            "{ipc_result:?}"
        );
    }
}

#[test]
fn variants_lists_every_request_kind_in_declaration_order() {
    let names: Vec<&str> = every_request_kind()
        .iter()
        .map(IpcRequestKind::get_request_kind_name)
        .collect();

    assert_eq!(names, IpcRequestKind::VARIANTS);
}

#[test]
fn variants_lists_every_result_in_declaration_order() {
    let names: Vec<&str> = list_all_ipc_results()
        .iter()
        .map(IpcResult::wire_name)
        .collect();

    assert_eq!(names, IpcResult::VARIANTS);
}

#[test]
fn a_request_naming_a_kind_this_build_does_not_have_reads_as_unknown() {
    let decoded: IncomingRequest =
        serde_json::from_str(r#"{"request_id":9,"request_kind":{"Floating":{"pane_id":3}}}"#)
            .expect("a kind this build does not have decodes as unknown");

    assert_eq!(
        decoded,
        IncomingRequest {
            request_id: 9,
            request_kind: MaybeKnown::Unknown {
                variant_name: "Floating".to_string(),
            },
        }
    );
}

#[test]
fn a_request_naming_a_kind_this_build_has_reads_as_known() {
    let decoded: IncomingRequest =
        serde_json::from_str(r#"{"request_id":9,"request_kind":{"Layout":{"tab_id":null}}}"#)
            .expect("a kind this build has decodes as known");

    assert_eq!(
        decoded,
        IncomingRequest {
            request_id: 9,
            request_kind: MaybeKnown::Known(IpcRequestKind::Layout { tab_id: None }),
        }
    );
}

#[test]
fn a_response_naming_a_result_this_build_does_not_have_reads_as_unknown() {
    let decoded: IncomingResponse =
        serde_json::from_str(r#"{"request_id":9,"answer_result":"Rebooted"}"#)
            .expect("a result this build does not have decodes as unknown");

    assert_eq!(
        decoded,
        IncomingResponse {
            request_id: Some(9),
            answer_result: MaybeKnown::Unknown {
                variant_name: "Rebooted".to_string(),
            },
        }
    );
}

#[test]
fn a_response_naming_a_result_this_build_has_reads_as_known() {
    let decoded: IncomingResponse =
        serde_json::from_str(r#"{"request_id":9,"answer_result":"Restarting"}"#)
            .expect("a result this build has decodes as known");

    assert_eq!(
        decoded,
        IncomingResponse {
            request_id: Some(9),
            answer_result: MaybeKnown::Known(IpcResult::Restarting),
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
        r#"{"requst_id":7,"answer_result":{"Hello":{"protocol_version":2}},"request_id":7}"#,
    );

    let error = decoded.expect_err("a misspelled envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `requst_id`, expected `request_id` or `answer_result` at line 1 column 12"
    );
}

#[test]
fn a_response_envelope_this_build_reads_decodes() {
    let decoded: IpcResponse = serde_json::from_str(
        r#"{"request_id":7,"answer_result":{"Hello":{"protocol_version":2}}}"#,
    )
    .expect("the same bytes without the misspelling decode");

    assert_eq!(
        decoded,
        IpcResponse {
            request_id: Some(7),
            answer_result: IpcResult::Hello {
                protocol_version: 2,
                build_version: String::new(),
            },
        }
    );
}

#[test]
fn a_request_carrying_an_unknown_field_is_refused() {
    let decoded: Result<IpcRequest, _> =
        serde_json::from_str(r#"{"request_id":1,"request_kind":"Discovery","junk":5}"#);

    let error = decoded.expect_err("an unknown envelope field decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "unknown field `junk`, expected `request_id` or `request_kind` at line 1 column 49"
    );
}

/// A field inside the Hello payload that this build does not have is ignored.
/// The envelope around it refuses one.
#[test]
fn a_hello_carrying_an_unknown_field_ignores_it() {
    let decoded: IpcRequest = serde_json::from_str(
        r#"{"request_id":1,"request_kind":{"Hello":{"min_protocol_version":2,"max_protocol_version":2,"connection_token":"k7QxSecret","junk":5}}}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded,
        IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: 2,
                max_protocol_version: 2,
                connection_token: build_test_connection_token(),
                is_remote: false,
            },
        }
    );
}

/// A Hello without `min_protocol_version` is refused; no default fills it in.
#[test]
fn a_hello_missing_a_version_is_refused() {
    let decoded: Result<IpcRequest, _> = serde_json::from_str(
        r#"{"request_id":1,"request_kind":{"Hello":{"max_protocol_version":2,"connection_token":"k7QxSecret"}}}"#,
    );

    let error = decoded.expect_err("a Hello missing a version decoded instead of failing");
    assert_eq!(
        error.to_string(),
        "missing field `min_protocol_version` at line 1 column 98"
    );
}

/// `IpcRequestKind::build_hello_request` fills `min_protocol_version` with
/// `MIN_PROTOCOL_VERSION` and `max_protocol_version` with `PROTOCOL_VERSION`,
/// carries the token through, and sets `remote` to `false`.
#[test]
fn the_hello_this_build_sends_carries_the_range_it_speaks() {
    let IpcRequestKind::Hello {
        min_protocol_version,
        max_protocol_version,
        connection_token: carried,
        is_remote: false,
    } = IpcRequestKind::build_hello_request(build_test_connection_token())
    else {
        panic!("the constructor builds a Hello");
    };

    assert_eq!(min_protocol_version, MIN_PROTOCOL_VERSION);
    assert_eq!(max_protocol_version, PROTOCOL_VERSION);
    assert!(
        min_protocol_version <= max_protocol_version,
        "the lowest version this build speaks is not above its highest"
    );
    assert_eq!(
        carried,
        build_test_connection_token(),
        "the endpoint's token is carried through"
    );
}

#[test]
fn token_encodes_as_a_bare_string() {
    assert_eq!(
        serde_json::to_value(build_test_connection_token()).expect("token encodes"),
        json!("k7QxSecret")
    );
}

#[test]
fn token_decodes_from_a_bare_string() {
    let decoded: ConnectionToken =
        serde_json::from_str(r#""k7QxSecret""#).expect("a bare string decodes as a token");

    assert_eq!(decoded, build_test_connection_token());
}

#[test]
fn token_debug_hides_the_secret() {
    assert_eq!(
        format!("{:?}", build_test_connection_token()),
        "ConnectionToken(***)"
    );
}

#[test]
fn token_display_hides_the_secret() {
    assert_eq!(build_test_connection_token().to_string(), "***");
}

#[test]
fn nesting_a_token_in_a_request_keeps_it_out_of_debug_output() {
    let request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            connection_token: build_test_connection_token(),
            is_remote: false,
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
            connection_token: build_test_connection_token(),
            is_remote: false,
        }
        .get_request_kind_name(),
        "Hello"
    );
    assert_eq!(
        IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24
            },
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        }
        .get_request_kind_name(),
        "Attach"
    );
    assert_eq!(
        IpcRequestKind::Keyboard {
            key_input: build_control_c_key_input(),
        }
        .get_request_kind_name(),
        "Keyboard"
    );
    assert_eq!(
        IpcRequestKind::Resize {
            viewport: Size {
                column_count: 120,
                row_count: 40,
            },
            pane_area: None,
            cell_size: None,
        }
        .get_request_kind_name(),
        "Resize"
    );
    assert_eq!(
        IpcRequestKind::Paste {
            pasted_text: String::from("hello\nworld"),
        }
        .get_request_kind_name(),
        "Paste"
    );
    assert_eq!(
        IpcRequestKind::Mouse(every_mouse_action()).get_request_kind_name(),
        "Mouse"
    );
    assert_eq!(
        IpcRequestKind::SubmitCommand(Box::new(build_test_command_envelope()))
            .get_request_kind_name(),
        "SubmitCommand"
    );
    assert_eq!(
        IpcRequestKind::Discovery.get_request_kind_name(),
        "Discovery"
    );
    assert_eq!(
        IpcRequestKind::Layout { tab_id: None }.get_request_kind_name(),
        "Layout"
    );
    assert_eq!(
        IpcRequestKind::Layout {
            tab_id: Some(TabId::from_uuid(build_fixed_test_uuid())),
        }
        .get_request_kind_name(),
        "Layout"
    );
    assert_eq!(
        IpcRequestKind::RecentEvents.get_request_kind_name(),
        "RecentEvents"
    );
    assert_eq!(IpcRequestKind::Restart.get_request_kind_name(), "Restart");
    assert_eq!(IpcRequestKind::Leaving.get_request_kind_name(), "Leaving");
}

/// Serializing a Hello writes the real secret.
#[test]
fn serializing_a_hello_writes_the_real_secret() {
    let request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            connection_token: build_test_connection_token(),
            is_remote: false,
        },
    };

    let encoded_json = serde_json::to_string(&request).expect("request encodes");

    assert!(encoded_json.contains("k7QxSecret"), "{encoded_json}");
}

#[test]
fn tokens_holding_the_same_secret_are_equal() {
    assert_eq!(
        ConnectionToken::from_secret("k7QxSecret"),
        build_test_connection_token()
    );
}

#[test]
fn tokens_differing_in_one_byte_are_not_equal() {
    assert_ne!(
        ConnectionToken::from_secret("k7QxSecreT"),
        build_test_connection_token()
    );
}

#[test]
fn tokens_differing_in_the_first_byte_are_not_equal() {
    assert_ne!(
        ConnectionToken::from_secret("K7QxSecret"),
        build_test_connection_token()
    );
}

#[test]
fn a_token_that_is_a_prefix_of_another_is_not_equal() {
    assert_ne!(
        ConnectionToken::from_secret("k7QxSecre"),
        build_test_connection_token()
    );
}

#[test]
fn an_empty_token_is_not_equal_to_a_real_one() {
    assert_ne!(
        ConnectionToken::from_secret(""),
        build_test_connection_token()
    );
}

#[test]
fn two_empty_tokens_are_equal() {
    assert_eq!(
        ConnectionToken::from_secret(""),
        ConnectionToken::from_secret("")
    );
}

#[test]
fn expose_returns_the_secret_for_writing_it_to_the_endpoint_file() {
    assert_eq!(build_test_connection_token().expose(), "k7QxSecret");
}

#[test]
fn a_generated_token_is_64_lowercase_hex_characters() {
    let token = ConnectionToken::generate();
    let secret = token.expose();
    assert_eq!(secret.len(), 64, "{secret}");
    assert!(
        secret
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
        "{secret}"
    );
}

#[test]
fn two_generated_tokens_differ() {
    assert_ne!(ConnectionToken::generate(), ConnectionToken::generate());
}
