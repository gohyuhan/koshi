//! Tests for the saved-server store: where the file lives, the write/read
//! roundtrip through the atomic writer, the private mode of the file, where
//! the lock guarding a change lives, and looking a server up by its name or
//! its address.

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
        fingerprint: Some("ab".repeat(32)),
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

/// The lock sits beside the store, not on it. The store file is replaced by a
/// rename; a lock held on the store path guards the file the rename removed.
#[test]
fn the_lock_path_sits_beside_the_store_and_is_not_the_store() {
    let data_dir = Path::new("/home/ada/.local/share/koshi");

    assert_eq!(
        lock_path(data_dir),
        Path::new("/home/ada/.local/share/koshi/remote/servers.lock")
    );
    assert_ne!(lock_path(data_dir), store_path(data_dir));
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
    assert_eq!(store.find("desk"), Lookup::NotSaved);
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

    let mut names: Vec<&str> = fields.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
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
        fields["secret"],
        serde_json::Value::String("a secret".to_string()),
        "the secret travels in the file, so the next connection presents it"
    );
}

#[test]
fn a_record_with_no_pinned_fingerprint_travels_without_the_field_and_reads_back() {
    let mut record = saved(Some("work"), "laptop.local:7654");
    record.fingerprint = None;

    let encoded = serde_json::to_value(&record).expect("a record encodes");
    let fields = encoded.as_object().expect("a record encodes as an object");
    assert!(
        !fields.contains_key("fingerprint"),
        "no pinned fingerprint leaves the file without the field: {fields:?}"
    );

    let decoded: SavedServer = serde_json::from_value(encoded).expect("the record reads back");
    assert_eq!(decoded, record);
}

#[test]
fn a_file_written_when_every_record_carried_a_fingerprint_still_reads() {
    let old_shape = serde_json::json!({
        "name": "work",
        "address": "laptop.local:7654",
        "secret": "a secret",
        "fingerprint": "ab".repeat(32),
        "added_at": SystemTime::UNIX_EPOCH,
        "last_used_at": null,
    });

    let decoded: SavedServer = serde_json::from_value(old_shape).expect("the old shape reads");
    assert_eq!(decoded.fingerprint, Some("ab".repeat(32)));
}

#[test]
fn pinning_puts_the_fingerprint_on_the_named_record_and_an_ambiguous_name_pins_nothing() {
    let mut store = one_server();
    store.records[0].fingerprint = None;

    store.pin("work", "ab".repeat(32));
    assert_eq!(store.records[0].fingerprint, Some("ab".repeat(32)));

    store.pin("nobody", "ff".repeat(32));
    assert_eq!(
        store.records[0].fingerprint,
        Some("ab".repeat(32)),
        "a selector naming no record pins nothing"
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
        fingerprint: Some("aa".repeat(32)),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });
    store.records.push(SavedServer {
        name: None,
        address: "target.example:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: Some("bb".repeat(32)),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });

    assert_eq!(
        store.find("target.example:7654"),
        Lookup::Ambiguous,
        "the selector answers for two different records, so it answers for neither"
    );
    assert_eq!(store.forget("target.example:7654"), None);
    assert_eq!(store.records.len(), 2, "and it removed nothing");
    assert_eq!(
        store.set_secret("target.example:7654", ConnectionToken::generate()),
        None
    );
}

#[test]
fn a_selector_matching_one_record_by_both_its_name_and_its_address_is_that_record() {
    // Both matches are the same record, so it is one answer.
    let mut store = ServerStore::new();
    store.records.push(SavedServer {
        name: Some("desk.local:7654".to_string()),
        address: "desk.local:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: Some("cc".repeat(32)),
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
        fingerprint: Some("dd".repeat(32)),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });
    store.records.push(SavedServer {
        name: None,
        address: "laptop.local:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: Some("ee".repeat(32)),
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
    again.fingerprint = Some("ff".repeat(32));
    store.save(again).expect("the same machine saves again");

    assert_eq!(store.records.len(), 1, "one address is one record");
    assert_eq!(store.records[0].fingerprint, Some("ff".repeat(32)));
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
    again.fingerprint = Some("ff".repeat(32));
    store
        .save(again)
        .expect("the machine that holds the name may keep it");

    assert_eq!(store.records.len(), 1);
    assert_eq!(store.records[0].fingerprint, Some("ff".repeat(32)));
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

    assert_eq!(
        store.find("work"),
        Lookup::Ambiguous,
        "two names, no answer"
    );
    assert_eq!(store.forget("work"), None);
    assert_eq!(store.records.len(), 2, "and nothing was removed");
    assert_eq!(store.set_secret("work", ConnectionToken::generate()), None);
    store.touch("work", SystemTime::now());
    assert_eq!(
        store.records[0].last_used_at, None,
        "and nothing was stamped"
    );
    assert_eq!(store.records[1].last_used_at, None);
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
    assert_eq!(
        store.find("desk.local:7654"),
        Lookup::Saved(&store.records[0])
    );
}

#[test]
fn a_selector_two_records_answer_to_pins_nothing() {
    // A hand-written file can hold what `save` refuses under rule 3.
    let mut store = ServerStore::new();
    store.records.push(SavedServer {
        name: Some("target.example:7654".to_string()),
        address: "other.example:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: None,
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });
    store.records.push(SavedServer {
        name: None,
        address: "target.example:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: None,
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    });

    store.pin("target.example:7654", "ab".repeat(32));

    assert_eq!(store.records[0].fingerprint, None);
    assert_eq!(store.records[1].fingerprint, None);
}

#[test]
fn a_record_with_no_pinned_fingerprint_survives_the_file_it_is_written_to() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    let mut store = one_server();
    store.records[0].fingerprint = None;

    store.write(&path).expect("write the store");

    assert_eq!(ServerStore::read(&path).expect("read it back"), store);
}

#[test]
fn an_empty_store_answers_to_nothing_and_forgets_nothing() {
    let mut store = ServerStore::new();

    assert_eq!(store.find("work"), Lookup::NotSaved);
    assert_eq!(store.find(""), Lookup::NotSaved);
    assert_eq!(store.forget("work"), None);
    assert_eq!(store.set_secret("work", ConnectionToken::new("x")), None);
    assert_eq!(store.records, Vec::new());
}

#[test]
fn a_store_holding_one_record_is_written_as_these_exact_bytes() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    one_server().write(&path).expect("write the store");

    let written = std::fs::read_to_string(&path).expect("read the file");
    assert_eq!(
        written,
        format!(
            r#"{{"format":{SERVER_STORE_FORMAT},"records":[{{"name":"work","address":"laptop.local:7654","secret":"a secret","fingerprint":"{}","added_at":{{"secs_since_epoch":100,"nanos_since_epoch":0}},"last_used_at":null}}]}}"#,
            "ab".repeat(32)
        )
    );
}

#[test]
fn junk_bytes_are_an_unreadable_saved_servers_file() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    std::fs::create_dir_all(path.parent().expect("the remote directory")).expect("make it");
    std::fs::write(&path, b"not a store").expect("write junk");

    let failure = ServerStore::read(&path).expect_err("junk is refused");
    let detail = serde_json::from_slice::<ServerStore>(b"not a store")
        .expect_err("junk does not decode")
        .to_string();
    assert_eq!(
        failure.to_string(),
        format!(
            "the saved servers file at {} is unreadable: {detail}",
            path.display()
        )
    );
}

