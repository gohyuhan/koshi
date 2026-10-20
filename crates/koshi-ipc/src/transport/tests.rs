//! Tests for the framed transport: byte-exact framing over in-memory buffers,
//! error classification, and end-to-end exchanges over a real socket.

use std::io::Cursor;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use super::*;
use crate::protocol::{
    ConnectionToken, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, ProcessImageId,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
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
            minimum_protocol_version: MIN_PROTOCOL_VERSION,
            maximum_protocol_version: PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret("test-secret"),
            is_remote: false,
        },
    }
}

/// The answer to the Hello request `request_id`, written out by hand: protocol
/// [`PROTOCOL_VERSION`], this crate's version, and this process's image id.
fn build_hello_answer(request_id: u64) -> IpcResponse {
    IpcResponse {
        request_id: Some(request_id),
        answer_result: IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
            process_image_id: ProcessImageId::get_own(),
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

    let read_error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    } = read_error
    else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(frame_byte_count, u64::from(MAX_FRAME_BYTE_COUNT) + 1);
    assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
    assert_eq!(reader.position(), 4);
}

#[test]
fn oversized_message_is_refused_with_nothing_written() {
    let oversized_message = "x".repeat(MAX_FRAME_BYTE_COUNT as usize);
    let mut frame_bytes: Vec<u8> = Vec::new();

    let write_error = write_message(&mut frame_bytes, &oversized_message).unwrap_err();
    let IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    } = write_error
    else {
        panic!("wrong error: {write_error}");
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

    let write_error =
        write_message(&mut frame_bytes, &[first_message_text, second_message_text]).unwrap_err();
    let IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    } = write_error
    else {
        panic!("wrong error: {write_error}");
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

    let write_error = write_message(&mut frame_bytes, &Unencodable).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = write_error else {
        panic!("wrong error: {write_error}");
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

    let read_error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::Disconnected = read_error else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(reader.position(), 4);
}

#[test]
fn empty_frame_is_a_malformed_message() {
    let mut reader = Cursor::new(vec![0, 0, 0, 0]);
    let read_error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = read_error else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(error_detail, "EOF while parsing a value at line 1 column 0");
    assert_eq!(reader.position(), 4);
}

#[test]
fn non_json_payload_is_malformed_and_the_whole_frame_is_consumed() {
    let mut frame_bytes = 3u32.to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(b"???");
    let mut reader = Cursor::new(frame_bytes);

    let read_error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = read_error else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(error_detail, "expected value at line 1 column 1");
    assert_eq!(reader.position(), 7);
}

#[test]
fn a_well_formed_payload_of_the_wrong_type_is_malformed_and_consumed() {
    let mut frame_bytes = 1u32.to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(b"7");
    let mut reader = Cursor::new(frame_bytes);

    let read_error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = read_error else {
        panic!("wrong error: {read_error}");
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

    let read_error = read_message::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = read_error else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(error_detail, "trailing characters at line 1 column 5");
    assert_eq!(reader.position(), 9);
}

/// The answer a koshi 0.4.0 session gives this build's Hello: the old envelope
/// fields `request_id` and `result`.
const PREVIOUS_RELEASE_ANSWER_JSON: &str = r#"{"request_id":null,"result":{"Error":{"code":"malformed_request","message":"unknown field `request_kind`, expected `request_id` or `kind`"}}}"#;

/// `answer_json` behind its four-byte length prefix.
fn build_frame_bytes(answer_json: &str) -> Vec<u8> {
    let mut frame_bytes = u32::try_from(answer_json.len())
        .expect("a short answer")
        .to_be_bytes()
        .to_vec();
    frame_bytes.extend_from_slice(answer_json.as_bytes());
    frame_bytes
}

#[test]
fn an_answer_in_the_envelope_of_koshi_0_4_0_is_a_previous_release_answer() {
    let parse_error =
        parse_answer::<IpcResult>(PREVIOUS_RELEASE_ANSWER_JSON.as_bytes()).unwrap_err();
    let IpcError::PreviousReleaseAnswer = parse_error else {
        panic!("wrong error: {parse_error}");
    };
}

#[test]
fn an_answer_in_this_builds_envelope_parses_to_the_answer_sent() {
    let hello_answer = IpcResponse {
        request_id: Some(3),
        answer_result: IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: "0.6.0".to_string(),
            process_image_id: ProcessImageId::get_own(),
        },
    };
    let answer_bytes = serde_json::to_vec(&hello_answer).expect("the answer serializes");

    let parsed_answer = parse_answer::<IpcResult>(&answer_bytes).expect("the answer parses");

    assert_eq!(parsed_answer, hello_answer);
}

#[test]
fn an_answer_with_a_third_field_beside_request_id_and_result_is_malformed() {
    let parse_error =
        parse_answer::<IpcResult>(br#"{"request_id":null,"result":{},"error":1}"#).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = parse_error else {
        panic!("wrong error: {parse_error}");
    };
    assert_eq!(
        error_detail,
        "unknown field `result`, expected `request_id` or `answer_result` at line 1 column 27"
    );
}

#[test]
fn an_answer_whose_two_fields_are_not_request_id_and_result_is_malformed() {
    let parse_error = parse_answer::<IpcResult>(br#"{"request_id":1,"answer":{}}"#).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = parse_error else {
        panic!("wrong error: {parse_error}");
    };
    assert_eq!(
        error_detail,
        "unknown field `answer`, expected `request_id` or `answer_result` at line 1 column 24"
    );
}

#[test]
fn an_answer_that_is_not_an_object_is_malformed() {
    let parse_error = parse_answer::<IpcResult>(b"7").unwrap_err();
    let IpcError::MalformedFrame { error_detail } = parse_error else {
        panic!("wrong error: {parse_error}");
    };
    assert_eq!(
        error_detail,
        "invalid type: integer `7`, expected struct Answer at line 1 column 1"
    );
}

#[test]
fn read_answer_reads_a_whole_frame_in_the_envelope_of_koshi_0_4_0_as_a_previous_release_answer() {
    let frame_bytes = build_frame_bytes(PREVIOUS_RELEASE_ANSWER_JSON);
    let frame_byte_count = frame_bytes.len() as u64;
    let mut reader = Cursor::new(frame_bytes);

    let read_error = read_answer::<IpcResult>(&mut reader).unwrap_err();

    let IpcError::PreviousReleaseAnswer = read_error else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(reader.position(), frame_byte_count);
}

#[test]
fn read_message_reads_the_envelope_of_koshi_0_4_0_as_a_malformed_frame() {
    let frame_bytes = build_frame_bytes(PREVIOUS_RELEASE_ANSWER_JSON);
    let frame_byte_count = frame_bytes.len() as u64;
    let mut reader = Cursor::new(frame_bytes);

    let read_error = read_message::<IpcResponse>(&mut reader).unwrap_err();

    let IpcError::MalformedFrame { error_detail } = read_error else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(
        error_detail,
        "unknown field `result`, expected `request_id` or `answer_result` at line 1 column 27"
    );
    assert_eq!(reader.position(), frame_byte_count);
}

#[test]
fn end_of_stream_before_a_header_reads_as_disconnected() {
    let read_error = read_message::<String>(&mut Cursor::new(Vec::<u8>::new())).unwrap_err();
    let IpcError::Disconnected = read_error else {
        panic!("wrong error: {read_error}");
    };
}

#[test]
fn end_of_stream_inside_a_header_reads_as_disconnected() {
    let read_error = read_message::<String>(&mut Cursor::new(vec![0, 0])).unwrap_err();
    let IpcError::Disconnected = read_error else {
        panic!("wrong error: {read_error}");
    };
}

#[test]
fn end_of_stream_inside_a_payload_reads_as_disconnected() {
    let mut frame_bytes = 5u32.to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(b"tr");
    let read_error = read_message::<String>(&mut Cursor::new(frame_bytes)).unwrap_err();
    let IpcError::Disconnected = read_error else {
        panic!("wrong error: {read_error}");
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
        let ipc_error = convert_io_error(io::Error::new(error_kind, "the peer is gone"));
        let IpcError::Disconnected = ipc_error else {
            panic!("{error_kind:?} should read as disconnected, got {ipc_error}");
        };
    }
}

#[test]
fn an_io_kind_that_is_not_the_peer_going_away_keeps_its_own_words() {
    let ipc_error = convert_io_error(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "permission denied",
    ));

    let IpcError::Transport { error_detail } = ipc_error else {
        panic!("wrong error: {ipc_error}");
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

// --- binary frames ---

#[test]
fn a_binary_frame_is_the_length_with_bit_31_set_then_the_bytes() {
    let mut frame_bytes: Vec<u8> = Vec::new();
    write_binary_frame(&mut frame_bytes, &[0x01, 0x02, 0x03], &[]).expect("write");
    assert_eq!(frame_bytes, [0x80, 0x00, 0x00, 0x03, 0x01, 0x02, 0x03]);
}

#[test]
fn json_and_binary_frames_read_back_in_order_from_one_stream() {
    let mut frame_bytes: Vec<u8> = Vec::new();
    write_message(&mut frame_bytes, &"before").expect("write first");
    write_binary_frame(&mut frame_bytes, &[0x00, 0xff], &[]).expect("write second");
    write_binary_frame(&mut frame_bytes, &[], &[]).expect("write third");
    write_message(&mut frame_bytes, &"after").expect("write fourth");

    let mut reader = Cursor::new(frame_bytes);
    assert_eq!(
        read_message_or_binary::<String>(&mut reader).expect("read first"),
        ReceivedFrame::Message("before".to_string())
    );
    assert_eq!(
        read_message_or_binary::<String>(&mut reader).expect("read second"),
        ReceivedFrame::Binary(vec![0x00, 0xff])
    );
    assert_eq!(
        read_message_or_binary::<String>(&mut reader).expect("read third"),
        ReceivedFrame::Binary(Vec::new())
    );
    assert_eq!(
        read_message_or_binary::<String>(&mut reader).expect("read fourth"),
        ReceivedFrame::Message("after".to_string())
    );
}

#[test]
fn a_json_reader_that_meets_a_binary_frame_stops_after_the_header() {
    let mut frame_bytes: Vec<u8> = Vec::new();
    write_binary_frame(&mut frame_bytes, b"\"text\"", &[]).expect("write");

    let mut message_reader = Cursor::new(frame_bytes.clone());
    let read_error = read_message::<String>(&mut message_reader).unwrap_err();
    let IpcError::UnexpectedBinaryFrame = read_error else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(message_reader.position(), 4);

    let mut answer_reader = Cursor::new(frame_bytes);
    let read_error = read_answer::<IpcResult>(&mut answer_reader).unwrap_err();
    let IpcError::UnexpectedBinaryFrame = read_error else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(answer_reader.position(), 4);
}

#[test]
fn a_json_frame_that_does_not_parse_is_malformed_and_the_next_binary_frame_still_reads() {
    let mut frame_bytes = build_frame_bytes("???");
    write_binary_frame(&mut frame_bytes, &[0x07], &[]).expect("write");

    let mut reader = Cursor::new(frame_bytes);
    let read_error = read_message_or_binary::<String>(&mut reader).unwrap_err();
    let IpcError::MalformedFrame { error_detail } = read_error else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(error_detail, "expected value at line 1 column 1");
    assert_eq!(reader.position(), 7);
    assert_eq!(
        read_message_or_binary::<String>(&mut reader).expect("read binary"),
        ReceivedFrame::Binary(vec![0x07])
    );
}

#[test]
fn a_binary_frame_of_exactly_the_limit_reads() {
    let payload_bytes = vec![0x5a; MAX_FRAME_BYTE_COUNT as usize];
    let mut frame_bytes: Vec<u8> = Vec::new();
    write_binary_frame(&mut frame_bytes, &payload_bytes, &[]).expect("write");
    assert_eq!(frame_bytes[..4], [0x81, 0x00, 0x00, 0x00]);

    let received_frame =
        read_message_or_binary::<String>(&mut Cursor::new(frame_bytes)).expect("read");
    assert_eq!(received_frame, ReceivedFrame::Binary(payload_bytes));
}

#[test]
fn a_binary_length_one_past_the_limit_is_refused_after_only_the_header_is_read() {
    let mut frame_bytes = ((MAX_FRAME_BYTE_COUNT + 1) | BINARY_FRAME_FLAG)
        .to_be_bytes()
        .to_vec();
    frame_bytes.extend_from_slice(b"payload that must never be read");
    let mut reader = Cursor::new(frame_bytes);

    let read_error = read_message_or_binary::<String>(&mut reader).unwrap_err();
    let IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    } = read_error
    else {
        panic!("wrong error: {read_error}");
    };
    assert_eq!(frame_byte_count, u64::from(MAX_FRAME_BYTE_COUNT) + 1);
    assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
    assert_eq!(reader.position(), 4);
}

#[test]
fn a_binary_payload_one_past_the_limit_is_refused_with_nothing_written() {
    let payload_bytes = vec![0x5a; MAX_FRAME_BYTE_COUNT as usize + 1];
    let mut frame_bytes: Vec<u8> = Vec::new();

    let write_error = write_binary_frame(&mut frame_bytes, &payload_bytes, &[]).unwrap_err();
    let IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    } = write_error
    else {
        panic!("wrong error: {write_error}");
    };
    assert_eq!(frame_byte_count, u64::from(MAX_FRAME_BYTE_COUNT) + 1);
    assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
    assert_eq!(frame_bytes, Vec::<u8>::new());
}

