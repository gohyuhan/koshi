//! Tests for the framed transport: byte-exact framing over in-memory buffers,
//! error classification, and end-to-end exchanges over a real socket.

use std::io::Cursor;
use std::sync::Mutex;
use std::thread;

use super::*;
use crate::protocol::{
    ConnectionToken, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};

/// A socket address unique to this test: a temporary-directory file path on Unix, a pipe
/// name on Windows.
fn build_test_socket_address(test_case_tag: &str) -> String {
    let unique_socket_name = format!("koshi-ipc-{}-{test_case_tag}", std::process::id());
    #[cfg(unix)]
    {
        // A Unix socket path holds about 100 bytes at most; `/tmp` keeps the
        // address short.
        std::path::Path::new("/tmp")
            .join(unique_socket_name)
            .with_extension("sock")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        unique_socket_name
    }
}

fn build_hello_request(request_id: u64) -> IpcRequest {
    IpcRequest {
        request_id,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret("test-secret"),
            is_remote: false,
        },
    }
}

// --- framing over in-memory buffers ---

#[test]
fn frame_is_a_big_endian_length_prefix_then_the_json_bytes() {
    let mut frame_bytes: Vec<u8> = Vec::new();
    write_message(&mut frame_bytes, &"hi").expect("write");
    assert_eq!(frame_bytes, [0, 0, 0, 4, b'"', b'h', b'i', b'"']);
}

/// The length prefix counts UTF-8 bytes: `"é"` is one character and four
/// payload bytes.
#[test]
fn the_length_prefix_counts_utf8_bytes_not_characters() {
    let mut frame_bytes: Vec<u8> = Vec::new();
    write_message(&mut frame_bytes, &"é").expect("write");
    assert_eq!(frame_bytes, [0, 0, 0, 4, b'"', 0xC3, 0xA9, b'"']);
    let decoded_message: String = read_message(&mut Cursor::new(frame_bytes)).expect("read");
    assert_eq!(decoded_message, "é");
}

#[test]
fn a_frame_reads_back_as_the_message_that_was_written() {
    let hello_request_message = build_hello_request(7);
    let mut frame_bytes: Vec<u8> = Vec::new();
    write_message(&mut frame_bytes, &hello_request_message).expect("write");
    let decoded_request: IpcRequest = read_message(&mut Cursor::new(frame_bytes)).expect("read");
    assert_eq!(decoded_request, hello_request_message);
}

#[test]
fn two_frames_written_back_to_back_read_back_as_two_messages_in_order() {
    let first_request = build_hello_request(1);
    let second_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Discovery,
    };
    let mut frame_bytes: Vec<u8> = Vec::new();
    write_message(&mut frame_bytes, &first_request).expect("write first");
    write_message(&mut frame_bytes, &second_request).expect("write second");

    let mut reader = Cursor::new(frame_bytes);
    let first_decoded_request: IpcRequest = read_message(&mut reader).expect("read first");
    let second_decoded_request: IpcRequest = read_message(&mut reader).expect("read second");
    assert_eq!(first_decoded_request, first_request);
    assert_eq!(second_decoded_request, second_request);
}

#[test]
fn oversized_length_prefix_is_refused_after_only_the_header_is_read() {
    let mut frame_bytes = (MAX_FRAME_BYTE_COUNT + 1).to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(b"payload that must never be read");
    let mut reader = Cursor::new(frame_bytes);

    let error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    } = error
    else {
        panic!("wrong error: {error}");
    };
    assert_eq!(frame_byte_count, u64::from(MAX_FRAME_BYTE_COUNT) + 1);
    assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
    assert_eq!(reader.position(), 4);
}

#[test]
fn oversized_message_is_refused_with_nothing_written() {
    let oversized_message = "x".repeat(MAX_FRAME_BYTE_COUNT as usize);
    let mut frame_bytes: Vec<u8> = Vec::new();

    let error = write_message(&mut frame_bytes, &oversized_message).unwrap_err();
    let IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    } = error
    else {
        panic!("wrong error: {error}");
    };
    // Encoding stops at the write that crosses the cap: the opening quote
    // byte was accepted, and the escape-free string body arrives as one
    // refused write. The size reached is 1 + the body.
    assert_eq!(frame_byte_count, u64::from(MAX_FRAME_BYTE_COUNT) + 1);
    assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
    assert_eq!(frame_bytes, Vec::<u8>::new());
}

