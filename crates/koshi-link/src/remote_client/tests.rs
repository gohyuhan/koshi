//! Tests for the dialling side: which strings count as an address, which saved
//! names are refused, which of the three lookup answers leads to a pinned dial,
//! which answer to a Hello reads as a refusal a repeat dial cannot change, what
//! an open link makes of each frame a server can answer a listing with, what
//! one sweep of the saved servers reports, and how the lock that guards a
//! change to the saved-server store behaves.

use std::time::SystemTime;

use koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT;

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
    // A name with the `host:port` shape can collide with an address, and a word
    // two records answer to reaches neither.
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

#[test]
fn an_empty_saved_name_is_refused() {
    // `--save-as ""` reaches here: clap takes the empty string, and a saved server
    // named with it lists blank.
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
fn a_one_shot_command_waits_a_bounded_time_and_an_attachment_does_not() {
    // A one-shot verb passes `Some(REPLY_TIMEOUT_DURATION)`; an attached client passes
    // `None`. The reply window is at least as long as the dial before it.
    assert!(
        REPLY_TIMEOUT_DURATION > DIAL_TIMEOUT_DURATION,
        "a reply has at least as long as the dial that asked for it"
    );
    assert_eq!(DIAL_TIMEOUT_DURATION, Duration::from_secs(10));
    assert_eq!(REPLY_TIMEOUT_DURATION, Duration::from_secs(20));
}

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
fn a_saved_server_is_dialled_with_the_certificate_pinned_for_it() {
    let saved_server = build_saved_server();
    let resolved_server_reference =
        resolve_server_reference(SavedServerLookup::Saved(&saved_server), "work")
            .expect("a saved server resolves");
    assert_eq!(
        resolved_server_reference,
        ServerReference::Saved(saved_server)
    );
}

#[test]
fn an_ambiguous_selector_is_refused_and_never_dialled_as_a_new_server() {
    // `ServerReference::New` dials with no pinned fingerprint and saves whichever
    // certificate is presented. An ambiguous selector is refused even when it
    // has the shape of an address.
    let refusal = resolve_server_reference(SavedServerLookup::Ambiguous, "laptop.local:7654")
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
    let resolved_server_reference =
        resolve_server_reference(SavedServerLookup::NotSaved, "laptop.local:7654")
            .expect("an address with nothing saved is a new server");
    assert_eq!(
        resolved_server_reference,
        ServerReference::New {
            server_address: "laptop.local:7654".to_string()
        }
    );
}

#[test]
fn a_plain_word_nothing_is_saved_under_is_refused_rather_than_dialled() {
    let refusal = resolve_server_reference(SavedServerLookup::NotSaved, "work")
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
fn a_selector_with_nothing_in_it_names_no_saved_server() {
    let refusal = resolve_server_reference(SavedServerLookup::NotSaved, "")
        .expect_err("an empty selector names nothing");

    let CliError::InvalidArgs { detail } = refusal else {
        panic!("a selector that names nothing is a bad argument, not a runtime failure");
    };
    assert_eq!(detail, "no saved server is named ; run `koshi remote list`");
}

#[test]
fn naming_a_server_that_is_already_saved_is_refused_rather_than_ignored() {
    // `--save-as` names a server this machine has not connected to.
    let saved_server = build_saved_server();

    let refusal = connect_saved_server(&ServerReference::Saved(saved_server), Some("home"), None)
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
    // A saved server with no name is labelled by its address.
    let mut saved_server = build_saved_server();
    saved_server.server_name = None;

    let refusal = connect_saved_server(&ServerReference::Saved(saved_server), Some("home"), None)
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
        let server_frame = RemoteServerFrame::Welcome {
            remote_protocol_version: version,
        };
        validate_remote_server_answer("desk.local:7654", &server_frame)
            .unwrap_or_else(|_| panic!("doorway {version} is inside the range this build speaks"));
    }
}

#[test]
fn a_welcome_naming_a_doorway_this_build_does_not_speak_is_refused() {
    let server_frame = RemoteServerFrame::Welcome {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION + 1,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
        .expect_err("the doorway is too new");

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
    let server_frame = RemoteServerFrame::Welcome {
        remote_protocol_version: MIN_REMOTE_PROTOCOL_VERSION - 1,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
        .expect_err("the doorway is too old");

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
    let server_frame = RemoteServerFrame::Sessions {
        session_rows: Vec::new(),
    };

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
        .expect_err("a listing is not a welcome");

    // `probe_saved_server` reads a refused dial carrying `CliError::IpcUnavailable` as the
    // pinned-certificate check, so a frame that answers nothing carries
    // `CliError::Runtime` like every other refusal `validate_remote_server_answer` builds.
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
            server_address: "laptop.local:7654".to_string()
        }
        .format_server_label(),
        "laptop.local:7654"
    );
}

#[test]
fn the_refusal_every_rejected_token_carries_names_both_ways_to_replace_it() {
    let server_frame = RemoteServerFrame::Refused {
        message: remote_wire::REMOTE_REFUSED.to_string(),
    };

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
        .expect_err("a refusal is not a welcome");

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
    let server_frame = RemoteServerFrame::Refused {
        message: "the session is gone".to_string(),
    };

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
        .expect_err("a refusal is not a welcome");

    let DialError::Refused(CliError::Runtime { detail }) = refusal else {
        panic!("a server that answered gives every dial after it the same answer");
    };
    assert_eq!(detail, "the session is gone (server desk.local:7654)");
}

#[test]
fn a_certificate_that_changed_carries_an_ipc_failure_and_never_a_runtime_one() {
    // `probe_saved_server` reads a refused dial carrying `CliError::IpcUnavailable` as the
    // pinned-certificate check and answers `Reach::CertificateChanged`; every
    // refusal `validate_remote_server_answer` builds carries `CliError::Runtime` instead.
    let certificate_change_error = classify_dial_failure(IpcError::CertificateChanged {
        server_address: "desk.local:7654".to_string(),
        pinned_certificate: "aa".repeat(32),
        presented_certificate: "bb".repeat(32),
    });

    let DialError::Refused(CliError::IpcUnavailable { detail }) = certificate_change_error else {
        panic!("a changed certificate is the shape `probe_saved_server` reads");
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
        get_reach_server_label(&Reach::CertificateChanged {
            server_label: "desk".to_string(),
            certificate_error_detail: "the certificate of desk.local:7654 changed".to_string(),
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

// End of stream with nothing typed is not an empty answer: it is the input
// ending, which every prompt that asks again must stop on.
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
fn a_server_not_heard_from_comes_back_unreachable() {
    let heard_reach_results = vec![Reach::Reached {
        server_label: "desk".to_string(),
        session_rows: Vec::new(),
    }];
    let requested_server_names = vec!["desk".to_string(), "work".to_string()];

    assert_eq!(
        complete_reach_results(heard_reach_results, requested_server_names),
        vec![
            Reach::Reached {
                server_label: "desk".to_string(),
                session_rows: Vec::new(),
            },
            Reach::Unreachable {
                server_label: "work".to_string(),
            },
        ]
    );
}

#[test]
fn a_sweep_with_every_server_heard_adds_nothing_and_sorts_by_server() {
    let heard_reach_results = vec![
        Reach::Refused {
            server_label: "work".to_string(),
        },
        Reach::Reached {
            server_label: "desk".to_string(),
            session_rows: Vec::new(),
        },
    ];
    let requested_server_names = vec!["desk".to_string(), "work".to_string()];

    assert_eq!(
        complete_reach_results(heard_reach_results, requested_server_names),
        vec![
            Reach::Reached {
                server_label: "desk".to_string(),
                session_rows: Vec::new(),
            },
            Reach::Refused {
                server_label: "work".to_string(),
            },
        ]
    );
}

#[test]
fn a_sweep_that_heard_nothing_reports_every_asked_server() {
    let requested_server_names = vec!["work".to_string(), "desk".to_string()];

    assert_eq!(
        complete_reach_results(Vec::new(), requested_server_names),
        vec![
            Reach::Unreachable {
                server_label: "desk".to_string(),
            },
            Reach::Unreachable {
                server_label: "work".to_string(),
            },
        ]
    );
}

/// An in-memory byte source serving as one half of a encode_server_frame link. The
/// deadline is taken and ignored.
struct ServerFrameByteStream(std::io::Cursor<Vec<u8>>);

impl Read for ServerFrameByteStream {
    fn read(&mut self, byte_buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(byte_buffer)
    }
}

impl koshi_ipc::transport::Deadlined for ServerFrameByteStream {
    fn set_deadline(&mut self, _at: Option<Instant>) {}
}

/// An in-memory byte sink serving as one half of a encode_server_frame link, keeping every
/// written byte. The deadline is taken and ignored.
struct SharedWrittenByteBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

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
    fn set_deadline(&mut self, _at: Option<Instant>) {}
}

/// A shared byte buffer that keeps every write for a reader.
type SharedWrittenByteBufferHandle = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

/// Build a link reading `server_frame_bytes` as the bytes the server sent,
/// together with the buffer this side's own writes go into.
fn build_remote_link(server_frame_bytes: Vec<u8>) -> (RemoteLink, SharedWrittenByteBufferHandle) {
    use koshi_ipc::transport::frame_halves;

    let written_bytes: SharedWrittenByteBufferHandle =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (reader, writer) = frame_halves(
        Box::new(ServerFrameByteStream(std::io::Cursor::new(
            server_frame_bytes,
        ))),
        Box::new(SharedWrittenByteBuffer(written_bytes.clone())),
    );
    (
        RemoteLink {
            reader,
            writer,
            certificate_fingerprint: "00".repeat(32),
        },
        written_bytes,
    )
}

/// Encode the `server_frame` bytes a server sends.
fn encode_server_frame(server_frame: &RemoteServerFrame) -> Vec<u8> {
    let (link, written_bytes) = build_remote_link(Vec::new());
    let mut encoder = link.writer;
    encoder
        .send(server_frame)
        .expect("a buffer takes every byte");
    let frame_bytes = written_bytes
        .lock()
        .expect("the encoder is finished")
        .clone();
    frame_bytes
}

/// Decode the one client frame held in `client_frame_bytes`.
fn decode_client_frame(client_frame_bytes: Vec<u8>) -> RemoteClientFrame {
    let (mut link, _) = build_remote_link(client_frame_bytes);
    link.reader.recv().expect("the frame decodes")
}

/// A link whose server side already answered `server_frame`, and whose own writes
/// go into a kept buffer nobody reads.
fn build_link_with_server_response(server_frame: &RemoteServerFrame) -> RemoteLink {
    build_remote_link(encode_server_frame(server_frame)).0
}

#[test]
fn listed_rows_arrive_exactly_as_the_server_sent_them() {
    // A name identifies a session, so the listing carries what the server
    // said. Filtering happens where a name is printed.
    let session_id = SessionId::new();
    let mut link = build_link_with_server_response(&RemoteServerFrame::Sessions {
        session_rows: vec![RemoteSessionRow {
            session_id,
            session_name: "dev\x1b[2K".to_string(),
        }],
    });

    assert_eq!(
        list_remote_sessions(&mut link).expect("the sessions frame is the answer"),
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
    let mut link = build_link_with_server_response(&RemoteServerFrame::Sessions {
        session_rows: session_rows.clone(),
    });

    assert_eq!(
        list_remote_sessions(&mut link).expect("the sessions frame is the answer"),
        session_rows
    );
}

#[test]
fn a_server_holding_no_session_lists_nothing() {
    let mut link = build_link_with_server_response(&RemoteServerFrame::Sessions {
        session_rows: Vec::new(),
    });

    assert_eq!(
        list_remote_sessions(&mut link).expect("an empty listing is an answer"),
        Vec::<RemoteSessionRow>::new()
    );
}

#[test]
fn a_refused_listing_carries_the_sentence_the_server_sent() {
    let mut link = build_link_with_server_response(&RemoteServerFrame::Refused {
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
    let mut link = build_link_with_server_response(&RemoteServerFrame::Welcome {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
    });

    let refusal = list_remote_sessions(&mut link).expect_err("a welcome is not a listing");

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
    let (mut link, _) = build_remote_link(Vec::new());

    let refusal = list_remote_sessions(&mut link).expect_err("nothing answers");

    let CliError::IpcUnavailable { detail } = refusal else {
        panic!("a link that ended is a transport failure");
    };
    assert_eq!(detail, "ipc peer disconnected");
}

#[test]
fn attaching_writes_one_attach_frame_naming_the_session() {
    let session = SessionId::new();
    let (link, written) = build_remote_link(Vec::new());

    let (_reader, writer) = attach_remote_session(link, SessionSelector::SessionId(session))
        .expect("the attach is written");
    drop(writer);

    let sent_frame_bytes = written.lock().expect("the writer is finished").clone();
    assert_eq!(
        decode_client_frame(sent_frame_bytes),
        RemoteClientFrame::Attach {
            session_selector: SessionSelector::SessionId(session),
        }
    );
}

// A saved server pinning no certificate is never dialled by the sweep: presenting
// the secret to whatever answers at that address is what pinning prevents.
#[test]
fn a_record_with_no_pinned_certificate_is_unchecked_and_is_not_dialled() {
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

// A dial that reaches nothing is unreachable, not refused: a refusal is a
// sentence the server sent.
#[test]
fn a_pinned_server_nothing_answers_for_is_unreachable() {
    let mut saved_server = build_saved_server();
    // Port 1 of the loopback address: a connect there is refused, and a
    // machine that drops it instead runs out of the deadline below.
    saved_server.server_address = "127.0.0.1:1".to_string();

    assert_eq!(
        probe_saved_server(&saved_server, Instant::now() + Duration::from_millis(200)),
        Reach::Unreachable {
            server_label: "work".to_string(),
        }
    );
}

// Every entry the sweep produces sorts by server name, whatever it says.
#[test]
fn an_unchecked_server_takes_its_place_among_the_answers() {
    let heard = vec![
        Reach::Unchecked {
            server_label: "work".to_string(),
        },
        Reach::Reached {
            server_label: "desk".to_string(),
            session_rows: Vec::new(),
        },
    ];

    assert_eq!(
        complete_reach_results(heard, vec!["desk".to_string(), "work".to_string()]),
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
    let mut leading = std::io::Cursor::new(b"\x7f\x08secret\n".to_vec());
    assert_eq!(read_hidden_terminal_line(&mut leading).unwrap(), "secret");

    let mut both = std::io::Cursor::new(b"secretxy\x08\x7f\n".to_vec());
    assert_eq!(read_hidden_terminal_line(&mut both).unwrap(), "secret");
}

// A secret is bytes until it is read back, so bytes that are not UTF-8 come
// back as the replacement character instead of ending the entry.
#[test]
fn read_hidden_terminal_line_replaces_bytes_that_are_not_utf_8() {
    let mut broken = std::io::Cursor::new(b"se\xffcret\n".to_vec());
    assert_eq!(
        read_hidden_terminal_line(&mut broken).unwrap(),
        "se\u{fffd}cret"
    );
}

// The pin a dial presents is read from the store by address, so a saved server that
// pinned nothing when it was taken is still dialled against the certificate an
// earlier dial saved.
#[test]
fn the_pin_for_an_address_is_the_one_its_record_holds() {
    let mut store = ServerStore::new();
    let mut saved_server = build_saved_server();
    saved_server.certificate_fingerprint = Some("cd".repeat(32));
    store.save_server(saved_server).expect("the store takes it");

    assert_eq!(
        find_pinned_certificate_fingerprint(&store, "desk.local:7654"),
        Some("cd".repeat(32)),
        "the address finds the saved server that holds the pin"
    );
    assert_eq!(
        find_pinned_certificate_fingerprint(&store, "work"),
        Some("cd".repeat(32)),
        "and so does the name it was saved under"
    );
}

#[test]
fn an_address_no_record_holds_and_a_record_that_pins_nothing_both_pin_nothing() {
    let mut store = ServerStore::new();
    let mut saved_server = build_saved_server();
    saved_server.certificate_fingerprint = None;
    store.save_server(saved_server).expect("the store takes it");

    assert_eq!(
        find_pinned_certificate_fingerprint(&store, "desk.local:7654"),
        None
    );
    assert_eq!(
        find_pinned_certificate_fingerprint(&store, "nobody.local:7654"),
        None
    );
}

// Two records answering to one word name neither, so a dial against that word
// presents no pin. `ServerStore::save_server` refuses to make such a pair, and a
// hand-written file holds one.
#[test]
fn a_word_two_records_answer_to_pins_nothing() {
    let mut store = ServerStore::new();
    let mut primary_saved_server = build_saved_server();
    primary_saved_server.certificate_fingerprint = Some("cd".repeat(32));
    let mut conflicting_saved_server = build_saved_server();
    conflicting_saved_server.server_address = "laptop.local:7654".to_string();
    conflicting_saved_server.certificate_fingerprint = Some("ef".repeat(32));
    store.saved_servers.push(primary_saved_server);
    store.saved_servers.push(conflicting_saved_server);

    assert_eq!(find_pinned_certificate_fingerprint(&store, "work"), None);
}

/// How long a lock test waits before it reads the lock as held. Short enough
/// that a refusal test does not slow the suite down.
const TEST_LOCK_WAIT_DURATION: Duration = Duration::from_millis(50);

#[test]
fn a_lock_taken_where_nothing_exists_makes_the_file_and_the_directory() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("remote").join("servers.lock");

    let lock_handle = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
        .expect("nothing else holds it");

    assert!(
        lock_file_path.is_file(),
        "the lock file is made where it was missing"
    );
    drop(lock_handle);
}

/// The lock sits beside the saved secrets, so the directory it goes in and the
/// file itself carry the same owner-only modes the store carries.
#[cfg(unix)]
#[test]
fn a_lock_and_the_directory_holding_it_are_readable_by_their_owner_alone() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_directory_path = test_directory.path().join("remote");
    let lock_file_path = lock_directory_path.join("servers.lock");

    let lock_handle = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
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
    drop(lock_handle);
}

/// The second koshi finds the lock held and says so rather than writing over
/// the first one's change.
#[test]
fn a_lock_another_holder_keeps_is_refused_after_the_wait() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("servers.lock");
    let lock_handle = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
        .expect("nothing else holds it");

    assert_eq!(
        acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
            .expect_err("the first holder has it")
            .to_string(),
        "IPC unavailable: another koshi is changing the saved servers; try again"
    );
    drop(lock_handle);
}

/// A wait of nothing tries the lock once and reports it held.
#[test]
fn a_lock_another_holder_keeps_is_refused_with_no_wait_at_all() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("servers.lock");
    let lock_handle =
        acquire_store_lock(&lock_file_path, Duration::ZERO).expect("nothing else holds it");

    assert_eq!(
        acquire_store_lock(&lock_file_path, Duration::ZERO)
            .expect_err("the first holder has it")
            .to_string(),
        "IPC unavailable: another koshi is changing the saved servers; try again"
    );
    drop(lock_handle);
}

/// A file where the directory belongs stops the lock, and the failure names
/// the directory that could not be made.
#[test]
fn a_lock_whose_directory_cannot_be_made_names_that_directory() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let blocker = test_directory.path().join("remote");
    std::fs::write(&blocker, b"a file where the directory belongs").expect("the file is written");

    let refusal = acquire_store_lock(&blocker.join("servers.lock"), TEST_LOCK_WAIT_DURATION)
        .expect_err("a file is in the way");

    let CliError::IpcUnavailable { detail } = refusal else {
        panic!("a lock that cannot be taken is a transport failure");
    };
    let expected_prefix = format!("{} could not be made: ", blocker.display());
    assert!(
        detail.starts_with(&expected_prefix),
        "the failure opens with {expected_prefix:?}, and reads {detail:?}"
    );
}

/// The first koshi finished, so the next one takes the lock instead of
/// reporting it held.
#[test]
fn a_lock_its_holder_released_is_taken_by_the_next_caller() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("servers.lock");

    let lock_handle = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
        .expect("nothing else holds it");
    drop(lock_handle);

    let second_lock_handle = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
        .expect("the first holder let it go");
    drop(second_lock_handle);
}

/// A name is what `targeting.rs` matches on, so a listed row carries the
/// server's own bytes and is filtered where it is printed
/// (`discovery::SessionRow`). A refusal is never matched on and never reaches a
/// row type: it goes straight to the terminal, so it is filtered here.
#[test]
fn a_refused_listing_loses_what_a_terminal_would_act_on() {
    let mut link = build_link_with_server_response(&RemoteServerFrame::Refused {
        message: "\u{1b}[2Jthe session\u{7f} is gone".to_string(),
    });

    let refusal = list_remote_sessions(&mut link).expect_err("the listing is refused");

    let CliError::Runtime { detail } = refusal else {
        panic!("a refusal the server sent is a runtime failure, not a transport one");
    };
    assert_eq!(detail, "[2Jthe session is gone");
}

#[test]
fn a_refused_dial_loses_what_a_terminal_would_act_on() {
    let server_frame = RemoteServerFrame::Refused {
        message: "\u{1b}[2Jthe session\u{7f} is gone".to_string(),
    };

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
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
    };

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
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
    // The last colon of `fe80::1` separates nothing: the host still holds a
    // colon, so the bracketed form is the only IPv6 address shape.
    assert!(!is_server_address("::1"));
    assert!(!is_server_address("fe80::1"));
    assert!(!is_server_address("desk.local:+7654"), "a port is digits");
    assert!(!is_server_address("desk.local:"), "a port is not empty");
    assert!(is_server_address("[::1]:7654"));
    assert!(is_server_address("laptop.local:7654"));
    assert!(!is_server_address("laptop.local"));
    assert!(!is_server_address("laptop.local:door"));
}
