//! Tests for reading a server's program file: which process wrote it, how the
//! version and program file it names compare with this koshi, whether a
//! restart may still come, and the hint each case gives.

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use koshi_ipc::protocol::ConnectionToken;
use tempfile::TempDir;

use super::*;

/// The clause the hints in these tests end a server with.
const TEST_END_INSTRUCTION: &str = "end it with: koshi kill-session session-test";

/// A program file naming `build_version` and `program_path`, written by this
/// process.
fn build_test_program_file(build_version: &str, program_path: &Path) -> ServerProgramFile {
    ServerProgramFile {
        process_id: std::process::id(),
        build_version: build_version.to_string(),
        program_path: program_path.to_string_lossy().into_owned(),
    }
}

/// An empty file at `file_path`, and its path.
fn write_empty_file(file_path: PathBuf) -> PathBuf {
    std::fs::write(&file_path, b"").expect("the file is written");
    file_path
}

#[test]
fn compare_server_build_orders_the_server_version_against_this_build() {
    let program_directory = TempDir::new().expect("a test directory");
    let program_path = write_empty_file(program_directory.path().join("koshi"));

    assert_eq!(
        compare_server_build(
            build_test_program_file("0.7.0", &program_path),
            "0.6.0",
            Some(&program_path)
        ),
        RefusingServerBuild::Newer(build_test_program_file("0.7.0", &program_path))
    );
    assert_eq!(
        compare_server_build(
            build_test_program_file("0.6.0", &program_path),
            "0.6.0",
            Some(&program_path)
        ),
        RefusingServerBuild::Unordered(build_test_program_file("0.6.0", &program_path))
    );
    assert_eq!(
        compare_server_build(
            build_test_program_file("dev", &program_path),
            "0.6.0",
            Some(&program_path)
        ),
        RefusingServerBuild::Unordered(build_test_program_file("dev", &program_path))
    );
    assert_eq!(
        compare_server_build(
            build_test_program_file("0.6.0-pr.1", &program_path),
            "0.6.0",
            Some(&program_path)
        ),
        RefusingServerBuild::Older {
            server_program_file: build_test_program_file("0.6.0-pr.1", &program_path),
            program_file_match: ProgramFileMatch::Same,
        }
    );
}

#[test]
fn compare_server_build_compares_the_program_file_of_an_older_server() {
    let program_directory = TempDir::new().expect("a test directory");
    let current_program_path = write_empty_file(program_directory.path().join("koshi"));
    let other_program_path = write_empty_file(program_directory.path().join("other-koshi"));
    let missing_program_path = program_directory.path().join("removed-koshi");

    assert_eq!(
        compare_server_build(
            build_test_program_file("0.5.0", &other_program_path),
            "0.6.0",
            Some(&current_program_path)
        ),
        RefusingServerBuild::Older {
            server_program_file: build_test_program_file("0.5.0", &other_program_path),
            program_file_match: ProgramFileMatch::Other,
        }
    );
    assert_eq!(
        compare_server_build(
            build_test_program_file("0.5.0", &missing_program_path),
            "0.6.0",
            Some(&current_program_path)
        ),
        RefusingServerBuild::Older {
            server_program_file: build_test_program_file("0.5.0", &missing_program_path),
            program_file_match: ProgramFileMatch::Undetermined,
        }
    );
    assert_eq!(
        compare_server_build(
            build_test_program_file("0.5.0", &current_program_path),
            "0.6.0",
            None
        ),
        RefusingServerBuild::Older {
            server_program_file: build_test_program_file("0.5.0", &current_program_path),
            program_file_match: ProgramFileMatch::Undetermined,
        }
    );
}

#[cfg(unix)]
#[test]
fn compare_program_files_matches_a_symbolic_link_with_the_file_it_names() {
    let program_directory = TempDir::new().expect("a test directory");
    let target_path = write_empty_file(program_directory.path().join("koshi-0.6.0"));
    let link_path = program_directory.path().join("koshi");
    std::os::unix::fs::symlink(&target_path, &link_path).expect("the link is made");

    assert_eq!(
        compare_program_files(&link_path, &target_path),
        ProgramFileMatch::Same
    );
}

#[test]
fn is_restart_expected_holds_only_while_a_restart_may_still_come() {
    let program_path = Path::new("/usr/local/bin/koshi");
    let older_with = |program_file_match| RefusingServerBuild::Older {
        server_program_file: build_test_program_file("0.5.0", program_path),
        program_file_match,
    };

    assert_eq!(
        [
            older_with(ProgramFileMatch::Same).is_restart_expected(),
            older_with(ProgramFileMatch::Undetermined).is_restart_expected(),
            RefusingServerBuild::NotRecorded.is_restart_expected(),
            RefusingServerBuild::Unreadable("junk".to_string()).is_restart_expected(),
            older_with(ProgramFileMatch::Other).is_restart_expected(),
            RefusingServerBuild::Newer(build_test_program_file("9.9.9", program_path))
                .is_restart_expected(),
            RefusingServerBuild::Unordered(build_test_program_file("dev", program_path))
                .is_restart_expected(),
        ],
        [true, true, true, true, false, false, false]
    );
}

