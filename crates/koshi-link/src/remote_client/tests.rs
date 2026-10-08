//! Tests for the dialling side: which strings count as an address, which saved
//! names are refused, which of the three lookup answers leads to a pinned dial,
//! which answer to a Hello reads as a refusal a repeat dial cannot change, what
//! an open link makes of each frame a server can answer a listing with, what
//! one sweep of the saved servers reports, and how the lock that guards a
//! change to the saved-server store behaves.

use std::io::Write;
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
        validate_remote_server_answer("desk.local:7654", &server_frame).unwrap_or_else(|_| {
            panic!(
                "remote protocol version {remote_protocol_version} is inside the range this build \
                 speaks"
            )
        });
    }
}

#[test]
fn a_welcome_naming_a_remote_protocol_version_this_build_does_not_speak_is_refused() {
    let server_frame = RemoteServerFrame::Welcome {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION + 1,
    };

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
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

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
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

    let refusal = validate_remote_server_answer("desk.local:7654", &server_frame)
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
            server_address: "laptop.local:7654".to_string()
        }
        .format_server_label(),
        "laptop.local:7654"
    );
}

#[test]
fn the_refusal_every_rejected_token_carries_names_an_old_server_and_both_ways_to_replace_it() {
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
        "the server desk.local:7654 did not admit the connection. if that machine runs \
         koshi 0.3.0 or 0.4.0, update koshi there. otherwise the token was rejected or \
         revoked: re-grant it on that machine with `koshi share grant`, then store the new \
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
fn a_server_not_heard_from_comes_back_unreachable() {
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
            Reach::Unreachable {
                server_label: "work".to_string(),
            },
        ]
    );
}

#[test]
fn a_sweep_with_every_server_heard_adds_nothing_and_sorts_by_server() {
    let received_reaches = vec![
        Reach::Refused {
            server_label: "work".to_string(),
            refusal_detail: "this server did not admit the connection".to_string(),
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
            Reach::Refused {
                server_label: "work".to_string(),
                refusal_detail: "this server did not admit the connection".to_string(),
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
            Reach::Unreachable {
                server_label: "desk".to_string(),
            },
            Reach::Unreachable {
                server_label: "work".to_string(),
            },
        ]
    );
}

/// An in-memory byte source standing in for the server's half of a link. The
/// deadline is taken and ignored.
struct ServerFrameByteStream(std::io::Cursor<Vec<u8>>);

impl Read for ServerFrameByteStream {
    fn read(&mut self, byte_buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(byte_buffer)
    }
}

impl koshi_ipc::transport::Deadlined for ServerFrameByteStream {
    fn set_deadline(&mut self, _deadline: Option<Instant>) {}
}

/// A shared byte buffer that keeps every write for a reader.
type SharedWrittenByteBufferHandle = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

/// An in-memory byte sink standing in for this client's half of a link,
/// keeping every written byte. The deadline is taken and ignored.
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
            certificate_fingerprint: "00".repeat(32),
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
fn attaching_writes_one_attach_frame_naming_the_session() {
    let session_id = SessionId::new();
    let (remote_link, written_bytes) = build_remote_link(Vec::new());

    let (_frame_reader, frame_writer) =
        attach_remote_session(remote_link, SessionSelector::SessionId(session_id))
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
        }
    );
}

// A saved server pinning no certificate is never dialled by the sweep.
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

// The pin a dial presents is read from the store by address or by name.
#[test]
fn the_pin_for_an_address_is_the_one_its_record_holds() {
    let mut saved_server_store = ServerStore::new();
    let mut saved_server = build_saved_server();
    saved_server.certificate_fingerprint = Some("cd".repeat(32));
    saved_server_store
        .save_server(saved_server)
        .expect("the store takes it");

    assert_eq!(
        find_pinned_certificate_fingerprint(&saved_server_store, "desk.local:7654"),
        Some("cd".repeat(32)),
        "the address finds the saved server that holds the pin"
    );
    assert_eq!(
        find_pinned_certificate_fingerprint(&saved_server_store, "work"),
        Some("cd".repeat(32)),
        "and so does the name it was saved under"
    );
}

#[test]
fn an_address_no_record_holds_and_a_record_that_pins_nothing_both_pin_nothing() {
    let mut saved_server_store = ServerStore::new();
    let mut saved_server = build_saved_server();
    saved_server.certificate_fingerprint = None;
    saved_server_store
        .save_server(saved_server)
        .expect("the store takes it");

    assert_eq!(
        find_pinned_certificate_fingerprint(&saved_server_store, "desk.local:7654"),
        None
    );
    assert_eq!(
        find_pinned_certificate_fingerprint(&saved_server_store, "nobody.local:7654"),
        None
    );
}

