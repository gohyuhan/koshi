//! The two files the router keeps beside its token store: the certificate
//! this machine presents to remote clients, and the record that the operator
//! switched remote access on.
//!
//! [`CertFile`](crate::remote_state::CertFile) holds a certificate koshi
//! generated itself, with its private key. There is no operator-supplied
//! certificate.
//!
//! [`EnabledFile`](crate::remote_state::EnabledFile) records that the operator
//! answered yes to opening the port. A listen address in `koshi.kdl` sets the
//! address and does not open the port. The port opens the first time the
//! operator answers yes, and on every start after that. The router is the only
//! writer of this file and of the token store.
//!
//! Both files sit inside the private koshi data directory, are restricted to
//! the owning user, and are replaced through
//! [`koshi_storage::atomic::write_atomic`]. The token store and the
//! saved-server store are written the same way, through the
//! `write_owner_only` this module holds, and each of the four files states a
//! format number this build does not read through the same `format_mismatch`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{IpcError, RemoteFile};

/// The format number this build writes into the certificate file, and the
/// only one it reads back.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::REMOTE_CERTIFICATE_FORMAT`].
pub const CERT_FILE_FORMAT: u32 = koshi_core::compat::REMOTE_CERTIFICATE_FORMAT.maximum_version;

/// The format number this build writes into the enabled file, and the only
/// one it reads back.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::REMOTE_ACCESS_MARK_FORMAT`].
pub const ENABLED_FILE_FORMAT: u32 = koshi_core::compat::REMOTE_ACCESS_MARK_FORMAT.maximum_version;

/// The certificate this machine presents to remote clients, and its private
/// key.
///
/// Decoding rejects any field it does not know; a misspelled field name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertFile {
    /// The format number of the file these bytes came from or go to.
    #[serde(rename = "format")]
    pub file_format: u32,
    /// The certificate, in DER form: the bytes a client fingerprints.
    pub cert_der: Vec<u8>,
    /// The certificate's private key, in DER form.
    pub key_der: Vec<u8>,
}

impl CertFile {
    /// Where the certificate file lives: `remote/cert` under `data_dir`.
    ///
    /// Callers resolve `data_directory` through `koshi_paths::resolve_data_directory()`.
    #[must_use]
    pub fn resolve_certificate_file_path(data_directory: &Path) -> PathBuf {
        data_directory.join("remote").join("cert")
    }

    /// Read the certificate file at `certificate_file_path`.
    ///
    /// A path with no file, a file that cannot be read, bytes that are not a
    /// readable certificate file, and a format number that is not
    /// [`CERT_FILE_FORMAT`] are all [`IpcError::RemoteFileUnreadable`].
    pub fn load_from_path(certificate_file_path: &Path) -> Result<CertFile, IpcError> {
        let certificate_file: CertFile =
            load_remote_file(RemoteFile::Certificate, certificate_file_path)?;
        if let Some(format_error) =
            find_format_mismatch(certificate_file.file_format, CERT_FILE_FORMAT)
        {
            return Err(build_unreadable_remote_file_error(
                RemoteFile::Certificate,
                certificate_file_path,
                format_error,
            ));
        }
        Ok(certificate_file)
    }

    /// Write this certificate file at `certificate_file_path`, replacing whatever is there.
    ///
    /// # Errors
    /// [`IpcError::RemoteFileWrite`] naming what failed.
    pub fn write_to_path(&self, certificate_file_path: &Path) -> Result<(), IpcError> {
        write_remote_file(RemoteFile::Certificate, certificate_file_path, self)
    }
}

/// The record that the operator switched remote access on.
///
/// Decoding rejects any field it does not know; a misspelled field name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnabledFile {
    /// The format number of the file this record came from or goes to.
    #[serde(rename = "format")]
    pub file_format: u32,
    /// When the operator answered yes.
    pub enabled_at: SystemTime,
}

impl EnabledFile {
    /// Where the enabled file lives: `remote/enabled` under `data_dir`.
    ///
    /// Callers resolve `data_directory` through `koshi_paths::resolve_data_directory()`.
    #[must_use]
    pub fn resolve_enabled_file_path(data_directory: &Path) -> PathBuf {
        data_directory.join("remote").join("enabled")
    }

