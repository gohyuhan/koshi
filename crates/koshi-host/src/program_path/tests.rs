//! Tests for the program path: the path a program started through a symbolic
//! link reports, whatever the first command-line argument names, and the
//! running program for a start through a file descriptor path.

use super::*;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Stdio};

/// The environment variable naming the file into which
/// [`probe_writes_the_resolved_program_path_into_the_named_file`] writes the
/// program path of its process.
#[cfg(unix)]
const PROGRAM_PATH_FILE_VARIABLE: &str = "KOSHI_TEST_PROGRAM_PATH_FILE";

/// The body of a probe process: with [`PROGRAM_PATH_FILE_VARIABLE`] set, it
/// writes what [`resolve_program_path`] gives into the file that variable
/// names. Without it, it does nothing.
#[cfg(unix)]
#[test]
#[ignore = "runs only inside a probe process"]
fn probe_writes_the_resolved_program_path_into_the_named_file() {
    let Some(program_path_file) = std::env::var_os(PROGRAM_PATH_FILE_VARIABLE) else {
        return;
    };
    let program_path = resolve_program_path().expect("the program path resolves");
    std::fs::write(
        program_path_file,
        program_path.as_os_str().as_encoded_bytes(),
    )
    .expect("the program path is written");
}

/// Run `probe_command`, a start of this test binary, as a probe process, and
/// hand back the program path it reports in the file `program-path` in
/// `probe_root`.
#[cfg(unix)]
fn run_program_path_probe(mut probe_command: Command, probe_root: &Path) -> PathBuf {
    let program_path_file = probe_root.join("program-path");
    let probe_status = probe_command
        .env(PROGRAM_PATH_FILE_VARIABLE, &program_path_file)
        .args([
            "--exact",
            "program_path::tests::probe_writes_the_resolved_program_path_into_the_named_file",
            "--ignored",
            "--test-threads=1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the probe runs");

    assert!(probe_status.success());
    PathBuf::from(
        std::fs::read_to_string(&program_path_file).expect("the probe wrote its program path"),
    )
}

#[cfg(unix)]
#[test]
fn resolve_program_path_keeps_the_symbolic_link_a_program_was_started_through() {
    let probe_root = tempfile::tempdir().expect("a test directory");
    let probe_root_path = std::fs::canonicalize(probe_root.path()).expect("the directory resolves");
    let link_path = probe_root_path.join("bin/kk");
    std::fs::create_dir_all(link_path.parent().expect("the link has a folder"))
        .expect("the folder is made");
    std::os::unix::fs::symlink(
        std::env::current_exe().expect("the test binary has a path"),
        &link_path,
    )
    .expect("the link is made");
    let mut path_lookup_command = Command::new("kk");
    path_lookup_command.env("PATH", probe_root_path.join("bin"));

    for probe_command in [Command::new(&link_path), path_lookup_command] {
        assert_eq!(
            run_program_path_probe(probe_command, &probe_root_path),
            link_path
        );
    }
}

#[cfg(unix)]
#[test]
fn resolve_program_path_ignores_a_first_argument_that_names_another_koshi_file() {
    let probe_root = tempfile::tempdir().expect("a test directory");
    let probe_root_path = std::fs::canonicalize(probe_root.path()).expect("the directory resolves");
    let test_binary_path = std::env::current_exe().expect("the test binary has a path");
    let other_program_path = probe_root_path.join("koshi");
    std::fs::write(&other_program_path, b"#!/bin/sh\n").expect("the file is written");
    std::fs::set_permissions(&other_program_path, std::fs::Permissions::from_mode(0o755))
        .expect("the mode is set");
    let mut probe_command = Command::new(&test_binary_path);
    probe_command.arg0(&other_program_path);

    assert_eq!(
        run_program_path_probe(probe_command, &probe_root_path),
        test_binary_path
    );
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_program_path_gives_the_running_program_for_a_start_through_a_file_descriptor_path() {
    use std::os::fd::AsRawFd as _;

    let probe_root = tempfile::tempdir().expect("a test directory");
    let test_binary_path = std::env::current_exe().expect("the test binary has a path");
    let test_binary_file = std::fs::File::open(&test_binary_path).expect("the test binary opens");
    let test_binary_descriptor = test_binary_file.as_raw_fd();

    for descriptor_path in [
        format!("/dev/fd/{test_binary_descriptor}"),
        format!("/proc/self/fd/{test_binary_descriptor}"),
    ] {
        let mut probe_command = Command::new(descriptor_path);
        // SAFETY: the closure runs in the child process between `fork` and
        // `execve`, and calls only `fcntl`, which is async-signal-safe.
        unsafe {
            probe_command.pre_exec(move || {
                if libc::fcntl(test_binary_descriptor, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        assert_eq!(
            run_program_path_probe(probe_command, probe_root.path()),
            test_binary_path
        );
    }
}

#[test]
fn resolve_program_path_gives_a_path_to_the_running_program() {
    let program_path = resolve_program_path().expect("the program path resolves");

    assert_eq!(
        std::fs::canonicalize(program_path).expect("the program path resolves to a file"),
        std::fs::canonicalize(std::env::current_exe().expect("the test binary has a path"))
            .expect("the test binary resolves to a file")
    );
}
