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
