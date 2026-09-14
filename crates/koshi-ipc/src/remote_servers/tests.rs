//! Tests for the saved-server store: where the file lives, the write/read
//! roundtrip through the atomic writer, the private mode of the file, where
//! the lock guarding a change lives, and looking a server up by its name or
//! its address.

use std::time::Duration;

use tempfile::TempDir;

use super::*;

/// Finds the one saved server a selector names, or returns `None` when it names
/// none or names more than one.
fn find_saved_server<'a>(store: &'a ServerStore, server_selector: &str) -> Option<&'a SavedServer> {
    match store.find_saved_server(server_selector) {
        SavedServerLookup::Saved(saved_server_record) => Some(saved_server_record),
        SavedServerLookup::NotSaved | SavedServerLookup::Ambiguous => None,
    }
}

/// A fixed point on the clock, `elapsed_seconds` seconds after the epoch.
fn build_timestamp(elapsed_seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(elapsed_seconds)
}

/// A saved server at `server_address`, named `server_name`.
fn build_saved_server(server_name: Option<&str>, server_address: &str) -> SavedServer {
    SavedServer {
        server_name: server_name.map(str::to_string),
        server_address: server_address.to_string(),
        connection_token: ConnectionToken::from_secret("a secret"),
        certificate_fingerprint: Some("ab".repeat(32)),
        added_at: build_timestamp(100),
        last_used_at: None,
    }
}

/// A store holding one server named `work` at `laptop.local:7654`.
fn build_single_server_store() -> ServerStore {
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(Some("work"), "laptop.local:7654"))
        .expect("a store with nothing in it takes any name");
    store
}

#[test]
fn the_store_path_is_remote_servers_under_the_data_dir() {
    assert_eq!(
        resolve_server_store_path(Path::new("/home/ada/.local/share/koshi")),
        Path::new("/home/ada/.local/share/koshi/remote/servers")
    );
}

/// The lock sits beside the store, not on it. The store file is replaced by a
/// rename; a lock held on the store path guards the file the rename removed.
#[test]
fn the_lock_path_sits_beside_the_store_and_is_not_the_store() {
    let data_directory = Path::new("/home/ada/.local/share/koshi");

    assert_eq!(
        resolve_server_store_lock_path(data_directory),
        Path::new("/home/ada/.local/share/koshi/remote/servers.lock")
    );
    assert_ne!(
        resolve_server_store_lock_path(data_directory),
        resolve_server_store_path(data_directory)
    );
}

#[test]
fn a_path_with_no_file_reads_as_an_empty_store() {
    let test_directory = TempDir::new().expect("make a test directory");
    let store =
        ServerStore::load_server_store_from_path(&resolve_server_store_path(test_directory.path()))
            .expect("read a missing store");
    assert_eq!(store, ServerStore::new());
}

#[test]
fn a_written_store_reads_back_the_same() {
    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    let store = build_single_server_store();
    store
        .write_server_store_to_path(&server_store_path)
        .expect("write the store");
    assert_eq!(
        ServerStore::load_server_store_from_path(&server_store_path).expect("read the store"),
        store
    );
}

#[cfg(unix)]
#[test]
fn the_written_file_and_its_directory_are_private_to_the_owner() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    build_single_server_store()
        .write_server_store_to_path(&server_store_path)
        .expect("write the store");

    let compute_file_mode = |file_path: &Path| {
        std::fs::metadata(file_path)
            .expect("stat file path")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(compute_file_mode(&server_store_path), 0o600);
    assert_eq!(
        compute_file_mode(server_store_path.parent().expect("the remote directory")),
        0o700
    );
}

#[test]
fn a_file_at_another_format_number_is_refused() {
    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    let mut store = build_single_server_store();
    store.store_format = SERVER_STORE_FORMAT + 1;
    store
        .write_server_store_to_path(&server_store_path)
        .expect("write the store");
    let unsupported_format_error = ServerStore::load_server_store_from_path(&server_store_path)
        .expect_err("another format number is refused");
    assert_eq!(
        unsupported_format_error.to_string(),
        format!(
            "the saved servers file at {} is unreadable: format {} is not the \
             {SERVER_STORE_FORMAT} this build reads",
            server_store_path.display(),
            SERVER_STORE_FORMAT + 1
        )
    );
}

