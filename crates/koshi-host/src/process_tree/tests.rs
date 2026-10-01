//! Tests for reading, walking and ending processes.

use std::process::{Child, Command, Stdio};
use std::time::UNIX_EPOCH;

use super::*;

/// A record of a process that need not exist, started `started_at_seconds`
/// after `UNIX_EPOCH`.
fn build_invented_record(
    process_id: u32,
    parent_process_id: u32,
    started_at_seconds: u64,
    executable_name: &str,
    is_owned_by_current_user: bool,
) -> ProcessRecord {
    ProcessRecord {
        process_id,
        parent_process_id,
        started_at: UNIX_EPOCH + Duration::from_secs(started_at_seconds),
        executable_name: executable_name.to_string(),
        is_owned_by_current_user,
    }
}

/// A child process that runs for 600 s unless it is ended.
fn spawn_long_running_child() -> Child {
    #[cfg(unix)]
    let mut child_command = Command::new("sleep");
    #[cfg(unix)]
    child_command.arg("600");
    #[cfg(windows)]
    let mut child_command = Command::new("ping");
    #[cfg(windows)]
    child_command.args(["-n", "600", "127.0.0.1"]);
    child_command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the child process starts")
}

/// A child process that is killed and reaped when this value is dropped.
struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The record of `process_id`, read again until it appears or 5 s pass.
fn wait_for_process_record(process_id: u32) -> ProcessRecord {
    let wait_end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(process_record) = find_process_record(process_id) {
            return process_record;
        }
        assert!(
            Instant::now() < wait_end,
            "process {process_id} never became readable"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn is_koshi_executable_name_accepts_koshi_and_koshi_exe_in_any_case() {
    let name_checks = [
        ("koshi", true),
        ("koshi.exe", true),
        ("KOSHI.EXE", true),
        ("Koshi.Exe", true),
        ("koshi-dev", false),
        ("koshi.ex", false),
        ("KOSHI", false),
        ("zsh", false),
        ("", false),
    ];
    for (executable_name, is_expected_koshi) in name_checks {
        assert_eq!(
            is_koshi_executable_name(executable_name),
            is_expected_koshi,
            "{executable_name:?}"
        );
    }
}

#[test]
fn list_member_records_walks_every_level_under_the_root() {
    let root_record = build_invented_record(5000, 1, 100, "koshi", true);
    let shell_record = build_invented_record(5001, 5000, 101, "zsh", true);
    let job_record = build_invented_record(5002, 5001, 102, "make", true);
    let compiler_record = build_invented_record(5003, 5002, 103, "cc", true);
    let sibling_record = build_invented_record(5004, 1, 104, "zsh", true);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        job_record.clone(),
        compiler_record.clone(),
        sibling_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record, job_record, compiler_record]
    );
}

