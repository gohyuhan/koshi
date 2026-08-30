//! Tests for the remote access token store: where the file lives, the hashing
//! that keeps the secret off the disk, the write/read roundtrip through the
//! atomic writer, the private mode of the file, the unreadable and unwritable
//! failure cases, and what a presented token resolves to.

use std::time::Duration;

use tempfile::TempDir;

use super::*;

/// A fixed point on the clock, `secs` seconds after the epoch.
fn moment(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// A store holding one grant to `ada`, and the secret that grant handed out.
fn granted(scope: TokenScope, expires_at: Option<SystemTime>) -> (TokenStore, ConnectionToken) {
    let mut store = TokenStore::new();
    let (token, _) = store.grant("ada".to_string(), scope, moment(100), expires_at);
    (store, token)
}

/// The permission bits of the file or directory at `path`.
#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("stat path")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn the_store_path_is_remote_tokens_under_the_data_dir() {
    assert_eq!(
        store_path(Path::new("/home/ada/.local/share/koshi")),
        Path::new("/home/ada/.local/share/koshi/remote/tokens")
    );
}

#[test]
fn a_written_store_reads_back_identical_including_its_format_number() {
    let dir = TempDir::new().expect("create temp dir");
    let path = store_path(dir.path());
    let mut store = TokenStore::new();
    store.grant(
        "ada".to_string(),
        TokenScope::HostWide,
        moment(100),
        Some(moment(900)),
    );
    store.grant(
        "zoe".to_string(),
        TokenScope::Session(SessionId::new()),
        moment(200),
        None,
    );

    store.write(&path).expect("write token store");

    let read_back = TokenStore::read(&path).expect("read token store");
    assert_eq!(read_back, store);
    assert_eq!(read_back.format, TOKEN_STORE_FORMAT);
}

/// The store carries the hash. The secret itself reaches only the operator it
/// was handed to.
#[test]
fn the_file_on_disk_holds_the_hash_and_not_the_secret() {
    let dir = TempDir::new().expect("create temp dir");
    let path = store_path(dir.path());
    let mut store = TokenStore::new();
    let (token, _) = store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);

    store.write(&path).expect("write token store");

    let data = std::fs::read_to_string(&path).expect("read file bytes");
    assert!(data.contains(&hash_token(&token)), "{data}");
    assert!(!data.contains(token.expose()), "{data}");
}

#[test]
fn reading_a_missing_file_gives_an_empty_store_at_this_builds_format() {
    let dir = TempDir::new().expect("create temp dir");

    let store = TokenStore::read(&store_path(dir.path())).expect("read missing store");

    assert_eq!(store.format, TOKEN_STORE_FORMAT);
    assert_eq!(store.records, Vec::<TokenRecord>::new());
    assert_eq!(store, TokenStore::default());
}

#[cfg(unix)]
#[test]
fn a_fresh_store_file_and_its_directory_are_private() {
    let dir = TempDir::new().expect("create temp dir");
    let path = store_path(dir.path());

    TokenStore::new().write(&path).expect("write token store");

    assert_eq!(mode_of(&path), 0o600);
    assert_eq!(mode_of(&dir.path().join("remote")), 0o700);
}

#[cfg(unix)]
#[test]
fn a_store_file_that_was_group_readable_is_private_after_the_write() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().expect("create temp dir");
    let path = store_path(dir.path());
    TokenStore::new()
        .write(&path)
        .expect("write the first store");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("widen the mode");

    TokenStore::new()
        .write(&path)
        .expect("write the second store");

    assert_eq!(mode_of(&path), 0o600);
}

#[cfg(windows)]
#[test]
fn a_written_store_reads_back_on_windows() {
    let dir = TempDir::new().expect("create temp dir");
    let path = store_path(dir.path());
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);

    store.write(&path).expect("write token store");

    assert_eq!(TokenStore::read(&path).expect("read token store"), store);
}

#[test]
fn reading_junk_bytes_is_token_store_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("tokens");
    std::fs::write(&path, b"not json").expect("write junk");

    match TokenStore::read(&path) {
        Err(IpcError::TokenStoreUnreadable {
            path: reported,
            detail,
        }) => {
            assert_eq!(reported, path.display().to_string());
            assert_eq!(detail, "expected ident at line 1 column 2");
        }
        other => panic!("expected TokenStoreUnreadable, got {other:?}"),
    }
}

