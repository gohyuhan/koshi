//! `koshi keys conflicts` and `koshi keys validate` against the
//! `keybinding.kdl` of a test home: the verdict a client reaches on the file
//! when it attaches, for every reason a client ignores the file, and the exit
//! code that verdict gives.

use std::fs;
use std::path::Path;

use koshi_config::types::SCHEMA_VERSION;
use tempfile::TempDir;

mod common;

use common::{create_keybinding_file_path, run_koshi_under_home, KoshiRunOutput};

/// The answer `koshi keys conflicts` gives for a `keybinding.kdl` at
/// `keybinding_file_path` that a client ignores for `file_error_message`.
fn build_ignored_file_answer(
    keybinding_file_path: &Path,
    file_error_message: &str,
) -> KoshiRunOutput {
    KoshiRunOutput {
        standard_output: format!("file: ignored ({file_error_message})\nverdict: reject\n"),
        standard_error: format!(
            "koshi: keybinding file {} failed validation\n",
            keybinding_file_path.display()
        ),
        exit_code: Some(2),
    }
}

#[test]
fn keys_conflicts_applies_the_defaults_when_no_keybinding_file_exists() {
    let home_directory = TempDir::new().expect("create test home");
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "conflicts"]),
        KoshiRunOutput {
            standard_output: "verdict: apply\n".to_string(),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
}

#[test]
fn keys_conflicts_rejects_a_keybinding_file_that_cannot_be_read() {
    // A directory named `keybinding.kdl`, and a file whose bytes are not
    // UTF-8: both exist, and neither reads as text.
    let directory_home = TempDir::new().expect("create test home");
    let directory_file_path = create_keybinding_file_path(directory_home.path());
    fs::create_dir(&directory_file_path).expect("create directory");
    let not_utf8_home = TempDir::new().expect("create test home");
    let not_utf8_file_path = create_keybinding_file_path(not_utf8_home.path());
    fs::write(&not_utf8_file_path, [0x76, 0x65, 0x80, 0x72]).expect("write");

    for (home_directory, keybinding_file_path) in [
        (&directory_home, &directory_file_path),
        (&not_utf8_home, &not_utf8_file_path),
    ] {
        let read_error =
            fs::read_to_string(keybinding_file_path).expect_err("the file does not read as text");
        assert_eq!(
            run_koshi_under_home(home_directory.path(), &["keys", "conflicts"]),
            build_ignored_file_answer(keybinding_file_path, &read_error.to_string())
        );
    }
}

#[test]
fn keys_conflicts_rejects_a_keybinding_file_that_is_not_kdl() {
    let home_directory = TempDir::new().expect("create test home");
    let keybinding_file_path = create_keybinding_file_path(home_directory.path());
    fs::write(&keybinding_file_path, "version 2\nmode \"normal\" {\n").expect("write");
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "conflicts"]),
        build_ignored_file_answer(
            &keybinding_file_path,
            &format!(
                "config parse error in {}: line 2, column 15: No closing '}}' for child block",
                keybinding_file_path.display()
            )
        )
    );
}

#[test]
fn keys_conflicts_rejects_a_keybinding_file_that_breaks_the_schema() {
    let newer_schema_version = SCHEMA_VERSION + 1;
    let schema_error_cases = [
        (
            "version 2\nunlock_alternative \"<A-u>\"\n".to_string(),
            "line 2, column 1: unknown key `unlock_alternative`; did you mean \
             `unlock-alternative`?"
                .to_string(),
        ),
        (
            "chord-timeout-ms 750\n".to_string(),
            "line 1, column 1: keybinding file must declare `version`".to_string(),
        ),
        (
            "version 0\n".to_string(),
            "line 1, column 1: config schema version must be at least 1".to_string(),
        ),
        (
            format!("version {newer_schema_version}\n"),
            format!(
                "line 1, column 1: config schema version {newer_schema_version} is newer than \
                 this koshi supports ({SCHEMA_VERSION})"
            ),
        ),
        (
            "version 2\nmode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n    bind \"<A-\" \
             \"core:new-tab\"\n}\n"
                .to_string(),
            "line 3, column 10: invalid key `<C-`: missing closing `>`; line 4, column 10: \
             invalid key `<A-`: missing closing `>`"
                .to_string(),
        ),
    ];
    for (keybinding_source_text, file_error_message) in schema_error_cases {
        let home_directory = TempDir::new().expect("create test home");
        let keybinding_file_path = create_keybinding_file_path(home_directory.path());
        fs::write(&keybinding_file_path, &keybinding_source_text).expect("write");
        assert_eq!(
            run_koshi_under_home(home_directory.path(), &["keys", "conflicts"]),
            build_ignored_file_answer(&keybinding_file_path, &file_error_message),
            "keybinding.kdl:\n{keybinding_source_text}"
        );
    }
}

