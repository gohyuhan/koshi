//! Tests for the certificate file and the enabled file: where they live, the
//! write/read roundtrip, the private mode of the file, the refused format
//! number, and what `is_remote_enabled` answers.

use std::time::Duration;

use tempfile::TempDir;

use super::*;

/// A fixed point on the clock, measured in seconds after the epoch.
fn moment(seconds_since_epoch: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds_since_epoch)
}

/// A certificate file holding two short stand-in byte strings.
fn cert_file() -> CertFile {
    CertFile {
        file_format: CERT_FILE_FORMAT,
        cert_der: vec![1, 2, 3, 4],
        key_der: vec![5, 6, 7, 8],
    }
}

#[test]
fn the_two_files_live_under_remote_in_the_data_dir() {
    let data_directory = Path::new("/home/ada/.local/share/koshi");
    assert_eq!(
        CertFile::resolve_certificate_file_path(data_directory),
        Path::new("/home/ada/.local/share/koshi/remote/cert")
    );
    assert_eq!(
        EnabledFile::resolve_enabled_file_path(data_directory),
        Path::new("/home/ada/.local/share/koshi/remote/enabled")
    );
}

#[test]
fn a_written_certificate_file_reads_back_the_same() {
    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = CertFile::resolve_certificate_file_path(test_directory.path());
    let certificate_file = cert_file();
    certificate_file
        .write_to_path(&remote_file_path)
        .expect("write the certificate file");
    assert_eq!(
        CertFile::load_from_path(&remote_file_path).expect("read it back"),
        certificate_file
    );
}

#[cfg(unix)]
#[test]
fn the_written_file_and_its_directory_are_private_to_the_owner() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = CertFile::resolve_certificate_file_path(test_directory.path());
    cert_file()
        .write_to_path(&remote_file_path)
        .expect("write the certificate file");

    let compute_file_mode = |file_path: &Path| {
        std::fs::metadata(file_path)
            .expect("stat file path")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(compute_file_mode(&remote_file_path), 0o600);
    assert_eq!(
        compute_file_mode(remote_file_path.parent().expect("the remote directory")),
        0o700
    );
}

#[test]
fn a_certificate_file_at_another_format_number_is_refused() {
    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = CertFile::resolve_certificate_file_path(test_directory.path());
    let mut certificate_file = cert_file();
    certificate_file.file_format = CERT_FILE_FORMAT + 1;
    certificate_file
        .write_to_path(&remote_file_path)
        .expect("write the certificate file");
    let format_error =
        CertFile::load_from_path(&remote_file_path).expect_err("another format number is refused");
    assert_eq!(
        format_error.to_string(),
        format!(
            "the remote access certificate at {} is unreadable: format {} is not the \
             {CERT_FILE_FORMAT} this build reads",
            remote_file_path.display(),
            CERT_FILE_FORMAT + 1
        )
    );
}

#[test]
fn a_missing_certificate_file_is_an_error() {
    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = CertFile::resolve_certificate_file_path(test_directory.path());
    let missing_certificate_error = CertFile::load_from_path(&remote_file_path)
        .expect_err("a missing certificate file is an error");
    let IpcError::RemoteFileUnreadable {
        remote_file,
        remote_file_path: reported_file_path,
        ..
    } = missing_certificate_error
    else {
        panic!("a missing certificate file names the certificate");
    };
    assert_eq!(remote_file, RemoteFile::Certificate);
    assert_eq!(reported_file_path, remote_file_path.display().to_string());
}

#[test]
fn remote_access_is_off_until_the_enabled_file_is_written() {
    let test_directory = TempDir::new().expect("make a test directory");
    assert!(!is_remote_enabled(test_directory.path()));

    let enabled_file = EnabledFile {
        file_format: ENABLED_FILE_FORMAT,
        enabled_at: moment(1_000),
    };
    enabled_file
        .write_to_path(&EnabledFile::resolve_enabled_file_path(
            test_directory.path(),
        ))
        .expect("write the enabled file");
    assert!(is_remote_enabled(test_directory.path()));
    assert_eq!(
        EnabledFile::load_from_path(&EnabledFile::resolve_enabled_file_path(
            test_directory.path()
        ))
        .expect("read it back"),
        enabled_file
    );
}

#[test]
fn an_enabled_file_at_another_format_number_leaves_remote_access_off() {
    let test_directory = TempDir::new().expect("make a test directory");
    EnabledFile {
        file_format: ENABLED_FILE_FORMAT + 1,
        enabled_at: moment(1_000),
    }
    .write_to_path(&EnabledFile::resolve_enabled_file_path(
        test_directory.path(),
    ))
    .expect("write the enabled file");
    assert!(!is_remote_enabled(test_directory.path()));
}

