//! Tests for the thread that writes the last-used time of each admitted
//! remote grant.

use super::*;

use std::time::{Duration, UNIX_EPOCH};

use koshi_ipc::remote_tokens::{
    hash_connection_token, resolve_token_store_lock_path, resolve_token_store_path, TokenScope,
};
use koshi_link::remote_client::{acquire_store_lock, STORE_LOCK_TIMEOUT_DURATION};
use koshi_test_support::fixtures::build_test_runtime_directory;

/// The clock reading `seconds` after the Unix epoch.
fn build_time_after_epoch(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// Write a token store at `token_store_path` holding one host-wide grant with
/// no expiry for each of `identities`, issued at second `1_000`, and hand back
/// the hash of each grant's secret, in the order of `identities`.
fn write_token_store_with_grants(token_store_path: &Path, identities: &[&str]) -> Vec<String> {
    let mut token_store = TokenStore::new();
    let token_hashes = identities
        .iter()
        .map(|identity| {
            let (connection_token, _) = token_store.grant_token(
                (*identity).to_string(),
                TokenScope::HostWide,
                build_time_after_epoch(1_000),
                None,
            );
            hash_connection_token(&connection_token)
        })
        .collect();
    token_store
        .write_token_store_to_path(token_store_path)
        .expect("the token store is written");
    token_hashes
}

/// The last-used time of each record in the store at `token_store_path`, in
/// record order.
fn list_last_used_times(token_store_path: &Path) -> Vec<Option<SystemTime>> {
    TokenStore::load_token_store_from_path(token_store_path)
        .expect("the store reads back")
        .token_records
        .iter()
        .map(|token_record| token_record.last_used_at)
        .collect()
}

#[test]
fn a_pass_stamps_each_grant_with_the_latest_of_its_uses() {
    // Three uses wait before the pass: alice at 3000 then 2000, bob at 2500.
    // The pass stamps alice with 3000 and bob with 2500.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(&runtime_directory.path().join("data"));
    let token_hashes = write_token_store_with_grants(&token_store_path, &["alice", "bob"]);
    let (token_use_sender, token_use_receiver) = mpsc::channel();
    for (token_hash, used_at_seconds) in [
        (&token_hashes[0], 3_000),
        (&token_hashes[0], 2_000),
        (&token_hashes[1], 2_500),
    ] {
        token_use_sender
            .send(TokenUse {
                token_hash: token_hash.clone(),
                used_at: build_time_after_epoch(used_at_seconds),
            })
            .expect("the receiver is alive");
    }
    drop(token_use_sender);

    run_token_use_writer(&token_store_path, &token_use_receiver);

    assert_eq!(
        list_last_used_times(&token_store_path),
        vec![
            Some(build_time_after_epoch(3_000)),
            Some(build_time_after_epoch(2_500)),
        ]
    );
}

#[test]
fn the_finish_returns_after_every_use_handed_over_before_it_is_written() {
    // A use handed to a running writer is in the store once `finish` returns.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(&runtime_directory.path().join("data"));
    let token_hashes = write_token_store_with_grants(&token_store_path, &["alice"]);
    let token_use_writer =
        TokenUseWriter::start(token_store_path.clone()).expect("the writer thread starts");

    token_use_writer.record_token_use(token_hashes[0].clone(), build_time_after_epoch(2_000));
    token_use_writer.finish();

    assert_eq!(
        list_last_used_times(&token_store_path),
        vec![Some(build_time_after_epoch(2_000))]
    );
}

#[test]
fn a_write_waits_while_another_koshi_holds_the_store_lock() {
    // While the lock beside the store is held, the use is not written. Once
    // the lock is let go, the writer takes it and stamps the use.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(&runtime_directory.path().join("data"));
    let token_hashes = write_token_store_with_grants(&token_store_path, &["alice"]);
    let token_store_bytes_before_use =
        std::fs::read(&token_store_path).expect("the store file reads");
    let held_store_lock_file = acquire_store_lock(
        &resolve_token_store_lock_path(&token_store_path),
        Duration::ZERO,
        "the test holds the lock",
    )
    .expect("the test takes the store lock");
    let token_use_writer =
        TokenUseWriter::start(token_store_path.clone()).expect("the writer thread starts");

    token_use_writer.record_token_use(token_hashes[0].clone(), build_time_after_epoch(2_000));
    std::thread::sleep(Duration::from_millis(200));

    assert_eq!(
        std::fs::read(&token_store_path).expect("the store file reads"),
        token_store_bytes_before_use,
        "nothing is written while the lock is held"
    );
    drop(held_store_lock_file);
    token_use_writer.finish();
    assert_eq!(
        list_last_used_times(&token_store_path),
        vec![Some(build_time_after_epoch(2_000))]
    );
}

#[test]
fn writes_and_grants_that_run_at_the_same_time_keep_every_change() {
    // Eight writes, each stamping its own grant `used-<n>` at second
    // `2_000 + n`, run at the same time as eight grants `granted-<n>` that
    // another koshi makes under the store lock. The store ends with every
    // grant and every stamp.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(&runtime_directory.path().join("data"));
    let used_identities: Vec<String> = (0..8)
        .map(|change_index| format!("used-{change_index}"))
        .collect();
    let used_identity_names: Vec<&str> = used_identities.iter().map(String::as_str).collect();
    let token_hashes = write_token_store_with_grants(&token_store_path, &used_identity_names);
    let mut change_threads = Vec::new();
    for (change_index, token_hash) in (0..8u64).zip(token_hashes) {
        let grant_store_path = token_store_path.clone();
        change_threads.push(std::thread::spawn(move || {
            let _store_lock_file = acquire_store_lock(
                &resolve_token_store_lock_path(&grant_store_path),
                STORE_LOCK_TIMEOUT_DURATION,
                "another test thread holds the lock",
            )
            .expect("the grant takes the store lock");
            let mut token_store = TokenStore::load_token_store_from_path(&grant_store_path)
                .expect("the store reads back");
            let _ = token_store.grant_token(
                format!("granted-{change_index}"),
                TokenScope::HostWide,
                build_time_after_epoch(1_000),
                None,
            );
            token_store
                .write_token_store_to_path(&grant_store_path)
                .expect("the grant is written");
        }));
        let use_store_path = token_store_path.clone();
        change_threads.push(std::thread::spawn(move || {
            let used_at_by_token_hash =
                HashMap::from([(token_hash, build_time_after_epoch(2_000 + change_index))]);
            assert_eq!(
                write_token_uses(&use_store_path, &used_at_by_token_hash),
                Ok(())
            );
        }));
    }
    for change_thread in change_threads {
        change_thread.join().expect("the change thread ends");
    }

    let token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    let mut last_used_by_identity: Vec<(&str, Option<SystemTime>)> = token_store
        .token_records
        .iter()
        .map(|token_record| (token_record.identity.as_str(), token_record.last_used_at))
        .collect();
    last_used_by_identity.sort_unstable_by_key(|(identity, _)| *identity);
    let granted_identities: Vec<String> = (0..8)
        .map(|change_index| format!("granted-{change_index}"))
        .collect();
    let mut expected_last_used_by_identity: Vec<(&str, Option<SystemTime>)> = granted_identities
        .iter()
        .map(|identity| (identity.as_str(), None))
        .collect();
    expected_last_used_by_identity.extend((0..8u64).zip(&used_identities).map(
        |(change_index, identity)| {
            (
                identity.as_str(),
                Some(build_time_after_epoch(2_000 + change_index)),
            )
        },
    ));
    assert_eq!(last_used_by_identity, expected_last_used_by_identity);
}

#[test]
fn a_failed_write_keeps_its_uses_and_the_next_write_stamps_them() {
    // A directory stands at the lock file's path, and the lock cannot be
    // taken: the use stays kept. Once the directory is gone, the next write
    // stamps the kept use and empties the kept uses.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(&runtime_directory.path().join("data"));
    let token_hashes = write_token_store_with_grants(&token_store_path, &["alice"]);
    let lock_file_path = resolve_token_store_lock_path(&token_store_path);
    std::fs::create_dir(&lock_file_path).expect("a directory takes the lock file's place");
    let mut used_at_by_token_hash =
        HashMap::from([(token_hashes[0].clone(), build_time_after_epoch(2_000))]);
    let mut write_failure_warning = WarningRateLimiter::new();

    write_kept_token_uses(
        &token_store_path,
        &mut used_at_by_token_hash,
        &mut write_failure_warning,
    );

    assert_eq!(
        used_at_by_token_hash,
        HashMap::from([(token_hashes[0].clone(), build_time_after_epoch(2_000))]),
        "the failed write kept its use"
    );
    assert_eq!(
        list_last_used_times(&token_store_path),
        vec![None],
        "the failed write stamped nothing"
    );

    std::fs::remove_dir(&lock_file_path).expect("the blocking directory is removed");
    write_kept_token_uses(
        &token_store_path,
        &mut used_at_by_token_hash,
        &mut write_failure_warning,
    );

    assert_eq!(used_at_by_token_hash, HashMap::new());
    assert_eq!(
        list_last_used_times(&token_store_path),
        vec![Some(build_time_after_epoch(2_000))]
    );
}

#[test]
fn a_store_file_that_does_not_exist_is_not_created() {
    // No record holds the hash of a use when no store file exists: the write
    // succeeds and leaves no store file.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(&runtime_directory.path().join("data"));
    let used_at_by_token_hash = HashMap::from([("a".repeat(64), build_time_after_epoch(2_000))]);

    assert_eq!(
        write_token_uses(&token_store_path, &used_at_by_token_hash),
        Ok(())
    );

    assert!(
        !token_store_path.exists(),
        "the write created no store file"
    );
}

#[test]
fn a_write_removes_grants_that_stopped_working_thirty_or_more_days_ago() {
    // A store holding alice's working grant and bob's grant revoked at second
    // 1_000: a use of alice's grant writes the store without bob's record.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(&runtime_directory.path().join("data"));
    let token_hashes = write_token_store_with_grants(&token_store_path, &["alice", "bob"]);
    let mut token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    token_store.revoke_token_grants("bob", None, build_time_after_epoch(1_000));
    token_store
        .write_token_store_to_path(&token_store_path)
        .expect("the store with bob revoked is written");
    let used_at_by_token_hash =
        HashMap::from([(token_hashes[0].clone(), build_time_after_epoch(2_000))]);

    assert_eq!(
        write_token_uses(&token_store_path, &used_at_by_token_hash),
        Ok(())
    );

    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    let identities: Vec<&str> = written_token_store
        .token_records
        .iter()
        .map(|token_record| token_record.identity.as_str())
        .collect();
    assert_eq!(identities, vec!["alice"]);
    assert_eq!(
        written_token_store.token_records[0].last_used_at,
        Some(build_time_after_epoch(2_000))
    );
}
