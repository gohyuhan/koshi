//! Tests for the dialling side: which strings count as an address, which saved
//! names are refused, which of the three lookup answers leads to a pinned dial,
//! and which answer to a Hello reads as a refusal a repeat dial cannot change.

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
        fingerprint: Some("aa".repeat(32)),
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

    let DialError::Refused(CliError::InvalidArgs { detail }) = refusal else {
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

    let DialError::Refused(CliError::InvalidArgs { detail }) = refusal else {
        panic!("a name that cannot be given is a bad argument");
    };
    assert_eq!(
        detail,
        "desk.local:7654 is already saved, so --save-as home would change nothing; \
         run `koshi remote forget desk.local:7654` first to save it under another name"
    );
}

#[test]
fn a_dial_failure_hands_back_the_error_it_carries_unchanged() {
    let unreachable = DialError::Unreachable(CliError::IpcUnavailable {
        detail: "the connect to desk.local:7654 was refused".to_string(),
    });
    assert_eq!(
        CliError::from(unreachable).to_string(),
        "IPC unavailable: the connect to desk.local:7654 was refused"
    );

    let refused = DialError::Refused(CliError::Runtime {
        detail: "the server desk.local:7654 did not admit the connection".to_string(),
    });
    assert_eq!(
        CliError::from(refused).to_string(),
        "the server desk.local:7654 did not admit the connection"
    );
}

#[test]
fn a_welcome_naming_a_doorway_this_build_speaks_opens_the_connection() {
    for version in [MIN_REMOTE_PROTOCOL_VERSION, REMOTE_PROTOCOL_VERSION] {
        let answer = RemoteServerFrame::Welcome {
            remote_version: version,
        };
        check_answer("desk.local:7654", &answer)
            .unwrap_or_else(|_| panic!("doorway {version} is inside the range this build speaks"));
    }
}

#[test]
fn a_welcome_naming_a_doorway_this_build_does_not_speak_is_refused() {
    let answer = RemoteServerFrame::Welcome {
        remote_version: REMOTE_PROTOCOL_VERSION + 1,
    };

    let refusal = check_answer("desk.local:7654", &answer).expect_err("the doorway is too new");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        format!(
            "server desk.local:7654 settled on remote doorway {}, which this koshi does not \
             speak: it speaks {MIN_REMOTE_PROTOCOL_VERSION} to {REMOTE_PROTOCOL_VERSION}",
            REMOTE_PROTOCOL_VERSION + 1
        )
    );
}

#[test]
fn a_session_listing_where_a_welcome_belongs_is_refused() {
    let answer = RemoteServerFrame::Sessions { rows: Vec::new() };

    let refusal = check_answer("desk.local:7654", &answer).expect_err("a listing is not a welcome");

    let DialError::Refused(CliError::IpcUnavailable { detail }) = refusal else {
        panic!("a frame this dial cannot use is a transport failure, not a runtime one");
    };
    assert_eq!(
        detail,
        "desk.local:7654 answered with a frame this request cannot produce"
    );
}

#[test]
fn a_saved_server_is_named_by_its_name_and_a_new_one_by_its_address() {
    assert_eq!(ServerArg::Saved(a_saved_server()).label(), "work");

    let mut nameless = a_saved_server();
    nameless.name = None;
    assert_eq!(ServerArg::Saved(nameless).label(), "desk.local:7654");

    assert_eq!(
        ServerArg::New {
            address: "laptop.local:7654".to_string()
        }
        .label(),
        "laptop.local:7654"
    );
}

#[test]
fn the_refusal_every_rejected_token_carries_names_both_ways_to_replace_it() {
    let answer = RemoteServerFrame::Refused {
        message: remote_wire::REMOTE_REFUSED.to_string(),
    };

    let refusal = check_answer("desk.local:7654", &answer).expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        "the server desk.local:7654 did not admit the connection: the token was rejected \
         or revoked. re-grant it on that machine with `koshi share grant`; store the new \
         secret with `koshi remote set-secret` for a saved server, or give it when the \
         next dial asks"
    );
}

#[test]
fn any_other_refusal_keeps_the_servers_own_sentence_and_names_the_server() {
    let answer = RemoteServerFrame::Refused {
        message: "the session is gone".to_string(),
    };

    let refusal = check_answer("desk.local:7654", &answer).expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(detail, "the session is gone (server desk.local:7654)");
}

