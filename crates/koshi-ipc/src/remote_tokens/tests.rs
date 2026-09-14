//! Tests for the remote access token store: where the file lives, the hashing
//! that keeps the secret off the disk, the write/read roundtrip through the
//! atomic writer, the private mode of the file, the unreadable and unwritable
//! failure cases, and what a presented token resolves to.

use std::time::Duration;

use tempfile::TempDir;

use super::*;

/// A fixed point on the clock, measured in seconds after the epoch.
fn system_time_at_seconds(seconds_since_epoch: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds_since_epoch)
}

/// A store holding one grant to `ada`, and the secret that grant handed out.
fn build_granted_token_store(
    scope: TokenScope,
    expires_at: Option<SystemTime>,
) -> (TokenStore, ConnectionToken) {
    let mut token_store = TokenStore::new();
    let (connection_token, _) = token_store.grant_token(
        "ada".to_string(),
        scope,
        system_time_at_seconds(100),
        expires_at,
    );
    (token_store, connection_token)
}

/// The permission bits of the file or directory at `file_path`.
#[cfg(unix)]
fn compute_file_mode(file_path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file_path)
        .expect("stat file path")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn the_store_path_is_remote_tokens_under_the_data_dir() {
    assert_eq!(
        resolve_token_store_path(Path::new("/home/ada/.local/share/koshi")),
        Path::new("/home/ada/.local/share/koshi/remote/tokens")
    );
}

#[test]
fn a_written_store_reads_back_identical_including_its_format_number() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = resolve_token_store_path(test_directory.path());
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        Some(system_time_at_seconds(900)),
    );
    token_store.grant_token(
        "zoe".to_string(),
        TokenScope::Session(SessionId::new()),
        system_time_at_seconds(200),
        None,
    );

    token_store
        .write_token_store_to_path(&token_store_path)
        .expect("write token store");

    let loaded_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("read token store");
    assert_eq!(loaded_token_store, token_store);
    assert_eq!(loaded_token_store.store_format, TOKEN_STORE_FORMAT);
}

/// The store carries the hash. The secret itself reaches only the operator it
/// was handed to.
#[test]
fn the_file_on_disk_holds_the_hash_and_not_the_secret() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = resolve_token_store_path(test_directory.path());
    let mut token_store = TokenStore::new();
    let (connection_token, _) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );

    token_store
        .write_token_store_to_path(&token_store_path)
        .expect("write token store");

    let token_store_text = std::fs::read_to_string(&token_store_path).expect("read file bytes");
    assert!(
        token_store_text.contains(&hash_connection_token(&connection_token)),
        "{token_store_text}"
    );
    assert!(
        !token_store_text.contains(connection_token.expose()),
        "{token_store_text}"
    );
}

#[test]
fn reading_a_missing_file_gives_an_empty_store_at_this_builds_format() {
    let test_directory = TempDir::new().expect("create test directory");

    let token_store =
        TokenStore::load_token_store_from_path(&resolve_token_store_path(test_directory.path()))
            .expect("read missing store");

    assert_eq!(token_store.store_format, TOKEN_STORE_FORMAT);
    assert_eq!(token_store.token_records, Vec::<TokenRecord>::new());
    assert_eq!(token_store, TokenStore::default());
}

#[cfg(unix)]
#[test]
fn a_fresh_store_file_and_its_directory_are_private() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = resolve_token_store_path(test_directory.path());

    TokenStore::new()
        .write_token_store_to_path(&token_store_path)
        .expect("write token store");

    assert_eq!(compute_file_mode(&token_store_path), 0o600);
    assert_eq!(
        compute_file_mode(&test_directory.path().join("remote")),
        0o700
    );
}

#[cfg(unix)]
#[test]
fn a_store_file_that_was_group_readable_is_private_after_the_write() {
    use std::os::unix::fs::PermissionsExt;
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = resolve_token_store_path(test_directory.path());
    TokenStore::new()
        .write_token_store_to_path(&token_store_path)
        .expect("write the first store");
    std::fs::set_permissions(&token_store_path, std::fs::Permissions::from_mode(0o644))
        .expect("widen the mode");

    TokenStore::new()
        .write_token_store_to_path(&token_store_path)
        .expect("write the second store");

    assert_eq!(compute_file_mode(&token_store_path), 0o600);
}

#[cfg(windows)]
#[test]
fn a_written_store_reads_back_on_windows() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = resolve_token_store_path(test_directory.path());
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );

    token_store
        .write_token_store_to_path(&token_store_path)
        .expect("write token store");

    assert_eq!(
        TokenStore::load_token_store_from_path(&token_store_path).expect("read token store"),
        token_store
    );
}

#[test]
fn reading_junk_bytes_is_token_store_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = test_directory.path().join("tokens");
    std::fs::write(&token_store_path, b"not json").expect("write junk");

    match TokenStore::load_token_store_from_path(&token_store_path) {
        Err(IpcError::RemoteFileUnreadable {
            remote_file: RemoteFile::TokenStore,
            remote_file_path: reported_file_path,
            error_detail,
        }) => {
            assert_eq!(reported_file_path, token_store_path.display().to_string());
            assert_eq!(error_detail, "expected ident at line 1 column 2");
        }
        unexpected_error => {
            panic!("expected a token store RemoteFileUnreadable, got {unexpected_error:?}")
        }
    }
}

