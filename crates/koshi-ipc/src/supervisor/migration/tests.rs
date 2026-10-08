//! Version 1 supervisor frames converted at a live session handoff.

use std::path::PathBuf;
use std::time::Duration;

use koshi_core::ids::PaneId;
use koshi_core::process::{KillPolicy, PtySize, ShellKind, SpawnSpec};

use crate::protocol::ConnectionToken;
use crate::supervisor::{
    SupervisorEvent, SupervisorMessage, SupervisorRequestKind, SupervisorResult,
};
use crate::wire::MaybeKnown;

use super::*;

#[test]
fn serialize_previous_hello_uses_version_one_and_previous_field_names() {
    let request = SupervisorRequest {
        request_id: 7,
        request_kind: SupervisorRequestKind::Hello {
            minimum_protocol_version: 2,
            maximum_protocol_version: 2,
            connection_token: ConnectionToken::from_secret("secret"),
        },
    };
    assert_eq!(
        serialize_previous_supervisor_request(&request).expect("serialize previous Hello"),
        serde_json::json!({
            "request_id": 7,
            "kind": {"Hello": {
                "min_protocol_version": 1,
                "max_protocol_version": 1,
                "token": "secret"
            }}
        })
    );
}

#[test]
fn serialize_previous_spawn_write_resize_and_kill_preserves_payloads() {
    let pane_id = PaneId::new();
    let spawn_spec = SpawnSpec {
        program: PathBuf::from("cmd.exe"),
        arguments: vec!["/C".to_string(), "echo".to_string()],
        working_directory: Some(PathBuf::from("C:\\work")),
        environment_variables: [("KOSHI_TEST".to_string(), "yes".to_string())]
            .into_iter()
            .collect(),
        shell_kind: ShellKind::Other("cmd".to_string()),
    };
    let spawn = SupervisorRequest {
        request_id: 8,
        request_kind: SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec,
            pty_size: PtySize {
                column_count: 80,
                row_count: 24,
            },
        },
    };
    assert_eq!(
        serialize_previous_supervisor_request(&spawn).expect("serialize previous Spawn"),
        serde_json::json!({
            "request_id": 8,
            "kind": {"Spawn": {
                "pane_id": pane_id,
                "spec": {
                    "program": "cmd.exe",
                    "args": ["/C", "echo"],
                    "cwd": "C:\\work",
                    "env": {"KOSHI_TEST": "yes"},
                    "shell_kind": {"Other": "cmd"}
                },
                "size": {"cols": 80, "rows": 24}
            }}
        })
    );
    let resize = SupervisorRequest {
        request_id: 9,
        request_kind: SupervisorRequestKind::Resize {
            pane_id,
            pty_size: PtySize {
                column_count: 100,
                row_count: 30,
            },
        },
    };
    assert_eq!(
        serialize_previous_supervisor_request(&resize).expect("serialize previous Resize"),
        serde_json::json!({"request_id": 9, "kind": {"Resize": {
            "pane_id": pane_id, "size": {"cols": 100, "rows": 30}
        }}})
    );
    let write = SupervisorRequest {
        request_id: 10,
        request_kind: SupervisorRequestKind::Write {
            pane_id,
            input_bytes: b"hi".to_vec(),
        },
    };
    assert_eq!(
        serialize_previous_supervisor_request(&write).expect("serialize previous Write"),
        serde_json::json!({"request_id": 10, "kind": {"Write": {
            "pane_id": pane_id, "bytes": "aGk="
        }}})
    );
    let kill = SupervisorRequest {
        request_id: 11,
        request_kind: SupervisorRequestKind::Kill {
            pane_id,
            kill_policy: KillPolicy::GracefulTree {
                timeout_duration: Duration::from_secs(3),
            },
        },
    };
    assert_eq!(
        serialize_previous_supervisor_request(&kill).expect("serialize previous Kill"),
        serde_json::json!({"request_id": 11, "kind": {"Kill": {
            "pane_id": pane_id, "kill_policy": {"GracefulTree": {"timeout": 3}}
        }}})
    );
}

#[test]
fn deserialize_previous_supervisor_responses_and_events_preserves_panes_and_bytes() {
    let pane_id = PaneId::new();
    let previous_panes = serde_json::json!({"Response": {
        "request_id": 12,
        "result": {"Panes": [{
            "pane_id": pane_id, "pid": 51234, "size": {"cols": 80, "rows": 24}
        }]}
    }});
    assert_eq!(
        deserialize_previous_supervisor_message(previous_panes).expect("deserialize pane list"),
        SupervisorMessage::Response(crate::supervisor::SupervisorResponse {
            request_id: Some(12),
            answer_result: MaybeKnown::Known(SupervisorResult::Panes(vec![
                crate::supervisor::SupervisorPane {
                    pane_id,
                    process_id: 51234,
                    pty_size: PtySize {
                        column_count: 80,
                        row_count: 24
                    },
                }
            ])),
        })
    );
    let previous_output = serde_json::json!({"Event": {"Output": {
        "pane_id": pane_id, "bytes": "aGk="
    }}});
    assert_eq!(
        deserialize_previous_supervisor_message(previous_output).expect("deserialize pane output"),
        SupervisorMessage::Event(MaybeKnown::Known(SupervisorEvent::Output {
            pane_id,
            output_bytes: b"hi".to_vec(),
        }))
    );
    let previous_exit = serde_json::json!({"Event": {"Exited": {
        "pane_id": pane_id, "status": {"ExitCode": 7}
    }}});
    assert_eq!(
        deserialize_previous_supervisor_message(previous_exit).expect("deserialize pane exit"),
        SupervisorMessage::Event(MaybeKnown::Known(SupervisorEvent::Exited {
            pane_id,
            exit_status: koshi_core::process::ExitStatus::ExitCode(7),
        }))
    );
}