#[test]
fn an_enabled_file_at_another_format_number_is_refused_naming_the_record() {
    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = EnabledFile::resolve_enabled_file_path(test_directory.path());
    EnabledFile {
        file_format: ENABLED_FILE_FORMAT + 1,
        enabled_at: moment(1_000),
    }
    .write_to_path(&remote_file_path)
    .expect("write the enabled file");

    let format_error = EnabledFile::load_from_path(&remote_file_path)
        .expect_err("another format number is refused");
    assert_eq!(
        format_error.to_string(),
        format!(
            "the remote access record at {} is unreadable: format {} is not the \
             {ENABLED_FILE_FORMAT} this build reads",
            remote_file_path.display(),
            ENABLED_FILE_FORMAT + 1
        )
    );
}

#[test]
fn a_missing_enabled_file_is_an_error_naming_the_record() {
    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = EnabledFile::resolve_enabled_file_path(test_directory.path());

    let missing_enabled_file_error = EnabledFile::load_from_path(&remote_file_path)
        .expect_err("a missing enabled file is an error");
    let IpcError::RemoteFileUnreadable {
        remote_file,
        remote_file_path: reported_file_path,
        ..
    } = missing_enabled_file_error
    else {
        panic!("a missing enabled file names the record: {missing_enabled_file_error}");
    };
    assert_eq!(remote_file, RemoteFile::RemoteAccessMark);
    assert_eq!(reported_file_path, remote_file_path.display().to_string());
}

#[test]
fn junk_bytes_in_the_enabled_file_leave_remote_access_off() {
    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = EnabledFile::resolve_enabled_file_path(test_directory.path());
    std::fs::create_dir_all(remote_file_path.parent().expect("the remote directory"))
        .expect("make it");
    std::fs::write(&remote_file_path, b"yes").expect("write junk");

    assert!(!is_remote_enabled(test_directory.path()));
    let unreadable_enabled_file_error =
        EnabledFile::load_from_path(&remote_file_path).expect_err("junk is refused");
    let error_detail = serde_json::from_slice::<EnabledFile>(b"yes")
        .expect_err("junk does not decode")
        .to_string();
    assert_eq!(
        unreadable_enabled_file_error.to_string(),
        format!(
            "the remote access record at {} is unreadable: {error_detail}",
            remote_file_path.display()
        )
    );
}

#[test]
fn junk_bytes_are_an_unreadable_certificate() {
    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = CertFile::resolve_certificate_file_path(test_directory.path());
    std::fs::create_dir_all(remote_file_path.parent().expect("the remote directory"))
        .expect("make it");
    std::fs::write(&remote_file_path, b"-----BEGIN CERTIFICATE-----").expect("write junk");

    let unreadable_certificate_error =
        CertFile::load_from_path(&remote_file_path).expect_err("junk is refused");
    let error_detail = serde_json::from_slice::<CertFile>(b"-----BEGIN CERTIFICATE-----")
        .expect_err("junk does not decode")
        .to_string();
    assert_eq!(
        unreadable_certificate_error.to_string(),
        format!(
            "the remote access certificate at {} is unreadable: {error_detail}",
            remote_file_path.display()
        )
    );
}

