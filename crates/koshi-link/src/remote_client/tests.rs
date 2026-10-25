//! Tests for the dialling side: which strings count as an address, which saved
//! names and certificate fingerprints are refused, which of the three lookup
//! answers leads to a dial and with which fingerprint, which saved server is
//! refused before any dial, which value of `KOSHI_REMOTE_SECRET` a saved
//! server refuses, how a saved server with no name takes a name,
//! which answer to a Hello reads as a refusal a repeat dial cannot change, what
//! an open link makes of each frame a server can answer a listing with, what
//! one sweep of the saved servers reports, how the lock that guards a
//! change to the saved-server store behaves, and which saved names a store
//! load drops.

use std::io::Write;
use std::net::TcpListener;
use std::time::SystemTime;

use koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT;
use koshi_ipc::protocol::{IpcErrorCode, IpcErrorPayload, IpcResponse};
use koshi_ipc::wire::MaybeKnown;
use koshi_test_support::fixtures::PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT;

use super::*;

#[test]
fn an_address_is_a_host_and_a_port_number() {
    assert!(is_server_address("laptop.local:7654"));
    assert!(is_server_address("127.0.0.1:22"));
    assert!(is_server_address("[::1]:7654"));
}

#[test]
fn a_plain_word_is_not_an_address() {
    assert!(!is_server_address("work"));
    assert!(!is_server_address("my-desktop"));
    assert!(!is_server_address("laptop.local"), "a host with no port");
    assert!(!is_server_address("laptop.local:"), "a colon and no port");
    assert!(
        !is_server_address("laptop.local:door"),
        "text where the port goes"
    );
    assert!(!is_server_address(":7654"), "a port and no host");
    assert!(
        !is_server_address("laptop.local:99999"),
        "a number too large for a port"
    );
}

#[test]
fn an_address_takes_the_whole_range_of_port_numbers_and_nothing_outside_it() {
    assert!(is_server_address("desk.local:0"), "the lowest port number");
    assert!(
        is_server_address("desk.local:65535"),
        "the highest port number"
    );
    assert!(
        !is_server_address("desk.local:65536"),
        "one past the highest port number"
    );
    assert!(!is_server_address("desk.local:-1"), "a port below zero");
    assert!(!is_server_address(""), "nothing at all");
}

#[test]
fn a_saved_name_shaped_like_an_address_is_refused() {
    let refusal =
        validate_saved_server_name("target.example:7654").expect_err("an address shape is refused");
    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a name that cannot be saved is a bad argument, not a runtime failure");
    };
    assert_eq!(
        detail,
        "target.example:7654 is the shape of an address, and a saved name must not be: \
         a lookup would take it for the server listening there. Pick a plain name."
    );
}

/// The refusal of a saved name that has the shape of a secret.
const SECRET_SHAPED_NAME_DETAIL: &str =
    "a saved name must not be 64 hex characters, the shape of a secret. Pick a plain name.";

/// The texts a pasted secret arrives as: the 64 lowercase hex characters
/// alone, and with whitespace around them.
fn build_secret_shaped_texts() -> [String; 3] {
    let secret_text = ConnectionToken::generate().expose_secret().to_string();
    [
        secret_text.clone(),
        format!(" {secret_text}"),
        format!("\t{secret_text}\n"),
    ]
}

#[test]
fn a_saved_name_that_is_a_secret_once_trimmed_is_refused_without_repeating_it() {
    for secret_shaped_name in build_secret_shaped_texts() {
        let refusal = validate_saved_server_name(&secret_shaped_name)
            .expect_err("a name with the shape of a secret is refused");

        let CliError::InvalidArgs { detail } = refusal else {
            panic!("a name that cannot be saved is a bad argument, not a runtime failure");
        };
        assert_eq!(detail, SECRET_SHAPED_NAME_DETAIL, "{secret_shaped_name:?}");
        assert!(!detail.contains(secret_shaped_name.trim()));
    }
    validate_saved_server_name(&"A".repeat(64)).expect("uppercase hex is not a secret's shape");
    validate_saved_server_name(&"a".repeat(63)).expect("63 characters is not a secret's shape");
    validate_saved_server_name(&format!("{} b", "a".repeat(64)))
        .expect("a secret and more text is not a secret's shape");
}

#[test]
fn an_empty_saved_name_is_refused() {
    let refusal = validate_saved_server_name("").expect_err("an empty name is refused");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a name that cannot be saved is a bad argument, not a runtime failure");
    };
    assert_eq!(detail, "a saved name must not be empty. Pick a plain name.");
}

#[test]
fn a_plain_saved_name_is_taken() {
    validate_saved_server_name("work").expect("a plain name is fine");
    validate_saved_server_name("desk").expect("a plain name is fine");
    validate_saved_server_name("laptop.local")
        .expect("a host with no port is not an address shape");
}

#[test]
fn the_dial_timeout_is_ten_seconds_and_the_reply_timeout_twenty() {
    assert_eq!(DIAL_TIMEOUT_DURATION, Duration::from_secs(10));
    assert_eq!(REPLY_TIMEOUT_DURATION, Duration::from_secs(20));
}

/// The refusal of a certificate fingerprint that is not `sha256:` followed by
/// 64 lowercase hex characters.
const FINGERPRINT_SHAPE_DETAIL: &str = "a certificate fingerprint is sha256: followed by 64 \
                                        lowercase hex characters, as `koshi share grant` prints \
                                        it on the machine that serves";

/// The refusal of 64 lowercase hex characters given as a certificate
/// fingerprint with no `sha256:` in front.
const SECRET_SHAPED_FINGERPRINT_DETAIL: &str = "a certificate fingerprint is sha256: followed by \
                                                64 lowercase hex characters, as `koshi share \
                                                grant` prints it on the machine that serves. 64 \
                                                hex characters alone is the shape of a secret, \
                                                and a secret is never given here";

/// Build a saved server named `work` at `desk.local:7654`.
fn build_saved_server() -> SavedServer {
    SavedServer {
        server_name: Some("work".to_string()),
        server_address: "desk.local:7654".to_string(),
        connection_token: ConnectionToken::generate(),
        certificate_fingerprint: Some("aa".repeat(32)),
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    }
}

#[test]
fn a_saved_server_resolves_to_its_own_record_and_the_fingerprint_it_holds() {
    let saved_server = build_saved_server();
    let resolved_server_reference =
        resolve_server_reference(SavedServerLookup::Saved(&saved_server), "work", None)
            .expect("a saved server resolves");
    assert_eq!(
        resolved_server_reference,
        ServerReference::Saved(saved_server)
    );
}

#[test]
fn a_fingerprint_given_for_a_saved_server_that_equals_the_one_it_holds_resolves_to_its_record() {
    let saved_server = build_saved_server();

    let resolved_server_reference = resolve_server_reference(
        SavedServerLookup::Saved(&saved_server),
        "work",
        Some(&format!("sha256:{}", "aa".repeat(32))),
    )
    .expect("a fingerprint equal to the saved one is taken");

    assert_eq!(
        resolved_server_reference,
        ServerReference::Saved(saved_server)
    );
}

#[test]
fn a_fingerprint_given_for_a_saved_server_that_differs_from_the_one_it_holds_is_refused() {
    let saved_server = build_saved_server();
    let refused_fingerprint_cases = [
        (
            format!("sha256:{}", "bb".repeat(32)),
            format!(
                "work expects the certificate fingerprint sha256:{}, and \
                 --certificate-fingerprint gives sha256:{}; run `koshi remote edit work` to \
                 change the fingerprint it expects",
                "aa".repeat(32),
                "bb".repeat(32)
            ),
        ),
        (
            format!("sha256:{}", "AA".repeat(32)),
            FINGERPRINT_SHAPE_DETAIL.to_string(),
        ),
        (
            "aa".repeat(32),
            SECRET_SHAPED_FINGERPRINT_DETAIL.to_string(),
        ),
    ];

    for (given_certificate_fingerprint, expected_detail) in refused_fingerprint_cases {
        let refusal = resolve_server_reference(
            SavedServerLookup::Saved(&saved_server),
            "work",
            Some(&given_certificate_fingerprint),
        )
        .expect_err("a fingerprint other than the saved one is refused");

        let CliError::InvalidArgs { detail } = refusal else {
            panic!("a fingerprint that cannot be used is a bad argument");
        };
        assert_eq!(detail, expected_detail, "{given_certificate_fingerprint}");
    }
}

#[test]
fn a_fingerprint_given_for_a_saved_server_holding_none_resolves_to_its_record() {
    // The dial refuses a saved server that holds no fingerprint.
    let saved_server = SavedServer {
        certificate_fingerprint: None,
        ..build_saved_server()
    };

    let resolved_server_reference = resolve_server_reference(
        SavedServerLookup::Saved(&saved_server),
        "work",
        Some(&format!("sha256:{}", "aa".repeat(32))),
    )
    .expect("a saved server holding no fingerprint resolves");

    assert_eq!(
        resolved_server_reference,
        ServerReference::Saved(saved_server)
    );
}

/// A saved server named `desk` at `desk.local:7655`, holding a secret of its
/// own.
fn build_second_saved_server() -> SavedServer {
    SavedServer {
        server_name: Some("desk".to_string()),
        server_address: "desk.local:7655".to_string(),
        ..build_saved_server()
    }
}

#[test]
fn an_unset_or_empty_secret_variable_passes_for_a_saved_server() {
    let saved_server = build_saved_server();
    let saved_servers = vec![saved_server.clone()];

    for environment_secret in [None, Some(OsStr::new("")), Some(OsStr::new(" \t "))] {
        assert_eq!(
            validate_environment_secret(&saved_servers, &saved_server, environment_secret)
                .map_err(|refusal| refusal.to_string()),
            Ok(()),
            "{environment_secret:?}"
        );
    }
}

