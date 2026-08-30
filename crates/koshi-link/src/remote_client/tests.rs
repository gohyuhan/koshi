//! Tests for the dialling side: which strings count as an address, which saved
//! names are refused, which of the three lookup answers leads to a pinned dial,
//! which answer to a Hello reads as a refusal a repeat dial cannot change, what
//! an open link makes of each frame a server can answer a listing with, what
//! one sweep of the saved servers reports, and how the lock that guards a
//! change to the saved-server store behaves.

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
fn an_address_takes_the_whole_range_of_port_numbers_and_nothing_outside_it() {
    assert!(looks_like_address("desk.local:0"), "the lowest port number");
    assert!(
        looks_like_address("desk.local:65535"),
        "the highest port number"
    );
    assert!(
        !looks_like_address("desk.local:65536"),
        "one past the highest port number"
    );
    assert!(!looks_like_address("desk.local:-1"), "a port below zero");
    assert!(!looks_like_address(""), "nothing at all");
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
fn an_empty_saved_name_is_refused() {
    // `--save-as ""` reaches here: clap takes the empty string, and a record
    // named with it lists blank.
    let refusal = check_name_shape("").expect_err("an empty name is refused");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a name that cannot be saved is a bad argument, not a runtime failure");
    };
    assert_eq!(detail, "a saved name must not be empty. Pick a plain name.");
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
    assert_eq!(DIAL_WAIT, Duration::from_secs(10));
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
    assert_eq!(resolved, ServerArg::Saved(record));
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
    let refusal =
        server_from(Lookup::NotSaved, "work").expect_err("a name with nothing saved is refused");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a selector that names nothing is a bad argument, not a runtime failure");
    };
    assert_eq!(
        detail,
        "no saved server is named work; run `koshi remote list`"
    );
}