#[test]
fn a_file_with_an_unknown_field_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = test_directory.path().join("tokens");
    std::fs::write(&token_store_path, r#"{"format":1,"records":[],"extra":1}"#)
        .expect("write file");

    match TokenStore::load_token_store_from_path(&token_store_path) {
        Err(IpcError::RemoteFileUnreadable {
            remote_file: RemoteFile::TokenStore,
            remote_file_path: reported_file_path,
            error_detail,
        }) => {
            assert_eq!(reported_file_path, token_store_path.display().to_string());
            assert_eq!(
                error_detail,
                "unknown field `extra`, expected `format` or `records` at line 1 column 32"
            );
        }
        unexpected_error => {
            panic!("expected a token store RemoteFileUnreadable, got {unexpected_error:?}")
        }
    }
}

#[test]
fn a_file_whose_format_number_is_two_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = test_directory.path().join("tokens");
    std::fs::write(&token_store_path, r#"{"format":2,"records":[]}"#).expect("write file");

    match TokenStore::load_token_store_from_path(&token_store_path) {
        Err(IpcError::RemoteFileUnreadable {
            remote_file: RemoteFile::TokenStore,
            remote_file_path: reported_file_path,
            error_detail,
        }) => {
            assert_eq!(reported_file_path, token_store_path.display().to_string());
            assert_eq!(error_detail, "format 2 is not the 1 this build reads");
        }
        unexpected_error => {
            panic!("expected a token store RemoteFileUnreadable, got {unexpected_error:?}")
        }
    }
}

/// The write creates the directory it needs. A plain file holding that
/// directory's name makes the write fail.
#[test]
fn writing_where_the_directory_cannot_exist_is_token_store_write() {
    let test_directory = TempDir::new().expect("create test directory");
    std::fs::write(test_directory.path().join("remote"), b"").expect("write the blocking file");
    let token_store_path = resolve_token_store_path(test_directory.path());

    match TokenStore::new().write_token_store_to_path(&token_store_path) {
        Err(IpcError::RemoteFileWrite {
            remote_file: RemoteFile::TokenStore,
            remote_file_path: reported_file_path,
            ..
        }) => {
            assert_eq!(reported_file_path, token_store_path.display().to_string());
        }
        unexpected_error => {
            panic!("expected a token store RemoteFileWrite, got {unexpected_error:?}")
        }
    }
}

#[test]
fn hashing_abc_gives_the_published_sha256_vector() {
    assert_eq!(
        hash_connection_token(&ConnectionToken::from_secret("abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn a_generated_token_hashes_to_sixty_four_lowercase_hex_characters() {
    let token_hash = hash_connection_token(&ConnectionToken::generate());

    assert_eq!(token_hash.len(), 64);
    assert!(
        token_hash
            .chars()
            .all(|character| matches!(character, '0'..='9' | 'a'..='f')),
        "{token_hash}"
    );
}

#[test]
fn a_host_wide_grant_admits_every_session() {
    let (mut token_store, connection_token) = build_granted_token_store(TokenScope::HostWide, None);

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(200),
        ),
        Resolution::Admitted
    );
    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(200),
        ),
        Resolution::Admitted
    );
}

#[test]
fn a_grant_scoped_to_one_session_admits_that_session() {
    let scoped_session_id = SessionId::new();
    let (mut token_store, connection_token) =
        build_granted_token_store(TokenScope::Session(scoped_session_id), None);

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            scoped_session_id,
            system_time_at_seconds(200),
        ),
        Resolution::Admitted
    );
}

#[test]
fn a_grant_scoped_to_one_session_refuses_any_other_session() {
    let scoped_session_id = SessionId::new();
    let other_session_id = SessionId::new();
    let (mut token_store, connection_token) =
        build_granted_token_store(TokenScope::Session(scoped_session_id), None);

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            other_session_id,
            system_time_at_seconds(200),
        ),
        Resolution::Refused
    );
    // A session no record names gets the identical answer.
    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(200),
        ),
        Resolution::Refused
    );
}

#[test]
fn an_expired_grant_refuses() {
    let (mut token_store, connection_token) =
        build_granted_token_store(TokenScope::HostWide, Some(system_time_at_seconds(150)));

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(200),
        ),
        Resolution::Refused
    );
    // The expiry instant itself is past the grant, not inside it.
    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(150),
        ),
        Resolution::Refused
    );
    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(149),
        ),
        Resolution::Admitted
    );
}

#[test]
fn a_grant_with_no_expiry_never_stops_on_its_own() {
    let (mut token_store, connection_token) = build_granted_token_store(TokenScope::HostWide, None);

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(4_000_000_000),
        ),
        Resolution::Admitted
    );
}

#[test]
fn a_revoked_grant_refuses() {
    let (mut token_store, connection_token) = build_granted_token_store(TokenScope::HostWide, None);
    token_store.revoke_token_grants("ada", None, system_time_at_seconds(150));

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(200),
        ),
        Resolution::Refused
    );
}

#[test]
fn an_unknown_token_refuses() {
    let (mut token_store, _connection_token) =
        build_granted_token_store(TokenScope::HostWide, None);

    assert_eq!(
        token_store.resolve_token_access(
            &ConnectionToken::generate(),
            SessionId::new(),
            system_time_at_seconds(200)
        ),
        Resolution::Refused
    );
}

#[test]
fn an_empty_store_refuses() {
    let mut token_store = TokenStore::new();

    assert_eq!(
        token_store.resolve_token_access(
            &ConnectionToken::generate(),
            SessionId::new(),
            system_time_at_seconds(200)
        ),
        Resolution::Refused
    );
}