#[test]
fn a_secret_variable_holding_a_secret_any_saved_server_holds_passes_for_a_saved_server() {
    let saved_server = build_saved_server();
    let second_saved_server = build_second_saved_server();
    let saved_servers = vec![saved_server.clone(), second_saved_server.clone()];
    let held_secret_texts = [
        saved_server.connection_token.expose_secret().to_string(),
        format!(" {}\n", saved_server.connection_token.expose_secret()),
        second_saved_server
            .connection_token
            .expose_secret()
            .to_string(),
    ];

    for held_secret_text in held_secret_texts {
        assert_eq!(
            validate_environment_secret(
                &saved_servers,
                &saved_server,
                Some(OsStr::new(&held_secret_text))
            )
            .map_err(|refusal| refusal.to_string()),
            Ok(()),
            "{held_secret_text:?}"
        );
    }
}

#[test]
fn a_secret_variable_holding_a_secret_no_saved_server_holds_is_refused_naming_set_secret() {
    let named_saved_server = build_saved_server();
    let unnamed_saved_server = SavedServer {
        server_name: None,
        ..build_second_saved_server()
    };
    let saved_servers = vec![named_saved_server.clone(), unnamed_saved_server.clone()];
    let refused_server_cases = [
        (
            named_saved_server,
            "work holds a saved secret, and KOSHI_REMOTE_SECRET gives one that no saved server \
             holds; run `koshi remote set-secret work` to replace the saved secret with it, or \
             unset KOSHI_REMOTE_SECRET to dial with the saved one",
        ),
        (
            unnamed_saved_server,
            "desk.local:7655 holds a saved secret, and KOSHI_REMOTE_SECRET gives one that no \
             saved server holds; run `koshi remote set-secret desk.local:7655` to replace the \
             saved secret with it, or unset KOSHI_REMOTE_SECRET to dial with the saved one",
        ),
    ];

    for (dialled_saved_server, expected_detail) in refused_server_cases {
        let refusal = validate_environment_secret(
            &saved_servers,
            &dialled_saved_server,
            Some(OsStr::new("unsaved-secret")),
        )
        .expect_err("a secret no saved server holds is refused");

        let CliError::InvalidArgs { detail } = refusal else {
            panic!("a secret the dial would not present is a bad argument");
        };
        assert_eq!(detail, expected_detail);
    }
}

#[test]
fn a_secret_variable_that_is_not_utf8_is_refused() {
    #[cfg(unix)]
    let environment_secret = {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(b"\xff").to_os_string()
    };
    #[cfg(windows)]
    let environment_secret = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xD800])
    };
    let saved_server = build_saved_server();

    let refusal = validate_environment_secret(
        std::slice::from_ref(&saved_server),
        &saved_server,
        Some(&environment_secret),
    )
    .expect_err("a value that is not UTF-8 is no saved secret");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a secret the dial would not present is a bad argument");
    };
    assert_eq!(
        detail,
        "work holds a saved secret, and KOSHI_REMOTE_SECRET gives one that no saved server \
         holds; run `koshi remote set-secret work` to replace the saved secret with it, or \
         unset KOSHI_REMOTE_SECRET to dial with the saved one"
    );
}

#[test]
fn an_ambiguous_selector_is_refused_and_never_dialled_as_a_new_server() {
    // An ambiguous selector is refused even when it has the shape of an
    // address and a fingerprint comes with it.
    let refusal = resolve_server_reference(
        SavedServerLookup::Ambiguous,
        "laptop.local:7654",
        Some(&format!("sha256:{}", "ab".repeat(32))),
    )
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
    let resolved_server_reference = resolve_server_reference(
        SavedServerLookup::NotSaved,
        "laptop.local:7654",
        Some(&format!("sha256:{}", "ab".repeat(32))),
    )
    .expect("an address with nothing saved is a new server");
    assert_eq!(
        resolved_server_reference,
        ServerReference::New {
            server_address: "laptop.local:7654".to_string(),
            certificate_fingerprint: "ab".repeat(32),
        }
    );
}

#[test]
fn an_address_nothing_is_saved_under_and_no_fingerprint_is_refused_before_any_dial() {
    let refusal = resolve_server_reference(SavedServerLookup::NotSaved, "laptop.local:7654", None)
        .expect_err("a new server with no fingerprint is refused");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a missing fingerprint is a bad argument, not a runtime failure");
    };
    assert_eq!(
        detail,
        "laptop.local:7654 is not saved, so the fingerprint of its certificate is needed: add \
         `--certificate-fingerprint <FINGERPRINT>` with the fingerprint `koshi share grant` \
         printed on that machine"
    );
}

#[test]
fn an_address_nothing_is_saved_under_and_a_malformed_fingerprint_is_refused_without_repeating_it() {
    let secret_text = ConnectionToken::generate().expose_secret().to_string();
    let refused_fingerprint_cases = [
        (
            format!("sha256:{}", "AB".repeat(32)),
            FINGERPRINT_SHAPE_DETAIL,
        ),
        (secret_text, SECRET_SHAPED_FINGERPRINT_DETAIL),
    ];

    for (given_certificate_fingerprint, expected_detail) in refused_fingerprint_cases {
        let refusal = resolve_server_reference(
            SavedServerLookup::NotSaved,
            "laptop.local:7654",
            Some(&given_certificate_fingerprint),
        )
        .expect_err("a malformed fingerprint is refused");

        let CliError::InvalidArgs { detail } = refusal else {
            panic!("a malformed fingerprint is a bad argument, not a runtime failure");
        };
        assert_eq!(detail, expected_detail);
    }
}

#[test]
fn a_fingerprint_is_sha256_followed_by_sixty_four_lowercase_hex_characters_and_nothing_else() {
    for accepted_fingerprint in ["0123456789abcdef".repeat(4), "f".repeat(64)] {
        assert_eq!(
            parse_certificate_fingerprint(&format!("sha256:{accepted_fingerprint}"))
                .expect("sha256: and 64 lowercase hex characters is a fingerprint"),
            accepted_fingerprint
        );
    }
    let refused_fingerprint_cases = [
        (String::new(), FINGERPRINT_SHAPE_DETAIL),
        ("sha256:".to_string(), FINGERPRINT_SHAPE_DETAIL),
        (
            format!("sha256:{}", "a".repeat(63)),
            FINGERPRINT_SHAPE_DETAIL,
        ),
        (
            format!("sha256:{}", "a".repeat(65)),
            FINGERPRINT_SHAPE_DETAIL,
        ),
        (
            format!("sha256:{}", "A".repeat(64)),
            FINGERPRINT_SHAPE_DETAIL,
        ),
        (
            format!("sha256:{}", "g".repeat(64)),
            FINGERPRINT_SHAPE_DETAIL,
        ),
        (
            format!("sha256:{} ", "a".repeat(63)),
            FINGERPRINT_SHAPE_DETAIL,
        ),
        (
            format!("SHA256:{}", "a".repeat(64)),
            FINGERPRINT_SHAPE_DETAIL,
        ),
        (
            format!("sha256: {}", "a".repeat(64)),
            FINGERPRINT_SHAPE_DETAIL,
        ),
        ("A".repeat(64), FINGERPRINT_SHAPE_DETAIL),
        ("a".repeat(64), SECRET_SHAPED_FINGERPRINT_DETAIL),
        (
            "0123456789abcdef".repeat(4),
            SECRET_SHAPED_FINGERPRINT_DETAIL,
        ),
        (
            format!(" {}\n", "a".repeat(64)),
            SECRET_SHAPED_FINGERPRINT_DETAIL,
        ),
    ];
    for (refused_fingerprint, expected_detail) in refused_fingerprint_cases {
        let Err(CliError::InvalidArgs { detail }) =
            parse_certificate_fingerprint(&refused_fingerprint)
        else {
            panic!("{refused_fingerprint:?} is not a fingerprint");
        };
        assert_eq!(detail, expected_detail, "{refused_fingerprint:?}");
    }
}

#[test]
fn a_plain_word_nothing_is_saved_under_is_refused_rather_than_dialled() {
    let refusal = resolve_server_reference(SavedServerLookup::NotSaved, "work", None)
        .expect_err("a name with nothing saved is refused");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a selector that names nothing is a bad argument, not a runtime failure");
    };
    assert_eq!(
        detail,
        "no saved server is named work; run `koshi remote list`"
    );
}

#[test]
fn a_secret_shaped_word_nothing_is_saved_under_is_refused_without_repeating_it() {
    for secret_shaped_selector in build_secret_shaped_texts() {
        for certificate_fingerprint_text in [None, Some(format!("sha256:{}", "ab".repeat(32)))] {
            let refusal = resolve_server_reference(
                SavedServerLookup::NotSaved,
                &secret_shaped_selector,
                certificate_fingerprint_text.as_deref(),
            )
            .expect_err("a secret names no saved server");

            let CliError::InvalidArgs { detail } = refusal else {
                panic!("a selector that names nothing is a bad argument, not a runtime failure");
            };
            assert_eq!(
                detail,
                "no saved server is named by the 64 hex characters given, the shape of a \
                 secret, and a secret is never given here; run `koshi remote list`",
                "{secret_shaped_selector:?}"
            );
            assert!(!detail.contains(secret_shaped_selector.trim()));
        }
    }
}

#[test]
fn a_secret_shaped_word_more_than_one_saved_server_answers_to_is_refused_without_repeating_it() {
    for secret_shaped_selector in build_secret_shaped_texts() {
        let refusal =
            resolve_server_reference(SavedServerLookup::Ambiguous, &secret_shaped_selector, None)
                .expect_err("an ambiguous selector is refused");

        let CliError::InvalidArgs { detail } = refusal else {
            panic!("an ambiguous selector is a bad argument");
        };
        assert_eq!(
            detail,
            "the 64 hex characters given are the name of one saved server and the address of \
             another; run `koshi remote list` and name the one you mean",
            "{secret_shaped_selector:?}"
        );
        assert!(!detail.contains(secret_shaped_selector.trim()));
    }
}

