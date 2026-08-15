//! Tests for the dialling side: which strings count as an address, which saved
//! names are refused, and which of the three lookup answers leads to a pinned
//! dial.

use std::time::SystemTime;

use super::*;

#[test]
fn an_address_is_a_host_and_a_port_number() {
    assert!(looks_like_address("laptop.local:7654"));
    assert!(looks_like_address("127.0.0.1:22"));
    assert!(looks_like_address("[::1]:7654"));
}

#[test]
fn a_plain_word_is_not_an_address() {
    assert!(!looks_like_address("work"));
    assert!(!looks_like_address("my-desktop"));
    assert!(!looks_like_address("laptop.local"), "a host with no port");
    assert!(!looks_like_address("laptop.local:"), "a colon and no port");
    assert!(
        !looks_like_address("laptop.local:door"),
        "text where the port goes"
    );
    assert!(!looks_like_address(":7654"), "a port and no host");
    assert!(
        !looks_like_address("laptop.local:99999"),
        "a number too large for a port"
    );
}

#[test]
fn a_saved_name_shaped_like_an_address_is_refused() {
    // A name with the `host:port` shape can collide with an address, and a word
    // two records answer to reaches neither.
    let refusal = check_name_shape("target.example:7654").expect_err("an address shape is refused");
    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a name that cannot be saved is a bad argument, not a runtime failure");
    };
    assert_eq!(
        detail,
        "target.example:7654 is the shape of an address, and a saved name must not be: \
         a lookup would take it for the server listening there. Pick a plain name."
    );
}

#[test]
fn a_plain_saved_name_is_taken() {
    check_name_shape("work").expect("a plain name is fine");
    check_name_shape("desk").expect("a plain name is fine");
    check_name_shape("laptop.local").expect("a host with no port is not an address shape");
}

#[test]
fn a_one_shot_command_waits_a_bounded_time_and_an_attachment_does_not() {
    // A one-shot verb passes `Some(REPLY_WAIT)`; an attached client passes
    // `None`. The reply window is at least as long as the dial before it.
    assert!(
        REPLY_WAIT > DIAL_WAIT,
        "a reply has at least as long as the dial that asked for it"
    );
    assert_eq!(REPLY_WAIT, Duration::from_secs(20));
}

/// A saved server named `work` at `desk.local:7654`.
fn a_saved_server() -> SavedServer {
    SavedServer {
        name: Some("work".to_string()),
        address: "desk.local:7654".to_string(),
        secret: ConnectionToken::generate(),
        fingerprint: "aa".repeat(32),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    }
}

#[test]
fn a_saved_server_is_dialled_with_the_certificate_pinned_for_it() {
    let record = a_saved_server();
    let resolved = server_from(Lookup::Saved(&record), "work").expect("a saved server resolves");
    match resolved {
        ServerArg::Saved(found) => assert_eq!(found.fingerprint, record.fingerprint),
        ServerArg::New { .. } => panic!("a saved server must not be dialled as a new one"),
    }
}

#[test]
fn an_ambiguous_selector_is_refused_and_never_dialled_as_a_new_server() {
    // `ServerArg::New` dials with no pinned fingerprint and saves whichever
    // certificate is presented. An ambiguous selector is refused even when it
    // has the shape of an address.
    let refusal = server_from(Lookup::Ambiguous, "laptop.local:7654")
        .expect_err("an ambiguous selector is refused");
    let CliError::InvalidArgs { detail } = refusal else {
        panic!("an ambiguous selector is a bad argument");
    };
    assert_eq!(
        detail,
        "laptop.local:7654 is the name of one saved server and the address of another; \
         run `koshi remote list` and name the one you mean"
    );
}

#[test]
fn an_address_nothing_is_saved_under_is_the_one_new_server_case() {
    let resolved = server_from(Lookup::NotSaved, "laptop.local:7654")
        .expect("an address with nothing saved is a new server");
    assert_eq!(
        resolved,
        ServerArg::New {
            address: "laptop.local:7654".to_string()
        }
    );
}

#[test]
fn a_plain_word_nothing_is_saved_under_is_refused_rather_than_dialled() {
    server_from(Lookup::NotSaved, "work").expect_err("a name with nothing saved is refused");
}

#[test]
fn naming_a_server_that_is_already_saved_is_refused_rather_than_ignored() {
    // `--save-as` names a server this machine has not connected to.
    let record = a_saved_server();

    let refusal = connect_saved(&ServerArg::Saved(record), Some("home"), None)
        .expect_err("a name for a server that is already saved is refused");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a name that cannot be given is a bad argument, not a runtime failure");
    };
    assert_eq!(
        detail,
        "work is already saved, so --save-as home would change nothing; \
         run `koshi remote forget work` first to save it under another name"
    );
}

#[test]
fn a_saved_server_with_no_name_is_named_by_its_address_when_it_refuses() {
    // A record with no name is labelled by its address.
    let mut record = a_saved_server();
    record.name = None;

    let refusal = connect_saved(&ServerArg::Saved(record), Some("home"), None)
        .expect_err("a name for a server that is already saved is refused");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a name that cannot be given is a bad argument");
    };
    assert_eq!(
        detail,
        "desk.local:7654 is already saved, so --save-as home would change nothing; \
         run `koshi remote forget desk.local:7654` first to save it under another name"
    );
}
