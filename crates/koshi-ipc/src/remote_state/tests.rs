//! Tests for the certificate file and the enabled file: where they live, the
//! write/read roundtrip, the private mode of the file, the refused format
//! number, and what `remote_enabled` answers.

use std::time::Duration;

use tempfile::TempDir;

use super::*;

/// A fixed point on the clock, `secs` seconds after the epoch.
fn moment(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// A certificate file holding two short stand-in byte strings.
fn cert_file() -> CertFile {
    CertFile {
        format: CERT_FILE_FORMAT,
        cert_der: vec![1, 2, 3, 4],
        key_der: vec![5, 6, 7, 8],
    }
}

#[test]
fn the_two_files_live_under_remote_in_the_data_dir() {
    let data_dir = Path::new("/home/ada/.local/share/koshi");
    assert_eq!(
        CertFile::path(data_dir),
        Path::new("/home/ada/.local/share/koshi/remote/cert")
    );
    assert_eq!(
        EnabledFile::path(data_dir),
        Path::new("/home/ada/.local/share/koshi/remote/enabled")
    );
}

#[test]
fn a_written_certificate_file_reads_back_the_same() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = CertFile::path(dir.path());
    let file = cert_file();
    file.write(&path).expect("write the certificate file");
    assert_eq!(CertFile::read(&path).expect("read it back"), file);
}

#[cfg(unix)]
#[test]
fn the_written_file_and_its_directory_are_private_to_the_owner() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("make a temp dir");
    let path = CertFile::path(dir.path());
    cert_file()
        .write(&path)
        .expect("write the certificate file");

    let mode_of = |path: &Path| {
        std::fs::metadata(path)
            .expect("stat path")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode_of(&path), 0o600);
    assert_eq!(mode_of(path.parent().expect("the remote directory")), 0o700);
}

#[test]
fn a_certificate_file_at_another_format_number_is_refused() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = CertFile::path(dir.path());
    let mut file = cert_file();
    file.format = CERT_FILE_FORMAT + 1;
    file.write(&path).expect("write the certificate file");
    let failure = CertFile::read(&path).expect_err("another format number is refused");
    assert_eq!(
        failure.to_string(),
        format!(
            "the remote access certificate at {} is unreadable: format {} is not the \
             {CERT_FILE_FORMAT} this build reads",
            path.display(),
            CERT_FILE_FORMAT + 1
        )
    );
}

#[test]
fn a_missing_certificate_file_is_an_error() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = CertFile::path(dir.path());
    let failure = CertFile::read(&path).expect_err("a missing certificate file is an error");
    let IpcError::RemoteFileUnreadable {
        file, path: named, ..
    } = failure
    else {
        panic!("a missing certificate file names the certificate");
    };
    assert_eq!(file, RemoteFile::Certificate);
    assert_eq!(named, path.display().to_string());
}

#[test]
fn remote_access_is_off_until_the_enabled_file_is_written() {
    let dir = TempDir::new().expect("make a temp dir");
    assert!(!remote_enabled(dir.path()));

    let file = EnabledFile {
        format: ENABLED_FILE_FORMAT,
        enabled_at: moment(1_000),
    };
    file.write(&EnabledFile::path(dir.path()))
        .expect("write the enabled file");
    assert!(remote_enabled(dir.path()));
    assert_eq!(
        EnabledFile::read(&EnabledFile::path(dir.path())).expect("read it back"),
        file
    );
}

#[test]
fn an_enabled_file_at_another_format_number_leaves_remote_access_off() {
    let dir = TempDir::new().expect("make a temp dir");
    EnabledFile {
        format: ENABLED_FILE_FORMAT + 1,
        enabled_at: moment(1_000),
    }
    .write(&EnabledFile::path(dir.path()))
    .expect("write the enabled file");
    assert!(!remote_enabled(dir.path()));
}
