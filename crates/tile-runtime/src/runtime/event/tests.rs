//! Construction and equality coverage for every [`RuntimeEvent`] variant.

use super::*;
use std::time::SystemTime;
use tile_core::command::{Command, CommandSource};
use tile_core::ids::CommandId;

/// A deterministic, boundary-free envelope for the IPC/plugin variants.
fn envelope() -> CommandEnvelope {
    CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Internal,
        SystemTime::UNIX_EPOCH,
        Command::ToggleLockMode,
    )
}

#[test]
fn pty_output_carries_its_pane_and_bytes() {
    let pane = PaneId::new();
    let event = RuntimeEvent::PtyOutput {
        pane_id: pane,
        bytes: vec![0x68, 0x69],
    };
    let RuntimeEvent::PtyOutput { pane_id, bytes } = &event else {
        panic!("expected PtyOutput");
    };
    assert_eq!(*pane_id, pane);
    assert_eq!(bytes, &[0x68, 0x69]);
}

#[test]
fn child_exit_carries_its_pane_and_status() {
    let pane = PaneId::new();
    let event = RuntimeEvent::ChildExit {
        pane_id: pane,
        status: ExitStatus::Signaled(9),
    };
    let RuntimeEvent::ChildExit { pane_id, status } = &event else {
        panic!("expected ChildExit");
    };
    assert_eq!(*pane_id, pane);
    assert_eq!(*status, ExitStatus::Signaled(9));
}

#[test]
fn resize_carries_its_client_and_size() {
    let client = ClientId::new();
    let event = RuntimeEvent::Resize {
        client_id: client,
        size: Size { cols: 80, rows: 24 },
    };
    let RuntimeEvent::Resize { client_id, size } = &event else {
        panic!("expected Resize");
    };
    assert_eq!(*client_id, client);
    assert_eq!(*size, Size { cols: 80, rows: 24 });
}

#[test]
fn outer_input_carries_its_client_and_bytes() {
    let client = ClientId::new();
    let event = RuntimeEvent::OuterInput {
        client_id: client,
        bytes: vec![0x1b, b'[', b'A'],
    };
    let RuntimeEvent::OuterInput { client_id, bytes } = &event else {
        panic!("expected OuterInput");
    };
    assert_eq!(*client_id, client);
    assert_eq!(bytes, &[0x1b, b'[', b'A']);
}

#[test]
fn ipc_and_plugin_carry_their_envelope() {
    let env = envelope();
    let ipc = RuntimeEvent::Ipc(env.clone());
    let plugin = RuntimeEvent::Plugin(env.clone());
    let RuntimeEvent::Ipc(carried) = &ipc else {
        panic!("expected Ipc");
    };
    assert_eq!(carried, &env);
    let RuntimeEvent::Plugin(carried) = &plugin else {
        panic!("expected Plugin");
    };
    assert_eq!(carried, &env);
}

#[test]
fn equal_payloads_compare_equal() {
    let pane = PaneId::new();
    assert_eq!(
        RuntimeEvent::PtyOutput {
            pane_id: pane,
            bytes: vec![1, 2, 3],
        },
        RuntimeEvent::PtyOutput {
            pane_id: pane,
            bytes: vec![1, 2, 3],
        },
    );
}

#[test]
fn distinct_variants_compare_unequal() {
    let client = ClientId::new();
    assert_ne!(
        RuntimeEvent::Timer,
        RuntimeEvent::OuterInput {
            client_id: client,
            bytes: Vec::new(),
        },
    );
}
