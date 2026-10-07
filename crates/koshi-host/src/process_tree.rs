//! The processes running on this machine: reading them, finding the processes
//! started under a given process, and ending them.
//!
//! A [`ProcessRecord`] names one process by its id and its start time. Every
//! signal and every kill reads the process again first, and acts only when
//! the id still carries the recorded start time. On Linux and macOS the
//! signal follows that read: a process that ends between the read and the
//! signal leaves its id free for the system to give to a new process, which
//! then gets the signal. On Windows the kill goes through a handle opened by
//! that read, and the id stays with the process while the handle is open.

use std::collections::{HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant, SystemTime};

use crate::program_path::is_backup_program_file_name;

#[cfg(test)]
mod tests;

/// How often [`wait_for_processes_to_end`] reads the processes again.
const PROCESS_END_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// The id of the first process the system starts: `init` or `systemd` on
/// Linux, `launchd` on macOS. It is in no POSIX session that a pane starts. A
/// process whose parent ended names it as its parent, unless a Linux
/// subreaper took that process.
const INIT_PROCESS_ID: u32 = 1;

/// One running process, as the system reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRecord {
    /// The process id.
    pub process_id: u32,
    /// The id of the process that started this one. On Linux and macOS, a
    /// process whose parent ended names the process that adopted it. On
    /// Windows, it keeps naming the process that started it, and the system
    /// can give that id to a new process.
    pub parent_process_id: u32,
    /// When the process started.
    pub started_at: SystemTime,
    /// The file name of the program the process runs, such as `koshi` or
    /// `koshi.exe`. On macOS it holds at most the first 15 bytes of that name.
    /// Empty when the system does not tell this process the name.
    pub executable_name: String,
    /// Whether the process runs as the user this process runs as: the same
    /// effective user id on Linux and macOS, the same user SID on Windows.
    pub is_owned_by_current_user: bool,
    /// The id of the POSIX session the process is in, on Linux and macOS: the
    /// process id of the process that started the session with `setsid`. A
    /// process keeps it when its parent ends and another process adopts it.
    /// `None` on Windows, and when the system does not tell this process.
    pub posix_session_id: Option<u32>,
}

impl ProcessRecord {
    /// Whether `other_record` names the same process: the same process id and
    /// the same start time. The program name may differ. Example: a shell
    /// that replaced itself with `sleep` through `exec` is the same process.
    #[must_use]
    pub fn is_same_process(&self, other_record: &ProcessRecord) -> bool {
        self.process_id == other_record.process_id && self.started_at == other_record.started_at
    }
}

/// The running process `process_id`. `None` for `0`, for an id no process
/// has, for a process that has ended and waits to be reaped (a zombie), and
/// for a process the system does not let this process read.
#[must_use]
pub fn find_process_record(process_id: u32) -> Option<ProcessRecord> {
    platform::find_process_record(process_id)
}

/// Every running process this process can read, zombies left out.
///
/// # Errors
/// The [`io::Error`] of a process list the system did not give.
pub fn list_process_records() -> io::Result<Vec<ProcessRecord>> {
    platform::list_process_records()
}

/// Whether `executable_name` names koshi: `koshi`; `koshi.exe` in any mix of
/// upper and lower case; or, in any mix of upper and lower case, a backup name
/// that a Windows update gives the running `koshi.exe`, as
/// [`is_backup_program_file_name`] reads it with the stem `koshi`:
/// `koshi.old`, or `koshi.<n>.old`. Example: `KOSHI.EXE` and `koshi.2.old`
/// give `true`, and `koshi-dev` and `KOSHI` give `false`.
#[must_use]
pub fn is_koshi_executable_name(executable_name: &str) -> bool {
    executable_name == "koshi"
        || executable_name.eq_ignore_ascii_case("koshi.exe")
        || is_backup_program_file_name(&executable_name.to_ascii_lowercase(), "koshi")
}