#[test]
fn a_file_with_an_unknown_field_is_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("tokens");
    std::fs::write(&path, r#"{"format":1,"records":[],"extra":1}"#).expect("write file");

    match TokenStore::read(&path) {
        Err(IpcError::TokenStoreUnreadable {
            path: reported,
            detail,
        }) => {
            assert_eq!(reported, path.display().to_string());
            assert_eq!(
                detail,
                "unknown field `extra`, expected `format` or `records` at line 1 column 32"
            );
        }
        other => panic!("expected TokenStoreUnreadable, got {other:?}"),
    }
}

#[test]
fn a_file_whose_format_number_is_two_is_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("tokens");
    std::fs::write(&path, r#"{"format":2,"records":[]}"#).expect("write file");

    match TokenStore::read(&path) {
        Err(IpcError::TokenStoreUnreadable {
            path: reported,
            detail,
        }) => {
            assert_eq!(reported, path.display().to_string());
            assert_eq!(detail, "format 2 is not the 1 this build reads");
        }
        other => panic!("expected TokenStoreUnreadable, got {other:?}"),
    }
}

/// The write creates the directory it needs. A plain file holding that
/// directory's name makes the write fail.
#[test]
fn writing_where_the_directory_cannot_exist_is_token_store_write() {
    let dir = TempDir::new().expect("create temp dir");
    std::fs::write(dir.path().join("remote"), b"").expect("write the blocking file");
    let path = store_path(dir.path());

    match TokenStore::new().write(&path) {
        Err(IpcError::TokenStoreWrite { path: reported, .. }) => {
            assert_eq!(reported, path.display().to_string());
        }
        other => panic!("expected TokenStoreWrite, got {other:?}"),
    }
}

#[test]
fn hashing_abc_gives_the_published_sha256_vector() {
    assert_eq!(
        hash_token(&ConnectionToken::new("abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn a_generated_token_hashes_to_sixty_four_lowercase_hex_characters() {
    let hash = hash_token(&ConnectionToken::generate());

    assert_eq!(hash.len(), 64);
    assert!(
        hash.chars().all(|ch| matches!(ch, '0'..='9' | 'a'..='f')),
        "{hash}"
    );
}

#[test]
fn a_host_wide_grant_admits_every_session() {
    let (mut store, token) = granted(TokenScope::HostWide, None);

    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(200)),
        Resolution::Admitted
    );
    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(200)),
        Resolution::Admitted
    );
}

#[test]
fn a_grant_scoped_to_one_session_admits_that_session() {
    let session = SessionId::new();
    let (mut store, token) = granted(TokenScope::Session(session), None);

    assert_eq!(
        store.resolve(&token, session, moment(200)),
        Resolution::Admitted
    );
}

#[test]
fn a_grant_scoped_to_one_session_refuses_any_other_session() {
    let session = SessionId::new();
    let other = SessionId::new();
    let (mut store, token) = granted(TokenScope::Session(session), None);

    assert_eq!(
        store.resolve(&token, other, moment(200)),
        Resolution::Refused
    );
    // A session no record names gets the identical answer.
    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(200)),
        Resolution::Refused
    );
}

#[test]
fn an_expired_grant_refuses() {
    let (mut store, token) = granted(TokenScope::HostWide, Some(moment(150)));

    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(200)),
        Resolution::Refused
    );
    // The expiry instant itself is past the grant, not inside it.
    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(150)),
        Resolution::Refused
    );
    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(149)),
        Resolution::Admitted
    );
}

#[test]
fn a_grant_with_no_expiry_never_stops_on_its_own() {
    let (mut store, token) = granted(TokenScope::HostWide, None);

    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(4_000_000_000)),
        Resolution::Admitted
    );
}

#[test]
fn a_revoked_grant_refuses() {
    let (mut store, token) = granted(TokenScope::HostWide, None);
    store.revoke("ada", None, moment(150));

    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(200)),
        Resolution::Refused
    );
}

#[test]
fn an_unknown_token_refuses() {
    let (mut store, _token) = granted(TokenScope::HostWide, None);

    assert_eq!(
        store.resolve(&ConnectionToken::generate(), SessionId::new(), moment(200)),
        Resolution::Refused
    );
}

