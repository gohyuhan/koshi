//! OSC 52 encoding tests: exact bytes, base64 roundtrip and padding, the empty
//! and non-ASCII payloads, an escape inside the payload, paste byte
//! translation, and the runtime copy path that queues the OSC 52 write on each
//! client's own outer-terminal queue.

use std::sync::{mpsc, Arc};

use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::event::RuntimeEvent;

use super::*;

/// A bare runtime over a fake backend. The sender keeps the inbox open.
fn build_test_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (event_sender, event_receiver) = mpsc::channel();
    let server = Server::from_runtime_parts(pty_backend, event_receiver, event_sender.clone());
    (server, event_sender)
}

#[test]
fn the_sequence_is_osc_52_c_base64_bel() {
    assert_eq!(osc52_copy("hello"), b"\x1b]52;c;aGVsbG8=\x07");
}

#[test]
fn the_payload_roundtrips_through_base64() {
    let clipboard_text = "line one\nline two\t– wide 世界";
    let clipboard_sequence = osc52_copy(clipboard_text);
    let clipboard_payload_bytes =
        &clipboard_sequence[b"\x1b]52;c;".len()..clipboard_sequence.len() - 1];
    let decoded_clipboard_bytes = STANDARD
        .decode(clipboard_payload_bytes)
        .expect("valid base64");
    assert_eq!(
        String::from_utf8(decoded_clipboard_bytes).expect("utf-8"),
        clipboard_text
    );
}

#[test]
fn empty_text_encodes_an_empty_payload() {
    assert_eq!(osc52_copy(""), b"\x1b]52;c;\x07");
}

#[test]
fn the_base64_padding_follows_the_text_length() {
    assert_eq!(osc52_copy("a"), b"\x1b]52;c;YQ==\x07");
    assert_eq!(osc52_copy("ab"), b"\x1b]52;c;YWI=\x07");
    assert_eq!(osc52_copy("abc"), b"\x1b]52;c;YWJj\x07");
}

#[test]
fn an_escape_in_the_text_is_base64_and_cannot_end_the_sequence() {
    let clipboard_text = "\x1b]52;c;x\x07";

    let clipboard_sequence = osc52_copy(clipboard_text);

    assert_eq!(clipboard_sequence, b"\x1b]52;c;G101MjtjO3gH\x07");
    let clipboard_payload_bytes =
        &clipboard_sequence[b"\x1b]52;c;".len()..clipboard_sequence.len() - 1];
    assert_eq!(clipboard_payload_bytes, b"G101MjtjO3gH");
    assert_eq!(
        STANDARD
            .decode(clipboard_payload_bytes)
            .expect("valid base64"),
        clipboard_text.as_bytes()
    );
}

#[test]
fn build_paste_bytes_writes_every_line_break_as_one_return() {
    // Clipboard text from Windows or a browser carries `\r\n`; a paste must
    // send ONE Enter per line break, never two.
    assert_eq!(build_paste_bytes("a\r\nb", false), b"a\rb");
    assert_eq!(build_paste_bytes("a\nb", false), b"a\rb");
    assert_eq!(build_paste_bytes("a\rb", false), b"a\rb");
}

#[test]
fn build_paste_bytes_folds_every_line_break_spelling_in_one_string_to_returns() {
    assert_eq!(build_paste_bytes("a\r\nb\nc\rd", false), b"a\rb\rc\rd");
}

#[test]
fn a_line_feed_followed_by_a_return_is_two_line_breaks() {
    assert_eq!(build_paste_bytes("a\n\rb", false), b"a\r\rb");
}

#[test]
fn text_that_is_only_line_breaks_pastes_only_returns() {
    assert_eq!(build_paste_bytes("\r\n\n\r", false), b"\r\r\r");
}

#[test]
fn a_line_break_at_either_end_stays_at_that_end() {
    assert_eq!(build_paste_bytes("\nls -l\n", false), b"\rls -l\r");
}