/// `frame_byte_count` is the payload size the refused write reached, not the message's
/// full size: here the first string and the punctuation around it are
/// accepted, and the second string's body is the write that crosses the cap.
#[test]
fn the_refused_write_names_the_size_it_reached_not_the_whole_message() {
    let first_message_text = "x".repeat(MAX_FRAME_BYTE_COUNT as usize - 10);
    let second_message_text = "y".repeat(100);
    let mut frame_bytes: Vec<u8> = Vec::new();

    let error =
        write_message(&mut frame_bytes, &[first_message_text, second_message_text]).unwrap_err();
    let IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    } = error
    else {
        panic!("wrong error: {error}");
    };
    // `[`, `"`, the first body, `"`, `,` and `"` are MAX_FRAME_BYTE_COUNT - 5 bytes;
    // the second body adds 100.
    assert_eq!(frame_byte_count, u64::from(MAX_FRAME_BYTE_COUNT) + 95);
    assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
    assert_eq!(frame_bytes, Vec::<u8>::new());
}

#[test]
fn a_message_that_fails_to_encode_is_malformed_with_nothing_written() {
    struct Unencodable;

    impl Serialize for Unencodable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(<S::Error as serde::ser::Error>::custom("cannot encode"))
        }
    }

    let mut frame_bytes: Vec<u8> = Vec::new();

    let error = write_message(&mut frame_bytes, &Unencodable).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(error_detail, "cannot encode");
    assert_eq!(frame_bytes, Vec::<u8>::new());
}

#[test]
fn message_encoding_to_exactly_the_limit_is_sent() {
    // Two quote bytes around the body bring the payload to exactly the cap.
    let message_body = "x".repeat(MAX_FRAME_BYTE_COUNT as usize - 2);
    let mut frame_bytes: Vec<u8> = Vec::new();

    write_message(&mut frame_bytes, &message_body).expect("write");
    assert_eq!(frame_bytes.len(), 4 + MAX_FRAME_BYTE_COUNT as usize);
    assert_eq!(frame_bytes[..4], MAX_FRAME_BYTE_COUNT.to_be_bytes());
}

/// A prefix naming exactly the limit passes the size check; the read then
/// runs out of bytes inside the payload.
#[test]
fn a_length_prefix_of_exactly_the_limit_passes_the_size_check() {
    let mut reader = Cursor::new(MAX_FRAME_BYTE_COUNT.to_be_bytes().to_vec());

    let error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(reader.position(), 4);
}

#[test]
fn empty_frame_is_a_malformed_message() {
    let mut reader = Cursor::new(vec![0, 0, 0, 0]);
    let error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(error_detail, "EOF while parsing a value at line 1 column 0");
    assert_eq!(reader.position(), 4);
}

#[test]
fn non_json_payload_is_malformed_and_the_whole_frame_is_consumed() {
    let mut frame_bytes = 3u32.to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(b"???");
    let mut reader = Cursor::new(frame_bytes);

    let error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(error_detail, "expected value at line 1 column 1");
    assert_eq!(reader.position(), 7);
}

#[test]
fn a_well_formed_payload_of_the_wrong_type_is_malformed_and_consumed() {
    let mut frame_bytes = 1u32.to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(b"7");
    let mut reader = Cursor::new(frame_bytes);

    let error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(
        error_detail,
        "invalid type: integer `7`, expected a string at line 1 column 1"
    );
    assert_eq!(reader.position(), 5);
}

#[test]
fn bytes_after_the_json_inside_one_frame_are_malformed_and_consumed() {
    let mut frame_bytes = 5u32.to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(b"\"hi\"x");
    let mut reader = Cursor::new(frame_bytes);

    let error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(error_detail, "trailing characters at line 1 column 5");
    assert_eq!(reader.position(), 9);
}

