//! Tests for [`PtyHandle`] receiver handoff.

use super::*;

use tile_core::ids::PaneId;

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
fn try_reads_work_while_receivers_are_held() {
    let (handle, output_tx, exit_tx) = PtyHandle::new(PaneId::new());

    assert!(handle.try_read_output().is_none());
    output_tx.send(b"x".to_vec()).expect("send output");
    assert_eq!(handle.try_read_output(), Some(b"x".to_vec()));

    exit_tx.send(ExitStatus::Signaled(9)).expect("send exit");
    assert_eq!(handle.try_exit_status(), Some(ExitStatus::Signaled(9)));
}
