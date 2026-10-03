//! What a replacement of the running program that fails leaves in this
//! process.

use super::*;

#[test]
fn a_failed_exec_hands_back_its_error_and_leaves_sigpipe_ignored() {
    let empty_directory = tempfile::tempdir().expect("an empty directory");
    let missing_program_path = empty_directory.path().join("koshi");

    let exec_error = exec_and_keep_ignoring_sigpipe(&mut Command::new(&missing_program_path));
    let sigpipe_disposition_after_exec = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    assert_eq!(exec_error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(sigpipe_disposition_after_exec, libc::SIG_IGN);
}
