//! Ending one session for `kill-session`, whatever koshi version it runs.
//!
//! [`end_session`](crate::session_end::end_session) asks the session to quit.
//! When the session's endpoint file names a process that
//! `find_server_process_record` confirms, that process and
//! every process started under it end, whether or not the session quits.
//! Example: a session that runs an older koshi and refuses this build's
//! protocol version is ended through the operating system, with the `sleep`
//! a pane left running in the background.

use std::path::Path;
use std::time::{Duration, Instant};

use koshi_core::command::{Command, CommandResult};
use koshi_core::ids::SessionId;
use koshi_host::process_tree::{self, ProcessRecord};
use koshi_ipc::endpoint::{find_endpoint_process_record, EndpointFile};
use koshi_link::error::CliError;
use koshi_link::ipc_client;

#[cfg(test)]
pub(crate) mod tests;

/// How long the session has to answer `Quit`.
const QUIT_ANSWER_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// How long the session's process has to end after it answered `Quit`.
const SESSION_EXIT_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

/// How long a process has to end after `SIGTERM` before it gets `SIGKILL`,
/// and how long the server's process has to be gone after that. On Windows,
/// also how long the processes holding a session's panes, and the processes
/// listed under them, have to end once the session quit.
pub(crate) const PROCESS_STOP_GRACE_DURATION: Duration = Duration::from_secs(2);

/// How one session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnding {
    /// The session quit. `stopped_process_count` processes started under it
    /// still ran after that, and koshi ended them.
    Quit { stopped_process_count: usize },
    /// The session's process still ran after `Quit`, and koshi ended it.
    Stopped {
        /// What `Quit` gave instead of ending the session, such as
        /// `IPC unavailable: the session did not answer in time`.
        quit_failure: String,
        /// The id of the session's process koshi ended.
        session_process_id: u32,
        /// How many processes started under the session still ran, and koshi
        /// ended.
        stopped_process_count: usize,
    },
}

/// End the session `session_id`, found in `runtime_directory` or, for a
/// session another local user started, in `shared_sessions_base_directory`.
///
/// A session whose process `find_server_process_record` confirms ends in
/// these steps:
///
/// 1. The processes under the session's process are listed. On Windows, the
///    processes holding the session's panes and the processes under them are
///    listed too.
/// 2. The session is sent `Quit`, and has 5 s to answer.
/// 3. If it answered and its process ends within 2 s, or its process is gone
///    after a failed `Quit`, the processes under the session's process from
///    step 1 that still run are ended: [`SessionEnding::Quit`]. On Windows,
///    the processes holding the panes and the processes listed under them in
///    step 1 then have 2 s to end. If one of them still runs after that, each
///    one that still runs is ended. For each process holding the panes that
///    still runs, every process under it at that moment is ended too.
/// 4. Otherwise the processes under the session's process are listed again.
///    On Windows, the processes holding the session's panes and the processes
///    under them are added. All of them, then the session's process, are
///    ended, with the processes from step 1: [`SessionEnding::Stopped`].
///
/// A session whose process is not confirmed is sent `Quit` alone, with 5 s
/// to answer: [`SessionEnding::Quit`] with `stopped_process_count` `0`.
///
/// `stopped_process_count` counts the listed processes that still run when
/// koshi ends them. A process that ends before that is not counted. On
/// Windows, a listed process that ends with the process holding its pane,
/// after koshi ended that process, is counted. Example: on Windows, a
/// session whose pane runs `cmd.exe` and `ping` quits, and the process
/// holding its panes ends with both within 2 s: [`SessionEnding::Quit`] with
/// `stopped_process_count` `0`.
///
/// # Errors
/// - A session that cannot be found: what
///   [`load_session_endpoint`](ipc_client::load_session_endpoint) gives.
/// - A confirmed session whose process list cannot be read:
///   [`CliError::Runtime`] naming the read failure, with nothing ended when
///   it is the list of step 1.
/// - A confirmed session whose process still runs 2 s after koshi ended it:
///   [`CliError::Runtime`].
/// - An unconfirmed session that does not quit: the `Quit` failure. While a
///   process with the endpoint file's process id runs, it is
///   [`CliError::Runtime`] naming that failure, the process id, and the
///   command that ends it.
pub fn end_session(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
) -> Result<SessionEnding, CliError> {
    let endpoint_file = ipc_client::load_session_endpoint(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
    )?;
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let Some(session_record) =
        find_server_process_record(&endpoint_file_path, endpoint_file.process_id)
    else {
        return quit_unconfirmed_session(&endpoint_file, session_id);
    };

    let session_records = std::slice::from_ref(&session_record);
    let first_pane_holder_records = list_pane_holder_records(runtime_directory, session_id);
    let first_process_records = list_running_process_records(session_id)?;
    let first_member_records =
        process_tree::list_member_records(session_records, &first_process_records);
    let first_holder_member_records =
        process_tree::list_member_records(&first_pane_holder_records, &first_process_records);
    let quit_failure = match submit_quit(&endpoint_file, session_id) {
        Ok(()) => {
            let has_session_exited = process_tree::wait_for_processes_to_end(
                session_records,
                SESSION_EXIT_TIMEOUT_DURATION,
            );
            (!has_session_exited).then(|| {
                format!(
                    "it answered quit and still ran {} s after it",
                    SESSION_EXIT_TIMEOUT_DURATION.as_secs()
                )
            })
        }
        Err(_) if !process_tree::is_process_running(&session_record) => None,
        Err(quit_error) => Some(format_quit_failure(&quit_error)),
    };
    let mut pane_holder_records = list_pane_holder_records(runtime_directory, session_id);
    append_unlisted_records(
        &mut pane_holder_records,
        first_pane_holder_records
            .into_iter()
            .filter(process_tree::is_process_running),
    );
    let Some(quit_failure) = quit_failure else {
        let mut stopped_records =
            process_tree::stop_processes(&first_member_records, PROCESS_STOP_GRACE_DURATION);
        stopped_records.extend(stop_pane_holder_trees_left_running(
            pane_holder_records,
            first_holder_member_records,
            session_id,
        )?);
        return Ok(SessionEnding::Quit {
            stopped_process_count: stopped_records.len(),
        });
    };

    let stopped_records = stop_session_tree(
        &session_record,
        pane_holder_records,
        first_member_records,
        first_holder_member_records,
        session_id,
    )?;
    if !process_tree::wait_for_processes_to_end(session_records, PROCESS_STOP_GRACE_DURATION) {
        return Err(CliError::Runtime {
            detail: format!(
                "{session_id} did not quit ({quit_failure}), and its process {} still runs after koshi ended it",
                session_record.process_id
            ),
        });
    }
    Ok(SessionEnding::Stopped {
        quit_failure,
        session_process_id: session_record.process_id,
        stopped_process_count: stopped_records
            .iter()
            .filter(|stopped_record| !stopped_record.is_same_process(&session_record))
            .count(),
    })
}