#[test]
fn a_selector_with_nothing_in_it_names_no_saved_server() {
    let refusal = resolve_server_reference(SavedServerLookup::NotSaved, "", None)
        .expect_err("an empty selector names nothing");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a selector that names nothing is a bad argument, not a runtime failure");
    };
    assert_eq!(detail, "no saved server is named ; run `koshi remote list`");
}

#[test]
fn naming_a_saved_server_that_holds_another_name_is_refused_before_any_dial() {
    let saved_server = build_saved_server();

    let refusal = connect_saved_server(&ServerReference::Saved(saved_server), Some("home"), None)
        .expect_err("another name for a saved server that holds one is refused");

    let DialError::Refused(CliError::InvalidArgs { detail }) = refusal else {
        panic!("a name that cannot be given is a bad argument, not a runtime failure");
    };
    assert_eq!(
        detail,
        "desk.local:7654 is already saved as work; run `koshi remote edit work` to rename it \
         to home"
    );
}

#[test]
fn a_secret_given_as_the_saved_name_is_refused_before_any_branch_and_never_repeated() {
    for secret_shaped_name in build_secret_shaped_texts() {
        // An address nothing listens on: a dial there ends unreachable, so the
        // exact refusal below proves no dial was made.
        let unreachable_saved_server = SavedServer {
            server_address: "127.0.0.1:1".to_string(),
            ..build_saved_server()
        };
        let server_references = [
            ServerReference::Saved(unreachable_saved_server.clone()),
            ServerReference::Saved(SavedServer {
                server_name: None,
                ..unreachable_saved_server.clone()
            }),
            ServerReference::Saved(SavedServer {
                server_name: Some(secret_shaped_name.clone()),
                ..unreachable_saved_server
            }),
            ServerReference::New {
                server_address: "127.0.0.1:1".to_string(),
                certificate_fingerprint: "ab".repeat(32),
            },
        ];

        for server_reference in server_references {
            let refusal = connect_saved_server(&server_reference, Some(&secret_shaped_name), None)
                .expect_err("a name with the shape of a secret is refused");

            let DialError::Refused(CliError::InvalidArgs { detail }) = refusal else {
                panic!("a name that cannot be given is a bad argument, not a runtime failure");
            };
            assert_eq!(
                detail, SECRET_SHAPED_NAME_DETAIL,
                "{server_reference:?} {secret_shaped_name:?}"
            );
            assert!(!detail.contains(secret_shaped_name.trim()));
        }
    }
}

/// A store holding the server `build_saved_server` builds, with no name, and
/// after it a server named `home` at `laptop.local:7654`.
fn build_store_with_unnamed_saved_server() -> ServerStore {
    ServerStore {
        saved_servers: vec![
            SavedServer {
                server_name: None,
                ..build_saved_server()
            },
            SavedServer {
                server_name: Some("home".to_string()),
                server_address: "laptop.local:7654".to_string(),
                ..build_saved_server()
            },
        ],
        ..ServerStore::new()
    }
}

#[test]
fn naming_a_saved_server_with_no_name_keeps_what_the_store_holds_and_moves_it_to_the_end() {
    let mut server_store = build_store_with_unnamed_saved_server();
    let stored_unnamed_server = server_store.saved_servers[0].clone();
    let stored_home_server = server_store.saved_servers[1].clone();
    let connection_opened_at = SystemTime::UNIX_EPOCH + Duration::from_secs(5000);

    let named_saved_server = name_saved_server(
        &mut server_store,
        "desk.local:7654",
        "work",
        connection_opened_at,
    )
    .expect("a saved server with no name takes a free name");

    let expected_named_server = SavedServer {
        server_name: Some("work".to_string()),
        last_used_at: Some(connection_opened_at),
        ..stored_unnamed_server
    };
    assert_eq!(named_saved_server, expected_named_server);
    assert_eq!(
        server_store.saved_servers,
        vec![stored_home_server, expected_named_server]
    );
}

#[test]
fn naming_a_saved_server_that_already_holds_that_name_stamps_its_last_use() {
    let mut server_store = build_store_with_unnamed_saved_server();
    server_store.saved_servers[0].server_name = Some("work".to_string());
    let stored_named_server = server_store.saved_servers[0].clone();
    let connection_opened_at = SystemTime::UNIX_EPOCH + Duration::from_secs(5000);

    let named_saved_server = name_saved_server(
        &mut server_store,
        "desk.local:7654",
        "work",
        connection_opened_at,
    )
    .expect("a saved server keeps the name it already holds");

    assert_eq!(
        named_saved_server,
        SavedServer {
            last_used_at: Some(connection_opened_at),
            ..stored_named_server
        }
    );
}

#[test]
fn naming_a_saved_server_another_koshi_renamed_or_forgot_meanwhile_is_refused() {
    let mut renamed_server_store = build_store_with_unnamed_saved_server();
    renamed_server_store.saved_servers[0].server_name = Some("office".to_string());
    let mut forgotten_server_store = build_store_with_unnamed_saved_server();
    forgotten_server_store.saved_servers.remove(0);

    for mut changed_server_store in [renamed_server_store, forgotten_server_store] {
        let server_store_before = changed_server_store.clone();

        let refusal = name_saved_server(
            &mut changed_server_store,
            "desk.local:7654",
            "work",
            SystemTime::UNIX_EPOCH,
        )
        .expect_err("a saved server that changed meanwhile is not named");

        let CliError::InvalidArgs { detail } = refusal else {
            panic!("a changed saved server is a bad argument, not a runtime failure");
        };
        assert_eq!(
            detail,
            "desk.local:7654 changed while this connection opened, so it was not named work; \
             run the command again"
        );
        assert_eq!(changed_server_store, server_store_before);
    }
}

#[test]
fn naming_a_saved_server_with_a_name_another_saved_server_holds_is_refused() {
    let mut server_store = build_store_with_unnamed_saved_server();
    let server_store_before = server_store.clone();

    let refusal = name_saved_server(
        &mut server_store,
        "desk.local:7654",
        "home",
        SystemTime::UNIX_EPOCH,
    )
    .expect_err("a name another saved server holds is refused");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a taken name is a bad argument, not a runtime failure");
    };
    assert_eq!(
        detail,
        "the name home already belongs to laptop.local:7654; run `koshi remote forget home` \
         first, or pick another name"
    );
    assert_eq!(server_store, server_store_before);
}

#[test]
fn a_dial_failure_hands_back_the_error_it_carries_unchanged() {
    let unreachable_dial_error = DialError::Unreachable(CliError::IpcUnavailable {
        detail: "the connect to desk.local:7654 was refused".to_string(),
    });
    assert_eq!(
        CliError::from(unreachable_dial_error).to_string(),
        "IPC unavailable: the connect to desk.local:7654 was refused"
    );

    let refused_dial_error = DialError::Refused(CliError::Runtime {
        detail: "the server desk.local:7654 did not admit the connection".to_string(),
    });
    assert_eq!(
        CliError::from(refused_dial_error).to_string(),
        "the server desk.local:7654 did not admit the connection"
    );
}

#[test]
fn a_welcome_naming_a_remote_protocol_version_this_build_speaks_opens_the_connection() {
    for remote_protocol_version in [MIN_REMOTE_PROTOCOL_VERSION, REMOTE_PROTOCOL_VERSION] {
        let server_frame = RemoteServerFrame::Welcome {
            remote_protocol_version,
        };
        validate_remote_server_answer("desk.local:7654", None, &server_frame).unwrap_or_else(
            |_| {
                panic!(
                "remote protocol version {remote_protocol_version} is inside the range this build \
                 speaks"
            )
            },
        );
    }
}

#[test]
fn a_welcome_naming_a_remote_protocol_version_this_build_does_not_speak_is_refused() {
    let server_frame = RemoteServerFrame::Welcome {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION + 1,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", None, &server_frame)
        .expect_err("the remote protocol version is too new");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        format!(
            "server desk.local:7654 settled on remote protocol version {}, which this koshi does \
             not speak: it speaks {MIN_REMOTE_PROTOCOL_VERSION} to {REMOTE_PROTOCOL_VERSION}",
            REMOTE_PROTOCOL_VERSION + 1
        )
    );
}

#[test]
fn a_welcome_naming_a_remote_protocol_version_older_than_this_build_speaks_is_refused() {
    let server_frame = RemoteServerFrame::Welcome {
        remote_protocol_version: MIN_REMOTE_PROTOCOL_VERSION - 1,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", None, &server_frame)
        .expect_err("the remote protocol version is too old");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        format!(
            "server desk.local:7654 settled on remote protocol version {}, which this koshi does \
             not speak: it speaks {MIN_REMOTE_PROTOCOL_VERSION} to {REMOTE_PROTOCOL_VERSION}",
            MIN_REMOTE_PROTOCOL_VERSION - 1
        )
    );
}

#[test]
fn a_session_listing_where_a_welcome_belongs_is_refused() {
    let server_frame = RemoteServerFrame::Sessions {
        session_rows: Vec::new(),
    };

    let refusal = validate_remote_server_answer("desk.local:7654", None, &server_frame)
        .expect_err("a listing is not a welcome");

    // Every refusal `validate_remote_server_answer` builds carries
    // `CliError::Runtime`.
    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        "desk.local:7654 answered with an unexpected Sessions reply"
    );
}