#[test]
fn a_server_is_found_by_its_name_and_by_its_address() {
    let store = build_single_server_store();
    assert_eq!(
        find_saved_server(&store, "work").map(|saved_server| saved_server.server_address.as_str()),
        Some("laptop.local:7654")
    );
    assert_eq!(
        find_saved_server(&store, "laptop.local:7654")
            .map(|saved_server| saved_server.server_address.as_str()),
        Some("laptop.local:7654")
    );
    assert_eq!(store.find_saved_server("desk"), SavedServerLookup::NotSaved);
}

#[test]
fn a_selector_that_is_one_record_s_name_and_another_s_address_is_neither() {
    // `work` is the second record's name and the first record's address.
    // Pushed rather than saved: `save_server` refuses this pair — see
    // `a_name_another_machine_already_answers_to_by_address_is_refused`.
    let mut store = ServerStore::new();
    store.saved_servers.push(build_saved_server(None, "work"));
    store
        .saved_servers
        .push(build_saved_server(Some("work"), "laptop.local:7654"));
    assert_eq!(
        find_saved_server(&store, "work").map(|saved_server| saved_server.server_address.as_str()),
        None
    );
}

#[test]
fn saving_the_same_address_again_takes_the_place_of_the_old_record() {
    let mut store = build_single_server_store();
    store
        .save_server(build_saved_server(Some("home"), "laptop.local:7654"))
        .expect("the name is free");
    assert_eq!(store.saved_servers.len(), 1);
    assert_eq!(store.saved_servers[0].server_name.as_deref(), Some("home"));
}

#[test]
fn forgetting_a_server_returns_its_address_and_drops_it() {
    let mut store = build_single_server_store();
    assert_eq!(
        store.forget_saved_server("work"),
        Some("laptop.local:7654".to_string())
    );
    assert!(store.saved_servers.is_empty());
    assert_eq!(store.forget_saved_server("work"), None);
}

#[test]
fn a_replaced_secret_lands_on_the_named_server() {
    let mut store = build_single_server_store();
    assert_eq!(
        store.set_connection_token("work", ConnectionToken::from_secret("a rotated secret")),
        Some("laptop.local:7654".to_string())
    );
    assert_eq!(
        store.saved_servers[0].connection_token,
        ConnectionToken::from_secret("a rotated secret")
    );
    assert_eq!(
        store.set_connection_token("desk", ConnectionToken::from_secret("x")),
        None
    );
}

#[test]
fn touching_a_server_stamps_its_last_used_time() {
    let mut store = build_single_server_store();
    store.mark_server_used("work", build_timestamp(500));
    assert_eq!(
        store.saved_servers[0].last_used_at,
        Some(build_timestamp(500))
    );
    store.mark_server_used("desk", build_timestamp(900));
    assert_eq!(
        store.saved_servers[0].last_used_at,
        Some(build_timestamp(500))
    );
}

#[test]
fn describing_a_saved_server_writes_its_secret_redacted() {
    let mut saved_server = build_saved_server(Some("work"), "laptop.local:7654");
    saved_server.connection_token =
        ConnectionToken::from_secret("the secret the operator handed out");

    let described_server = format!("{saved_server:?}");
    assert!(
        !described_server.contains("the secret the operator handed out"),
        "a described record carries no secret: {described_server}"
    );
    assert!(
        described_server.contains("ConnectionToken(***)"),
        "a described record writes its secret redacted: {described_server}"
    );
    assert_eq!(format!("{}", saved_server.connection_token), "***");
}

