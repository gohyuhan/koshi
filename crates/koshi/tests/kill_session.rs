//! `koshi kill-session` against a real session server: the session ends, and
//! so does every process its panes started, whether the session answers
//! `Quit` or not.
//!
//! Each test opens one pane that runs a script with `/bin/sh -c` on Linux and
//! macOS, and with `cmd.exe /C` on Windows. On Linux and macOS the session's
//! process holds its panes. On Windows a koshi process of its own holds them,
//! and every pane process runs under it. Where that changes what
//! `kill-session` prints, the expected output is set per platform.

use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_host::process_tree::{self, ProcessRecord};

mod common;

use common::session_connection::{
    attach_client_on_connection, build_pane, open_session_connection,
};
#[cfg(unix)]
use common::RunningProcess;
use common::{
    build_koshi_command_under_home, build_shell_spawn_spec, build_short_test_directory,
    resolve_runtime_directory_under_home, start_session_server_under_home, write_test_config,
    SessionProcess, POLL_INTERVAL_DURATION, WAIT_DURATION,
};
use koshi_test_support::fixtures::start_program_process;

/// The file the pane script writes its first background job's process id
/// into, inside the test home.
#[cfg(unix)]
const BACKGROUND_JOB_FILE_NAME: &str = "background-job-pid";

/// The file the pane script writes its late background job's process id into,
/// inside the test home.
#[cfg(unix)]
const LATE_BACKGROUND_JOB_FILE_NAME: &str = "late-background-job-pid";

/// A pane script that starts `sleep 600` as a background job in a process
/// group of its own, writes that job's process id into
/// [`BACKGROUND_JOB_FILE_NAME`], and then runs `sleep 600` itself.
#[cfg(unix)]
const PANE_SHELL_SCRIPT: &str = "set -m
sleep 600 &
echo $! > \"$HOME/background-job-pid.tmp\"
mv \"$HOME/background-job-pid.tmp\" \"$HOME/background-job-pid\"
exec sleep 600
";

/// A pane script that starts `ping -n 600 127.0.0.1` in the background with
/// `start /b`, and then runs the same `ping` itself.
#[cfg(windows)]
const PANE_SHELL_SCRIPT: &str = "start /b ping -n 600 127.0.0.1 >nul & ping -n 600 127.0.0.1 >nul";

/// [`PANE_SHELL_SCRIPT`], with a second background job started 2 s after the
/// first, whose process id goes into [`LATE_BACKGROUND_JOB_FILE_NAME`].
#[cfg(unix)]
const LATE_JOB_PANE_SHELL_SCRIPT: &str = "set -m
sleep 600 &
echo $! > \"$HOME/background-job-pid.tmp\"
mv \"$HOME/background-job-pid.tmp\" \"$HOME/background-job-pid\"
sleep 2
sleep 600 &
echo $! > \"$HOME/late-background-job-pid.tmp\"
mv \"$HOME/late-background-job-pid.tmp\" \"$HOME/late-background-job-pid\"
exec sleep 600
";

/// [`PANE_SHELL_SCRIPT`], with a second background `ping` started once
/// `ping -n 3 127.0.0.1`, about 2 s, has ended.
#[cfg(windows)]
const LATE_JOB_PANE_SHELL_SCRIPT: &str = "start /b ping -n 600 127.0.0.1 >nul & ping -n 3 127.0.0.1 >nul & start /b ping -n 600 127.0.0.1 >nul & ping -n 600 127.0.0.1 >nul";

/// What `kill-session` prints for a session whose pane runs
/// [`PANE_SHELL_SCRIPT`] and that answers `Quit`: one process left running,
/// the background job in its own process group.
#[cfg(unix)]
const QUIT_SESSION_ENDING_OUTPUT: &str =
    "the session quit; koshi ended 1 process it left running\n";