#[test]
fn admitting_stamps_the_record_with_the_time_it_was_asked_about() {
    let (mut token_store, connection_token) = build_granted_token_store(TokenScope::HostWide, None);

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(200),
        ),
        Resolution::Admitted
    );

    assert_eq!(
        token_store.token_records[0].last_used_at,
        Some(system_time_at_seconds(200))
    );
}

#[test]
fn refusing_leaves_the_last_used_time_unset() {
    let (mut token_store, _connection_token) =
        build_granted_token_store(TokenScope::HostWide, None);

    assert_eq!(
        token_store.resolve_token_access(
            &ConnectionToken::generate(),
            SessionId::new(),
            system_time_at_seconds(200)
        ),
        Resolution::Refused
    );

    assert_eq!(token_store.token_records[0].last_used_at, None);
}

#[test]
fn a_second_grant_on_the_same_identity_and_scope_replaces_the_first() {
    let mut token_store = TokenStore::new();
    let (first_connection_token, has_replaced_first_grant) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );

    let (second_connection_token, has_replaced_second_grant) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(200),
        None,
    );

    assert!(!has_replaced_first_grant);
    assert!(has_replaced_second_grant);
    assert_eq!(token_store.token_records.len(), 1);
    assert_eq!(
        token_store.token_records[0].token_hash,
        hash_connection_token(&second_connection_token)
    );
    assert_ne!(
        token_store.token_records[0].token_hash,
        hash_connection_token(&first_connection_token)
    );
    assert_eq!(
        token_store.resolve_token_access(
            &first_connection_token,
            SessionId::new(),
            system_time_at_seconds(300),
        ),
        Resolution::Refused
    );
}

#[test]
fn a_grant_on_a_different_scope_for_the_same_identity_keeps_both() {
    let session_id = SessionId::new();
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );

    let (_, has_replaced_grant) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::Session(session_id),
        system_time_at_seconds(200),
        None,
    );

    assert!(!has_replaced_grant);
    assert_eq!(token_store.token_records.len(), 2);
    assert_eq!(token_store.token_records[0].scope, TokenScope::HostWide);
    assert_eq!(
        token_store.token_records[1].scope,
        TokenScope::Session(session_id)
    );
}

#[test]
fn entries_drop_the_hash_and_narrow_to_one_scope() {
    let session_id = SessionId::new();
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "zoe".to_string(),
        TokenScope::Session(session_id),
        system_time_at_seconds(100),
        None,
    );
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(200),
        Some(system_time_at_seconds(900)),
    );
    let ada_entry = TokenEntry {
        identity: "ada".to_string(),
        scope: TokenScope::HostWide,
        issued_at: system_time_at_seconds(200),
        expires_at: Some(system_time_at_seconds(900)),
        last_used_at: None,
        revoked_at: None,
    };
    let zoe_entry = TokenEntry {
        identity: "zoe".to_string(),
        scope: TokenScope::Session(session_id),
        issued_at: system_time_at_seconds(100),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
    };

    assert_eq!(
        token_store.list_token_entries(None),
        vec![ada_entry.clone(), zoe_entry.clone()]
    );
    assert_eq!(
        token_store.list_token_entries(Some(&TokenScope::HostWide)),
        vec![ada_entry.clone()],
        "host-wide keeps the grants reaching every session, and zoe's grant reaches one"
    );
    assert_eq!(
        token_store.list_token_entries(Some(&TokenScope::Session(session_id))),
        vec![ada_entry, zoe_entry],
        "one session keeps every grant that reaches it, so ada's host-wide grant is kept \
         beside zoe's grant on that session"
    );
}

#[test]
fn a_bare_revoke_stops_every_scope_that_identity_holds() {
    let session_id = SessionId::new();
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::Session(session_id),
        system_time_at_seconds(100),
        None,
    );

    assert_eq!(
        token_store.revoke_token_grants("ada", None, system_time_at_seconds(300)),
        vec![TokenScope::HostWide, TokenScope::Session(session_id)]
    );

    assert_eq!(
        token_store.token_records[0].revoked_at,
        Some(system_time_at_seconds(300))
    );
    assert_eq!(
        token_store.token_records[1].revoked_at,
        Some(system_time_at_seconds(300))
    );
}

#[test]
fn a_scoped_revoke_stops_one_grant_and_leaves_the_other_standing() {
    let session_id = SessionId::new();
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );
    let (scoped_connection_token, _) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::Session(session_id),
        system_time_at_seconds(100),
        None,
    );

    assert_eq!(
        token_store.revoke_token_grants(
            "ada",
            Some(&TokenScope::HostWide),
            system_time_at_seconds(300)
        ),
        vec![TokenScope::HostWide]
    );

    assert_eq!(
        token_store.token_records[0].revoked_at,
        Some(system_time_at_seconds(300))
    );
    assert_eq!(token_store.token_records[1].revoked_at, None);
    assert_eq!(
        token_store.resolve_token_access(
            &scoped_connection_token,
            session_id,
            system_time_at_seconds(400),
        ),
        Resolution::Admitted
    );
}

#[test]
fn revoking_an_identity_holding_nothing_stops_nothing() {
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );

    assert_eq!(
        token_store.revoke_token_grants("bob", None, system_time_at_seconds(300)),
        Vec::<TokenScope>::new()
    );

    assert_eq!(token_store.token_records[0].revoked_at, None);
}