#[test]
fn a_selector_with_nothing_in_it_names_no_saved_server() {
    let refusal = server_from(Lookup::NotSaved, "").expect_err("an empty selector names nothing");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a selector that names nothing is a bad argument, not a runtime failure");
    };
    assert_eq!(detail, "no saved server is named ; run `koshi remote list`");
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
fn a_welcome_naming_a_doorway_older_than_this_build_speaks_is_refused() {
    let answer = RemoteServerFrame::Welcome {
        remote_version: MIN_REMOTE_PROTOCOL_VERSION - 1,
    };

    let refusal = check_answer("desk.local:7654", &answer).expect_err("the doorway is too old");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(
        detail,
        format!(
            "server desk.local:7654 settled on remote doorway {}, which this koshi does not \
             speak: it speaks {MIN_REMOTE_PROTOCOL_VERSION} to {REMOTE_PROTOCOL_VERSION}",
            MIN_REMOTE_PROTOCOL_VERSION - 1
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
    // `probe` reads a refused dial carrying `CliError::IpcUnavailable` as the
    // pinned-certificate check and answers `Reach::CertificateChanged`; every
    // refusal `check_answer` builds carries `CliError::Runtime` instead.
    let changed = dial_failed(IpcError::CertificateChanged {
        address: "desk.local:7654".to_string(),
        pinned: "aa".repeat(32),
        presented: "bb".repeat(32),
    });

    let DialError::Refused(CliError::IpcUnavailable { detail }) = changed else {
        panic!("a changed certificate is the shape `probe` reads");
    };
    assert_eq!(
        detail,
        format!(
            "the certificate of desk.local:7654 changed: pinned {}, \
             presented {}. if the server was reinstalled on purpose, run \
             `koshi remote forget desk.local:7654` and connect again.",
            "aa".repeat(32),
            "bb".repeat(32)
        )
    );
}

#[test]
fn a_sweep_answer_names_its_server_whatever_it_says() {
    assert_eq!(
        server_of(&Reach::CertificateChanged {
            server: "desk".to_string(),
            detail: "the certificate of desk.local:7654 changed".to_string(),
        }),
        "desk"
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

// `0x04` ends the entry where `\r` and `\n` do, and the bytes after it stay
// unread.
#[test]
fn read_hidden_line_ends_at_end_of_transmission() {
    let mut transmitted = std::io::Cursor::new(b"secret\x04rest".to_vec());
    assert_eq!(read_hidden_line(&mut transmitted).unwrap(), "secret");
}

// Enter with nothing before it is an empty answer, not an entry that ended.
#[test]
fn read_hidden_line_takes_an_answer_with_nothing_in_it() {
    let mut nothing_typed = std::io::Cursor::new(b"\n".to_vec());
    assert_eq!(read_hidden_line(&mut nothing_typed).unwrap(), "");
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

/// A buffer every write is kept in, shared with whoever reads it back.
type Kept = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

/// A link reading `sent` as the bytes the server sent, together with the
/// buffer this side's own writes go into.
fn link_over(sent: Vec<u8>) -> (RemoteLink, Kept) {
    use koshi_ipc::transport::frame_halves;

    let written: Kept = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (reader, writer) = frame_halves(
        Box::new(ByteStream(std::io::Cursor::new(sent))),
        Box::new(SharedBuffer(written.clone())),
    );
    (
        RemoteLink {
            reader,
            writer,
            fingerprint: "00".repeat(32),
        },
        written,
    )
}

/// The bytes a server sends to answer with `frame`.
fn framed(frame: &RemoteServerFrame) -> Vec<u8> {
    let (link, written) = link_over(Vec::new());
    let mut encoder = link.writer;
    encoder.send(frame).expect("a buffer takes every byte");
    let bytes = written.lock().expect("the encoder is finished").clone();
    bytes
}

/// The one frame `bytes` holds, as a client sends it.
fn client_frame_in(bytes: Vec<u8>) -> RemoteClientFrame {
    let (mut link, _) = link_over(bytes);
    link.reader.recv().expect("the frame decodes")
}

/// A link whose server side already answered `answer`, and whose own writes
/// go into a kept buffer nobody reads.
fn link_answering(answer: &RemoteServerFrame) -> RemoteLink {
    link_over(framed(answer)).0
}

#[test]
fn listed_rows_arrive_exactly_as_the_server_sent_them() {
    // A name identifies a session, so the listing carries what the server
    // said. Filtering happens where a name is printed.
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
            name: "dev\x1b[2K".to_string(),
        }]
    );
}

#[test]
fn a_listing_keeps_the_order_the_server_holds_its_sessions_in() {
    let rows: Vec<RemoteSessionRow> = ["S-quiet-lake", "S-loud-river", "S-still-bay"]
        .into_iter()
        .map(|name| RemoteSessionRow {
            id: SessionId::new(),
            name: name.to_string(),
        })
        .collect();
    let mut link = link_answering(&RemoteServerFrame::Sessions { rows: rows.clone() });

    assert_eq!(
        list_remote_sessions(&mut link).expect("the sessions frame is the answer"),
        rows
    );
}

#[test]
fn a_server_holding_no_session_lists_nothing() {
    let mut link = link_answering(&RemoteServerFrame::Sessions { rows: Vec::new() });

    assert_eq!(
        list_remote_sessions(&mut link).expect("an empty listing is an answer"),
        Vec::<RemoteSessionRow>::new()
    );
}

#[test]
fn a_refused_listing_carries_the_sentence_the_server_sent() {
    let mut link = link_answering(&RemoteServerFrame::Refused {
        message: "the session is gone".to_string(),
    });

    let refusal = list_remote_sessions(&mut link).expect_err("the listing is refused");

    let CliError::Runtime { detail } = refusal else {
        panic!("a refusal the server sent is a runtime failure, not a transport one");
    };
    assert_eq!(detail, "the session is gone");
}

#[test]
fn a_welcome_where_a_listing_belongs_is_refused() {
    let mut link = link_answering(&RemoteServerFrame::Welcome {
        remote_version: REMOTE_PROTOCOL_VERSION,
    });

    let refusal = list_remote_sessions(&mut link).expect_err("a welcome is not a listing");

    let CliError::IpcUnavailable { detail } = refusal else {
        panic!("a frame this request cannot use is a transport failure, not a runtime one");
    };
    assert_eq!(
        detail,
        "the server answered with a frame this request cannot produce"
    );
}

#[test]
fn a_server_that_hangs_up_before_it_answers_reports_the_peer_disconnected() {
    let (mut link, _) = link_over(Vec::new());

    let refusal = list_remote_sessions(&mut link).expect_err("nothing answers");

    let CliError::IpcUnavailable { detail } = refusal else {
        panic!("a link that ended is a transport failure");
    };
    assert_eq!(detail, "ipc peer disconnected");
}

#[test]
fn attaching_writes_one_attach_frame_naming_the_session() {
    let session = SessionId::new();
    let (link, written) = link_over(Vec::new());

    let (_reader, writer) =
        attach_remote(link, SessionSelector::Id(session)).expect("the attach is written");
    drop(writer);

    let sent = written.lock().expect("the writer is finished").clone();
    assert_eq!(
        client_frame_in(sent),
        RemoteClientFrame::Attach {
            session: SessionSelector::Id(session),
        }
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

// A dial that reaches nothing is unreachable, not refused: a refusal is a
// sentence the server sent.
#[test]
fn a_pinned_server_nothing_answers_for_is_unreachable() {
    let mut record = a_saved_server();
    // Port 1 of the loopback address: a connect there is refused, and a
    // machine that drops it instead runs out of the deadline below.
    record.address = "127.0.0.1:1".to_string();

    assert_eq!(
        probe(&record, Instant::now() + Duration::from_millis(200)),
        Reach::Unreachable {
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

// Two records answering to one word name neither, so a dial against that word
// presents no pin. `ServerStore::save` refuses to make such a pair, and a
// hand-written file holds one.
#[test]
fn a_word_two_records_answer_to_pins_nothing() {
    let mut store = ServerStore::new();
    let mut first = a_saved_server();
    first.fingerprint = Some("cd".repeat(32));
    let mut second = a_saved_server();
    second.address = "laptop.local:7654".to_string();
    second.fingerprint = Some("ef".repeat(32));
    store.records.push(first);
    store.records.push(second);

    assert_eq!(pinned_in(&store, "work"), None);
}

/// How long a lock test waits before it reads the lock as held. Short enough
/// that a refusal test does not slow the suite down.
const TEST_LOCK_WAIT: Duration = Duration::from_millis(50);

#[test]
fn a_lock_taken_where_nothing_exists_makes_the_file_and_the_directory() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let path = dir.path().join("remote").join("servers.lock");

    let held = hold_store(&path, TEST_LOCK_WAIT).expect("nothing else holds it");

    assert!(path.is_file(), "the lock file is made where it was missing");
    drop(held);
}

/// The lock sits beside the saved secrets, so the directory it goes in and the
/// file itself carry the same owner-only modes the store carries.
#[cfg(unix)]
#[test]
fn a_lock_and_the_directory_holding_it_are_readable_by_their_owner_alone() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("a temp directory");
    let parent = dir.path().join("remote");
    let path = parent.join("servers.lock");

    let held = hold_store(&path, TEST_LOCK_WAIT).expect("nothing else holds it");

    let mode = |at: &std::path::Path| {
        at.metadata()
            .expect("it was just made")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&parent), 0o700, "nobody else may list the directory");
    assert_eq!(mode(&path), 0o600, "nobody else may open the lock");
    drop(held);
}

/// The second koshi finds the lock held and says so rather than writing over
/// the first one's change.
#[test]
fn a_lock_another_holder_keeps_is_refused_after_the_wait() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let path = dir.path().join("servers.lock");
    let held = hold_store(&path, TEST_LOCK_WAIT).expect("nothing else holds it");

    assert_eq!(
        hold_store(&path, TEST_LOCK_WAIT)
            .expect_err("the first holder has it")
            .to_string(),
        "IPC unavailable: another koshi is changing the saved servers; try again"
    );
    drop(held);
}

/// A wait of nothing tries the lock once and reports it held.
#[test]
fn a_lock_another_holder_keeps_is_refused_with_no_wait_at_all() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let path = dir.path().join("servers.lock");
    let held = hold_store(&path, Duration::ZERO).expect("nothing else holds it");

    assert_eq!(
        hold_store(&path, Duration::ZERO)
            .expect_err("the first holder has it")
            .to_string(),
        "IPC unavailable: another koshi is changing the saved servers; try again"
    );
    drop(held);
}

/// A file where the directory belongs stops the lock, and the failure names
/// the directory that could not be made.
#[test]
fn a_lock_whose_directory_cannot_be_made_names_that_directory() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let blocker = dir.path().join("remote");
    std::fs::write(&blocker, b"a file where the directory belongs").expect("the file is written");

    let refusal = hold_store(&blocker.join("servers.lock"), TEST_LOCK_WAIT)
        .expect_err("a file is in the way");

    let CliError::IpcUnavailable { detail } = refusal else {
        panic!("a lock that cannot be taken is a transport failure");
    };
    let named = format!("{} could not be made: ", blocker.display());
    assert!(
        detail.starts_with(&named),
        "the failure opens with {named:?}, and reads {detail:?}"
    );
}

/// The first koshi finished, so the next one takes the lock instead of
/// reporting it held.
#[test]
fn a_lock_its_holder_released_is_taken_by_the_next_caller() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let path = dir.path().join("servers.lock");

    let held = hold_store(&path, TEST_LOCK_WAIT).expect("nothing else holds it");
    drop(held);

    let again = hold_store(&path, TEST_LOCK_WAIT).expect("the first holder let it go");
    drop(again);
}

#[test]
fn this_layer_never_alters_what_a_peer_reported() {
    // A session or tab name is what `targeting.rs` matches on, so everything
    // this module returns carries the peer's own bytes. Filtering happens in
    // the row types built for printing (`discovery::SessionRow` and its
    // siblings), never here.
    //
    // Driving `fetch_remote_overview` needs a live connection, so the rule is
    // checked against the source instead.
    let source = include_str!("../remote_client.rs");
    assert!(
        !source.contains("sanitize_reported_text"),
        "remote_client.rs filters reported text; that belongs in the display rows"
    );
}

#[test]
fn a_bare_ipv6_literal_is_not_an_address() {
    // The last colon of `fe80::1` separates nothing: the host still holds a
    // colon, so the bracketed form is the only IPv6 address shape.
    assert!(!looks_like_address("::1"));
    assert!(!looks_like_address("fe80::1"));
    assert!(!looks_like_address("desk.local:+7654"), "a port is digits");
    assert!(!looks_like_address("desk.local:"), "a port is not empty");
    assert!(looks_like_address("[::1]:7654"));
    assert!(looks_like_address("laptop.local:7654"));
    assert!(!looks_like_address("laptop.local"));
    assert!(!looks_like_address("laptop.local:door"));
}
