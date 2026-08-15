//! The two files the router keeps beside its token store: the certificate
//! this machine presents to remote clients, and the record that the operator
//! switched remote access on.
//!
//! [`CertFile`](crate::remote_state::CertFile) holds a certificate koshi
//! generated itself, with its private key. There is no operator-supplied
//! certificate.
//!
//! [`EnabledFile`](crate::remote_state::EnabledFile) is the durable record of
//! the operator's one yes. A listen address in `koshi.kdl` sets the address; it
//! does not open the port. The port opens the first time the operator answers
//! yes to the offer, and on every start after that. The router is this file's
//! only writer, as it is the token store's.
//!
//! Both files sit inside the private koshi data directory, are restricted to
//! the owning user, and are replaced through
//! [`koshi_storage::atomic::write_atomic`].

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
pub const CERT_FILE_FORMAT: u32 = koshi_core::compat::REMOTE_CERTIFICATE_FORMAT.max;

/// The format number this build writes into the enabled file, and the only
/// one it reads back.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::REMOTE_ACCESS_MARK_FORMAT`].
pub const ENABLED_FILE_FORMAT: u32 = koshi_core::compat::REMOTE_ACCESS_MARK_FORMAT.max;

/// The certificate this machine presents to remote clients, and its private
/// key.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertFile {
    /// The format number of the file these bytes came from or go to.
    pub format: u32,
    /// The certificate, in DER form: the bytes a client fingerprints.
    pub cert_der: Vec<u8>,
    /// The certificate's private key, in DER form.
    pub key_der: Vec<u8>,
}

impl CertFile {
    /// Where the certificate file lives: `remote/cert` under `data_dir`.
    ///
    /// Callers resolve `data_dir` through `koshi_paths::data_dir()`.
    #[must_use]
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("remote").join("cert")
    }

    /// Read the certificate file at `path`.
    ///
    /// A path with no file, a file that cannot be read, bytes that are not a
    /// readable certificate file, and a format number that is not
    /// [`CERT_FILE_FORMAT`] are all [`IpcError::RemoteFileUnreadable`].
    pub fn read(path: &Path) -> Result<CertFile, IpcError> {
        let file: CertFile = read_private(RemoteFile::Certificate, path)?;
        if file.format != CERT_FILE_FORMAT {
            return Err(unreadable(
                RemoteFile::Certificate,
                path,
                format!(
                    "format {} is not the {CERT_FILE_FORMAT} this build reads",
                    file.format
                ),
            ));
        }
        Ok(file)
    }

    /// Write this certificate file at `path`, replacing whatever is there.
    ///
    /// # Errors
    /// [`IpcError::RemoteFileWrite`] naming what failed.
    pub fn write(&self, path: &Path) -> Result<(), IpcError> {
        write_private(RemoteFile::Certificate, path, self)
    }
}

/// The record that the operator switched remote access on.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnabledFile {
    /// The format number of the file this record came from or goes to.
    pub format: u32,
    /// When the operator answered yes.
    pub enabled_at: SystemTime,
}

impl EnabledFile {
    /// Where the enabled file lives: `remote/enabled` under `data_dir`.
    ///
    /// Callers resolve `data_dir` through `koshi_paths::data_dir()`.
    #[must_use]
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("remote").join("enabled")
    }

    /// Read the enabled file at `path`.
    ///
    /// A path with no file, a file that cannot be read, bytes that are not a
    /// readable enabled file, and a format number that is not
    /// [`ENABLED_FILE_FORMAT`] are all [`IpcError::RemoteFileUnreadable`].
    pub fn read(path: &Path) -> Result<EnabledFile, IpcError> {
        let file: EnabledFile = read_private(RemoteFile::RemoteAccessMark, path)?;
        if file.format != ENABLED_FILE_FORMAT {
            return Err(unreadable(
                RemoteFile::RemoteAccessMark,
                path,
                format!(
                    "format {} is not the {ENABLED_FILE_FORMAT} this build reads",
                    file.format
                ),
            ));
        }
        Ok(file)
    }

    /// Write this enabled file at `path`, replacing whatever is there.
    ///
    /// # Errors
    /// [`IpcError::RemoteFileWrite`] naming what failed.
    pub fn write(&self, path: &Path) -> Result<(), IpcError> {
        write_private(RemoteFile::RemoteAccessMark, path, self)
    }
}

/// Whether the operator has switched remote access on for the koshi data
/// directory at `data_dir`: whether the enabled file reads.
#[must_use]
pub fn remote_enabled(data_dir: &Path) -> bool {
    EnabledFile::read(&EnabledFile::path(data_dir)).is_ok()
}

/// A file under `remote/` that could not be used, named in plain words.
fn unreadable(file: RemoteFile, path: &Path, detail: String) -> IpcError {
    IpcError::RemoteFileUnreadable {
        file,
        path: path.display().to_string(),
        detail,
    }
}

/// Read and decode the JSON file at `path`. A path with no file is an error:
/// each of these files means something by existing.
fn read_private<T: DeserializeOwned>(file: RemoteFile, path: &Path) -> Result<T, IpcError> {
    let data = std::fs::read(path).map_err(|error| unreadable(file, path, error.to_string()))?;
    serde_json::from_slice(&data).map_err(|error| unreadable(file, path, error.to_string()))
}

/// Encode `value` and write it at `path`, replacing whatever is there, and
/// create the directory holding it when it is missing.
///
/// The file is restricted to the owning user: mode `0600` on Unix, set on an
/// existing file before the replace so the new file carries it too. On Windows
/// the file takes the data directory's owner-scoped ACLs. The directory itself
/// gets mode `0700` on Unix.
fn write_private<T: Serialize>(file: RemoteFile, path: &Path, value: &T) -> Result<(), IpcError> {
    let write_failed = |detail: String| IpcError::RemoteFileWrite {
        file,
        path: path.display().to_string(),
        detail,
    };
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| write_failed(error.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| write_failed(error.to_string()))?;
        }
    }
    #[cfg(unix)]
    if path.exists() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| write_failed(error.to_string()))?;
    }
    let data = serde_json::to_vec(value).map_err(|error| write_failed(error.to_string()))?;
    koshi_storage::atomic::write_atomic(path, &data)
        .map_err(|error| write_failed(error.to_string()))
}

#[cfg(test)]
mod tests;
