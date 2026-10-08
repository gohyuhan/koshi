//! Tests for reading, walking and ending processes, and for telling whether a
//! process id is free.

use std::process::{Child, Command, Stdio};
use std::time::UNIX_EPOCH;

use super::*;

/// A record of a process that need not exist, started `started_at_seconds`
/// after `UNIX_EPOCH`, with no POSIX session id.
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
        posix_session_id: None,
    }
}

/// [`build_invented_record`] in the POSIX session `posix_session_id`.
fn build_invented_session_record(
    process_id: u32,
    parent_process_id: u32,
    started_at_seconds: u64,
    executable_name: &str,
    posix_session_id: u32,
) -> ProcessRecord {
    ProcessRecord {
        posix_session_id: Some(posix_session_id),
        ..build_invented_record(
            process_id,
            parent_process_id,
            started_at_seconds,
            executable_name,
            true,
        )
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
fn is_koshi_executable_name_accepts_koshi_koshi_exe_and_its_backup_names() {
    let name_checks = [
        ("koshi", true),
        ("koshi.exe", true),
        ("KOSHI.EXE", true),
        ("Koshi.Exe", true),
        ("koshi.old", true),
        ("KOSHI.OLD", true),
        ("koshi.2.old", true),
        ("Koshi.12.Old", true),
        ("koshi-dev", false),
        ("koshi.ex", false),
        ("koshi.x.old", false),
        ("koshi..old", false),
        ("other.old", false),
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
fn list_member_records_adds_an_adopted_process_still_in_the_session_a_member_leads() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let adopted_record = build_invented_session_record(5006, 1, 102, "sleep", 5001);
    let adopted_child_record = build_invented_session_record(5007, 5006, 103, "cat", 5007);
    let unrelated_record = build_invented_session_record(5008, 1, 104, "sleep", 4000);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        adopted_record.clone(),
        adopted_child_record.clone(),
        unrelated_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record, adopted_record, adopted_child_record]
    );
}

#[test]
fn list_member_records_leaves_out_a_process_in_the_session_of_a_leader_that_ended() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let adopted_record = build_invented_session_record(5006, 1, 102, "sleep", 5001);
    let process_records = vec![root_record.clone(), adopted_record];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        Vec::new()
    );
}

#[test]
fn list_member_records_leaves_out_a_session_process_under_koshi_or_another_user_in_that_session() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let attach_record = build_invented_session_record(5002, 1, 102, "koshi", 5001);
    let attach_child_record = build_invented_session_record(5003, 5002, 103, "zsh", 5001);
    let sudo_record = ProcessRecord {
        is_owned_by_current_user: false,
        ..build_invented_session_record(5004, 1, 104, "sudo", 5001)
    };
    let sudo_child_record = build_invented_session_record(5005, 5004, 105, "vim", 5001);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        attach_record,
        attach_child_record,
        sudo_record,
        sudo_child_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_leaves_out_a_session_process_that_started_before_its_leader() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 105, "zsh", 5001);
    let earlier_record = build_invented_session_record(5006, 1, 104, "sleep", 5001);
    let process_records = vec![root_record.clone(), shell_record.clone(), earlier_record];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_leaves_out_the_session_of_a_member_that_does_not_lead_it() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let child_record = build_invented_session_record(5001, 5000, 101, "zsh", 4000);
    let session_neighbor_record = build_invented_session_record(5006, 1, 102, "sleep", 4000);
    let process_records = vec![
        root_record.clone(),
        child_record.clone(),
        session_neighbor_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![child_record]
    );
}

#[test]
fn list_member_records_leaves_out_session_processes_whose_parents_name_each_other() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let first_looped_record = build_invented_session_record(5006, 5007, 102, "sleep", 5001);
    let second_looped_record = build_invented_session_record(5007, 5006, 102, "sleep", 5001);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        first_looped_record,
        second_looped_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_leaves_out_a_session_whose_id_matches_a_member_outside_it() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let editor_record = build_invented_session_record(5002, 5001, 103, "vim", 5001);
    let other_session_record = build_invented_session_record(5009, 1, 104, "sleep", 5002);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        editor_record.clone(),
        other_session_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record, editor_record]
    );
}

#[test]
fn list_member_records_adds_a_session_process_adopted_by_any_users_process_above_the_root() {
    let subreaper_record = ProcessRecord {
        is_owned_by_current_user: false,
        ..build_invented_session_record(4500, 1, 50, "tini", 4500)
    };
    let user_manager_record = build_invented_session_record(4600, 4500, 60, "systemd", 4600);
    let root_record = build_invented_session_record(5000, 4600, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let first_adopted_record = build_invented_session_record(5006, 4500, 102, "sleep", 5001);
    let second_adopted_record = build_invented_session_record(5007, 4600, 103, "sleep", 5001);
    let process_records = vec![
        subreaper_record,
        user_manager_record,
        root_record.clone(),
        shell_record.clone(),
        first_adopted_record.clone(),
        second_adopted_record.clone(),
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record, first_adopted_record, second_adopted_record]
    );
}