#[test]
fn a_saved_server_is_named_by_its_name_and_a_new_one_by_its_address() {
    assert_eq!(
        ServerReference::Saved(build_saved_server()).format_server_label(),
        "work"
    );

    let mut unnamed_saved_server = build_saved_server();
    unnamed_saved_server.server_name = None;
    assert_eq!(
        ServerReference::Saved(unnamed_saved_server).format_server_label(),
        "desk.local:7654"
    );

    assert_eq!(
        ServerReference::New {
            server_address: "laptop.local:7654".to_string(),
            certificate_fingerprint: "ab".repeat(32),
        }
        .format_server_label(),
        "laptop.local:7654"
    );
}

#[test]
fn a_saved_server_holding_no_fingerprint_is_refused_before_any_dial() {
    let mut saved_server = build_saved_server();
    saved_server.certificate_fingerprint = None;
    // An address nothing listens on: a dial there ends unreachable, so the
    // exact refusal below proves no dial was made.
    saved_server.server_address = "127.0.0.1:1".to_string();

    let refusal = connect_saved_server(&ServerReference::Saved(saved_server), None, None)
        .expect_err("a saved server with no fingerprint is refused");

    let DialError::Refused(CliError::InvalidArgs { detail }) = refusal else {
        panic!("a saved server with no fingerprint is a bad argument, not an unreachable one");
    };
    assert_eq!(
        detail,
        "work holds no certificate fingerprint, so it is not dialled; run `koshi remote edit \
         work` and give the fingerprint `koshi share grant` printed on that machine"
    );
}

#[test]
fn a_refusal_naming_the_settled_version_names_expiry_and_no_old_koshi() {
    let server_frame = RemoteServerFrame::Refused {
        message: remote_wire::REMOTE_REFUSED.to_string(),
        remote_protocol_version: Some(2),
    };

    let refusal = validate_remote_server_answer("desk.local:7654", None, &server_frame)
        .expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        "the server desk.local:7654 did not admit the connection: the token expired, was \
         revoked, or was never granted on that machine, or that machine already serves as many \
         remote connections as it admits. `koshi share list` on that machine shows when each \
         grant expires or was revoked. to replace the token, run `koshi share grant` there, \
         then store the new secret with `koshi remote set-secret` for a saved server, or give \
         it when the next dial asks"
    );
}

#[test]
fn a_refusal_naming_the_settled_version_to_a_saved_server_names_set_secret_with_its_label() {
    let server_frame = RemoteServerFrame::Refused {
        message: remote_wire::REMOTE_REFUSED.to_string(),
        remote_protocol_version: Some(2),
    };

    let refusal = validate_remote_server_answer("desk.local:7654", Some("work"), &server_frame)
        .expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        "the server desk.local:7654 did not admit the connection: the token expired, was \
         revoked, or was never granted on that machine, or that machine already serves as many \
         remote connections as it admits. `koshi share list` on that machine shows when each \
         grant expires or was revoked. to replace the token, run `koshi share grant` there, \
         then store the new secret with `koshi remote set-secret work`"
    );
}

#[test]
fn a_refusal_naming_no_version_names_the_token_first_and_an_old_koshi_last() {
    let server_frame = RemoteServerFrame::Refused {
        message: remote_wire::REMOTE_REFUSED.to_string(),
        remote_protocol_version: None,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", None, &server_frame)
        .expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        "the server desk.local:7654 did not admit the connection: the token expired, was \
         revoked, or was never granted on that machine, that machine already serves as many \
         remote connections as it admits, or it runs koshi 0.3.0 or 0.4.0, which cannot read \
         this koshi's request. `koshi share list` on that machine shows when each grant \
         expires or was revoked. to replace the token, run `koshi share grant` there, then \
         store the new secret with `koshi remote set-secret` for a saved server, or give it \
         when the next dial asks. if that machine runs koshi 0.3.0 or 0.4.0, update koshi there"
    );
}

#[test]
fn a_refusal_naming_no_version_to_a_saved_server_names_set_secret_with_its_label() {
    let server_frame = RemoteServerFrame::Refused {
        message: remote_wire::REMOTE_REFUSED.to_string(),
        remote_protocol_version: None,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", Some("work"), &server_frame)
        .expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        "the server desk.local:7654 did not admit the connection: the token expired, was \
         revoked, or was never granted on that machine, that machine already serves as many \
         remote connections as it admits, or it runs koshi 0.3.0 or 0.4.0, which cannot read \
         this koshi's request. `koshi share list` on that machine shows when each grant \
         expires or was revoked. to replace the token, run `koshi share grant` there, then \
         store the new secret with `koshi remote set-secret work`. if that machine runs koshi \
         0.3.0 or 0.4.0, update koshi there"
    );
}

#[test]
fn any_other_refusal_keeps_the_servers_own_sentence_and_names_the_server() {
    let server_frame = RemoteServerFrame::Refused {
        message: "the session is gone".to_string(),
        remote_protocol_version: None,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", None, &server_frame)
        .expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(detail, "the session is gone (server desk.local:7654)");
}

#[test]
fn a_certificate_that_is_not_the_expected_one_carries_an_ipc_failure_and_never_a_runtime_one() {
    // A certificate failure exits with `CliExitCode::IpcUnavailable`; every
    // refusal `validate_remote_server_answer` builds carries `CliError::Runtime`.
    let certificate_mismatch_error = classify_dial_failure(
        IpcError::CertificateMismatch {
            server_address: "desk.local:7654".to_string(),
            expected_certificate_fingerprint: "aa".repeat(32),
            presented_certificate_fingerprint: "bb".repeat(32),
            saved_server_label: None,
        },
        None,
    );

    let DialError::Refused(CliError::IpcUnavailable { detail }) = certificate_mismatch_error else {
        panic!("a certificate that is not the expected one is an IPC failure the dial refuses");
    };
    assert_eq!(
        detail,
        format!(
            "the certificate desk.local:7654 presented is not the expected one: expected \
             sha256:{}, presented sha256:{}. the secret was not sent. if that machine's certificate changed on \
             purpose, `koshi share open` there prints the new fingerprint: give it with \
             --certificate-fingerprint, or with `koshi remote edit` for a saved server",
            "aa".repeat(32),
            "bb".repeat(32)
        )
    );
}

#[test]
fn a_certificate_that_is_not_the_expected_one_on_a_saved_server_names_the_edit_of_it() {
    // The TLS layer names no saved server; the dial of a saved record names
    // it.
    let certificate_mismatch_error = classify_dial_failure(
        IpcError::CertificateMismatch {
            server_address: "desk.local:7654".to_string(),
            expected_certificate_fingerprint: "aa".repeat(32),
            presented_certificate_fingerprint: "bb".repeat(32),
            saved_server_label: None,
        },
        Some("work"),
    );

    let DialError::Refused(CliError::IpcUnavailable { detail }) = certificate_mismatch_error else {
        panic!("a certificate that is not the expected one is an IPC failure the dial refuses");
    };
    assert_eq!(
        detail,
        format!(
            "the certificate desk.local:7654 presented is not the expected one: expected \
             sha256:{}, presented sha256:{}. the secret was not sent. if that machine's certificate changed on \
             purpose, `koshi share open` there prints the new fingerprint: give it with `koshi \
             remote edit work`",
            "aa".repeat(32),
            "bb".repeat(32)
        )
    );
}

#[test]
fn a_connection_closed_during_the_handshake_is_unreachable_so_a_redial_dials_again() {
    let handshake_closed_error = classify_dial_failure(
        IpcError::TlsHandshakeClosed {
            server_address: "desk.local:7654".to_string(),
        },
        Some("work"),
    );

    let DialError::Unreachable(CliError::IpcUnavailable { detail }) = handshake_closed_error else {
        panic!("a connection closed during the handshake can open on the next dial");
    };
    assert_eq!(
        detail,
        IpcError::TlsHandshakeClosed {
            server_address: "desk.local:7654".to_string(),
        }
        .to_string()
    );
}

#[test]
fn a_sweep_answer_names_its_server_whatever_it_says() {
    assert_eq!(
        get_reach_server_label(&Reach::Failed {
            server_label: "desk".to_string(),
            failure_message: "the certificate desk.local:7654 presented is not the expected one"
                .to_string(),
        }),
        "desk"
    );
}

// The hidden-line reader over an in-memory stream: Enter ends the entry,
// backspace removes the last byte, Ctrl-C interrupts it, and end of stream
// ends the entry where it stands.
#[test]
fn read_hidden_terminal_line_edits_and_terminators() {
    let mut plain_line_bytes = std::io::Cursor::new(b"secret\n".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut plain_line_bytes).unwrap(),
        "secret"
    );

    let mut carriage_terminated_line_bytes = std::io::Cursor::new(b"secret\rrest".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut carriage_terminated_line_bytes).unwrap(),
        "secret"
    );

    let mut backspaced_line_bytes = std::io::Cursor::new(b"secrex\x7ft\n".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut backspaced_line_bytes).unwrap(),
        "secret"
    );

    let mut interrupted_line_bytes = std::io::Cursor::new(b"sec\x03ret\n".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut interrupted_line_bytes)
            .expect_err("Ctrl-C interrupts the entry")
            .kind(),
        io::ErrorKind::Interrupted
    );

    let mut ended_line_bytes = std::io::Cursor::new(b"secret".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut ended_line_bytes).unwrap(),
        "secret"
    );
}

// `0x04` ends the entry where `\r` and `\n` do, and the bytes after it stay
// unread.
#[test]
fn read_hidden_terminal_line_ends_at_end_of_transmission() {
    let mut transmitted_line_bytes = std::io::Cursor::new(b"secret\x04rest".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut transmitted_line_bytes).unwrap(),
        "secret"
    );
}

// Enter with nothing before it is an empty answer, not an entry that ended.
#[test]
fn read_hidden_terminal_line_takes_an_answer_with_nothing_in_it() {
    let mut empty_line_bytes = std::io::Cursor::new(b"\n".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut empty_line_bytes).unwrap(),
        ""
    );
}

