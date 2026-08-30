//! Tests for the runtime inbox: what a [`RuntimeEvent`] variant carries, and
//! how an [`EndingNotice`] holds the session's last frame and counts the
//! client writing threads.

use super::*;
use koshi_core::command::{Command, CommandSource, ToggleLockModeArgs};
use koshi_core::ids::CommandId;
use koshi_core::key::{Key, ModFlags};
use std::time::SystemTime;

/// A deterministic, boundary-free envelope for the IPC/plugin variants.
fn envelope() -> CommandEnvelope {
    CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Internal,
        SystemTime::UNIX_EPOCH,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
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
        pane_area: None,
    };
    let RuntimeEvent::Resize {
        client_id,
        size,
        pane_area,
    } = &event
    else {
        panic!("expected Resize");
    };
    assert_eq!(*client_id, client);
    assert_eq!(*size, Size { cols: 80, rows: 24 });
    assert_eq!(*pane_area, None);
}

#[test]
fn resize_carries_a_reported_pane_area() {
    let client = ClientId::new();
    let reported = PaneArea::Reported(Size { cols: 60, rows: 20 });
    let event = RuntimeEvent::Resize {
        client_id: client,
        size: Size { cols: 80, rows: 24 },
        pane_area: Some(reported),
    };
    let RuntimeEvent::Resize {
        client_id,
        size,
        pane_area,
    } = &event
    else {
        panic!("expected Resize");
    };
    assert_eq!(*client_id, client);
    assert_eq!(*size, Size { cols: 80, rows: 24 });
    assert_eq!(*pane_area, Some(reported));
}

#[test]
fn client_key_press_carries_its_client_and_chord() {
    let client = ClientId::new();
    let pressed = KeyChord::new(ModFlags::CTRL, Key::Char('t'));
    let event = RuntimeEvent::ClientKeyPress {
        client_id: client,
        chord: pressed,
    };
    let RuntimeEvent::ClientKeyPress { client_id, chord } = &event else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(*client_id, client);
    assert_eq!(*chord, pressed);
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
fn an_ending_notice_starts_empty_and_holds_the_ending_it_was_raised_with() {
    for ending in [SessionEnding::Quit, SessionEnding::Restarting] {
        let notice = EndingNotice::default();
        assert_eq!(notice.raised(), None);
        notice.raise(ending);
        assert_eq!(notice.raised(), Some(ending));
        notice.raise(ending);
        assert_eq!(notice.raised(), Some(ending));
    }
}

#[test]
fn an_ending_notice_keeps_the_first_ending_when_a_second_one_is_raised() {
    let notice = EndingNotice::default();

    notice.raise(SessionEnding::Restarting);
    notice.raise(SessionEnding::Quit);

    assert_eq!(notice.raised(), Some(SessionEnding::Restarting));
}

#[test]
fn an_ending_notice_counts_every_writing_thread_from_start_to_end() {
    let notice = EndingNotice::default();
    assert_eq!(notice.writers_running(), 0);

    notice.writer_started();
    notice.writer_started();
    assert_eq!(notice.writers_running(), 2);

    notice.writer_ended();
    assert_eq!(notice.writers_running(), 1);

    notice.writer_ended();
    assert_eq!(notice.writers_running(), 0);
}

#[test]
fn writing_threads_sharing_one_ending_notice_all_count_into_it() {
    let notice = Arc::new(EndingNotice::default());

    let threads: Vec<_> = (0..8)
        .map(|_| {
            let notice = Arc::clone(&notice);
            std::thread::spawn(move || notice.writer_started())
        })
        .collect();
    for thread in threads {
        thread.join().expect("the counting thread finished");
    }

    assert_eq!(notice.writers_running(), 8);
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
