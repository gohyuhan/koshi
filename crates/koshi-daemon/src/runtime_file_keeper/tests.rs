//! Tests for the runtime file keeper: the access times one pass sets and the
//! modification times it keeps, the folder locks it takes, waits for and gives
//! up, and the locks a started keeper holds until it is dropped.

use std::os::unix::net::UnixListener;
use std::time::Instant;

use super::*;

/// The access time the pass tests set: 2 000 000 000 seconds after
/// `UNIX_EPOCH`, a whole second in May 2033.
const TEST_ACCESS_TIME_SECONDS: u64 = 2_000_000_000;

/// The modification time the pass tests give the endpoint file before the
/// pass: 1 000 000 000 seconds after `UNIX_EPOCH`.
const TEST_MODIFICATION_TIME_SECONDS: u64 = 1_000_000_000;

/// Whether an exclusive `flock` on a fresh open of the folder at
/// `directory_path` would wait: another open file holds a lock on it.
fn is_exclusive_lock_blocked(directory_path: &Path) -> bool {
    let directory = File::open(directory_path).expect("open the folder");
    match directory.try_lock() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(lock_error)) => panic!("lock the folder: {lock_error}"),
    }
}

/// How long a test waits for a folder lock that was let go to come free: 5 s.
/// A child process another test starts holds a copy of each open descriptor
/// from its fork to its exec, and a lock on that copy stays held until then.
const LOCK_RELEASE_WAIT_DURATION: Duration = Duration::from_secs(5);

/// Whether a fresh open of the folder at `directory_path` takes an exclusive
/// `flock` within [`LOCK_RELEASE_WAIT_DURATION`], trying every 5 ms.
fn wait_for_exclusive_lock(directory_path: &Path) -> bool {
    let deadline = Instant::now() + LOCK_RELEASE_WAIT_DURATION;
    while is_exclusive_lock_blocked(directory_path) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

/// The access time of the path at `timed_path`, without following a link.
fn read_access_time(timed_path: &Path) -> SystemTime {
    std::fs::symlink_metadata(timed_path)
        .and_then(|timed_path_metadata| timed_path_metadata.accessed())
        .expect("read the access time")
}

/// The modification time of the path at `timed_path`, without following a
/// link.
fn read_modification_time(timed_path: &Path) -> SystemTime {
    std::fs::symlink_metadata(timed_path)
        .and_then(|timed_path_metadata| timed_path_metadata.modified())
        .expect("read the modification time")
}

#[test]
fn each_folder_holding_a_kept_path_is_listed_once_in_order() {
    let kept_runtime_files = KeptRuntimeFiles::from_kept_paths(vec![
        PathBuf::from("/tmp/koshi-1000/router.json"),
        PathBuf::from("/tmp/koshi/1000/session-1.sock"),
        PathBuf::from("/tmp/koshi-1000/router.lock"),
    ]);

    assert_eq!(
        kept_runtime_files.unlocked_directory_paths,
        [
            PathBuf::from("/tmp/koshi-1000"),
            PathBuf::from("/tmp/koshi/1000")
        ]
    );
    assert!(kept_runtime_files.locked_directories.is_empty());
}

#[test]
fn a_pass_sets_each_kept_access_time_and_keeps_every_modification_time() {
    let runtime_directory = tempfile::tempdir().expect("runtime folder");
    let endpoint_file_path = runtime_directory.path().join("session-1.json");
    std::fs::write(&endpoint_file_path, b"{}").expect("write the endpoint file");
    File::options()
        .write(true)
        .open(&endpoint_file_path)
        .and_then(|endpoint_file| {
            endpoint_file
                .set_modified(UNIX_EPOCH + Duration::from_secs(TEST_MODIFICATION_TIME_SECONDS))
        })
        .expect("set the endpoint file's modification time");
    let socket_path = runtime_directory.path().join("session-1.sock");
    let _socket_listener = UnixListener::bind(&socket_path).expect("bind the socket");
    let clipboard_tool_directory_path = runtime_directory.path().join("session-1.clipboard-tools");
    std::fs::create_dir(&clipboard_tool_directory_path).expect("create the tool folder");
    let tool_link_path = clipboard_tool_directory_path.join("pbcopy");
    std::os::unix::fs::symlink("/nonexistent/koshi", &tool_link_path).expect("create the link");
    let resume_file_path = runtime_directory.path().join("session-1.resume");
    std::fs::write(&resume_file_path, b"resume").expect("write the resume file");
    let missing_program_file_path = runtime_directory.path().join("session-1.program");
    let socket_modification_time = read_modification_time(&socket_path);
    let clipboard_tool_directory_modification_time =
        read_modification_time(&clipboard_tool_directory_path);
    let tool_link_modification_time = read_modification_time(&tool_link_path);
    let resume_file_access_time = read_access_time(&resume_file_path);

    let access_time = UNIX_EPOCH + Duration::from_secs(TEST_ACCESS_TIME_SECONDS);
    KeptRuntimeFiles::from_kept_paths(vec![
        endpoint_file_path.clone(),
        missing_program_file_path.clone(),
        socket_path.clone(),
        clipboard_tool_directory_path.clone(),
    ])
    .run_keeper_pass(access_time);

    assert_eq!(read_access_time(&endpoint_file_path), access_time);
    assert_eq!(read_access_time(&socket_path), access_time);
    assert_eq!(
        read_access_time(&clipboard_tool_directory_path),
        access_time
    );
    assert_eq!(read_access_time(&tool_link_path), access_time);
    assert_eq!(
        read_modification_time(&endpoint_file_path),
        UNIX_EPOCH + Duration::from_secs(TEST_MODIFICATION_TIME_SECONDS)
    );
    assert_eq!(
        read_modification_time(&socket_path),
        socket_modification_time
    );
    assert_eq!(
        read_modification_time(&clipboard_tool_directory_path),
        clipboard_tool_directory_modification_time
    );
    assert_eq!(
        read_modification_time(&tool_link_path),
        tool_link_modification_time
    );
    assert_eq!(read_access_time(&resume_file_path), resume_file_access_time);
    assert_eq!(
        std::fs::symlink_metadata(&missing_program_file_path)
            .map_err(|metadata_error| metadata_error.kind())
            .err(),
        Some(io::ErrorKind::NotFound)
    );
}

#[test]
fn a_pass_leaves_a_folder_locked_exclusively_elsewhere_for_the_next_pass() {
    let runtime_directory = tempfile::tempdir().expect("runtime folder");
    let endpoint_file_path = runtime_directory.path().join("router.json");
    std::fs::write(&endpoint_file_path, b"{}").expect("write the endpoint file");
    let exclusive_holder = File::open(runtime_directory.path()).expect("open the folder");
    exclusive_holder.try_lock().expect("lock the folder");
    let mut kept_runtime_files = KeptRuntimeFiles::from_kept_paths(vec![endpoint_file_path]);
    let access_time = UNIX_EPOCH + Duration::from_secs(TEST_ACCESS_TIME_SECONDS);

    kept_runtime_files.run_keeper_pass(access_time);

    assert_eq!(
        kept_runtime_files.unlocked_directory_paths,
        [runtime_directory.path().to_path_buf()]
    );
    assert!(kept_runtime_files.locked_directories.is_empty());

    drop(exclusive_holder);
    let deadline = Instant::now() + LOCK_RELEASE_WAIT_DURATION;
    kept_runtime_files.run_keeper_pass(access_time);
    while !kept_runtime_files.unlocked_directory_paths.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        kept_runtime_files.run_keeper_pass(access_time);
    }

    assert_eq!(
        kept_runtime_files.unlocked_directory_paths,
        Vec::<PathBuf>::new()
    );
    assert!(is_exclusive_lock_blocked(runtime_directory.path()));
    let shared_holder = File::open(runtime_directory.path()).expect("open the folder");
    assert_eq!(shared_holder.try_lock_shared().ok(), Some(()));
}

