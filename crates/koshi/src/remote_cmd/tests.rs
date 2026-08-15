//! Tests for the `remote` verbs: how the three subcommands parse, what each
//! of the three answers renders to, and that no rendering prints a secret.

use super::*;

use std::time::{Duration, SystemTime};

use clap::Parser;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_servers::SavedServer;

use crate::cli::{Cli, CliCommand, FormatArg};

/// The parsed `remote` subcommand of `argv`.
fn remote_command(argv: &[&str]) -> RemoteCommand {
    match Cli::try_parse_from(argv)
        .expect("argv must parse")
        .command
        .expect("argv must carry a subcommand")
    {
        CliCommand::Remote { command } => command,
        other => panic!("argv must parse as a remote verb, got {other:?}"),
    }
}

/// The moment `seconds` after the Unix epoch.
fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

/// Two saved servers: one named and used, one unnamed and never used.
fn two_records() -> Vec<SavedServer> {
    vec![
        SavedServer {
            name: Some("work".to_string()),
            address: "laptop.local:7654".to_string(),
            secret: ConnectionToken::new("f00d"),
            fingerprint: "aa".repeat(32),
            added_at: at(10),
            last_used_at: Some(at(20)),
        },
        SavedServer {
            name: None,
            address: "10.0.0.4:7654".to_string(),
            secret: ConnectionToken::new("beef"),
            fingerprint: "bb".repeat(32),
            added_at: at(30),
            last_used_at: None,
        },
    ]
}

#[test]
fn a_bare_listing_takes_the_table_format() {
    assert_eq!(
        remote_command(&["koshi", "remote", "list"]),
        RemoteCommand::List {
            format: FormatArg::Table
        }
    );
}

#[test]
fn a_listing_takes_the_json_format_flag() {
    assert_eq!(
        remote_command(&["koshi", "remote", "list", "--format", "json"]),
        RemoteCommand::List {
            format: FormatArg::Json
        }
    );
}

#[test]
fn a_forget_takes_the_server_it_drops() {
    assert_eq!(
        remote_command(&["koshi", "remote", "forget", "work"]),
        RemoteCommand::Forget {
            server: "work".to_string()
        }
    );
}

#[test]
fn a_set_secret_takes_the_server_whose_secret_is_replaced() {
    assert_eq!(
        remote_command(&["koshi", "remote", "set-secret", "laptop.local:7654"]),
        RemoteCommand::SetSecret {
            server: "laptop.local:7654".to_string()
        }
    );
}

#[test]
fn a_forget_with_no_server_is_a_usage_error() {
    let err = Cli::try_parse_from(["koshi", "remote", "forget"]).expect_err("argv must not parse");
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn a_listing_renders_one_table_row_per_saved_server_with_an_absent_value_as_a_dash() {
    assert_eq!(
        output::render_remote_list(&two_records(), FormatArg::Table),
        format!(
            "name  address            fingerprint{}  last_used\n\
             work  laptop.local:7654  {}  20\n\
             -     10.0.0.4:7654      {}  -\n",
            " ".repeat(64 - "fingerprint".len()),
            "aa".repeat(32),
            "bb".repeat(32)
        )
    );
}

#[test]
fn a_listing_never_prints_a_saved_secret_in_either_format() {
    let records = two_records();
    let table = output::render_remote_list(&records, FormatArg::Table);
    let rendered_json = output::render_remote_list(&records, FormatArg::Json);

    assert!(!table.contains("f00d"), "table printed a secret: {table}");
    assert!(!table.contains("beef"), "table printed a secret: {table}");
    assert!(
        !rendered_json.contains("f00d"),
        "json printed a secret: {rendered_json}"
    );
    assert!(
        !rendered_json.contains("beef"),
        "json printed a secret: {rendered_json}"
    );
    assert!(
        !rendered_json.contains("secret"),
        "json carried a secret field: {rendered_json}"
    );
}

#[test]
fn an_empty_listing_is_the_header_row_alone_and_an_empty_json_array() {
    assert_eq!(
        output::render_remote_list(&[], FormatArg::Table),
        "name  address  fingerprint  last_used\n"
    );
    assert_eq!(output::render_remote_list(&[], FormatArg::Json), "[]\n");
}

#[test]
fn a_forget_names_the_address_it_dropped() {
    assert_eq!(
        output::render_remote_forget("laptop.local:7654"),
        "forgot laptop.local:7654.\n"
    );
}

#[test]
fn a_replaced_secret_names_the_address_it_belongs_to() {
    assert_eq!(
        output::render_remote_secret("laptop.local:7654"),
        "the secret for laptop.local:7654 was replaced.\n"
    );
}

#[test]
fn a_server_that_is_not_saved_is_refused_naming_the_listing_command() {
    let err = not_saved("work");
    assert_eq!(
        err.to_string(),
        "invalid arguments: no saved server is named work; run `koshi remote list`"
    );
}