#[test]
fn granting_after_a_revoke_reports_that_nothing_standing_stopped() {
    // Ada's grant was revoked before the new grant was made. The new grant
    // reports that nothing standing stopped.
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );
    token_store.revoke_token_grants("ada", None, system_time_at_seconds(200));

    let (_, has_replaced_grant) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(300),
        None,
    );

    assert!(!has_replaced_grant);
    assert_eq!(token_store.token_records.len(), 1);
    assert_eq!(
        token_store.token_records[0].issued_at,
        system_time_at_seconds(300)
    );
    assert_eq!(token_store.token_records[0].revoked_at, None);
}

#[test]
fn granting_after_an_expiry_reports_that_nothing_standing_stopped() {
    // The old grant ran out before this one was made. Nothing that still
    // worked stopped working.
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        Some(system_time_at_seconds(200)),
    );

    let (_, has_replaced_grant) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(300),
        None,
    );

    assert!(!has_replaced_grant);
    assert_eq!(token_store.token_records.len(), 1);
    assert_eq!(token_store.token_records[0].expires_at, None);
}

#[test]
fn granting_over_a_standing_grant_reports_that_it_stopped() {
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );

    let (_, has_replaced_grant) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(300),
        None,
    );

    assert!(has_replaced_grant);
    assert_eq!(token_store.token_records.len(), 1);
    assert_eq!(
        token_store.token_records[0].issued_at,
        system_time_at_seconds(300)
    );
}

#[test]
fn revoking_an_already_stopped_grant_keeps_the_first_time() {
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );
    token_store.revoke_token_grants("ada", None, system_time_at_seconds(300));

    assert_eq!(
        token_store.revoke_token_grants("ada", None, system_time_at_seconds(400)),
        Vec::<TokenScope>::new()
    );

    assert_eq!(
        token_store.token_records[0].revoked_at,
        Some(system_time_at_seconds(300))
    );
}

#[test]
fn a_grant_stops_working_at_the_moment_it_expires_and_not_a_moment_before() {
    // The expiry is the first instant the grant no longer works; the instant
    // before it still admits. One tick is 100 nanoseconds, the smallest step
    // a Windows system time holds.
    const SYSTEM_TIME_TICK_DURATION: Duration = Duration::from_nanos(100);
    let (mut token_store, connection_token) =
        build_granted_token_store(TokenScope::HostWide, Some(system_time_at_seconds(900)));
    let session_id = SessionId::new();

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            session_id,
            system_time_at_seconds(900) - SYSTEM_TIME_TICK_DURATION,
        ),
        Resolution::Admitted
    );
    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            session_id,
            system_time_at_seconds(900),
        ),
        Resolution::Refused
    );
    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            session_id,
            system_time_at_seconds(900) + SYSTEM_TIME_TICK_DURATION,
        ),
        Resolution::Refused
    );
}

#[test]
fn a_grant_made_with_an_expiry_already_past_never_admits() {
    let mut token_store = TokenStore::new();
    let (connection_token, _) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(500),
        Some(system_time_at_seconds(100)),
    );

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(500),
        ),
        Resolution::Refused
    );
    assert_eq!(token_store.token_records.len(), 1);
}

#[test]
fn a_grant_whose_expiry_is_the_moment_it_was_made_never_admits() {
    // A zero-length expiry lands the expiry on the issue time. A grant stops
    // working at its expiry, and this one never works at all.
    let mut token_store = TokenStore::new();
    let (connection_token, _) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(500),
        Some(system_time_at_seconds(500)),
    );

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(500),
        ),
        Resolution::Refused
    );
    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(501),
        ),
        Resolution::Refused
    );
    assert_eq!(token_store.token_records[0].last_used_at, None);
}

#[test]
fn an_empty_secret_reaches_nothing() {
    let (mut token_store, _connection_token) =
        build_granted_token_store(TokenScope::HostWide, None);

    assert_eq!(
        token_store.resolve_token_access(
            &ConnectionToken::from_secret(""),
            SessionId::new(),
            system_time_at_seconds(200)
        ),
        Resolution::Refused
    );
    assert_eq!(token_store.token_records[0].last_used_at, None);
}

#[test]
fn a_secret_presented_to_an_empty_store_reaches_nothing() {
    let mut token_store = TokenStore::new();

    assert_eq!(
        token_store.resolve_token_access(
            &ConnectionToken::generate(),
            SessionId::new(),
            system_time_at_seconds(200)
        ),
        Resolution::Refused
    );
    assert_eq!(token_store.token_records, Vec::new());
}

#[test]
fn a_record_carrying_a_hash_that_is_not_a_real_digest_admits_nothing() {
    // A hand-edited store can hold anything in the hash field. Nothing a
    // caller can present hashes to a value that is not a digest.
    let mut token_store = TokenStore::new();
    token_store.token_records.push(TokenRecord {
        identity: "ada".to_string(),
        token_hash: "not-a-digest".to_string(),
        scope: TokenScope::HostWide,
        issued_at: system_time_at_seconds(100),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
    });

    assert_eq!(
        token_store.resolve_token_access(
            &ConnectionToken::generate(),
            SessionId::new(),
            system_time_at_seconds(200)
        ),
        Resolution::Refused
    );
    assert_eq!(
        token_store.resolve_token_access(
            &ConnectionToken::from_secret("not-a-digest"),
            SessionId::new(),
            system_time_at_seconds(200)
        ),
        Resolution::Refused,
        "the stored hash is compared against the hash of what is presented, never against it"
    );
}

