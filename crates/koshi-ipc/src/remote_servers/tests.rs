//! Tests for the saved-server store: where the file lives, the write/read
//! roundtrip through the atomic writer, the private mode of the file, and
//! looking a server up by its name or its address.

use std::time::Duration;

use tempfile::TempDir;

use super::*;

/// The one record `arg` names, or `None` when it names none or names more than
/// one. The tests below say which of those two they mean by checking `find`
/// itself where it matters.
fn saved_by<'a>(store: &'a ServerStore, arg: &str) -> Option<&'a SavedServer> {
    match store.find(arg) {
        Lookup::Saved(record) => Some(record),
        Lookup::NotSaved | Lookup::Ambiguous => None,
    }
}

/// A fixed point on the clock, `secs` seconds after the epoch.
fn moment(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// A saved server at `address`, named `name`.
fn saved(name: Option<&str>, address: &str) -> SavedServer {
    SavedServer {
        name: name.map(str::to_string),
        address: address.to_string(),
        secret: ConnectionToken::new("a secret"),
        fingerprint: "ab".repeat(32),
        added_at: moment(100),
        last_used_at: None,
    }
}

/// A store holding one server named `work` at `laptop.local:7654`.
fn one_server() -> ServerStore {
    let mut store = ServerStore::new();
    store
        .save(saved(Some("work"), "laptop.local:7654"))
        .expect("a store with nothing in it takes any name");
    store
}

#[test]
fn the_store_path_is_remote_servers_under_the_data_dir() {
    assert_eq!(
        store_path(Path::new("/home/ada/.local/share/koshi")),
        Path::new("/home/ada/.local/share/koshi/remote/servers")
    );
}

#[test]
fn a_path_with_no_file_reads_as_an_empty_store() {
    let dir = TempDir::new().expect("make a temp dir");
    let store = ServerStore::read(&store_path(dir.path())).expect("read a missing store");
    assert_eq!(store, ServerStore::new());
}

#[test]
fn a_written_store_reads_back_the_same() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    let store = one_server();
    store.write(&path).expect("write the store");
    assert_eq!(ServerStore::read(&path).expect("read the store"), store);
}

#[cfg(unix)]
#[test]
fn the_written_file_and_its_directory_are_private_to_the_owner() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    one_server().write(&path).expect("write the store");

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
fn a_file_at_another_format_number_is_refused() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    let mut store = one_server();
    store.format = SERVER_STORE_FORMAT + 1;
    store.write(&path).expect("write the store");
    let failure = ServerStore::read(&path).expect_err("another format number is refused");
    assert_eq!(
        failure.to_string(),
        format!(
            "the saved servers file at {} is unreadable: format {} is not the \
             {SERVER_STORE_FORMAT} this build reads",
            path.display(),
            SERVER_STORE_FORMAT + 1
        )
    );
}

#[test]
fn a_server_is_found_by_its_name_and_by_its_address() {
    let store = one_server();
    assert_eq!(
        saved_by(&store, "work").map(|server| server.address.as_str()),
        Some("laptop.local:7654")
    );
    assert_eq!(
        saved_by(&store, "laptop.local:7654").map(|server| server.address.as_str()),
        Some("laptop.local:7654")
    );
    assert!(saved_by(&store, "desk").is_none());
}

#[test]
fn a_selector_that_is_one_record_s_name_and_another_s_address_is_neither() {
    // `work` is the second record's name and the first record's address.
    // Pushed rather than saved: `save` refuses this pair — see
    // `a_name_another_machine_already_answers_to_by_address_is_refused`.
    let mut store = ServerStore::new();
    store.records.push(saved(None, "work"));
    store.records.push(saved(Some("work"), "laptop.local:7654"));
    assert_eq!(
        saved_by(&store, "work").map(|server| server.address.as_str()),
        None
    );
}

#[test]
fn saving_the_same_address_again_takes_the_place_of_the_old_record() {
    let mut store = one_server();
    store
        .save(saved(Some("home"), "laptop.local:7654"))
        .expect("the name is free");
    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].name.as_deref(), Some("home"));
}

#[test]
fn forgetting_a_server_returns_its_address_and_drops_it() {
    let mut store = one_server();
    assert_eq!(store.forget("work"), Some("laptop.local:7654".to_string()));
    assert!(store.records.is_empty());
    assert_eq!(store.forget("work"), None);
}

#[test]
fn a_replaced_secret_lands_on_the_named_server() {
    let mut store = one_server();
    assert_eq!(
        store.set_secret("work", ConnectionToken::new("a rotated secret")),
        Some("laptop.local:7654".to_string())
    );
    assert_eq!(
        store.records[0].secret,
        ConnectionToken::new("a rotated secret")
    );
    assert_eq!(store.set_secret("desk", ConnectionToken::new("x")), None);
}

