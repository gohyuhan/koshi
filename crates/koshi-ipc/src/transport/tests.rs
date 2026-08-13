//! Tests for the framed transport: byte-exact framing over in-memory buffers,
//! error classification, and end-to-end exchanges over a real socket.

use std::io::Cursor;
use std::thread;

use super::*;
use crate::protocol::{
    ConnectionToken, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};

/// A socket address unique to this test: a temp-dir file path on Unix, a pipe
/// name on Windows.
fn test_addr(tag: &str) -> String {
    let unique = format!("koshi-ipc-{}-{tag}", std::process::id());
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(unique)
            .with_extension("sock")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        unique
    }
}

fn hello_request(request_id: u64) -> IpcRequest {
    IpcRequest {
        request_id,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: ConnectionToken::new("test-secret"),
        },
    }
}

// --- framing over in-memory buffers ---

#[test]
fn frame_is_a_big_endian_length_prefix_then_the_json_bytes() {
    let mut written: Vec<u8> = Vec::new();
    write_message(&mut written, &"hi").expect("write");
    assert_eq!(written, [0, 0, 0, 4, b'"', b'h', b'i', b'"']);
}

#[test]
fn a_frame_reads_back_as_the_message_that_was_written() {
    let sent = hello_request(7);
    let mut written: Vec<u8> = Vec::new();
    write_message(&mut written, &sent).expect("write");
    let read: IpcRequest = read_message(&mut Cursor::new(written)).expect("read");
    assert_eq!(read, sent);
}

#[test]
fn two_frames_written_back_to_back_read_back_as_two_messages_in_order() {
    let first = hello_request(1);
    let second = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Discovery,
    };
    let mut written: Vec<u8> = Vec::new();
    write_message(&mut written, &first).expect("write first");
    write_message(&mut written, &second).expect("write second");

    let mut reader = Cursor::new(written);
    let read_first: IpcRequest = read_message(&mut reader).expect("read first");
    let read_second: IpcRequest = read_message(&mut reader).expect("read second");
    assert_eq!(read_first, first);
    assert_eq!(read_second, second);
}

#[test]
fn oversized_length_prefix_is_refused_after_only_the_header_is_read() {
    let mut bytes = (MAX_FRAME_LEN + 1).to_be_bytes().to_vec();
    bytes.extend_from_slice(b"payload that must never be read");
    let mut reader = Cursor::new(bytes);

    let err = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::FrameTooLarge { len, max } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(len, u64::from(MAX_FRAME_LEN) + 1);
    assert_eq!(max, MAX_FRAME_LEN);
    assert_eq!(reader.position(), 4);
}

#[test]
fn oversized_message_is_refused_with_nothing_written() {
    let huge = "x".repeat(MAX_FRAME_LEN as usize);
    let mut written: Vec<u8> = Vec::new();

    let err = write_message(&mut written, &huge).unwrap_err();
    let IpcError::FrameTooLarge { len, max } = err else {
        panic!("wrong error: {err}");
    };
    // Encoding stops at the write that crosses the cap: the opening quote
    // byte was accepted, and the escape-free string body arrives as one
    // refused write, so the size reached is 1 + the body.
    assert_eq!(len, u64::from(MAX_FRAME_LEN) + 1);
    assert_eq!(max, MAX_FRAME_LEN);
    assert_eq!(written, Vec::<u8>::new());
}

#[test]
fn message_encoding_to_exactly_the_limit_is_sent() {
    // Two quote bytes around the body bring the payload to exactly the cap.
    let body = "x".repeat(MAX_FRAME_LEN as usize - 2);
    let mut written: Vec<u8> = Vec::new();

    write_message(&mut written, &body).expect("write");
    assert_eq!(written.len(), 4 + MAX_FRAME_LEN as usize);
    assert_eq!(written[..4], MAX_FRAME_LEN.to_be_bytes());
}