/// A writer that keeps the bytes of each `write` call apart, in call order.
#[derive(Default)]
struct WriteCallRecorder {
    written_byte_calls: Vec<Vec<u8>>,
}

impl Write for WriteCallRecorder {
    fn write(&mut self, written_bytes: &[u8]) -> io::Result<usize> {
        self.written_byte_calls.push(written_bytes.to_vec());
        Ok(written_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_binary_frame_with_a_head_writes_the_prefix_and_head_then_the_body_and_reads_as_one_payload() {
    let mut write_call_recorder = WriteCallRecorder::default();

    let frame_byte_count =
        write_binary_frame(&mut write_call_recorder, &[0x02, 0x07], &[0x0a, 0x0b, 0x0c])
            .expect("write");

    assert_eq!(frame_byte_count, 9);
    assert_eq!(
        write_call_recorder.written_byte_calls,
        [
            vec![0x80, 0x00, 0x00, 0x05, 0x02, 0x07],
            vec![0x0a, 0x0b, 0x0c]
        ]
    );
    let frame_bytes = write_call_recorder.written_byte_calls.concat();
    assert_eq!(
        read_message_or_binary::<String>(&mut Cursor::new(frame_bytes)).expect("read"),
        ReceivedFrame::Binary(vec![0x02, 0x07, 0x0a, 0x0b, 0x0c])
    );
}

#[test]
fn a_head_and_body_one_past_the_limit_together_are_refused_with_nothing_written() {
    let frame_body = vec![0x5a; MAX_FRAME_BYTE_COUNT as usize - 1];
    let mut write_call_recorder = WriteCallRecorder::default();

    let write_error =
        write_binary_frame(&mut write_call_recorder, &[0x02, 0x07], &frame_body).unwrap_err();

    let IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    } = write_error
    else {
        panic!("wrong error: {write_error}");
    };
    assert_eq!(frame_byte_count, u64::from(MAX_FRAME_BYTE_COUNT) + 1);
    assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
    assert_eq!(
        write_call_recorder.written_byte_calls,
        Vec::<Vec<u8>>::new()
    );
}

// --- the frame shape on a stream that is not a local socket ---

/// An in-memory stream half that records the last deadline and the last idle
/// timeout it was given.
struct RecordedStream {
    read_cursor: Cursor<Vec<u8>>,
    written_bytes: Arc<Mutex<Vec<u8>>>,
    deadline: Arc<Mutex<Option<Instant>>>,
    idle_timeout: Arc<Mutex<Option<Duration>>>,
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

    fn set_idle_timeout(&mut self, idle_timeout: Option<Duration>) {
        *self.idle_timeout.lock().expect("idle timeout") = idle_timeout;
    }
}

#[test]
fn a_frame_writer_counts_every_byte_of_each_whole_frame_it_wrote() {
    let written_bytes = Arc::new(Mutex::new(Vec::new()));
    let (_reader, mut writer) = build_frame_halves(
        Box::new(RecordedStream {
            read_cursor: Cursor::new(Vec::new()),
            written_bytes: Arc::new(Mutex::new(Vec::new())),
            deadline: Arc::new(Mutex::new(None)),
            idle_timeout: Arc::new(Mutex::new(None)),
        }),
        Box::new(RecordedStream {
            read_cursor: Cursor::new(Vec::new()),
            written_bytes: Arc::clone(&written_bytes),
            deadline: Arc::new(Mutex::new(None)),
            idle_timeout: Arc::new(Mutex::new(None)),
        }),
    );
    assert_eq!(writer.get_written_byte_count(), 0);

    writer.send(&"hi").expect("send the JSON frame");
    writer
        .send_binary(&[1, 2, 3])
        .expect("send the binary frame");
    writer
        .send_binary_with_head(&[4, 5], &[6])
        .expect("send the binary frame with a head");
    let oversized_payload = vec![0; MAX_FRAME_BYTE_COUNT as usize + 1];
    writer
        .send_binary(&oversized_payload)
        .expect_err("a payload past the cap is refused");
    writer
        .send_binary_with_head(&[4], &oversized_payload[1..])
        .expect_err("a head and body past the cap are refused");

    assert_eq!(writer.get_written_byte_count(), 22);
    assert_eq!(written_bytes.lock().expect("written_bytes").len(), 22);
}

#[test]
fn frame_halves_speak_the_frame_shape_and_hand_each_limit_to_its_half() {
    let mut framed: Vec<u8> = Vec::new();
    write_message(&mut framed, &build_hello_request(5)).expect("frame");
    let written_bytes = Arc::new(Mutex::new(Vec::new()));
    let read_deadline = Arc::new(Mutex::new(None));
    let write_deadline = Arc::new(Mutex::new(None));
    let read_idle_timeout = Arc::new(Mutex::new(None));
    let write_idle_timeout = Arc::new(Mutex::new(None));

    let (mut reader, mut writer) = build_frame_halves(
        Box::new(RecordedStream {
            read_cursor: Cursor::new(framed),
            written_bytes: Arc::clone(&written_bytes),
            deadline: Arc::clone(&read_deadline),
            idle_timeout: Arc::clone(&read_idle_timeout),
        }),
        Box::new(RecordedStream {
            read_cursor: Cursor::new(Vec::new()),
            written_bytes: Arc::clone(&written_bytes),
            deadline: Arc::clone(&write_deadline),
            idle_timeout: Arc::clone(&write_idle_timeout),
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

    reader.set_idle_timeout(Some(Duration::from_secs(30)));
    assert_eq!(
        *read_idle_timeout.lock().expect("idle timeout"),
        Some(Duration::from_secs(30))
    );
    assert_eq!(*write_idle_timeout.lock().expect("idle timeout"), None);
    writer.set_idle_timeout(Some(Duration::from_secs(20)));
    assert_eq!(
        *write_idle_timeout.lock().expect("idle timeout"),
        Some(Duration::from_secs(20))
    );
    reader.set_idle_timeout(None);
    assert_eq!(*read_idle_timeout.lock().expect("idle timeout"), None);
}

#[test]
fn a_step_ends_by_the_earlier_of_the_deadline_and_the_idle_timeout() {
    let idle_timeout = Duration::from_secs(30);
    let near_deadline = Instant::now() + Duration::from_secs(10);
    assert_eq!(
        compute_step_deadline(Some(near_deadline), Some(idle_timeout)),
        Some(near_deadline)
    );

    let far_deadline = Instant::now() + Duration::from_secs(60);
    let computed_before = Instant::now();
    let step_deadline = compute_step_deadline(Some(far_deadline), Some(idle_timeout))
        .expect("an idle timeout limits the step");
    let computed_after = Instant::now();
    assert!(
        step_deadline >= computed_before + idle_timeout
            && step_deadline <= computed_after + idle_timeout,
        "the step ends 30 seconds after it starts"
    );
}

#[test]
fn a_step_under_an_idle_timeout_alone_ends_that_long_after_it_starts() {
    let idle_timeout = Duration::from_secs(30);

    let computed_before = Instant::now();
    let step_deadline =
        compute_step_deadline(None, Some(idle_timeout)).expect("an idle timeout limits the step");
    let computed_after = Instant::now();

    assert!(
        step_deadline >= computed_before + idle_timeout
            && step_deadline <= computed_after + idle_timeout,
        "the step ends 30 seconds after it starts"
    );
}

#[test]
fn an_idle_timeout_absent_or_too_large_to_add_leaves_the_step_to_the_deadline() {
    let deadline = Instant::now() + Duration::from_secs(10);

    assert_eq!(compute_step_deadline(None, None), None);
    assert_eq!(compute_step_deadline(Some(deadline), None), Some(deadline));
    assert_eq!(compute_step_deadline(None, Some(Duration::MAX)), None);
    assert_eq!(
        compute_step_deadline(Some(deadline), Some(Duration::MAX)),
        Some(deadline)
    );
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
fn a_windows_address_maps_to_its_pipe_path() {
    assert_eq!(
        format_pipe_path("koshi-test")
            .expect("pipe path")
            .to_string_lossy(),
        r"\\.\pipe\koshi-test"
    );
}

#[cfg(windows)]
#[test]
fn a_windows_address_holding_a_nul_character_is_refused() {
    let path_error = format_pipe_path("koshi\0test").expect_err("NUL refused");
    assert_eq!(path_error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        path_error.to_string(),
        "the pipe name holds a NUL character"
    );
}

/// The pipe a listener binds is owned by this process's user, and its access
/// list grants every right to that user and nothing to anyone else.
///
/// The pipe's descriptor and the expected descriptor both convert to the
/// security descriptor string format before the compare. That format writes a
/// well-known account by its alias: the built-in Administrator is `LA`, and
/// `LocalSystem` is `SY`.
#[cfg(windows)]
#[test]
fn a_bound_pipe_grants_access_to_this_user_alone() {
    use interprocess::os::windows::security_descriptor::{
        AsSecurityDescriptorExt, BorrowedSecurityDescriptor,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    let socket_address = build_test_socket_address("private-access");
    let listener = Listener::bind(&socket_address).expect("bind");
    let pipe_handle =
        open_pipe_caller_end(&socket_address, CONNECT_WAIT_DURATION).expect("open the pipe");
    let mut pipe_security_descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `pipe_handle` is open, and each out pointer is a writable local
    // or NULL.
    let lookup_status = unsafe {
        GetSecurityInfo(
            pipe_handle.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut pipe_security_descriptor,
        )
    };
    assert_eq!(lookup_status, ERROR_SUCCESS);
    let _pipe_security_descriptor_allocation = LocalAllocation(pipe_security_descriptor);
    let access_selector = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    // SAFETY: `GetSecurityInfo` succeeded, so `pipe_security_descriptor` is a
    // valid descriptor that `_pipe_security_descriptor_allocation` keeps alive.
    let pipe_access_text =
        unsafe { BorrowedSecurityDescriptor::from_ptr(pipe_security_descriptor) }
            .serialize(access_selector, U16CStr::to_string_lossy)
            .expect("the pipe's security descriptor converts to text");
    let own_user_sid = format_own_user_sid().expect("read this process's user");
    let expected_access_source =
        U16CString::from_str(format!("O:{own_user_sid}D:(A;;FA;;;{own_user_sid})"))
            .expect("a SID holds no NUL character");
    let expected_access_text = SecurityDescriptor::deserialize(&expected_access_source)
        .expect("the expected security descriptor parses")
        .serialize(access_selector, U16CStr::to_string_lossy)
        .expect("the expected security descriptor converts to text");

    assert_eq!(pipe_access_text, expected_access_text);
    drop(pipe_handle);
    drop(listener);
}

/// The raw handle inside `socket_stream`, a named pipe stream.
#[cfg(windows)]
fn get_pipe_stream_raw_handle(socket_stream: &socket::Stream) -> HANDLE {
    let socket::Stream::NamedPipe(pipe_stream) = socket_stream;
    pipe_stream.inner().as_raw_handle()
}

/// Wrapping a caller's end keeps the handle it was given while the listener
/// holds a free instance for the next caller.
#[cfg(windows)]
#[test]
fn wrapping_a_caller_end_keeps_its_handle_while_a_free_instance_waits() {
    let socket_address = build_test_socket_address("wrap-caller-end");
    let listener = Listener::bind(&socket_address).expect("bind");
    let accepting_thread = thread::spawn(move || {
        let accepted_peer = listener.accept().expect("accept");
        (accepted_peer, listener)
    });
    let caller_end =
        open_pipe_caller_end(&socket_address, CONNECT_WAIT_DURATION).expect("open the pipe");
    let (accepted_peer, listener) = accepting_thread.join().expect("accepting thread");
    let caller_end_handle = caller_end.as_raw_handle();

    let caller_stream = wrap_pipe_handle(caller_end).expect("wrap the caller end");

    assert_eq!(
        get_pipe_stream_raw_handle(&caller_stream),
        caller_end_handle
    );
    drop(caller_stream);
    drop(accepted_peer);
    drop(listener);
}

/// Wrapping a pipe instance keeps the handle it was given while another
/// instance of the pipe is free.
#[cfg(windows)]
#[test]
fn wrapping_a_pipe_instance_keeps_its_handle_while_another_instance_is_free() {
    let socket_address = build_test_socket_address("wrap-instance");
    let pipe_path = format_pipe_path(&socket_address).expect("pipe path");
    let access = build_pipe_access().expect("build the pipe access");
    let pipe_instance = create_pipe_instance(&pipe_path, &access, true).expect("first instance");
    let free_instance = create_pipe_instance(&pipe_path, &access, false).expect("free instance");
    let pipe_instance_handle = pipe_instance.as_raw_handle();

    let instance_stream = wrap_pipe_handle(pipe_instance).expect("wrap the instance");

    assert_eq!(
        get_pipe_stream_raw_handle(&instance_stream),
        pipe_instance_handle
    );
    drop(instance_stream);
    drop(free_instance);
}

// --- end to end over a real socket ---

#[test]
fn request_and_response_cross_a_real_socket() {
    let socket_address = build_test_socket_address("roundtrip");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let request: IpcRequest = connection.recv().expect("server recv");
        connection
            .send(&build_hello_answer(request.request_id))
            .expect("server send");
        request
    });

    let mut connection = Connection::connect(&socket_address).expect("connect");
    let hello_request_message = build_hello_request(7);
    connection
        .send(&hello_request_message)
        .expect("client send");
    let response: IpcResponse = connection.recv().expect("client recv");

    assert_eq!(response, build_hello_answer(7));
    assert_eq!(
        server_thread.join().expect("server thread"),
        hello_request_message
    );
}

/// Connect to `listener` at `socket_address` through
/// [`Connection::connect_to_own_server_within`], send a Hello numbered 9, and
/// assert the listener read that Hello and the caller read its answer,
/// `Restarting` numbered 9.
fn assert_own_server_connect_round_trip(listener: Listener, socket_address: &str) {
    let server_thread = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let request: IpcRequest = connection.recv().expect("server recv");
        connection
            .send(&IpcResponse {
                request_id: Some(request.request_id),
                answer_result: IpcResult::Restarting,
            })
            .expect("server send");
        request
    });
    let mut connection =
        Connection::connect_to_own_server_within(socket_address, CONNECT_WAIT_DURATION)
            .expect("own server connect");
    let hello_request_message = build_hello_request(9);
    connection
        .send(&hello_request_message)
        .expect("client send");
    let response: IpcResponse = connection.recv().expect("client recv");
    assert_eq!(
        response,
        IpcResponse {
            request_id: Some(9),
            answer_result: IpcResult::Restarting,
        }
    );
    assert_eq!(
        server_thread.join().expect("server thread"),
        hello_request_message
    );
}

#[test]
fn an_own_server_connect_keeps_a_listener_this_user_binds() {
    let socket_address = build_test_socket_address("own-private");
    let listener = Listener::bind(&socket_address).expect("bind");
    assert_own_server_connect_round_trip(listener, &socket_address);
}

#[test]
fn an_own_server_connect_to_an_address_nothing_listens_at_is_no_listener() {
    let socket_address = build_test_socket_address("own-nothing");
    let connect_error =
        Connection::connect_to_own_server_within(&socket_address, CONNECT_WAIT_DURATION)
            .expect_err("nothing listens");
    let IpcError::NoListener {
        socket_address: reported_socket_address,
    } = connect_error
    else {
        panic!("expected NoListener, got {connect_error:?}");
    };
    assert_eq!(reported_socket_address, socket_address);
}

#[test]
fn a_write_the_peer_takes_no_byte_of_fails_once_its_idle_timeout_passes() {
    let socket_address = build_test_socket_address("write-idle-timeout");
    let listener = Listener::bind(&socket_address).expect("bind");
    let (test_done_sender, test_done_receiver) = std::sync::mpsc::channel::<()>();
    let server_thread = thread::spawn(move || {
        let _connection = listener.accept().expect("accept");
        let _ = test_done_receiver.recv();
    });
    let (_reader, mut writer) = Connection::connect(&socket_address)
        .expect("connect")
        .split();
    let idle_timeout = Duration::from_millis(200);
    writer.set_idle_timeout(Some(idle_timeout));

    let send_started_at = Instant::now();
    let send_result = writer.send(&"x".repeat(4 * 1024 * 1024));

    assert!(send_started_at.elapsed() >= idle_timeout - MINIMUM_STEP_REMAINING_DURATION);
    let Err(IpcError::Transport { error_detail }) = &send_result else {
        panic!("a write the peer takes no byte of times out, got {send_result:?}");
    };
    assert_eq!(error_detail, "this step ran out of time");
    test_done_sender.send(()).expect("the server waits");
    server_thread.join().expect("server thread");
}

/// On Windows the second caller is served only if the listener created the
/// pipe instance behind it.
#[test]
fn a_bind_serves_one_caller_after_another() {
    let socket_address = build_test_socket_address("one-after-another");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
        for _ in 0..2 {
            let mut connection = listener.accept().expect("accept");
            let request: IpcRequest = connection.recv().expect("server recv");
            connection
                .send(&build_hello_answer(request.request_id))
                .expect("server send");
        }
    });

    for request_id in [1, 2] {
        let mut connection = Connection::connect(&socket_address).expect("connect");
        connection
            .send(&build_hello_request(request_id))
            .expect("client send");
        let response: IpcResponse = connection.recv().expect("client recv");
        assert_eq!(response, build_hello_answer(request_id));
    }
    server_thread.join().expect("server thread");
}

#[test]
fn a_caller_from_this_same_process_is_reported_as_the_same_user() {
    let socket_address = build_test_socket_address("peeruser");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
        let connection = listener.accept().expect("accept");
        connection.is_peer_same_user().expect("peer creds")
    });

    // The caller stays connected until the server has read its credentials.
    let caller = Connection::connect(&socket_address).expect("connect");
    assert!(server_thread.join().expect("server thread"));
    drop(caller);
}