#[test]
fn touching_a_server_stamps_its_last_used_time() {
    let mut store = one_server();
    store.touch("work", moment(500));
    assert_eq!(store.records[0].last_used_at, Some(moment(500)));
    store.touch("desk", moment(900));
    assert_eq!(store.records[0].last_used_at, Some(moment(500)));
}

#[test]
fn describing_a_saved_server_writes_its_secret_redacted() {
    let mut record = saved(Some("work"), "laptop.local:7654");
    record.secret = ConnectionToken::new("the secret the operator handed out");

    let described = format!("{record:?}");
    assert!(
        !described.contains("the secret the operator handed out"),
        "a described record carries no secret: {described}"
    );
    assert!(
        described.contains("ConnectionToken(***)"),
        "a described record writes its secret redacted: {described}"
    );
    assert_eq!(format!("{}", record.secret), "***");
}

#[test]
fn a_record_carries_the_four_fields_a_listing_reports_and_the_secret_it_leaves_behind() {
    let record = saved(Some("work"), "laptop.local:7654");
    let encoded = serde_json::to_value(&record).expect("a record encodes");
    let fields = encoded.as_object().expect("a record encodes as an object");

    for field in ["name", "address", "fingerprint", "last_used_at"] {
        assert!(fields.contains_key(field), "a record carries {field}");
    }
    assert_eq!(
        fields["secret"],
        serde_json::Value::String("a secret".to_string()),
        "the secret travels in the file, so the next connection presents it"
    );
    assert_eq!(
        fields.len(),
        6,
        "a record carries these six fields: {fields:?}"
    );
}