#[test]
fn end_of_stream_before_a_header_reads_as_disconnected() {
    let error = read_message::<String>(&mut Cursor::new(Vec::<u8>::new())).unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
}

#[test]
fn end_of_stream_inside_a_header_reads_as_disconnected() {
    let error = read_message::<String>(&mut Cursor::new(vec![0, 0])).unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
}

#[test]
fn end_of_stream_inside_a_payload_reads_as_disconnected() {
    let mut frame_bytes = 5u32.to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(b"tr");
    let error = read_message::<String>(&mut Cursor::new(frame_bytes)).unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
}

/// Every IO kind that means the peer is gone reads as one error, whichever
/// kind the operating system chose: macOS answers a read from a socket whose
/// peer has closed with `ENOTCONN`, where Linux answers end of stream.
#[test]
fn every_peer_is_gone_io_kind_reads_as_disconnected() {
    for error_kind in [
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::NotConnected,
    ] {
        let error = convert_io_error(io::Error::new(error_kind, "the peer is gone"));
        let IpcError::Disconnected = error else {
            panic!("{error_kind:?} should read as disconnected, got {error}");
        };
    }
}

#[test]
fn an_io_kind_that_is_not_the_peer_going_away_keeps_its_own_words() {
    let error = convert_io_error(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "permission denied",
    ));

    let IpcError::Transport { error_detail } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(error_detail, "permission denied");
}

#[test]
fn no_listener_error_is_true_for_a_refused_or_missing_address_only() {
    for (error_kind, expected) in [
        (io::ErrorKind::ConnectionRefused, true),
        (io::ErrorKind::NotFound, true),
        (io::ErrorKind::PermissionDenied, false),
        (io::ErrorKind::TimedOut, false),
        (io::ErrorKind::Other, false),
    ] {
        assert_eq!(
            is_no_listener_error(&io::Error::new(error_kind, "probe")),
            expected,
            "{error_kind:?}"
        );
    }
    #[cfg(unix)]
    {
        assert!(is_no_listener_error(&io::Error::from_raw_os_error(
            libc::ENOTSOCK
        )));
        assert!(!is_no_listener_error(&io::Error::from_raw_os_error(
            libc::EACCES
        )));
    }
}

#[test]
fn waited_out_is_true_for_a_timeout_and_false_for_any_other_kind() {
    for (error_kind, expected) in [
        (io::ErrorKind::WouldBlock, true),
        (io::ErrorKind::TimedOut, true),
        (io::ErrorKind::Interrupted, false),
        (io::ErrorKind::UnexpectedEof, false),
        (io::ErrorKind::Other, false),
    ] {
        assert_eq!(
            is_io_timeout(&io::Error::new(error_kind, "probe")),
            expected,
            "{error_kind:?}"
        );
    }
}

// --- the frame shape on a stream that is not a local socket ---

/// An in-memory stream half that records the last deadline it was given.
struct RecordedStream {
    read_cursor: Cursor<Vec<u8>>,
    written_bytes: Arc<Mutex<Vec<u8>>>,
    deadline: Arc<Mutex<Option<Instant>>>,
}

impl Read for RecordedStream {
    fn read(&mut self, read_buffer: &mut [u8]) -> io::Result<usize> {
        self.read_cursor.read(read_buffer)
    }
}