// End of stream with nothing typed is `UnexpectedEof`, not an empty answer.
#[test]
fn read_hidden_terminal_line_reports_an_entry_that_ended_before_anything_was_typed() {
    let mut empty_input_bytes = std::io::Cursor::new(Vec::new());
    assert_eq!(
        read_hidden_terminal_line(&mut empty_input_bytes)
            .expect_err("the input ended")
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
}

// The sweep completion: every asked server comes back as exactly one entry,
// sorted by server name.
#[test]
fn a_server_not_heard_from_comes_back_unanswered() {
    let received_reaches = vec![Reach::Reached {
        server_label: "desk".to_string(),
        session_rows: Vec::new(),
    }];
    let requested_server_labels = vec!["desk".to_string(), "work".to_string()];

    assert_eq!(
        complete_reach_results(received_reaches, requested_server_labels),
        vec![
            Reach::Reached {
                server_label: "desk".to_string(),
                session_rows: Vec::new(),
            },
            Reach::Unanswered {
                server_label: "work".to_string(),
            },
        ]
    );
}

#[test]
fn a_sweep_with_every_server_heard_adds_nothing_and_sorts_by_server() {
    let received_reaches = vec![
        Reach::Failed {
            server_label: "work".to_string(),
            failure_message: "this server did not admit the connection".to_string(),
        },
        Reach::Reached {
            server_label: "desk".to_string(),
            session_rows: Vec::new(),
        },
    ];
    let requested_server_labels = vec!["desk".to_string(), "work".to_string()];

    assert_eq!(
        complete_reach_results(received_reaches, requested_server_labels),
        vec![
            Reach::Reached {
                server_label: "desk".to_string(),
                session_rows: Vec::new(),
            },
            Reach::Failed {
                server_label: "work".to_string(),
                failure_message: "this server did not admit the connection".to_string(),
            },
        ]
    );
}

#[test]
fn a_sweep_that_heard_nothing_reports_every_asked_server() {
    let requested_server_labels = vec!["work".to_string(), "desk".to_string()];

    assert_eq!(
        complete_reach_results(Vec::new(), requested_server_labels),
        vec![
            Reach::Unanswered {
                server_label: "desk".to_string(),
            },
            Reach::Unanswered {
                server_label: "work".to_string(),
            },
        ]
    );
}

/// An in-memory byte source standing in for the server's half of a link. The
/// deadline and the idle timeout are taken and ignored.
struct ServerFrameByteStream(std::io::Cursor<Vec<u8>>);

impl Read for ServerFrameByteStream {
    fn read(&mut self, byte_buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(byte_buffer)
    }
}

impl koshi_ipc::transport::Deadlined for ServerFrameByteStream {
    fn set_deadline(&mut self, _deadline: Option<Instant>) {}

    fn set_idle_timeout(&mut self, _idle_timeout: Option<Duration>) {}
}

/// A shared byte buffer that keeps every write for a reader.
type SharedWrittenByteBufferHandle = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

/// An in-memory byte sink standing in for this client's half of a link,
/// keeping every written byte. The deadline and the idle timeout are taken and
/// ignored.
struct SharedWrittenByteBuffer(SharedWrittenByteBufferHandle);

impl Write for SharedWrittenByteBuffer {
    fn write(&mut self, byte_buffer: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("no other holder panics")
            .extend(byte_buffer);
        Ok(byte_buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl koshi_ipc::transport::Deadlined for SharedWrittenByteBuffer {
    fn set_deadline(&mut self, _deadline: Option<Instant>) {}

    fn set_idle_timeout(&mut self, _idle_timeout: Option<Duration>) {}
}

/// Build a link reading `server_frame_bytes` as the bytes the server sent,
/// together with the buffer this side's own writes go into.
fn build_remote_link(server_frame_bytes: Vec<u8>) -> (RemoteLink, SharedWrittenByteBufferHandle) {
    use koshi_ipc::transport::build_frame_halves;

    let written_bytes: SharedWrittenByteBufferHandle =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (frame_reader, frame_writer) = build_frame_halves(
        Box::new(ServerFrameByteStream(std::io::Cursor::new(
            server_frame_bytes,
        ))),
        Box::new(SharedWrittenByteBuffer(written_bytes.clone())),
    );
    (
        RemoteLink {
            frame_reader,
            frame_writer,
        },
        written_bytes,
    )
}

/// Serialize `server_frame` as the bytes a server sends.
fn serialize_server_frame(server_frame: &impl serde::Serialize) -> Vec<u8> {
    let (remote_link, written_bytes) = build_remote_link(Vec::new());
    let mut frame_writer = remote_link.frame_writer;
    frame_writer
        .send(server_frame)
        .expect("a buffer takes every byte");
    let server_frame_bytes = written_bytes
        .lock()
        .expect("the frame writer is finished")
        .clone();
    server_frame_bytes
}

/// Deserialize the one client frame held in `client_frame_bytes`.
fn deserialize_client_frame(client_frame_bytes: Vec<u8>) -> RemoteClientFrame {
    let (mut remote_link, _) = build_remote_link(client_frame_bytes);
    remote_link
        .frame_reader
        .recv()
        .expect("the frame deserializes")
}

/// A link whose server side already answered `server_frame`, and whose own writes
/// go into a kept buffer nobody reads.
fn build_link_with_server_response(server_frame: &RemoteServerFrame) -> RemoteLink {
    build_remote_link(serialize_server_frame(server_frame)).0
}

#[test]
fn listed_rows_arrive_exactly_as_the_server_sent_them() {
    // The listing carries the name bytes the server sent, unfiltered.
    let session_id = SessionId::new();
    let mut remote_link = build_link_with_server_response(&RemoteServerFrame::Sessions {
        session_rows: vec![RemoteSessionRow {
            session_id,
            session_name: "dev\x1b[2K".to_string(),
        }],
    });

    assert_eq!(
        list_remote_sessions(&mut remote_link).expect("the sessions frame is the answer"),
        vec![RemoteSessionRow {
            session_id,
            session_name: "dev\x1b[2K".to_string(),
        }]
    );
}

#[test]
fn a_listing_keeps_the_order_the_server_holds_its_sessions_in() {
    let session_rows: Vec<RemoteSessionRow> = ["S-quiet-lake", "S-loud-river", "S-still-bay"]
        .into_iter()
        .map(|session_name| RemoteSessionRow {
            session_id: SessionId::new(),
            session_name: session_name.to_string(),
        })
        .collect();
    let mut remote_link = build_link_with_server_response(&RemoteServerFrame::Sessions {
        session_rows: session_rows.clone(),
    });

    assert_eq!(
        list_remote_sessions(&mut remote_link).expect("the sessions frame is the answer"),
        session_rows
    );
}

#[test]
fn a_server_holding_no_session_lists_nothing() {
    let mut remote_link = build_link_with_server_response(&RemoteServerFrame::Sessions {
        session_rows: Vec::new(),
    });

    assert_eq!(
        list_remote_sessions(&mut remote_link).expect("an empty listing is an answer"),
        Vec::<RemoteSessionRow>::new()
    );
}

#[test]
fn a_refused_listing_carries_the_sentence_the_server_sent() {
    let mut remote_link = build_link_with_server_response(&RemoteServerFrame::Refused {
        message: "the session is gone".to_string(),
        remote_protocol_version: None,
    });

    let refusal = list_remote_sessions(&mut remote_link).expect_err("the listing is refused");

    let CliError::Runtime { detail } = refusal else {
        panic!("a refusal the server sent is a runtime failure, not a transport one");
    };
    assert_eq!(detail, "the session is gone");
}

#[test]
fn a_welcome_where_a_listing_belongs_is_refused() {
    let mut remote_link = build_link_with_server_response(&RemoteServerFrame::Welcome {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
    });

    let refusal = list_remote_sessions(&mut remote_link).expect_err("a welcome is not a listing");

    let CliError::IpcUnavailable { detail } = refusal else {
        panic!("a frame this request cannot use is a transport failure, not a runtime one");
    };
    assert_eq!(
        detail,
        "the server answered with an unexpected Welcome reply"
    );
}

#[test]
fn a_server_that_hangs_up_before_it_answers_reports_the_peer_disconnected() {
    let (mut remote_link, _) = build_remote_link(Vec::new());

    let refusal = list_remote_sessions(&mut remote_link).expect_err("nothing answers");

    let CliError::IpcUnavailable { detail } = refusal else {
        panic!("a link that ended is a transport failure");
    };
    assert_eq!(detail, "ipc peer disconnected");
}

#[test]
fn attaching_writes_one_attach_frame_naming_the_session_and_its_link_timeout() {
    // Whole milliseconds go up; a timeout past `u32::MAX` milliseconds goes up
    // as `u32::MAX`.
    for (link_timeout, expected_link_timeout_ms) in [
        (Duration::from_secs(30), 30_000),
        (Duration::from_micros(20_000_999), 20_000),
        (Duration::from_millis(u64::from(u32::MAX)), u32::MAX),
        (Duration::from_millis(u64::from(u32::MAX) + 1), u32::MAX),
    ] {
        let session_id = SessionId::new();
        let (remote_link, written_bytes) = build_remote_link(Vec::new());

        let (_frame_reader, frame_writer) = attach_remote_session(
            remote_link,
            SessionSelector::SessionId(session_id),
            link_timeout,
        )
        .expect("the attach is written");
        drop(frame_writer);

        let sent_frame_bytes = written_bytes
            .lock()
            .expect("the frame writer is finished")
            .clone();
        assert_eq!(
            deserialize_client_frame(sent_frame_bytes),
            RemoteClientFrame::Attach {
                session_selector: SessionSelector::SessionId(session_id),
                link_timeout_ms: Some(expected_link_timeout_ms),
            }
        );
    }
}

// A saved server holding no certificate fingerprint is never dialled by the
// sweep.
#[test]
fn a_record_with_no_certificate_fingerprint_is_unchecked_and_is_not_dialled() {
    let mut saved_server = build_saved_server();
    saved_server.certificate_fingerprint = None;
    // An address nothing listens on: a dial would have to fail, and this
    // returns before one is made.
    saved_server.server_address = "127.0.0.1:1".to_string();

    assert_eq!(
        probe_saved_server(&saved_server, Instant::now()),
        Reach::Unchecked {
            server_label: "work".to_string(),
        }
    );
}

/// The time a sweep test gives a dial that fails before it: 10 seconds.
const PROBE_DEADLINE_DURATION: Duration = Duration::from_secs(10);

/// A loopback address that nothing listens on.
fn build_closed_loopback_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    listener
        .local_addr()
        .expect("read the bound address")
        .to_string()
}

#[test]
fn a_saved_server_that_refuses_the_connection_fails_with_the_dials_own_message() {
    let mut saved_server = build_saved_server();
    saved_server.server_address = build_closed_loopback_address();

    assert_eq!(
        probe_saved_server(&saved_server, Instant::now() + PROBE_DEADLINE_DURATION),
        Reach::Failed {
            server_label: "work".to_string(),
            failure_message: format!(
                "IPC unavailable: {} refused the connection: nothing is listening on that \
                 port. if remote access is not enabled on that machine, run `koshi share open` \
                 there to open the port",
                saved_server.server_address
            ),
        }
    );
}

#[test]
fn a_saved_server_whose_address_cannot_be_looked_up_fails_with_the_dials_own_message() {
    let mut saved_server = build_saved_server();
    saved_server.server_address = "127.0.0.1:seven".to_string();

    assert_eq!(
        probe_saved_server(&saved_server, Instant::now() + PROBE_DEADLINE_DURATION),
        Reach::Failed {
            server_label: "work".to_string(),
            failure_message: "IPC unavailable: ipc transport error: 127.0.0.1:seven could not \
                              be looked up: invalid port value"
                .to_string(),
        }
    );
}

#[test]
fn a_saved_server_that_closes_during_the_handshake_fails_with_the_dials_own_message() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let mut saved_server = build_saved_server();
    saved_server.server_address = listener
        .local_addr()
        .expect("read the bound address")
        .to_string();
    let closing_thread = std::thread::spawn(move || {
        // Accepted, and closed before a byte is answered.
        let _ = listener.accept();
    });

    assert_eq!(
        probe_saved_server(&saved_server, Instant::now() + PROBE_DEADLINE_DURATION),
        Reach::Failed {
            server_label: "work".to_string(),
            failure_message: format!(
                "IPC unavailable: {}",
                IpcError::TlsHandshakeClosed {
                    server_address: saved_server.server_address.clone(),
                }
            ),
        }
    );
    let _ = closing_thread.join();
}

#[test]
fn a_saved_server_that_answers_nothing_by_the_deadline_is_unanswered() {
    // Bound and never accepted: the operating system's backlog completes the
    // TCP connection, so the dial waits in the handshake until the deadline.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let mut saved_server = build_saved_server();
    saved_server.server_address = listener
        .local_addr()
        .expect("read the bound address")
        .to_string();

    assert_eq!(
        probe_saved_server(&saved_server, Instant::now() + Duration::from_millis(200)),
        Reach::Unanswered {
            server_label: "work".to_string(),
        }
    );
}

// Every entry the sweep produces sorts by server name, whatever it says.
#[test]
fn an_unchecked_server_takes_its_place_among_the_answers() {
    let received_reaches = vec![
        Reach::Unchecked {
            server_label: "work".to_string(),
        },
        Reach::Reached {
            server_label: "desk".to_string(),
            session_rows: Vec::new(),
        },
    ];

    assert_eq!(
        complete_reach_results(
            received_reaches,
            vec!["desk".to_string(), "work".to_string()]
        ),
        vec![
            Reach::Reached {
                server_label: "desk".to_string(),
                session_rows: Vec::new(),
            },
            Reach::Unchecked {
                server_label: "work".to_string(),
            },
        ]
    );
}

// Backspace at the start of an entry removes nothing, and both backspace
// bytes reach the same place.
#[test]
fn read_hidden_terminal_line_takes_a_backspace_before_anything_was_typed() {
    let mut leading_backspace_bytes = std::io::Cursor::new(b"\x7f\x08secret\n".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut leading_backspace_bytes).unwrap(),
        "secret"
    );

    let mut trailing_backspace_bytes = std::io::Cursor::new(b"secretxy\x08\x7f\n".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut trailing_backspace_bytes).unwrap(),
        "secret"
    );
}