#[test]
fn a_selector_naming_one_record_and_addressing_another_names_neither() {
    // A hand-written file can hold what `save` refuses under rule 3.
    let mut store = ServerStore::new();
    store.records.push(SavedServer {
        name: Some("target.example:7654".to_string()),
        address: "other.example:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: "aa".repeat(32),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });
    store.records.push(SavedServer {
        name: None,
        address: "target.example:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: "bb".repeat(32),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });

    assert!(
        saved_by(&store, "target.example:7654").is_none(),
        "the selector answers for two different records, so it answers for neither"
    );
    assert!(store.forget("target.example:7654").is_none());
    assert_eq!(store.records.len(), 2, "and it removed nothing");
    assert!(store
        .set_secret("target.example:7654", ConnectionToken::generate())
        .is_none());
}

#[test]
fn a_selector_matching_one_record_by_both_its_name_and_its_address_is_that_record() {
    // Both matches are the same record, so it is one answer.
    let mut store = ServerStore::new();
    store.records.push(SavedServer {
        name: Some("desk.local:7654".to_string()),
        address: "desk.local:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: "cc".repeat(32),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });

    let found = saved_by(&store, "desk.local:7654").expect("one record answers both ways");
    assert_eq!(found.address, "desk.local:7654");
}

#[test]
fn an_unambiguous_name_and_an_unambiguous_address_each_find_their_own_record() {
    let mut store = ServerStore::new();
    store.records.push(SavedServer {
        name: Some("work".to_string()),
        address: "desk.local:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: "dd".repeat(32),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });
    store.records.push(SavedServer {
        name: None,
        address: "laptop.local:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: "ee".repeat(32),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });

    assert_eq!(
        saved_by(&store, "work").expect("the named record").address,
        "desk.local:7654"
    );
    assert_eq!(
        saved_by(&store, "laptop.local:7654")
            .expect("the addressed record")
            .address,
        "laptop.local:7654"
    );
}

// `save` keeps three things true, and the tests from here down pin all three:
//   1. one address appears once,
//   2. one name appears once,
//   3. no name is another record's address.
// A selector that matches more than one record matches none, which is what
// keeps a store this build did not write from resolving to the wrong machine.

#[test]
fn saving_an_address_again_replaces_that_machine_s_record() {
    let mut store = ServerStore::new();
    store
        .save(saved(Some("work"), "desk.local:7654"))
        .expect("the first save");
    let mut again = saved(Some("work"), "desk.local:7654");
    again.fingerprint = "ff".repeat(32);
    store.save(again).expect("the same machine saves again");

    assert_eq!(store.records.len(), 1, "one address is one record");
    assert_eq!(store.records[0].fingerprint, "ff".repeat(32));
}

#[test]
fn a_name_another_machine_holds_is_refused_rather_than_doubled() {
    // Rule 2: one name appears once.
    let mut store = ServerStore::new();
    store
        .save(saved(Some("work"), "desk.local:7654"))
        .expect("the first save");

    let refusal = store
        .save(saved(Some("work"), "laptop.local:7654"))
        .expect_err("a second machine cannot take the name");

    assert_eq!(refusal.name, "work");
    assert_eq!(refusal.address, "desk.local:7654");
    assert_eq!(
        refusal.to_string(),
        "the name work already belongs to desk.local:7654; run `koshi remote forget work` \
         first, or pick another name"
    );
    assert_eq!(store.records.len(), 1, "and nothing was added");
    assert_eq!(
        saved_by(&store, "work")
            .expect("the name still answers")
            .address,
        "desk.local:7654",
        "for the machine that had it"
    );
}

#[test]
fn a_name_another_machine_already_answers_to_by_address_is_refused() {
    // Rule 3, reached by a name: `work` is already one record's address.
    let mut store = ServerStore::new();
    store.save(saved(None, "work")).expect("the first save");

    let refusal = store
        .save(saved(Some("work"), "laptop.local:7654"))
        .expect_err("a word another record answers to cannot be taken as a name");

    assert_eq!(refusal.name, "work");
    assert_eq!(refusal.address, "work");
    assert_eq!(store.records.len(), 1, "and nothing was added");
    assert_eq!(
        saved_by(&store, "work")
            .expect("the word still answers")
            .address,
        "work",
        "for the machine that had it"
    );
}

#[test]
fn an_address_another_machine_already_answers_to_by_name_is_refused() {
    // Rule 3, reached by an address: `work` is already one record's name.
    let mut store = ServerStore::new();
    store
        .save(saved(Some("work"), "laptop.local:7654"))
        .expect("the first save");

    let refusal = store
        .save(saved(None, "work"))
        .expect_err("a word another record answers to cannot be taken as an address");

    assert_eq!(refusal.name, "work");
    assert_eq!(refusal.address, "laptop.local:7654");
    assert_eq!(store.records.len(), 1, "and nothing was added");
    assert_eq!(
        saved_by(&store, "work")
            .expect("the word still answers")
            .address,
        "laptop.local:7654"
    );
}

#[test]
fn a_machine_keeps_its_own_name_when_it_saves_again() {
    // The three rules compare against other records only.
    let mut store = ServerStore::new();
    store
        .save(saved(Some("work"), "desk.local:7654"))
        .expect("the first save");

    let mut again = saved(Some("work"), "desk.local:7654");
    again.fingerprint = "ff".repeat(32);
    store
        .save(again)
        .expect("the machine that holds the name may keep it");

    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].fingerprint, "ff".repeat(32));
}

#[test]
fn a_free_name_reports_free_and_a_held_one_does_not() {
    let mut store = ServerStore::new();
    store
        .save(saved(Some("work"), "desk.local:7654"))
        .expect("the first save");

    assert!(store.name_free_for("home", "laptop.local:7654"));
    assert!(
        store.name_free_for("work", "desk.local:7654"),
        "the machine that already holds the name may keep it"
    );
    assert!(!store.name_free_for("work", "laptop.local:7654"));
    assert!(
        !store.name_free_for("desk.local:7654", "laptop.local:7654"),
        "a word another record answers to by address is not free either"
    );
}

#[test]
fn a_selector_matching_two_records_matches_neither_however_they_match() {
    // A store written by hand can hold what save refuses. Whichever way two
    // records answer to one word, the answer is none.
    let mut store = ServerStore::new();
    store.records.push(saved(Some("work"), "desk.local:7654"));
    store.records.push(saved(Some("work"), "laptop.local:7654"));

    assert!(saved_by(&store, "work").is_none(), "two names, no answer");
    assert!(store.forget("work").is_none());
    assert_eq!(store.records.len(), 2, "and nothing was removed");
    assert!(store
        .set_secret("work", ConnectionToken::generate())
        .is_none());
    store.touch("work", SystemTime::now());
    assert!(
        store.records.iter().all(|r| r.last_used_at.is_none()),
        "and nothing was stamped"
    );
}

#[test]
fn each_machine_still_answers_to_its_own_name_and_its_own_address() {
    let mut store = ServerStore::new();
    store
        .save(saved(Some("work"), "desk.local:7654"))
        .expect("save the first");
    store
        .save(saved(Some("home"), "laptop.local:7654"))
        .expect("save the second");

    for (selector, expected) in [
        ("work", "desk.local:7654"),
        ("home", "laptop.local:7654"),
        ("desk.local:7654", "desk.local:7654"),
        ("laptop.local:7654", "laptop.local:7654"),
    ] {
        assert_eq!(
            saved_by(&store, selector)
                .expect("one record answers")
                .address,
            expected,
            "the selector {selector}"
        );
    }
}

#[test]
fn an_ambiguous_selector_says_so_and_never_reads_as_nothing_saved() {
    // `Ambiguous`, never `NotSaved`.
    let mut store = ServerStore::new();
    store
        .records
        .push(saved(Some("laptop.local:7654"), "desk.local:7654"));
    store.records.push(saved(None, "laptop.local:7654"));

    assert_eq!(store.find("laptop.local:7654"), Lookup::Ambiguous);
    assert_eq!(store.find("nothing-is-saved-here"), Lookup::NotSaved);
    assert!(matches!(store.find("desk.local:7654"), Lookup::Saved(_)));
}