impl Write for RecordedStream {
    fn write(&mut self, frame_bytes: &[u8]) -> io::Result<usize> {
        self.written_bytes
            .lock()
            .expect("written_bytes")
            .extend_from_slice(frame_bytes);
        Ok(frame_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Deadlined for RecordedStream {
    fn set_deadline(&mut self, deadline_instant: Option<Instant>) {
        *self.deadline.lock().expect("deadline") = deadline_instant;
    }
}

#[test]
fn frame_halves_speak_the_frame_shape_and_hand_each_deadline_to_its_half() {
    let mut framed: Vec<u8> = Vec::new();
    write_message(&mut framed, &build_hello_request(5)).expect("frame");
    let written_bytes = Arc::new(Mutex::new(Vec::new()));
    let read_deadline = Arc::new(Mutex::new(None));
    let write_deadline = Arc::new(Mutex::new(None));

    let (mut reader, mut writer) = frame_halves(
        Box::new(RecordedStream {
            read_cursor: Cursor::new(framed),
            written_bytes: Arc::clone(&written_bytes),
            deadline: Arc::clone(&read_deadline),
        }),
        Box::new(RecordedStream {
            read_cursor: Cursor::new(Vec::new()),
            written_bytes: Arc::clone(&written_bytes),
            deadline: Arc::clone(&write_deadline),
        }),
    );

    assert_eq!(
        reader.recv::<IpcRequest>().expect("recv"),
        build_hello_request(5)
    );
    writer.send(&"hi").expect("send");
    assert_eq!(
        *written_bytes.lock().expect("written_bytes"),
        [0, 0, 0, 4, b'"', b'h', b'i', b'"']
    );

    let deadline_instant = Instant::now();
    reader.set_deadline(Some(deadline_instant));
    assert_eq!(
        *read_deadline.lock().expect("deadline"),
        Some(deadline_instant)
    );
    assert_eq!(*write_deadline.lock().expect("deadline"), None);
    writer.set_deadline(Some(deadline_instant));
    assert_eq!(
        *write_deadline.lock().expect("deadline"),
        Some(deadline_instant)
    );
    reader.set_deadline(None);
    assert_eq!(*read_deadline.lock().expect("deadline"), None);
}

// --- address mapping ---

#[cfg(unix)]
#[test]
fn a_unix_address_maps_to_a_filesystem_path() {
    let socket_name = resolve_socket_name("/tmp/koshi-test.sock").expect("map");
    assert!(socket_name.is_path());
}

#[cfg(windows)]
#[test]
fn a_windows_address_maps_to_the_pipe_namespace() {
    let socket_name = resolve_socket_name("koshi-test").expect("map");
    assert!(socket_name.is_namespaced());
}

// --- end to end over a real socket ---

#[test]
fn request_and_response_cross_a_real_socket() {
    let socket_address = build_test_socket_address("roundtrip");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let request: IpcRequest = connection.recv().expect("server recv");
        connection
            .send(&IpcResponse {
                request_id: Some(request.request_id),
                answer_result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            })
            .expect("server send");
        request
    });

    let mut connection = Connection::connect(&socket_address).expect("connect");
    let hello_request_message = build_hello_request(7);
    connection
        .send(&hello_request_message)
        .expect("client send");
    let response: IpcResponse = connection.recv().expect("client recv");

    assert_eq!(
        response,
        IpcResponse {
            request_id: Some(7),
            answer_result: IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                build_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        }
    );
    assert_eq!(server.join().expect("server thread"), hello_request_message);
}

/// On Windows this pins the shared pipe's security descriptor from both sides:
/// the caller opens the pipe with `GENERIC_READ | GENERIC_WRITE`, and the
/// second caller is served only if the descriptor also let the listener create
/// the pipe instance behind it.
#[test]
fn a_shared_bind_serves_one_caller_after_another() {
    let socket_address = build_test_socket_address("shared");
    let listener = Listener::bind_shared(&socket_address).expect("bind shared");

    let server = thread::spawn(move || {
        for _ in 0..2 {
            let mut connection = listener.accept().expect("accept");
            let request: IpcRequest = connection.recv().expect("server recv");
            connection
                .send(&IpcResponse {
                    request_id: Some(request.request_id),
                    answer_result: IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        build_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                })
                .expect("server send");
        }
    });

    for request_id in [1, 2] {
        let mut connection = Connection::connect(&socket_address).expect("connect");
        connection
            .send(&build_hello_request(request_id))
            .expect("client send");
        let response: IpcResponse = connection.recv().expect("client recv");
        assert_eq!(
            response,
            IpcResponse {
                request_id: Some(request_id),
                answer_result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            }
        );
    }
    server.join().expect("server thread");
}

#[test]
fn a_caller_from_this_same_process_is_reported_as_the_same_user() {
    let socket_address = build_test_socket_address("peeruser");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server = thread::spawn(move || {
        let connection = listener.accept().expect("accept");
        connection.is_peer_same_user().expect("peer creds")
    });

    // The caller stays connected until the server has read its credentials.
    let caller = Connection::connect(&socket_address).expect("connect");
    assert!(server.join().expect("server thread"));
    drop(caller);
}

#[test]
fn hello_and_request_sent_back_to_back_arrive_as_two_messages() {
    let socket_address = build_test_socket_address("backtoback");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let first_received_request: IpcRequest = connection.recv().expect("server recv first");
        let second_received_request: IpcRequest = connection.recv().expect("server recv second");
        (first_received_request, second_received_request)
    });

    let mut connection = Connection::connect(&socket_address).expect("connect");
    let hello = build_hello_request(1);
    let request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Discovery,
    };
    connection.send(&hello).expect("send hello");
    connection.send(&request).expect("send request");

    assert_eq!(server.join().expect("server thread"), (hello, request));
}