/// The processes in `process_records` that run under `root_records`: their
/// children, the children of those, and so on down. On Linux and macOS, the
/// processes still in a POSIX session that one of those processes leads are
/// added, with every process under them.
///
/// A process is a child of a parent when all of these hold:
///
/// 1. It names the parent's id as its parent id.
/// 2. It started at or after the parent.
/// 3. It runs as the current user.
/// 4. It is not this process.
/// 5. Its program is not koshi, as [`is_koshi_executable_name`] reads it.
///
/// A process that fails one of them is left out, and so is every process
/// under it. The roots themselves are left out.
///
/// A process in the result leads a POSIX session when the session's id is its
/// own process id. A process in that session is added when all of these hold:
///
/// 1. It holds rules 3, 4 and 5 above.
/// 2. It started at or after the leader.
/// 3. No process above it fails rule 3, 4 or 5, up to where the walk up ends.
///    The walk goes from each process to its parent, whatever POSIX session
///    the parent is in. It ends at process 1, at a root, or at a process
///    above a root: the root's parent, the parent of that, and so on up. The
///    user that process runs as does not matter. A parent that
///    `process_records` does not hold, or that started after the process
///    below it, leaves the process out.
///
/// Example: session process 5000 runs `zsh` 5001. `zsh` runs `sleep` 5002 and
/// `koshi attach` 5003, and 5003 started a router 5004. The result holds 5001
/// and 5002.
///
/// Example: `zsh` 5001 leads POSIX session 5001. A subshell of `zsh` started
/// `sleep` 5006 in the background and ended, and process 1 adopted 5006. 5006
/// is still in session 5001, so the result holds it too.
///
/// Example: process 900 of another user runs above the root 5000, and 900
/// adopted `sleep` 5007 of session 5001. The result holds 5007.
///
/// Example: in session 5001, a `root` process 5004 started `vim` 5005 as the
/// current user. On macOS this process cannot read 5004, and
/// `process_records` does not hold it. On Linux 5004 fails rule 3, also after
/// it moves to a POSIX session of its own. The result does not hold 5005.
#[must_use]
pub fn list_member_records(
    root_records: &[ProcessRecord],
    process_records: &[ProcessRecord],
) -> Vec<ProcessRecord> {
    let this_process_id = std::process::id();
    let mut child_records_by_parent_process_id: HashMap<u32, Vec<&ProcessRecord>> = HashMap::new();
    let mut session_records_by_posix_session_id: HashMap<u32, Vec<&ProcessRecord>> = HashMap::new();
    for process_record in process_records {
        if !can_be_member(process_record, this_process_id) {
            continue;
        }
        child_records_by_parent_process_id
            .entry(process_record.parent_process_id)
            .or_default()
            .push(process_record);
        if let Some(posix_session_id) = process_record.posix_session_id {
            session_records_by_posix_session_id
                .entry(posix_session_id)
                .or_default()
                .push(process_record);
        }
    }
    let record_by_process_id: HashMap<u32, &ProcessRecord> = process_records
        .iter()
        .map(|process_record| (process_record.process_id, process_record))
        .collect();
    let root_and_ancestor_process_ids =
        list_root_and_ancestor_process_ids(root_records, &record_by_process_id);

    let mut member_process_ids: HashSet<u32> = root_records
        .iter()
        .map(|root_record| root_record.process_id)
        .collect();
    let mut member_records = Vec::new();
    let mut parent_records: Vec<&ProcessRecord> = root_records.iter().collect();
    let mut session_leader_records: Vec<&ProcessRecord> = Vec::new();
    loop {
        while let Some(parent_record) = parent_records.pop() {
            let Some(child_records) =
                child_records_by_parent_process_id.remove(&parent_record.process_id)
            else {
                continue;
            };
            for child_record in child_records {
                if child_record.started_at < parent_record.started_at
                    || !member_process_ids.insert(child_record.process_id)
                {
                    continue;
                }
                member_records.push(child_record.clone());
                parent_records.push(child_record);
                if child_record.posix_session_id == Some(child_record.process_id) {
                    session_leader_records.push(child_record);
                }
            }
        }
        let Some(session_leader_record) = session_leader_records.pop() else {
            break;
        };
        let Some(session_records) =
            session_records_by_posix_session_id.remove(&session_leader_record.process_id)
        else {
            continue;
        };
        for session_record in session_records {
            let is_session_member = session_record.started_at >= session_leader_record.started_at
                && !is_under_left_out_session_process(
                    session_record,
                    &root_and_ancestor_process_ids,
                    &record_by_process_id,
                    this_process_id,
                )
                && member_process_ids.insert(session_record.process_id);
            if is_session_member {
                member_records.push(session_record.clone());
                parent_records.push(session_record);
            }
        }
    }
    member_records
}

/// Whether `process_record` holds rules 3, 4 and 5 of
/// [`list_member_records`]: it runs as the current user, its id is not
/// `this_process_id`, and its program is not koshi.
fn can_be_member(process_record: &ProcessRecord, this_process_id: u32) -> bool {
    process_record.is_owned_by_current_user
        && process_record.process_id != this_process_id
        && !is_koshi_executable_name(&process_record.executable_name)
}

