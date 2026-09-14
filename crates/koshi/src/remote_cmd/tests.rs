//! Tests for the `remote` verbs: how the subcommands parse, what each answer
//! renders to, that no rendering prints a secret, which names and addresses a
//! saved server may take, which fingerprint a changed saved server keeps, what the store a
//! settled saved server goes into holds, that a refused placement changes nothing,
//! and which change another koshi made while the questions were open refuses
//! an edit.

use super::*;

use std::time::{Duration, SystemTime};

use clap::Parser;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_servers::SavedServer;

use crate::cli::{Cli, CliCommand, OutputFormat};

/// The parsed `remote` subcommand of `argv`.
fn parse_test_remote_command(argument_values: &[&str]) -> RemoteCommand {
    match Cli::try_parse_from(argument_values)
        .expect("argv must parse")
        .command
        .expect("argv must carry a subcommand")
    {
        CliCommand::Remote { command } => command,
        unexpected_command => {
            panic!("argv must parse as a remote verb, got {unexpected_command:?}")
        }
    }
}

/// The moment `seconds` after the Unix epoch.
fn build_test_system_time_at_seconds(elapsed_seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(elapsed_seconds)
}

/// Two saved servers: one named and used, one unnamed and never used.
fn build_two_saved_server_records() -> Vec<SavedServer> {
    vec![
        SavedServer {
            server_name: Some("work".to_string()),
            server_address: "laptop.local:7654".to_string(),
            connection_token: ConnectionToken::from_secret("f00d"),
            certificate_fingerprint: Some("aa".repeat(32)),
            added_at: build_test_system_time_at_seconds(10),
            last_used_at: Some(build_test_system_time_at_seconds(20)),
        },
        SavedServer {
            server_name: None,
            server_address: "10.0.0.4:7654".to_string(),
            connection_token: ConnectionToken::from_secret("beef"),
            certificate_fingerprint: Some("bb".repeat(32)),
            added_at: build_test_system_time_at_seconds(30),
            last_used_at: None,
        },
    ]
}

#[test]
fn a_bare_listing_takes_the_table_format() {
    assert_eq!(
        parse_test_remote_command(&["koshi", "remote", "list"]),
        RemoteCommand::List {
            output_format: OutputFormat::Table
        }
    );
}

#[test]
fn a_listing_takes_the_json_format_flag() {
    assert_eq!(
        parse_test_remote_command(&["koshi", "remote", "list", "--format", "json"]),
        RemoteCommand::List {
            output_format: OutputFormat::Json
        }
    );
}

#[test]
fn a_forget_takes_the_server_it_drops() {
    assert_eq!(
        parse_test_remote_command(&["koshi", "remote", "forget", "work"]),
        RemoteCommand::Forget {
            server_reference: "work".to_string()
        }
    );
}

#[test]
fn a_set_secret_takes_the_server_whose_secret_is_replaced() {
    assert_eq!(
        parse_test_remote_command(&["koshi", "remote", "set-secret", "laptop.local:7654"]),
        RemoteCommand::SetSecret {
            server_reference: "laptop.local:7654".to_string()
        }
    );
}

#[test]
fn a_forget_with_no_server_is_a_usage_error() {
    let parse_error =
        Cli::try_parse_from(["koshi", "remote", "forget"]).expect_err("argv must not parse");
    assert_eq!(parse_error.exit_code(), 2);
}