#[test]
fn format_refusal_hint_names_a_newer_koshi_and_offers_no_end() {
    let refusing_server_build =
        RefusingServerBuild::Newer(build_test_program_file("9.9.9", Path::new("/opt/koshi")));

    assert_eq!(
        refusing_server_build.format_refusal_hint(TEST_END_INSTRUCTION),
        format!(
            "it runs koshi 9.9.9 from /opt/koshi, which is newer than this koshi \
             {CURRENT_BUILD_VERSION}; use /opt/koshi for it"
        )
    );
}

#[test]
fn format_refusal_hint_says_an_older_server_on_this_program_file_tries_again() {
    let refusing_server_build = RefusingServerBuild::Older {
        server_program_file: build_test_program_file("0.0.1", Path::new("/opt/koshi")),
        program_file_match: ProgramFileMatch::Same,
    };

    assert_eq!(
        refusing_server_build.format_refusal_hint(TEST_END_INSTRUCTION),
        format!(
            "it runs koshi 0.0.1 and has not restarted into this koshi {CURRENT_BUILD_VERSION} \
             yet; it tries again at each command from this koshi, and its log says what stopped \
             it"
        )
    );
}

#[test]
fn format_refusal_hint_offers_the_end_for_an_older_server_on_another_program_file() {
    let refusing_server_build = RefusingServerBuild::Older {
        server_program_file: build_test_program_file(
            "0.0.1",
            Path::new("/home/user/.cargo/bin/koshi"),
        ),
        program_file_match: ProgramFileMatch::Other,
    };

    assert_eq!(
        refusing_server_build.format_refusal_hint(TEST_END_INSTRUCTION),
        "it runs koshi 0.0.1 from /home/user/.cargo/bin/koshi, a program file this koshi does \
         not replace; use that koshi for it, or end it with: koshi kill-session session-test"
    );
}

#[test]
fn format_refusal_hint_offers_the_end_for_a_server_whose_program_file_is_gone() {
    let program_directory = TempDir::new().expect("a test directory");
    let missing_program_path = program_directory.path().join("removed-koshi");
    let refusing_server_build = RefusingServerBuild::Older {
        server_program_file: build_test_program_file("0.0.1", &missing_program_path),
        program_file_match: ProgramFileMatch::Undetermined,
    };

    assert_eq!(
        refusing_server_build.format_refusal_hint(TEST_END_INSTRUCTION),
        format!(
            "it runs koshi 0.0.1 from {}, which no longer exists; {TEST_END_INSTRUCTION}",
            missing_program_path.display()
        )
    );
}

#[test]
fn format_refusal_hint_says_a_server_on_a_program_file_it_cannot_compare_tries_again() {
    let program_directory = TempDir::new().expect("a test directory");
    let program_path = write_empty_file(program_directory.path().join("koshi"));
    let refusing_server_build = RefusingServerBuild::Older {
        server_program_file: build_test_program_file("0.0.1", &program_path),
        program_file_match: ProgramFileMatch::Undetermined,
    };

    assert_eq!(
        refusing_server_build.format_refusal_hint(TEST_END_INSTRUCTION),
        format!(
            "it runs koshi 0.0.1 from {} and has not restarted into this koshi \
             {CURRENT_BUILD_VERSION} yet; it tries again at each command from this koshi, and its \
             log says what stopped it",
            program_path.display()
        )
    );
}

#[test]
fn format_refusal_hint_offers_the_end_for_an_unordered_unrecorded_or_unreadable_server() {
    assert_eq!(
        [
            RefusingServerBuild::Unordered(build_test_program_file("dev", Path::new("/opt/koshi")))
                .format_refusal_hint(TEST_END_INSTRUCTION),
            RefusingServerBuild::NotRecorded.format_refusal_hint(TEST_END_INSTRUCTION),
            RefusingServerBuild::Unreadable("program file x is unreadable: junk".to_string())
                .format_refusal_hint(TEST_END_INSTRUCTION),
        ],
        [
            "it runs koshi dev from /opt/koshi; use that koshi for it, or end it with: koshi \
             kill-session session-test"
                .to_string(),
            format!(
                "it runs a koshi older than {CURRENT_BUILD_VERSION} that cannot restart into it; \
                 {TEST_END_INSTRUCTION}"
            ),
            "koshi cannot tell which koshi it runs (program file x is unreadable: junk); end it \
             with: koshi kill-session session-test"
                .to_string(),
        ]
    );
}

#[test]
fn format_refusal_hint_strips_control_characters_from_the_recorded_path() {
    let refusing_server_build = RefusingServerBuild::Newer(ServerProgramFile {
        process_id: std::process::id(),
        build_version: "9.9.9\u{1b}[2J".to_string(),
        program_path: "/opt/\u{1b}]0;title\u{7}koshi".to_string(),
    });

    assert_eq!(
        refusing_server_build.format_refusal_hint(TEST_END_INSTRUCTION),
        format!(
            "it runs koshi 9.9.9[2J from /opt/]0;titlekoshi, which is newer than this koshi \
             {CURRENT_BUILD_VERSION}; use /opt/]0;titlekoshi for it"
        )
    );
}

