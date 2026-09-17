//! Tests for the runtime inbox: what a [`RuntimeEvent`] variant carries, and
//! how an [`EndingNotice`] holds the session's last frame and counts the
//! client writing threads.

use super::*;
use koshi_core::command::{Command, CommandSource, ToggleLockModeArgs};
use koshi_core::ids::CommandId;
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_test_support::fixtures::build_key_input_for_chord;
use std::time::SystemTime;

/// A deterministic, boundary-free envelope for the IPC/plugin variants.
fn build_test_command_envelope() -> CommandEnvelope {
    CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::Internal,
        SystemTime::UNIX_EPOCH,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
}

#[test]
fn pty_output_carries_its_pane_and_bytes() {
    let expected_pane_id = PaneId::new();
    let runtime_event = RuntimeEvent::PtyOutput {
        pane_id: expected_pane_id,
        output_bytes: vec![0x68, 0x69],
    };
    let RuntimeEvent::PtyOutput {
        pane_id: carried_pane_id,
        output_bytes,
    } = &runtime_event
    else {
        panic!("expected PtyOutput");
    };
    assert_eq!(*carried_pane_id, expected_pane_id);
    assert_eq!(output_bytes, &[0x68, 0x69]);
}

#[test]
fn child_exit_carries_its_pane_and_status() {
    let expected_pane_id = PaneId::new();
    let runtime_event = RuntimeEvent::ChildExit {
        pane_id: expected_pane_id,
        exit_status: ExitStatus::Signaled(9),
    };
    let RuntimeEvent::ChildExit {
        pane_id: carried_pane_id,
        exit_status,
    } = &runtime_event
    else {
        panic!("expected ChildExit");
    };
    assert_eq!(*carried_pane_id, expected_pane_id);
    assert_eq!(*exit_status, ExitStatus::Signaled(9));
}

#[test]
fn resize_carries_its_client_and_size() {
    let expected_client_id = ClientId::new();
    let runtime_event = RuntimeEvent::Resize {
        client_id: expected_client_id,
        viewport_size: Size {
            column_count: 80,
            row_count: 24,
        },
        pane_area: None,
        cell_size: None,
    };
    let RuntimeEvent::Resize {
        client_id: carried_client_id,
        viewport_size,
        pane_area,
        cell_size,
    } = &runtime_event
    else {
        panic!("expected Resize");
    };
    assert_eq!(*carried_client_id, expected_client_id);
    assert_eq!(
        *viewport_size,
        Size {
            column_count: 80,
            row_count: 24
        }
    );
    assert_eq!(*pane_area, None);
    assert_eq!(*cell_size, None);
}

#[test]
fn resize_carries_a_reported_pane_area() {
    let expected_client_id = ClientId::new();
    let reported_pane_area = PaneArea::Reported(Size {
        column_count: 60,
        row_count: 20,
    });
    let runtime_event = RuntimeEvent::Resize {
        client_id: expected_client_id,
        viewport_size: Size {
            column_count: 80,
            row_count: 24,
        },
        pane_area: Some(reported_pane_area),
        cell_size: None,
    };
    let RuntimeEvent::Resize {
        client_id: carried_client_id,
        viewport_size,
        pane_area,
        cell_size,
    } = &runtime_event
    else {
        panic!("expected Resize");
    };
    assert_eq!(*carried_client_id, expected_client_id);
    assert_eq!(
        *viewport_size,
        Size {
            column_count: 80,
            row_count: 24
        }
    );
    assert_eq!(*pane_area, Some(reported_pane_area));
    assert_eq!(*cell_size, None);
}

#[test]
fn client_keyboard_carries_its_client_and_the_whole_key_input() {
    let expected_client_id = ClientId::new();
    let pressed_key_input =
        build_key_input_for_chord(KeyChord::from_parts(ModFlags::CTRL, Key::Char('t')));
    let runtime_event = RuntimeEvent::ClientKeyboard {
        client_id: expected_client_id,
        key_input: pressed_key_input.clone(),
    };
    let RuntimeEvent::ClientKeyboard {
        client_id: carried_client_id,
        key_input,
    } = &runtime_event
    else {
        panic!("expected ClientKeyboard");
    };
    assert_eq!(*carried_client_id, expected_client_id);
    assert_eq!(*key_input, pressed_key_input);
}

