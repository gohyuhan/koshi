//! Tests for ending a session: which process counts as the session's, which
//! processes hold its panes, how a session that does not quit is ended, how a
//! session that exits while its quit runs is ended, how a pane holder that
//! still runs after its session quit is ended, and which processes listed
//! under a pane holder are ended and counted once the session quit.

use std::path::PathBuf;
use std::process::{Child, Command as ProcessCommand, Stdio};

use koshi_core::ids::parse_prefixed_uuid;
use koshi_ipc::endpoint::compute_socket_address;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::supervisor::compute_supervisor_socket_address;
use koshi_ipc::transport::{Connection, Listener};
use koshi_test_support::fixtures::{start_program_process, NO_SUCH_PROCESS_ID};
use tempfile::TempDir;

use super::*;

/// The environment variable that makes [`run_stand_in_session_process`] start
/// its children and wait.
const STAND_IN_SESSION_VARIABLE: &str = "KOSHI_TEST_STAND_IN_SESSION";

/// The environment variable naming the directory in which
/// [`run_stand_in_session_process`] writes an endpoint file naming its own
/// process, and exits with code `0` when [`find_server_process_record`]
/// refuses it, `1` when it accepts it.
const STAND_IN_SELF_CHECK_VARIABLE: &str = "KOSHI_TEST_STAND_IN_SELF_CHECK";

/// The environment variable naming the runtime directory in which
/// [`run_stand_in_session_process`] serves the pane holder address of its own
/// process, for the session that [`STAND_IN_PANE_HOLDER_SESSION_VARIABLE`]
/// names.
const STAND_IN_PANE_HOLDER_DIRECTORY_VARIABLE: &str = "KOSHI_TEST_STAND_IN_PANE_HOLDER_DIRECTORY";

/// The environment variable holding the session id, such as
/// `session-<uuid>`, whose pane holder address a stand-in serves.
const STAND_IN_PANE_HOLDER_SESSION_VARIABLE: &str = "KOSHI_TEST_STAND_IN_PANE_HOLDER_SESSION";

/// The number of children a stand-in session starts.
const STAND_IN_CHILD_COUNT: usize = 2;

/// A fresh runtime directory under `/tmp` on Unix, where a session socket
/// path fits a Unix socket address, and under the folder
/// [`std::env::temp_dir`] gives on Windows.
fn build_short_runtime_directory() -> TempDir {
    #[cfg(unix)]
    let base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base_directory = std::env::temp_dir();
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in(base_directory)
        .expect("the test directory")
}

/// Write the endpoint file of `session_id` into `runtime_directory`, naming
/// `process_id` and a control socket nothing listens on.
fn write_endpoint_file_naming_process(
    runtime_directory: &Path,
    session_id: SessionId,
    process_id: u32,
) -> PathBuf {
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory, session_id),
        connection_token: ConnectionToken::from_secret("test-token"),
        process_id,
    }
    .write_to_path(&endpoint_file_path)
    .expect("the endpoint file is written");
    endpoint_file_path
}

/// A child process that runs for 600 s: `sleep 600` on Linux and macOS,
/// `ping` 600 times on Windows.
fn spawn_long_running_child() -> Child {
    let mut child_command = if cfg!(windows) {
        let mut child_command = ProcessCommand::new("ping");
        child_command.args(["-n", "600", "127.0.0.1"]);
        child_command
    } else {
        let mut child_command = ProcessCommand::new("sleep");
        child_command.arg("600");
        child_command
    };
    child_command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the child process starts")
}

/// A child from [`spawn_long_running_child`], and its record.
fn start_recorded_long_running_child() -> (Child, ProcessRecord) {
    let child = spawn_long_running_child();
    let child_record = process_tree::find_process_record(child.id()).expect("the child runs");
    (child, child_record)
}