#[test]
fn a_store_whose_bytes_stop_part_way_is_unreadable() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    one_server().write(&path).expect("write the store");
    let whole = std::fs::read(&path).expect("read the file");
    let cut = &whole[..whole.len() / 2];
    std::fs::write(&path, cut).expect("write the first half");

    let failure = ServerStore::read(&path).expect_err("a cut file is refused");
    let detail = serde_json::from_slice::<ServerStore>(cut)
        .expect_err("half a store does not decode")
        .to_string();
    assert_eq!(
        failure.to_string(),
        format!(
            "the saved servers file at {} is unreadable: {detail}",
            path.display()
        )
    );
}

#[test]
fn a_record_carrying_an_unknown_field_makes_the_store_unreadable() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    let bytes = format!(
        r#"{{"format":{SERVER_STORE_FORMAT},"records":[{{"name":"work","address":"laptop.local:7654","secret":"a secret","colour":"red","added_at":{{"secs_since_epoch":100,"nanos_since_epoch":0}},"last_used_at":null}}]}}"#
    );
    std::fs::create_dir_all(path.parent().expect("the remote directory")).expect("make it");
    std::fs::write(&path, &bytes).expect("write the store by hand");

    let failure = ServerStore::read(&path).expect_err("an unknown field is refused");
    let detail = serde_json::from_str::<ServerStore>(&bytes)
        .expect_err("an unknown field does not decode")
        .to_string();
    assert_eq!(
        failure.to_string(),
        format!(
            "the saved servers file at {} is unreadable: {detail}",
            path.display()
        )
    );
}