#[test]
fn empty_frame_is_a_malformed_message() {
    let mut reader = Cursor::new(vec![0, 0, 0, 0]);
    let err = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { .. } = err else {
        panic!("wrong error: {err}");
    };
}

#[test]
fn non_json_payload_is_malformed_and_the_whole_frame_is_consumed() {
    let mut bytes = 3u32.to_be_bytes().to_vec();
    bytes.extend_from_slice(b"???");
    let mut reader = Cursor::new(bytes);

    let err = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { .. } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(reader.position(), 7);
}

#[test]
fn end_of_stream_before_a_header_reads_as_disconnected() {
    let err = read_message::<String>(&mut Cursor::new(Vec::<u8>::new())).unwrap_err();
    let IpcError::Disconnected = err else {
        panic!("wrong error: {err}");
    };
}

#[test]
fn end_of_stream_inside_a_header_reads_as_disconnected() {
    let err = read_message::<String>(&mut Cursor::new(vec![0, 0])).unwrap_err();
    let IpcError::Disconnected = err else {
        panic!("wrong error: {err}");
    };
}

#[test]
fn end_of_stream_inside_a_payload_reads_as_disconnected() {
    let mut bytes = 5u32.to_be_bytes().to_vec();
    bytes.extend_from_slice(b"tr");
    let err = read_message::<String>(&mut Cursor::new(bytes)).unwrap_err();
    let IpcError::Disconnected = err else {
        panic!("wrong error: {err}");
    };
}

// --- address mapping ---

#[cfg(unix)]
#[test]
fn a_unix_address_maps_to_a_filesystem_path() {
    let name = socket_name("/tmp/koshi-test.sock").expect("map");
    assert!(name.is_path());
}

#[cfg(windows)]
#[test]
fn a_windows_address_maps_to_the_pipe_namespace() {
    let name = socket_name("koshi-test").expect("map");
    assert!(name.is_namespaced());
}

// --- end to end over a real socket ---

#[test]
fn request_and_response_cross_a_real_socket() {
    let addr = test_addr("roundtrip");
    let listener = Listener::bind(&addr).expect("bind");

    let server = thread::spawn(move || {
        let mut conn = listener.accept().expect("accept");
        let request: IpcRequest = conn.recv().expect("server recv");
        conn.send(&IpcResponse {
            request_id: Some(request.request_id),
            result: IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        })
        .expect("server send");
        request
    });

    let mut conn = Connection::connect(&addr).expect("connect");
    let sent = hello_request(7);
    conn.send(&sent).expect("client send");
    let response: IpcResponse = conn.recv().expect("client recv");

    assert_eq!(
        response,
        IpcResponse {
            request_id: Some(7),
            result: IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        }
    );
    assert_eq!(server.join().expect("server thread"), sent);
}

/// On Windows this pins the shared pipe's security descriptor from both sides:
/// the caller opens the pipe with `GENERIC_READ | GENERIC_WRITE`, and the
/// second caller is served only if the descriptor also let the listener create
/// the pipe instance behind it.
#[test]
fn a_shared_bind_serves_one_caller_after_another() {
    let addr = test_addr("shared");
    let listener = Listener::bind_shared(&addr).expect("bind shared");

    let server = thread::spawn(move || {
        for _ in 0..2 {
            let mut conn = listener.accept().expect("accept");
            let request: IpcRequest = conn.recv().expect("server recv");
            conn.send(&IpcResponse {
                request_id: Some(request.request_id),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
            })
            .expect("server send");
        }
    });

    for request_id in [1, 2] {
        let mut conn = Connection::connect(&addr).expect("connect");
        conn.send(&hello_request(request_id)).expect("client send");
        let response: IpcResponse = conn.recv().expect("client recv");
        assert_eq!(
            response,
            IpcResponse {
                request_id: Some(request_id),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
            }
        );
    }
    server.join().expect("server thread");
}

