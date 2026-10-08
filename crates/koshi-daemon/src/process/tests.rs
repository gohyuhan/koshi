//! Tests for the process helpers.

use super::*;

use koshi_test_support::child_exit::wait_until_child_has_exited;

/// A serving thread's SIGPIPE block must hold while the process-wide
/// disposition sits at its default, the state a running exec puts it in. The
/// raise is thread-directed, like the signal a write to a hung-up peer
/// raises; blocked, it stays pending, the thread runs on, and the pending
/// signal dies with the thread.
#[test]
fn a_serving_threads_sigpipe_block_holds_under_the_default_disposition() {
    let is_serving_thread_alive_after_sigpipe = std::thread::spawn(|| {
        block_sigpipe_on_this_thread();
        let prior_sigpipe_disposition = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let sigpipe_raise_result = unsafe { libc::raise(libc::SIGPIPE) };
        unsafe { libc::signal(libc::SIGPIPE, prior_sigpipe_disposition) };
        sigpipe_raise_result == 0
    })
    .join()
    .expect("the thread survives the raised SIGPIPE");
    assert!(
        is_serving_thread_alive_after_sigpipe,
        "the raise itself reported an error"
    );
}

/// How many running children the child-listing test starts: more than the 64
/// ids the first read on macOS makes room for.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const LISTED_RUNNING_CHILD_COUNT: usize = 65;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn listing_the_child_processes_names_every_child_running_or_exited() {
    let mut running_children: Vec<std::process::Child> = (0..LISTED_RUNNING_CHILD_COUNT)
        .map(|_| {
            std::process::Command::new("sleep")
                .arg("100")
                .spawn()
                .expect("start a running child")
        })
        .collect();
    let mut exited_child = std::process::Command::new("true")
        .spawn()
        .expect("start a child that exits");
    wait_until_child_has_exited(exited_child.id());

    let child_process_ids = list_child_process_ids().expect("child processes can be listed");

    let unlisted_running_child_process_ids: Vec<u32> = running_children
        .iter()
        .map(std::process::Child::id)
        .filter(|running_child_process_id| !child_process_ids.contains(running_child_process_id))
        .collect();
    let is_exited_child_listed = child_process_ids.contains(&exited_child.id());
    let is_parent_listed = child_process_ids.contains(&std::os::unix::process::parent_id());
    for running_child in &mut running_children {
        running_child.kill().expect("the running child is ended");
        running_child.wait().expect("the running child is reaped");
    }
    exited_child.wait().expect("the exited child is reaped");
    assert_eq!(
        (
            unlisted_running_child_process_ids,
            is_exited_child_listed,
            is_parent_listed
        ),
        (Vec::new(), true, false),
        "every child is listed, and the process that started this one is not"
    );
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn listing_child_processes_reports_unsupported_platforms() {
    assert_eq!(
        list_child_process_ids()
            .expect_err("this platform has no child-listing implementation")
            .kind(),
        std::io::ErrorKind::Unsupported
    );
}

/// The wait returns once the child has exited, and the child is reaped: a
/// second wait for it finds no such child.
#[test]
fn waiting_for_a_child_returns_once_it_exits_and_reaps_it() {
    let mut exiting_child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 0.2")
        .spawn()
        .expect("start a child that exits");

    wait_for_child_exit(libc::pid_t::try_from(exiting_child.id()).expect("a child id fits a pid"));

    assert_eq!(
        exiting_child
            .try_wait()
            .err()
            .map(|wait_error| wait_error.raw_os_error()),
        Some(Some(libc::ECHILD)),
        "the wait reaped the child"
    );
}