#[test]
fn a_hand_written_store_holding_two_records_on_one_key_revokes_both_at_once() {
    // The store keeps one record per identity and scope. A file written by
    // hand can hold two, and a revoke stops every one of them.
    let mut token_store = TokenStore::new();
    for issued_at_seconds in [100, 200] {
        token_store.token_records.push(TokenRecord {
            identity: "ada".to_string(),
            token_hash: format!("{issued_at_seconds:064}"),
            scope: TokenScope::HostWide,
            issued_at: system_time_at_seconds(issued_at_seconds),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        });
    }

    assert_eq!(
        token_store.revoke_token_grants("ada", None, system_time_at_seconds(300)),
        vec![TokenScope::HostWide, TokenScope::HostWide]
    );
    assert_eq!(
        token_store.token_records[0].revoked_at,
        Some(system_time_at_seconds(300))
    );
    assert_eq!(
        token_store.token_records[1].revoked_at,
        Some(system_time_at_seconds(300))
    );
}

#[test]
fn a_grant_over_a_hand_written_pair_on_one_key_leaves_exactly_one_record() {
    let mut token_store = TokenStore::new();
    for issued_at_seconds in [100, 200] {
        token_store.token_records.push(TokenRecord {
            identity: "ada".to_string(),
            token_hash: format!("{issued_at_seconds:064}"),
            scope: TokenScope::HostWide,
            issued_at: system_time_at_seconds(issued_at_seconds),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        });
    }

    let (_, did_replace_grant) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(300),
        None,
    );

    assert!(did_replace_grant);
    assert_eq!(token_store.token_records.len(), 1);
    assert_eq!(
        token_store.token_records[0].issued_at,
        system_time_at_seconds(300)
    );
}

#[test]
fn every_grant_being_revoked_still_lists_all_of_them() {
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );
    token_store.revoke_token_grants("ada", None, system_time_at_seconds(200));

    let listed_token_entries = token_store.list_token_entries(None);

    assert_eq!(
        listed_token_entries,
        vec![TokenEntry {
            identity: "ada".to_string(),
            scope: TokenScope::HostWide,
            issued_at: system_time_at_seconds(100),
            expires_at: None,
            last_used_at: None,
            revoked_at: Some(system_time_at_seconds(200)),
        }]
    );
}

#[test]
fn a_store_whose_bytes_stop_part_way_is_refused_and_admits_nothing() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = test_directory.path().join("tokens");
    std::fs::write(
        &token_store_path,
        br#"{"format":1,"records":[{"identity":"ad"#,
    )
    .expect("write the truncated store");

    let truncated_store_error = TokenStore::load_token_store_from_path(&token_store_path)
        .expect_err("a truncated store is refused");

    let IpcError::RemoteFileUnreadable {
        remote_file: RemoteFile::TokenStore,
        remote_file_path: reported_file_path,
        error_detail,
    } = truncated_store_error
    else {
        panic!("expected a token store RemoteFileUnreadable, got {truncated_store_error:?}");
    };
    assert_eq!(reported_file_path, token_store_path.display().to_string());
    assert_eq!(
        error_detail,
        "EOF while parsing a string at line 1 column 38"
    );
}

#[test]
fn a_directory_where_the_store_belongs_is_refused_rather_than_read_as_empty() {
    // A missing store reads as empty. A directory is not missing, and it is
    // refused.
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = test_directory.path().join("tokens");
    std::fs::create_dir(&token_store_path).expect("create a directory where the store belongs");

    let directory_store_error = TokenStore::load_token_store_from_path(&token_store_path)
        .expect_err("a directory is refused");

    let IpcError::RemoteFileUnreadable {
        remote_file: RemoteFile::TokenStore,
        remote_file_path: reported_file_path,
        ..
    } = directory_store_error
    else {
        panic!("expected a token store RemoteFileUnreadable, got {directory_store_error:?}");
    };
    assert_eq!(reported_file_path, token_store_path.display().to_string());
}

#[test]
fn a_store_holding_no_records_reads_back_as_a_store_holding_no_records() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = test_directory.path().join("tokens");
    std::fs::write(&token_store_path, br#"{"format":1,"records":[]}"#)
        .expect("write the empty store");

    let token_store = TokenStore::load_token_store_from_path(&token_store_path)
        .expect("an empty record list reads");

    assert_eq!(token_store.store_format, TOKEN_STORE_FORMAT);
    assert_eq!(token_store.token_records, Vec::new());
    assert_eq!(token_store.list_token_entries(None), Vec::new());
}

#[test]
fn a_live_secret_is_admitted_with_the_scope_it_was_granted_on() {
    let session_id = SessionId::new();
    let (mut token_store, connection_token) =
        build_granted_token_store(TokenScope::Session(session_id), None);

    assert_eq!(
        token_store.admit_token_scope(&connection_token, system_time_at_seconds(200)),
        Some(TokenScope::Session(session_id))
    );
    assert_eq!(
        token_store.token_records[0].last_used_at,
        Some(system_time_at_seconds(200))
    );
    assert!(TokenScope::Session(session_id).is_allowed_for_session(session_id));
    assert!(!TokenScope::Session(session_id).is_allowed_for_session(SessionId::new()));
}