#[test]
fn hello_and_request_sent_back_to_back_arrive_as_two_messages() {
    let socket_address = build_test_socket_address("backtoback");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
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

    assert_eq!(
        server_thread.join().expect("server thread"),
        (hello, request)
    );
}

#[test]
fn split_halves_carry_binary_frames_between_json_frames() {
    let socket_address = build_test_socket_address("binary");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
        let (_reader, mut writer) = listener.accept().expect("accept").split();
        writer.send(&"first").expect("send first");
        writer.send_binary(&[0x01, 0x02]).expect("send second");
        writer.send(&"third").expect("send third");
    });

    let (mut reader, _writer) = Connection::connect(&socket_address)
        .expect("connect")
        .split();
    let received_frames: Vec<ReceivedFrame<String>> = (0..3)
        .map(|_| reader.recv_message_or_binary().expect("recv"))
        .collect();
    server_thread.join().expect("server thread");
    assert_eq!(
        received_frames,
        vec![
            ReceivedFrame::Message("first".to_string()),
            ReceivedFrame::Binary(vec![0x01, 0x02]),
            ReceivedFrame::Message("third".to_string()),
        ]
    );
}

#[test]
fn split_halves_carry_frames_both_ways_from_two_threads() {
    let socket_address = build_test_socket_address("split");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
        let (mut reader, mut writer) = listener.accept().expect("accept").split();
        // The reading half moves to its own thread and blocks there while the
        // writing half sends on this one.
        let reading_thread = thread::spawn(move || {
            let first_received_request: IpcRequest = reader.recv().expect("server recv first");
            let second_received_request: IpcRequest = reader.recv().expect("server recv second");
            vec![
                first_received_request.request_id,
                second_received_request.request_id,
            ]
        });
        for request_id in [11, 12] {
            writer
                .send(&build_hello_answer(request_id))
                .expect("server send");
        }
        reading_thread.join().expect("server reading thread")
    });

    let (mut reader, mut writer) = Connection::connect(&socket_address)
        .expect("connect")
        .split();
    let reading_thread = thread::spawn(move || {
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
        reading_thread.join().expect("client reading thread"),
        vec![Some(11), Some(12)]
    );
    assert_eq!(server_thread.join().expect("server thread"), vec![21, 22]);
}