#[test]
fn an_empty_store_refuses() {
    let mut store = TokenStore::new();

    assert_eq!(
        store.resolve(&ConnectionToken::generate(), SessionId::new(), moment(200)),
        Resolution::Refused
    );
}

#[test]
fn admitting_stamps_the_record_with_the_time_it_was_asked_about() {
    let (mut store, token) = granted(TokenScope::HostWide, None);

    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(200)),
        Resolution::Admitted
    );

    assert_eq!(store.records[0].last_used_at, Some(moment(200)));
}

#[test]
fn refusing_leaves_the_last_used_time_unset() {
    let (mut store, _token) = granted(TokenScope::HostWide, None);

    assert_eq!(
        store.resolve(&ConnectionToken::generate(), SessionId::new(), moment(200)),
        Resolution::Refused
    );

    assert_eq!(store.records[0].last_used_at, None);
}

#[test]
fn a_second_grant_on_the_same_identity_and_scope_replaces_the_first() {
    let mut store = TokenStore::new();
    let (first, replaced_first) =
        store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);

    let (second, replaced_second) =
        store.grant("ada".to_string(), TokenScope::HostWide, moment(200), None);

    assert!(!replaced_first);
    assert!(replaced_second);
    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].hash, hash_token(&second));
    assert_ne!(store.records[0].hash, hash_token(&first));
    assert_eq!(
        store.resolve(&first, SessionId::new(), moment(300)),
        Resolution::Refused
    );
}

#[test]
fn a_grant_on_a_different_scope_for_the_same_identity_keeps_both() {
    let session = SessionId::new();
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);

    let (_, replaced) = store.grant(
        "ada".to_string(),
        TokenScope::Session(session),
        moment(200),
        None,
    );

    assert!(!replaced);
    assert_eq!(store.records.len(), 2);
    assert_eq!(store.records[0].scope, TokenScope::HostWide);
    assert_eq!(store.records[1].scope, TokenScope::Session(session));
}

#[test]
fn entries_drop_the_hash_and_narrow_to_one_scope() {
    let session = SessionId::new();
    let mut store = TokenStore::new();
    store.grant(
        "zoe".to_string(),
        TokenScope::Session(session),
        moment(100),
        None,
    );
    store.grant(
        "ada".to_string(),
        TokenScope::HostWide,
        moment(200),
        Some(moment(900)),
    );
    let ada = TokenEntry {
        identity: "ada".to_string(),
        scope: TokenScope::HostWide,
        issued_at: moment(200),
        expires_at: Some(moment(900)),
        last_used_at: None,
        revoked_at: None,
    };
    let zoe = TokenEntry {
        identity: "zoe".to_string(),
        scope: TokenScope::Session(session),
        issued_at: moment(100),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
    };

    assert_eq!(store.entries(None), vec![ada.clone(), zoe.clone()]);
    assert_eq!(
        store.entries(Some(&TokenScope::HostWide)),
        vec![ada.clone()],
        "host-wide keeps the grants reaching every session, and zoe's grant reaches one"
    );
    assert_eq!(
        store.entries(Some(&TokenScope::Session(session))),
        vec![ada, zoe],
        "one session keeps every grant that reaches it, so ada's host-wide grant is kept \
         beside zoe's grant on that session"
    );
}

#[test]
fn a_bare_revoke_stops_every_scope_that_identity_holds() {
    let session = SessionId::new();
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);
    store.grant(
        "ada".to_string(),
        TokenScope::Session(session),
        moment(100),
        None,
    );

    assert_eq!(
        store.revoke("ada", None, moment(300)),
        vec![TokenScope::HostWide, TokenScope::Session(session)]
    );

    assert_eq!(store.records[0].revoked_at, Some(moment(300)));
    assert_eq!(store.records[1].revoked_at, Some(moment(300)));
}

#[test]
fn a_scoped_revoke_stops_one_grant_and_leaves_the_other_standing() {
    let session = SessionId::new();
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);
    let (scoped, _) = store.grant(
        "ada".to_string(),
        TokenScope::Session(session),
        moment(100),
        None,
    );

    assert_eq!(
        store.revoke("ada", Some(&TokenScope::HostWide), moment(300)),
        vec![TokenScope::HostWide]
    );

    assert_eq!(store.records[0].revoked_at, Some(moment(300)));
    assert_eq!(store.records[1].revoked_at, None);
    assert_eq!(
        store.resolve(&scoped, session, moment(400)),
        Resolution::Admitted
    );
}