    /// Read the enabled file at `enabled_file_path`.
    ///
    /// A path with no file, a file that cannot be read, bytes that are not a
    /// readable enabled file, and a format number that is not
    /// [`ENABLED_FILE_FORMAT`] are all [`IpcError::RemoteFileUnreadable`].
    pub fn load_from_path(enabled_file_path: &Path) -> Result<EnabledFile, IpcError> {
        let enabled_file: EnabledFile =
            load_remote_file(RemoteFile::RemoteAccessMark, enabled_file_path)?;
        if let Some(format_error) =
            find_format_mismatch(enabled_file.file_format, ENABLED_FILE_FORMAT)
        {
            return Err(build_unreadable_remote_file_error(
                RemoteFile::RemoteAccessMark,
                enabled_file_path,
                format_error,
            ));
        }
        Ok(enabled_file)
    }

    /// Write this enabled file at `enabled_file_path`, replacing whatever is there.
    ///
    /// # Errors
    /// [`IpcError::RemoteFileWrite`] naming what failed.
    pub fn write_to_path(&self, enabled_file_path: &Path) -> Result<(), IpcError> {
        write_remote_file(RemoteFile::RemoteAccessMark, enabled_file_path, self)
    }
}

/// Whether the operator has switched remote access on for the koshi data
/// directory at `data_dir`: whether the enabled file reads.
#[must_use]
pub fn is_remote_enabled(data_directory: &Path) -> bool {
    EnabledFile::load_from_path(&EnabledFile::resolve_enabled_file_path(data_directory)).is_ok()
}

/// The reason `found` is not the format number this build reads, or `None`
/// when it is that number.
///
/// Example — `found` 2 against `expected` 1 gives `Some("format 2 is not the
/// 1 this build reads")`.
pub(crate) fn find_format_mismatch(found_format: u32, expected_format: u32) -> Option<String> {
    (found_format != expected_format)
        .then(|| format!("format {found_format} is not the {expected_format} this build reads"))
}

/// A file under `remote/` that could not be used, named in plain words.
pub(crate) fn build_unreadable_remote_file_error(
    remote_file: RemoteFile,
    remote_file_path: &Path,
    error_detail: String,
) -> IpcError {
    IpcError::RemoteFileUnreadable {
        remote_file,
        remote_file_path: remote_file_path.display().to_string(),
        error_detail,
    }
}

/// Read and decode the JSON file at `path`. A path with no file is
/// [`IpcError::RemoteFileUnreadable`] on `file`, as is a file that cannot be
/// read or decoded.
fn load_remote_file<FileContents: DeserializeOwned>(
    remote_file: RemoteFile,
    remote_file_path: &Path,
) -> Result<FileContents, IpcError> {
    let private_file_bytes = std::fs::read(remote_file_path).map_err(|io_error| {
        build_unreadable_remote_file_error(remote_file, remote_file_path, io_error.to_string())
    })?;
    serde_json::from_slice(&private_file_bytes).map_err(|decode_error| {
        build_unreadable_remote_file_error(remote_file, remote_file_path, decode_error.to_string())
    })
}

/// Encode `value` and write it at `path` as a file only the owning user
/// reaches, naming the failure as [`IpcError::RemoteFileWrite`] on `file`.
pub(crate) fn write_remote_file<FileContents: Serialize>(
    remote_file: RemoteFile,
    remote_file_path: &Path,
    serializable_contents: &FileContents,
) -> Result<(), IpcError> {
    write_owner_only(remote_file_path, serializable_contents).map_err(|error_detail| {
        IpcError::RemoteFileWrite {
            remote_file,
            remote_file_path: remote_file_path.display().to_string(),
            error_detail,
        }
    })
}

/// Encode `value` and write it at `path`, replacing whatever is there, and
/// create the directory holding it when it is missing.
///
/// The file is restricted to the owning user: mode `0600` on Unix, set on an
/// existing file before the replace; the new file carries it too. On Windows
/// the file takes the data directory's owner-scoped ACLs. The directory itself
/// gets mode `0700` on Unix. A `path` with no directory part creates no
/// directory.
///
/// # Errors
/// The text of the first step that failed: creating the directory, setting a
/// mode, encoding `value`, or replacing the file.
pub(crate) fn write_owner_only<FileContents: Serialize>(
    file_path: &Path,
    serializable_contents: &FileContents,
) -> Result<(), String> {
    if let Some(parent) = file_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| error.to_string())?;
        }
    }
    #[cfg(unix)]
    if file_path.exists() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
    }
    let serialized_file_bytes = serde_json::to_vec(serializable_contents)
        .map_err(|serialization_error| serialization_error.to_string())?;
    koshi_storage::atomic::write_atomic(file_path, &serialized_file_bytes)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests;