/// A caller connected to a fresh listener at `test_case_tag`'s address, and
/// the listener's end of that connection, accepted on another thread.
fn connect_caller_and_peer(test_case_tag: &str) -> (Connection, Connection) {
    let socket_address = build_test_socket_address(test_case_tag);
    let listener = Listener::bind(&socket_address).expect("bind");
    let accepting_thread = thread::spawn(move || listener.accept().expect("accept"));
    let caller = Connection::connect(&socket_address).expect("connect");
    (caller, accepting_thread.join().expect("accepting thread"))
}

/// The detail of `ipc_error`, which must be [`IpcError::Transport`].
fn read_transport_error_detail(ipc_error: IpcError) -> String {
    let IpcError::Transport { error_detail } = ipc_error else {
        panic!("not a transport error: {ipc_error}");
    };
    error_detail
}

#[test]
fn a_read_under_a_deadline_ends_at_the_deadline_while_the_peer_stays_silent() {
    let (caller, silent_peer) = connect_caller_and_peer("read-deadline");
    let (mut reader, _writer) = caller.split();
    let read_started_at = Instant::now();
    reader.set_deadline(Some(read_started_at + Duration::from_millis(200)));

    let receive_error = reader.recv::<IpcResponse>().unwrap_err();

    let read_duration = read_started_at.elapsed();
    assert_eq!(
        read_transport_error_detail(receive_error),
        "this step ran out of time"
    );
    assert!(
        read_duration >= Duration::from_millis(199),
        "the read runs to the deadline, it took {read_duration:?}"
    );
    assert!(
        read_duration < Duration::from_secs(2),
        "the read ends at the deadline, it took {read_duration:?}"
    );
    drop(silent_peer);
}

