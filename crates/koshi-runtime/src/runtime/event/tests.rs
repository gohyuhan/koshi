//! Construction coverage for every [`RuntimeEvent`] variant.

use super::*;
use koshi_core::command::{Command, CommandSource};
use koshi_core::ids::CommandId;
use std::time::SystemTime;

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
fn child_exit_carries_its_pane_status_and_time() {
    let pane = PaneId::new();
    let event = RuntimeEvent::ChildExit {
        pane_id: pane,
        status: ExitStatus::Signaled(9),
        exited_at: SystemTime::UNIX_EPOCH,
    };
    let RuntimeEvent::ChildExit {
        pane_id,
        status,
        exited_at,
    } = &event
    else {
        panic!("expected ChildExit");
    };
    assert_eq!(*pane_id, pane);
    assert_eq!(*status, ExitStatus::Signaled(9));
    assert_eq!(*exited_at, SystemTime::UNIX_EPOCH);
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
fn ipc_carries_its_envelope_and_a_working_reply_channel() {
    let env = envelope();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let ipc = RuntimeEvent::Ipc {
        envelope: env.clone(),
        reply: reply_tx,
    };
    let RuntimeEvent::Ipc { envelope, reply } = &ipc else {
        panic!("expected Ipc");
    };
    assert_eq!(envelope, &env);
    reply
        .send(CommandResult::Ok {
            command_id: env.id,
            emitted_events: Vec::new(),
        })
        .expect("send on the carried reply channel");
    assert_eq!(
        reply_rx.recv().expect("receive the reply"),
        CommandResult::Ok {
            command_id: env.id,
            emitted_events: Vec::new(),
        },
    );
}

#[test]
fn ipc_discovery_carries_a_working_reply_channel() {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let event = RuntimeEvent::IpcDiscovery { reply: reply_tx };
    let RuntimeEvent::IpcDiscovery { reply } = &event else {
        panic!("expected IpcDiscovery");
    };
    reply.send(None).expect("send on the carried reply channel");
    assert_eq!(reply_rx.recv().expect("receive the reply"), None);
}

#[test]
fn plugin_carries_its_envelope() {
    let env = envelope();
    let plugin = RuntimeEvent::Plugin(env.clone());
    let RuntimeEvent::Plugin(carried) = &plugin else {
        panic!("expected Plugin");
    };
    assert_eq!(carried, &env);
}
