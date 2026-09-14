//! Tests for [`PtyHandle`]: receiver handoff, the `try_*` polls, and detached
//! handles.

use super::*;

use koshi_core::ids::PaneId;

#[test]
fn take_output_and_exit_receivers_hands_them_over_once() {
    let (mut handle, output_sender, exit_sender) = PtyHandle::from_pane_id(PaneId::new());

    let (output_receiver, exit_receiver) =
        handle.take_output_and_exit_receivers().expect("first take");

    // The moved receivers still receive from the backend's senders.
    output_sender.send(b"out".to_vec()).expect("send output");
    assert_eq!(
        output_receiver.recv().expect("recv output"),
        b"out".to_vec()
    );
    exit_sender
        .send(ExitStatus::ExitCode(0))
        .expect("send exit");
    assert_eq!(
        exit_receiver.recv().expect("recv exit"),
        ExitStatus::ExitCode(0)
    );
}

#[test]
fn drained_handle_yields_none() {
    let (mut handle, _output_sender, _exit_sender) = PtyHandle::from_pane_id(PaneId::new());

    handle.take_output_and_exit_receivers().expect("first take");

    assert!(handle.take_output_and_exit_receivers().is_none());
    assert_eq!(handle.try_receive_output_chunk(), None);
    assert_eq!(handle.try_receive_exit_status(), None);
}

#[test]
fn a_drained_handle_still_answers_its_pane_id() {
    let pane_id = PaneId::new();
    let (mut handle, _output_sender, _exit_sender) = PtyHandle::from_pane_id(pane_id);

    handle.take_output_and_exit_receivers().expect("first take");

    assert_eq!(handle.get_pane_id(), pane_id);
}

#[test]
fn a_detached_handle_has_its_pane_id_and_no_receivers() {
    let pane_id = PaneId::new();
    let mut handle = PtyHandle::from_detached_pane_id(pane_id);

    assert_eq!(handle.get_pane_id(), pane_id);
    assert!(handle.take_output_and_exit_receivers().is_none());
    assert_eq!(handle.try_receive_output_chunk(), None);
    assert_eq!(handle.try_receive_exit_status(), None);
}

#[test]
fn an_exit_status_is_read_once() {
    let (handle, _output_sender, exit_sender) = PtyHandle::from_pane_id(PaneId::new());

    exit_sender
        .send(ExitStatus::ExitCode(3))
        .expect("send exit");

    assert_eq!(
        handle.try_receive_exit_status(),
        Some(ExitStatus::ExitCode(3))
    );
    assert_eq!(handle.try_receive_exit_status(), None);
}

#[test]
fn an_empty_chunk_is_delivered_as_an_empty_chunk() {
    let (handle, output_sender, _exit_sender) = PtyHandle::from_pane_id(PaneId::new());

    output_sender.send(Vec::new()).expect("send empty chunk");

    assert_eq!(handle.try_receive_output_chunk(), Some(Vec::new()));
    assert_eq!(handle.try_receive_output_chunk(), None);
}

#[test]
fn output_and_exit_are_separate_queues() {
    let (handle, output_sender, exit_sender) = PtyHandle::from_pane_id(PaneId::new());

    exit_sender
        .send(ExitStatus::ExitCode(0))
        .expect("send exit");

    // An exit pending on its own channel leaves the output channel empty.
    assert_eq!(handle.try_receive_output_chunk(), None);
    output_sender.send(b"late".to_vec()).expect("send output");
    assert_eq!(handle.try_receive_output_chunk(), Some(b"late".to_vec()));
    assert_eq!(
        handle.try_receive_exit_status(),
        Some(ExitStatus::ExitCode(0))
    );
}

#[test]
fn dropping_the_taken_receivers_disconnects_the_senders() {
    let (mut handle, output_sender, exit_sender) = PtyHandle::from_pane_id(PaneId::new());

    let taken_receivers = handle.take_output_and_exit_receivers().expect("first take");
    drop(taken_receivers);
    drop(handle);

    assert_eq!(
        output_sender.send(b"x".to_vec()).unwrap_err().0,
        b"x".to_vec()
    );
    assert_eq!(
        exit_sender.send(ExitStatus::Signaled(15)).unwrap_err().0,
        ExitStatus::Signaled(15)
    );
}

#[test]
fn a_disconnected_channel_reads_as_none_not_a_panic() {
    let (handle, output_sender, exit_sender) = PtyHandle::from_pane_id(PaneId::new());

    drop(output_sender);
    drop(exit_sender);

    // A hung-up backend looks the same as "nothing pending".
    assert_eq!(handle.try_receive_output_chunk(), None);
    assert_eq!(handle.try_receive_exit_status(), None);
}

#[test]
fn output_chunks_arrive_in_send_order() {
    let pane_id = PaneId::new();
    let (handle, output_sender, _exit_sender) = PtyHandle::from_pane_id(pane_id);

    output_sender.send(b"first".to_vec()).expect("send first");
    output_sender.send(b"second".to_vec()).expect("send second");

    assert_eq!(handle.get_pane_id(), pane_id);
    assert_eq!(handle.try_receive_output_chunk(), Some(b"first".to_vec()));
    assert_eq!(handle.try_receive_output_chunk(), Some(b"second".to_vec()));
    assert_eq!(handle.try_receive_output_chunk(), None);
}

#[test]
fn try_reads_work_while_receivers_are_held() {
    let (handle, output_sender, exit_sender) = PtyHandle::from_pane_id(PaneId::new());

    assert_eq!(handle.try_receive_output_chunk(), None);
    output_sender.send(b"x".to_vec()).expect("send output");
    assert_eq!(handle.try_receive_output_chunk(), Some(b"x".to_vec()));

    exit_sender
        .send(ExitStatus::Signaled(9))
        .expect("send exit");
    assert_eq!(
        handle.try_receive_exit_status(),
        Some(ExitStatus::Signaled(9))
    );
}

#[test]
fn dropping_the_handle_disconnects_the_senders() {
    let (handle, output_sender, exit_sender) = PtyHandle::from_pane_id(PaneId::new());

    drop(handle);

    // With the receiving ends gone, each send fails and hands its payload back.
    assert_eq!(
        output_sender.send(b"x".to_vec()).unwrap_err().0,
        b"x".to_vec()
    );
    assert_eq!(
        exit_sender.send(ExitStatus::ExitCode(0)).unwrap_err().0,
        ExitStatus::ExitCode(0)
    );
}