#[test]
fn a_record_carries_the_four_fields_a_listing_reports_and_the_secret_it_leaves_behind() {
    let saved_server = build_saved_server(Some("work"), "laptop.local:7654");
    let encoded_saved_server = serde_json::to_value(&saved_server).expect("a record encodes");
    let saved_server_fields = encoded_saved_server
        .as_object()
        .expect("a record encodes as an object");

    let mut field_names: Vec<&str> = saved_server_fields.keys().map(String::as_str).collect();
    field_names.sort_unstable();
    assert_eq!(
        field_names,
        [
            "added_at",
            "address",
            "fingerprint",
            "last_used_at",
            "name",
            "secret"
        ]
    );
    assert_eq!(
        saved_server_fields["secret"],
        serde_json::Value::String("a secret".to_string()),
        "the secret travels in the file, so the next connection presents it"
    );
}

#[test]
fn a_record_with_no_pinned_fingerprint_travels_without_the_field_and_reads_back() {
    let mut saved_server = build_saved_server(Some("work"), "laptop.local:7654");
    saved_server.certificate_fingerprint = None;

    let encoded_saved_server = serde_json::to_value(&saved_server).expect("a record encodes");
    let saved_server_fields = encoded_saved_server
        .as_object()
        .expect("a record encodes as an object");
    assert!(
        !saved_server_fields.contains_key("fingerprint"),
        "no pinned fingerprint leaves the file without the field: {saved_server_fields:?}"
    );

    let decoded_saved_server: SavedServer =
        serde_json::from_value(encoded_saved_server).expect("the record reads back");
    assert_eq!(decoded_saved_server, saved_server);
}

#[test]
fn a_file_written_when_every_record_carried_a_fingerprint_still_reads() {
    let previous_saved_server_json = serde_json::json!({
        "name": "work",
        "address": "laptop.local:7654",
        "secret": "a secret",
        "fingerprint": "ab".repeat(32),
        "added_at": SystemTime::UNIX_EPOCH,
        "last_used_at": null,
    });

    let decoded_saved_server: SavedServer =
        serde_json::from_value(previous_saved_server_json).expect("the old shape reads");
    assert_eq!(
        decoded_saved_server.certificate_fingerprint,
        Some("ab".repeat(32))
    );
}

#[test]
fn pinning_puts_the_fingerprint_on_the_named_record_and_an_ambiguous_name_pins_nothing() {
    let mut store = build_single_server_store();
    store.saved_servers[0].certificate_fingerprint = None;

    store.pin_certificate_fingerprint("work", "ab".repeat(32));
    assert_eq!(
        store.saved_servers[0].certificate_fingerprint,
        Some("ab".repeat(32))
    );

    store.pin_certificate_fingerprint("nobody", "ff".repeat(32));
    assert_eq!(
        store.saved_servers[0].certificate_fingerprint,
        Some("ab".repeat(32)),
        "a selector naming no record pins nothing"
    );
}

#[test]
fn a_selector_naming_one_record_and_addressing_another_names_neither() {
    // A hand-written file can hold what `save` refuses under rule 3.
    let mut store = ServerStore::new();
    store.saved_servers.push(SavedServer {
        server_name: Some("target.example:7654".to_string()),
        server_address: "other.example:7654".to_string(),
        connection_token: ConnectionToken::generate(),
        certificate_fingerprint: Some("aa".repeat(32)),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });
    store.saved_servers.push(SavedServer {
        server_name: None,
        server_address: "target.example:7654".to_string(),
        connection_token: ConnectionToken::generate(),
        certificate_fingerprint: Some("bb".repeat(32)),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });

    assert_eq!(
        store.find_saved_server("target.example:7654"),
        SavedServerLookup::Ambiguous,
        "the selector answers for two different saved_servers, so it answers for neither"
    );
    assert_eq!(store.forget_saved_server("target.example:7654"), None);
    assert_eq!(store.saved_servers.len(), 2, "and it removed nothing");
    assert_eq!(
        store.set_connection_token("target.example:7654", ConnectionToken::generate()),
        None
    );
}