#[test]
fn split_halves_carry_frames_both_ways_from_two_threads() {
    let socket_address = build_test_socket_address("split");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server = thread::spawn(move || {
        let (mut reader, mut writer) = listener.accept().expect("accept").split();
        // The reading half moves to its own thread and blocks there while the
        // writing half sends on this one.
        let reading = thread::spawn(move || {
            let first_received_request: IpcRequest = reader.recv().expect("server recv first");
            let second_received_request: IpcRequest = reader.recv().expect("server recv second");
            vec![
                first_received_request.request_id,
                second_received_request.request_id,
            ]
        });
        for request_id in [11, 12] {
            writer
                .send(&IpcResponse {
                    request_id: Some(request_id),
                    answer_result: IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        build_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                })
                .expect("server send");
        }
        reading.join().expect("server reading thread")
    });

    let (mut reader, mut writer) = Connection::connect(&socket_address)
        .expect("connect")
        .split();
    let reading = thread::spawn(move || {
        let first_received_response: IpcResponse = reader.recv().expect("client recv first");
        let second_received_response: IpcResponse = reader.recv().expect("client recv second");
        vec![
            first_received_response.request_id,
            second_received_response.request_id,
        ]
    });
    for request_id in [21, 22] {
        writer
            .send(&build_hello_request(request_id))
            .expect("client send");
    }

    assert_eq!(
        reading.join().expect("client reading thread"),
        vec![Some(11), Some(12)]
    );
    assert_eq!(server.join().expect("server thread"), vec![21, 22]);
}

#[test]
fn one_listener_serves_two_callers_in_turn() {
    let socket_address = build_test_socket_address("twocallers");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server = thread::spawn(move || {
        let mut request_ids = Vec::new();
        for _ in 0..2 {
            let mut connection = listener.accept().expect("accept");
            let request: IpcRequest = connection.recv().expect("server recv");
            request_ids.push(request.request_id);
        }
        request_ids
    });

    for request_id in [10, 20] {
        let mut connection = Connection::connect(&socket_address).expect("connect");
        connection
            .send(&IpcRequest {
                request_id,
                request_kind: IpcRequestKind::Discovery,
            })
            .expect("send");
    }

    assert_eq!(server.join().expect("server thread"), vec![10, 20]);
}

#[test]
fn accept_until_shutdown_serves_each_caller_and_drops_the_wake_up_connection() {
    let socket_address = build_test_socket_address("acceptloop");
    let listener = Listener::bind(&socket_address).expect("bind");
    let shutting_down = Arc::new(AtomicBool::new(false));
    let (served_tx, served_rx) = std::sync::mpsc::channel();

    let server = {
        let shutting_down = Arc::clone(&shutting_down);
        thread::spawn(move || {
            let mut served = 0;
            accept_until_shutdown(
                &listener,
                &shutting_down,
                std::time::Duration::from_millis(1),
                |connection| {
                    served += 1;
                    served_tx.send(()).expect("report the served connection");
                    drop(connection);
                },
            );
            served
        })
    };

    for _ in 0..2 {
        let _caller = Connection::connect(&socket_address).expect("connect");
        served_rx.recv().expect("the connection was served");
    }
    shutting_down.store(true, Ordering::SeqCst);
    let _wake_up = Connection::connect(&socket_address).expect("connect to wake the loop");

    assert_eq!(server.join().expect("server thread"), 2);
}