#[test]
fn list_member_records_adds_a_session_process_adopted_above_a_root_whose_ancestors_name_each_other()
{
    let user_manager_record = build_invented_session_record(4600, 4500, 60, "systemd", 4600);
    let subreaper_record = ProcessRecord {
        is_owned_by_current_user: false,
        ..build_invented_session_record(4500, 4600, 60, "tini", 4500)
    };
    let root_record = build_invented_session_record(5000, 4600, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let adopted_record = build_invented_session_record(5006, 4500, 102, "sleep", 5001);
    let process_records = vec![
        user_manager_record,
        subreaper_record,
        root_record.clone(),
        shell_record.clone(),
        adopted_record.clone(),
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record, adopted_record]
    );
}

#[test]
fn list_member_records_leaves_out_a_session_process_under_the_roots_parent_started_after_the_root()
{
    let reused_id_record = ProcessRecord {
        is_owned_by_current_user: false,
        ..build_invented_session_record(4600, 1, 150, "tini", 4600)
    };
    let root_record = build_invented_session_record(5000, 4600, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let adopted_record = build_invented_session_record(5006, 4600, 160, "sleep", 5001);
    let process_records = vec![
        reused_id_record,
        root_record.clone(),
        shell_record.clone(),
        adopted_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_leaves_out_a_session_process_under_koshi_or_another_user_in_another_session()
{
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let sudo_record = ProcessRecord {
        is_owned_by_current_user: false,
        ..build_invented_session_record(5002, 5001, 102, "sudo", 5002)
    };
    let sudo_child_record = build_invented_session_record(5003, 5002, 103, "vim", 5001);
    let attach_record = build_invented_session_record(5004, 5001, 104, "koshi", 5004);
    let attach_child_record = build_invented_session_record(5005, 5004, 105, "zsh", 5001);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        sudo_record,
        sudo_child_record,
        attach_record,
        attach_child_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_leaves_out_the_child_of_a_parent_in_another_session_under_another_user() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let sudo_record = ProcessRecord {
        is_owned_by_current_user: false,
        ..build_invented_session_record(5002, 5001, 102, "sudo", 5001)
    };
    let helper_record = build_invented_session_record(5003, 5002, 103, "sh", 5003);
    let editor_record = build_invented_session_record(5004, 5003, 104, "vim", 5001);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        sudo_record,
        helper_record,
        editor_record,
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_adds_a_session_process_whose_parent_in_another_session_process_1_adopted() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let helper_record = build_invented_session_record(5003, 1, 103, "sh", 5003);
    let editor_record = build_invented_session_record(5004, 5003, 104, "vim", 5001);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        helper_record,
        editor_record.clone(),
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record, editor_record]
    );
}

#[test]
fn list_member_records_leaves_out_a_session_process_whose_parent_is_not_listed() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let editor_record = build_invented_session_record(5005, 5004, 105, "vim", 5001);
    let process_records = vec![root_record.clone(), shell_record.clone(), editor_record];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record]
    );
}

#[test]
fn list_member_records_leaves_out_a_session_process_whose_named_parent_started_after_it() {
    let root_record = build_invented_session_record(5000, 1, 100, "koshi", 4000);
    let shell_record = build_invented_session_record(5001, 5000, 101, "zsh", 5001);
    let earlier_record = build_invented_session_record(5006, 5009, 102, "sleep", 5001);
    let later_record = build_invented_session_record(5009, 1, 110, "zsh", 5001);
    let process_records = vec![
        root_record.clone(),
        shell_record.clone(),
        earlier_record,
        later_record.clone(),
    ];

    assert_eq!(
        list_member_records(&[root_record], &process_records),
        vec![shell_record, later_record]
    );
}

/// A process this test did not start as its own child, ended with `SIGKILL`
/// when this value is dropped.
#[cfg(unix)]
struct AdoptedProcessGuard {
    process_record: ProcessRecord,
}

#[cfg(unix)]
impl Drop for AdoptedProcessGuard {
    fn drop(&mut self) {
        stop_processes(std::slice::from_ref(&self.process_record), Duration::ZERO);
    }
}