/// What `kill-session` prints for a session whose pane runs
/// [`PANE_SHELL_SCRIPT`] and that answers `Quit`: nothing. The process
/// holding the panes ends every pane process when the session quits.
#[cfg(windows)]
const QUIT_SESSION_ENDING_OUTPUT: &str = "";

/// How long every process of the session has to be gone once `kill-session`
/// returned: 5 seconds.
const PROCESS_END_WAIT_DURATION: Duration = Duration::from_secs(5);

/// A session server under a fresh test home, whose second pane runs a pane
/// script. Dropping it ends the session server, then, on Linux and macOS, the
/// pane's first background job, then removes the home directory.
struct SessionWithScriptedPane {
    session_process: SessionProcess,
    #[cfg(unix)]
    _background_job_guard: RunningProcess,
    home_directory: tempfile::TempDir,
    session_id: SessionId,
    session_record: ProcessRecord,
}

/// Start a session server under a fresh test home, open a pane that runs
/// `pane_shell_script`, and wait until the script's first background job runs.
fn start_session_with_scripted_pane(pane_shell_script: &str) -> SessionWithScriptedPane {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);
    let session_endpoint =
        session_process.wait_for_session_endpoint(&runtime_directory, session_id);
    let session_record = process_tree::find_process_record(session_process.child_process.id())
        .expect("the session server runs");
    {
        let mut client_connection = open_session_connection(&session_endpoint);
        let client_id = attach_client_on_connection(&mut client_connection, session_id);
        let mut control_connection = open_session_connection(&session_endpoint);
        build_pane(
            &mut control_connection,
            session_id,
            client_id,
            Some(build_shell_spawn_spec(pane_shell_script)),
        );
    }
    #[cfg(unix)]
    let background_job_process_id =
        wait_for_background_job_process_id(&home_directory.path().join(BACKGROUND_JOB_FILE_NAME));
    #[cfg(windows)]
    wait_for_session_tree(&session_record, |session_tree_records| {
        count_ping_records(session_tree_records) == 2
    });
    SessionWithScriptedPane {
        session_process,
        #[cfg(unix)]
        _background_job_guard: RunningProcess {
            process_id: background_job_process_id,
        },
        home_directory,
        session_id,
        session_record,
    }
}