#[test]
fn keys_conflicts_json_rejects_an_ignored_file() {
    let home_directory = TempDir::new().expect("create test home");
    let keybinding_file_path = create_keybinding_file_path(home_directory.path());
    fs::write(
        &keybinding_file_path,
        "version 2\nunlock_alternative \"<A-u>\"\n",
    )
    .expect("write");
    assert_eq!(
        run_koshi_under_home(
            home_directory.path(),
            &["keys", "conflicts", "--format", "json"]
        ),
        KoshiRunOutput {
            standard_output: r#"{
  "verdict": "reject",
  "file_error": "line 2, column 1: unknown key `unlock_alternative`; did you mean `unlock-alternative`?",
  "conflict_findings": []
}
"#
            .to_string(),
            standard_error: format!(
                "koshi: keybinding file {} failed validation\n",
                keybinding_file_path.display()
            ),
            exit_code: Some(2),
        }
    );
}

#[test]
fn keys_conflicts_rejects_a_keybinding_file_with_a_fatal_finding() {
    let home_directory = TempDir::new().expect("create test home");
    let keybinding_file_path = create_keybinding_file_path(home_directory.path());
    fs::write(
        &keybinding_file_path,
        "version 2\nmode \"locked\" {\n    remove \"<C-l>\"\n}\n",
    )
    .expect("write");
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "conflicts"]),
        KoshiRunOutput {
            standard_output: "verdict: reject
severity  finding
fatal     locked mode has no binding from `<C-l>` to `core:unlock`; the unlock escape would be unreachable
"
            .to_string(),
            standard_error: format!(
                "koshi: keybinding file {} failed validation\n",
                keybinding_file_path.display()
            ),
            exit_code: Some(2),
        }
    );
}

#[test]
fn keys_conflicts_applies_a_keybinding_file_with_warnings_alone() {
    let home_directory = TempDir::new().expect("create test home");
    let keybinding_file_path = create_keybinding_file_path(home_directory.path());
    fs::write(
        &keybinding_file_path,
        "version 2\nmode \"normal\" {\n    bind \"<C-y>\" \"core:not-a-real-action\"\n}\n",
    )
    .expect("write");
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "conflicts"]),
        KoshiRunOutput {
            standard_output: "verdict: apply
severity  finding
warning   `<C-y>` in mode `normal` (user) names unknown action `core:not-a-real-action`; the binding is inactive until the action is registered
"
            .to_string(),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
}

#[test]
fn keys_conflicts_applies_a_keybinding_file_on_an_older_schema_version() {
    let home_directory = TempDir::new().expect("create test home");
    let keybinding_file_path = create_keybinding_file_path(home_directory.path());
    fs::write(
        &keybinding_file_path,
        "version 1\nmode \"normal\" {\n    bind \"<C-y>\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "conflicts"]),
        KoshiRunOutput {
            standard_output: "verdict: apply\n".to_string(),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
}

#[test]
fn keys_validate_exits_with_the_verdict_keys_conflicts_reaches() {
    let test_directory = TempDir::new().expect("create test home");
    let applied_file_path = test_directory.path().join("applied.kdl");
    fs::write(
        &applied_file_path,
        "version 2\nmode \"normal\" {\n    bind \"<C-y>\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    let refused_file_path = test_directory.path().join("refused.kdl");
    fs::write(
        &refused_file_path,
        "version 2\nmode \"locked\" {\n    remove \"<C-l>\"\n}\n",
    )
    .expect("write");

    assert_eq!(
        run_koshi_under_home(
            test_directory.path(),
            &["keys", "validate", &applied_file_path.display().to_string()]
        ),
        KoshiRunOutput {
            standard_output: "valid: koshi would apply this file as keybinding.kdl\n".to_string(),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
    assert_eq!(
        run_koshi_under_home(
            test_directory.path(),
            &["keys", "validate", &refused_file_path.display().to_string()]
        ),
        KoshiRunOutput {
            standard_output: "invalid: koshi would refuse this file as keybinding.kdl
severity  finding
fatal     locked mode has no binding from `<C-l>` to `core:unlock`; the unlock escape would be unreachable
"
            .to_string(),
            standard_error: format!(
                "koshi: keybinding file {} failed validation\n",
                refused_file_path.display()
            ),
            exit_code: Some(2),
        }
    );
}