#[test]
fn a_write_under_a_deadline_ends_at_the_deadline_while_the_peer_reads_nothing() {
    // 8 MiB is more than the socket or the pipe holds unread.
    let (caller, unread_peer) = connect_caller_and_peer("write-deadline");
    let (_reader, mut writer) = caller.split();
    let write_started_at = Instant::now();
    writer.set_deadline(Some(write_started_at + Duration::from_millis(300)));

    let send_error = writer.send(&"x".repeat(8 * 1024 * 1024)).unwrap_err();

    let write_duration = write_started_at.elapsed();
    assert_eq!(
        read_transport_error_detail(send_error),
        "this step ran out of time"
    );
    assert!(
        write_duration >= Duration::from_millis(299),
        "the write runs to the deadline, it took {write_duration:?}"
    );
    assert!(
        write_duration < Duration::from_secs(2),
        "the write ends at the deadline, it took {write_duration:?}"
    );
    drop(unread_peer);
}

#[test]
fn a_frame_the_peer_sent_before_hanging_up_is_read_under_a_deadline() {
    let (caller, mut answering_peer) = connect_caller_and_peer("answer-then-hang-up");
    let sent_response = build_hello_answer(7);
    answering_peer.send(&sent_response).expect("peer send");
    drop(answering_peer);
    let (mut reader, _writer) = caller.split();
    reader.set_deadline(Some(Instant::now() + Duration::from_secs(2)));

    let received_response: IpcResponse = reader.recv().expect("the sent frame is read");

    assert_eq!(received_response, sent_response);
    let IpcError::Disconnected = reader.recv::<IpcResponse>().unwrap_err() else {
        panic!("the read after the frame is the end of the stream");
    };
}