#[test]
fn a_secret_no_record_holds_is_admitted_by_nothing() {
    let (mut token_store, _connection_token) =
        build_granted_token_store(TokenScope::HostWide, None);

    assert_eq!(
        token_store.admit_token_scope(&ConnectionToken::generate(), system_time_at_seconds(200)),
        None
    );
    assert_eq!(token_store.token_records[0].last_used_at, None);
}

#[test]
fn a_revoked_secret_and_an_expired_one_are_admitted_by_nothing() {
    let (mut token_store, connection_token) = build_granted_token_store(TokenScope::HostWide, None);
    token_store.revoke_token_grants("ada", None, system_time_at_seconds(150));
    assert_eq!(
        token_store.admit_token_scope(&connection_token, system_time_at_seconds(200)),
        None
    );

    let (mut token_store, connection_token) =
        build_granted_token_store(TokenScope::HostWide, Some(system_time_at_seconds(150)));
    assert_eq!(
        token_store.admit_token_scope(&connection_token, system_time_at_seconds(150)),
        None
    );
    assert_eq!(
        token_store.admit_token_scope(&connection_token, system_time_at_seconds(149)),
        Some(TokenScope::HostWide)
    );
}

#[test]
fn this_build_writes_token_store_format_one() {
    // A store written by this build carries format 1. A build that reads only
    // another number refuses this machine's grants.
    assert_eq!(TOKEN_STORE_FORMAT, 1);
}

// --- Whether a listed grant still stands ---

/// One listing row at `expires_at`, revoked at `revoked_at`.
fn build_token_entry_at(
    expires_at: Option<SystemTime>,
    revoked_at: Option<SystemTime>,
) -> TokenEntry {
    TokenEntry {
        identity: "alice".to_string(),
        scope: TokenScope::HostWide,
        issued_at: SystemTime::UNIX_EPOCH,
        expires_at,
        last_used_at: None,
        revoked_at,
    }
}

#[test]
fn a_listed_grant_stands_until_it_is_revoked_or_its_expiry_passes() {
    let current_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let expired_time = current_time - Duration::from_secs(1);
    let unexpired_time = current_time + Duration::from_secs(1);

    assert!(
        build_token_entry_at(None, None).is_active_at(current_time),
        "never expires, not revoked"
    );
    assert!(
        build_token_entry_at(Some(unexpired_time), None).is_active_at(current_time),
        "expiry remains ahead"
    );

    assert!(
        !build_token_entry_at(Some(expired_time), None).is_active_at(current_time),
        "expiry passed"
    );
    assert!(
        !build_token_entry_at(Some(current_time), None).is_active_at(current_time),
        "the expiry instant itself is past: the check is `expiry > now`"
    );
    assert!(
        !build_token_entry_at(None, Some(expired_time)).is_active_at(current_time),
        "revoked"
    );
    assert!(
        !build_token_entry_at(Some(unexpired_time), Some(expired_time)).is_active_at(current_time),
        "revoked beats an expiry still ahead"
    );
}

#[test]
fn a_listed_grant_stands_exactly_when_the_record_it_came_from_does() {
    let current_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    for expires_at in [
        None,
        Some(current_time - Duration::from_secs(1)),
        Some(current_time + Duration::from_secs(1)),
    ] {
        for revoked_at in [None, Some(current_time - Duration::from_secs(1))] {
            let token_record = TokenRecord {
                identity: "alice".to_string(),
                token_hash: "a".repeat(64),
                scope: TokenScope::HostWide,
                issued_at: SystemTime::UNIX_EPOCH,
                expires_at,
                last_used_at: None,
                revoked_at,
            };
            assert_eq!(
                token_record.build_token_entry().is_active_at(current_time),
                token_record.is_active_at(current_time),
                "the row and its record answer alike for {expires_at:?} / {revoked_at:?}"
            );
        }
    }
}

/// A file written by hand can hold two token_records on one hash. The walk runs to
/// the end of the list. The last live record holding the secret is the one
/// that answers and the one that is stamped.
#[test]
fn the_last_live_record_holding_a_secret_is_the_one_that_answers() {
    let session_id = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    let connection_token = ConnectionToken::generate();
    let mut token_store = TokenStore::new();
    for (scope, issued_at_seconds) in [
        (TokenScope::Session(session_id), 100),
        (TokenScope::HostWide, 200),
    ] {
        token_store.token_records.push(TokenRecord {
            identity: "ada".to_string(),
            token_hash: hash_connection_token(&connection_token),
            scope,
            issued_at: system_time_at_seconds(issued_at_seconds),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        });
    }

    assert_eq!(
        token_store.admit_token_scope(&connection_token, system_time_at_seconds(300)),
        Some(TokenScope::HostWide)
    );
    assert_eq!(token_store.token_records[0].last_used_at, None);
    assert_eq!(
        token_store.token_records[1].last_used_at,
        Some(system_time_at_seconds(300))
    );
}

/// The walk keeps only the token_records that still stand and reach the session
/// asked for. A record after it that is revoked or scoped elsewhere leaves
/// the earlier one answering.
#[test]
fn a_later_record_that_is_revoked_or_scoped_elsewhere_leaves_an_earlier_one_answering() {
    let requested_session_id = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    let other_session_id = SessionId::from_uuid(uuid::Uuid::from_u128(2));
    let connection_token = ConnectionToken::generate();
    let mut token_store = TokenStore::new();
    for (scope, revoked_at) in [
        (TokenScope::Session(requested_session_id), None),
        (TokenScope::Session(other_session_id), None),
        (TokenScope::HostWide, Some(system_time_at_seconds(250))),
    ] {
        token_store.token_records.push(TokenRecord {
            identity: "ada".to_string(),
            token_hash: hash_connection_token(&connection_token),
            scope,
            issued_at: system_time_at_seconds(100),
            expires_at: None,
            last_used_at: None,
            revoked_at,
        });
    }

    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            requested_session_id,
            system_time_at_seconds(300),
        ),
        Resolution::Admitted
    );
    assert_eq!(
        token_store.token_records[0].last_used_at,
        Some(system_time_at_seconds(300))
    );
    assert_eq!(token_store.token_records[1].last_used_at, None);
    assert_eq!(token_store.token_records[2].last_used_at, None);
}