/// End what the processes holding a session's panes, its pane holders, leave
/// running once the session quit. Give back the records of the processes that
/// ran when koshi ended them.
///
/// `pane_holder_records` and `first_holder_member_records`, the processes
/// listed under the pane holders before `Quit`, have 2 s to end. If one of
/// them still runs after that, koshi ends these processes:
///
/// 1. each of `first_holder_member_records` that still runs,
/// 2. each process under a pane holder that still runs, listed at that moment,
/// 3. each pane holder that still runs.
///
/// Example: the pane holder `5000` ends within 2 s, and the `ping` `5002`
/// listed under it still runs: `5002` is ended and given back, and `5000` is
/// not. When `5002` also ends within the 2 s, nothing is ended and the list is
/// empty.
///
/// # Errors
/// [`CliError::Runtime`] naming `session_id` and the failure of a process list
/// the system did not give, with nothing ended by this call.
fn stop_pane_holder_trees_left_running(
    pane_holder_records: Vec<ProcessRecord>,
    first_holder_member_records: Vec<ProcessRecord>,
    session_id: SessionId,
) -> Result<Vec<ProcessRecord>, CliError> {
    let mut holder_wait_records = first_holder_member_records.clone();
    holder_wait_records.extend(pane_holder_records.iter().cloned());
    if process_tree::wait_for_processes_to_end(&holder_wait_records, PROCESS_STOP_GRACE_DURATION) {
        return Ok(Vec::new());
    }
    let process_records = list_running_process_records(session_id)?;
    let running_pane_holder_records: Vec<ProcessRecord> = pane_holder_records
        .into_iter()
        .filter(|pane_holder_record| {
            process_records
                .iter()
                .any(|process_record| process_record.is_same_process(pane_holder_record))
        })
        .collect();
    let mut holder_stop_records = first_holder_member_records;
    append_unlisted_records(
        &mut holder_stop_records,
        process_tree::list_member_records(&running_pane_holder_records, &process_records),
    );
    holder_stop_records.extend(running_pane_holder_records);
    Ok(process_tree::stop_processes(
        &holder_stop_records,
        PROCESS_STOP_GRACE_DURATION,
    ))
}

