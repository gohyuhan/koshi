//! Tests for reading the version a program file holds, and for when a watch
//! reads it again.

use std::time::Instant;

use koshi_test_support::fixtures::{count_program_runs, write_printing_program};
use tempfile::TempDir;

use super::*;

/// How long a test waits for a version read to end before it fails.
const VERSION_READ_TEST_WAIT_DURATION: Duration = Duration::from_secs(10);

/// Wait until no version read of `executable_watch` runs.
fn wait_for_version_read_to_end(executable_watch: &ExecutableWatch) {
    let wait_end = Instant::now() + VERSION_READ_TEST_WAIT_DURATION;
    while executable_watch
        .watch_state
        .lock()
        .expect("executable watch")
        .is_version_read_running
    {
        assert!(Instant::now() < wait_end, "the version read never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Start one check of `executable_watch` through `start_version_check`, which
/// hands any other version it reads to the sender it is given. Wait for the
/// version read to end, and hand back that version, if any.
fn run_version_check(
    executable_watch: &Arc<ExecutableWatch>,
    start_version_check: fn(&Arc<ExecutableWatch>, mpsc::Sender<String>),
) -> Option<String> {
    let (other_version_sender, other_version_receiver) = mpsc::channel();
    start_version_check(executable_watch, other_version_sender);
    wait_for_version_read_to_end(executable_watch);
    other_version_receiver
        .recv_timeout(Duration::from_millis(200))
        .ok()
}

/// [`run_version_check`] through [`ExecutableWatch::check_executable_file`].
fn run_executable_file_check(executable_watch: &Arc<ExecutableWatch>) -> Option<String> {
    run_version_check(
        executable_watch,
        |executable_watch, other_version_sender| {
            executable_watch.check_executable_file(move |other_version| {
                let _ = other_version_sender.send(other_version);
            });
        },
    )
}

/// [`run_version_check`] through
/// [`ExecutableWatch::check_executable_file_after_refused_hello`].
fn run_check_after_refused_hello(executable_watch: &Arc<ExecutableWatch>) -> Option<String> {
    run_version_check(
        executable_watch,
        |executable_watch, other_version_sender| {
            executable_watch.check_executable_file_after_refused_hello(move |other_version| {
                let _ = other_version_sender.send(other_version);
            });
        },
    )
}

#[test]
fn read_installed_version_takes_the_version_after_the_program_name() {
    let program_directory = TempDir::new().expect("a test directory");
    let program_path = write_printing_program(
        program_directory.path(),
        "koshi",
        &program_directory.path().join("runs"),
        "koshi 0.6.0-pr.2",
    );

    assert_eq!(
        read_installed_version(&program_path),
        Ok("0.6.0-pr.2".to_string())
    );
}

#[test]
fn read_installed_version_refuses_a_line_that_names_another_program() {
    let program_directory = TempDir::new().expect("a test directory");
    let program_path = write_printing_program(
        program_directory.path(),
        "koshi",
        &program_directory.path().join("runs"),
        "other 1.0.0",
    );

    assert_eq!(
        read_installed_version(&program_path),
        Err(format!(
            "the binary at {} printed \"other 1.0.0\" for --version",
            program_path.display()
        ))
    );
}

#[test]
fn read_installed_version_names_a_program_that_cannot_run() {
    let program_directory = TempDir::new().expect("a test directory");
    let program_path = program_directory.path().join("absent");
    let spawn_error = ProcessCommand::new(&program_path)
        .spawn()
        .expect_err("nothing runs at that path");

    assert_eq!(
        read_installed_version(&program_path),
        Err(format!(
            "the binary at {} could not be run: {spawn_error}",
            program_path.display()
        ))
    );
}

#[cfg(unix)]
#[test]
fn read_first_output_line_ends_a_program_that_prints_nothing_within_the_wait() {
    use std::os::unix::fs::PermissionsExt as _;

    let program_directory = TempDir::new().expect("a test directory");
    let program_path = program_directory.path().join("silent");
    std::fs::write(&program_path, "#!/bin/sh\nexec sleep 600\n").expect("the program is written");
    std::fs::set_permissions(&program_path, std::fs::Permissions::from_mode(0o755))
        .expect("the program runs");

    let read_started_at = Instant::now();
    let output_line_result =
        read_first_output_line(&program_path, "--version", Duration::from_millis(200));

    let Err(OutputLineError::NoLineInTime) = output_line_result else {
        panic!("expected NoLineInTime, got {output_line_result:?}");
    };
    assert!(read_started_at.elapsed() < Duration::from_secs(5));
}

#[test]
fn read_first_output_line_starts_a_program_file_once_its_writer_closes_it() {
    let program_directory = TempDir::new().expect("a test directory");
    let program_path = write_printing_program(
        program_directory.path(),
        "koshi",
        &program_directory.path().join("runs"),
        "koshi 1.0.0",
    );
    let program_writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&program_path)
        .expect("the program file opens for writing");
    let writer_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(program_writer);
    });

    let output_line_result =
        read_first_output_line(&program_path, "--version", VERSION_READ_TEST_WAIT_DURATION);

    let Ok(output_line) = output_line_result else {
        panic!("expected the program's line, got {output_line_result:?}");
    };
    assert_eq!(output_line.trim_end(), "koshi 1.0.0");
    writer_thread.join().expect("the writer thread ends");
}