// Bytes that are not UTF-8 come back as the replacement character, and the
// entry goes on.
#[test]
fn read_hidden_terminal_line_replaces_bytes_that_are_not_utf_8() {
    let mut non_utf8_secret_bytes = std::io::Cursor::new(b"se\xffcret\n".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut non_utf8_secret_bytes).unwrap(),
        "se\u{fffd}cret"
    );
}

/// How long a lock test waits before it reads the lock as held: 50 ms.
const TEST_LOCK_WAIT_DURATION: Duration = Duration::from_millis(50);

/// The detail a lock test expects when the lock is held at the deadline.
const TEST_HELD_LOCK_DETAIL: &str = "another test holds the lock";

#[test]
fn a_lock_taken_where_nothing_exists_makes_the_file_and_the_directory() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("remote").join("servers.lock");

    let held_lock_file = acquire_store_lock(
        &lock_file_path,
        TEST_LOCK_WAIT_DURATION,
        TEST_HELD_LOCK_DETAIL,
    )
    .expect("nothing else holds it");

    assert!(
        lock_file_path.is_file(),
        "the lock file is made where it was missing"
    );
    drop(held_lock_file);
}

/// The lock directory carries mode `0700` and the lock file mode `0600`.
#[cfg(unix)]
#[test]
fn a_lock_and_the_directory_holding_it_are_readable_by_their_owner_alone() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_directory_path = test_directory.path().join("remote");
    let lock_file_path = lock_directory_path.join("servers.lock");

    let held_lock_file = acquire_store_lock(
        &lock_file_path,
        TEST_LOCK_WAIT_DURATION,
        TEST_HELD_LOCK_DETAIL,
    )
    .expect("nothing else holds it");

    let get_file_mode = |file_path: &std::path::Path| {
        file_path
            .metadata()
            .expect("it was just made")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(
        get_file_mode(&lock_directory_path),
        0o700,
        "nobody else may list the directory"
    );
    assert_eq!(
        get_file_mode(&lock_file_path),
        0o600,
        "nobody else may open the lock"
    );
    drop(held_lock_file);
}

/// A second caller finds the lock held and gets an error after the wait.
#[test]
fn a_lock_another_holder_keeps_is_refused_after_the_wait() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("servers.lock");
    let held_lock_file = acquire_store_lock(
        &lock_file_path,
        TEST_LOCK_WAIT_DURATION,
        TEST_HELD_LOCK_DETAIL,
    )
    .expect("nothing else holds it");

    assert_eq!(
        acquire_store_lock(
            &lock_file_path,
            TEST_LOCK_WAIT_DURATION,
            TEST_HELD_LOCK_DETAIL
        )
        .expect_err("the first holder has it"),
        TEST_HELD_LOCK_DETAIL
    );
    drop(held_lock_file);
}

/// A wait of nothing tries the lock once and reports it held.
#[test]
fn a_lock_another_holder_keeps_is_refused_with_no_wait_at_all() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("servers.lock");
    let held_lock_file = acquire_store_lock(&lock_file_path, Duration::ZERO, TEST_HELD_LOCK_DETAIL)
        .expect("nothing else holds it");

    assert_eq!(
        acquire_store_lock(&lock_file_path, Duration::ZERO, TEST_HELD_LOCK_DETAIL)
            .expect_err("the first holder has it"),
        TEST_HELD_LOCK_DETAIL
    );
    drop(held_lock_file);
}

/// A file where the directory belongs stops the lock, and the failure names
/// the directory that could not be made.
#[test]
fn a_lock_whose_directory_cannot_be_made_names_that_directory() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let blocking_file_path = test_directory.path().join("remote");
    std::fs::write(&blocking_file_path, b"a file where the directory belongs")
        .expect("the file is written");
    let create_directory_error =
        std::fs::create_dir_all(&blocking_file_path).expect_err("a file is in the way");

    let refusal_detail = acquire_store_lock(
        &blocking_file_path.join("servers.lock"),
        TEST_LOCK_WAIT_DURATION,
        TEST_HELD_LOCK_DETAIL,
    )
    .expect_err("a file is in the way");

    assert_eq!(
        refusal_detail,
        format!(
            "{} could not be made: {create_directory_error}",
            blocking_file_path.display()
        )
    );
}

/// A lock its holder released is taken by the next caller.
#[test]
fn a_lock_its_holder_released_is_taken_by_the_next_caller() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("servers.lock");

    let first_held_lock_file = acquire_store_lock(
        &lock_file_path,
        TEST_LOCK_WAIT_DURATION,
        TEST_HELD_LOCK_DETAIL,
    )
    .expect("nothing else holds it");
    drop(first_held_lock_file);

    let second_held_lock_file = acquire_store_lock(
        &lock_file_path,
        TEST_LOCK_WAIT_DURATION,
        TEST_HELD_LOCK_DETAIL,
    )
    .expect("the first holder let it go");
    drop(second_held_lock_file);
}