#[test]
fn a_selector_matching_one_record_by_both_its_name_and_its_address_is_that_record() {
    // Both matches are the same record, so it is one answer.
    let mut store = ServerStore::new();
    store.saved_servers.push(SavedServer {
        server_name: Some("desk.local:7654".to_string()),
        server_address: "desk.local:7654".to_string(),
        connection_token: ConnectionToken::generate(),
        certificate_fingerprint: Some("cc".repeat(32)),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });

    let found_saved_server =
        find_saved_server(&store, "desk.local:7654").expect("one record answers both ways");
    assert_eq!(found_saved_server.server_address, "desk.local:7654");
}

#[test]
fn an_unambiguous_name_and_an_unambiguous_address_each_find_their_own_record() {
    let mut store = ServerStore::new();
    store.saved_servers.push(SavedServer {
        server_name: Some("work".to_string()),
        server_address: "desk.local:7654".to_string(),
        connection_token: ConnectionToken::generate(),
        certificate_fingerprint: Some("dd".repeat(32)),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });
    store.saved_servers.push(SavedServer {
        server_name: None,
        server_address: "laptop.local:7654".to_string(),
        connection_token: ConnectionToken::generate(),
        certificate_fingerprint: Some("ee".repeat(32)),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });

    assert_eq!(
        find_saved_server(&store, "work")
            .expect("the named record")
            .server_address,
        "desk.local:7654"
    );
    assert_eq!(
        find_saved_server(&store, "laptop.local:7654")
            .expect("the addressed record")
            .server_address,
        "laptop.local:7654"
    );
}

// `save_server` keeps three things true, and the tests from here down pin all three:
//   1. one address appears once,
//   2. one name appears once,
//   3. no name is another record's address.
// A selector that matches more than one record matches none, which is what
// keeps a store this build did not write from resolving to the wrong machine.

#[test]
fn saving_an_address_again_replaces_that_machine_s_record() {
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(Some("work"), "desk.local:7654"))
        .expect("the first save");
    let mut replacement_saved_server = build_saved_server(Some("work"), "desk.local:7654");
    replacement_saved_server.certificate_fingerprint = Some("ff".repeat(32));
    store
        .save_server(replacement_saved_server)
        .expect("the same machine saves again");

    assert_eq!(store.saved_servers.len(), 1, "one address is one record");
    assert_eq!(
        store.saved_servers[0].certificate_fingerprint,
        Some("ff".repeat(32))
    );
}

#[test]
fn a_name_another_machine_holds_is_refused_rather_than_doubled() {
    // Rule 2: one name appears once.
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(Some("work"), "desk.local:7654"))
        .expect("the first save");

    let save_refusal = store
        .save_server(build_saved_server(Some("work"), "laptop.local:7654"))
        .expect_err("a second machine cannot take the name");

    assert_eq!(save_refusal.server_name, "work");
    assert_eq!(save_refusal.server_address, "desk.local:7654");
    assert_eq!(
        save_refusal.to_string(),
        "the name work already belongs to desk.local:7654; run `koshi remote forget work` \
         first, or pick another name"
    );
    assert_eq!(store.saved_servers.len(), 1, "and nothing was added");
    assert_eq!(
        find_saved_server(&store, "work")
            .expect("the name still answers")
            .server_address,
        "desk.local:7654",
        "for the machine that had it"
    );
}

#[test]
fn a_name_another_machine_already_answers_to_by_address_is_refused() {
    // Rule 3, reached by a name: `work` is already one record's address.
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(None, "work"))
        .expect("the first save");

    let save_refusal = store
        .save_server(build_saved_server(Some("work"), "laptop.local:7654"))
        .expect_err("a word another record answers to cannot be taken as a name");

    assert_eq!(save_refusal.server_name, "work");
    assert_eq!(save_refusal.server_address, "work");
    assert_eq!(store.saved_servers.len(), 1, "and nothing was added");
    assert_eq!(
        find_saved_server(&store, "work")
            .expect("the word still answers")
            .server_address,
        "work",
        "for the machine that had it"
    );
}