#[cfg(target_os = "linux")]
#[test]
fn read_first_output_line_gives_up_on_a_program_file_held_open_for_writing_past_the_wait() {
    let program_directory = TempDir::new().expect("a test directory");
    let program_path = write_printing_program(
        program_directory.path(),
        "koshi",
        &program_directory.path().join("runs"),
        "koshi 1.0.0",
    );
    let _program_writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&program_path)
        .expect("the program file opens for writing");
    let read_started_at = Instant::now();

    let output_line_result =
        read_first_output_line(&program_path, "--version", Duration::from_millis(200));

    let Err(OutputLineError::NotStarted(process_spawn_error)) = output_line_result else {
        panic!("expected NotStarted, got {output_line_result:?}");
    };
    assert_eq!(
        process_spawn_error.kind(),
        std::io::ErrorKind::ExecutableFileBusy
    );
    assert!(read_started_at.elapsed() >= Duration::from_millis(180));
    assert_eq!(
        count_program_runs(&program_directory.path().join("runs")),
        0
    );
}

/// Write a program printing `printed_line` beside `program_path` and rename
/// it over `program_path`, as an installer replaces a program file. Its runs
/// are logged to `run_log_path`.
fn replace_program(program_path: &Path, run_log_path: &Path, printed_line: &str) {
    let replacement_path = write_printing_program(
        program_path
            .parent()
            .expect("the program sits in a directory"),
        "replacement",
        run_log_path,
        printed_line,
    );
    std::fs::rename(&replacement_path, program_path).expect("the program file is replaced");
}

/// A program file printing `koshi 1.0.0` in `program_directory`, which logs
/// its runs to `started_runs`, and a watch of it made for a server on `1.0.0`.
/// Hands back the program's path and the watch.
fn start_watch_of_running_program(program_directory: &Path) -> (PathBuf, Arc<ExecutableWatch>) {
    let program_path = write_printing_program(
        program_directory,
        "koshi",
        &program_directory.join("started_runs"),
        "koshi 1.0.0",
    );
    let executable_watch = Arc::new(ExecutableWatch::new(program_path.clone(), "1.0.0"));
    (program_path, executable_watch)
}

#[test]
fn the_program_file_a_server_started_from_is_never_read() {
    let program_directory = TempDir::new().expect("a test directory");
    let (_, executable_watch) = start_watch_of_running_program(program_directory.path());

    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(
        count_program_runs(&program_directory.path().join("started_runs")),
        0
    );
}

#[test]
fn a_program_file_replaced_by_another_version_hands_that_version_over_once() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "koshi 9.9.9");

    assert_eq!(
        run_executable_file_check(&executable_watch),
        Some("9.9.9".to_string())
    );
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 1);
}

#[test]
fn a_program_file_replaced_by_the_running_version_hands_nothing_over_and_is_read_once() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "koshi 1.0.0");

    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 1);
}

#[test]
fn a_program_file_replaced_twice_is_read_after_each_replacement() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "koshi 1.0.0");
    assert_eq!(run_executable_file_check(&executable_watch), None);

    replace_program(&program_path, &run_log_path, "koshi 2.0.0-pr.1");

    assert_eq!(
        run_executable_file_check(&executable_watch),
        Some("2.0.0-pr.1".to_string())
    );
    assert_eq!(count_program_runs(&run_log_path), 2);
}

#[test]
fn a_program_file_whose_version_cannot_be_read_is_not_read_again_until_it_changes() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "not a version");

    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 1);
}

#[test]
fn a_missing_program_file_starts_no_version_read() {
    let program_directory = TempDir::new().expect("a test directory");
    let executable_watch = Arc::new(ExecutableWatch::new(
        program_directory.path().join("absent"),
        "1.0.0",
    ));

    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(
        executable_watch
            .watch_state
            .lock()
            .expect("executable watch")
            .examined_file_identity,
        None
    );
}

#[test]
fn a_program_file_that_appears_where_none_was_is_read() {
    let program_directory = TempDir::new().expect("a test directory");
    let program_path =
        program_directory
            .path()
            .join(if cfg!(windows) { "koshi.cmd" } else { "koshi" });
    let executable_watch = Arc::new(ExecutableWatch::new(program_path.clone(), "1.0.0"));
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "koshi 9.9.9");

    assert_eq!(
        run_executable_file_check(&executable_watch),
        Some("9.9.9".to_string())
    );
    assert_eq!(count_program_runs(&run_log_path), 1);
}

