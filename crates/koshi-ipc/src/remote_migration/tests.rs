//! Tests for automatic conversion of format 1 remote files.

use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use super::*;

/// Write one previous-format JSON record to its private directory.
fn write_previous_file(file_path: &Path, file_contents: serde_json::Value) {
    std::fs::create_dir_all(file_path.parent().expect("remote file parent"))
        .expect("create remote directory");
    std::fs::write(
        file_path,
        serde_json::to_vec(&file_contents).expect("encode previous file"),
    )
    .expect("write previous file");
}

#[test]
fn listener_migration_preserves_certificate_access_mark_and_grant() {
    let test_directory = TempDir::new().expect("create data directory");
    let data_directory = test_directory.path();
    let certificate_path = CertFile::resolve_certificate_file_path(data_directory);
    let enabled_path = EnabledFile::resolve_enabled_file_path(data_directory);
    let token_store_path = crate::remote_tokens::resolve_token_store_path(data_directory);
    let enabled_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let issued_at = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
    let last_used_at = SystemTime::UNIX_EPOCH + Duration::from_secs(300);
    let connection_token = ConnectionToken::from_secret("released-token");
    let token_hash = crate::remote_tokens::hash_connection_token(&connection_token);

    write_previous_file(
        &certificate_path,
        json!({"format": 1, "cert_der": [1, 2, 3], "key_der": [4, 5, 6]}),
    );
    write_previous_file(
        &enabled_path,
        json!({"format": 1, "enabled_at": enabled_at}),
    );
    write_previous_file(
        &token_store_path,
        json!({"format": 1, "records": [{
            "identity": "ada", "hash": token_hash, "scope": "HostWide",
            "issued_at": issued_at, "expires_at": null,
            "last_used_at": last_used_at, "revoked_at": null
        }]}),
    );

    assert!(migrate_remote_listener_files(data_directory).is_empty());
    assert_eq!(
        CertFile::load_from_path(&certificate_path).expect("read certificate"),
        CertFile {
            file_format: CERT_FILE_FORMAT,
            cert_der: vec![1, 2, 3],
            key_der: vec![4, 5, 6],
        }
    );
    assert_eq!(
        EnabledFile::load_from_path(&enabled_path).expect("read remote access record"),
        EnabledFile {
            file_format: ENABLED_FILE_FORMAT,
            enabled_at,
        }
    );
    assert_eq!(
        TokenStore::load_token_store_from_path(&token_store_path).expect("read grants"),
        TokenStore {
            store_format: TOKEN_STORE_FORMAT,
            token_records: vec![TokenRecord {
                identity: "ada".to_string(),
                token_hash,
                scope: TokenScope::HostWide,
                issued_at,
                expires_at: None,
                last_used_at: Some(last_used_at),
                revoked_at: None,
            }],
        }
    );

    let mut migrated_grants =
        TokenStore::load_token_store_from_path(&token_store_path).expect("read migrated grants");
    let admitted_at = SystemTime::UNIX_EPOCH + Duration::from_secs(400);
    assert_eq!(
        migrated_grants.admit_token_scope(&connection_token, admitted_at),
        Some(TokenScope::HostWide)
    );
    assert_eq!(
        migrated_grants.token_records[0].last_used_at,
        Some(admitted_at)
    );

    let certificate_bytes = std::fs::read(&certificate_path).expect("read migrated certificate");
    assert!(migrate_remote_listener_files(data_directory).is_empty());
    assert_eq!(
        std::fs::read(&certificate_path).expect("read certificate again"),
        certificate_bytes
    );
}