#[test]
fn a_peer_that_never_reads_holds_back_no_other_connections_close() {
    for frame_write_shape in [
        FrameWriteShape::WholeConnection,
        FrameWriteShape::SplitAfterTheWrite,
        FrameWriteShape::WritingHalfUnderADeadline,
    ] {
        // The first connection's listener end writes a frame its caller never
        // reads, and is dropped while that caller stays open.
        let (unread_caller, unread_peer) =
            connect_caller_and_peer(&format!("close-unread-{frame_write_shape:?}"));
        write_hello_request_and_drop(unread_peer, 1, frame_write_shape);
        // The second connection's caller writes a frame and is dropped. The
        // listener end then reads that frame, then the end of the stream.
        let (closing_caller, closing_peer) =
            connect_caller_and_peer(&format!("close-read-{frame_write_shape:?}"));
        write_hello_request_and_drop(closing_caller, 2, frame_write_shape);
        let (mut reader, _writer) = closing_peer.split();
        reader.set_deadline(Some(Instant::now() + Duration::from_secs(5)));
        assert_eq!(
            reader
                .recv::<IpcRequest>()
                .expect("the frame written before the drop is read"),
            build_hello_request(2)
        );

        let receive_error = reader.recv::<IpcRequest>().unwrap_err();

        let IpcError::Disconnected = receive_error else {
            panic!(
                "the read after the caller is dropped is the end of the stream: {receive_error}"
            );
        };
        drop(unread_caller);
    }
}

#[test]
fn an_address_binds_again_as_soon_as_its_listener_and_written_connection_are_dropped() {
    // A caller connects and the listener accepts it. Each end writes a frame
    // the other never reads. Both connection ends and the listener are
    // dropped, and the same address binds again at once.
    let socket_address = build_test_socket_address("rebind-written");
    let listener = Listener::bind(&socket_address).expect("bind");
    let accepting_thread = thread::spawn(move || {
        let accepted_peer = listener.accept().expect("accept");
        (accepted_peer, listener)
    });
    let mut caller = Connection::connect(&socket_address).expect("connect");
    let (mut accepted_peer, listener) = accepting_thread.join().expect("accepting thread");
    caller.send(&build_hello_request(1)).expect("caller send");
    accepted_peer
        .send(&build_hello_request(2))
        .expect("accepted peer send");
    drop(caller);
    drop(accepted_peer);
    drop(listener);

    let rebind_outcome = Listener::bind(&socket_address);

    rebind_outcome.expect("the address binds again");
}

#[test]
fn a_caller_that_hangs_up_before_accept_with_nothing_written_reads_as_the_end_of_the_stream() {
    // The caller connects and is dropped before the listener accepts, with
    // nothing written. The listener accepts that connection, and its first
    // read is the end of the stream.
    let socket_address = build_test_socket_address("hangup-unwritten-before-accept");
    let listener = Listener::bind(&socket_address).expect("bind");
    drop(Connection::connect(&socket_address).expect("connect"));
    let (accepted_sender, accepted_receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = accepted_sender.send(listener.accept());
    });

    let mut accepted_peer = accepted_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the listener accepts the connection the caller hung up")
        .expect("accept");

    let receive_error = accepted_peer.recv::<IpcRequest>().unwrap_err();
    let IpcError::Disconnected = receive_error else {
        panic!("the first read is the end of the stream: {receive_error}");
    };
}

#[test]
fn a_flush_returns_at_once_while_the_peer_reads_nothing() {
    // The caller writes a byte its peer never reads, then flushes.
    let (caller, unread_peer) = connect_caller_and_peer("flush-unread");
    let (_reader, mut raw_writer) = caller.split_raw();
    let (flushed_sender, flushed_receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        raw_writer.write_all(b"x").expect("write one byte");
        let flush_outcome = raw_writer
            .flush()
            .map_err(|flush_error| flush_error.to_string());
        let _ = flushed_sender.send(flush_outcome);
    });

    let flush_outcome = flushed_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the flush returns while the peer reads nothing");

    assert_eq!(flush_outcome, Ok(()));
    drop(unread_peer);
}

#[test]
fn a_frame_a_caller_sent_before_hanging_up_reaches_a_listener_that_accepts_afterwards() {
    // The caller connects, writes a frame and is dropped before the listener
    // accepts. The listener then accepts that connection and reads the frame,
    // then the end of the stream.
    let socket_address = build_test_socket_address("hangup-before-accept");
    let listener = Listener::bind(&socket_address).expect("bind");
    let mut caller = Connection::connect(&socket_address).expect("connect");
    caller.send(&build_hello_request(1)).expect("caller send");
    drop(caller);
    let (accepted_sender, accepted_receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = accepted_sender.send(listener.accept());
    });

    let accepted_peer = accepted_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the listener accepts the connection the caller hung up")
        .expect("accept");

    let (mut reader, _writer) = accepted_peer.split();
    reader.set_deadline(Some(Instant::now() + Duration::from_secs(5)));
    assert_eq!(
        reader
            .recv::<IpcRequest>()
            .expect("the frame written before the hang-up is read"),
        build_hello_request(1)
    );
    let receive_error = reader.recv::<IpcRequest>().unwrap_err();
    let IpcError::Disconnected = receive_error else {
        panic!("the read after the frame is the end of the stream: {receive_error}");
    };
}

/// How [`write_hello_request_and_drop`] writes its frame.
#[derive(Clone, Copy, Debug)]
enum FrameWriteShape {
    /// The whole connection writes the frame.
    WholeConnection,
    /// The whole connection writes the frame, then is split into its two
    /// halves.
    SplitAfterTheWrite,
    /// The connection is split into its two halves, and the writing half
    /// writes the frame under a 5 s deadline.
    WritingHalfUnderADeadline,
}

/// Write the hello request numbered `request_id` through `connection` as
/// `frame_write_shape` says, then drop every part of the connection.
fn write_hello_request_and_drop(
    mut connection: Connection,
    request_id: u64,
    frame_write_shape: FrameWriteShape,
) {
    match frame_write_shape {
        FrameWriteShape::WholeConnection => {
            connection
                .send(&build_hello_request(request_id))
                .expect("send through the whole connection");
        }
        FrameWriteShape::SplitAfterTheWrite => {
            connection
                .send(&build_hello_request(request_id))
                .expect("send through the whole connection");
            drop(connection.split());
        }
        FrameWriteShape::WritingHalfUnderADeadline => {
            let (_reader, mut writer) = connection.split();
            writer.set_deadline(Some(Instant::now() + Duration::from_secs(5)));
            writer
                .send(&build_hello_request(request_id))
                .expect("send through the writing half");
        }
    }
}

#[test]
fn a_read_after_the_deadline_is_cleared_waits_past_the_old_deadline() {
    // The first frame arrives under a 300 ms deadline. The second arrives
    // 600 ms after the deadline was given, once it is cleared.
    let (caller, mut answering_peer) = connect_caller_and_peer("cleared-deadline");
    let (mut reader, _writer) = caller.split();
    let deadline_given_at = Instant::now();
    reader.set_deadline(Some(deadline_given_at + Duration::from_millis(300)));
    let answering_thread = thread::spawn(move || {
        for request_id in [1, 2] {
            answering_peer
                .send(&build_hello_request(request_id))
                .expect("peer send");
            thread::sleep(Duration::from_millis(600));
        }
        answering_peer
    });

    let first_request: IpcRequest = reader.recv().expect("the first frame is read");
    reader.set_deadline(None);
    let second_request: IpcRequest = reader.recv().expect("the second frame is read");

    assert_eq!(
        (first_request.request_id, second_request.request_id),
        (1, 2)
    );
    assert!(deadline_given_at.elapsed() >= Duration::from_millis(600));
    drop(answering_thread.join().expect("answering thread"));
}