#[test]
fn non_ascii_text_pastes_its_utf8_bytes() {
    assert_eq!(build_paste_bytes("héllo 世", false), "héllo 世".as_bytes());
}

#[test]
fn empty_paste_is_empty_bytes_when_unbracketed() {
    assert_eq!(build_paste_bytes("", false), b"");
}

#[test]
fn a_bracketed_paste_wraps_the_payload_in_the_paste_markers() {
    assert_eq!(build_paste_bytes("ab", true), b"\x1b[200~ab\x1b[201~");
}

#[test]
fn a_bracketed_paste_still_folds_line_breaks_to_returns_inside_the_markers() {
    assert_eq!(build_paste_bytes("a\r\nb", true), b"\x1b[200~a\rb\x1b[201~");
}

#[test]
fn an_empty_bracketed_paste_is_just_the_two_markers() {
    assert_eq!(build_paste_bytes("", true), b"\x1b[200~\x1b[201~");
}

#[test]
fn copying_queues_the_osc_52_sequence_for_the_clients_outer_terminal() {
    let (mut server, _event_sender) = build_test_runtime();
    let client_id = ClientId::new();

    server.copy_to_clipboard(client_id, CopyTarget::Osc52, "hello");

    assert_eq!(
        server.take_host_writes(client_id),
        Some(osc52_copy("hello"))
    );
    // The queue is drained by the take, so a second take finds nothing.
    assert_eq!(server.take_host_writes(client_id), None);
}

#[test]
fn two_copies_to_one_client_queue_both_sequences_in_order() {
    let (mut server, _event_sender) = build_test_runtime();
    let client_id = ClientId::new();

    server.copy_to_clipboard(client_id, CopyTarget::Osc52, "one");
    server.copy_to_clipboard(client_id, CopyTarget::Osc52, "two");

    let mut expected_host_writes = osc52_copy("one");
    expected_host_writes.extend_from_slice(&osc52_copy("two"));
    assert_eq!(
        server.take_host_writes(client_id),
        Some(expected_host_writes)
    );
}

#[test]
fn copying_an_empty_selection_queues_the_empty_sequence() {
    let (mut server, _event_sender) = build_test_runtime();
    let client_id = ClientId::new();

    server.copy_to_clipboard(client_id, CopyTarget::Osc52, "");

    assert_eq!(
        server.take_host_writes(client_id),
        Some(b"\x1b]52;c;\x07".to_vec())
    );
}

#[test]
fn each_client_takes_only_the_sequence_its_own_copy_queued() {
    let (mut server, _event_sender) = build_test_runtime();
    let client_with_first_copy = ClientId::new();
    let client_with_second_copy = ClientId::new();

    server.copy_to_clipboard(client_with_first_copy, CopyTarget::Osc52, "one");
    server.copy_to_clipboard(client_with_second_copy, CopyTarget::Osc52, "two");

    assert_eq!(
        server.take_host_writes(client_with_first_copy),
        Some(osc52_copy("one"))
    );
    assert_eq!(
        server.take_host_writes(client_with_second_copy),
        Some(osc52_copy("two"))
    );
}

#[test]
fn a_native_copy_leaves_an_earlier_osc_52_copy_on_the_queue_unchanged() {
    let (mut server, _event_sender) = build_test_runtime();
    let client_id = ClientId::new();

    server.copy_to_clipboard(client_id, CopyTarget::Osc52, "one");
    server.copy_to_clipboard(client_id, CopyTarget::Native, "two");

    assert_eq!(server.take_host_writes(client_id), Some(osc52_copy("one")));
}

#[test]
fn a_copy_to_the_native_clipboard_writes_nothing_to_the_outer_terminal() {
    // Koshi builds no native operating-system clipboard backend, so a copy
    // naming one has nowhere to go and queues no escape.
    let (mut server, _event_sender) = build_test_runtime();
    let client_id = ClientId::new();

    server.copy_to_clipboard(client_id, CopyTarget::Native, "hello");

    assert_eq!(server.take_host_writes(client_id), None);
}