#[test]
fn revoking_an_identity_holding_nothing_stops_nothing() {
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);

    assert_eq!(
        store.revoke("bob", None, moment(300)),
        Vec::<TokenScope>::new()
    );

    assert_eq!(store.records[0].revoked_at, None);
}

#[test]
fn granting_after_a_revoke_reports_that_nothing_standing_stopped() {
    // Ada's grant was revoked before the new grant was made. The new grant
    // reports that nothing standing stopped.
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);
    store.revoke("ada", None, moment(200));

    let (_, replaced) = store.grant("ada".to_string(), TokenScope::HostWide, moment(300), None);

    assert!(!replaced);
    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].issued_at, moment(300));
    assert_eq!(store.records[0].revoked_at, None);
}

#[test]
fn granting_after_an_expiry_reports_that_nothing_standing_stopped() {
    // The old grant ran out before this one was made. Nothing that still
    // worked stopped working.
    let mut store = TokenStore::new();
    store.grant(
        "ada".to_string(),
        TokenScope::HostWide,
        moment(100),
        Some(moment(200)),
    );

    let (_, replaced) = store.grant("ada".to_string(), TokenScope::HostWide, moment(300), None);

    assert!(!replaced);
    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].expires_at, None);
}

#[test]
fn granting_over_a_standing_grant_reports_that_it_stopped() {
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);

    let (_, replaced) = store.grant("ada".to_string(), TokenScope::HostWide, moment(300), None);

    assert!(replaced);
    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].issued_at, moment(300));
}

#[test]
fn revoking_an_already_stopped_grant_keeps_the_first_time() {
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);
    store.revoke("ada", None, moment(300));

    assert_eq!(
        store.revoke("ada", None, moment(400)),
        Vec::<TokenScope>::new()
    );

    assert_eq!(store.records[0].revoked_at, Some(moment(300)));
}

#[test]
fn a_grant_stops_working_at_the_moment_it_expires_and_not_a_moment_before() {
    // The expiry is the first instant the grant no longer works; the instant
    // before it still admits. One tick is 100 nanoseconds, the smallest step
    // a Windows system time holds.
    const TICK: Duration = Duration::from_nanos(100);
    let (mut store, token) = granted(TokenScope::HostWide, Some(moment(900)));
    let session = SessionId::new();

    assert_eq!(
        store.resolve(&token, session, moment(900) - TICK),
        Resolution::Admitted
    );
    assert_eq!(
        store.resolve(&token, session, moment(900)),
        Resolution::Refused
    );
    assert_eq!(
        store.resolve(&token, session, moment(900) + TICK),
        Resolution::Refused
    );
}

#[test]
fn a_grant_made_with_an_expiry_already_past_never_admits() {
    let mut store = TokenStore::new();
    let (token, _) = store.grant(
        "ada".to_string(),
        TokenScope::HostWide,
        moment(500),
        Some(moment(100)),
    );

    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(500)),
        Resolution::Refused
    );
    assert_eq!(store.records.len(), 1);
}

#[test]
fn a_grant_whose_expiry_is_the_moment_it_was_made_never_admits() {
    // A zero-length expiry lands the expiry on the issue time. A grant stops
    // working at its expiry, and this one never works at all.
    let mut store = TokenStore::new();
    let (token, _) = store.grant(
        "ada".to_string(),
        TokenScope::HostWide,
        moment(500),
        Some(moment(500)),
    );

    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(500)),
        Resolution::Refused
    );
    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(501)),
        Resolution::Refused
    );
    assert_eq!(store.records[0].last_used_at, None);
}

#[test]
fn an_empty_secret_reaches_nothing() {
    let (mut store, _) = granted(TokenScope::HostWide, None);

    assert_eq!(
        store.resolve(&ConnectionToken::new(""), SessionId::new(), moment(200)),
        Resolution::Refused
    );
    assert_eq!(store.records[0].last_used_at, None);
}

#[test]
fn a_secret_presented_to_an_empty_store_reaches_nothing() {
    let mut store = TokenStore::new();

    assert_eq!(
        store.resolve(&ConnectionToken::generate(), SessionId::new(), moment(200)),
        Resolution::Refused
    );
    assert_eq!(store.records, Vec::new());
}

