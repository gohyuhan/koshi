//! Tests for the wait on a child process exit.

use super::*;

#[test]
fn waiting_for_a_child_that_keeps_running_panics_at_its_deadline() {
    let mut running_child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("start a running child");
    let child_process_id = running_child.id();
    let (wait_result_sender, wait_result_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let wait_result = std::panic::catch_unwind(|| {
            wait_until_child_has_exited_with_timeout(child_process_id, Duration::from_millis(20));
        });
        let _ = wait_result_sender.send(wait_result.is_err());
    });
    let wait_result = wait_result_receiver.recv_timeout(Duration::from_secs(5));
    running_child.kill().expect("end the running child");
    running_child.wait().expect("reap the running child");

    assert_eq!(wait_result, Ok(true), "the wait reaches its deadline");
}