#[test]
fn one_identity_holding_several_scopes_lists_host_wide_first_then_sessions_by_id() {
    let first_session_id = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    let second_session_id = SessionId::from_uuid(uuid::Uuid::from_u128(2));
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::Session(second_session_id),
        system_time_at_seconds(100),
        None,
    );
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::Session(first_session_id),
        system_time_at_seconds(200),
        None,
    );
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(300),
        None,
    );

    let listed_token_scopes: Vec<TokenScope> = token_store
        .list_token_entries(None)
        .into_iter()
        .map(|token_entry| token_entry.scope)
        .collect();

    assert_eq!(
        listed_token_scopes,
        vec![
            TokenScope::HostWide,
            TokenScope::Session(first_session_id),
            TokenScope::Session(second_session_id),
        ]
    );
}

#[test]
fn a_store_holding_one_record_with_every_field_set_is_written_as_these_exact_bytes() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = resolve_token_store_path(test_directory.path());
    let token_store = TokenStore {
        store_format: TOKEN_STORE_FORMAT,
        token_records: vec![TokenRecord {
            identity: "ada".to_string(),
            token_hash: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                .to_string(),
            scope: TokenScope::Session(SessionId::from_uuid(uuid::Uuid::from_u128(1))),
            issued_at: system_time_at_seconds(100),
            expires_at: Some(system_time_at_seconds(900)),
            last_used_at: Some(system_time_at_seconds(200)),
            revoked_at: Some(system_time_at_seconds(300)),
        }],
    };

    token_store
        .write_token_store_to_path(&token_store_path)
        .expect("write token store");

    assert_eq!(
        std::fs::read_to_string(&token_store_path).expect("read file bytes"),
        r#"{"format":1,"records":[{"identity":"ada","hash":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","scope":{"Session":"00000000-0000-0000-0000-000000000001"},"issued_at":{"secs_since_epoch":100,"nanos_since_epoch":0},"expires_at":{"secs_since_epoch":900,"nanos_since_epoch":0},"last_used_at":{"secs_since_epoch":200,"nanos_since_epoch":0},"revoked_at":{"secs_since_epoch":300,"nanos_since_epoch":0}}]}"#
    );
    assert_eq!(
        TokenStore::load_token_store_from_path(&token_store_path).expect("read token store"),
        token_store
    );
}