/// The ids of `root_records` and of every process above them: each root's
/// parent, the parent of that, and so on up, as `record_by_process_id` names
/// them. The walk up from a root ends at a parent that `record_by_process_id`
/// does not hold, at a parent that started after the process below it, and at
/// a process already in the result. Example: root 5000 names parent 4600, and
/// 4600 names parent 1, which `record_by_process_id` does not hold. The result
/// holds 5000 and 4600.
fn list_root_and_ancestor_process_ids(
    root_records: &[ProcessRecord],
    record_by_process_id: &HashMap<u32, &ProcessRecord>,
) -> HashSet<u32> {
    let mut root_and_ancestor_process_ids = HashSet::new();
    for root_record in root_records {
        let mut current_record = root_record;
        while root_and_ancestor_process_ids.insert(current_record.process_id) {
            let Some(parent_record) = record_by_process_id.get(&current_record.parent_process_id)
            else {
                break;
            };
            if parent_record.started_at > current_record.started_at {
                break;
            }
            current_record = parent_record;
        }
    }
    root_and_ancestor_process_ids
}

/// Whether a process above `session_record` fails [`can_be_member`], or
/// cannot be confirmed, before the walk up ends. The walk goes from each
/// process to its parent, as `record_by_process_id` names it, in any POSIX
/// session, and stops at the first of these:
///
/// - A parent id of [`INIT_PROCESS_ID`]: `false`.
/// - A parent id that `record_by_process_id` does not hold: `true`.
/// - A parent that started after the process below it: `true`.
/// - A parent in `root_and_ancestor_process_ids`, whatever user it runs as:
///   `false`.
/// - A parent that fails [`can_be_member`]: `true`.
///
/// A walk longer than `record_by_process_id` holds processes gives `true`.
fn is_under_left_out_session_process(
    session_record: &ProcessRecord,
    root_and_ancestor_process_ids: &HashSet<u32>,
    record_by_process_id: &HashMap<u32, &ProcessRecord>,
    this_process_id: u32,
) -> bool {
    let mut current_record = session_record;
    for _ in 0..record_by_process_id.len() {
        if current_record.parent_process_id == INIT_PROCESS_ID {
            return false;
        }
        let Some(parent_record) = record_by_process_id.get(&current_record.parent_process_id)
        else {
            return true;
        };
        if parent_record.started_at > current_record.started_at {
            return true;
        }
        if root_and_ancestor_process_ids.contains(&parent_record.process_id) {
            return false;
        }
        if !can_be_member(parent_record, this_process_id) {
            return true;
        }
        current_record = parent_record;
    }
    true
}

/// Whether the process `process_record` names still runs: its id names a
/// running process with the same start time. A zombie gives `false`. On
/// Windows the check opens the process by its id, and reads no list of every
/// process.
#[must_use]
pub fn is_process_running(process_record: &ProcessRecord) -> bool {
    #[cfg(windows)]
    {
        platform::is_process_running(process_record)
    }
    #[cfg(not(windows))]
    {
        find_process_record(process_record.process_id)
            .is_some_and(|current_record| current_record.is_same_process(process_record))
    }
}

/// Wait until no process in `process_records` runs, as
/// [`is_process_running`] reads it, reading them every 50 ms. `true` once
/// none runs, and `false` when `wait_duration` passes first.
#[must_use]
pub fn wait_for_processes_to_end(
    process_records: &[ProcessRecord],
    wait_duration: Duration,
) -> bool {
    let wait_end = Instant::now() + wait_duration;
    loop {
        if !process_records.iter().any(is_process_running) {
            return true;
        }
        let now = Instant::now();
        if now >= wait_end {
            return false;
        }
        std::thread::sleep(PROCESS_END_POLL_INTERVAL_DURATION.min(wait_end - now));
    }
}

/// End every process in `process_records` that still runs, and give back the
/// records of the processes that ran when this was called.
///
/// On Linux and macOS each running process gets `SIGHUP`, `SIGTERM` and
/// `SIGCONT`, in that order. Once none of them runs, or `grace_duration` has
/// passed, each one still running gets `SIGKILL`.
///
/// On Windows a handle is opened to every running process first. Then each
/// one is ended by `TerminateProcess` with exit code `1`, in the order of
/// `process_records`. A process that ends while an earlier one is ended, such
/// as a member of a job that closes with it, is still given back.
/// `grace_duration` is not used.
///
/// A record whose process id now carries another start time is skipped, and
/// is not given back.
pub fn stop_processes(
    process_records: &[ProcessRecord],
    grace_duration: Duration,
) -> Vec<ProcessRecord> {
    platform::stop_processes(process_records, grace_duration)
}

/// Make this process ignore the requests a terminal sends to end it.
///
/// On Linux and macOS: `SIGHUP`, `SIGINT` and `SIGQUIT` are ignored. On
/// Windows: Ctrl+C is ignored.
pub fn ignore_terminal_signals() {
    platform::ignore_terminal_signals();
}