#[test]
fn a_record_carrying_a_hash_that_is_not_a_real_digest_admits_nothing() {
    // A hand-edited store can hold anything in the hash field. Nothing a
    // caller can present hashes to a value that is not a digest.
    let mut store = TokenStore::new();
    store.records.push(TokenRecord {
        identity: "ada".to_string(),
        hash: "not-a-digest".to_string(),
        scope: TokenScope::HostWide,
        issued_at: moment(100),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
    });

    assert_eq!(
        store.resolve(&ConnectionToken::generate(), SessionId::new(), moment(200)),
        Resolution::Refused
    );
    assert_eq!(
        store.resolve(
            &ConnectionToken::new("not-a-digest"),
            SessionId::new(),
            moment(200)
        ),
        Resolution::Refused,
        "the stored hash is compared against the hash of what is presented, never against it"
    );
}

#[test]
fn a_hand_written_store_holding_two_records_on_one_key_revokes_both_at_once() {
    // The store keeps one record per identity and scope. A file written by
    // hand can hold two, and a revoke stops every one of them.
    let mut store = TokenStore::new();
    for issued in [100, 200] {
        store.records.push(TokenRecord {
            identity: "ada".to_string(),
            hash: format!("{issued:064}"),
            scope: TokenScope::HostWide,
            issued_at: moment(issued),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        });
    }

    assert_eq!(
        store.revoke("ada", None, moment(300)),
        vec![TokenScope::HostWide, TokenScope::HostWide]
    );
    assert_eq!(store.records[0].revoked_at, Some(moment(300)));
    assert_eq!(store.records[1].revoked_at, Some(moment(300)));
}

#[test]
fn a_grant_over_a_hand_written_pair_on_one_key_leaves_exactly_one_record() {
    let mut store = TokenStore::new();
    for issued in [100, 200] {
        store.records.push(TokenRecord {
            identity: "ada".to_string(),
            hash: format!("{issued:064}"),
            scope: TokenScope::HostWide,
            issued_at: moment(issued),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        });
    }

    let (_, replaced) = store.grant("ada".to_string(), TokenScope::HostWide, moment(300), None);

    assert!(replaced);
    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].issued_at, moment(300));
}

#[test]
fn every_grant_being_revoked_still_lists_all_of_them() {
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);
    store.revoke("ada", None, moment(200));

    let listed = store.entries(None);

    assert_eq!(
        listed,
        vec![TokenEntry {
            identity: "ada".to_string(),
            scope: TokenScope::HostWide,
            issued_at: moment(100),
            expires_at: None,
            last_used_at: None,
            revoked_at: Some(moment(200)),
        }]
    );
}

#[test]
fn a_store_whose_bytes_stop_part_way_is_refused_and_admits_nothing() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("tokens");
    std::fs::write(&path, br#"{"format":1,"records":[{"identity":"ad"#)
        .expect("write the truncated store");

    let error = TokenStore::read(&path).expect_err("a truncated store is refused");

    let IpcError::TokenStoreUnreadable {
        path: named,
        detail,
    } = error
    else {
        panic!("expected TokenStoreUnreadable, got {error:?}");
    };
    assert_eq!(named, path.display().to_string());
    assert_eq!(detail, "EOF while parsing a string at line 1 column 38");
}

#[test]
fn a_directory_where_the_store_belongs_is_refused_rather_than_read_as_empty() {
    // A missing store reads as empty. A directory is not missing, and it is
    // refused.
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("tokens");
    std::fs::create_dir(&path).expect("create a directory where the store belongs");

    let error = TokenStore::read(&path).expect_err("a directory is refused");

    let IpcError::TokenStoreUnreadable { path: named, .. } = error else {
        panic!("expected TokenStoreUnreadable, got {error:?}");
    };
    assert_eq!(named, path.display().to_string());
}

#[test]
fn a_store_holding_no_records_reads_back_as_a_store_holding_no_records() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("tokens");
    std::fs::write(&path, br#"{"format":1,"records":[]}"#).expect("write the empty store");

    let store = TokenStore::read(&path).expect("an empty record list reads");

    assert_eq!(store.format, TOKEN_STORE_FORMAT);
    assert_eq!(store.records, Vec::new());
    assert_eq!(store.entries(None), Vec::new());
}