#[test]
fn hashing_an_empty_secret_gives_the_published_sha256_vector() {
    assert_eq!(
        hash_connection_token(&ConnectionToken::from_secret("")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn a_scope_reads_back_from_its_two_shapes_and_refuses_a_third() {
    assert_eq!(
        serde_json::from_str::<TokenScope>(r#""HostWide""#).expect("a host-wide scope reads"),
        TokenScope::HostWide
    );
    assert_eq!(
        serde_json::from_str::<TokenScope>(r#"{"Session":"00000000-0000-0000-0000-000000000001"}"#)
            .expect("a session scope reads"),
        TokenScope::Session(SessionId::from_uuid(uuid::Uuid::from_u128(1)))
    );

    let refused = serde_json::from_str::<TokenScope>(r#""Everywhere""#)
        .expect_err("a scope this build does not have is refused");

    assert_eq!(
        refused.to_string(),
        "unknown variant `Everywhere`, expected `HostWide` or `Session` at line 1 column 12"
    );
}

#[test]
fn a_record_carrying_an_unknown_field_makes_the_store_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = test_directory.path().join("tokens");
    std::fs::write(
        &token_store_path,
        r#"{"format":1,"records":[{"identity":"ada","hash":"h","scope":"HostWide","issued_at":{"secs_since_epoch":100,"nanos_since_epoch":0},"expires_at":null,"last_used_at":null,"revoked_at":null,"extra":1}]}"#,
    )
    .expect("write file");

    let unknown_record_field_error = TokenStore::load_token_store_from_path(&token_store_path)
        .expect_err("an unknown record field is refused");

    let IpcError::RemoteFileUnreadable {
        remote_file: RemoteFile::TokenStore,
        remote_file_path: reported_file_path,
        error_detail,
    } = unknown_record_field_error
    else {
        panic!("expected a token store RemoteFileUnreadable, got {unknown_record_field_error:?}");
    };
    assert_eq!(reported_file_path, token_store_path.display().to_string());
    assert_eq!(
        error_detail,
        "unknown field `extra`, expected one of `identity`, `hash`, `scope`, `issued_at`, \
         `expires_at`, `last_used_at`, `revoked_at` at line 1 column 193"
    );
}

#[test]
fn a_listed_grant_carrying_a_field_this_build_does_not_know_still_reads() {
    let listed_token_entry = serde_json::from_str::<TokenEntry>(
        r#"{"identity":"ada","scope":"HostWide","issued_at":{"secs_since_epoch":100,"nanos_since_epoch":0},"expires_at":null,"last_used_at":null,"revoked_at":null,"extra":1}"#,
    )
    .expect("a listing row with an added field reads");

    assert_eq!(
        listed_token_entry,
        TokenEntry {
            identity: "ada".to_string(),
            scope: TokenScope::HostWide,
            issued_at: system_time_at_seconds(100),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        }
    );
}

#[test]
fn a_file_missing_its_record_list_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = test_directory.path().join("tokens");
    std::fs::write(&token_store_path, r#"{"format":1}"#).expect("write file");

    let missing_records_field_error = TokenStore::load_token_store_from_path(&token_store_path)
        .expect_err("a store without records is refused");

    let IpcError::RemoteFileUnreadable {
        remote_file: RemoteFile::TokenStore,
        remote_file_path: reported_file_path,
        error_detail,
    } = missing_records_field_error
    else {
        panic!("expected a token store RemoteFileUnreadable, got {missing_records_field_error:?}");
    };
    assert_eq!(reported_file_path, token_store_path.display().to_string());
    assert_eq!(error_detail, "missing field `records` at line 1 column 12");
}

#[test]
fn a_file_whose_format_number_is_zero_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let token_store_path = test_directory.path().join("tokens");
    std::fs::write(&token_store_path, r#"{"format":0,"records":[]}"#).expect("write file");

    let unsupported_format_error =
        TokenStore::load_token_store_from_path(&token_store_path).expect_err("format 0 is refused");

    let IpcError::RemoteFileUnreadable {
        remote_file: RemoteFile::TokenStore,
        remote_file_path: reported_file_path,
        error_detail,
    } = unsupported_format_error
    else {
        panic!("expected a token store RemoteFileUnreadable, got {unsupported_format_error:?}");
    };
    assert_eq!(reported_file_path, token_store_path.display().to_string());
    assert_eq!(error_detail, "format 0 is not the 1 this build reads");
}

#[test]
fn a_scoped_revoke_naming_a_scope_the_identity_does_not_hold_stops_nothing() {
    let (mut token_store, connection_token) = build_granted_token_store(TokenScope::HostWide, None);

    assert_eq!(
        token_store.revoke_token_grants(
            "ada",
            Some(&TokenScope::Session(SessionId::new())),
            system_time_at_seconds(300)
        ),
        Vec::<TokenScope>::new()
    );

    assert_eq!(token_store.token_records[0].revoked_at, None);
    assert_eq!(
        token_store.resolve_token_access(
            &connection_token,
            SessionId::new(),
            system_time_at_seconds(400),
        ),
        Resolution::Admitted
    );
}

#[test]
fn a_bare_revoke_leaves_another_identitys_grants_standing() {
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );
    let (bob_connection_token, _) = token_store.grant_token(
        "bob".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );

    assert_eq!(
        token_store.revoke_token_grants("ada", None, system_time_at_seconds(300)),
        vec![TokenScope::HostWide]
    );

    assert_eq!(
        token_store.token_records[0].revoked_at,
        Some(system_time_at_seconds(300))
    );
    assert_eq!(token_store.token_records[1].revoked_at, None);
    assert_eq!(
        token_store.resolve_token_access(
            &bob_connection_token,
            SessionId::new(),
            system_time_at_seconds(400),
        ),
        Resolution::Admitted
    );
}

#[test]
fn narrowing_to_a_scope_no_grant_reaches_lists_nothing() {
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "zoe".to_string(),
        TokenScope::Session(SessionId::from_uuid(uuid::Uuid::from_u128(1))),
        system_time_at_seconds(100),
        None,
    );

    assert_eq!(
        token_store.list_token_entries(Some(&TokenScope::Session(SessionId::from_uuid(
            uuid::Uuid::from_u128(2)
        )))),
        Vec::new()
    );
    assert_eq!(
        token_store.list_token_entries(Some(&TokenScope::HostWide)),
        Vec::new()
    );
}

#[test]
fn an_uppercase_identity_lists_before_a_lowercase_one() {
    let mut token_store = TokenStore::new();
    token_store.grant_token(
        "ada".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(100),
        None,
    );
    token_store.grant_token(
        "Bob".to_string(),
        TokenScope::HostWide,
        system_time_at_seconds(200),
        None,
    );

    let identities: Vec<String> = token_store
        .list_token_entries(None)
        .into_iter()
        .map(|token_entry| token_entry.identity)
        .collect();

    assert_eq!(identities, vec!["Bob".to_string(), "ada".to_string()]);
}

#[test]
fn a_fresh_grant_is_recorded_with_the_times_it_was_given_and_no_other() {
    let session_id = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    let mut token_store = TokenStore::new();

    let (connection_token, has_replaced_grant) = token_store.grant_token(
        "ada".to_string(),
        TokenScope::Session(session_id),
        system_time_at_seconds(100),
        Some(system_time_at_seconds(900)),
    );

    assert!(!has_replaced_grant);
    assert_eq!(token_store.token_records.len(), 1);
    assert_eq!(
        token_store.token_records[0].token_hash,
        hash_connection_token(&connection_token)
    );
    assert_eq!(
        token_store.token_records[0].build_token_entry(),
        TokenEntry {
            identity: "ada".to_string(),
            scope: TokenScope::Session(session_id),
            issued_at: system_time_at_seconds(100),
            expires_at: Some(system_time_at_seconds(900)),
            last_used_at: None,
            revoked_at: None,
        }
    );
}