#[test]
fn a_read_after_the_peer_hangs_up_reports_disconnected() {
    let socket_address = build_test_socket_address("peergone");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server = thread::spawn(move || {
        drop(listener.accept().expect("accept"));
    });

    let mut caller = Connection::connect(&socket_address).expect("connect");
    server.join().expect("server thread");

    let error = caller.recv::<IpcResponse>().unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
}

#[cfg(unix)]
#[test]
fn binding_an_address_a_listener_already_holds_is_refused() {
    let socket_address = build_test_socket_address("bindtwice");
    let _first = Listener::bind(&socket_address).expect("bind");

    let error = Listener::bind(&socket_address).unwrap_err();
    let IpcError::Transport { error_detail } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(
        error_detail,
        io::Error::from_raw_os_error(libc::EADDRINUSE).to_string()
    );
}

/// A Unix socket path longer than `sun_path` holds is refused before any
/// bind, with the socket library's own words.
#[cfg(unix)]
#[test]
fn binding_a_path_too_long_for_a_unix_socket_is_refused() {
    let socket_address = format!(
        "/tmp/koshi-ipc-{}-{}.sock",
        std::process::id(),
        "x".repeat(200)
    );

    let error = Listener::bind(&socket_address).unwrap_err();
    let IpcError::Transport { error_detail } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(
        error_detail,
        "local socket name length exceeds capacity of sun_path of sockaddr_un"
    );
    assert!(!std::path::Path::new(&socket_address).exists());
}

// --- closing one connection's read direction ---

#[test]
fn a_closed_read_direction_reports_end_of_stream_on_the_next_read() {
    let socket_address = build_test_socket_address("readclose-next");
    let listener = Listener::bind(&socket_address).expect("bind");

    let (sent_tx, sent_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let closer = connection.read_closer().expect("take the read closer");
        sent_rx.recv().expect("the caller sent its frame");
        closer.close();
        // The caller's frame is on the socket by now, and the read still ends.
        connection.recv::<IpcRequest>()
    });

    let mut caller = Connection::connect(&socket_address).expect("connect");
    caller.send(&build_hello_request(1)).expect("client send");
    sent_tx.send(()).expect("the closer is told");

    let error = server.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
}

#[test]
fn a_second_read_closer_closes_the_same_read_direction() {
    let socket_address = build_test_socket_address("readclose-second");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let _first_read_closer = connection.read_closer().expect("take the first closer");
        let second_read_closer = connection.read_closer().expect("take the second closer");
        second_read_closer.close();
        connection.recv::<IpcRequest>()
    });

    let _caller = Connection::connect(&socket_address).expect("connect");

    let error = server.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
}

#[test]
fn closing_the_read_direction_twice_changes_nothing() {
    let socket_address = build_test_socket_address("readclose-twice");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let closer = connection.read_closer().expect("take the read closer");
        closer.close();
        closer.close();
        connection.recv::<IpcRequest>()
    });

    let _caller = Connection::connect(&socket_address).expect("connect");

    let error = server.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
}

#[test]
fn a_read_closer_taken_before_the_split_closes_the_reading_half() {
    let socket_address = build_test_socket_address("readclose-split");
    let listener = Listener::bind(&socket_address).expect("bind");

    let (sent_tx, sent_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let connection = listener.accept().expect("accept");
        let closer = connection.read_closer().expect("take the read closer");
        let (mut reader, _writer) = connection.split();
        sent_rx.recv().expect("the caller sent its frame");
        closer.close();
        reader.recv::<IpcRequest>()
    });

    let mut caller = Connection::connect(&socket_address).expect("connect");
    caller.send(&build_hello_request(2)).expect("client send");
    sent_tx.send(()).expect("the closer is told");

    let error = server.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
}