/// End and reap `child`.
fn end_child(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The body of a stand-in session process: with
/// [`STAND_IN_SELF_CHECK_VARIABLE`] set, it runs that check and exits. With
/// [`STAND_IN_PANE_HOLDER_DIRECTORY_VARIABLE`] set, it serves the address
/// [`compute_supervisor_socket_address`] gives for its own process id and
/// waits 600 s. With [`STAND_IN_SESSION_VARIABLE`] set, it starts
/// [`STAND_IN_CHILD_COUNT`] children from [`spawn_long_running_child`] and
/// waits 600 s. Without any of them, it does nothing.
#[test]
#[ignore = "runs only inside a stand-in session process"]
fn run_stand_in_session_process() {
    if let Some(pane_holder_directory) = std::env::var_os(STAND_IN_PANE_HOLDER_DIRECTORY_VARIABLE) {
        let session_id = SessionId::from_uuid(
            parse_prefixed_uuid(
                &std::env::var(STAND_IN_PANE_HOLDER_SESSION_VARIABLE)
                    .expect("the stand-in names its session"),
                "session",
            )
            .expect("the stand-in names a session id"),
        );
        let _pane_holder_listener = Listener::bind(&compute_supervisor_socket_address(
            Path::new(&pane_holder_directory),
            session_id,
            std::process::id(),
        ))
        .expect("the stand-in serves its pane holder address");
        std::thread::sleep(Duration::from_secs(600));
        return;
    }
    if let Some(self_check_directory) = std::env::var_os(STAND_IN_SELF_CHECK_VARIABLE) {
        let endpoint_file_path = write_endpoint_file_naming_process(
            Path::new(&self_check_directory),
            SessionId::new(),
            std::process::id(),
        );
        let is_refused =
            find_server_process_record(&endpoint_file_path, std::process::id()).is_none();
        std::process::exit(if is_refused { 0 } else { 1 });
    }
    if std::env::var_os(STAND_IN_SESSION_VARIABLE).is_none() {
        return;
    }
    let _children: Vec<Child> = (0..STAND_IN_CHILD_COUNT)
        .map(|_| spawn_long_running_child())
        .collect();
    std::thread::sleep(Duration::from_secs(600));
}

/// The command that runs [`run_stand_in_session_process`] in a copy of this
/// test binary saved as `program_file_name` in `program_directory`, with no
/// input and its output discarded. Start it with [`start_program_process`].
fn build_stand_in_command(program_directory: &Path, program_file_name: &str) -> ProcessCommand {
    let program_path = program_directory.join(program_file_name);
    std::fs::copy(
        std::env::current_exe().expect("the test binary has a path"),
        &program_path,
    )
    .expect("the test binary is copied");
    let mut stand_in_command = ProcessCommand::new(&program_path);
    stand_in_command
        .args([
            "--exact",
            "session_end::tests::run_stand_in_session_process",
            "--ignored",
            "--test-threads=1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    stand_in_command
}

/// A stand-in session process: a copy of this test binary saved as
/// `program_file_name` in `program_directory`, running
/// [`run_stand_in_session_process`]. Hands back the process, its record, and
/// the records of its children once all of them run.
pub(crate) fn start_stand_in_session(
    program_directory: &Path,
    program_file_name: &str,
) -> (Child, ProcessRecord, Vec<ProcessRecord>) {
    let mut child = start_program_process(
        build_stand_in_command(program_directory, program_file_name)
            .env(STAND_IN_SESSION_VARIABLE, "1"),
    );
    let wait_end = Instant::now() + Duration::from_secs(10);
    let started_records = loop {
        if let Some(child_record) = process_tree::find_process_record(child.id()) {
            let member_records = process_tree::list_member_records(
                std::slice::from_ref(&child_record),
                &process_tree::list_process_records().expect("the process list is readable"),
            );
            if member_records.len() == STAND_IN_CHILD_COUNT {
                break Some((child_record, member_records));
            }
        }
        if Instant::now() >= wait_end {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let Some((child_record, member_records)) = started_records else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the stand-in session never started its children");
    };
    assert_eq!(child_record.executable_name, program_file_name);
    (child, child_record, member_records)
}

/// End a stand-in session process and its children.
pub(crate) fn end_stand_in_session(mut child: Child, member_records: &[ProcessRecord]) {
    let _ = child.kill();
    let _ = child.wait();
    process_tree::stop_processes(member_records, Duration::ZERO);
}

/// The file name a stand-in session runs under: `koshi` or `koshi.exe` for a
/// session, and another name for a process that is not koshi.
pub(crate) fn format_stand_in_file_name(is_koshi: bool) -> &'static str {
    match (is_koshi, cfg!(windows)) {
        (true, true) => "koshi.exe",
        (true, false) => "koshi",
        (false, true) => "other.exe",
        (false, false) => "other",
    }
}

#[test]
fn a_quit_refused_by_a_server_of_koshi_0_4_0_reads_as_the_format_sentence_alone() {
    let quit_error = CliError::PreviousReleaseServer {
        detail: "the server answered in the format of koshi 0.4.0 or older, which this koshi \
                 cannot talk to"
            .to_string(),
    };

    assert_eq!(
        format_quit_failure(&quit_error),
        "the server answered in the format of koshi 0.4.0 or older, which this koshi cannot \
         talk to"
    );
}

#[test]
fn any_other_quit_failure_reads_as_the_errors_own_sentence() {
    assert_eq!(
        format_quit_failure(&CliError::SessionAnswerTimedOut),
        "IPC unavailable: the session did not answer in time"
    );
}

#[test]
fn format_process_kill_command_names_the_command_of_this_platform() {
    let expected_kill_command = if cfg!(windows) {
        "taskkill /PID 5000 /T /F"
    } else {
        "kill 5000"
    };

    assert_eq!(format_process_kill_command(5000), expected_kill_command);
}

#[test]
fn find_server_process_record_refuses_this_process() {
    let runtime_directory = build_short_runtime_directory();
    let endpoint_file_path = write_endpoint_file_naming_process(
        runtime_directory.path(),
        SessionId::new(),
        std::process::id(),
    );

    assert!(find_endpoint_process_record(&endpoint_file_path, std::process::id()).is_some());
    assert_eq!(
        find_server_process_record(&endpoint_file_path, std::process::id()),
        None
    );
}

#[test]
fn find_server_process_record_refuses_the_koshi_process_that_asks() {
    let runtime_directory = build_short_runtime_directory();

    let self_check_status = start_program_process(
        build_stand_in_command(runtime_directory.path(), format_stand_in_file_name(true))
            .env(STAND_IN_SELF_CHECK_VARIABLE, runtime_directory.path()),
    )
    .wait()
    .expect("the stand-in runs its check");

    assert_eq!(self_check_status.code(), Some(0));
}

#[test]
fn find_server_process_record_refuses_a_process_that_does_not_run_koshi() {
    let runtime_directory = build_short_runtime_directory();
    let (child, child_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(false));
    let endpoint_file_path =
        write_endpoint_file_naming_process(runtime_directory.path(), SessionId::new(), child.id());

    assert_eq!(
        find_endpoint_process_record(&endpoint_file_path, child.id()),
        Some(child_record)
    );
    assert_eq!(
        find_server_process_record(&endpoint_file_path, child.id()),
        None
    );

    end_stand_in_session(child, &member_records);
}

#[test]
fn find_server_process_record_accepts_koshi_that_started_before_its_endpoint_file() {
    let runtime_directory = build_short_runtime_directory();
    let (child, child_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    let endpoint_file_path =
        write_endpoint_file_naming_process(runtime_directory.path(), SessionId::new(), child.id());

    assert_eq!(
        find_server_process_record(&endpoint_file_path, child.id()),
        Some(child_record)
    );

    std::fs::File::options()
        .write(true)
        .open(&endpoint_file_path)
        .expect("the endpoint file opens")
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
        .expect("the endpoint file is aged");
    assert_eq!(
        find_server_process_record(&endpoint_file_path, child.id()),
        None
    );

    end_stand_in_session(child, &member_records);
}

#[test]
fn find_server_process_record_accepts_koshi_whose_program_file_was_renamed_to_a_backup_name() {
    let runtime_directory = build_short_runtime_directory();
    let (child, child_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    std::fs::rename(
        runtime_directory
            .path()
            .join(format_stand_in_file_name(true)),
        runtime_directory.path().join("koshi.old"),
    )
    .expect("the running program file is renamed");
    let endpoint_file_path =
        write_endpoint_file_naming_process(runtime_directory.path(), SessionId::new(), child.id());

    let server_record = find_server_process_record(&endpoint_file_path, child.id());

    end_stand_in_session(child, &member_records);
    assert_eq!(
        server_record.map(|server_record| (server_record.process_id, server_record.started_at)),
        Some((child_record.process_id, child_record.started_at))
    );
}

#[test]
fn end_session_leaves_an_unconfirmed_process_running_and_names_it() {
    let runtime_directory = build_short_runtime_directory();
    let session_id = SessionId::new();
    let (child, child_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(false));
    write_endpoint_file_naming_process(runtime_directory.path(), session_id, child.id());

    let end_error = end_session(runtime_directory.path(), None, session_id)
        .expect_err("an unconfirmed process is not ended");

    assert_eq!(
        end_error.to_string(),
        format!(
            "session {session_id} is not running; koshi cannot confirm that process {process_id} is {session_id}, and leaves it running. If it is, end it with: {kill_command}",
            process_id = child.id(),
            kill_command = format_process_kill_command(child.id()),
        )
    );
    let CliError::Runtime { .. } = end_error else {
        panic!("expected Runtime, got {end_error:?}");
    };
    assert!(process_tree::is_process_running(&child_record));
    assert!(member_records.iter().all(process_tree::is_process_running));

    end_stand_in_session(child, &member_records);
}

#[test]
fn end_session_gives_the_quit_failure_for_an_endpoint_naming_no_running_process() {
    let runtime_directory = build_short_runtime_directory();
    let session_id = SessionId::new();
    write_endpoint_file_naming_process(runtime_directory.path(), session_id, NO_SUCH_PROCESS_ID);

    let end_error = end_session(runtime_directory.path(), None, session_id)
        .expect_err("nothing listens for the session");

    let CliError::SessionNotFound { session_name } = end_error else {
        panic!("expected SessionNotFound, got {end_error:?}");
    };
    assert_eq!(session_name, session_id.to_string());
}

#[test]
fn end_session_ends_a_confirmed_session_that_cannot_quit_and_every_process_under_it() {
    let runtime_directory = build_short_runtime_directory();
    let session_id = SessionId::new();
    let (mut child, child_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    write_endpoint_file_naming_process(runtime_directory.path(), session_id, child.id());

    let session_ending =
        end_session(runtime_directory.path(), None, session_id).expect("the session is ended");

    assert_eq!(
        session_ending,
        SessionEnding::Stopped {
            quit_failure: format!("session {session_id} is not running"),
            session_process_id: child.id(),
            stopped_process_count: STAND_IN_CHILD_COUNT,
        }
    );
    assert!(process_tree::wait_for_processes_to_end(
        std::slice::from_ref(&child_record),
        Duration::from_secs(5)
    ));
    child.wait().expect("the session process is reaped");
    assert!(process_tree::wait_for_processes_to_end(
        &member_records,
        Duration::from_secs(5)
    ));
}

/// A record of a process with id [`NO_SUCH_PROCESS_ID`] that runs
/// `executable_name`, as the current user when `is_owned_by_current_user` is
/// `true`.
fn build_invented_record(executable_name: &str, is_owned_by_current_user: bool) -> ProcessRecord {
    ProcessRecord {
        process_id: NO_SUCH_PROCESS_ID,
        parent_process_id: 1,
        started_at: std::time::UNIX_EPOCH,
        executable_name: executable_name.to_string(),
        is_owned_by_current_user,
        posix_session_id: None,
    }
}

#[test]
fn is_other_koshi_process_of_current_user_accepts_koshi_that_the_current_user_runs() {
    assert!(is_other_koshi_process_of_current_user(
        &build_invented_record("koshi", true)
    ));
    assert!(is_other_koshi_process_of_current_user(
        &build_invented_record("KOSHI.EXE", true)
    ));
}

#[test]
fn is_other_koshi_process_of_current_user_refuses_koshi_that_another_user_runs() {
    assert!(!is_other_koshi_process_of_current_user(
        &build_invented_record("koshi", false)
    ));
    assert!(!is_other_koshi_process_of_current_user(
        &build_invented_record("koshi.exe", false)
    ));
}

#[test]
fn end_session_ends_the_processes_under_a_session_that_exits_while_its_quit_runs() {
    // The test serves the session's socket. When the quit connects, the
    // stand-in session process is ended and reaped, and then the connection
    // closes with no answer.
    let runtime_directory = build_short_runtime_directory();
    let session_id = SessionId::new();
    let (mut child, child_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    write_endpoint_file_naming_process(runtime_directory.path(), session_id, child.id());
    let socket_address = compute_socket_address(runtime_directory.path(), session_id);
    let session_listener = Listener::bind(&socket_address).expect("bind the session socket");
    let exiting_session_thread = std::thread::spawn(move || {
        let quit_connection = session_listener.accept().expect("accept the quit");
        let _ = child.kill();
        let _ = child.wait();
        drop(quit_connection);
    });

    let session_ending = end_session(runtime_directory.path(), None, session_id);

    let _ = Connection::connect(&socket_address);
    exiting_session_thread
        .join()
        .expect("the stand-in session ended");
    assert_eq!(
        session_ending.expect("the session is ended"),
        SessionEnding::Quit {
            stopped_process_count: STAND_IN_CHILD_COUNT,
        }
    );
    assert!(!process_tree::is_process_running(&child_record));
    assert!(process_tree::wait_for_processes_to_end(
        &member_records,
        Duration::from_secs(5)
    ));
}

/// A stand-in process saved as `program_file_name` in `program_directory`
/// that serves the pane holder address of its own process for `session_id` in
/// `runtime_directory`. Hands back the process, and whether that address was
/// bound within 10 s.
fn start_stand_in_pane_holder(
    program_directory: &Path,
    runtime_directory: &Path,
    program_file_name: &str,
    session_id: SessionId,
) -> (Child, bool) {
    let pane_holder_child = start_program_process(
        build_stand_in_command(program_directory, program_file_name)
            .env(STAND_IN_PANE_HOLDER_DIRECTORY_VARIABLE, runtime_directory)
            .env(
                STAND_IN_PANE_HOLDER_SESSION_VARIABLE,
                session_id.to_string(),
            ),
    );
    let pane_holder_address =
        compute_supervisor_socket_address(runtime_directory, session_id, pane_holder_child.id());
    let wait_end = Instant::now() + Duration::from_secs(10);
    let is_address_bound = loop {
        if is_pane_holder_address_bound(&pane_holder_address) {
            break true;
        }
        if Instant::now() >= wait_end {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (pane_holder_child, is_address_bound)
}

/// Whether `pane_holder_address` is bound, read without connecting to it: the
/// pipe is listed on Windows, and the socket file exists on Linux and macOS.
fn is_pane_holder_address_bound(pane_holder_address: &str) -> bool {
    #[cfg(windows)]
    {
        process_tree::list_pipe_names()
            .iter()
            .any(|pipe_name| pipe_name == pane_holder_address)
    }
    #[cfg(not(windows))]
    {
        Path::new(pane_holder_address).exists()
    }
}

#[test]
fn list_pane_holder_records_lists_the_koshi_process_serving_a_pane_address_on_windows_alone() {
    let runtime_directory = build_short_runtime_directory();
    let session_id = SessionId::new();
    let (mut pane_holder_child, is_address_bound) = start_stand_in_pane_holder(
        runtime_directory.path(),
        runtime_directory.path(),
        format_stand_in_file_name(true),
        session_id,
    );
    let pane_holder_record = process_tree::find_process_record(pane_holder_child.id());

    let pane_holder_records = list_pane_holder_records(runtime_directory.path(), session_id);

    let _ = pane_holder_child.kill();
    let _ = pane_holder_child.wait();
    assert!(is_address_bound);
    let expected_pane_holder_records: Vec<ProcessRecord> = if cfg!(windows) {
        vec![pane_holder_record.expect("the stand-in pane holder runs")]
    } else {
        Vec::new()
    };
    assert_eq!(pane_holder_records, expected_pane_holder_records);
}

#[test]
fn list_pane_holder_records_leaves_out_a_pane_address_served_by_a_process_that_does_not_run_koshi()
{
    let runtime_directory = build_short_runtime_directory();
    let session_id = SessionId::new();
    let (mut pane_holder_child, is_address_bound) = start_stand_in_pane_holder(
        runtime_directory.path(),
        runtime_directory.path(),
        format_stand_in_file_name(false),
        session_id,
    );

    let pane_holder_records = list_pane_holder_records(runtime_directory.path(), session_id);

    let _ = pane_holder_child.kill();
    let _ = pane_holder_child.wait();
    assert!(is_address_bound);
    assert_eq!(pane_holder_records, Vec::new());
}

#[test]
fn end_session_ends_a_pane_holder_that_still_runs_after_its_session_quit_on_windows_alone() {
    // The test serves the session's socket. When the quit connects, the
    // stand-in session process is ended and reaped, and then the connection
    // closes with no answer. The stand-in pane holder keeps serving its
    // address, as a pane holder of a session that never sends it `Shutdown`.
    let runtime_directory = build_short_runtime_directory();
    let pane_holder_program_directory = build_short_runtime_directory();
    let session_id = SessionId::new();
    let (mut child, child_record, member_records) =
        start_stand_in_session(runtime_directory.path(), format_stand_in_file_name(true));
    let (mut pane_holder_child, is_address_bound) = start_stand_in_pane_holder(
        pane_holder_program_directory.path(),
        runtime_directory.path(),
        format_stand_in_file_name(true),
        session_id,
    );
    let pane_holder_record = process_tree::find_process_record(pane_holder_child.id());
    write_endpoint_file_naming_process(runtime_directory.path(), session_id, child.id());
    let socket_address = compute_socket_address(runtime_directory.path(), session_id);
    let session_listener = Listener::bind(&socket_address).expect("bind the session socket");
    let exiting_session_thread = std::thread::spawn(move || {
        let quit_connection = session_listener.accept().expect("accept the quit");
        let _ = child.kill();
        let _ = child.wait();
        drop(quit_connection);
    });

    let session_ending = end_session(runtime_directory.path(), None, session_id);

    let _ = Connection::connect(&socket_address);
    exiting_session_thread
        .join()
        .expect("the stand-in session ended");
    let pane_holder_end_wait_duration = if cfg!(windows) {
        Duration::from_secs(5)
    } else {
        Duration::ZERO
    };
    let has_pane_holder_ended = pane_holder_record
        .as_ref()
        .is_some_and(|pane_holder_record| {
            process_tree::wait_for_processes_to_end(
                std::slice::from_ref(pane_holder_record),
                pane_holder_end_wait_duration,
            )
        });
    let _ = pane_holder_child.kill();
    let _ = pane_holder_child.wait();
    assert!(is_address_bound);
    assert!(pane_holder_record.is_some());
    let expected_stopped_process_count = if cfg!(windows) {
        STAND_IN_CHILD_COUNT + 1
    } else {
        STAND_IN_CHILD_COUNT
    };
    assert_eq!(
        session_ending.expect("the session is ended"),
        SessionEnding::Quit {
            stopped_process_count: expected_stopped_process_count,
        }
    );
    assert_eq!(has_pane_holder_ended, cfg!(windows));
    assert!(!process_tree::is_process_running(&child_record));
    assert!(process_tree::wait_for_processes_to_end(
        &member_records,
        Duration::from_secs(5)
    ));
}

#[test]
fn stop_pane_holder_trees_left_running_ends_the_listed_processes_that_outlive_their_holder() {
    // The pane holder ends before the call. The two processes listed under it
    // keep running.
    let (pane_holder_child, pane_holder_record) = start_recorded_long_running_child();
    end_child(pane_holder_child);
    let (first_member_child, first_member_record) = start_recorded_long_running_child();
    let (second_member_child, second_member_record) = start_recorded_long_running_child();
    let member_records = vec![first_member_record, second_member_record];

    let stopped_records = stop_pane_holder_trees_left_running(
        vec![pane_holder_record],
        member_records.clone(),
        SessionId::new(),
    );

    let has_every_member_ended =
        process_tree::wait_for_processes_to_end(&member_records, Duration::from_secs(5));
    end_child(first_member_child);
    end_child(second_member_child);
    assert_eq!(
        stopped_records.expect("the process list is readable"),
        member_records
    );
    assert!(has_every_member_ended);
}

#[test]
fn stop_pane_holder_trees_left_running_ends_nothing_when_the_holder_and_its_listed_processes_have_ended(
) {
    let (pane_holder_child, pane_holder_record) = start_recorded_long_running_child();
    let (member_child, member_record) = start_recorded_long_running_child();
    end_child(pane_holder_child);
    end_child(member_child);

    let stopped_records = stop_pane_holder_trees_left_running(
        vec![pane_holder_record],
        vec![member_record],
        SessionId::new(),
    );

    assert_eq!(
        stopped_records.expect("the process list is readable"),
        Vec::new()
    );
}

#[test]
fn stop_pane_holder_trees_left_running_ends_a_holder_that_still_runs_with_every_process_under_it() {
    let program_directory = build_short_runtime_directory();
    let (pane_holder_child, pane_holder_record, member_records) =
        start_stand_in_session(program_directory.path(), format_stand_in_file_name(true));

    let stopped_records = stop_pane_holder_trees_left_running(
        vec![pane_holder_record.clone()],
        member_records.clone(),
        SessionId::new(),
    );

    let mut expected_stopped_records = member_records.clone();
    expected_stopped_records.push(pane_holder_record);
    let has_every_process_ended =
        process_tree::wait_for_processes_to_end(&expected_stopped_records, Duration::from_secs(5));
    end_stand_in_session(pane_holder_child, &member_records);
    assert_eq!(
        stopped_records.expect("the process list is readable"),
        expected_stopped_records
    );
    assert!(has_every_process_ended);
}

#[test]
fn stop_pane_holder_trees_left_running_leaves_running_the_processes_under_an_ended_holder_id_now_reused(
) {
    // The ended pane holder's record carries the id of a running stand-in
    // process and an earlier start time. A child listed under the ended pane
    // holder still runs.
    let program_directory = build_short_runtime_directory();
    let (reusing_child, reusing_record, reusing_member_records) =
        start_stand_in_session(program_directory.path(), format_stand_in_file_name(false));
    let ended_pane_holder_record = ProcessRecord {
        started_at: std::time::UNIX_EPOCH + Duration::from_secs(1),
        ..reusing_record.clone()
    };
    let (outliving_child, outliving_record) = start_recorded_long_running_child();

    let stopped_records = stop_pane_holder_trees_left_running(
        vec![ended_pane_holder_record],
        vec![outliving_record.clone()],
        SessionId::new(),
    );

    let is_reusing_tree_running = process_tree::is_process_running(&reusing_record)
        && reusing_member_records
            .iter()
            .all(process_tree::is_process_running);
    end_child(outliving_child);
    end_stand_in_session(reusing_child, &reusing_member_records);
    assert_eq!(
        stopped_records.expect("the process list is readable"),
        vec![outliving_record]
    );
    assert!(is_reusing_tree_running);
}

#[test]
fn stop_session_tree_ends_a_process_listed_under_a_pane_holder_before_quit_that_no_walk_reaches() {
    // Neither process has a child, and the process listed under a pane holder
    // is not under the session process.
    let (session_child, session_record) = start_recorded_long_running_child();
    let (orphaned_child, orphaned_record) = start_recorded_long_running_child();

    let stopped_records = stop_session_tree(
        &session_record,
        Vec::new(),
        Vec::new(),
        vec![orphaned_record.clone()],
        SessionId::new(),
    );

    let expected_stopped_records = vec![orphaned_record, session_record];
    let has_every_process_ended =
        process_tree::wait_for_processes_to_end(&expected_stopped_records, Duration::from_secs(5));
    end_child(orphaned_child);
    end_child(session_child);
    assert_eq!(
        stopped_records.expect("the process list is readable"),
        expected_stopped_records
    );
    assert!(has_every_process_ended);
}

#[test]
fn append_unlisted_records_adds_each_process_not_listed_yet_once_in_order() {
    let listed_record = ProcessRecord {
        process_id: 5000,
        ..build_invented_record("sleep", true)
    };
    let renamed_record = ProcessRecord {
        executable_name: "ping".to_string(),
        ..listed_record.clone()
    };
    let reused_process_id_record = ProcessRecord {
        started_at: std::time::UNIX_EPOCH + Duration::from_secs(1),
        ..listed_record.clone()
    };
    let other_process_record = ProcessRecord {
        process_id: 5001,
        ..listed_record.clone()
    };
    let mut listed_records = vec![listed_record.clone()];

    append_unlisted_records(
        &mut listed_records,
        vec![
            renamed_record,
            reused_process_id_record.clone(),
            other_process_record.clone(),
            other_process_record.clone(),
        ],
    );

    assert_eq!(
        listed_records,
        vec![
            listed_record,
            reused_process_id_record,
            other_process_record
        ]
    );
}