#[test]
fn a_live_secret_is_admitted_with_the_scope_it_was_granted_on() {
    let session = SessionId::new();
    let (mut store, token) = granted(TokenScope::Session(session), None);

    assert_eq!(
        store.admit(&token, moment(200)),
        Some(TokenScope::Session(session))
    );
    assert_eq!(store.records[0].last_used_at, Some(moment(200)));
    assert!(TokenScope::Session(session).covers(session));
    assert!(!TokenScope::Session(session).covers(SessionId::new()));
}

#[test]
fn a_secret_no_record_holds_is_admitted_by_nothing() {
    let (mut store, _token) = granted(TokenScope::HostWide, None);

    assert_eq!(store.admit(&ConnectionToken::generate(), moment(200)), None);
    assert_eq!(store.records[0].last_used_at, None);
}

#[test]
fn a_revoked_secret_and_an_expired_one_are_admitted_by_nothing() {
    let (mut store, token) = granted(TokenScope::HostWide, None);
    store.revoke("ada", None, moment(150));
    assert_eq!(store.admit(&token, moment(200)), None);

    let (mut store, token) = granted(TokenScope::HostWide, Some(moment(150)));
    assert_eq!(store.admit(&token, moment(150)), None);
    assert_eq!(store.admit(&token, moment(149)), Some(TokenScope::HostWide));
}

#[test]
fn this_build_writes_token_store_format_one() {
    // A store written by this build carries format 1. A build that reads only
    // another number refuses this machine's grants.
    assert_eq!(TOKEN_STORE_FORMAT, 1);
}

// --- Whether a listed grant still stands ---

/// One listing row at `expires_at`, revoked at `revoked_at`.
fn entry_at(expires_at: Option<SystemTime>, revoked_at: Option<SystemTime>) -> TokenEntry {
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
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let earlier = now - Duration::from_secs(1);
    let later = now + Duration::from_secs(1);

    assert!(
        entry_at(None, None).is_live(now),
        "never expires, not revoked"
    );
    assert!(entry_at(Some(later), None).is_live(now), "expires later");

    assert!(!entry_at(Some(earlier), None).is_live(now), "expiry passed");
    assert!(
        !entry_at(Some(now), None).is_live(now),
        "the expiry instant itself is past: the check is `expiry > now`"
    );
    assert!(!entry_at(None, Some(earlier)).is_live(now), "revoked");
    assert!(
        !entry_at(Some(later), Some(earlier)).is_live(now),
        "revoked beats an expiry still ahead"
    );
}

#[test]
fn a_listed_grant_stands_exactly_when_the_record_it_came_from_does() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    for expires_at in [
        None,
        Some(now - Duration::from_secs(1)),
        Some(now + Duration::from_secs(1)),
    ] {
        for revoked_at in [None, Some(now - Duration::from_secs(1))] {
            let record = TokenRecord {
                identity: "alice".to_string(),
                hash: "a".repeat(64),
                scope: TokenScope::HostWide,
                issued_at: SystemTime::UNIX_EPOCH,
                expires_at,
                last_used_at: None,
                revoked_at,
            };
            assert_eq!(
                record.entry().is_live(now),
                record.is_live(now),
                "the row and its record answer alike for {expires_at:?} / {revoked_at:?}"
            );
        }
    }
}

/// A file written by hand can hold two records on one hash. The walk runs to
/// the end of the list. The last live record holding the secret is the one
/// that answers and the one that is stamped.
#[test]
fn the_last_live_record_holding_a_secret_is_the_one_that_answers() {
    let session = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    let token = ConnectionToken::generate();
    let mut store = TokenStore::new();
    for (scope, issued) in [
        (TokenScope::Session(session), 100),
        (TokenScope::HostWide, 200),
    ] {
        store.records.push(TokenRecord {
            identity: "ada".to_string(),
            hash: hash_token(&token),
            scope,
            issued_at: moment(issued),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        });
    }

    assert_eq!(store.admit(&token, moment(300)), Some(TokenScope::HostWide));
    assert_eq!(store.records[0].last_used_at, None);
    assert_eq!(store.records[1].last_used_at, Some(moment(300)));
}