/// The names of every named pipe on this machine, without the `\\.\pipe\`
/// prefix. Empty when the system does not list them.
#[cfg(windows)]
#[must_use]
pub fn list_pipe_names() -> Vec<String> {
    platform::list_pipe_names()
}

/// The id of the process that serves the named pipe `pipe_name`, given without
/// the `\\.\pipe\` prefix. Opening the pipe connects to it, and the
/// connection closes before this returns. `None` when the pipe cannot be
/// opened, for example while every instance of it is busy.
#[cfg(windows)]
#[must_use]
pub fn find_pipe_server_process_id(pipe_name: &str) -> Option<u32> {
    platform::find_pipe_server_process_id(pipe_name)
}

/// The signals sent to processes on Linux and macOS.
#[cfg(unix)]
mod unix_signals {
    use std::time::Duration;

    use super::{is_process_running, wait_for_processes_to_end, ProcessRecord};

    /// Send `signal_number` to the process `process_record` names. `false`
    /// when that process no longer runs, as [`is_process_running`] reads it,
    /// and when `kill` fails.
    fn send_signal(process_record: &ProcessRecord, signal_number: libc::c_int) -> bool {
        let Ok(unix_process_id) = libc::pid_t::try_from(process_record.process_id) else {
            return false;
        };
        if unix_process_id <= 0 || !is_process_running(process_record) {
            return false;
        }
        // SAFETY: `kill` takes a process id and a signal number, and reads no
        // memory of this process.
        unsafe { libc::kill(unix_process_id, signal_number) == 0 }
    }

    pub(super) fn stop_processes(
        process_records: &[ProcessRecord],
        grace_duration: Duration,
    ) -> Vec<ProcessRecord> {
        let running_records: Vec<ProcessRecord> = process_records
            .iter()
            .filter(|process_record| send_signal(process_record, libc::SIGHUP))
            .cloned()
            .collect();
        for running_record in &running_records {
            send_signal(running_record, libc::SIGTERM);
            send_signal(running_record, libc::SIGCONT);
        }
        if !wait_for_processes_to_end(&running_records, grace_duration) {
            for running_record in &running_records {
                send_signal(running_record, libc::SIGKILL);
            }
        }
        running_records
    }