#[test]
fn a_read_under_an_idle_timeout_ends_once_the_peer_stays_silent_that_long() {
    let (caller, silent_peer) = connect_caller_and_peer("read-idle-timeout");
    let (mut reader, _writer) = caller.split();
    reader.set_idle_timeout(Some(Duration::from_millis(200)));
    let read_started_at = Instant::now();

    let receive_error = reader.recv::<IpcResponse>().unwrap_err();

    let read_duration = read_started_at.elapsed();
    assert_eq!(
        read_transport_error_detail(receive_error),
        "this step ran out of time"
    );
    assert!(
        read_duration >= Duration::from_millis(199),
        "the read runs to its idle timeout, it took {read_duration:?}"
    );
    assert!(
        read_duration < Duration::from_secs(2),
        "the read ends at its idle timeout, it took {read_duration:?}"
    );
    drop(silent_peer);
}

#[test]
fn a_frame_whose_parts_arrive_within_the_idle_timeout_of_each_other_is_read_whole() {
    // At least twelve parts 100 ms apart: the last part leaves at least 1.1
    // seconds after the read starts, later than the 1-second idle timeout, and
    // no part leaves more than 100 ms after the one before it.
    let (caller, sending_peer) = connect_caller_and_peer("read-idle-parts");
    let (mut reader, _writer) = caller.split();
    reader.set_idle_timeout(Some(Duration::from_secs(1)));
    let mut framed_bytes: Vec<u8> = Vec::new();
    write_message(&mut framed_bytes, &build_hello_request(5)).expect("frame");
    let part_byte_count = framed_bytes.len() / 12;
    let (_peer_reader, mut peer_writer) = sending_peer.split_raw();
    let read_started_at = Instant::now();
    let sending_thread = thread::spawn(move || {
        for (part_index, frame_part) in framed_bytes.chunks(part_byte_count).enumerate() {
            if part_index > 0 {
                thread::sleep(Duration::from_millis(100));
            }
            peer_writer.write_all(frame_part).expect("peer write");
        }
        peer_writer
    });

    let received_request: IpcRequest = reader.recv().expect("the frame is read whole");

    assert_eq!(received_request, build_hello_request(5));
    assert!(
        read_started_at.elapsed() > Duration::from_secs(1),
        "the frame took longer than one idle timeout to arrive"
    );
    drop(sending_thread.join().expect("sending thread"));
}

#[test]
fn a_read_after_the_idle_timeout_is_taken_away_waits_past_it() {
    // The first frame waits on the socket before the read under the 300 ms
    // idle timeout starts. The second leaves 600 ms later, once the idle
    // timeout is taken away.
    let (caller, mut answering_peer) = connect_caller_and_peer("cleared-idle-timeout");
    let (mut reader, _writer) = caller.split();
    reader.set_idle_timeout(Some(Duration::from_millis(300)));
    answering_peer
        .send(&build_hello_request(1))
        .expect("peer send");
    let answering_thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(600));
        answering_peer
            .send(&build_hello_request(2))
            .expect("peer send");
        answering_peer
    });

    let first_request: IpcRequest = reader.recv().expect("the first frame is read");
    reader.set_idle_timeout(None);
    let second_request: IpcRequest = reader.recv().expect("the second frame is read");

    assert_eq!(
        (first_request.request_id, second_request.request_id),
        (1, 2)
    );
    drop(answering_thread.join().expect("answering thread"));
}

#[test]
fn one_listener_serves_two_callers_in_turn() {
    let socket_address = build_test_socket_address("twocallers");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
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

    assert_eq!(server_thread.join().expect("server thread"), vec![10, 20]);
}

#[test]
fn accept_until_shutdown_serves_each_caller_and_drops_the_wake_up_connection() {
    let socket_address = build_test_socket_address("acceptloop");
    let listener = Listener::bind(&socket_address).expect("bind");
    let is_shutting_down = Arc::new(AtomicBool::new(false));
    let (served_sender, served_receiver) = std::sync::mpsc::channel();

    let server_thread = {
        let is_shutting_down = Arc::clone(&is_shutting_down);
        thread::spawn(move || {
            let mut served_connection_count = 0;
            accept_until_shutdown(
                &listener,
                &is_shutting_down,
                std::time::Duration::from_millis(1),
                |accept_error| panic!("no accept fails: {accept_error}"),
                |connection| {
                    served_connection_count += 1;
                    served_sender
                        .send(())
                        .expect("report the served connection");
                    drop(connection);
                },
            );
            served_connection_count
        })
    };

    for _ in 0..2 {
        let _caller = Connection::connect(&socket_address).expect("connect");
        served_receiver.recv().expect("the connection was served");
    }
    is_shutting_down.store(true, Ordering::SeqCst);
    let _wake_up = Connection::connect(&socket_address).expect("connect to wake the loop");

    assert_eq!(server_thread.join().expect("server thread"), 2);
}

#[test]
fn a_read_after_the_peer_hangs_up_reports_disconnected() {
    let socket_address = build_test_socket_address("peergone");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
        drop(listener.accept().expect("accept"));
    });

    let mut caller = Connection::connect(&socket_address).expect("connect");
    server_thread.join().expect("server thread");

    let receive_error = caller.recv::<IpcResponse>().unwrap_err();
    let IpcError::Disconnected = receive_error else {
        panic!("wrong error: {receive_error}");
    };
}

#[cfg(unix)]
#[test]
fn binding_an_address_a_listener_already_holds_is_refused() {
    let socket_address = build_test_socket_address("bindtwice");
    let _first = Listener::bind(&socket_address).expect("bind");

    let bind_error = Listener::bind(&socket_address).unwrap_err();
    let IpcError::Transport { error_detail } = bind_error else {
        panic!("wrong error: {bind_error}");
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

    let bind_error = Listener::bind(&socket_address).unwrap_err();
    let IpcError::Transport { error_detail } = bind_error else {
        panic!("wrong error: {bind_error}");
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

    let (sent_sender, sent_receiver) = std::sync::mpsc::channel();
    let server_thread = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let read_closer = connection
            .create_read_closer()
            .expect("take the read closer");
        sent_receiver.recv().expect("the caller sent its frame");
        read_closer.close();
        // The caller's frame is on the socket by now, and the read still ends.
        connection.recv::<IpcRequest>()
    });

    let mut caller = Connection::connect(&socket_address).expect("connect");
    caller.send(&build_hello_request(1)).expect("client send");
    sent_sender.send(()).expect("the closer is told");

    let server_error = server_thread.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = server_error else {
        panic!("wrong error: {server_error}");
    };
}

#[test]
fn a_second_read_closer_closes_the_same_read_direction() {
    let socket_address = build_test_socket_address("readclose-second");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let _first_read_closer = connection
            .create_read_closer()
            .expect("take the first closer");
        let second_read_closer = connection
            .create_read_closer()
            .expect("take the second closer");
        second_read_closer.close();
        connection.recv::<IpcRequest>()
    });

    let _caller = Connection::connect(&socket_address).expect("connect");

    let server_error = server_thread.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = server_error else {
        panic!("wrong error: {server_error}");
    };
}

