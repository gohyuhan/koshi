//! Tests for the `remote` verbs: how the subcommands parse, what each answer
//! renders to, that no rendering prints a secret, which names and addresses a
//! record may take, which fingerprint a changed record keeps, what the store a
//! settled record goes into holds, that a refused placement changes nothing,
//! and which change another koshi made while the questions were open refuses
//! an edit.

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
            fingerprint: Some("aa".repeat(32)),
            added_at: at(10),
            last_used_at: Some(at(20)),
        },
        SavedServer {
            name: None,
            address: "10.0.0.4:7654".to_string(),
            secret: ConnectionToken::new("beef"),
            fingerprint: Some("bb".repeat(32)),
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

/// A store holding the two records `two_records` builds.
fn store_of(records: Vec<SavedServer>) -> ServerStore {
    let mut store = ServerStore::new();
    store.records = records;
    store
}

#[test]
fn a_saved_server_is_found_by_its_name_and_by_its_address() {
    let store = store_of(two_records());

    assert_eq!(
        named(&store, "work").expect("the name is saved").address,
        "laptop.local:7654"
    );
    assert_eq!(
        named(&store, "10.0.0.4:7654")
            .expect("the address is saved")
            .address,
        "10.0.0.4:7654"
    );
}

#[test]
fn a_server_the_store_does_not_hold_is_refused_naming_the_listing_command() {
    let store = store_of(two_records());

    assert_eq!(
        named(&store, "desk")
            .expect_err("nothing is saved under desk")
            .to_string(),
        "invalid arguments: no saved server is named desk; run `koshi remote list`"
    );
}

/// One word answering for two records — one record's chosen name, another
/// record's address — is refused rather than resolved to either of them.
#[test]
fn a_word_naming_one_server_and_addressing_another_is_refused_as_ambiguous() {
    let store = store_of(vec![
        SavedServer {
            name: Some("desk:7654".to_string()),
            address: "laptop.local:7654".to_string(),
            secret: ConnectionToken::new("f00d"),
            fingerprint: Some("aa".repeat(32)),
            added_at: at(10),
            last_used_at: None,
        },
        SavedServer {
            name: None,
            address: "desk:7654".to_string(),
            secret: ConnectionToken::new("beef"),
            fingerprint: Some("bb".repeat(32)),
            added_at: at(20),
            last_used_at: None,
        },
    ]);

    assert_eq!(
        named(&store, "desk:7654")
            .expect_err("two records answer to that word")
            .to_string(),
        "invalid arguments: desk:7654 is the name of one saved server and the address of \
         another; run `koshi remote list` and name the one you mean"
    );
}

#[test]
fn a_new_takes_no_argument_and_an_edit_takes_the_server_it_changes() {
    assert_eq!(
        remote_command(&["koshi", "remote", "new"]),
        RemoteCommand::New
    );
    assert_eq!(
        remote_command(&["koshi", "remote", "edit", "work"]),
        RemoteCommand::Edit {
            server: "work".to_string()
        }
    );
}

#[test]
fn an_edit_with_no_server_is_a_usage_error() {
    let err = Cli::try_parse_from(["koshi", "remote", "edit"]).expect_err("argv must not parse");
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn a_checked_record_renders_its_name_and_address_and_an_unchecked_one_says_when_it_pins() {
    let mut record = two_records().remove(0);
    assert_eq!(
        output::render_remote_saved(&record),
        "saved work at laptop.local:7654.\n"
    );
    assert_eq!(
        output::render_remote_updated(&record),
        "updated work at laptop.local:7654.\n"
    );

    record.fingerprint = None;
    assert_eq!(
        output::render_remote_saved(&record),
        "saved work at laptop.local:7654; its certificate is pinned on the first connection.\n"
    );
}

#[test]
fn a_record_with_no_name_renders_its_address_alone() {
    let record = two_records().remove(1);
    assert_eq!(
        output::render_remote_updated(&record),
        "updated 10.0.0.4:7654.\n"
    );
}

#[test]
fn a_discarded_answer_says_nothing_was_saved() {
    assert_eq!(output::render_remote_discarded(), "nothing was saved.\n");
}

#[test]
fn neither_settled_line_prints_the_secret_it_carries() {
    for record in two_records() {
        let saved = output::render_remote_saved(&record);
        let updated = output::render_remote_updated(&record);
        assert!(
            !saved.contains("f00d") && !saved.contains("beef"),
            "{saved}"
        );
        assert!(
            !updated.contains("f00d") && !updated.contains("beef"),
            "{updated}"
        );
    }
}

#[test]
fn an_empty_name_is_refused_and_a_free_one_is_taken() {
    let store = store_of(two_records());

    assert_eq!(
        free_name(&store, "")
            .expect_err("an empty name is refused")
            .to_string(),
        "invalid arguments: a name is needed, such as work"
    );
    free_name(&store, "desk").expect("no record answers to desk");
}

#[test]
fn a_name_with_the_shape_of_an_address_is_refused() {
    let store = store_of(two_records());

    assert!(
        free_name(&store, "desk.local:7654")
            .expect_err("a name must not be an address")
            .to_string()
            .contains("the shape of an address"),
        "the refusal names the shape"
    );
}

#[test]
fn a_name_another_record_answers_to_is_refused() {
    let store = store_of(two_records());

    assert_eq!(
        free_name(&store, "work")
            .expect_err("another record answers to it")
            .to_string(),
        "invalid arguments: work already answers for a saved server; \
         run `koshi remote list` and pick another name"
    );
}

/// A name is checked for the shape of an address before the store is asked,
/// so a name that is another record's address is refused for its shape.
#[test]
fn a_name_that_is_another_record_s_address_is_refused_for_its_shape() {
    let store = store_of(two_records());

    assert!(
        free_name(&store, "10.0.0.4:7654")
            .expect_err("that is another record's address")
            .to_string()
            .contains("the shape of an address"),
        "the refusal names the shape"
    );
}

#[test]
fn an_address_that_is_not_host_port_is_refused_naming_the_shape() {
    let store = store_of(two_records());

    for wrong in ["", "laptop.local", "laptop.local:door"] {
        assert_eq!(
            free_address(&store, wrong)
                .expect_err("that is not an address")
                .to_string(),
            format!(
                "invalid arguments: an address is host:port, such as laptop.local:7654, \
                 and {wrong} is not"
            )
        );
    }
    free_address(&store, "desk.local:7654").expect("no record answers to that address");
}

#[test]
fn an_address_another_record_answers_to_is_refused_naming_the_edit_command() {
    let store = store_of(two_records());

    assert_eq!(
        free_address(&store, "10.0.0.4:7654")
            .expect_err("another record holds that address")
            .to_string(),
        "invalid arguments: 10.0.0.4:7654 already answers for a saved server; \
         run `koshi remote edit 10.0.0.4:7654` to change it"
    );
}

/// What a changed record offers the check and keeps afterwards: the pinned
/// fingerprint while the address is unchanged, and nothing once the user
/// changed the address.
#[test]
fn an_address_that_changed_drops_what_was_held_and_an_unchanged_one_keeps_it() {
    let held = || Some("bb".repeat(32));

    assert_eq!(kept_pin(held(), false), held());
    assert_eq!(kept_pin(held(), true), None);
    assert_eq!(kept_pin(None, false), None);
    assert_eq!(kept_pin(None, true), None);
}

/// A record named `desk` at `desk.local:7654`, pinning nothing.
fn a_new_record() -> SavedServer {
    SavedServer {
        name: Some("desk".to_string()),
        address: "desk.local:7654".to_string(),
        secret: ConnectionToken::new("cafe"),
        fingerprint: None,
        added_at: at(40),
        last_used_at: None,
    }
}

#[test]
fn a_new_record_joins_the_records_already_there() {
    let mut settled = store_of(two_records());
    place(&mut settled, &a_new_record(), None).expect("its name and address are free");

    assert_eq!(
        settled
            .records
            .iter()
            .map(|record| record.address.as_str())
            .collect::<Vec<_>>(),
        vec!["laptop.local:7654", "10.0.0.4:7654", "desk.local:7654"]
    );
}

#[test]
fn a_replaced_record_leaves_the_store_and_the_new_one_takes_its_place() {
    let mut moved = two_records().remove(0);
    moved.address = "desk.local:7655".to_string();

    let mut settled = store_of(two_records());
    place(&mut settled, &moved, Some("work")).expect("the record moves");

    assert_eq!(
        settled
            .records
            .iter()
            .map(|record| (record.name.as_deref(), record.address.as_str()))
            .collect::<Vec<_>>(),
        vec![(None, "10.0.0.4:7654"), (Some("work"), "desk.local:7655")],
        "one record answers for work, at the address the edit typed"
    );
}

#[test]
fn a_record_that_keeps_its_own_name_and_address_is_placed_back() {
    let mut same = two_records().remove(0);
    same.secret = ConnectionToken::new("new secret");

    let mut settled = store_of(two_records());
    place(&mut settled, &same, Some("work")).expect("it may keep both");

    assert_eq!(settled.records.len(), 2);
    assert_eq!(
        settled
            .records
            .iter()
            .find(|record| record.name.as_deref() == Some("work"))
            .expect("work is still saved")
            .secret,
        ConnectionToken::new("new secret")
    );
}

/// The record was forgotten while the questions were open. Placing it back
/// would return a secret the user dropped, so it is refused.
#[test]
fn a_record_that_is_no_longer_saved_is_refused_rather_than_put_back() {
    assert_eq!(
        place(&mut ServerStore::new(), &a_new_record(), Some("work"))
            .expect_err("nothing answers to work now")
            .to_string(),
        "invalid arguments: no saved server is named work; run `koshi remote list`"
    );
}

/// Another koshi saved a record under this name while the questions were
/// open. The name is taken now, so the placement is refused.
#[test]
fn a_name_another_record_took_meanwhile_is_refused_and_changes_nothing() {
    let mut taken = a_new_record();
    taken.name = Some("work".to_string());
    let mut store = store_of(two_records());

    assert_eq!(
        place(&mut store, &taken, None)
            .expect_err("work is taken")
            .to_string(),
        "invalid arguments: work already answers for a saved server; \
         run `koshi remote list` and pick another name"
    );
    assert_eq!(store.records, two_records(), "the refusal wrote nothing");
}

#[test]
fn an_address_another_record_took_meanwhile_is_refused_and_changes_nothing() {
    let mut taken = a_new_record();
    taken.address = "10.0.0.4:7654".to_string();
    let mut store = store_of(two_records());

    assert_eq!(
        place(&mut store, &taken, None)
            .expect_err("that address is taken")
            .to_string(),
        "invalid arguments: 10.0.0.4:7654 already answers for a saved server; \
         run `koshi remote edit 10.0.0.4:7654` to change it"
    );
    assert_eq!(store.records, two_records(), "the refusal wrote nothing");
}

/// The replaced record leaves before the checks, so a refusal after that point
/// must put it back rather than leave the store missing it.
#[test]
fn a_replacement_refused_after_the_record_left_puts_every_record_back() {
    let mut moved = two_records().remove(0);
    moved.address = "10.0.0.4:7654".to_string();
    let mut store = store_of(two_records());

    assert_eq!(
        place(&mut store, &moved, Some("work"))
            .expect_err("the other record already answers to that address")
            .to_string(),
        "invalid arguments: 10.0.0.4:7654 already answers for a saved server; \
         run `koshi remote edit 10.0.0.4:7654` to change it"
    );
    assert_eq!(store.records, two_records(), "work is still saved");
}

#[test]
fn a_record_with_no_name_is_placed_without_a_name_check() {
    let mut nameless = a_new_record();
    nameless.name = None;

    let mut settled = store_of(two_records());
    place(&mut settled, &nameless, None).expect("its address is free");

    assert_eq!(settled.records.len(), 3);
    assert_eq!(settled.records[2].name, None);
}

/// The record on disk still holds everything the questions were answered
/// against, so the edit goes on.
#[test]
fn a_record_no_other_koshi_touched_is_taken_as_it_stands() {
    let held = two_records().remove(0);

    let now_held = record_if_unchanged(&store_of(two_records()), "work", &held)
        .expect("nothing about it changed");

    assert_eq!(now_held, held);
}

/// Another koshi dialled this server while the questions were open, so its
/// last-used time moved. That is not a change to what the questions asked
/// about, and the fresh time is the one the edit carries.
#[test]
fn a_last_used_time_another_koshi_stamped_is_carried_and_does_not_refuse() {
    let held = two_records().remove(0);
    let mut dialled = two_records();
    dialled[0].last_used_at = Some(at(99));

    let now_held = record_if_unchanged(&store_of(dialled), "work", &held)
        .expect("only the last-used time moved");

    assert_eq!(now_held.last_used_at, Some(at(99)));
}

/// Another koshi replaced this record while the questions were open, so its
/// added time moved. The edit carries the fresh one rather than putting the
/// old one back.
#[test]
fn an_added_time_another_koshi_wrote_is_carried_and_does_not_refuse() {
    let held = two_records().remove(0);
    let mut resaved = two_records();
    resaved[0].added_at = at(77);

    let now_held =
        record_if_unchanged(&store_of(resaved), "work", &held).expect("only the added time moved");

    assert_eq!(now_held.added_at, at(77));
}

/// Another koshi replaced the secret while the questions were open. Writing
/// the edit would put the old secret back, so it is refused.
#[test]
fn a_secret_another_koshi_replaced_meanwhile_refuses_the_edit() {
    let held = two_records().remove(0);
    let mut replaced = two_records();
    replaced[0].secret = ConnectionToken::new("newer");

    assert_eq!(
        record_if_unchanged(&store_of(replaced), "work", &held)
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
    let mut held = two_records().remove(0);
    held.fingerprint = None;

    assert_eq!(
        record_if_unchanged(&store_of(two_records()), "work", &held)
            .expect_err("a fingerprint appeared")
            .to_string(),
        "invalid arguments: work changed while the questions were open, so nothing \
         was saved; run `koshi remote edit work` again"
    );
}

#[test]
fn an_address_another_koshi_moved_meanwhile_refuses_the_edit() {
    let held = two_records().remove(0);
    let mut moved = two_records();
    moved[0].address = "laptop.local:7655".to_string();

    assert_eq!(
        record_if_unchanged(&store_of(moved), "work", &held)
            .expect_err("it sits at another address now")
            .to_string(),
        "invalid arguments: work changed while the questions were open, so nothing \
         was saved; run `koshi remote edit work` again"
    );
}

#[test]
fn a_record_another_koshi_forgot_meanwhile_refuses_the_edit() {
    let held = two_records().remove(0);

    assert_eq!(
        record_if_unchanged(&ServerStore::new(), "work", &held)
            .expect_err("nothing answers to work now")
            .to_string(),
        "invalid arguments: no saved server is named work; run `koshi remote list`"
    );
}

/// Another koshi renamed this record while the questions were open. Writing
/// the edit would put the old name back, so it is refused.
#[test]
fn a_name_another_koshi_changed_meanwhile_refuses_the_edit() {
    let held = two_records().remove(0);
    let mut renamed = two_records();
    renamed[0].name = Some("desk".to_string());

    assert_eq!(
        record_if_unchanged(&store_of(renamed), "laptop.local:7654", &held)
            .expect_err("it answers to another name now")
            .to_string(),
        "invalid arguments: laptop.local:7654 changed while the questions were open, \
         so nothing was saved; run `koshi remote edit laptop.local:7654` again"
    );
}

/// Another koshi saved a record whose address is this record's name while the
/// questions were open, so the selector answers for two records now.
#[test]
fn a_selector_that_answers_for_two_records_meanwhile_refuses_the_edit() {
    let held = two_records().remove(0);
    let mut crowded = two_records();
    crowded[1].address = "work".to_string();

    assert_eq!(
        record_if_unchanged(&store_of(crowded), "work", &held)
            .expect_err("work answers for two records")
            .to_string(),
        "invalid arguments: work is the name of one saved server and the address of \
         another; run `koshi remote list` and name the one you mean"
    );
}
