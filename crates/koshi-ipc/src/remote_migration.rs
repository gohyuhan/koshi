//! Convert format 1 remote files while their owning store is locked.

use std::path::Path;
use std::time::SystemTime;

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::error::{IpcError, RemoteFile};
use crate::protocol::ConnectionToken;
use crate::remote_servers::{SavedServer, ServerStore, SERVER_STORE_FORMAT};
use crate::remote_state::{
    build_unreadable_remote_file_error, CertFile, EnabledFile, CERT_FILE_FORMAT,
    ENABLED_FILE_FORMAT,
};
use crate::remote_tokens::{TokenRecord, TokenScope, TokenStore, TOKEN_STORE_FORMAT};

/// The previous certificate record.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousCertificate {
    format: u32,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

/// The previous remote access record.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousRemoteAccessRecord {
    format: u32,
    enabled_at: SystemTime,
}

/// One previous grant, with its original hash and timestamps.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousTokenRecord {
    identity: String,
    hash: String,
    scope: TokenScope,
    issued_at: SystemTime,
    expires_at: Option<SystemTime>,
    last_used_at: Option<SystemTime>,
    revoked_at: Option<SystemTime>,
}

/// The previous grant store.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousTokenStore {
    format: u32,
    records: Vec<PreviousTokenRecord>,
}

/// One previous saved server, with its original secret and certificate pin.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousSavedServer {
    name: Option<String>,
    address: String,
    secret: ConnectionToken,
    #[serde(default)]
    fingerprint: Option<String>,
    added_at: SystemTime,
    last_used_at: Option<SystemTime>,
}

/// The previous saved-server store.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousServerStore {
    format: u32,
    records: Vec<PreviousSavedServer>,
}

/// Read the format 1 file at `remote_file_path` that still needs converting.
///
/// Returns `Ok(None)` when no file is there, or when the file decodes as
/// `Current` at `expected_current_format`. Returns the decoded `Previous` when
/// the file decodes as `Previous` at format 1. Every other case is
/// [`IpcError::RemoteFileUnreadable`] on `remote_file`: an unreadable file, a
/// `Current` at another format, a `Previous` at a format other than 1, or bytes
/// that decode as neither.
fn load_previous_file<Current, Previous>(
    remote_file: RemoteFile,
    remote_file_path: &Path,
    read_current_file_format: impl FnOnce(&Current) -> u32,
    read_previous_file_format: impl FnOnce(&Previous) -> u32,
    expected_current_format: u32,
) -> Result<Option<Previous>, IpcError>
where
    Current: DeserializeOwned,
    Previous: DeserializeOwned,
{
    let file_bytes = match std::fs::read(remote_file_path) {
        Ok(file_bytes) => file_bytes,
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(read_error) => {
            return Err(build_unreadable_remote_file_error(
                remote_file,
                remote_file_path,
                read_error.to_string(),
            ));
        }
    };
    let build_refusal = |error_detail: String| {
        build_unreadable_remote_file_error(remote_file, remote_file_path, error_detail)
    };
    if let Ok(current_file) = serde_json::from_slice::<Current>(&file_bytes) {
        let found_format = read_current_file_format(&current_file);
        return if found_format == expected_current_format {
            Ok(None)
        } else {
            Err(build_refusal(format!(
                "format {found_format} is not the {expected_current_format} this build reads"
            )))
        };
    }
    let previous_file: Previous = serde_json::from_slice(&file_bytes)
        .map_err(|decode_error| build_refusal(decode_error.to_string()))?;
    let found_format = read_previous_file_format(&previous_file);
    if found_format != 1 {
        return Err(build_refusal(format!(
            "format {found_format} is not the 1 this migration reads"
        )));
    }
    Ok(Some(previous_file))
}

/// Convert the remote listener's certificate, remote access record, and token store.
///
/// Call this while the router holds its lock, before opening the listener.
/// Each file is replaced atomically; a retry accepts a file already at format 2.
/// A fault in one file leaves the other two eligible for conversion.
#[must_use]
pub fn migrate_remote_listener_files(data_directory: &Path) -> Vec<IpcError> {
    let mut migration_errors = Vec::new();
    for migrate_file in [
        migrate_certificate_file as fn(&Path) -> Result<(), IpcError>,
        migrate_remote_access_record_file,
        migrate_token_store_file,
    ] {
        if let Err(migration_error) = migrate_file(data_directory) {
            migration_errors.push(migration_error);
        }
    }
    migration_errors
}