#[test]
fn saved_server_migration_preserves_secret_pin_and_timestamps() {
    let test_directory = TempDir::new().expect("create data directory");
    let server_store_path = crate::remote_servers::resolve_server_store_path(test_directory.path());
    let added_at = SystemTime::UNIX_EPOCH + Duration::from_secs(400);
    let last_used_at = SystemTime::UNIX_EPOCH + Duration::from_secs(500);
    write_previous_file(
        &server_store_path,
        json!({"format": 1, "records": [{
            "name": "work", "address": "host.example:7777", "secret": "old-secret",
            "fingerprint": "cd".repeat(32), "added_at": added_at,
            "last_used_at": last_used_at
        }]}),
    );

    migrate_saved_server_file(&server_store_path).expect("migrate saved servers");
    assert_eq!(
        ServerStore::load_server_store_from_path(&server_store_path).expect("read saved servers"),
        ServerStore {
            store_format: SERVER_STORE_FORMAT,
            saved_servers: vec![SavedServer {
                server_name: Some("work".to_string()),
                server_address: "host.example:7777".to_string(),
                connection_token: ConnectionToken::from_secret("old-secret"),
                certificate_fingerprint: Some("cd".repeat(32)),
                added_at,
                last_used_at: Some(last_used_at),
            }],
        }
    );
}

#[test]
fn unsupported_previous_format_is_refused_without_replacing_file() {
    let test_directory = TempDir::new().expect("create data directory");
    let certificate_path = CertFile::resolve_certificate_file_path(test_directory.path());
    let previous_file = json!({"format": 7, "cert_der": [1], "key_der": [2]});
    write_previous_file(&certificate_path, previous_file.clone());

    let migration_errors = migrate_remote_listener_files(test_directory.path());
    assert_eq!(
        migration_errors
            .into_iter()
            .map(|migration_error| migration_error.to_string())
            .collect::<Vec<_>>(),
        vec![format!(
            "the remote access certificate at {} is unreadable: format 7 is not the 1 this migration reads",
            certificate_path.display()
        )]
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(&certificate_path).expect("read unchanged certificate")
        )
        .expect("decode unchanged certificate"),
        previous_file
    );
}

#[test]
fn unreadable_certificate_does_not_block_grant_migration() {
    let test_directory = TempDir::new().expect("create data directory");
    let certificate_path = CertFile::resolve_certificate_file_path(test_directory.path());
    let token_store_path = crate::remote_tokens::resolve_token_store_path(test_directory.path());
    write_previous_file(
        &certificate_path,
        json!({"format": 7, "cert_der": [1], "key_der": [2]}),
    );
    write_previous_file(&token_store_path, json!({"format": 1, "records": []}));

    let migration_errors = migrate_remote_listener_files(test_directory.path());
    assert_eq!(
        migration_errors
            .into_iter()
            .map(|migration_error| migration_error.to_string())
            .collect::<Vec<_>>(),
        vec![format!(
            "the remote access certificate at {} is unreadable: format 7 is not the 1 this migration reads",
            certificate_path.display()
        )]
    );
    assert_eq!(
        TokenStore::load_token_store_from_path(&token_store_path).expect("read converted grants"),
        TokenStore {
            store_format: TOKEN_STORE_FORMAT,
            token_records: Vec::new(),
        }
    );
}

#[test]
fn diagnostics_reads_previous_grants_without_changing_the_file() {
    let test_directory = TempDir::new().expect("create data directory");
    let token_store_path = crate::remote_tokens::resolve_token_store_path(test_directory.path());
    let issued_at = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
    let previous_store = json!({"format": 1, "records": [{
        "identity": "ada", "hash": "ab".repeat(32), "scope": "HostWide",
        "issued_at": issued_at, "expires_at": null,
        "last_used_at": null, "revoked_at": null
    }]});
    write_previous_file(&token_store_path, previous_store);
    let previous_bytes = std::fs::read(&token_store_path).expect("read previous grants");

    assert_eq!(
        load_token_store_for_diagnostics(test_directory.path()).expect("inspect previous grants"),
        TokenStore {
            store_format: TOKEN_STORE_FORMAT,
            token_records: vec![TokenRecord {
                identity: "ada".to_string(),
                token_hash: "ab".repeat(32),
                scope: TokenScope::HostWide,
                issued_at,
                expires_at: None,
                last_used_at: None,
                revoked_at: None,
            }],
        }
    );
    assert_eq!(
        std::fs::read(&token_store_path).expect("read grants after inspection"),
        previous_bytes
    );

    migrate_token_store_file(test_directory.path()).expect("migrate previous grants");
    assert_eq!(
        load_token_store_for_diagnostics(test_directory.path()).expect("inspect current grants"),
        TokenStore::load_token_store_from_path(&token_store_path).expect("read current grants")
    );
}