/// End the session process `session_record`, which did not quit, with every
/// process under it. Give back the records of the processes that ran when
/// koshi ended them, the session process last.
///
/// The processes are ended in this order, each one once:
///
/// 1. `pane_holder_records`, the processes holding the session's panes,
/// 2. `first_member_records` and `first_holder_member_records`, the processes
///    listed under the session process and under the pane holders before
///    `Quit`,
/// 3. the processes under `pane_holder_records` and `session_record`, listed
///    at that moment,
/// 4. `session_record`.
///
/// Example: a `ping` `5002` is listed under a pane holder before `Quit`, and
/// its parent ends after that. The list of step 3 does not hold `5002`, and
/// step 2 ends it.
///
/// # Errors
/// [`CliError::Runtime`] naming `session_id` and the failure of a process list
/// the system did not give, with nothing ended by this call.
fn stop_session_tree(
    session_record: &ProcessRecord,
    pane_holder_records: Vec<ProcessRecord>,
    first_member_records: Vec<ProcessRecord>,
    first_holder_member_records: Vec<ProcessRecord>,
    session_id: SessionId,
) -> Result<Vec<ProcessRecord>, CliError> {
    let mut root_records = pane_holder_records;
    root_records.push(session_record.clone());
    let mut stop_records: Vec<ProcessRecord> = root_records
        .iter()
        .filter(|root_record| !root_record.is_same_process(session_record))
        .cloned()
        .collect();
    let current_member_records = process_tree::list_member_records(
        &root_records,
        &list_running_process_records(session_id)?,
    );
    append_unlisted_records(
        &mut stop_records,
        first_member_records
            .into_iter()
            .chain(first_holder_member_records)
            .chain(current_member_records),
    );
    stop_records.push(session_record.clone());
    Ok(process_tree::stop_processes(
        &stop_records,
        PROCESS_STOP_GRACE_DURATION,
    ))
}

/// Add to `listed_records` each of `candidate_records`, in order, that names
/// a process `listed_records` does not name yet, as
/// [`is_same_process`](ProcessRecord::is_same_process) reads it.
///
/// Example: `listed_records` names process `5000`, and `candidate_records`
/// names `5000` with the same start time, then `5001` twice: `5001` is added
/// once.
fn append_unlisted_records(
    listed_records: &mut Vec<ProcessRecord>,
    candidate_records: impl IntoIterator<Item = ProcessRecord>,
) {
    for candidate_record in candidate_records {
        let is_listed = listed_records
            .iter()
            .any(|listed_record| listed_record.is_same_process(&candidate_record));
        if !is_listed {
            listed_records.push(candidate_record);
        }
    }
}

/// The koshi server process — a session or the router — that the endpoint
/// file at `endpoint_file_path` names as `process_id`, when both of these
/// hold:
///
/// 1. [`find_endpoint_process_record`] finds it: it runs, and it started in
///    the whole second of the file's modification time or earlier.
/// 2. [`is_other_koshi_process_of_current_user`] accepts it.
///
/// `None` otherwise. Example: a session that ended long ago left its file
/// naming process `5000`, and a `zsh` started an hour after that file now has
/// id `5000`: `None`.
pub(crate) fn find_server_process_record(
    endpoint_file_path: &Path,
    process_id: u32,
) -> Option<ProcessRecord> {
    find_endpoint_process_record(endpoint_file_path, process_id)
        .filter(is_other_koshi_process_of_current_user)
}

/// Whether `process_record` names a koshi process of the current user other
/// than this one: it runs as the current user, its program is koshi, as
/// [`is_koshi_executable_name`](process_tree::is_koshi_executable_name) reads
/// it, and its id is not the id of this process.
fn is_other_koshi_process_of_current_user(process_record: &ProcessRecord) -> bool {
    process_record.is_owned_by_current_user
        && process_tree::is_koshi_executable_name(&process_record.executable_name)
        && process_record.process_id != std::process::id()
}