/// The process id the pane script wrote into `background_job_file_path`.
///
/// # Panics
/// When the file holds no process id within [`WAIT_DURATION`].
#[cfg(unix)]
fn wait_for_background_job_process_id(background_job_file_path: &Path) -> u32 {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Ok(background_job_text) = std::fs::read_to_string(background_job_file_path) {
            return background_job_text
                .trim()
                .parse()
                .expect("the file holds a process id");
        }
        assert!(
            Instant::now() < wait_deadline,
            "the pane script never started its job"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
}

/// Every process under `session_record`: each process that names the session
/// server, or a process already found, as its parent and started at or after
/// that parent. A process of any user and any program, koshi included, is
/// kept. The session server itself is left out.
fn list_session_tree_records(session_record: &ProcessRecord) -> Vec<ProcessRecord> {
    let process_records =
        process_tree::list_process_records().expect("the processes on this machine are listed");
    let mut session_tree_records: Vec<ProcessRecord> = Vec::new();
    let mut parent_records = vec![session_record.clone()];
    while let Some(parent_record) = parent_records.pop() {
        for process_record in &process_records {
            let is_child = process_record.parent_process_id == parent_record.process_id
                && process_record.started_at >= parent_record.started_at;
            let is_listed = session_tree_records
                .iter()
                .any(|session_tree_record| session_tree_record.is_same_process(process_record));
            if is_child && !is_listed {
                session_tree_records.push(process_record.clone());
                parent_records.push(process_record.clone());
            }
        }
    }
    session_tree_records
}

/// List the processes under `session_record` every [`POLL_INTERVAL_DURATION`]
/// until two lists in a row name the same processes and `is_ready` accepts
/// that list, and hand it back.
///
/// # Panics
/// When no such list comes within [`WAIT_DURATION`].
fn wait_for_session_tree(
    session_record: &ProcessRecord,
    is_ready: impl Fn(&[ProcessRecord]) -> bool,
) -> Vec<ProcessRecord> {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    let mut previous_tree_records = list_session_tree_records(session_record);
    loop {
        std::thread::sleep(POLL_INTERVAL_DURATION);
        let session_tree_records = list_session_tree_records(session_record);
        if session_tree_records == previous_tree_records && is_ready(&session_tree_records) {
            return session_tree_records;
        }
        assert!(
            Instant::now() < wait_deadline,
            "the processes under the session never settled: {session_tree_records:?}"
        );
        previous_tree_records = session_tree_records;
    }
}

/// How many of `process_records` run `PING.EXE`, in any letter case.
#[cfg(windows)]
fn count_ping_records(process_records: &[ProcessRecord]) -> usize {
    process_records
        .iter()
        .filter(|process_record| {
            process_record
                .executable_name
                .eq_ignore_ascii_case("PING.EXE")
        })
        .count()
}

/// Start `koshi kill-session <session_id>` under `home_directory`, with both
/// output streams piped.
fn start_kill_session(home_directory: &Path, session_id: SessionId) -> Child {
    start_program_process(
        build_koshi_command_under_home(home_directory)
            .arg("kill-session")
            .arg(session_id.to_string()),
    )
}

/// Wait for `kill_session_process` to end, and hand back its exit code and
/// standard output.
///
/// # Panics
/// When it wrote to standard error.
fn read_kill_session_output(kill_session_process: Child) -> (Option<i32>, String) {
    let kill_output = kill_session_process
        .wait_with_output()
        .expect("kill-session ends");
    assert_eq!(
        String::from_utf8_lossy(&kill_output.stderr),
        "",
        "kill-session wrote to standard error"
    );
    (
        kill_output.status.code(),
        String::from_utf8_lossy(&kill_output.stdout).into_owned(),
    )
}

/// Stop the process `process_id` from running any code: `SIGSTOP`.
#[cfg(unix)]
fn suspend_process(process_id: u32) {
    // SAFETY: `kill` takes a process id and a signal number, and reads no
    // memory of this process.
    let stop_answer = unsafe {
        libc::kill(
            libc::pid_t::try_from(process_id).expect("the id fits pid_t"),
            libc::SIGSTOP,
        )
    };
    assert_eq!(stop_answer, 0);
}

/// Stop the process `process_id` from running any code: every thread of it is
/// suspended. The threads are listed again after each round, until a round
/// finds no thread it has not suspended yet. A thread that ended before it
/// could be suspended counts as handled.
///
/// # Panics
/// When the process has no thread.
#[cfg(windows)]
fn suspend_process(process_id: u32) {
    use std::collections::HashSet;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenThread, SuspendThread, THREAD_SUSPEND_RESUME};

    let mut handled_thread_ids: HashSet<u32> = HashSet::new();
    loop {
        let new_thread_ids: Vec<u32> = list_thread_ids(process_id)
            .into_iter()
            .filter(|thread_id| !handled_thread_ids.contains(thread_id))
            .collect();
        if new_thread_ids.is_empty() {
            assert!(
                !handled_thread_ids.is_empty(),
                "process {process_id} has no thread"
            );
            return;
        }
        for thread_id in new_thread_ids {
            handled_thread_ids.insert(thread_id);
            // SAFETY: the call takes plain values.
            let thread_handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
            if thread_handle.is_null() {
                continue;
            }
            // SAFETY: `thread_handle` is a thread handle this call opened with
            // `THREAD_SUSPEND_RESUME`, and it is closed once.
            unsafe {
                SuspendThread(thread_handle);
                CloseHandle(thread_handle);
            }
        }
    }
}