#[test]
fn an_address_another_machine_already_answers_to_by_name_is_refused() {
    // Rule 3, reached by an address: `work` is already one record's name.
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(Some("work"), "laptop.local:7654"))
        .expect("the first save");

    let save_refusal = store
        .save_server(build_saved_server(None, "work"))
        .expect_err("a word another record answers to cannot be taken as an address");

    assert_eq!(save_refusal.server_name, "work");
    assert_eq!(save_refusal.server_address, "laptop.local:7654");
    assert_eq!(store.saved_servers.len(), 1, "and nothing was added");
    assert_eq!(
        find_saved_server(&store, "work")
            .expect("the word still answers")
            .server_address,
        "laptop.local:7654"
    );
}

#[test]
fn a_machine_keeps_its_own_name_when_it_saves_again() {
    // The three rules compare against other saved_servers only.
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(Some("work"), "desk.local:7654"))
        .expect("the first save");

    let mut replacement_saved_server = build_saved_server(Some("work"), "desk.local:7654");
    replacement_saved_server.certificate_fingerprint = Some("ff".repeat(32));
    store
        .save_server(replacement_saved_server)
        .expect("the machine that holds the name may keep it");

    assert_eq!(store.saved_servers.len(), 1);
    assert_eq!(
        store.saved_servers[0].certificate_fingerprint,
        Some("ff".repeat(32))
    );
}

#[test]
fn a_free_name_reports_free_and_a_held_one_does_not() {
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(Some("work"), "desk.local:7654"))
        .expect("the first save");

    assert!(store.is_server_name_free("home", "laptop.local:7654"));
    assert!(
        store.is_server_name_free("work", "desk.local:7654"),
        "the machine that already holds the name may keep it"
    );
    assert!(!store.is_server_name_free("work", "laptop.local:7654"));
    assert!(
        !store.is_server_name_free("desk.local:7654", "laptop.local:7654"),
        "a word another record answers to by address is not free either"
    );
}

#[test]
fn a_selector_matching_two_records_matches_neither_however_they_match() {
    // A store written by hand can hold what save refuses. Whichever way two
    // saved_servers answer to one word, the answer is none.
    let mut store = ServerStore::new();
    store
        .saved_servers
        .push(build_saved_server(Some("work"), "desk.local:7654"));
    store
        .saved_servers
        .push(build_saved_server(Some("work"), "laptop.local:7654"));

    assert_eq!(
        store.find_saved_server("work"),
        SavedServerLookup::Ambiguous,
        "two names, no answer"
    );
    assert_eq!(store.forget_saved_server("work"), None);
    assert_eq!(store.saved_servers.len(), 2, "and nothing was removed");
    assert_eq!(
        store.set_connection_token("work", ConnectionToken::generate()),
        None
    );
    store.mark_server_used("work", SystemTime::now());
    assert_eq!(
        store.saved_servers[0].last_used_at, None,
        "and nothing was stamped"
    );
    assert_eq!(store.saved_servers[1].last_used_at, None);
}

#[test]
fn each_machine_still_answers_to_its_own_name_and_its_own_address() {
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(Some("work"), "desk.local:7654"))
        .expect("save the first");
    store
        .save_server(build_saved_server(Some("home"), "laptop.local:7654"))
        .expect("save the second");

    for (server_selector, expected_server_address) in [
        ("work", "desk.local:7654"),
        ("home", "laptop.local:7654"),
        ("desk.local:7654", "desk.local:7654"),
        ("laptop.local:7654", "laptop.local:7654"),
    ] {
        assert_eq!(
            find_saved_server(&store, server_selector)
                .expect("one record answers")
                .server_address,
            expected_server_address,
            "the selector {server_selector}"
        );
    }
}

#[test]
fn an_ambiguous_selector_says_so_and_never_reads_as_nothing_saved() {
    // `Ambiguous`, never `NotSaved`.
    let mut store = ServerStore::new();
    store.saved_servers.push(build_saved_server(
        Some("laptop.local:7654"),
        "desk.local:7654",
    ));
    store
        .saved_servers
        .push(build_saved_server(None, "laptop.local:7654"));

    assert_eq!(
        store.find_saved_server("laptop.local:7654"),
        SavedServerLookup::Ambiguous
    );
    assert_eq!(
        store.find_saved_server("nothing-is-saved-here"),
        SavedServerLookup::NotSaved
    );
    assert_eq!(
        store.find_saved_server("desk.local:7654"),
        SavedServerLookup::Saved(&store.saved_servers[0])
    );
}

