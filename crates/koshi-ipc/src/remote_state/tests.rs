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

#[test]
fn an_enabled_file_at_another_format_number_is_refused_naming_the_record() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = EnabledFile::path(dir.path());
    EnabledFile {
        format: ENABLED_FILE_FORMAT + 1,
        enabled_at: moment(1_000),
    }
    .write(&path)
    .expect("write the enabled file");

    let failure = EnabledFile::read(&path).expect_err("another format number is refused");
    assert_eq!(
        failure.to_string(),
        format!(
            "the remote access record at {} is unreadable: format {} is not the \
             {ENABLED_FILE_FORMAT} this build reads",
            path.display(),
            ENABLED_FILE_FORMAT + 1
        )
    );
}

#[test]
fn a_missing_enabled_file_is_an_error_naming_the_record() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = EnabledFile::path(dir.path());

    let failure = EnabledFile::read(&path).expect_err("a missing enabled file is an error");
    let IpcError::RemoteFileUnreadable {
        file, path: named, ..
    } = failure
    else {
        panic!("a missing enabled file names the record: {failure}");
    };
    assert_eq!(file, RemoteFile::RemoteAccessMark);
    assert_eq!(named, path.display().to_string());
}

#[test]
fn junk_bytes_in_the_enabled_file_leave_remote_access_off() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = EnabledFile::path(dir.path());
    std::fs::create_dir_all(path.parent().expect("the remote directory")).expect("make it");
    std::fs::write(&path, b"yes").expect("write junk");

    assert!(!remote_enabled(dir.path()));
    let failure = EnabledFile::read(&path).expect_err("junk is refused");
    let detail = serde_json::from_slice::<EnabledFile>(b"yes")
        .expect_err("junk does not decode")
        .to_string();
    assert_eq!(
        failure.to_string(),
        format!(
            "the remote access record at {} is unreadable: {detail}",
            path.display()
        )
    );
}

#[test]
fn junk_bytes_are_an_unreadable_certificate() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = CertFile::path(dir.path());
    std::fs::create_dir_all(path.parent().expect("the remote directory")).expect("make it");
    std::fs::write(&path, b"-----BEGIN CERTIFICATE-----").expect("write junk");

    let failure = CertFile::read(&path).expect_err("junk is refused");
    let detail = serde_json::from_slice::<CertFile>(b"-----BEGIN CERTIFICATE-----")
        .expect_err("junk does not decode")
        .to_string();
    assert_eq!(
        failure.to_string(),
        format!(
            "the remote access certificate at {} is unreadable: {detail}",
            path.display()
        )
    );
}

#[test]
fn a_certificate_file_carrying_an_unknown_field_is_unreadable() {
    let bytes =
        format!(r#"{{"issuer":"ada","format":{CERT_FILE_FORMAT},"cert_der":[],"key_der":[]}}"#);

    let failure = serde_json::from_str::<CertFile>(&bytes).expect_err("refused");
    assert_eq!(
        failure.to_string(),
        "unknown field `issuer`, expected one of `format`, `cert_der`, `key_der` at line 1 \
         column 9"
    );
}

#[test]
fn an_enabled_file_carrying_an_unknown_field_is_unreadable() {
    let bytes = format!(
        r#"{{"by":"ada","format":{ENABLED_FILE_FORMAT},"enabled_at":{{"secs_since_epoch":1000,"nanos_since_epoch":0}}}}"#
    );

    let failure = serde_json::from_str::<EnabledFile>(&bytes).expect_err("refused");
    assert_eq!(
        failure.to_string(),
        "unknown field `by`, expected `format` or `enabled_at` at line 1 column 5"
    );
}

#[test]
fn a_directory_where_the_certificate_belongs_is_unreadable() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = CertFile::path(dir.path());
    std::fs::create_dir_all(&path).expect("make a directory at the certificate path");

    let failure = CertFile::read(&path).expect_err("a directory is not a certificate file");
    let IpcError::RemoteFileUnreadable {
        file, path: named, ..
    } = failure
    else {
        panic!("a directory at the certificate path names the certificate: {failure}");
    };
    assert_eq!(file, RemoteFile::Certificate);
    assert_eq!(named, path.display().to_string());
}

#[test]
fn writing_where_the_directory_cannot_exist_names_the_file_that_failed() {
    let dir = TempDir::new().expect("make a temp dir");
    std::fs::write(dir.path().join("remote"), b"a file, not a directory").expect("write it");

    let cert_path = CertFile::path(dir.path());
    let failure = cert_file()
        .write(&cert_path)
        .expect_err("a file in the directory's place stops the write");
    let IpcError::RemoteFileWrite {
        file, path: named, ..
    } = failure
    else {
        panic!("a failed write names the certificate: {failure}");
    };
    assert_eq!(file, RemoteFile::Certificate);
    assert_eq!(named, cert_path.display().to_string());

    let enabled_path = EnabledFile::path(dir.path());
    let failure = EnabledFile {
        format: ENABLED_FILE_FORMAT,
        enabled_at: moment(1_000),
    }
    .write(&enabled_path)
    .expect_err("a file in the directory's place stops the write");
    let IpcError::RemoteFileWrite {
        file, path: named, ..
    } = failure
    else {
        panic!("a failed write names the record: {failure}");
    };
    assert_eq!(file, RemoteFile::RemoteAccessMark);
    assert_eq!(named, enabled_path.display().to_string());
    assert!(!remote_enabled(dir.path()));
}

#[cfg(unix)]
#[test]
fn a_certificate_file_that_was_group_readable_is_private_after_the_write() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("make a temp dir");
    let path = CertFile::path(dir.path());
    cert_file()
        .write(&path)
        .expect("write the certificate file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("open the file up");

    cert_file()
        .write(&path)
        .expect("write the certificate file again");

    let mode = std::fs::metadata(&path)
        .expect("stat the certificate file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn the_two_files_are_written_as_these_exact_bytes() {
    let dir = TempDir::new().expect("make a temp dir");
    cert_file()
        .write(&CertFile::path(dir.path()))
        .expect("write the certificate file");
    EnabledFile {
        format: ENABLED_FILE_FORMAT,
        enabled_at: moment(1_000),
    }
    .write(&EnabledFile::path(dir.path()))
    .expect("write the enabled file");

    assert_eq!(
        std::fs::read_to_string(CertFile::path(dir.path())).expect("read the certificate file"),
        format!(r#"{{"format":{CERT_FILE_FORMAT},"cert_der":[1,2,3,4],"key_der":[5,6,7,8]}}"#)
    );
    assert_eq!(
        std::fs::read_to_string(EnabledFile::path(dir.path())).expect("read the enabled file"),
        format!(
            r#"{{"format":{ENABLED_FILE_FORMAT},"enabled_at":{{"secs_since_epoch":1000,"nanos_since_epoch":0}}}}"#
        )
    );
}

#[test]
fn writing_the_enabled_file_again_replaces_the_time_it_holds() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = EnabledFile::path(dir.path());
    EnabledFile {
        format: ENABLED_FILE_FORMAT,
        enabled_at: moment(1_000),
    }
    .write(&path)
    .expect("write the enabled file");
    let second = EnabledFile {
        format: ENABLED_FILE_FORMAT,
        enabled_at: moment(2_000),
    };

    second.write(&path).expect("write it again");

    assert_eq!(EnabledFile::read(&path).expect("read it back"), second);
}