/// The walk keeps only the records that still stand and reach the session
/// asked for. A record after it that is revoked or scoped elsewhere leaves
/// the earlier one answering.
#[test]
fn a_later_record_that_is_revoked_or_scoped_elsewhere_leaves_an_earlier_one_answering() {
    let wanted = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    let other = SessionId::from_uuid(uuid::Uuid::from_u128(2));
    let token = ConnectionToken::generate();
    let mut store = TokenStore::new();
    for (scope, revoked_at) in [
        (TokenScope::Session(wanted), None),
        (TokenScope::Session(other), None),
        (TokenScope::HostWide, Some(moment(250))),
    ] {
        store.records.push(TokenRecord {
            identity: "ada".to_string(),
            hash: hash_token(&token),
            scope,
            issued_at: moment(100),
            expires_at: None,
            last_used_at: None,
            revoked_at,
        });
    }

    assert_eq!(
        store.resolve(&token, wanted, moment(300)),
        Resolution::Admitted
    );
    assert_eq!(store.records[0].last_used_at, Some(moment(300)));
    assert_eq!(store.records[1].last_used_at, None);
    assert_eq!(store.records[2].last_used_at, None);
}

#[test]
fn one_identity_holding_several_scopes_lists_host_wide_first_then_sessions_by_id() {
    let first = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    let second = SessionId::from_uuid(uuid::Uuid::from_u128(2));
    let mut store = TokenStore::new();
    store.grant(
        "ada".to_string(),
        TokenScope::Session(second),
        moment(100),
        None,
    );
    store.grant(
        "ada".to_string(),
        TokenScope::Session(first),
        moment(200),
        None,
    );
    store.grant("ada".to_string(), TokenScope::HostWide, moment(300), None);

    let listed: Vec<TokenScope> = store
        .entries(None)
        .into_iter()
        .map(|entry| entry.scope)
        .collect();

    assert_eq!(
        listed,
        vec![
            TokenScope::HostWide,
            TokenScope::Session(first),
            TokenScope::Session(second),
        ]
    );
}

#[test]
fn a_store_holding_one_record_with_every_field_set_is_written_as_these_exact_bytes() {
    let dir = TempDir::new().expect("create temp dir");
    let path = store_path(dir.path());
    let store = TokenStore {
        format: TOKEN_STORE_FORMAT,
        records: vec![TokenRecord {
            identity: "ada".to_string(),
            hash: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string(),
            scope: TokenScope::Session(SessionId::from_uuid(uuid::Uuid::from_u128(1))),
            issued_at: moment(100),
            expires_at: Some(moment(900)),
            last_used_at: Some(moment(200)),
            revoked_at: Some(moment(300)),
        }],
    };

    store.write(&path).expect("write token store");

    assert_eq!(
        std::fs::read_to_string(&path).expect("read file bytes"),
        r#"{"format":1,"records":[{"identity":"ada","hash":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","scope":{"Session":"00000000-0000-0000-0000-000000000001"},"issued_at":{"secs_since_epoch":100,"nanos_since_epoch":0},"expires_at":{"secs_since_epoch":900,"nanos_since_epoch":0},"last_used_at":{"secs_since_epoch":200,"nanos_since_epoch":0},"revoked_at":{"secs_since_epoch":300,"nanos_since_epoch":0}}]}"#
    );
    assert_eq!(TokenStore::read(&path).expect("read token store"), store);
}