#[test]
fn a_selector_two_records_answer_to_pins_nothing() {
    // A hand-written file can hold what `save_server` refuses under rule 3.
    let mut store = ServerStore::new();
    store.saved_servers.push(SavedServer {
        server_name: Some("target.example:7654".to_string()),
        server_address: "other.example:7654".to_string(),
        connection_token: ConnectionToken::generate(),
        certificate_fingerprint: None,
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });
    store.saved_servers.push(SavedServer {
        server_name: None,
        server_address: "target.example:7654".to_string(),
        connection_token: ConnectionToken::generate(),
        certificate_fingerprint: None,
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });

    store.pin_certificate_fingerprint("target.example:7654", "ab".repeat(32));

    assert_eq!(store.saved_servers[0].certificate_fingerprint, None);
    assert_eq!(store.saved_servers[1].certificate_fingerprint, None);
}

#[test]
fn a_record_with_no_pinned_fingerprint_survives_the_file_it_is_written_to() {
    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    let mut store = build_single_server_store();
    store.saved_servers[0].certificate_fingerprint = None;

    store
        .write_server_store_to_path(&server_store_path)
        .expect("write the store");

    assert_eq!(
        ServerStore::load_server_store_from_path(&server_store_path).expect("read it back"),
        store
    );
}

#[test]
fn an_empty_store_answers_to_nothing_and_forgets_nothing() {
    let mut store = ServerStore::new();

    assert_eq!(store.find_saved_server("work"), SavedServerLookup::NotSaved);
    assert_eq!(store.find_saved_server(""), SavedServerLookup::NotSaved);
    assert_eq!(store.forget_saved_server("work"), None);
    assert_eq!(
        store.set_connection_token("work", ConnectionToken::from_secret("x")),
        None
    );
    assert_eq!(store.saved_servers, Vec::new());
}

#[test]
fn a_store_holding_one_record_is_written_as_these_exact_bytes() {
    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    build_single_server_store()
        .write_server_store_to_path(&server_store_path)
        .expect("write the store");

    let serialized_store_text = std::fs::read_to_string(&server_store_path).expect("read the file");
    assert_eq!(
        serialized_store_text,
        format!(
            r#"{{"format":{SERVER_STORE_FORMAT},"records":[{{"name":"work","address":"laptop.local:7654","secret":"a secret","fingerprint":"{}","added_at":{{"secs_since_epoch":100,"nanos_since_epoch":0}},"last_used_at":null}}]}}"#,
            "ab".repeat(32)
        )
    );
}

#[test]
fn junk_bytes_are_an_unreadable_saved_servers_file() {
    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    std::fs::create_dir_all(server_store_path.parent().expect("the remote directory"))
        .expect("make it");
    std::fs::write(&server_store_path, b"not a store").expect("write junk");

    let unreadable_store_error =
        ServerStore::load_server_store_from_path(&server_store_path).expect_err("junk is refused");
    let error_detail = serde_json::from_slice::<ServerStore>(b"not a store")
        .expect_err("junk does not decode")
        .to_string();
    assert_eq!(
        unreadable_store_error.to_string(),
        format!(
            "the saved servers file at {} is unreadable: {error_detail}",
            server_store_path.display()
        )
    );
}

#[test]
fn a_store_whose_bytes_stop_part_way_is_unreadable() {
    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    build_single_server_store()
        .write_server_store_to_path(&server_store_path)
        .expect("write the store");
    let serialized_store_bytes = std::fs::read(&server_store_path).expect("read the file");
    let truncated_store_bytes = &serialized_store_bytes[..serialized_store_bytes.len() / 2];
    std::fs::write(&server_store_path, truncated_store_bytes).expect("write the first half");

    let truncated_store_error = ServerStore::load_server_store_from_path(&server_store_path)
        .expect_err("a cut file is refused");
    let error_detail = serde_json::from_slice::<ServerStore>(truncated_store_bytes)
        .expect_err("half a store does not decode")
        .to_string();
    assert_eq!(
        truncated_store_error.to_string(),
        format!(
            "the saved servers file at {} is unreadable: {error_detail}",
            server_store_path.display()
        )
    );
}