/// A refusal the server sends in answer to a listing passes through
/// `sanitize_reported_text` before it is returned: control bytes are dropped.
#[test]
fn a_refused_listing_loses_what_a_terminal_would_act_on() {
    let mut remote_link = build_link_with_server_response(&RemoteServerFrame::Refused {
        message: "\u{1b}[2Jthe session\u{7f} is gone".to_string(),
        remote_protocol_version: None,
    });

    let refusal = list_remote_sessions(&mut remote_link).expect_err("the listing is refused");

    let CliError::Runtime { detail } = refusal else {
        panic!("a refusal the server sent is a runtime failure, not a transport one");
    };
    assert_eq!(detail, "[2Jthe session is gone");
}

#[test]
fn a_refused_dial_loses_what_a_terminal_would_act_on() {
    let server_frame = RemoteServerFrame::Refused {
        message: "\u{1b}[2Jthe session\u{7f} is gone".to_string(),
        remote_protocol_version: None,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", None, &server_frame)
        .expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(detail, "[2Jthe session is gone (server desk.local:7654)");
}

#[test]
fn a_refusal_longer_than_the_cap_is_cut_to_it() {
    let server_frame = RemoteServerFrame::Refused {
        message: "n".repeat(MAX_REPORTED_TEXT_BYTE_COUNT + 100),
        remote_protocol_version: None,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", None, &server_frame)
        .expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        format!(
            "{} (server desk.local:7654)",
            "n".repeat(MAX_REPORTED_TEXT_BYTE_COUNT)
        )
    );
}

#[test]
fn a_bare_ipv6_literal_is_not_an_address() {
    // The host before the last colon of `2001:db8::1` still holds a colon: only
    // the bracketed form is an IPv6 address shape.
    assert!(!is_server_address("::1"));
    assert!(!is_server_address("2001:db8::1"));
    assert!(!is_server_address("desk.local:+7654"), "a port is digits");
}

#[test]
fn a_selector_reads_in_a_message_as_its_id_or_its_display_name() {
    let session_id = SessionId::new();

    assert_eq!(
        format_session_selector_name(&SessionSelector::SessionId(session_id)),
        session_id.to_string()
    );
    assert_eq!(
        format_session_selector_name(&SessionSelector::SessionName(String::from("quiet-lake"))),
        "quiet-lake"
    );
}

/// The reading half of a link whose server side sent `server_frame_bytes`.
fn build_frame_reader(server_frame_bytes: Vec<u8>) -> FrameReader {
    build_remote_link(server_frame_bytes).0.frame_reader
}

#[test]
fn a_forwarded_answer_carrying_the_restarting_sentence_reads_as_restarting() {
    let mut frame_reader =
        build_frame_reader(serialize_server_frame(&RemoteServerFrame::Refused {
            message: ROUTER_RESTARTING_MESSAGE.to_string(),
            remote_protocol_version: None,
        }));

    let forwarded_answer_result = read_forwarded_hello_answer(
        &mut frame_reader,
        "desk.local:7654",
        &SessionSelector::SessionName(String::from("quiet-lake")),
    );

    let Err(DialError::Restarting(CliError::IpcUnavailable { detail })) = forwarded_answer_result
    else {
        panic!("expected a restarting answer, got {forwarded_answer_result:?}");
    };
    assert_eq!(detail, ROUTER_RESTARTING_MESSAGE);
}

#[test]
fn a_forwarded_refusal_carrying_the_not_answering_sentence_names_that_session() {
    let mut frame_reader =
        build_frame_reader(serialize_server_frame(&RemoteServerFrame::Refused {
            message: remote_wire::SESSION_NOT_ANSWERING_MESSAGE.to_string(),
            remote_protocol_version: None,
        }));

    let forwarded_answer_result = read_forwarded_hello_answer(
        &mut frame_reader,
        "desk.local:7654",
        &SessionSelector::SessionName(String::from("quiet-lake")),
    );

    let Err(DialError::Refused(CliError::Runtime { detail })) = forwarded_answer_result else {
        panic!("expected a refusal, got {forwarded_answer_result:?}");
    };
    assert_eq!(
        detail,
        "session quiet-lake did not answer on the server desk.local:7654"
    );
}

#[test]
fn any_other_forwarded_refusal_reads_as_the_token_not_reaching_the_session() {
    let mut frame_reader =
        build_frame_reader(serialize_server_frame(&RemoteServerFrame::Refused {
            message: remote_wire::REMOTE_REFUSED.to_string(),
            remote_protocol_version: None,
        }));

    let forwarded_answer_result = read_forwarded_hello_answer(
        &mut frame_reader,
        "desk.local:7654",
        &SessionSelector::SessionName(String::from("quiet-lake")),
    );

    let Err(DialError::Refused(CliError::Runtime { detail })) = forwarded_answer_result else {
        panic!("expected a refusal, got {forwarded_answer_result:?}");
    };
    assert_eq!(
        detail,
        "the token saved for the server desk.local:7654 does not reach session quiet-lake"
    );
}

#[test]
fn a_forwarded_session_answer_parses_as_the_session_response() {
    let session_refusal = IpcErrorPayload {
        code: IpcErrorCode::RequestFailed,
        message: "the session is ending".to_string(),
    };
    let mut frame_reader = build_frame_reader(serialize_server_frame(&IpcResponse {
        request_id: Some(1),
        answer_result: IpcResult::Error(session_refusal.clone()),
    }));

    let incoming_response = read_forwarded_hello_answer(
        &mut frame_reader,
        "desk.local:7654",
        &SessionSelector::SessionName(String::from("quiet-lake")),
    )
    .expect("a session answer is handed back");

    assert_eq!(
        incoming_response,
        IncomingResponse {
            request_id: Some(1),
            answer_result: MaybeKnown::Known(IpcResult::Error(session_refusal)),
        }
    );
}

#[test]
fn a_forwarded_answer_that_parses_as_neither_is_refused_naming_the_parse_error() {
    let welcome_frame = RemoteServerFrame::Welcome {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
    };
    let response_parse_error = serde_json::from_str::<IncomingResponse>(
        &serde_json::to_string(&welcome_frame).expect("a frame serializes"),
    )
    .expect_err("a Welcome is no session answer");
    let mut frame_reader = build_frame_reader(serialize_server_frame(&welcome_frame));

    let forwarded_answer_result = read_forwarded_hello_answer(
        &mut frame_reader,
        "desk.local:7654",
        &SessionSelector::SessionName(String::from("quiet-lake")),
    );

    let Err(DialError::Refused(CliError::IpcUnavailable { detail })) = forwarded_answer_result
    else {
        panic!("expected a refusal, got {forwarded_answer_result:?}");
    };
    assert_eq!(
        detail,
        format!("the server answered with a frame this attach cannot read: {response_parse_error}")
    );
}

#[test]
fn a_forwarded_answer_quoting_escape_bytes_is_refused_with_them_filtered_out() {
    let mut frame_reader = build_frame_reader(serialize_server_frame(
        &RawValue::from_string(r#"{"request_id":1,"\u001b[2Jx":1}"#.to_string())
            .expect("the answer is JSON"),
    ));

    let forwarded_answer_result = read_forwarded_hello_answer(
        &mut frame_reader,
        "desk.local:7654",
        &SessionSelector::SessionName(String::from("quiet-lake")),
    );

    let Err(DialError::Refused(CliError::IpcUnavailable { detail })) = forwarded_answer_result
    else {
        panic!("expected a refusal, got {forwarded_answer_result:?}");
    };
    assert_eq!(
        detail,
        "the server answered with a frame this attach cannot read: unknown field `[2Jx`, \
         expected `request_id` or `answer_result` at line 1 column 28"
    );
}

#[test]
fn a_forwarded_answer_from_a_session_of_koshi_0_4_0_names_the_step_on_the_serving_machine() {
    let mut frame_reader = build_frame_reader(serialize_server_frame(
        &RawValue::from_string(PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string())
            .expect("the answer is JSON"),
    ));

    let forwarded_answer_result = read_forwarded_hello_answer(
        &mut frame_reader,
        "desk.local:7654",
        &SessionSelector::SessionName(String::from("quiet-lake")),
    );

    let Err(DialError::Refused(CliError::Runtime { detail })) = forwarded_answer_result else {
        panic!("expected a refusal, got {forwarded_answer_result:?}");
    };
    assert_eq!(
        detail,
        "session quiet-lake answered in the format of koshi 0.4.0 or older, which this koshi \
         cannot talk to; on the machine that serves it, the user who started it runs: koshi \
         restart-servers"
    );
}

#[test]
fn a_link_that_ends_before_the_forwarded_answer_is_unreachable() {
    let mut frame_reader = build_frame_reader(Vec::new());

    let forwarded_answer_result = read_forwarded_hello_answer(
        &mut frame_reader,
        "desk.local:7654",
        &SessionSelector::SessionName(String::from("quiet-lake")),
    );

    let Err(DialError::Unreachable(unreachable_error)) = forwarded_answer_result else {
        panic!("expected an unreachable answer, got {forwarded_answer_result:?}");
    };
    assert_eq!(
        unreachable_error.to_string(),
        build_ipc_unavailable_error(IpcError::Disconnected).to_string()
    );
}

/// A dial that answers each of `dial_answers` in turn, and counts the dials
/// in `dial_count`. A dial past the last answer panics.
fn build_scripted_dial(
    dial_answers: Vec<Result<u32, DialError>>,
    dial_count: &std::cell::Cell<usize>,
) -> impl FnMut() -> Result<u32, DialError> + '_ {
    let mut dial_answers = dial_answers.into_iter();
    move || {
        dial_count.set(dial_count.get() + 1);
        dial_answers
            .next()
            .expect("no dial past the scripted answers")
    }
}

/// A [`CliError`] carrying `detail`, for a scripted dial answer.
fn build_dial_error_detail(detail: &str) -> CliError {
    CliError::IpcUnavailable {
        detail: detail.to_string(),
    }
}

#[test]
fn a_first_dial_that_is_not_restarting_is_the_answer_and_nothing_is_dialed_again() {
    let dial_count = std::cell::Cell::new(0);

    let dial_result = dial_through_router_restart(
        Duration::from_secs(30),
        build_scripted_dial(
            vec![Err(DialError::Unreachable(build_dial_error_detail(
                "connection refused",
            )))],
            &dial_count,
        ),
    );

    let Err(DialError::Unreachable(unreachable_error)) = dial_result else {
        panic!("expected the first answer, got {dial_result:?}");
    };
    assert_eq!(
        unreachable_error.to_string(),
        "IPC unavailable: connection refused"
    );
    assert_eq!(dial_count.get(), 1);
}

#[test]
fn a_restarting_answer_is_dialed_through_a_closed_port_until_a_dial_joins() {
    let dial_count = std::cell::Cell::new(0);

    let dial_result = dial_through_router_restart(
        Duration::from_secs(30),
        build_scripted_dial(
            vec![
                Err(DialError::Restarting(build_dial_error_detail(
                    ROUTER_RESTARTING_MESSAGE,
                ))),
                Err(DialError::Unreachable(build_dial_error_detail(
                    "connection refused",
                ))),
                Err(DialError::Restarting(build_dial_error_detail(
                    ROUTER_RESTARTING_MESSAGE,
                ))),
                Ok(7),
            ],
            &dial_count,
        ),
    );

    assert_eq!(dial_result.expect("the last dial joins"), 7);
    assert_eq!(dial_count.get(), 4);
}

#[test]
fn a_refusal_after_a_restarting_answer_ends_the_dialing() {
    let dial_count = std::cell::Cell::new(0);

    let dial_result = dial_through_router_restart(
        Duration::from_secs(30),
        build_scripted_dial(
            vec![
                Err(DialError::Restarting(build_dial_error_detail(
                    ROUTER_RESTARTING_MESSAGE,
                ))),
                Err(DialError::Refused(build_dial_error_detail(
                    "the certificate is not the expected one",
                ))),
            ],
            &dial_count,
        ),
    );

    let Err(DialError::Refused(refusal)) = dial_result else {
        panic!("expected the refusal, got {dial_result:?}");
    };
    assert_eq!(
        refusal.to_string(),
        "IPC unavailable: the certificate is not the expected one"
    );
    assert_eq!(dial_count.get(), 2);
}

#[test]
fn a_restart_window_that_has_passed_hands_back_the_last_dials_answer() {
    let dial_count = std::cell::Cell::new(0);

    let dial_result = dial_through_router_restart(
        Duration::ZERO,
        build_scripted_dial(
            vec![
                Err(DialError::Restarting(build_dial_error_detail(
                    ROUTER_RESTARTING_MESSAGE,
                ))),
                Err(DialError::Unreachable(build_dial_error_detail(
                    "connection refused",
                ))),
            ],
            &dial_count,
        ),
    );

    let Err(DialError::Unreachable(unreachable_error)) = dial_result else {
        panic!("expected the last dial's answer, got {dial_result:?}");
    };
    assert_eq!(
        unreachable_error.to_string(),
        "IPC unavailable: connection refused"
    );
    assert_eq!(dial_count.get(), 2);
}

#[test]
fn a_session_refusal_while_restarting_is_a_restarting_dial_and_every_other_refusal_is_refused() {
    let restarting_dial = DialError::from_session_refusal(CliError::SessionRestarting {
        detail: "the session is restarting; ask again in a moment".to_string(),
    });
    let refused_dial = DialError::from_session_refusal(CliError::IpcUnavailable {
        detail: "the token presented does not match this Koshi's".to_string(),
    });

    let DialError::Restarting(CliError::SessionRestarting { detail }) = restarting_dial else {
        panic!("expected a restarting dial, got {restarting_dial:?}");
    };
    assert_eq!(detail, "the session is restarting; ask again in a moment");
    let DialError::Refused(CliError::IpcUnavailable { detail }) = refused_dial else {
        panic!("expected a refused dial, got {refused_dial:?}");
    };
    assert_eq!(detail, "the token presented does not match this Koshi's");
}

/// The saved-server store bytes `v0.5.0` writes: three servers, the first
/// named with `secret_text`, the second with `secret_text` between a space and
/// a line break, and the third named `work`.
fn build_previous_release_store_text(secret_text: &str) -> String {
    let second_fingerprint = "ab".repeat(32);
    let third_fingerprint = "cd".repeat(32);
    format!(
        r#"{{"store_format":2,"saved_servers":[{{"server_name":"{secret_text}","server_address":"desk.local:7654","connection_token":"first-secret","added_at":{{"secs_since_epoch":10,"nanos_since_epoch":0}},"last_used_at":null}},{{"server_name":" {secret_text}\n","server_address":"laptop.local:7654","connection_token":"second-secret","certificate_fingerprint":"{second_fingerprint}","added_at":{{"secs_since_epoch":20,"nanos_since_epoch":0}},"last_used_at":{{"secs_since_epoch":30,"nanos_since_epoch":0}}}},{{"server_name":"work","server_address":"work.local:7654","connection_token":"third-secret","certificate_fingerprint":"{third_fingerprint}","added_at":{{"secs_since_epoch":40,"nanos_since_epoch":0}},"last_used_at":null}}]}}"#
    )
}

#[test]
fn a_saved_name_with_the_shape_of_a_secret_is_dropped_on_load_and_written_back() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let saved_server_store_path = test_directory.path().join("servers.json");
    let secret_text = ConnectionToken::generate().expose_secret().to_string();
    std::fs::write(
        &saved_server_store_path,
        build_previous_release_store_text(&secret_text),
    )
    .expect("the previous release's store is written");

    let loaded_server_store =
        load_locked_saved_server_store(&saved_server_store_path).expect("the store loads");

    let expected_server_store = ServerStore {
        saved_servers: vec![
            SavedServer {
                server_name: None,
                server_address: "desk.local:7654".to_string(),
                connection_token: ConnectionToken::from_secret("first-secret"),
                certificate_fingerprint: None,
                added_at: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
                last_used_at: None,
            },
            SavedServer {
                server_name: None,
                server_address: "laptop.local:7654".to_string(),
                connection_token: ConnectionToken::from_secret("second-secret"),
                certificate_fingerprint: Some("ab".repeat(32)),
                added_at: SystemTime::UNIX_EPOCH + Duration::from_secs(20),
                last_used_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(30)),
            },
            SavedServer {
                server_name: Some("work".to_string()),
                server_address: "work.local:7654".to_string(),
                connection_token: ConnectionToken::from_secret("third-secret"),
                certificate_fingerprint: Some("cd".repeat(32)),
                added_at: SystemTime::UNIX_EPOCH + Duration::from_secs(40),
                last_used_at: None,
            },
        ],
        ..ServerStore::new()
    };
    assert_eq!(loaded_server_store, expected_server_store);
    let expected_store_path = test_directory.path().join("expected-servers.json");
    expected_server_store
        .write_server_store_to_path(&expected_store_path)
        .expect("the expected store is written");
    let written_store_bytes =
        std::fs::read(&saved_server_store_path).expect("the store is read back");
    assert_eq!(
        written_store_bytes,
        std::fs::read(&expected_store_path).expect("the expected store is read back")
    );
    assert!(!String::from_utf8_lossy(&written_store_bytes).contains(&secret_text));
}