/// Send `Quit` to the session `session_id` at `endpoint_file`, which has 5 s
/// to answer.
///
/// # Errors
/// A rejected `Quit` is [`CliError::CommandRejected`]. Every other failure is
/// what [`submit_external_command_to_endpoint`](ipc_client::submit_external_command_to_endpoint)
/// gives.
fn submit_quit(endpoint_file: &EndpointFile, session_id: SessionId) -> Result<(), CliError> {
    match ipc_client::submit_external_command_to_endpoint(
        endpoint_file,
        session_id,
        Command::Quit,
        Some(Instant::now() + QUIT_ANSWER_TIMEOUT_DURATION),
    )? {
        CommandResult::Ok { .. } => Ok(()),
        CommandResult::Rejected { reason, help, .. } => {
            Err(CliError::CommandRejected { reason, help })
        }
    }
}

/// Send `Quit` to a session whose process is not confirmed, and end nothing
/// through the operating system.
///
/// # Errors
/// The `Quit` failure. While a process with the endpoint file's process id
/// runs, the failure is [`CliError::Runtime`], for example
/// `IPC unavailable: the session did not answer in time; koshi cannot confirm
/// that process 5000 is session-…, and leaves it running. If it is, end it
/// with: kill 5000`.
fn quit_unconfirmed_session(
    endpoint_file: &EndpointFile,
    session_id: SessionId,
) -> Result<SessionEnding, CliError> {
    let Err(quit_error) = submit_quit(endpoint_file, session_id) else {
        return Ok(SessionEnding::Quit {
            stopped_process_count: 0,
        });
    };
    if process_tree::find_process_record(endpoint_file.process_id).is_none() {
        return Err(quit_error);
    }
    Err(CliError::Runtime {
        detail: format!(
            "{quit_failure}; koshi cannot confirm that process {process_id} is {session_id}, and leaves it running. If it is, end it with: {kill_command}",
            quit_failure = format_quit_failure(&quit_error),
            process_id = endpoint_file.process_id,
            kill_command = format_process_kill_command(endpoint_file.process_id),
        ),
    })
}

/// The sentence a failed `Quit` reports: the `detail` of
/// [`CliError::PreviousReleaseServer`], without its `koshi restart-servers`
/// advice, and the error's own sentence for every other failure.
///
/// Example — a koshi 0.4.0 session gives `the server answered in the format of
/// koshi 0.4.0 or older, which this koshi cannot talk to`.
fn format_quit_failure(quit_error: &CliError) -> String {
    match quit_error {
        CliError::PreviousReleaseServer { detail } => detail.clone(),
        other_quit_error => other_quit_error.to_string(),
    }
}

/// The command a user types to end the process `process_id`: `kill 5000` on
/// Linux and macOS, `taskkill /PID 5000 /T /F` on Windows.
pub(crate) fn format_process_kill_command(process_id: u32) -> String {
    if cfg!(windows) {
        format!("taskkill /PID {process_id} /T /F")
    } else {
        format!("kill {process_id}")
    }
}

/// Every running process this process can read, as
/// [`list_process_records`](process_tree::list_process_records) gives them.
///
/// # Errors
/// [`CliError::Runtime`] naming `session_id` and the failure of a process
/// list the system did not give.
fn list_running_process_records(session_id: SessionId) -> Result<Vec<ProcessRecord>, CliError> {
    process_tree::list_process_records().map_err(|list_error| CliError::Runtime {
        detail: format!(
            "cannot list the processes on this machine to end the ones under {session_id}: {list_error}"
        ),
    })
}

/// The processes holding the panes of `session_id`.
///
/// On Windows each one is found by its pipe, named
/// [`compute_supervisor_socket_address`](koshi_ipc::supervisor::compute_supervisor_socket_address)
/// for its own process id. A pipe counts when the process serving it has the
/// id its name ends with, and [`is_other_koshi_process_of_current_user`]
/// accepts that process. On Linux and macOS the session's process holds its
/// own panes, and the list is empty.
fn list_pane_holder_records(runtime_directory: &Path, session_id: SessionId) -> Vec<ProcessRecord> {
    #[cfg(windows)]
    {
        process_tree::list_pipe_names()
            .into_iter()
            .filter_map(|pipe_name| {
                let (_, process_id_text) = pipe_name.rsplit_once('-')?;
                let holder_process_id: u32 = process_id_text.parse().ok()?;
                let expected_pipe_name = koshi_ipc::supervisor::compute_supervisor_socket_address(
                    runtime_directory,
                    session_id,
                    holder_process_id,
                );
                if pipe_name != expected_pipe_name
                    || process_tree::find_pipe_server_process_id(&pipe_name)
                        != Some(holder_process_id)
                {
                    return None;
                }
                process_tree::find_process_record(holder_process_id)
                    .filter(is_other_koshi_process_of_current_user)
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = (runtime_directory, session_id);
        Vec::new()
    }
}