#[test]
fn list_member_records_leaves_out_koshi_and_everything_under_it() {
    let root_record = build_invented_record(5000, 1, 100, "koshi", true);
    let shell_record = build_invented_record(5001, 5000, 101, "zsh", true);
    let attach_record = build_invented_record(5002, 5001, 102, "koshi", true);
    let router_child_record = build_invented_record(5003, 5002, 103, "zsh", true);
    let windows_attach_record = build_invented_record(5004, 5001, 104, "KOSHI.EXE", true);
    let windows_child_record = build_invented_record(5005, 5004, 105, "cmd.exe", true);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        attach_record,
        router_child_record,
        windows_attach_record,
        windows_child_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_leaves_out_another_users_process_and_everything_under_it() {
    let root_record = build_invented_record(5000, 1, 100, "koshi", true);
    let shell_record = build_invented_record(5001, 5000, 101, "zsh", true);
    let sudo_record = build_invented_record(5002, 5001, 102, "sudo", false);
    let sudo_child_record = build_invented_record(5003, 5002, 103, "vim", true);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        sudo_record,
        sudo_child_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_leaves_out_a_process_that_started_before_its_named_parent() {
    let root_record = build_invented_record(5000, 1, 100, "koshi", true);
    let earlier_record = build_invented_record(5001, 5000, 99, "zsh", true);
    let earlier_child_record = build_invented_record(5002, 5001, 101, "sleep", true);
    let same_second_record = build_invented_record(5003, 5000, 100, "zsh", true);
    let process_records = vec![
        root_record.clone(),
        earlier_record,
        earlier_child_record,
        same_second_record.clone(),
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![same_second_record]
    );
}

#[test]
fn list_member_records_leaves_out_this_process_and_everything_under_it() {
    let this_process_id = std::process::id();
    let root_record = build_invented_record(5000, 1, 100, "koshi", true);
    let shell_record = build_invented_record(5001, 5000, 101, "zsh", true);
    let this_process_record = build_invented_record(this_process_id, 5001, 102, "tests", true);
    let this_process_child_record =
        build_invented_record(5003, this_process_id, 103, "sleep", true);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        this_process_record,
        this_process_child_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_walks_under_every_root_and_leaves_the_roots_out() {
    let session_record = build_invented_record(5000, 1, 100, "koshi", true);
    let supervisor_record = build_invented_record(5001, 5000, 101, "koshi.exe", true);
    let session_child_record = build_invented_record(5002, 5000, 102, "cmd.exe", true);
    let pane_record = build_invented_record(5003, 5001, 103, "pwsh.exe", true);
    let process_records = vec![
        session_record.clone(),
        supervisor_record.clone(),
        session_child_record.clone(),
        pane_record.clone(),
    ];

    let mut member_process_ids: Vec<u32> =
        list_member_records(&[session_record, supervisor_record], &process_records)
            .iter()
            .map(|member_record| member_record.process_id)
            .collect();
    member_process_ids.sort_unstable();
    assert_eq!(member_process_ids, vec![5002, 5003]);
}

#[test]
fn find_process_record_gives_none_for_process_id_zero() {
    assert_eq!(find_process_record(0), None);
}

#[test]
fn find_process_record_reads_this_process() {
    let this_process_record =
        find_process_record(std::process::id()).expect("this process is readable");

    let executable_path = std::env::current_exe().expect("the test binary has a path");
    let executable_file_name = executable_path
        .file_name()
        .expect("the test binary path has a file name")
        .to_string_lossy()
        .into_owned();
    let expected_executable_name = if cfg!(target_os = "macos") {
        executable_file_name.chars().take(15).collect::<String>()
    } else {
        executable_file_name
    };
    assert_eq!(this_process_record.process_id, std::process::id());
    assert_eq!(
        this_process_record.executable_name,
        expected_executable_name
    );
    assert!(this_process_record.is_owned_by_current_user);
    assert!(this_process_record.started_at <= SystemTime::now());
    #[cfg(unix)]
    assert_eq!(
        this_process_record.parent_process_id,
        std::os::unix::process::parent_id()
    );
}

#[test]
fn list_process_records_holds_this_process() {
    let this_process_record =
        find_process_record(std::process::id()).expect("this process is readable");

    let process_records = list_process_records().expect("the process list is readable");

    assert!(process_records.contains(&this_process_record));
}

#[cfg(unix)]
#[test]
fn find_process_record_gives_none_for_a_zombie() {
    let mut child = Command::new("true").spawn().expect("`true` starts");
    let child_process_id = child.id();
    // SAFETY: `siginfo_t` is a plain C struct; all zero bytes is a valid
    // value of it.
    let mut child_exit_record: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `waitid` writes into `child_exit_record`, which lives for the
    // call. `WNOWAIT` leaves the child unreaped.
    let wait_answer = unsafe {
        libc::waitid(
            libc::P_PID,
            child_process_id as libc::id_t,
            &mut child_exit_record,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    assert_eq!(wait_answer, 0);

    assert_eq!(find_process_record(child_process_id), None);

    child.wait().expect("the child is reaped");
}

#[cfg(unix)]
#[test]
fn stop_processes_ends_a_shell_and_both_of_its_background_jobs() {
    let mut shell_child = ChildGuard {
        child: Command::new("sh")
            .args(["-c", "sleep 600 & sleep 600 & wait"])
            .stdin(Stdio::null())
            .spawn()
            .expect("`sh` starts"),
    };
    let shell_record = wait_for_process_record(shell_child.child.id());
    let wait_end = Instant::now() + Duration::from_secs(5);
    let member_records = loop {
        let member_records = list_member_records(
            std::slice::from_ref(&shell_record),
            &list_process_records().expect("the process list is readable"),
        );
        if member_records.len() == 2 || Instant::now() >= wait_end {
            break member_records;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let member_summaries: Vec<(u32, &str)> = member_records
        .iter()
        .map(|member_record| {
            (
                member_record.parent_process_id,
                member_record.executable_name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        member_summaries,
        vec![
            (shell_record.process_id, "sleep"),
            (shell_record.process_id, "sleep")
        ]
    );

    let mut stop_records = member_records.clone();
    stop_records.push(shell_record.clone());
    assert_eq!(
        stop_processes(&stop_records, Duration::from_secs(2)),
        stop_records
    );

    assert!(wait_for_processes_to_end(
        &stop_records,
        Duration::from_secs(5)
    ));
    shell_child.child.wait().expect("the shell is reaped");
}

#[cfg(windows)]
#[test]
fn stop_processes_ends_a_command_shell_and_its_child() {
    let mut shell_child = ChildGuard {
        child: Command::new("cmd")
            .args(["/C", "ping -n 600 127.0.0.1 >NUL"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("`cmd` starts"),
    };
    let shell_record = wait_for_process_record(shell_child.child.id());
    let wait_end = Instant::now() + Duration::from_secs(5);
    let member_records = loop {
        let member_records = list_member_records(
            std::slice::from_ref(&shell_record),
            &list_process_records().expect("the process list is readable"),
        );
        let has_ping = member_records.iter().any(|member_record| {
            member_record
                .executable_name
                .eq_ignore_ascii_case("PING.EXE")
        });
        if has_ping || Instant::now() >= wait_end {
            break member_records;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let ping_records: Vec<&ProcessRecord> = member_records
        .iter()
        .filter(|member_record| {
            member_record
                .executable_name
                .eq_ignore_ascii_case("PING.EXE")
        })
        .collect();
    assert_eq!(ping_records.len(), 1);
    assert_eq!(ping_records[0].parent_process_id, shell_record.process_id);

    let mut stop_records = vec![shell_record];
    stop_records.extend(member_records.iter().cloned());
    assert_eq!(
        stop_processes(&stop_records, Duration::from_secs(2)),
        stop_records
    );

    assert!(wait_for_processes_to_end(
        &stop_records,
        Duration::from_secs(5)
    ));
    shell_child.child.wait().expect("the shell is reaped");
}

#[test]
fn stop_processes_skips_a_record_whose_start_time_differs_from_the_running_process() {
    let mut child_guard = ChildGuard {
        child: spawn_long_running_child(),
    };
    let child_record = wait_for_process_record(child_guard.child.id());
    let mut reused_id_record = child_record.clone();
    reused_id_record.started_at += Duration::from_secs(1);

    assert_eq!(
        stop_processes(&[reused_id_record], Duration::from_secs(2)),
        Vec::new()
    );
    assert!(is_process_running(&child_record));
    assert!(!wait_for_processes_to_end(
        std::slice::from_ref(&child_record),
        Duration::from_millis(100)
    ));

    assert_eq!(
        stop_processes(std::slice::from_ref(&child_record), Duration::from_secs(2)),
        vec![child_record.clone()]
    );
    assert!(wait_for_processes_to_end(
        std::slice::from_ref(&child_record),
        Duration::from_secs(5)
    ));
    child_guard.child.wait().expect("the child is reaped");
}

#[cfg(unix)]
#[test]
fn stop_processes_kills_a_process_that_ignores_hangup_and_terminate_once_the_grace_passes() {
    let stubborn_child = ChildGuard {
        child: Command::new("sh")
            .args(["-c", "trap '' HUP TERM; while :; do sleep 1; done"])
            .stdin(Stdio::null())
            .spawn()
            .expect("`sh` starts"),
    };
    let stubborn_record = wait_for_process_record(stubborn_child.child.id());
    std::thread::sleep(Duration::from_millis(200));

    assert_eq!(
        stop_processes(
            std::slice::from_ref(&stubborn_record),
            Duration::from_millis(300)
        ),
        vec![stubborn_record.clone()]
    );

    assert!(wait_for_processes_to_end(
        std::slice::from_ref(&stubborn_record),
        Duration::from_secs(2)
    ));
}

#[test]
fn is_same_process_compares_the_process_id_and_the_start_time_alone() {
    let shell_record = build_invented_record(5000, 1, 100, "sh", true);
    let replaced_record = build_invented_record(5000, 1, 100, "sleep", true);
    let reused_id_record = build_invented_record(5000, 1, 101, "sh", true);
    let other_id_record = build_invented_record(5001, 1, 100, "sh", true);

    assert!(shell_record.is_same_process(&replaced_record));
    assert!(!shell_record.is_same_process(&reused_id_record));
    assert!(!shell_record.is_same_process(&other_id_record));
}