/// Move the retry instant `executable_watch` holds to now, as if
/// [`RESTART_RETRY_INTERVAL_DURATION`] had passed since
/// [`ExecutableWatch::schedule_restart_retry`].
fn make_restart_retry_due(executable_watch: &ExecutableWatch) {
    executable_watch
        .watch_state
        .lock()
        .expect("executable watch")
        .restart_retry_at = Some(Instant::now());
}

#[test]
fn a_scheduled_restart_retry_reads_the_same_file_again_once_it_is_due() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "koshi 9.9.9");
    assert_eq!(
        run_executable_file_check(&executable_watch),
        Some("9.9.9".to_string())
    );

    executable_watch.schedule_restart_retry();
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 1);

    make_restart_retry_due(&executable_watch);
    assert_eq!(
        run_executable_file_check(&executable_watch),
        Some("9.9.9".to_string())
    );
    assert_eq!(count_program_runs(&run_log_path), 2);
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 2);
}

#[test]
fn a_program_file_that_changes_while_a_restart_retry_waits_is_read_at_the_next_check() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "koshi 9.9.9");
    assert_eq!(
        run_executable_file_check(&executable_watch),
        Some("9.9.9".to_string())
    );

    executable_watch.schedule_restart_retry();
    replace_program(&program_path, &run_log_path, "koshi 9.9.10");

    assert_eq!(
        run_executable_file_check(&executable_watch),
        Some("9.9.10".to_string())
    );
    assert_eq!(count_program_runs(&run_log_path), 2);
}

#[test]
fn schedule_restart_retry_waits_the_retry_interval() {
    let program_directory = TempDir::new().expect("a test directory");
    let (_, executable_watch) = start_watch_of_running_program(program_directory.path());
    let scheduled_at = Instant::now();

    executable_watch.schedule_restart_retry();

    let restart_retry_at = executable_watch
        .watch_state
        .lock()
        .expect("executable watch")
        .restart_retry_at
        .expect("a retry is scheduled");
    assert!(restart_retry_at >= scheduled_at + RESTART_RETRY_INTERVAL_DURATION);
    assert!(restart_retry_at <= Instant::now() + RESTART_RETRY_INTERVAL_DURATION);
}

#[test]
fn a_refused_hello_reads_the_program_file_the_server_started_from() {
    let program_directory = TempDir::new().expect("a test directory");
    let run_log_path = program_directory.path().join("runs");
    let program_path = write_printing_program(
        program_directory.path(),
        "koshi",
        &run_log_path,
        "koshi 1.0.0",
    );
    let executable_watch = Arc::new(ExecutableWatch::new(program_path, "0.5.0"));
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 0);

    assert_eq!(
        run_check_after_refused_hello(&executable_watch),
        Some("1.0.0".to_string())
    );
    assert_eq!(count_program_runs(&run_log_path), 1);
}

#[test]
fn a_refused_hello_reads_no_file_whose_last_read_printed_the_running_version() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "koshi 1.0.0");
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 1);

    assert_eq!(run_check_after_refused_hello(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 1);
}

#[test]
fn a_refused_hello_reads_again_a_file_whose_last_read_failed() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "not a version");
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(run_executable_file_check(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 1);

    assert_eq!(run_check_after_refused_hello(&executable_watch), None);
    assert_eq!(count_program_runs(&run_log_path), 2);
}

#[test]
fn a_refused_hello_hands_over_again_the_other_version_the_file_still_holds() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "koshi 9.9.9");
    assert_eq!(
        run_executable_file_check(&executable_watch),
        Some("9.9.9".to_string())
    );
    assert_eq!(run_executable_file_check(&executable_watch), None);

    assert_eq!(
        run_check_after_refused_hello(&executable_watch),
        Some("9.9.9".to_string())
    );
    assert_eq!(count_program_runs(&run_log_path), 2);
}

#[test]
fn a_refused_hello_reads_nothing_while_a_version_read_runs() {
    let program_directory = TempDir::new().expect("a test directory");
    let (program_path, executable_watch) = start_watch_of_running_program(program_directory.path());
    let run_log_path = program_directory.path().join("runs");
    replace_program(&program_path, &run_log_path, "koshi 9.9.9");
    executable_watch
        .watch_state
        .lock()
        .expect("executable watch")
        .is_version_read_running = true;

    let (other_version_sender, other_version_receiver) = mpsc::channel();
    executable_watch.check_executable_file_after_refused_hello(move |other_version| {
        let _ = other_version_sender.send(other_version);
    });

    assert_eq!(
        other_version_receiver.recv_timeout(Duration::from_millis(500)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
    assert_eq!(count_program_runs(&run_log_path), 0);
}