/// Convert the certificate without changing its DER bytes.
fn migrate_certificate_file(data_directory: &Path) -> Result<(), IpcError> {
    let certificate_path = CertFile::resolve_certificate_file_path(data_directory);
    if let Some(previous_certificate) = load_previous_file::<CertFile, PreviousCertificate>(
        RemoteFile::Certificate,
        &certificate_path,
        |certificate| certificate.file_format,
        |certificate| certificate.format,
        CERT_FILE_FORMAT,
    )? {
        CertFile {
            file_format: CERT_FILE_FORMAT,
            cert_der: previous_certificate.cert_der,
            key_der: previous_certificate.key_der,
        }
        .write_to_path(&certificate_path)?;
    }
    Ok(())
}

/// Convert the remote access record, keeping its `enabled_at` time.
fn migrate_remote_access_record_file(data_directory: &Path) -> Result<(), IpcError> {
    let enabled_file_path = EnabledFile::resolve_enabled_file_path(data_directory);
    if let Some(previous_remote_access_record) =
        load_previous_file::<EnabledFile, PreviousRemoteAccessRecord>(
            RemoteFile::RemoteAccessRecord,
            &enabled_file_path,
            |enabled_file| enabled_file.file_format,
            |previous_remote_access_record| previous_remote_access_record.format,
            ENABLED_FILE_FORMAT,
        )?
    {
        EnabledFile {
            file_format: ENABLED_FILE_FORMAT,
            enabled_at: previous_remote_access_record.enabled_at,
        }
        .write_to_path(&enabled_file_path)?;
    }
    Ok(())
}

/// Convert grant hashes and their original timestamps.
fn migrate_token_store_file(data_directory: &Path) -> Result<(), IpcError> {
    let token_store_path = crate::remote_tokens::resolve_token_store_path(data_directory);
    if let Some(previous_token_store) = load_previous_token_store(&token_store_path)? {
        convert_previous_token_store(previous_token_store)
            .write_token_store_to_path(&token_store_path)?;
    }
    Ok(())
}

fn load_previous_token_store(
    token_store_path: &Path,
) -> Result<Option<PreviousTokenStore>, IpcError> {
    load_previous_file::<TokenStore, PreviousTokenStore>(
        RemoteFile::TokenStore,
        token_store_path,
        |token_store| token_store.store_format,
        |previous_token_store| previous_token_store.format,
        TOKEN_STORE_FORMAT,
    )
}

fn convert_previous_token_store(previous_token_store: PreviousTokenStore) -> TokenStore {
    TokenStore {
        store_format: TOKEN_STORE_FORMAT,
        token_records: previous_token_store
            .records
            .into_iter()
            .map(|previous_token_record| TokenRecord {
                identity: previous_token_record.identity,
                token_hash: previous_token_record.hash,
                scope: previous_token_record.scope,
                issued_at: previous_token_record.issued_at,
                expires_at: previous_token_record.expires_at,
                last_used_at: previous_token_record.last_used_at,
                revoked_at: previous_token_record.revoked_at,
            })
            .collect(),
    }
}

/// Read format 1 or 2 grants for diagnostics without changing the file.
///
/// # Errors
/// Reports an unreadable grant file, including an unsupported format.
pub fn load_token_store_for_diagnostics(data_directory: &Path) -> Result<TokenStore, IpcError> {
    let token_store_path = crate::remote_tokens::resolve_token_store_path(data_directory);
    let previous_token_store = load_previous_token_store(&token_store_path)?;
    match previous_token_store {
        Some(previous_token_store) => Ok(convert_previous_token_store(previous_token_store)),
        None => TokenStore::load_token_store_from_path(&token_store_path),
    }
}

/// Convert the saved-server file while its store lock is held.
///
/// # Errors
/// Reports an unreadable source or a failed atomic replacement.
pub fn migrate_saved_server_file(server_store_path: &Path) -> Result<(), IpcError> {
    if let Some(previous_server_store) = load_previous_file::<ServerStore, PreviousServerStore>(
        RemoteFile::SavedServers,
        server_store_path,
        |server_store| server_store.store_format,
        |previous_server_store| previous_server_store.format,
        SERVER_STORE_FORMAT,
    )? {
        ServerStore {
            store_format: SERVER_STORE_FORMAT,
            saved_servers: previous_server_store
                .records
                .into_iter()
                .map(|previous_saved_server| SavedServer {
                    server_name: previous_saved_server.name,
                    server_address: previous_saved_server.address,
                    connection_token: previous_saved_server.secret,
                    certificate_fingerprint: previous_saved_server.fingerprint,
                    added_at: previous_saved_server.added_at,
                    last_used_at: previous_saved_server.last_used_at,
                })
                .collect(),
        }
        .write_server_store_to_path(server_store_path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