#[test]
fn a_certificate_that_changed_carries_an_ipc_failure_and_never_a_runtime_one() {
    // `probe` sends a `CliError::Runtime` to `Reach::Refused` and everything
    // else to `Reach::Unreachable`.
    let changed = DialError::Refused(talk_failed(IpcError::CertificateChanged {
        address: "desk.local:7654".to_string(),
        pinned: "aa".repeat(32),
        presented: "bb".repeat(32),
    }));

    assert_eq!(
        CliError::from(changed).to_string(),
        format!(
            "IPC unavailable: the certificate of desk.local:7654 changed: pinned {}, \
             presented {}. if the server was reinstalled on purpose, run \
             `koshi remote forget desk.local:7654` and connect again.",
            "aa".repeat(32),
            "bb".repeat(32)
        )
    );
}

// The hidden-line reader over an in-memory stream: Enter ends the entry,
// backspace removes the last byte, Ctrl-C interrupts it, and end of stream
// ends the entry where it stands.
#[test]
fn read_hidden_line_edits_and_terminators() {
    let mut plain = std::io::Cursor::new(b"secret\n".to_vec());
    assert_eq!(read_hidden_line(&mut plain).unwrap(), "secret");

    let mut carriage = std::io::Cursor::new(b"secret\rrest".to_vec());
    assert_eq!(read_hidden_line(&mut carriage).unwrap(), "secret");

    let mut backspaced = std::io::Cursor::new(b"secrex\x7ft\n".to_vec());
    assert_eq!(read_hidden_line(&mut backspaced).unwrap(), "secret");

    let mut interrupted = std::io::Cursor::new(b"sec\x03ret\n".to_vec());
    assert_eq!(
        read_hidden_line(&mut interrupted)
            .expect_err("Ctrl-C interrupts the entry")
            .kind(),
        io::ErrorKind::Interrupted
    );

    let mut ended = std::io::Cursor::new(b"secret".to_vec());
    assert_eq!(read_hidden_line(&mut ended).unwrap(), "secret");
}