#[test]
fn find_server_program_file_takes_a_file_this_process_wrote_after_it_started() {
    let runtime_directory = TempDir::new().expect("a test directory");
    let program_file_path = runtime_directory.path().join("router.program");
    let program_file = build_test_program_file("0.6.0", Path::new("/usr/local/bin/koshi"));
    program_file
        .write_to_path(&program_file_path)
        .expect("the program file is written");

    assert_eq!(
        find_server_program_file(&program_file_path, std::process::id())
            .expect("the program file reads"),
        Some(program_file)
    );
}

#[test]
fn find_server_program_file_leaves_out_a_file_another_process_wrote() {
    let runtime_directory = TempDir::new().expect("a test directory");
    let program_file_path = runtime_directory.path().join("router.program");
    let program_file = ServerProgramFile {
        process_id: 5000,
        ..build_test_program_file("0.6.0", Path::new("/usr/local/bin/koshi"))
    };
    program_file
        .write_to_path(&program_file_path)
        .expect("the program file is written");

    assert_eq!(
        find_server_program_file(&program_file_path, std::process::id())
            .expect("the program file reads"),
        None
    );
}

#[test]
fn find_server_program_file_leaves_out_a_file_written_before_the_process_started() {
    let runtime_directory = TempDir::new().expect("a test directory");
    let program_file_path = runtime_directory.path().join("router.program");
    build_test_program_file("0.6.0", Path::new("/usr/local/bin/koshi"))
        .write_to_path(&program_file_path)
        .expect("the program file is written");
    std::fs::File::options()
        .write(true)
        .open(&program_file_path)
        .expect("the program file opens")
        .set_modified(UNIX_EPOCH + Duration::from_secs(1))
        .expect("the program file is aged");

    assert_eq!(
        find_server_program_file(&program_file_path, std::process::id())
            .expect("the program file reads"),
        None
    );
}

#[test]
fn find_server_program_file_gives_none_for_a_missing_file_and_an_error_for_junk() {
    let runtime_directory = TempDir::new().expect("a test directory");
    let program_file_path = runtime_directory.path().join("router.program");

    assert_eq!(
        find_server_program_file(&program_file_path, std::process::id())
            .expect("a missing file is no error"),
        None
    );
    std::fs::write(&program_file_path, b"not json").expect("write junk");
    assert_eq!(
        find_server_program_file(&program_file_path, std::process::id())
            .expect_err("junk is an error")
            .to_string(),
        format!(
            "program file {} is unreadable: expected ident at line 1 column 2",
            program_file_path.display()
        )
    );
}

#[test]
fn find_refusing_server_build_reads_no_record_and_an_unreadable_record() {
    let runtime_directory = TempDir::new().expect("a test directory");
    let program_file_path = runtime_directory.path().join("router.program");

    assert_eq!(
        find_refusing_server_build(&program_file_path, std::process::id()),
        RefusingServerBuild::NotRecorded
    );
    std::fs::write(&program_file_path, b"not json").expect("write junk");
    assert_eq!(
        find_refusing_server_build(&program_file_path, std::process::id()),
        RefusingServerBuild::Unreadable(format!(
            "program file {} is unreadable: expected ident at line 1 column 2",
            program_file_path.display()
        ))
    );
}

#[test]
fn find_refusing_server_build_compares_a_record_with_the_program_this_process_runs() {
    let runtime_directory = TempDir::new().expect("a test directory");
    let program_file_path = runtime_directory.path().join("router.program");
    let current_program_path = std::env::current_exe().expect("the test binary has a path");
    let program_file = build_test_program_file("0.0.0-older", &current_program_path);
    program_file
        .write_to_path(&program_file_path)
        .expect("the program file is written");

    assert_eq!(
        find_refusing_server_build(&program_file_path, std::process::id()),
        RefusingServerBuild::Older {
            server_program_file: program_file,
            program_file_match: ProgramFileMatch::Same,
        }
    );
}

#[test]
fn find_advertised_server_program_file_pairs_the_record_with_the_endpoint_file() {
    let runtime_directory = TempDir::new().expect("a test directory");
    let program_file_path = runtime_directory.path().join("router.program");
    let endpoint_file_path = runtime_directory.path().join("router.json");
    let program_file = build_test_program_file("0.6.0", Path::new("/usr/local/bin/koshi"));
    program_file
        .write_to_path(&program_file_path)
        .expect("the program file is written");

    assert_eq!(
        find_advertised_server_program_file(&program_file_path, &endpoint_file_path),
        None
    );
    EndpointFile {
        socket_address: "/run/koshi/router.sock".to_string(),
        connection_token: ConnectionToken::from_secret("a".repeat(64)),
        process_id: std::process::id(),
    }
    .write_to_path(&endpoint_file_path)
    .expect("the endpoint file is written");
    assert_eq!(
        find_advertised_server_program_file(&program_file_path, &endpoint_file_path),
        Some(program_file)
    );
}