#[test]
fn a_folder_that_cannot_be_opened_leaves_the_lock_list_for_good() {
    let base_directory = tempfile::tempdir().expect("base folder");
    let missing_directory_path = base_directory.path().join("missing-folder");
    let mut kept_runtime_files =
        KeptRuntimeFiles::from_kept_paths(vec![missing_directory_path.join("router.json")]);

    kept_runtime_files.run_keeper_pass(UNIX_EPOCH + Duration::from_secs(TEST_ACCESS_TIME_SECONDS));

    assert_eq!(
        kept_runtime_files.unlocked_directory_paths,
        Vec::<PathBuf>::new()
    );
    assert!(kept_runtime_files.locked_directories.is_empty());
}

#[test]
fn a_started_keeper_holds_its_folder_locks_and_access_times_ahead_until_dropped() {
    let runtime_directory = tempfile::tempdir().expect("runtime folder");
    let shared_user_directory = tempfile::tempdir().expect("shared user folder");
    let endpoint_file_path = runtime_directory.path().join("session-1.json");
    std::fs::write(&endpoint_file_path, b"{}").expect("write the endpoint file");
    let shared_socket_path = shared_user_directory.path().join("session-1.sock");
    let _socket_listener = UnixListener::bind(&shared_socket_path).expect("bind the socket");

    let started_at = SystemTime::now();
    let runtime_file_keeper =
        RuntimeFileKeeper::start(vec![endpoint_file_path.clone(), shared_socket_path.clone()]);
    let start_returned_at = SystemTime::now();

    let compute_whole_seconds = |clock_reading: SystemTime| {
        clock_reading
            .duration_since(UNIX_EPOCH)
            .expect("a time after UNIX_EPOCH")
            .as_secs()
    };
    let earliest_access_seconds = compute_whole_seconds(started_at + ACCESS_TIME_LEAD_DURATION);
    let latest_access_seconds =
        compute_whole_seconds(start_returned_at + ACCESS_TIME_LEAD_DURATION);
    for kept_path in [&endpoint_file_path, &shared_socket_path] {
        let access_seconds = compute_whole_seconds(read_access_time(kept_path));
        assert!(
            (earliest_access_seconds..=latest_access_seconds).contains(&access_seconds),
            "{} has access second {access_seconds}, not in {earliest_access_seconds}..={latest_access_seconds}",
            kept_path.display()
        );
    }
    assert!(is_exclusive_lock_blocked(runtime_directory.path()));
    assert!(is_exclusive_lock_blocked(shared_user_directory.path()));

    drop(runtime_file_keeper);

    assert!(wait_for_exclusive_lock(runtime_directory.path()));
    assert!(wait_for_exclusive_lock(shared_user_directory.path()));
}