#[test]
fn a_record_carrying_an_unknown_field_makes_the_store_unreadable() {
    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    let serialized_store_json = format!(
        r#"{{"format":{SERVER_STORE_FORMAT},"records":[{{"name":"work","address":"laptop.local:7654","secret":"a secret","colour":"red","added_at":{{"secs_since_epoch":100,"nanos_since_epoch":0}},"last_used_at":null}}]}}"#
    );
    std::fs::create_dir_all(server_store_path.parent().expect("the remote directory"))
        .expect("make it");
    std::fs::write(&server_store_path, &serialized_store_json).expect("write the store by hand");

    let unknown_field_store_error = ServerStore::load_server_store_from_path(&server_store_path)
        .expect_err("an unknown field is refused");
    let error_detail = serde_json::from_str::<ServerStore>(&serialized_store_json)
        .expect_err("an unknown field does not decode")
        .to_string();
    assert_eq!(
        unknown_field_store_error.to_string(),
        format!(
            "the saved servers file at {} is unreadable: {error_detail}",
            server_store_path.display()
        )
    );
}

#[test]
fn a_store_carrying_an_unknown_top_level_field_is_unreadable() {
    let serialized_store_json =
        format!(r#"{{"owner":"ada","format":{SERVER_STORE_FORMAT},"records":[]}}"#);

    let unknown_field_store_error =
        serde_json::from_str::<ServerStore>(&serialized_store_json).expect_err("refused");
    assert_eq!(
        unknown_field_store_error.to_string(),
        "unknown field `owner`, expected `format` or `records` at line 1 column 8"
    );
}

#[test]
fn a_fingerprint_written_as_null_reads_back_as_none() {
    let saved_server_json = serde_json::json!({
        "name": "work",
        "address": "laptop.local:7654",
        "secret": "a secret",
        "fingerprint": null,
        "added_at": SystemTime::UNIX_EPOCH,
        "last_used_at": null,
    });

    let decoded_saved_server: SavedServer =
        serde_json::from_value(saved_server_json).expect("null reads");
    assert_eq!(decoded_saved_server.certificate_fingerprint, None);
}

#[test]
fn a_directory_where_the_store_belongs_is_refused_rather_than_read_as_empty() {
    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    std::fs::create_dir_all(&server_store_path).expect("make a directory at the store path");

    let directory_store_error = ServerStore::load_server_store_from_path(&server_store_path)
        .expect_err("a directory is not a store");
    let IpcError::RemoteFileUnreadable {
        remote_file,
        remote_file_path: reported_file_path,
        ..
    } = directory_store_error
    else {
        panic!(
            "a directory at the store path names the saved servers file: {directory_store_error}"
        );
    };
    assert_eq!(remote_file, RemoteFile::SavedServers);
    assert_eq!(reported_file_path, server_store_path.display().to_string());
}

#[test]
fn writing_where_the_directory_cannot_exist_is_a_saved_servers_write_failure() {
    let test_directory = TempDir::new().expect("make a test directory");
    std::fs::write(
        test_directory.path().join("remote"),
        b"a file, not a directory",
    )
    .expect("write it");
    let server_store_path = resolve_server_store_path(test_directory.path());

    let write_store_error = build_single_server_store()
        .write_server_store_to_path(&server_store_path)
        .expect_err("a file in the directory's place stops the write");
    let IpcError::RemoteFileWrite {
        remote_file,
        remote_file_path: reported_file_path,
        ..
    } = write_store_error
    else {
        panic!("a failed write names the saved servers file: {write_store_error}");
    };
    assert_eq!(remote_file, RemoteFile::SavedServers);
    assert_eq!(reported_file_path, server_store_path.display().to_string());
    assert!(
        !server_store_path.exists(),
        "and nothing was written at {}",
        server_store_path.display()
    );
}

#[cfg(unix)]
#[test]
fn a_store_file_that_was_group_readable_is_private_after_the_write() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().expect("make a test directory");
    let server_store_path = resolve_server_store_path(test_directory.path());
    build_single_server_store()
        .write_server_store_to_path(&server_store_path)
        .expect("write the store");
    std::fs::set_permissions(&server_store_path, std::fs::Permissions::from_mode(0o644))
        .expect("open the file up");

    build_single_server_store()
        .write_server_store_to_path(&server_store_path)
        .expect("write the store again");

    let file_mode = std::fs::metadata(&server_store_path)
        .expect("stat the store")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o600);
}

