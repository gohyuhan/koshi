//! Tests for the program path: the path the first command-line argument
//! names, the file it must share with `current_exe`, and the path a program
//! started through a symbolic link reports.

use super::*;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

/// The environment variable naming the file into which
/// [`probe_writes_the_resolved_program_path_into_the_named_file`] writes the
/// program path of its process.
#[cfg(unix)]
const PROGRAM_PATH_FILE_VARIABLE: &str = "KOSHI_TEST_PROGRAM_PATH_FILE";

/// How long [`run_program_path_probe`] keeps trying to start a probe that the
/// operating system reports as held open for writing: 20 seconds.
#[cfg(unix)]
const BUSY_PROBE_WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long [`run_program_path_probe`] pauses between those attempts: 20
/// milliseconds.
#[cfg(unix)]
const BUSY_PROBE_RETRY_INTERVAL_DURATION: Duration = Duration::from_millis(20);

/// Write a file at `file_path` with the execute permission bits `mode`.
#[cfg(unix)]
fn write_program_file(file_path: &Path, mode: u32) {
    std::fs::create_dir_all(file_path.parent().expect("the file has a folder"))
        .expect("the folder is made");
    std::fs::write(file_path, b"#!/bin/sh\n").expect("the file is written");
    std::fs::set_permissions(file_path, std::fs::Permissions::from_mode(mode))
        .expect("the mode is set");
}

#[cfg(unix)]
#[test]
fn find_launch_path_takes_an_argument_that_holds_a_slash_as_written() {
    assert_eq!(
        find_launch_path(OsStr::new("/opt/koshi/bin/koshi"), None),
        Some(PathBuf::from("/opt/koshi/bin/koshi"))
    );
}

#[cfg(unix)]
#[test]
fn find_launch_path_makes_a_relative_argument_absolute_against_the_working_directory() {
    assert_eq!(
        find_launch_path(OsStr::new("./bin/koshi"), None),
        Some(
            std::env::current_dir()
                .expect("the working directory is readable")
                .join("bin/koshi")
        )
    );
}

#[cfg(unix)]
#[test]
fn find_launch_path_takes_the_first_executable_file_of_that_name_in_path() {
    let search_root = tempfile::tempdir().expect("a test directory");
    let not_executable_path = search_root.path().join("first/koshi");
    let first_executable_path = search_root.path().join("second/koshi");
    let second_executable_path = search_root.path().join("third/koshi");
    write_program_file(&not_executable_path, 0o644);
    write_program_file(&first_executable_path, 0o755);
    write_program_file(&second_executable_path, 0o755);
    let search_path = std::env::join_paths([
        search_root.path().join("missing"),
        search_root.path().join("first"),
        search_root.path().join("second"),
        search_root.path().join("third"),
    ])
    .expect("the folders join");

    assert_eq!(
        find_launch_path(OsStr::new("koshi"), Some(&search_path)),
        Some(first_executable_path)
    );
}

#[cfg(unix)]
#[test]
fn find_launch_path_keeps_the_symbolic_link_that_path_holds() {
    let search_root = tempfile::tempdir().expect("a test directory");
    let target_path = search_root.path().join("versions/0.5.0/koshi");
    write_program_file(&target_path, 0o755);
    let link_path = search_root.path().join("bin/koshi");
    std::fs::create_dir_all(link_path.parent().expect("the link has a folder"))
        .expect("the folder is made");
    std::os::unix::fs::symlink(&target_path, &link_path).expect("the link is made");
    let search_path =
        std::env::join_paths([search_root.path().join("bin")]).expect("the folders join");

    assert_eq!(
        find_launch_path(OsStr::new("koshi"), Some(&search_path)),
        Some(link_path)
    );
}

#[cfg(unix)]
#[test]
fn find_launch_path_finds_nothing_for_a_name_no_folder_holds() {
    let search_root = tempfile::tempdir().expect("a test directory");
    let search_path = std::env::join_paths([search_root.path()]).expect("the folders join");

    assert_eq!(
        find_launch_path(OsStr::new("koshi"), Some(&search_path)),
        None
    );
    assert_eq!(find_launch_path(OsStr::new("koshi"), None), None);
}

#[cfg(unix)]
#[test]
fn is_same_file_matches_a_symbolic_link_with_its_target_and_nothing_else() {
    let file_root = tempfile::tempdir().expect("a test directory");
    let target_path = file_root.path().join("koshi");
    let other_path = file_root.path().join("other");
    write_program_file(&target_path, 0o755);
    write_program_file(&other_path, 0o755);
    let link_path = file_root.path().join("link");
    std::os::unix::fs::symlink(&target_path, &link_path).expect("the link is made");

    assert!(is_same_file(&link_path, &target_path));
    assert!(!is_same_file(&link_path, &other_path));
    assert!(!is_same_file(
        &file_root.path().join("missing"),
        &target_path
    ));
}

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

/// Run a copy of this test binary through the symbolic link `bin/koshi` in
/// `probe_root`, by the name `launch_name` with `bin` as the only `PATH` folder,
/// and hand back the program path it reports. A start that fails with
/// [`std::io::ErrorKind::ExecutableFileBusy`] is tried again every
/// [`BUSY_PROBE_RETRY_INTERVAL_DURATION`] for up to
/// [`BUSY_PROBE_WAIT_DURATION`].
#[cfg(unix)]
fn run_program_path_probe(probe_root: &Path, launch_name: &OsStr) -> PathBuf {
    let target_path = probe_root.join("versions/0.5.0/koshi");
    std::fs::create_dir_all(target_path.parent().expect("the copy has a folder"))
        .expect("the folder is made");
    std::fs::copy(
        std::env::current_exe().expect("the test binary has a path"),
        &target_path,
    )
    .expect("the test binary is copied");
    let link_path = probe_root.join("bin/koshi");
    std::fs::create_dir_all(link_path.parent().expect("the link has a folder"))
        .expect("the folder is made");
    std::os::unix::fs::symlink(&target_path, &link_path).expect("the link is made");
    let program_path_file = probe_root.join("program-path");

    let mut probe_command = Command::new(launch_name);
    probe_command
        .env("PATH", probe_root.join("bin"))
        .env(PROGRAM_PATH_FILE_VARIABLE, &program_path_file)
        .args([
            "--exact",
            "program_path::tests::probe_writes_the_resolved_program_path_into_the_named_file",
            "--ignored",
            "--test-threads=1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let busy_deadline = Instant::now() + BUSY_PROBE_WAIT_DURATION;
    let probe_status = loop {
        match probe_command.status() {
            Err(spawn_error)
                if spawn_error.kind() == io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < busy_deadline =>
            {
                std::thread::sleep(BUSY_PROBE_RETRY_INTERVAL_DURATION);
            }
            probe_result => break probe_result.expect("the probe runs"),
        }
    };

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

    assert_eq!(
        run_program_path_probe(&probe_root_path, OsStr::new("koshi")),
        probe_root_path.join("bin/koshi")
    );
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
