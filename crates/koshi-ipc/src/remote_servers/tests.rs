//! Tests for the saved-server store: where the file lives, the write/read
//! roundtrip through the atomic writer, the private mode of the file, and
//! looking a server up by its name or its address.

use std::time::Duration;

use tempfile::TempDir;

use super::*;

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
    store.save(saved(Some("work"), "laptop.local:7654"));
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
        store.find("work").map(|server| server.address.as_str()),
        Some("laptop.local:7654")
    );
    assert_eq!(
        store
            .find("laptop.local:7654")
            .map(|server| server.address.as_str()),
        Some("laptop.local:7654")
    );
    assert!(store.find("desk").is_none());
}

#[test]
fn a_name_is_matched_before_an_address() {
    let mut store = ServerStore::new();
    store.save(saved(None, "work"));
    store.save(saved(Some("work"), "laptop.local:7654"));
    assert_eq!(
        store.find("work").map(|server| server.address.as_str()),
        Some("laptop.local:7654")
    );
}

#[test]
fn saving_the_same_address_again_takes_the_place_of_the_old_record() {
    let mut store = one_server();
    store.save(saved(Some("home"), "laptop.local:7654"));
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