    pub(super) fn ignore_terminal_signals() {
        for signal_number in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            // SAFETY: `SIG_IGN` installs no handler code; the call changes
            // only how this process treats `signal_number`.
            unsafe {
                libc::signal(signal_number, libc::SIG_IGN);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::ProcessRecord;

    pub(super) use super::unix_signals::{ignore_terminal_signals, stop_processes};

    /// Reads `/proc/<process_id>/stat` for the state, the parent id, the POSIX
    /// session id and the start time, the owner of `/proc/<process_id>` for
    /// the user, and the target of `/proc/<process_id>/exe` for the program. A
    /// program file that was replaced or deleted after the process started
    /// keeps its name: the ` (deleted)` the link ends with is dropped.
    pub(super) fn find_process_record(process_id: u32) -> Option<ProcessRecord> {
        if process_id == 0 {
            return None;
        }
        let process_directory = PathBuf::from(format!("/proc/{process_id}"));
        let stat_text = fs::read_to_string(process_directory.join("stat")).ok()?;
        let (_, stat_fields_text) = stat_text.rsplit_once(") ")?;
        let stat_fields: Vec<&str> = stat_fields_text.split_ascii_whitespace().collect();
        if matches!(stat_fields.first(), Some(&("Z" | "X" | "x")) | None) {
            return None;
        }
        let parent_process_id = stat_fields.get(1)?.parse().ok()?;
        let posix_session_id = stat_fields
            .get(3)
            .and_then(|session_id_text| session_id_text.parse().ok());
        let start_tick_count: u64 = stat_fields.get(19)?.parse().ok()?;
        let started_at = compute_started_at(start_tick_count)?;
        let owner_user_id = fs::metadata(&process_directory).ok()?.uid();
        // SAFETY: `geteuid` reads no memory of this process and cannot fail.
        let is_owned_by_current_user = owner_user_id == unsafe { libc::geteuid() };
        let executable_name = fs::read_link(process_directory.join("exe"))
            .ok()
            .and_then(|executable_path| {
                let file_name = executable_path.file_name()?.to_string_lossy().into_owned();
                Some(match file_name.strip_suffix(" (deleted)") {
                    Some(kept_file_name) => kept_file_name.to_string(),
                    None => file_name,
                })
            })
            .unwrap_or_default();
        Some(ProcessRecord {
            process_id,
            parent_process_id,
            started_at,
            executable_name,
            is_owned_by_current_user,
            posix_session_id,
        })
    }

    pub(super) fn list_process_records() -> io::Result<Vec<ProcessRecord>> {
        let mut process_records = Vec::new();
        for process_directory_entry in fs::read_dir("/proc")? {
            let Some(process_id) = process_directory_entry?
                .file_name()
                .to_str()
                .and_then(|directory_name| directory_name.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some(process_record) = find_process_record(process_id) {
                process_records.push(process_record);
            }
        }
        Ok(process_records)
    }

    /// The moment a process started `start_tick_count` clock ticks after the
    /// machine booted. The boot time is the `btime` line of `/proc/stat`, read
    /// once per process. `None` when either cannot be read.
    fn compute_started_at(start_tick_count: u64) -> Option<SystemTime> {
        static BOOTED_AT: OnceLock<Option<SystemTime>> = OnceLock::new();
        let booted_at = (*BOOTED_AT.get_or_init(|| {
            let machine_stat_text = fs::read_to_string("/proc/stat").ok()?;
            let boot_seconds: u64 = machine_stat_text
                .lines()
                .find_map(|stat_line| stat_line.strip_prefix("btime "))?
                .trim()
                .parse()
                .ok()?;
            Some(UNIX_EPOCH + Duration::from_secs(boot_seconds))
        }))?;
        // SAFETY: `sysconf` reads no memory of this process.
        let tick_count_per_second =
            u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
        if tick_count_per_second == 0 {
            return None;
        }
        Some(
            booted_at
                + Duration::from_secs(start_tick_count / tick_count_per_second)
                + Duration::from_nanos(
                    (start_tick_count % tick_count_per_second) * 1_000_000_000
                        / tick_count_per_second,
                ),
        )
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::time::{Duration, UNIX_EPOCH};

    use super::ProcessRecord;

    pub(super) use super::unix_signals::{ignore_terminal_signals, stop_processes};

    /// Reads the `PROC_PIDTBSDINFO` record `proc_pidinfo` gives: the status,
    /// the parent id, the effective user id, the start time and the program
    /// name (`pbi_comm`). The POSIX session id is what `getsid` gives after
    /// that read.
    pub(super) fn find_process_record(process_id: u32) -> Option<ProcessRecord> {
        let process_bsd_record = read_process_bsd_record(process_id)?;
        if process_bsd_record.pbi_status == libc::SZOMB {
            return None;
        }
        let unix_process_id = libc::pid_t::try_from(process_id).ok()?;
        // SAFETY: `getsid` takes a process id and reads no memory of this
        // process.
        let posix_session_id = u32::try_from(unsafe { libc::getsid(unix_process_id) }).ok();
        let executable_name_bytes: Vec<u8> = process_bsd_record
            .pbi_comm
            .iter()
            .take_while(|&&name_byte| name_byte != 0)
            .map(|&name_byte| name_byte as u8)
            .collect();
        Some(ProcessRecord {
            process_id,
            parent_process_id: process_bsd_record.pbi_ppid,
            started_at: UNIX_EPOCH
                + Duration::from_secs(process_bsd_record.pbi_start_tvsec)
                + Duration::from_micros(process_bsd_record.pbi_start_tvusec),
            executable_name: String::from_utf8_lossy(&executable_name_bytes).into_owned(),
            // SAFETY: `geteuid` reads no memory of this process and cannot
            // fail.
            is_owned_by_current_user: process_bsd_record.pbi_uid == unsafe { libc::geteuid() },
            posix_session_id,
        })
    }

    /// Reads every process id `proc_listallpids` gives, with room for 64
    /// processes started between the count and the read.
    pub(super) fn list_process_records() -> io::Result<Vec<ProcessRecord>> {
        // SAFETY: a null buffer of size `0` asks only for the process count.
        let process_count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let process_count =
            usize::try_from(process_count).map_err(|_| io::Error::last_os_error())?;
        let mut process_ids: Vec<libc::c_int> = vec![0; process_count + 64];
        let buffer_byte_count =
            libc::c_int::try_from(process_ids.len() * std::mem::size_of::<libc::c_int>())
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        // SAFETY: `proc_listallpids` writes at most `buffer_byte_count` bytes
        // into `process_ids`, which lives for the call.
        let listed_count =
            unsafe { libc::proc_listallpids(process_ids.as_mut_ptr().cast(), buffer_byte_count) };
        let listed_count = usize::try_from(listed_count).map_err(|_| io::Error::last_os_error())?;
        process_ids.truncate(listed_count);
        Ok(process_ids
            .into_iter()
            .filter_map(|process_id| u32::try_from(process_id).ok())
            .filter_map(find_process_record)
            .collect())
    }

    /// The record `proc_pidinfo` gives for the process `process_id` with
    /// `PROC_PIDTBSDINFO`. `None` for `0`, for an id past the range of
    /// `pid_t`, and for a process `proc_pidinfo` cannot read.
    fn read_process_bsd_record(process_id: u32) -> Option<libc::proc_bsdinfo> {
        let Ok(unix_process_id) = libc::pid_t::try_from(process_id) else {
            return None;
        };
        if unix_process_id <= 0 {
            return None;
        }
        // SAFETY: `proc_bsdinfo` is a plain C struct; all zero bytes is a
        // valid value of it.
        let mut process_bsd_record: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let process_bsd_record_byte_count =
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `proc_pidinfo` writes at most `process_bsd_record_byte_count`
        // bytes into `process_bsd_record`, which lives for the call.
        let written_byte_count = unsafe {
            libc::proc_pidinfo(
                unix_process_id,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::addr_of_mut!(process_bsd_record).cast(),
                process_bsd_record_byte_count,
            )
        };
        if written_byte_count != process_bsd_record_byte_count {
            return None;
        }
        Some(process_bsd_record)
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
mod platform {
    use std::io;

    use super::ProcessRecord;

    pub(super) use super::unix_signals::{ignore_terminal_signals, stop_processes};

    pub(super) fn find_process_record(_process_id: u32) -> Option<ProcessRecord> {
        None
    }

    pub(super) fn list_process_records() -> io::Result<Vec<ProcessRecord>> {
        Ok(Vec::new())
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use windows_sys::Win32::Foundation::{
        FILETIME, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FindClose, FindFirstFileW, FindNextFileW, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, WIN32_FIND_DATAW,
    };
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, TerminateProcess,
        WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE,
    };

    use super::ProcessRecord;

    /// The number of 100 ns intervals from 1601-01-01, where a `FILETIME`
    /// counts from, to 1970-01-01.
    const UNIX_EPOCH_FILE_TIME_INTERVAL_COUNT: u64 = 116_444_736_000_000_000;

    pub(super) fn find_process_record(process_id: u32) -> Option<ProcessRecord> {
        if process_id == 0 {
            return None;
        }
        list_process_entries()
            .ok()?
            .iter()
            .find(|process_entry| process_entry.th32ProcessID == process_id)
            .and_then(build_process_record)
    }

    pub(super) fn list_process_records() -> io::Result<Vec<ProcessRecord>> {
        Ok(list_process_entries()?
            .iter()
            .filter_map(build_process_record)
            .collect())
    }

    pub(super) fn stop_processes(
        process_records: &[ProcessRecord],
        _grace_duration: Duration,
    ) -> Vec<ProcessRecord> {
        let running_processes: Vec<(&ProcessRecord, OwnedHandle)> = process_records
            .iter()
            .filter_map(|process_record| {
                open_running_process(process_record, PROCESS_TERMINATE)
                    .map(|process_handle| (process_record, process_handle))
            })
            .collect();
        for (_, process_handle) in &running_processes {
            // SAFETY: `process_handle` is an open process handle with
            // `PROCESS_TERMINATE`.
            unsafe {
                TerminateProcess(process_handle.as_raw_handle(), 1);
            }
        }
        running_processes
            .into_iter()
            .map(|(process_record, _)| process_record.clone())
            .collect()
    }

    pub(super) fn ignore_terminal_signals() {
        // SAFETY: a null handler with `TRUE` makes this process ignore
        // Ctrl+C; no code of this process runs for it.
        unsafe {
            SetConsoleCtrlHandler(None, 1);
        }
    }

    pub(super) fn list_pipe_names() -> Vec<String> {
        let pipe_pattern = encode_wide_text(r"\\.\pipe\*");
        // SAFETY: `WIN32_FIND_DATAW` is a plain C struct; all zero bytes is
        // a valid value of it.
        let mut pipe_find_data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        // SAFETY: `pipe_pattern` ends with a 0 and lives for the call;
        // `pipe_find_data` lives for the call.
        let pipe_search_handle =
            unsafe { FindFirstFileW(pipe_pattern.as_ptr(), &mut pipe_find_data) };
        if pipe_search_handle == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut pipe_names = Vec::new();
        loop {
            pipe_names.push(decode_wide_text(&pipe_find_data.cFileName));
            // SAFETY: `pipe_search_handle` is the open search, and
            // `pipe_find_data` lives for the call.
            if unsafe { FindNextFileW(pipe_search_handle, &mut pipe_find_data) } == 0 {
                break;
            }
        }
        // SAFETY: `pipe_search_handle` is the open search, closed once.
        unsafe {
            FindClose(pipe_search_handle);
        }
        pipe_names
    }

    pub(super) fn find_pipe_server_process_id(pipe_name: &str) -> Option<u32> {
        let pipe_path = encode_wide_text(&format!(r"\\.\pipe\{pipe_name}"));
        // SAFETY: `pipe_path` ends with a 0 and lives for the call; the other
        // arguments are plain values and null pointers the call accepts.
        let pipe_handle = unsafe {
            CreateFileW(
                pipe_path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if pipe_handle == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: `pipe_handle` is a handle this call opened and nothing else
        // owns.
        let pipe = unsafe { OwnedHandle::from_raw_handle(pipe_handle) };
        let mut server_process_id: u32 = 0;
        // SAFETY: `pipe` is an open pipe handle, and `server_process_id`
        // lives for the call.
        let has_server_process_id =
            unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut server_process_id) }
                != 0;
        has_server_process_id.then_some(server_process_id)
    }

    /// Every entry a `TH32CS_SNAPPROCESS` snapshot holds.
    fn list_process_entries() -> io::Result<Vec<PROCESSENTRY32W>> {
        // SAFETY: the call takes plain values.
        let snapshot_handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot_handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `snapshot_handle` is a handle this call opened and nothing
        // else owns.
        let process_snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot_handle) };
        // SAFETY: `PROCESSENTRY32W` is a plain C struct; all zero bytes is a
        // valid value of it.
        let mut process_entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        process_entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut process_entries = Vec::new();
        // SAFETY: `process_snapshot` is an open snapshot, and `process_entry` lives
        // for the call with `dwSize` set.
        let mut has_process_entry =
            unsafe { Process32FirstW(process_snapshot.as_raw_handle(), &mut process_entry) } != 0;
        while has_process_entry {
            process_entries.push(process_entry);
            // SAFETY: as for `Process32FirstW`.
            has_process_entry =
                unsafe { Process32NextW(process_snapshot.as_raw_handle(), &mut process_entry) }
                    != 0;
        }
        Ok(process_entries)
    }

    /// The record for one snapshot entry. `None` when the process cannot be
    /// opened, has ended, or has no readable start time.
    fn build_process_record(process_entry: &PROCESSENTRY32W) -> Option<ProcessRecord> {
        let process_handle = open_process(
            process_entry.th32ProcessID,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
        )?;
        let started_at = read_running_process_start(&process_handle)?;
        let is_owned_by_current_user = match (
            read_process_user_sid(process_handle.as_raw_handle()),
            get_current_user_sid(),
        ) {
            (Some(process_user_sid), Some(current_user_sid)) => {
                process_user_sid == *current_user_sid
            }
            _ => false,
        };
        Some(ProcessRecord {
            process_id: process_entry.th32ProcessID,
            parent_process_id: process_entry.th32ParentProcessID,
            started_at,
            executable_name: decode_wide_text(&process_entry.szExeFile),
            is_owned_by_current_user,
            posix_session_id: None,
        })
    }

    /// Whether the process `process_record` names still runs, as
    /// [`open_running_process`] reads it with no added access right.
    pub(super) fn is_process_running(process_record: &ProcessRecord) -> bool {
        open_running_process(process_record, 0).is_some()
    }

    /// A handle to the process `process_record` names, with
    /// `PROCESS_QUERY_LIMITED_INFORMATION`, `PROCESS_SYNCHRONIZE` and
    /// `added_access_rights`, such as `PROCESS_TERMINATE`. `None` when no
    /// process runs under that id with that start time, or the system refuses
    /// one of those rights. While the handle is open, the id stays with that
    /// process.
    fn open_running_process(
        process_record: &ProcessRecord,
        added_access_rights: u32,
    ) -> Option<OwnedHandle> {
        let process_handle = open_process(
            process_record.process_id,
            added_access_rights | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
        )?;
        (read_running_process_start(&process_handle) == Some(process_record.started_at))
            .then_some(process_handle)
    }

    fn open_process(process_id: u32, access_rights: u32) -> Option<OwnedHandle> {
        // SAFETY: the call takes plain values.
        let process_handle = unsafe { OpenProcess(access_rights, 0, process_id) };
        if process_handle.is_null() {
            return None;
        }
        // SAFETY: `process_handle` is a handle this call opened and nothing
        // else owns.
        Some(unsafe { OwnedHandle::from_raw_handle(process_handle) })
    }

    /// The creation time of the process behind `process_handle`. `None` once
    /// the process has ended.
    fn read_running_process_start(process_handle: &OwnedHandle) -> Option<SystemTime> {
        // SAFETY: `process_handle` is an open process handle with
        // `PROCESS_SYNCHRONIZE`; a 0 ms wait only reads its state.
        if unsafe { WaitForSingleObject(process_handle.as_raw_handle(), 0) } != WAIT_TIMEOUT {
            return None;
        }
        let empty_file_time = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created_at, mut exited_at, mut kernel_time, mut user_time) = (
            empty_file_time,
            empty_file_time,
            empty_file_time,
            empty_file_time,
        );
        // SAFETY: `process_handle` is an open process handle with
        // `PROCESS_QUERY_LIMITED_INFORMATION`, and the four times live for the
        // call.
        let has_times = unsafe {
            GetProcessTimes(
                process_handle.as_raw_handle(),
                &mut created_at,
                &mut exited_at,
                &mut kernel_time,
                &mut user_time,
            )
        } != 0;
        if !has_times {
            return None;
        }
        let interval_count =
            (u64::from(created_at.dwHighDateTime) << 32) | u64::from(created_at.dwLowDateTime);
        let unix_interval_count =
            interval_count.checked_sub(UNIX_EPOCH_FILE_TIME_INTERVAL_COUNT)?;
        Some(
            UNIX_EPOCH
                + Duration::from_secs(unix_interval_count / 10_000_000)
                + Duration::from_nanos((unix_interval_count % 10_000_000) * 100),
        )
    }

    /// The SID bytes of the user `process_handle`'s token runs as. `None`
    /// when the token cannot be read.
    fn read_process_user_sid(process_handle: HANDLE) -> Option<Vec<u8>> {
        let mut token_handle: HANDLE = std::ptr::null_mut();
        // SAFETY: `process_handle` is an open process handle, and
        // `token_handle` lives for the call.
        if unsafe { OpenProcessToken(process_handle, TOKEN_QUERY, &mut token_handle) } == 0 {
            return None;
        }
        // SAFETY: `token_handle` is a handle this call opened and nothing else
        // owns.
        let process_token = unsafe { OwnedHandle::from_raw_handle(token_handle) };
        let mut token_user_byte_count: u32 = 0;
        // SAFETY: a null buffer of size `0` asks only for the size.
        unsafe {
            GetTokenInformation(
                process_token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut token_user_byte_count,
            );
        }
        if token_user_byte_count == 0 {
            return None;
        }
        let mut token_user_buffer: Vec<u64> =
            vec![0; (token_user_byte_count as usize).div_ceil(std::mem::size_of::<u64>())];
        // SAFETY: `token_user_buffer` holds at least `token_user_byte_count`
        // bytes, aligned for `TOKEN_USER`, and lives for the call.
        let has_token_user = unsafe {
            GetTokenInformation(
                process_token.as_raw_handle(),
                TokenUser,
                token_user_buffer.as_mut_ptr().cast(),
                token_user_byte_count,
                &mut token_user_byte_count,
            )
        } != 0;
        if !has_token_user {
            return None;
        }
        // SAFETY: `GetTokenInformation` wrote a `TOKEN_USER` at the start of
        // `token_user_buffer`, whose SID points inside the same buffer.
        let user_sid = unsafe { (*token_user_buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        // SAFETY: `user_sid` is a valid SID inside `token_user_buffer`.
        let user_sid_byte_count = unsafe { GetLengthSid(user_sid) } as usize;
        // SAFETY: the SID spans `user_sid_byte_count` bytes inside
        // `token_user_buffer`, which outlives this slice.
        let user_sid_bytes =
            unsafe { std::slice::from_raw_parts(user_sid.cast::<u8>(), user_sid_byte_count) };
        Some(user_sid_bytes.to_vec())
    }

    /// The SID bytes of the user this process runs as, read once per process.
    fn get_current_user_sid() -> Option<&'static Vec<u8>> {
        static CURRENT_USER_SID: OnceLock<Option<Vec<u8>>> = OnceLock::new();
        // SAFETY: `GetCurrentProcess` gives a handle to this process that
        // needs no closing.
        CURRENT_USER_SID
            .get_or_init(|| read_process_user_sid(unsafe { GetCurrentProcess() }))
            .as_ref()
    }

    /// `text` as UTF-16 code units ending with a 0.
    fn encode_wide_text(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The UTF-16 code units of `wide_text` up to its first 0, as text.
    fn decode_wide_text(wide_text: &[u16]) -> String {
        let text_length = wide_text
            .iter()
            .position(|&code_unit| code_unit == 0)
            .unwrap_or(wide_text.len());
        String::from_utf16_lossy(&wide_text[..text_length])
    }
}