/// The ids of every thread of the process `process_id`, from one
/// `TH32CS_SNAPTHREAD` snapshot.
///
/// # Panics
/// When the snapshot cannot be taken.
#[cfg(windows)]
fn list_thread_ids(process_id: u32) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };

    // SAFETY: the call takes plain values.
    let snapshot_handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    assert_ne!(
        snapshot_handle, INVALID_HANDLE_VALUE,
        "the thread snapshot is taken"
    );
    let mut thread_entry = THREADENTRY32 {
        dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>())
            .expect("the entry size fits u32"),
        ..THREADENTRY32::default()
    };
    let mut thread_ids = Vec::new();
    // SAFETY: `snapshot_handle` is an open snapshot, and `thread_entry` lives
    // for the call with `dwSize` set.
    let mut has_thread_entry = unsafe { Thread32First(snapshot_handle, &mut thread_entry) } != 0;
    while has_thread_entry {
        if thread_entry.th32OwnerProcessID == process_id {
            thread_ids.push(thread_entry.th32ThreadID);
        }
        // SAFETY: as for `Thread32First`.
        has_thread_entry = unsafe { Thread32Next(snapshot_handle, &mut thread_entry) } != 0;
    }
    // SAFETY: `snapshot_handle` is the open snapshot, closed once.
    unsafe {
        CloseHandle(snapshot_handle);
    }
    thread_ids
}

#[test]
fn kill_session_quits_the_session_and_ends_every_process_its_panes_started() {
    let mut running_session = start_session_with_scripted_pane(PANE_SHELL_SCRIPT);
    let session_tree_records = list_session_tree_records(&running_session.session_record);

    let (exit_code, standard_output) = read_kill_session_output(start_kill_session(
        running_session.home_directory.path(),
        running_session.session_id,
    ));

    assert_eq!(exit_code, Some(0));
    assert_eq!(standard_output, QUIT_SESSION_ENDING_OUTPUT);
    assert!(process_tree::wait_for_processes_to_end(
        std::slice::from_ref(&running_session.session_record),
        PROCESS_END_WAIT_DURATION
    ));
    running_session
        .session_process
        .child_process
        .wait()
        .expect("the session server is reaped");
    assert!(process_tree::wait_for_processes_to_end(
        &session_tree_records,
        PROCESS_END_WAIT_DURATION
    ));
}

#[test]
fn kill_session_ends_a_session_that_does_not_answer_and_every_process_under_it() {
    let mut running_session = start_session_with_scripted_pane(LATE_JOB_PANE_SHELL_SCRIPT);
    let session_process_id = running_session.session_record.process_id;
    suspend_process(session_process_id);

    let kill_session_process = start_kill_session(
        running_session.home_directory.path(),
        running_session.session_id,
    );
    #[cfg(unix)]
    let late_background_job_guard = RunningProcess {
        process_id: wait_for_background_job_process_id(
            &running_session
                .home_directory
                .path()
                .join(LATE_BACKGROUND_JOB_FILE_NAME),
        ),
    };
    let session_tree_records =
        wait_for_session_tree(&running_session.session_record, |session_tree_records| {
            #[cfg(unix)]
            let has_late_job = session_tree_records.iter().any(|session_tree_record| {
                session_tree_record.process_id == late_background_job_guard.process_id
            });
            #[cfg(windows)]
            let has_late_job = count_ping_records(session_tree_records) == 3;
            has_late_job
        });
    let (exit_code, standard_output) = read_kill_session_output(kill_session_process);

    assert_eq!(exit_code, Some(0));
    assert_eq!(
        standard_output,
        format!(
            "the session did not quit (IPC unavailable: the session did not answer in time); koshi ended its process {session_process_id} and {} processes under it\n",
            session_tree_records.len()
        )
    );
    assert!(process_tree::wait_for_processes_to_end(
        std::slice::from_ref(&running_session.session_record),
        PROCESS_END_WAIT_DURATION
    ));
    running_session
        .session_process
        .child_process
        .wait()
        .expect("the session server is reaped");
    assert!(process_tree::wait_for_processes_to_end(
        &session_tree_records,
        PROCESS_END_WAIT_DURATION
    ));
}
