//! Tests for the OS working-directory and hostname lookups, probed against
//! this test process itself.

use super::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_lookup_answers_this_process_own_directory() {
    let answered = process_cwd(std::process::id()).expect("the OS answers for a live process");
    let current = std::env::current_dir().expect("current dir");
    // The OS answers the real path; the env answer may travel through a
    // symlink (macOS `/tmp` → `/private/tmp`), so both sides canonicalize.
    assert_eq!(
        answered.canonicalize().expect("canonicalize the answer"),
        current
            .canonicalize()
            .expect("canonicalize the current dir"),
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_lookup_answers_a_child_own_directory_not_this_process_one() {
    let dir = tempfile::tempdir().expect("create a temp dir");
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(dir.path())
        .spawn()
        .expect("spawn sleep child");

    let answered = process_cwd(child.id());

    child.kill().expect("kill the child");
    child.wait().expect("reap child");
    let answered = answered.expect("the OS answers for a live child");
    assert_eq!(
        answered.canonicalize().expect("canonicalize the answer"),
        dir.path()
            .canonicalize()
            .expect("canonicalize the temp dir"),
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_reaped_child_answers_nothing() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep child");
    let pid = child.id();
    child.kill().expect("kill the child");
    child.wait().expect("reap child");

    assert_eq!(process_cwd(pid), None);
}

#[test]
fn a_process_that_cannot_exist_answers_nothing() {
    // `u32::MAX` is no valid PID on any supported OS.
    assert_eq!(process_cwd(u32::MAX), None);
}

#[cfg(any(unix, windows))]
#[test]
fn the_machine_names_itself() {
    let name = local_hostname().expect("the OS names this machine");
    assert!(!name.is_empty());
}
