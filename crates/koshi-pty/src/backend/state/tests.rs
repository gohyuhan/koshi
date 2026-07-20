//! Tests for [`PtyHandle`] receiver handoff.

use super::*;

use koshi_core::ids::PaneId;

#[test]
fn take_receivers_hands_them_over_once() {
    let (mut handle, output_tx, exit_tx) = PtyHandle::new(PaneId::new());

    let (output_rx, exit_rx) = handle.take_receivers().expect("first take");

    // The moved receivers still receive from the backend's senders.
    output_tx.send(b"out".to_vec()).expect("send output");
    assert_eq!(output_rx.recv().expect("recv output"), b"out".to_vec());
    exit_tx.send(ExitStatus::ExitCode(0)).expect("send exit");
    assert_eq!(exit_rx.recv().expect("recv exit"), ExitStatus::ExitCode(0));
}

#[test]
fn drained_handle_yields_none() {
    let (mut handle, _output_tx, _exit_tx) = PtyHandle::new(PaneId::new());

    handle.take_receivers().expect("first take");

    assert!(handle.take_receivers().is_none());
    assert!(handle.try_read_output().is_none());
    assert!(handle.try_exit_status().is_none());
}

#[test]
fn a_disconnected_channel_reads_as_none_not_a_panic() {
    let (handle, output_tx, exit_tx) = PtyHandle::new(PaneId::new());

    drop(output_tx);
    drop(exit_tx);

    // A hung-up backend looks the same as "nothing pending".
    assert_eq!(handle.try_read_output(), None);
    assert_eq!(handle.try_exit_status(), None);
}

#[test]
fn output_chunks_arrive_in_send_order() {
    let id = PaneId::new();
    let (handle, output_tx, _exit_tx) = PtyHandle::new(id);

    output_tx.send(b"first".to_vec()).expect("send first");
    output_tx.send(b"second".to_vec()).expect("send second");

    assert_eq!(handle.pane_id(), id);
    assert_eq!(handle.try_read_output(), Some(b"first".to_vec()));
    assert_eq!(handle.try_read_output(), Some(b"second".to_vec()));
    assert_eq!(handle.try_read_output(), None);
}

#[test]
fn try_reads_work_while_receivers_are_held() {
    let (handle, output_tx, exit_tx) = PtyHandle::new(PaneId::new());

    assert!(handle.try_read_output().is_none());
    output_tx.send(b"x".to_vec()).expect("send output");
    assert_eq!(handle.try_read_output(), Some(b"x".to_vec()));

    exit_tx.send(ExitStatus::Signaled(9)).expect("send exit");
    assert_eq!(handle.try_exit_status(), Some(ExitStatus::Signaled(9)));
}

#[test]
fn dropping_the_handle_disconnects_the_senders() {
    let (handle, output_tx, exit_tx) = PtyHandle::new(PaneId::new());

    drop(handle);

    // With the receiving ends gone, each send fails and hands its payload back.
    assert_eq!(output_tx.send(b"x".to_vec()).unwrap_err().0, b"x".to_vec());
    assert_eq!(
        exit_tx.send(ExitStatus::ExitCode(0)).unwrap_err().0,
        ExitStatus::ExitCode(0)
    );
}