#[cfg(unix)]
#[test]
fn list_member_records_finds_a_process_adopted_while_its_session_leader_runs() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;

    // The leader runs a shell that starts `sleep` in the background, prints
    // the ids of that `sleep` and of itself, and ends. The leader then
    // replaces itself with `sleep 600`.
    let mut leader_command = Command::new("sh");
    leader_command
        .args([
            "-c",
            "sh -c 'sleep 600 >/dev/null 2>&1 & echo $! $$'; exec sleep 600",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: the closure runs in the child between `fork` and `exec`, and
    // calls `setsid` alone, which is async-signal-safe.
    unsafe {
        leader_command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut leader_child = ChildGuard {
        child: leader_command.spawn().expect("`sh` starts"),
    };
    let mut process_ids_line = String::new();
    BufReader::new(
        leader_child
            .child
            .stdout
            .take()
            .expect("the leader's output is piped"),
    )
    .read_line(&mut process_ids_line)
    .expect("the inner shell prints two process ids");
    let process_ids: Vec<u32> = process_ids_line
        .split_whitespace()
        .map(|process_id_text| process_id_text.parse().expect("a process id"))
        .collect();
    let [adopted_process_id, inner_shell_process_id] = process_ids[..] else {
        panic!("expected two process ids, got {process_ids_line:?}");
    };
    let wait_end = Instant::now() + Duration::from_secs(5);
    let adopted_record = loop {
        let adopted_record = wait_for_process_record(adopted_process_id);
        if adopted_record.parent_process_id != inner_shell_process_id {
            break adopted_record;
        }
        assert!(
            Instant::now() < wait_end,
            "the background `sleep` was never adopted"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let _adopted_guard = AdoptedProcessGuard {
        process_record: adopted_record.clone(),
    };
    let leader_record = wait_for_process_record(leader_child.child.id());
    let root_record = wait_for_process_record(std::process::id());

    let member_records = list_member_records(
        std::slice::from_ref(&root_record),
        &list_process_records().expect("the process list is readable"),
    );

    assert_eq!(
        leader_record.posix_session_id,
        Some(leader_record.process_id)
    );
    assert_eq!(
        adopted_record.posix_session_id,
        Some(leader_record.process_id)
    );
    let member_process_ids: Vec<u32> = member_records
        .iter()
        .map(|member_record| member_record.process_id)
        .collect();
    assert!(!member_process_ids.contains(&adopted_record.parent_process_id));
    let session_member_process_ids: Vec<u32> = member_records
        .iter()
        .filter(|member_record| member_record.posix_session_id == leader_record.posix_session_id)
        .map(|member_record| member_record.process_id)
        .collect();
    assert_eq!(
        session_member_process_ids,
        vec![leader_record.process_id, adopted_record.process_id]
    );
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
    // SAFETY: `getsid(0)` reads the session of this process and reads no
    // memory of it.
    #[cfg(unix)]
    assert_eq!(
        this_process_record.posix_session_id,
        u32::try_from(unsafe { libc::getsid(0) }).ok()
    );
    #[cfg(windows)]
    assert_eq!(this_process_record.posix_session_id, None);
}

#[cfg(unix)]
#[test]
fn find_process_record_reads_the_posix_session_of_a_child_that_leads_its_own_process_group() {
    use std::os::unix::process::CommandExt;

    let child_guard = ChildGuard {
        child: Command::new("sleep")
            .arg("600")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("`sleep` starts"),
    };
    let child_record = wait_for_process_record(child_guard.child.id());
    // SAFETY: `getsid(0)` reads the session of this process and reads no
    // memory of it.
    let this_posix_session_id =
        u32::try_from(unsafe { libc::getsid(0) }).expect("this process is in a session");

    assert_ne!(this_posix_session_id, child_record.process_id);
    assert_eq!(child_record.posix_session_id, Some(this_posix_session_id));
}

#[test]
fn list_process_records_holds_this_process() {
    let this_process_record =
        find_process_record(std::process::id()).expect("this process is readable");

    let process_records = list_process_records().expect("the process list is readable");

    assert!(process_records.contains(&this_process_record));
}

/// A process id above every id that Linux, macOS, and Windows give a process:
/// `2147483647`.
const FREE_PROCESS_ID: u32 = 2_147_483_647;

#[test]
fn is_process_id_free_gives_true_for_an_id_that_no_process_has() {
    assert!(is_process_id_free(FREE_PROCESS_ID));
}

#[test]
fn is_process_id_free_gives_false_for_this_process() {
    assert!(!is_process_id_free(std::process::id()));
}

#[test]
fn is_process_id_free_gives_false_for_process_id_zero() {
    assert!(!is_process_id_free(0));
}

#[cfg(unix)]
#[test]
fn is_process_id_free_gives_false_for_the_first_process_the_system_starts() {
    assert!(!is_process_id_free(INIT_PROCESS_ID));
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

    assert!(!is_process_running(&reused_id_record));
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