// Two records answering to one word pin nothing for that word. A hand-written
// file can hold such a pair; `ServerStore::save_server` refuses to make one.
#[test]
fn a_word_two_records_answer_to_pins_nothing() {
    let mut saved_server_store = ServerStore::new();
    let mut primary_saved_server = build_saved_server();
    primary_saved_server.certificate_fingerprint = Some("cd".repeat(32));
    let mut conflicting_saved_server = build_saved_server();
    conflicting_saved_server.server_address = "laptop.local:7654".to_string();
    conflicting_saved_server.certificate_fingerprint = Some("ef".repeat(32));
    saved_server_store.saved_servers.push(primary_saved_server);
    saved_server_store
        .saved_servers
        .push(conflicting_saved_server);

    assert_eq!(
        find_pinned_certificate_fingerprint(&saved_server_store, "work"),
        None
    );
}

/// How long a lock test waits before it reads the lock as held: 50 ms.
const TEST_LOCK_WAIT_DURATION: Duration = Duration::from_millis(50);

#[test]
fn a_lock_taken_where_nothing_exists_makes_the_file_and_the_directory() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("remote").join("servers.lock");

    let held_lock_file = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
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

    let held_lock_file = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
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
    let held_lock_file = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
        .expect("nothing else holds it");

    assert_eq!(
        acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
            .expect_err("the first holder has it")
            .to_string(),
        "IPC unavailable: another koshi is changing the saved servers; try again"
    );
    drop(held_lock_file);
}

/// A wait of nothing tries the lock once and reports it held.
#[test]
fn a_lock_another_holder_keeps_is_refused_with_no_wait_at_all() {
    let test_directory = tempfile::tempdir().expect("a temp directory");
    let lock_file_path = test_directory.path().join("servers.lock");
    let held_lock_file =
        acquire_store_lock(&lock_file_path, Duration::ZERO).expect("nothing else holds it");

    assert_eq!(
        acquire_store_lock(&lock_file_path, Duration::ZERO)
            .expect_err("the first holder has it")
            .to_string(),
        "IPC unavailable: another koshi is changing the saved servers; try again"
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

    let refusal = acquire_store_lock(
        &blocking_file_path.join("servers.lock"),
        TEST_LOCK_WAIT_DURATION,
    )
    .expect_err("a file is in the way");

    let CliError::IpcUnavailable { detail } = refusal else {
        panic!("a lock that cannot be taken is a transport failure");
    };
    assert_eq!(
        detail,
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

    let first_held_lock_file = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
        .expect("nothing else holds it");
    drop(first_held_lock_file);

    let second_held_lock_file = acquire_store_lock(&lock_file_path, TEST_LOCK_WAIT_DURATION)
        .expect("the first holder let it go");
    drop(second_held_lock_file);
}

/// A refusal the server sends in answer to a listing passes through
/// `sanitize_reported_text` before it is returned: control bytes are dropped.
#[test]
fn a_refused_listing_loses_what_a_terminal_would_act_on() {
    let mut remote_link = build_link_with_server_response(&RemoteServerFrame::Refused {
        message: "\u{1b}[2Jthe session\u{7f} is gone".to_string(),
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
    // The host before the last colon of `fe80::1` still holds a colon: only
    // the bracketed form is an IPv6 address shape.
    assert!(!is_server_address("::1"));
    assert!(!is_server_address("fe80::1"));
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
        }));

    let forwarded_answer_result = read_forwarded_hello_answer(
        &mut frame_reader,
        &SessionSelector::SessionName(String::from("quiet-lake")),
    );

    let Err(DialError::Restarting(CliError::IpcUnavailable { detail })) = forwarded_answer_result
    else {
        panic!("expected a restarting answer, got {forwarded_answer_result:?}");
    };
    assert_eq!(detail, ROUTER_RESTARTING_MESSAGE);
}

#[test]
fn any_other_forwarded_refusal_reads_as_the_token_not_reaching_the_session() {
    let mut frame_reader =
        build_frame_reader(serialize_server_frame(&RemoteServerFrame::Refused {
            message: remote_wire::REMOTE_REFUSED.to_string(),
        }));

    let forwarded_answer_result = read_forwarded_hello_answer(
        &mut frame_reader,
        &SessionSelector::SessionName(String::from("quiet-lake")),
    );

    let Err(DialError::Refused(CliError::Runtime { detail })) = forwarded_answer_result else {
        panic!("expected a refusal, got {forwarded_answer_result:?}");
    };
    assert_eq!(
        detail,
        "the token this server saved does not reach session quiet-lake"
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
                    "certificate changed",
                ))),
            ],
            &dial_count,
        ),
    );

    let Err(DialError::Refused(refusal)) = dial_result else {
        panic!("expected the refusal, got {dial_result:?}");
    };
    assert_eq!(refusal.to_string(), "IPC unavailable: certificate changed");
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