// End of stream with nothing typed is not an empty answer: it is the input
// ending, which every prompt that asks again must stop on.
#[test]
fn read_hidden_line_reports_an_entry_that_ended_before_anything_was_typed() {
    let mut nothing = std::io::Cursor::new(Vec::new());
    assert_eq!(
        read_hidden_line(&mut nothing)
            .expect_err("the input ended")
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
}

// The sweep completion: every asked server comes back as exactly one entry,
// sorted by server name.
#[test]
fn a_server_not_heard_from_comes_back_unreachable() {
    let heard = vec![Reach::Reached {
        server: "desk".to_string(),
        rows: Vec::new(),
    }];
    let asked = vec!["desk".to_string(), "work".to_string()];

    assert_eq!(
        complete_sweep(heard, asked),
        vec![
            Reach::Reached {
                server: "desk".to_string(),
                rows: Vec::new(),
            },
            Reach::Unreachable {
                server: "work".to_string(),
            },
        ]
    );
}

#[test]
fn a_sweep_with_every_server_heard_adds_nothing_and_sorts_by_server() {
    let heard = vec![
        Reach::Refused {
            server: "work".to_string(),
        },
        Reach::Reached {
            server: "desk".to_string(),
            rows: Vec::new(),
        },
    ];
    let asked = vec!["desk".to_string(), "work".to_string()];

    assert_eq!(
        complete_sweep(heard, asked),
        vec![
            Reach::Reached {
                server: "desk".to_string(),
                rows: Vec::new(),
            },
            Reach::Refused {
                server: "work".to_string(),
            },
        ]
    );
}

#[test]
fn a_sweep_that_heard_nothing_reports_every_asked_server() {
    let asked = vec!["work".to_string(), "desk".to_string()];

    assert_eq!(
        complete_sweep(Vec::new(), asked),
        vec![
            Reach::Unreachable {
                server: "desk".to_string(),
            },
            Reach::Unreachable {
                server: "work".to_string(),
            },
        ]
    );
}

// Control characters in a server-sent name are removed before the name is
// printed anywhere.
#[test]
fn control_characters_are_stripped_from_server_sent_text() {
    assert_eq!(without_control("dev\x1b[2K\x1b[A"), "dev[2K[A");
    assert_eq!(without_control("web \x07bell\x7f"), "web bell");
    assert_eq!(
        without_control("csi\u{9b}31m"),
        "csi31m",
        "the C1 range too"
    );
    assert_eq!(without_control("plain-name"), "plain-name");
    assert_eq!(without_control("héllo wörld"), "héllo wörld");
}

/// An in-memory byte source serving as one half of a framed link. The
/// deadline is taken and ignored.
struct ByteStream(std::io::Cursor<Vec<u8>>);

impl Read for ByteStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl koshi_ipc::transport::Deadlined for ByteStream {
    fn set_deadline(&mut self, _at: Option<Instant>) {}
}

/// An in-memory byte sink serving as one half of a framed link, keeping every
/// written byte. The deadline is taken and ignored.
struct SharedBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("no other holder panics").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl koshi_ipc::transport::Deadlined for SharedBuffer {
    fn set_deadline(&mut self, _at: Option<Instant>) {}
}

/// A link whose server side already answered `answer`, and whose own writes
/// go into a kept buffer nobody reads.
fn link_answering(answer: &RemoteServerFrame) -> RemoteLink {
    use koshi_ipc::transport::frame_halves;

    let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (_, mut encoder) = frame_halves(
        Box::new(ByteStream(std::io::Cursor::new(Vec::new()))),
        Box::new(SharedBuffer(written.clone())),
    );
    encoder.send(answer).expect("a buffer takes every byte");
    let encoded = written.lock().expect("the encoder is finished").clone();

    let (reader, writer) = frame_halves(
        Box::new(ByteStream(std::io::Cursor::new(encoded))),
        Box::new(SharedBuffer(std::sync::Arc::new(std::sync::Mutex::new(
            Vec::new(),
        )))),
    );
    RemoteLink {
        reader,
        writer,
        fingerprint: "00".repeat(32),
    }
}

#[test]
fn listed_rows_arrive_with_control_characters_removed() {
    let id = SessionId::new();
    let mut link = link_answering(&RemoteServerFrame::Sessions {
        rows: vec![RemoteSessionRow {
            id,
            name: "dev\x1b[2K".to_string(),
        }],
    });

    assert_eq!(
        list_remote_sessions(&mut link).expect("the sessions frame is the answer"),
        vec![RemoteSessionRow {
            id,
            name: "dev[2K".to_string(),
        }]
    );
}

// A record pinning no certificate is never dialled by the sweep: presenting
// the secret to whatever answers at that address is what pinning prevents.
#[test]
fn a_record_with_no_pinned_certificate_is_unchecked_and_is_not_dialled() {
    let mut record = a_saved_server();
    record.fingerprint = None;
    // An address nothing listens on: a dial would have to fail, and this
    // returns before one is made.
    record.address = "127.0.0.1:1".to_string();

    assert_eq!(
        probe(&record, Instant::now()),
        Reach::Unchecked {
            server: "work".to_string()
        }
    );
}

// Every entry the sweep produces sorts by server name, whatever it says.
#[test]
fn an_unchecked_server_takes_its_place_among_the_answers() {
    let heard = vec![
        Reach::Unchecked {
            server: "work".to_string(),
        },
        Reach::Reached {
            server: "desk".to_string(),
            rows: Vec::new(),
        },
    ];

    assert_eq!(
        complete_sweep(heard, vec!["desk".to_string(), "work".to_string()]),
        vec![
            Reach::Reached {
                server: "desk".to_string(),
                rows: Vec::new(),
            },
            Reach::Unchecked {
                server: "work".to_string(),
            },
        ]
    );
}

// Backspace at the start of an entry removes nothing, and both backspace
// bytes reach the same place.
#[test]
fn read_hidden_line_takes_a_backspace_before_anything_was_typed() {
    let mut leading = std::io::Cursor::new(b"\x7f\x08secret\n".to_vec());
    assert_eq!(read_hidden_line(&mut leading).unwrap(), "secret");

    let mut both = std::io::Cursor::new(b"secretxy\x08\x7f\n".to_vec());
    assert_eq!(read_hidden_line(&mut both).unwrap(), "secret");
}

// A secret is bytes until it is read back, so bytes that are not UTF-8 come
// back as the replacement character instead of ending the entry.
#[test]
fn read_hidden_line_replaces_bytes_that_are_not_utf_8() {
    let mut broken = std::io::Cursor::new(b"se\xffcret\n".to_vec());
    assert_eq!(read_hidden_line(&mut broken).unwrap(), "se\u{fffd}cret");
}

// The pin a dial presents is read from the store by address, so a record that
// pinned nothing when it was taken is still dialled against the certificate an
// earlier dial saved.
#[test]
fn the_pin_for_an_address_is_the_one_its_record_holds() {
    let mut store = ServerStore::new();
    let mut record = a_saved_server();
    record.fingerprint = Some("cd".repeat(32));
    store.save(record).expect("the store takes it");

    assert_eq!(
        pinned_in(&store, "desk.local:7654"),
        Some("cd".repeat(32)),
        "the address finds the record that holds the pin"
    );
    assert_eq!(
        pinned_in(&store, "work"),
        Some("cd".repeat(32)),
        "and so does the name it was saved under"
    );
}

#[test]
fn an_address_no_record_holds_and_a_record_that_pins_nothing_both_pin_nothing() {
    let mut store = ServerStore::new();
    let mut record = a_saved_server();
    record.fingerprint = None;
    store.save(record).expect("the store takes it");

    assert_eq!(pinned_in(&store, "desk.local:7654"), None);
    assert_eq!(pinned_in(&store, "nobody.local:7654"), None);
}