#[test]
fn a_listing_renders_one_table_row_per_saved_server_with_an_absent_value_as_a_dash() {
    assert_eq!(
        output::render_remote_list(&build_two_saved_server_records(), OutputFormat::Table),
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
    let saved_server_records = build_two_saved_server_records();
    let table_output = output::render_remote_list(&saved_server_records, OutputFormat::Table);
    let json_output = output::render_remote_list(&saved_server_records, OutputFormat::Json);

    assert!(
        !table_output.contains("f00d"),
        "table printed a secret: {table_output}"
    );
    assert!(
        !table_output.contains("beef"),
        "table printed a secret: {table_output}"
    );
    assert!(
        !json_output.contains("f00d"),
        "json printed a secret: {json_output}"
    );
    assert!(
        !json_output.contains("beef"),
        "json printed a secret: {json_output}"
    );
    assert!(
        !json_output.contains("secret"),
        "json carried a secret field: {json_output}"
    );
}

#[test]
fn an_empty_listing_is_the_header_row_alone_and_an_empty_json_array() {
    assert_eq!(
        output::render_remote_list(&[], OutputFormat::Table),
        "name  address  fingerprint  last_used\n"
    );
    assert_eq!(output::render_remote_list(&[], OutputFormat::Json), "[]\n");
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
    let validation_error = build_saved_server_not_found_error("work");
    assert_eq!(
        validation_error.to_string(),
        "invalid arguments: no saved server is named work; run `koshi remote list`"
    );
}

/// A store holding the two records `build_two_saved_server_records` builds.
fn build_server_store(saved_server_records: Vec<SavedServer>) -> ServerStore {
    let mut server_store = ServerStore::new();
    server_store.saved_servers = saved_server_records;
    server_store
}

#[test]
fn a_saved_server_is_found_by_its_name_and_by_its_address() {
    let server_store = build_server_store(build_two_saved_server_records());

    assert_eq!(
        find_saved_server(&server_store, "work")
            .expect("the name is saved")
            .server_address,
        "laptop.local:7654"
    );
    assert_eq!(
        find_saved_server(&server_store, "10.0.0.4:7654")
            .expect("the address is saved")
            .server_address,
        "10.0.0.4:7654"
    );
}

#[test]
fn a_server_the_store_does_not_hold_is_refused_naming_the_listing_command() {
    let server_store = build_server_store(build_two_saved_server_records());

    assert_eq!(
        find_saved_server(&server_store, "desk")
            .expect_err("nothing is saved under desk")
            .to_string(),
        "invalid arguments: no saved server is named desk; run `koshi remote list`"
    );
}

/// One word answering for two records — one saved server's chosen name, another
/// saved server's address — is refused rather than resolved to either of them.
#[test]
fn a_word_naming_one_server_and_addressing_another_is_refused_as_ambiguous() {
    let server_store = build_server_store(vec![
        SavedServer {
            server_name: Some("desk:7654".to_string()),
            server_address: "laptop.local:7654".to_string(),
            connection_token: ConnectionToken::from_secret("f00d"),
            certificate_fingerprint: Some("aa".repeat(32)),
            added_at: build_test_system_time_at_seconds(10),
            last_used_at: None,
        },
        SavedServer {
            server_name: None,
            server_address: "desk:7654".to_string(),
            connection_token: ConnectionToken::from_secret("beef"),
            certificate_fingerprint: Some("bb".repeat(32)),
            added_at: build_test_system_time_at_seconds(20),
            last_used_at: None,
        },
    ]);

    assert_eq!(
        find_saved_server(&server_store, "desk:7654")
            .expect_err("two records answer to that word")
            .to_string(),
        "invalid arguments: desk:7654 is the name of one saved server and the address of \
         another; run `koshi remote list` and name the one you mean"
    );
}

#[test]
fn a_new_takes_no_argument_and_an_edit_takes_the_server_it_changes() {
    assert_eq!(
        parse_test_remote_command(&["koshi", "remote", "new"]),
        RemoteCommand::New
    );
    assert_eq!(
        parse_test_remote_command(&["koshi", "remote", "edit", "work"]),
        RemoteCommand::Edit {
            server_reference: "work".to_string()
        }
    );
}

#[test]
fn an_edit_with_no_server_is_a_usage_error() {
    let parse_error =
        Cli::try_parse_from(["koshi", "remote", "edit"]).expect_err("argv must not parse");
    assert_eq!(parse_error.exit_code(), 2);
}

#[test]
fn a_checked_record_renders_its_name_and_address_and_an_unchecked_one_says_when_it_pins() {
    let mut saved_server = build_two_saved_server_records().remove(0);
    assert_eq!(
        output::render_remote_saved(&saved_server),
        "saved work at laptop.local:7654.\n"
    );
    assert_eq!(
        output::render_remote_updated(&saved_server),
        "updated work at laptop.local:7654.\n"
    );

    saved_server.certificate_fingerprint = None;
    assert_eq!(
        output::render_remote_saved(&saved_server),
        "saved work at laptop.local:7654; its certificate is pinned on the first connection.\n"
    );
}

#[test]
fn a_record_with_no_name_renders_its_address_alone() {
    let saved_server = build_two_saved_server_records().remove(1);
    assert_eq!(
        output::render_remote_updated(&saved_server),
        "updated 10.0.0.4:7654.\n"
    );
}

#[test]
fn a_discarded_answer_says_nothing_was_saved() {
    assert_eq!(output::render_remote_discarded(), "nothing was saved.\n");
}

#[test]
fn neither_settled_line_prints_the_secret_it_carries() {
    for saved_server in build_two_saved_server_records() {
        let saved_output = output::render_remote_saved(&saved_server);
        let updated_output = output::render_remote_updated(&saved_server);
        assert!(
            !saved_output.contains("f00d") && !saved_output.contains("beef"),
            "{saved_output}"
        );
        assert!(
            !updated_output.contains("f00d") && !updated_output.contains("beef"),
            "{updated_output}"
        );
    }
}

#[test]
fn an_empty_name_is_refused_and_a_free_one_is_taken() {
    let server_store = build_server_store(build_two_saved_server_records());

    assert_eq!(
        validate_server_name_available(&server_store, "")
            .expect_err("an empty name is refused")
            .to_string(),
        "invalid arguments: a name is needed, such as work"
    );
    validate_server_name_available(&server_store, "desk").expect("no saved server answers to desk");
}

#[test]
fn a_name_with_the_shape_of_an_address_is_refused() {
    let server_store = build_server_store(build_two_saved_server_records());

    assert!(
        validate_server_name_available(&server_store, "desk.local:7654")
            .expect_err("a name must not be an address")
            .to_string()
            .contains("the shape of an address"),
        "the refusal names the shape"
    );
}

#[test]
fn a_name_another_record_answers_to_is_refused() {
    let server_store = build_server_store(build_two_saved_server_records());

    assert_eq!(
        validate_server_name_available(&server_store, "work")
            .expect_err("another saved server answers to it")
            .to_string(),
        "invalid arguments: work already answers for a saved server; \
         run `koshi remote list` and pick another name"
    );
}

/// A name is checked for the shape of an address before the store is asked,
/// so a name that is another saved server's address is refused for its shape.
#[test]
fn a_name_that_is_another_record_s_address_is_refused_for_its_shape() {
    let server_store = build_server_store(build_two_saved_server_records());

    assert!(
        validate_server_name_available(&server_store, "10.0.0.4:7654")
            .expect_err("that is another saved server's address")
            .to_string()
            .contains("the shape of an address"),
        "the refusal names the shape"
    );
}

#[test]
fn an_address_that_is_not_host_port_is_refused_naming_the_shape() {
    let server_store = build_server_store(build_two_saved_server_records());

    for invalid_address in ["", "laptop.local", "laptop.local:door"] {
        assert_eq!(
            validate_saved_server_address_is_available(&server_store, invalid_address)
                .expect_err("that is not an address")
                .to_string(),
            format!(
                "invalid arguments: an address is host:port, such as laptop.local:7654, \
                 and {invalid_address} is not"
            )
        );
    }
    validate_saved_server_address_is_available(&server_store, "desk.local:7654")
        .expect("no saved server answers to that address");
}

#[test]
fn an_address_another_record_answers_to_is_refused_naming_the_edit_command() {
    let server_store = build_server_store(build_two_saved_server_records());

    assert_eq!(
        validate_saved_server_address_is_available(&server_store, "10.0.0.4:7654")
            .expect_err("another saved server holds that address")
            .to_string(),
        "invalid arguments: 10.0.0.4:7654 already answers for a saved server; \
         run `koshi remote edit 10.0.0.4:7654` to change it"
    );
}

/// What a changed saved server offers the check and keeps afterwards: the pinned
/// fingerprint while the address is unchanged, and nothing once the user
/// changed the address.
#[test]
fn an_address_that_changed_drops_what_was_held_and_an_unchanged_one_keeps_it() {
    let build_held_certificate_fingerprint = || Some("bb".repeat(32));

    assert_eq!(
        resolve_certificate_fingerprint(build_held_certificate_fingerprint(), false),
        build_held_certificate_fingerprint()
    );
    assert_eq!(
        resolve_certificate_fingerprint(build_held_certificate_fingerprint(), true),
        None
    );
    assert_eq!(resolve_certificate_fingerprint(None, false), None);
    assert_eq!(resolve_certificate_fingerprint(None, true), None);
}

/// A saved server named `desk` at `desk.local:7654`, pinning nothing.
fn build_new_saved_server_record() -> SavedServer {
    SavedServer {
        server_name: Some("desk".to_string()),
        server_address: "desk.local:7654".to_string(),
        connection_token: ConnectionToken::from_secret("cafe"),
        certificate_fingerprint: None,
        added_at: build_test_system_time_at_seconds(40),
        last_used_at: None,
    }
}

#[test]
fn a_new_record_joins_the_saved_servers_already_there() {
    let mut settled_server_store = build_server_store(build_two_saved_server_records());
    save_saved_server(
        &mut settled_server_store,
        &build_new_saved_server_record(),
        None,
    )
    .expect("its name and address are free");

    assert_eq!(
        settled_server_store
            .saved_servers
            .iter()
            .map(|saved_server| saved_server.server_address.as_str())
            .collect::<Vec<_>>(),
        vec!["laptop.local:7654", "10.0.0.4:7654", "desk.local:7654"]
    );
}

#[test]
fn a_replaced_record_leaves_the_store_and_the_new_one_takes_its_place() {
    let mut moved_saved_server = build_two_saved_server_records().remove(0);
    moved_saved_server.server_address = "desk.local:7655".to_string();

    let mut settled_server_store = build_server_store(build_two_saved_server_records());
    save_saved_server(&mut settled_server_store, &moved_saved_server, Some("work"))
        .expect("the saved server moves");

    assert_eq!(
        settled_server_store
            .saved_servers
            .iter()
            .map(|saved_server| (
                saved_server.server_name.as_deref(),
                saved_server.server_address.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![(None, "10.0.0.4:7654"), (Some("work"), "desk.local:7655")],
        "one saved server answers for work, at the address the edit typed"
    );
}

#[test]
fn a_record_that_keeps_its_own_name_and_address_is_placed_back() {
    let mut updated_saved_server = build_two_saved_server_records().remove(0);
    updated_saved_server.connection_token = ConnectionToken::from_secret("new secret");

    let mut settled_server_store = build_server_store(build_two_saved_server_records());
    save_saved_server(
        &mut settled_server_store,
        &updated_saved_server,
        Some("work"),
    )
    .expect("it may keep both");

    assert_eq!(settled_server_store.saved_servers.len(), 2);
    assert_eq!(
        settled_server_store
            .saved_servers
            .iter()
            .find(|saved_server| saved_server.server_name.as_deref() == Some("work"))
            .expect("work is still saved")
            .connection_token,
        ConnectionToken::from_secret("new secret")
    );
}

/// The saved server was forgotten while the questions were open. Placing it back
/// would return a secret the user dropped, so it is refused.
#[test]
fn a_record_that_is_no_longer_saved_is_refused_rather_than_put_back() {
    assert_eq!(
        save_saved_server(
            &mut ServerStore::new(),
            &build_new_saved_server_record(),
            Some("work")
        )
        .expect_err("nothing answers to work now")
        .to_string(),
        "invalid arguments: no saved server is named work; run `koshi remote list`"
    );
}

/// Another koshi saved a saved server under this name while the questions were
/// open. The name is taken now, so the placement is refused.
#[test]
fn a_name_another_record_took_meanwhile_is_refused_and_changes_nothing() {
    let mut taken_saved_server = build_new_saved_server_record();
    taken_saved_server.server_name = Some("work".to_string());
    let mut server_store = build_server_store(build_two_saved_server_records());

    assert_eq!(
        save_saved_server(&mut server_store, &taken_saved_server, None)
            .expect_err("work is taken")
            .to_string(),
        "invalid arguments: work already answers for a saved server; \
         run `koshi remote list` and pick another name"
    );
    assert_eq!(
        server_store.saved_servers,
        build_two_saved_server_records(),
        "the refusal wrote nothing"
    );
}

#[test]
fn an_address_another_record_took_meanwhile_is_refused_and_changes_nothing() {
    let mut taken_saved_server = build_new_saved_server_record();
    taken_saved_server.server_address = "10.0.0.4:7654".to_string();
    let mut server_store = build_server_store(build_two_saved_server_records());

    assert_eq!(
        save_saved_server(&mut server_store, &taken_saved_server, None)
            .expect_err("that address is taken")
            .to_string(),
        "invalid arguments: 10.0.0.4:7654 already answers for a saved server; \
         run `koshi remote edit 10.0.0.4:7654` to change it"
    );
    assert_eq!(
        server_store.saved_servers,
        build_two_saved_server_records(),
        "the refusal wrote nothing"
    );
}

/// The replaced saved server leaves before the checks, so a refusal after that point
/// must put it back rather than leave the store missing it.
#[test]
fn a_replacement_refused_after_the_record_left_puts_every_saved_server_back() {
    let mut moved_saved_server = build_two_saved_server_records().remove(0);
    moved_saved_server.server_address = "10.0.0.4:7654".to_string();
    let mut server_store = build_server_store(build_two_saved_server_records());

    assert_eq!(
        save_saved_server(&mut server_store, &moved_saved_server, Some("work"))
            .expect_err("the other saved server already answers to that address")
            .to_string(),
        "invalid arguments: 10.0.0.4:7654 already answers for a saved server; \
         run `koshi remote edit 10.0.0.4:7654` to change it"
    );
    assert_eq!(
        server_store.saved_servers,
        build_two_saved_server_records(),
        "work is still saved"
    );
}

#[test]
fn a_record_with_no_name_is_placed_without_a_name_check() {
    let mut nameless_saved_server = build_new_saved_server_record();
    nameless_saved_server.server_name = None;

    let mut settled_server_store = build_server_store(build_two_saved_server_records());
    save_saved_server(&mut settled_server_store, &nameless_saved_server, None)
        .expect("its address is free");

    assert_eq!(settled_server_store.saved_servers.len(), 3);
    assert_eq!(settled_server_store.saved_servers[2].server_name, None);
}

/// The saved server on disk still holds everything the questions were answered
/// against, so the edit goes on.
#[test]
fn a_record_no_other_koshi_touched_is_taken_as_it_stands() {
    let held_saved_server = build_two_saved_server_records().remove(0);

    let current_saved_server = find_unchanged_saved_server(
        &build_server_store(build_two_saved_server_records()),
        "work",
        &held_saved_server,
    )
    .expect("nothing about it changed");

    assert_eq!(current_saved_server, held_saved_server);
}

/// Another koshi dialled this server while the questions were open, so its
/// last-used time moved. That is not a change to what the questions asked
/// about, and the fresh time is the one the edit carries.
#[test]
fn a_last_used_time_another_koshi_stamped_is_carried_and_does_not_refuse() {
    let held_saved_server = build_two_saved_server_records().remove(0);
    let mut dialled_server_records = build_two_saved_server_records();
    dialled_server_records[0].last_used_at = Some(build_test_system_time_at_seconds(99));

    let current_saved_server = find_unchanged_saved_server(
        &build_server_store(dialled_server_records),
        "work",
        &held_saved_server,
    )
    .expect("only the last-used time moved");

    assert_eq!(
        current_saved_server.last_used_at,
        Some(build_test_system_time_at_seconds(99))
    );
}

/// Another koshi replaced this saved server while the questions were open, so its
/// added time moved. The edit carries the fresh one rather than putting the
/// old one back.
#[test]
fn an_added_time_another_koshi_wrote_is_carried_and_does_not_refuse() {
    let held_saved_server = build_two_saved_server_records().remove(0);
    let mut resaved_server_records = build_two_saved_server_records();
    resaved_server_records[0].added_at = build_test_system_time_at_seconds(77);

    let current_saved_server = find_unchanged_saved_server(
        &build_server_store(resaved_server_records),
        "work",
        &held_saved_server,
    )
    .expect("only the added time moved");

    assert_eq!(
        current_saved_server.added_at,
        build_test_system_time_at_seconds(77)
    );
}

/// Another koshi replaced the secret while the questions were open. Writing
/// the edit would put the old secret back, so it is refused.
#[test]
fn a_secret_another_koshi_replaced_meanwhile_refuses_the_edit() {
    let held_saved_server = build_two_saved_server_records().remove(0);
    let mut replaced_server_records = build_two_saved_server_records();
    replaced_server_records[0].connection_token = ConnectionToken::from_secret("newer");

    assert_eq!(
        find_unchanged_saved_server(
            &build_server_store(replaced_server_records),
            "work",
            &held_saved_server,
        )
        .expect_err("the secret is not the one that was asked about")
        .to_string(),
        "invalid arguments: work changed while the questions were open, so nothing \
         was saved; run `koshi remote edit work` again"
    );
}

/// Another koshi's first connection pinned a certificate while the questions
/// were open. Writing the edit would drop that pin, so it is refused.
#[test]
fn a_fingerprint_another_koshi_pinned_meanwhile_refuses_the_edit() {
    let mut held_saved_server = build_two_saved_server_records().remove(0);
    held_saved_server.certificate_fingerprint = None;

    assert_eq!(
        find_unchanged_saved_server(
            &build_server_store(build_two_saved_server_records()),
            "work",
            &held_saved_server
        )
        .expect_err("a fingerprint appeared")
        .to_string(),
        "invalid arguments: work changed while the questions were open, so nothing \
         was saved; run `koshi remote edit work` again"
    );
}

#[test]
fn an_address_another_koshi_moved_meanwhile_refuses_the_edit() {
    let held_saved_server = build_two_saved_server_records().remove(0);
    let mut moved_server_records = build_two_saved_server_records();
    moved_server_records[0].server_address = "laptop.local:7655".to_string();

    assert_eq!(
        find_unchanged_saved_server(
            &build_server_store(moved_server_records),
            "work",
            &held_saved_server,
        )
        .expect_err("it sits at another address now")
        .to_string(),
        "invalid arguments: work changed while the questions were open, so nothing \
         was saved; run `koshi remote edit work` again"
    );
}

#[test]
fn a_record_another_koshi_forgot_meanwhile_refuses_the_edit() {
    let held_saved_server = build_two_saved_server_records().remove(0);

    assert_eq!(
        find_unchanged_saved_server(&ServerStore::new(), "work", &held_saved_server)
            .expect_err("nothing answers to work now")
            .to_string(),
        "invalid arguments: no saved server is named work; run `koshi remote list`"
    );
}

/// Another koshi renamed this saved server while the questions were open. Writing
/// the edit would put the old name back, so it is refused.
#[test]
fn a_name_another_koshi_changed_meanwhile_refuses_the_edit() {
    let held_saved_server = build_two_saved_server_records().remove(0);
    let mut renamed_server_records = build_two_saved_server_records();
    renamed_server_records[0].server_name = Some("desk".to_string());

    assert_eq!(
        find_unchanged_saved_server(
            &build_server_store(renamed_server_records),
            "laptop.local:7654",
            &held_saved_server,
        )
        .expect_err("it answers to another name now")
        .to_string(),
        "invalid arguments: laptop.local:7654 changed while the questions were open, \
         so nothing was saved; run `koshi remote edit laptop.local:7654` again"
    );
}

/// Another koshi saved a saved server whose address is this saved server's name while the
/// questions were open, so the selector answers for two saved servers now.
#[test]
fn a_selector_that_answers_for_two_saved_servers_meanwhile_refuses_the_edit() {
    let held_saved_server = build_two_saved_server_records().remove(0);
    let mut ambiguous_server_records = build_two_saved_server_records();
    ambiguous_server_records[1].server_address = "work".to_string();

    assert_eq!(
        find_unchanged_saved_server(
            &build_server_store(ambiguous_server_records),
            "work",
            &held_saved_server,
        )
        .expect_err("work answers for two records")
        .to_string(),
        "invalid arguments: work is the name of one saved server and the address of \
         another; run `koshi remote list` and name the one you mean"
    );
}