#[test]
fn a_store_carrying_an_unknown_top_level_field_is_unreadable() {
    let bytes = format!(r#"{{"owner":"ada","format":{SERVER_STORE_FORMAT},"records":[]}}"#);

    let failure = serde_json::from_str::<ServerStore>(&bytes).expect_err("refused");
    assert_eq!(
        failure.to_string(),
        "unknown field `owner`, expected `format` or `records` at line 1 column 8"
    );
}

#[test]
fn a_fingerprint_written_as_null_reads_back_as_none() {
    let shape = serde_json::json!({
        "name": "work",
        "address": "laptop.local:7654",
        "secret": "a secret",
        "fingerprint": null,
        "added_at": SystemTime::UNIX_EPOCH,
        "last_used_at": null,
    });

    let decoded: SavedServer = serde_json::from_value(shape).expect("null reads");
    assert_eq!(decoded.fingerprint, None);
}

#[test]
fn a_directory_where_the_store_belongs_is_refused_rather_than_read_as_empty() {
    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    std::fs::create_dir_all(&path).expect("make a directory at the store path");

    let failure = ServerStore::read(&path).expect_err("a directory is not a store");
    let IpcError::RemoteFileUnreadable {
        file, path: named, ..
    } = failure
    else {
        panic!("a directory at the store path names the saved servers file: {failure}");
    };
    assert_eq!(file, RemoteFile::SavedServers);
    assert_eq!(named, path.display().to_string());
}

#[test]
fn writing_where_the_directory_cannot_exist_is_a_saved_servers_write_failure() {
    let dir = TempDir::new().expect("make a temp dir");
    std::fs::write(dir.path().join("remote"), b"a file, not a directory").expect("write it");
    let path = store_path(dir.path());

    let failure = one_server()
        .write(&path)
        .expect_err("a file in the directory's place stops the write");
    let IpcError::RemoteFileWrite {
        file, path: named, ..
    } = failure
    else {
        panic!("a failed write names the saved servers file: {failure}");
    };
    assert_eq!(file, RemoteFile::SavedServers);
    assert_eq!(named, path.display().to_string());
    assert!(
        !path.exists(),
        "and nothing was written at {}",
        path.display()
    );
}

#[cfg(unix)]
#[test]
fn a_store_file_that_was_group_readable_is_private_after_the_write() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("make a temp dir");
    let path = store_path(dir.path());
    one_server().write(&path).expect("write the store");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("open the file up");

    one_server().write(&path).expect("write the store again");

    let mode = std::fs::metadata(&path)
        .expect("stat the store")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn saving_an_address_again_moves_its_record_to_the_end() {
    let mut store = ServerStore::new();
    store
        .save(saved(Some("work"), "desk.local:7654"))
        .expect("save the first");
    store
        .save(saved(Some("home"), "laptop.local:7654"))
        .expect("save the second");

    let mut again = saved(Some("work"), "desk.local:7654");
    again.secret = ConnectionToken::new("a rotated secret");
    store
        .save(again.clone())
        .expect("the same machine saves again");

    assert_eq!(
        store.records,
        vec![saved(Some("home"), "laptop.local:7654"), again]
    );
}

#[test]
fn a_server_may_be_named_by_its_own_address() {
    let mut store = ServerStore::new();
    store
        .save(saved(Some("desk.local:7654"), "desk.local:7654"))
        .expect("a record may answer to one word both ways");

    assert_eq!(
        store.find("desk.local:7654"),
        Lookup::Saved(&store.records[0])
    );
    assert!(store.name_free_for("desk.local:7654", "desk.local:7654"));
}

#[test]
fn a_name_several_records_hold_is_refused_naming_the_first_holder() {
    // A hand-written file can hold what `save` refuses under rule 2.
    let mut store = ServerStore::new();
    store.records.push(saved(Some("work"), "desk.local:7654"));
    store.records.push(saved(Some("work"), "laptop.local:7654"));

    let refusal = store
        .save(saved(Some("work"), "phone.local:7654"))
        .expect_err("the name is held");

    assert_eq!(refusal.name, "work");
    assert_eq!(refusal.address, "desk.local:7654");
    assert_eq!(store.records.len(), 2, "and nothing was added");
}

#[test]
fn forgetting_one_of_two_servers_leaves_the_other_where_it_was() {
    let mut store = ServerStore::new();
    store
        .save(saved(Some("work"), "desk.local:7654"))
        .expect("save the first");
    store
        .save(saved(Some("home"), "laptop.local:7654"))
        .expect("save the second");

    assert_eq!(
        store.forget("desk.local:7654"),
        Some("desk.local:7654".to_string())
    );

    assert_eq!(
        store.records,
        vec![saved(Some("home"), "laptop.local:7654")]
    );
    assert_eq!(store.find("work"), Lookup::NotSaved);
    assert_eq!(store.find("home"), Lookup::Saved(&store.records[0]));
}

#[test]
fn a_refusal_over_an_address_held_as_a_name_says_how_to_free_it() {
    let mut store = ServerStore::new();
    store
        .save(saved(Some("laptop.local:7654"), "desk.local:7654"))
        .expect("the first save");

    let refusal = store
        .save(saved(None, "laptop.local:7654"))
        .expect_err("the address is another record's name");

    assert_eq!(
        refusal.to_string(),
        "the name laptop.local:7654 already belongs to desk.local:7654; run \
         `koshi remote forget laptop.local:7654` first, or pick another name"
    );
    assert_eq!(
        store.forget("laptop.local:7654"),
        Some("desk.local:7654".to_string()),
        "the forget the message names drops the holder"
    );
}