#[test]
fn a_caller_from_this_same_process_is_reported_as_the_same_user() {
    let addr = test_addr("peeruser");
    let listener = Listener::bind(&addr).expect("bind");

    let server = thread::spawn(move || {
        let conn = listener.accept().expect("accept");
        conn.peer_is_same_user().expect("peer creds")
    });

    // The caller stays connected until the server has read its credentials.
    let caller = Connection::connect(&addr).expect("connect");
    assert!(server.join().expect("server thread"));
    drop(caller);
}

#[test]
fn hello_and_request_sent_back_to_back_arrive_as_two_messages() {
    let addr = test_addr("backtoback");
    let listener = Listener::bind(&addr).expect("bind");

    let server = thread::spawn(move || {
        let mut conn = listener.accept().expect("accept");
        let first: IpcRequest = conn.recv().expect("server recv first");
        let second: IpcRequest = conn.recv().expect("server recv second");
        (first, second)
    });

    let mut conn = Connection::connect(&addr).expect("connect");
    let hello = hello_request(1);
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Discovery,
    };
    conn.send(&hello).expect("send hello");
    conn.send(&request).expect("send request");

    assert_eq!(server.join().expect("server thread"), (hello, request));
}

#[test]
fn split_halves_carry_frames_both_ways_from_two_threads() {
    let addr = test_addr("split");
    let listener = Listener::bind(&addr).expect("bind");

    let server = thread::spawn(move || {
        let (mut reader, mut writer) = listener.accept().expect("accept").split();
        // The reading half moves to its own thread and blocks there while the
        // writing half sends on this one.
        let reading = thread::spawn(move || {
            let first: IpcRequest = reader.recv().expect("server recv first");
            let second: IpcRequest = reader.recv().expect("server recv second");
            vec![first.request_id, second.request_id]
        });
        for id in [11, 12] {
            writer
                .send(&IpcResponse {
                    request_id: Some(id),
                    result: IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                })
                .expect("server send");
        }
        reading.join().expect("server reading thread")
    });

    let (mut reader, mut writer) = Connection::connect(&addr).expect("connect").split();
    let reading = thread::spawn(move || {
        let first: IpcResponse = reader.recv().expect("client recv first");
        let second: IpcResponse = reader.recv().expect("client recv second");
        vec![first.request_id, second.request_id]
    });
    for id in [21, 22] {
        writer.send(&hello_request(id)).expect("client send");
    }

    assert_eq!(
        reading.join().expect("client reading thread"),
        vec![Some(11), Some(12)]
    );
    assert_eq!(server.join().expect("server thread"), vec![21, 22]);
}

#[test]
fn one_listener_serves_two_callers_in_turn() {
    let addr = test_addr("twocallers");
    let listener = Listener::bind(&addr).expect("bind");

    let server = thread::spawn(move || {
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut conn = listener.accept().expect("accept");
            let request: IpcRequest = conn.recv().expect("server recv");
            ids.push(request.request_id);
        }
        ids
    });

    for id in [10, 20] {
        let mut conn = Connection::connect(&addr).expect("connect");
        conn.send(&IpcRequest {
            request_id: id,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send");
    }

    assert_eq!(server.join().expect("server thread"), vec![10, 20]);
}

// --- closing one connection's read direction ---

#[test]
fn a_closed_read_direction_reports_end_of_stream_on_the_next_read() {
    let addr = test_addr("readclose-next");
    let listener = Listener::bind(&addr).expect("bind");

    let (sent_tx, sent_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut conn = listener.accept().expect("accept");
        let closer = conn.read_closer().expect("take the read closer");
        sent_rx.recv().expect("the caller sent its frame");
        closer.close();
        // The caller's frame is on the socket by now, and the read still ends.
        conn.recv::<IpcRequest>()
    });

    let mut caller = Connection::connect(&addr).expect("connect");
    caller.send(&hello_request(1)).expect("client send");
    sent_tx.send(()).expect("the closer is told");

    let err = server.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = err else {
        panic!("wrong error: {err}");
    };
}