#[test]
fn hashing_an_empty_secret_gives_the_published_sha256_vector() {
    assert_eq!(
        hash_token(&ConnectionToken::new("")),
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
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("tokens");
    std::fs::write(
        &path,
        r#"{"format":1,"records":[{"identity":"ada","hash":"h","scope":"HostWide","issued_at":{"secs_since_epoch":100,"nanos_since_epoch":0},"expires_at":null,"last_used_at":null,"revoked_at":null,"extra":1}]}"#,
    )
    .expect("write file");

    let error = TokenStore::read(&path).expect_err("an unknown record field is refused");

    let IpcError::TokenStoreUnreadable {
        path: named,
        detail,
    } = error
    else {
        panic!("expected TokenStoreUnreadable, got {error:?}");
    };
    assert_eq!(named, path.display().to_string());
    assert_eq!(
        detail,
        "unknown field `extra`, expected one of `identity`, `hash`, `scope`, `issued_at`, \
         `expires_at`, `last_used_at`, `revoked_at` at line 1 column 193"
    );
}

#[test]
fn a_listed_grant_carrying_a_field_this_build_does_not_know_still_reads() {
    let listed = serde_json::from_str::<TokenEntry>(
        r#"{"identity":"ada","scope":"HostWide","issued_at":{"secs_since_epoch":100,"nanos_since_epoch":0},"expires_at":null,"last_used_at":null,"revoked_at":null,"extra":1}"#,
    )
    .expect("a listing row with an added field reads");

    assert_eq!(
        listed,
        TokenEntry {
            identity: "ada".to_string(),
            scope: TokenScope::HostWide,
            issued_at: moment(100),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        }
    );
}

#[test]
fn a_file_missing_its_record_list_is_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("tokens");
    std::fs::write(&path, r#"{"format":1}"#).expect("write file");

    let error = TokenStore::read(&path).expect_err("a store without records is refused");

    let IpcError::TokenStoreUnreadable {
        path: named,
        detail,
    } = error
    else {
        panic!("expected TokenStoreUnreadable, got {error:?}");
    };
    assert_eq!(named, path.display().to_string());
    assert_eq!(detail, "missing field `records` at line 1 column 12");
}

#[test]
fn a_file_whose_format_number_is_zero_is_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("tokens");
    std::fs::write(&path, r#"{"format":0,"records":[]}"#).expect("write file");

    let error = TokenStore::read(&path).expect_err("format 0 is refused");

    let IpcError::TokenStoreUnreadable {
        path: named,
        detail,
    } = error
    else {
        panic!("expected TokenStoreUnreadable, got {error:?}");
    };
    assert_eq!(named, path.display().to_string());
    assert_eq!(detail, "format 0 is not the 1 this build reads");
}

#[test]
fn a_scoped_revoke_naming_a_scope_the_identity_does_not_hold_stops_nothing() {
    let (mut store, token) = granted(TokenScope::HostWide, None);

    assert_eq!(
        store.revoke(
            "ada",
            Some(&TokenScope::Session(SessionId::new())),
            moment(300)
        ),
        Vec::<TokenScope>::new()
    );

    assert_eq!(store.records[0].revoked_at, None);
    assert_eq!(
        store.resolve(&token, SessionId::new(), moment(400)),
        Resolution::Admitted
    );
}

#[test]
fn a_bare_revoke_leaves_another_identitys_grants_standing() {
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);
    let (bobs, _) = store.grant("bob".to_string(), TokenScope::HostWide, moment(100), None);

    assert_eq!(
        store.revoke("ada", None, moment(300)),
        vec![TokenScope::HostWide]
    );

    assert_eq!(store.records[0].revoked_at, Some(moment(300)));
    assert_eq!(store.records[1].revoked_at, None);
    assert_eq!(
        store.resolve(&bobs, SessionId::new(), moment(400)),
        Resolution::Admitted
    );
}

#[test]
fn narrowing_to_a_scope_no_grant_reaches_lists_nothing() {
    let mut store = TokenStore::new();
    store.grant(
        "zoe".to_string(),
        TokenScope::Session(SessionId::from_uuid(uuid::Uuid::from_u128(1))),
        moment(100),
        None,
    );

    assert_eq!(
        store.entries(Some(&TokenScope::Session(SessionId::from_uuid(
            uuid::Uuid::from_u128(2)
        )))),
        Vec::new()
    );
    assert_eq!(store.entries(Some(&TokenScope::HostWide)), Vec::new());
}

#[test]
fn an_uppercase_identity_lists_before_a_lowercase_one() {
    let mut store = TokenStore::new();
    store.grant("ada".to_string(), TokenScope::HostWide, moment(100), None);
    store.grant("Bob".to_string(), TokenScope::HostWide, moment(200), None);

    let identities: Vec<String> = store
        .entries(None)
        .into_iter()
        .map(|entry| entry.identity)
        .collect();

    assert_eq!(identities, vec!["Bob".to_string(), "ada".to_string()]);
}

#[test]
fn a_fresh_grant_is_recorded_with_the_times_it_was_given_and_no_other() {
    let session = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    let mut store = TokenStore::new();

    let (token, replaced) = store.grant(
        "ada".to_string(),
        TokenScope::Session(session),
        moment(100),
        Some(moment(900)),
    );

    assert!(!replaced);
    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].hash, hash_token(&token));
    assert_eq!(
        store.records[0].entry(),
        TokenEntry {
            identity: "ada".to_string(),
            scope: TokenScope::Session(session),
            issued_at: moment(100),
            expires_at: Some(moment(900)),
            last_used_at: None,
            revoked_at: None,
        }
    );
}