#[test]
fn closing_the_read_direction_twice_changes_nothing() {
    let socket_address = build_test_socket_address("readclose-twice");
    let listener = Listener::bind(&socket_address).expect("bind");

    let server_thread = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        let read_closer = connection
            .create_read_closer()
            .expect("take the read closer");
        read_closer.close();
        read_closer.close();
        connection.recv::<IpcRequest>()
    });

    let _caller = Connection::connect(&socket_address).expect("connect");

    let server_error = server_thread.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = server_error else {
        panic!("wrong error: {server_error}");
    };
}

#[test]
fn a_read_closer_taken_before_the_split_closes_the_reading_half() {
    let socket_address = build_test_socket_address("readclose-split");
    let listener = Listener::bind(&socket_address).expect("bind");

    let (sent_sender, sent_receiver) = std::sync::mpsc::channel();
    let server_thread = thread::spawn(move || {
        let connection = listener.accept().expect("accept");
        let read_closer = connection
            .create_read_closer()
            .expect("take the read closer");
        let (mut reader, _writer) = connection.split();
        sent_receiver.recv().expect("the caller sent its frame");
        read_closer.close();
        reader.recv::<IpcRequest>()
    });

    let mut caller = Connection::connect(&socket_address).expect("connect");
    caller.send(&build_hello_request(2)).expect("client send");
    sent_sender.send(()).expect("the closer is told");

    let server_error = server_thread.join().expect("server thread").unwrap_err();
    let IpcError::Disconnected = server_error else {
        panic!("wrong error: {server_error}");
    };
}

#[test]
fn a_closed_read_direction_leaves_the_writing_direction_open() {
    let socket_address = build_test_socket_address("readclose-write");
    let listener = Listener::bind(&socket_address).expect("bind");
    let expected_response = build_hello_answer(3);
    let response_to_send = expected_response.clone();

    let server_thread = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept");
        connection
            .create_read_closer()
            .expect("take the read closer")
            .close();
        connection.send(&response_to_send).expect("server send");
    });

    let mut caller = Connection::connect(&socket_address).expect("connect");
    assert_eq!(
        caller.recv::<IpcResponse>().expect("client recv"),
        expected_response
    );
    server_thread.join().expect("server thread");
}

#[test]
fn closing_the_read_direction_ends_a_read_the_reader_is_blocked_in() {
    use std::io::Write as _;

    // Half a length prefix: the reader waits inside the read for the rest of
    // the header, past the check it makes before reading.
    let (caller, mut connection) = connect_caller_and_peer("readclose-blocked");
    let (caller_reader, mut caller_writer) = caller.split_raw();
    caller_writer
        .write_all(&[0, 0])
        .expect("write half a header");

    let read_closer = connection
        .create_read_closer()
        .expect("take the read closer");
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let reading_thread = thread::spawn(move || {
        started_sender
            .send(())
            .expect("the reader reports it is starting");
        connection.recv::<IpcRequest>()
    });

    started_receiver.recv().expect("the reader started");
    thread::sleep(std::time::Duration::from_millis(50));
    read_closer.close();

    let read_error = reading_thread.join().expect("reading thread").unwrap_err();
    let IpcError::Disconnected = read_error else {
        panic!("wrong error: {read_error}");
    };
    drop(caller_reader);
    drop(caller_writer);
}

/// Split a connection whose caller wrote half a length prefix, give the
/// reading half `idle_timeout`, close the read direction 50 ms after the
/// reading thread starts, and hand back what that read gives.
fn read_a_split_connection_closed_while_blocked(
    test_case_tag: &str,
    idle_timeout: Option<Duration>,
) -> Result<IpcRequest, IpcError> {
    use std::io::Write as _;

    let (caller, connection) = connect_caller_and_peer(test_case_tag);
    let (caller_reader, mut caller_writer) = caller.split_raw();
    caller_writer
        .write_all(&[0, 0])
        .expect("write half a header");
    let read_closer = connection
        .create_read_closer()
        .expect("take the read closer");
    let (mut frame_reader, _frame_writer) = connection.split();
    frame_reader.set_idle_timeout(idle_timeout);
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let reading_thread = thread::spawn(move || {
        started_sender
            .send(())
            .expect("the reader reports it is starting");
        frame_reader.recv::<IpcRequest>()
    });

    started_receiver.recv().expect("the reader started");
    thread::sleep(Duration::from_millis(50));
    read_closer.close();

    let read_outcome = reading_thread.join().expect("reading thread");
    drop(caller_reader);
    drop(caller_writer);
    read_outcome
}

#[test]
fn closing_the_read_direction_ends_a_split_read_the_reader_is_blocked_in() {
    let read_error =
        read_a_split_connection_closed_while_blocked("readclose-split-blocked", None).unwrap_err();
    let IpcError::Disconnected = read_error else {
        panic!("wrong error: {read_error}");
    };
}

#[test]
fn closing_the_read_direction_ends_a_split_read_under_an_idle_timeout() {
    let read_error = read_a_split_connection_closed_while_blocked(
        "readclose-split-idle",
        Some(Duration::from_secs(60)),
    )
    .unwrap_err();
    let IpcError::Disconnected = read_error else {
        panic!("wrong error: {read_error}");
    };
}

#[test]
fn a_raw_read_after_the_read_direction_is_closed_gives_the_end_of_the_stream() {
    use std::io::Read as _;

    let (caller, connection) = connect_caller_and_peer("readclose-raw-after");
    let read_closer = connection
        .create_read_closer()
        .expect("take the read closer");
    let (mut raw_reader, _raw_writer) = connection.split_raw();
    read_closer.close();

    let mut read_buffer = [0u8; 4];
    assert_eq!(
        raw_reader
            .read(&mut read_buffer)
            .expect("the read ends with the end of the stream"),
        0
    );
    drop(caller);
}

#[test]
fn closing_the_read_direction_ends_a_raw_read_the_reader_is_blocked_in() {
    use std::io::Read as _;

    let (caller, connection) = connect_caller_and_peer("readclose-raw");
    let read_closer = connection
        .create_read_closer()
        .expect("take the read closer");
    let (mut raw_reader, _raw_writer) = connection.split_raw();
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let reading_thread = thread::spawn(move || {
        started_sender
            .send(())
            .expect("the reader reports it is starting");
        let mut read_buffer = [0u8; 4];
        raw_reader.read(&mut read_buffer)
    });

    started_receiver.recv().expect("the reader started");
    thread::sleep(Duration::from_millis(50));
    read_closer.close();

    assert_eq!(
        reading_thread
            .join()
            .expect("reading thread")
            .expect("the read ends with the end of the stream"),
        0
    );
    drop(caller);
}

#[test]
fn connecting_where_nothing_listens_reports_no_listener() {
    let expected_socket_address = build_test_socket_address("nobody");
    let connect_error = Connection::connect(&expected_socket_address).unwrap_err();
    let IpcError::NoListener { socket_address } = connect_error else {
        panic!("wrong error: {connect_error}");
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

    let connect_error = Connection::connect(&expected_socket_address).unwrap_err();
    let IpcError::NoListener { socket_address } = connect_error else {
        panic!("wrong error: {connect_error}");
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

    let server_thread = thread::spawn(move || {
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
    assert_eq!(&server_thread.join().expect("server thread"), b"hello");
}