#[test]
fn a_read_closer_taken_before_the_split_closes_the_reading_half() {
    let addr = test_addr("readclose-split");
    let listener = Listener::bind(&addr).expect("bind");

    let (sent_tx, sent_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let conn = listener.accept().expect("accept");
        let closer = conn.read_closer().expect("take the read closer");
        let (mut reader, _writer) = conn.split();
        sent_rx.recv().expect("the caller sent its frame");
        closer.close();
        reader.recv::<IpcRequest>()
    });

    let mut caller = Connection::connect(&addr).expect("connect");
    caller.send(&hello_request(2)).expect("client send");
    sent_tx.send(()).expect("the closer is told");

    let err = server.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = err else {
        panic!("wrong error: {err}");
    };
}

#[test]
fn a_closed_read_direction_leaves_the_writing_direction_open() {
    let addr = test_addr("readclose-write");
    let listener = Listener::bind(&addr).expect("bind");
    let owed = IpcResponse {
        request_id: Some(3),
        result: IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    };
    let sent = owed.clone();

    let server = thread::spawn(move || {
        let mut conn = listener.accept().expect("accept");
        conn.read_closer().expect("take the read closer").close();
        conn.send(&sent).expect("server send");
    });

    let mut caller = Connection::connect(&addr).expect("connect");
    assert_eq!(caller.recv::<IpcResponse>().expect("client recv"), owed);
    server.join().expect("server thread");
}

/// Unix only: a Windows named pipe has no half-close, so a read already waiting
/// on the pipe ends when its peer sends the next frame or hangs up.
#[cfg(unix)]
#[test]
fn closing_the_read_direction_ends_a_read_the_reader_is_blocked_in() {
    use std::io::Write as _;

    let addr = test_addr("readclose-blocked");
    let listener = Listener::bind(&addr).expect("bind");

    // Half a length prefix: the reader waits inside the read for the rest of
    // the header, past the check it makes before reading.
    let mut caller = std::os::unix::net::UnixStream::connect(&addr).expect("connect");
    caller.write_all(&[0, 0]).expect("write half a header");

    let mut conn = listener.accept().expect("accept");
    let closer = conn.read_closer().expect("take the read closer");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let reading = thread::spawn(move || {
        started_tx
            .send(())
            .expect("the reader reports it is starting");
        conn.recv::<IpcRequest>()
    });

    started_rx.recv().expect("the reader started");
    thread::sleep(std::time::Duration::from_millis(50));
    closer.close();

    let err = reading.join().expect("reading thread").unwrap_err();
    let IpcError::Disconnected = err else {
        panic!("wrong error: {err}");
    };
    drop(caller);
}

#[test]
fn connecting_where_nothing_listens_reports_no_listener() {
    let expected = test_addr("nobody");
    let err = Connection::connect(&expected).unwrap_err();
    let IpcError::NoListener { addr } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(addr, expected);
}

#[cfg(unix)]
#[test]
fn connecting_to_a_stale_socket_file_reports_no_listener() {
    let expected = test_addr("stalefile");
    // `std`'s listener does not unlink its socket file on drop: the file
    // stays behind with nothing listening, as after a crash.
    let dead = std::os::unix::net::UnixListener::bind(&expected).expect("bind stale");
    drop(dead);

    let err = Connection::connect(&expected).unwrap_err();
    let IpcError::NoListener { addr } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(addr, expected);
    std::fs::remove_file(&expected).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn dropping_the_listener_unlinks_the_socket_file() {
    let addr = test_addr("unlink");
    let listener = Listener::bind(&addr).expect("bind");
    assert!(std::path::Path::new(&addr).exists());
    drop(listener);
    assert!(!std::path::Path::new(&addr).exists());
}
