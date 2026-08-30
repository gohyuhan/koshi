//! Tests for redaction helpers.

use super::*;

#[test]
fn redact_argv_keeps_the_program_and_hides_every_argument() {
    // `mysql -pHUNTER2` carries the password in the argument itself.
    let argv = vec![
        "mysql".to_string(),
        "-pHUNTER2".to_string(),
        "--host=db.internal".to_string(),
    ];

    assert_eq!(
        redact_argv(&argv),
        vec!["mysql".to_string(), "***".to_string(), "***".to_string()],
    );
}

#[test]
fn redact_argv_hides_an_argument_that_holds_no_secret() {
    // Redaction is by position, not by content: `-la` is hidden too.
    let argv = vec!["ls".to_string(), "-la".to_string()];

    assert_eq!(
        redact_argv(&argv),
        vec!["ls".to_string(), "***".to_string()],
    );
}

#[test]
fn redact_argv_of_a_program_alone_returns_that_program() {
    let argv = vec!["htop".to_string()];

    assert_eq!(redact_argv(&argv), vec!["htop".to_string()]);
}

#[test]
fn redact_argv_hides_every_argument_of_an_argv_that_is_all_secrets() {
    // Index 0 is the program name and always prints, whatever it holds.
    let argv = vec![
        "-pHUNTER2".to_string(),
        "--token=abc123".to_string(),
        "--password=hunter2".to_string(),
    ];

    assert_eq!(
        redact_argv(&argv),
        vec![
            "-pHUNTER2".to_string(),
            "***".to_string(),
            "***".to_string(),
        ],
    );
}

#[test]
fn redact_argv_of_an_empty_argv_is_empty() {
    assert_eq!(redact_argv(&[]), Vec::<String>::new());
}
