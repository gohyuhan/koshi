//! Tests for the OS working-directory and hostname lookups, probed against
//! this test process itself.

use super::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_lookup_answers_this_process_own_directory() {
    let reported_working_directory = get_process_working_directory(std::process::id())
        .expect("the OS answers for a live process");
    let current_working_directory = std::env::current_dir().expect("current directory");
    // The OS answers the real path; the env answer may travel through a
    // symlink (macOS `/tmp` → `/private/tmp`), so both sides canonicalize.
    assert_eq!(
        reported_working_directory
            .canonicalize()
            .expect("canonicalize the reported directory"),
        current_working_directory
            .canonicalize()
            .expect("canonicalize the current dir"),
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_lookup_answers_a_child_own_directory_not_this_process_one() {
    let test_directory = tempfile::tempdir().expect("create a test directory");
    let mut child_process = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(test_directory.path())
        .spawn()
        .expect("spawn sleep child");

    let reported_working_directory = get_process_working_directory(child_process.id());

    child_process.kill().expect("kill the child");
    child_process.wait().expect("reap child");
    let reported_working_directory =
        reported_working_directory.expect("the OS answers for a live child");
    assert_eq!(
        reported_working_directory
            .canonicalize()
            .expect("canonicalize the reported directory"),
        test_directory
            .path()
            .canonicalize()
            .expect("canonicalize the test directory"),
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_reaped_child_answers_nothing() {
    let mut child_process = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep child");
    let process_id = child_process.id();
    child_process.kill().expect("kill the child");
    child_process.wait().expect("reap child");

    assert_eq!(get_process_working_directory(process_id), None);
}

#[test]
fn a_process_that_cannot_exist_answers_nothing() {
    // `u32::MAX` is no valid PID on any supported OS.
    assert_eq!(get_process_working_directory(u32::MAX), None);
}

#[cfg(any(unix, windows))]
#[test]
fn the_machine_names_itself() {
    let local_hostname = get_local_hostname().expect("the OS names this machine");
    assert!(!local_hostname.is_empty());
}
