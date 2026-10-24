//! A `koshi serve-session` start with logging off deletes the log files of old
//! sessions: it keeps the router's file and the files of the 20 sessions
//! written last, and creates no file of its own.

use std::path::Path;
use std::time::{Duration, SystemTime};

use koshi_core::ids::SessionId;

mod common;

use common::{
    build_short_test_directory, resolve_log_directory_under_home,
    resolve_runtime_directory_under_home, start_session_server_under_home,
};

/// Create the empty file `log_file_name` in `log_directory`, and set its
/// modification time to `written_at`.
fn write_log_file_at(log_directory: &Path, log_file_name: &str, written_at: SystemTime) {
    let log_file =
        std::fs::File::create(log_directory.join(log_file_name)).expect("create the log file");
    log_file
        .set_modified(written_at)
        .expect("set the log file's modification time");
}

/// Every name in `directory`, sorted.
fn list_sorted_file_names(directory: &Path) -> Vec<String> {
    let mut file_names: Vec<String> = std::fs::read_dir(directory)
        .expect("read the directory")
        .map(|directory_entry| {
            directory_entry
                .expect("read a directory entry")
                .file_name()
                .into_string()
                .expect("a UTF-8 name")
        })
        .collect();
    file_names.sort_unstable();
    file_names
}

#[test]
fn a_session_server_start_with_logging_off_deletes_the_logs_of_old_sessions() {
    let home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let log_directory = resolve_log_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&log_directory).expect("a log directory under the test home");
    let first_written_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let old_session_ids: Vec<SessionId> = (0..21).map(|_| SessionId::new()).collect();
    for (session_index, old_session_id) in old_session_ids.iter().enumerate() {
        let session_index = u64::try_from(session_index).expect("the index fits in u64");
        write_log_file_at(
            &log_directory,
            &format!("koshi-log-{}.log", old_session_id.get_uuid()),
            first_written_at + Duration::from_secs(10 * (session_index + 1)),
        );
    }
    write_log_file_at(&log_directory, "koshi-log-router.log", first_written_at);
    let oldest_log_file_name = format!("koshi-log-{}.log", old_session_ids[0].get_uuid());
    let mut expected_file_names = list_sorted_file_names(&log_directory);
    expected_file_names.retain(|file_name| *file_name != oldest_log_file_name);

    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);
    session_process.wait_for_session_endpoint(&runtime_directory, session_id);

    assert_eq!(list_sorted_file_names(&log_directory), expected_file_names);
    assert_eq!(expected_file_names.len(), 21);
}