#[test]
fn ipc_carries_its_envelope_and_a_working_reply_channel() {
    let command_envelope = build_test_command_envelope();
    let (reply_sender, reply_receiver) = std::sync::mpsc::channel();
    let ipc_event = RuntimeEvent::Ipc {
        envelope: command_envelope.clone(),
        response_sender: reply_sender,
    };
    let RuntimeEvent::Ipc {
        envelope,
        response_sender,
    } = &ipc_event
    else {
        panic!("expected Ipc");
    };
    assert_eq!(envelope, &command_envelope);
    response_sender
        .send(CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        })
        .expect("send on the carried reply channel");
    assert_eq!(
        reply_receiver.recv().expect("receive the reply"),
        CommandResult::Ok {
            command_id: command_envelope.command_id,
            emitted_events: Vec::new(),
        },
    );
}

#[test]
fn ipc_discovery_carries_a_working_reply_channel() {
    let (reply_sender, reply_receiver) = std::sync::mpsc::channel();
    let runtime_event = RuntimeEvent::IpcDiscovery {
        response_sender: reply_sender,
    };
    let RuntimeEvent::IpcDiscovery { response_sender } = &runtime_event else {
        panic!("expected IpcDiscovery");
    };
    response_sender
        .send(None)
        .expect("send on the carried reply channel");
    assert_eq!(reply_receiver.recv().expect("receive the reply"), None);
}

#[test]
fn an_ending_notice_starts_empty_and_holds_the_ending_it_was_raised_with() {
    for session_ending in [SessionEnding::Quit, SessionEnding::Restarting] {
        let ending_notice = EndingNotice::default();
        assert_eq!(ending_notice.get_session_ending(), None);
        ending_notice.raise_session_ending(session_ending);
        assert_eq!(ending_notice.get_session_ending(), Some(session_ending));
        ending_notice.raise_session_ending(session_ending);
        assert_eq!(ending_notice.get_session_ending(), Some(session_ending));
    }
}

#[test]
fn an_ending_notice_keeps_the_first_ending_when_a_second_one_is_raised() {
    let ending_notice = EndingNotice::default();

    ending_notice.raise_session_ending(SessionEnding::Restarting);
    ending_notice.raise_session_ending(SessionEnding::Quit);

    assert_eq!(
        ending_notice.get_session_ending(),
        Some(SessionEnding::Restarting)
    );
}

#[test]
fn an_ending_notice_counts_every_writing_thread_from_start_to_end() {
    let ending_notice = EndingNotice::default();
    assert_eq!(ending_notice.count_running_writers(), 0);

    ending_notice.record_writer_started();
    ending_notice.record_writer_started();
    assert_eq!(ending_notice.count_running_writers(), 2);

    ending_notice.record_writer_ended();
    assert_eq!(ending_notice.count_running_writers(), 1);

    ending_notice.record_writer_ended();
    assert_eq!(ending_notice.count_running_writers(), 0);
}

#[test]
fn writing_threads_sharing_one_ending_notice_all_count_into_it() {
    let ending_notice = Arc::new(EndingNotice::default());

    let writer_threads: Vec<_> = (0..8)
        .map(|_| {
            let ending_notice = Arc::clone(&ending_notice);
            std::thread::spawn(move || ending_notice.record_writer_started())
        })
        .collect();
    for writer_thread in writer_threads {
        writer_thread.join().expect("the counting thread finished");
    }

    assert_eq!(ending_notice.count_running_writers(), 8);
}

#[test]
fn plugin_carries_its_envelope() {
    let command_envelope = build_test_command_envelope();
    let plugin = RuntimeEvent::Plugin(command_envelope.clone());
    let RuntimeEvent::Plugin(carried_envelope) = &plugin else {
        panic!("expected Plugin");
    };
    assert_eq!(carried_envelope, &command_envelope);
}