#[test]
fn a_store_holding_no_secret_shaped_name_is_loaded_without_being_written() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let saved_server_store_path = test_directory.path().join("servers.json");
    // Spaced unlike the bytes `write_server_store_to_path` writes.
    let previous_release_store_text = r#"{ "store_format": 2, "saved_servers": [ { "server_name": "work", "server_address": "desk.local:7654", "connection_token": "first-secret", "added_at": { "secs_since_epoch": 10, "nanos_since_epoch": 0 }, "last_used_at": null } ] }"#;
    std::fs::write(&saved_server_store_path, previous_release_store_text)
        .expect("the previous release's store is written");

    let loaded_server_store =
        load_locked_saved_server_store(&saved_server_store_path).expect("the store loads");

    assert_eq!(
        loaded_server_store.saved_servers[0].server_name.as_deref(),
        Some("work")
    );
    assert_eq!(
        std::fs::read_to_string(&saved_server_store_path).expect("the store is read back"),
        previous_release_store_text
    );
}

#[test]
fn a_format_1_store_naming_a_server_with_a_secret_loads_with_the_name_dropped() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let saved_server_store_path = test_directory.path().join("servers.json");
    let secret_text = ConnectionToken::generate().expose_secret().to_string();
    std::fs::write(
        &saved_server_store_path,
        format!(
            r#"{{"format":1,"records":[{{"name":"{secret_text}","address":"desk.local:7654","secret":"old-secret","added_at":{{"secs_since_epoch":10,"nanos_since_epoch":0}},"last_used_at":null}}]}}"#
        ),
    )
    .expect("the format 1 store is written");

    let loaded_server_store =
        load_locked_saved_server_store(&saved_server_store_path).expect("the store loads");

    let expected_server_store = ServerStore {
        saved_servers: vec![SavedServer {
            server_name: None,
            server_address: "desk.local:7654".to_string(),
            connection_token: ConnectionToken::from_secret("old-secret"),
            certificate_fingerprint: None,
            added_at: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            last_used_at: None,
        }],
        ..ServerStore::new()
    };
    assert_eq!(loaded_server_store, expected_server_store);
    assert_eq!(
        ServerStore::load_server_store_from_path(&saved_server_store_path)
            .expect("the store on disk reads"),
        expected_server_store
    );
}