#[test]
fn a_closed_read_direction_leaves_the_writing_direction_open() {
    let socket_address = build_test_socket_address("readclose-write");
    let listener = Listener::bind(&socket_address).expect("bind");
    let expected_response = IpcResponse {
        request_id: Some(3),
        answer_result: IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    };
    let response_to_send = expected_response.clone();

    let server = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        connection
            .read_closer()
            .expect("take the read closer")
            .close();
        connection.send(&response_to_send).expect("server send");
    });

    let mut caller = Connection::connect(&socket_address).expect("connect");
    assert_eq!(
        caller.recv::<IpcResponse>().expect("client recv"),
        expected_response
    );
    server.join().expect("server thread");
}

/// Unix only. A Windows named pipe has no half-close: a read already waiting
/// on the pipe ends when its peer sends the next frame or hangs up.
#[cfg(unix)]
#[test]
fn closing_the_read_direction_ends_a_read_the_reader_is_blocked_in() {
    use std::io::Write as _;

    let socket_address = build_test_socket_address("readclose-blocked");
    let listener = Listener::bind(&socket_address).expect("bind");

    // Half a length prefix: the reader waits inside the read for the rest of
    // the header, past the check it makes before reading.
    let mut caller = std::os::unix::net::UnixStream::connect(&socket_address).expect("connect");
    caller.write_all(&[0, 0]).expect("write half a header");

    let mut connection = listener.accept().expect("accept");
    let closer = connection.read_closer().expect("take the read closer");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let reading = thread::spawn(move || {
        started_tx
            .send(())
            .expect("the reader reports it is starting");
        connection.recv::<IpcRequest>()
    });

    started_rx.recv().expect("the reader started");
    thread::sleep(std::time::Duration::from_millis(50));
    closer.close();

    let error = reading.join().expect("reading thread").unwrap_err();
    let IpcError::Disconnected = error else {
        panic!("wrong error: {error}");
    };
    drop(caller);
}

#[test]
fn connecting_where_nothing_listens_reports_no_listener() {
    let expected_socket_address = build_test_socket_address("nobody");
    let error = Connection::connect(&expected_socket_address).unwrap_err();
    let IpcError::NoListener { socket_address } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(socket_address, expected_socket_address);
}

#[cfg(unix)]
#[test]
fn connecting_to_a_stale_socket_file_reports_no_listener() {
    let expected_socket_address = build_test_socket_address("stalefile");
    // `std`'s listener does not unlink its socket file on drop: the file
    // stays behind with nothing listening, as after a crash.
    let stale_listener =
        std::os::unix::net::UnixListener::bind(&expected_socket_address).expect("bind stale");
    drop(stale_listener);

    let error = Connection::connect(&expected_socket_address).unwrap_err();
    let IpcError::NoListener { socket_address } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(socket_address, expected_socket_address);
    std::fs::remove_file(&expected_socket_address).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn dropping_the_listener_unlinks_the_socket_file() {
    let socket_address = build_test_socket_address("unlink");
    let listener = Listener::bind(&socket_address).expect("bind");
    assert!(std::path::Path::new(&socket_address).exists());
    drop(listener);
    assert!(!std::path::Path::new(&socket_address).exists());
}

/// The raw halves carry bytes with no frame shape around them: five bytes
/// handed in arrive as those same five bytes.
#[test]
fn raw_halves_carry_the_bytes_as_given_with_no_frame_around_them() {
    let socket_address = build_test_socket_address("rawsplit");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server = thread::spawn(move || {
        let (mut reader, mut writer) = listener.accept().expect("accept").split_raw();
        let mut received_bytes = [0u8; 5];
        reader.read_exact(&mut received_bytes).expect("server read");
        writer.write_all(b"pong").expect("server write");
        writer.flush().expect("server flush");
        received_bytes
    });

    let (mut reader, mut writer) = Connection::connect(&socket_address)
        .expect("connect")
        .split_raw();
    writer.write_all(b"hello").expect("client write");
    writer.flush().expect("client flush");
    let mut response_bytes = [0u8; 4];
    reader.read_exact(&mut response_bytes).expect("client read");

    assert_eq!(&response_bytes, b"pong");
    assert_eq!(&server.join().expect("server thread"), b"hello");
}