#[test]
fn a_certificate_file_carrying_an_unknown_field_is_unreadable() {
    let certificate_json =
        format!(r#"{{"issuer":"ada","format":{CERT_FILE_FORMAT},"cert_der":[],"key_der":[]}}"#);

    let unknown_field_error =
        serde_json::from_str::<CertFile>(&certificate_json).expect_err("refused");
    assert_eq!(
        unknown_field_error.to_string(),
        "unknown field `issuer`, expected one of `format`, `cert_der`, `key_der` at line 1 \
         column 9"
    );
}

#[test]
fn an_enabled_file_carrying_an_unknown_field_is_unreadable() {
    let enabled_file_json = format!(
        r#"{{"by":"ada","format":{ENABLED_FILE_FORMAT},"enabled_at":{{"secs_since_epoch":1000,"nanos_since_epoch":0}}}}"#
    );

    let unknown_field_error =
        serde_json::from_str::<EnabledFile>(&enabled_file_json).expect_err("refused");
    assert_eq!(
        unknown_field_error.to_string(),
        "unknown field `by`, expected `format` or `enabled_at` at line 1 column 5"
    );
}

#[test]
fn a_directory_where_the_certificate_belongs_is_unreadable() {
    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = CertFile::resolve_certificate_file_path(test_directory.path());
    std::fs::create_dir_all(&remote_file_path).expect("make a directory at the certificate path");

    let directory_at_certificate_path_error = CertFile::load_from_path(&remote_file_path)
        .expect_err("a directory is not a certificate file");
    let IpcError::RemoteFileUnreadable {
        remote_file,
        remote_file_path: reported_file_path,
        ..
    } = directory_at_certificate_path_error
    else {
        panic!(
            "a directory at the certificate path names the certificate: \
             {directory_at_certificate_path_error}"
        );
    };
    assert_eq!(remote_file, RemoteFile::Certificate);
    assert_eq!(reported_file_path, remote_file_path.display().to_string());
}

#[test]
fn writing_where_the_directory_cannot_exist_names_the_file_that_failed() {
    let test_directory = TempDir::new().expect("make a test directory");
    std::fs::write(
        test_directory.path().join("remote"),
        b"a file, not a directory",
    )
    .expect("write it");

    let cert_path = CertFile::resolve_certificate_file_path(test_directory.path());
    let certificate_write_error = cert_file()
        .write_to_path(&cert_path)
        .expect_err("a file in the directory's place stops the write");
    let IpcError::RemoteFileWrite {
        remote_file,
        remote_file_path: reported_file_path,
        ..
    } = certificate_write_error
    else {
        panic!("a failed write names the certificate: {certificate_write_error}");
    };
    assert_eq!(remote_file, RemoteFile::Certificate);
    assert_eq!(reported_file_path, cert_path.display().to_string());

    let enabled_path = EnabledFile::resolve_enabled_file_path(test_directory.path());
    let enabled_file_write_error = EnabledFile {
        file_format: ENABLED_FILE_FORMAT,
        enabled_at: moment(1_000),
    }
    .write_to_path(&enabled_path)
    .expect_err("a file in the directory's place stops the write");
    let IpcError::RemoteFileWrite {
        remote_file,
        remote_file_path: reported_file_path,
        ..
    } = enabled_file_write_error
    else {
        panic!("a failed write names the record: {enabled_file_write_error}");
    };
    assert_eq!(remote_file, RemoteFile::RemoteAccessMark);
    assert_eq!(reported_file_path, enabled_path.display().to_string());
    assert!(!is_remote_enabled(test_directory.path()));
}

#[cfg(unix)]
#[test]
fn a_certificate_file_that_was_group_readable_is_private_after_the_write() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = CertFile::resolve_certificate_file_path(test_directory.path());
    cert_file()
        .write_to_path(&remote_file_path)
        .expect("write the certificate file");
    std::fs::set_permissions(&remote_file_path, std::fs::Permissions::from_mode(0o644))
        .expect("open the file up");

    cert_file()
        .write_to_path(&remote_file_path)
        .expect("write the certificate file again");

    let file_mode = std::fs::metadata(&remote_file_path)
        .expect("stat the certificate file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o600);
}

#[test]
fn the_two_files_are_written_as_these_exact_bytes() {
    let test_directory = TempDir::new().expect("make a test directory");
    cert_file()
        .write_to_path(&CertFile::resolve_certificate_file_path(
            test_directory.path(),
        ))
        .expect("write the certificate file");
    EnabledFile {
        file_format: ENABLED_FILE_FORMAT,
        enabled_at: moment(1_000),
    }
    .write_to_path(&EnabledFile::resolve_enabled_file_path(
        test_directory.path(),
    ))
    .expect("write the enabled file");

    assert_eq!(
        std::fs::read_to_string(CertFile::resolve_certificate_file_path(
            test_directory.path()
        ))
        .expect("read the certificate file"),
        format!(r#"{{"format":{CERT_FILE_FORMAT},"cert_der":[1,2,3,4],"key_der":[5,6,7,8]}}"#)
    );
    assert_eq!(
        std::fs::read_to_string(EnabledFile::resolve_enabled_file_path(
            test_directory.path()
        ))
        .expect("read the enabled file"),
        format!(
            r#"{{"format":{ENABLED_FILE_FORMAT},"enabled_at":{{"secs_since_epoch":1000,"nanos_since_epoch":0}}}}"#
        )
    );
}

#[test]
fn writing_the_enabled_file_again_replaces_the_time_it_holds() {
    let test_directory = TempDir::new().expect("make a test directory");
    let remote_file_path = EnabledFile::resolve_enabled_file_path(test_directory.path());
    EnabledFile {
        file_format: ENABLED_FILE_FORMAT,
        enabled_at: moment(1_000),
    }
    .write_to_path(&remote_file_path)
    .expect("write the enabled file");
    let replacement_enabled_file = EnabledFile {
        file_format: ENABLED_FILE_FORMAT,
        enabled_at: moment(2_000),
    };

    replacement_enabled_file
        .write_to_path(&remote_file_path)
        .expect("write it again");

    assert_eq!(
        EnabledFile::load_from_path(&remote_file_path).expect("read it back"),
        replacement_enabled_file
    );
}