#[test]
fn saving_an_address_again_moves_its_record_to_the_end() {
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(Some("work"), "desk.local:7654"))
        .expect("save the first");
    store
        .save_server(build_saved_server(Some("home"), "laptop.local:7654"))
        .expect("save the second");

    let mut replacement_saved_server = build_saved_server(Some("work"), "desk.local:7654");
    replacement_saved_server.connection_token = ConnectionToken::from_secret("a rotated secret");
    store
        .save_server(replacement_saved_server.clone())
        .expect("the same machine saves again");

    assert_eq!(
        store.saved_servers,
        vec![
            build_saved_server(Some("home"), "laptop.local:7654"),
            replacement_saved_server
        ]
    );
}

#[test]
fn a_server_may_be_named_by_its_own_address() {
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(
            Some("desk.local:7654"),
            "desk.local:7654",
        ))
        .expect("a record may answer to one word both ways");

    assert_eq!(
        store.find_saved_server("desk.local:7654"),
        SavedServerLookup::Saved(&store.saved_servers[0])
    );
    assert!(store.is_server_name_free("desk.local:7654", "desk.local:7654"));
}

#[test]
fn a_name_several_records_hold_is_refused_naming_the_first_holder() {
    // A hand-written file can hold what `save_server` refuses under rule 2.
    let mut store = ServerStore::new();
    store
        .saved_servers
        .push(build_saved_server(Some("work"), "desk.local:7654"));
    store
        .saved_servers
        .push(build_saved_server(Some("work"), "laptop.local:7654"));

    let save_refusal = store
        .save_server(build_saved_server(Some("work"), "phone.local:7654"))
        .expect_err("the name is held");

    assert_eq!(save_refusal.server_name, "work");
    assert_eq!(save_refusal.server_address, "desk.local:7654");
    assert_eq!(store.saved_servers.len(), 2, "and nothing was added");
}

#[test]
fn forgetting_one_of_two_servers_leaves_the_other_where_it_was() {
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(Some("work"), "desk.local:7654"))
        .expect("save the first");
    store
        .save_server(build_saved_server(Some("home"), "laptop.local:7654"))
        .expect("save the second");

    assert_eq!(
        store.forget_saved_server("desk.local:7654"),
        Some("desk.local:7654".to_string())
    );

    assert_eq!(
        store.saved_servers,
        vec![build_saved_server(Some("home"), "laptop.local:7654")]
    );
    assert_eq!(store.find_saved_server("work"), SavedServerLookup::NotSaved);
    assert_eq!(
        store.find_saved_server("home"),
        SavedServerLookup::Saved(&store.saved_servers[0])
    );
}

#[test]
fn a_refusal_over_an_address_held_as_a_name_says_how_to_free_it() {
    let mut store = ServerStore::new();
    store
        .save_server(build_saved_server(
            Some("laptop.local:7654"),
            "desk.local:7654",
        ))
        .expect("the first save");

    let save_refusal = store
        .save_server(build_saved_server(None, "laptop.local:7654"))
        .expect_err("the address is another record's name");

    assert_eq!(
        save_refusal.to_string(),
        "the name laptop.local:7654 already belongs to desk.local:7654; run \
         `koshi remote forget laptop.local:7654` first, or pick another name"
    );
    assert_eq!(
        store.forget_saved_server("laptop.local:7654"),
        Some("desk.local:7654".to_string()),
        "the forget the message names drops the holder"
    );
}
