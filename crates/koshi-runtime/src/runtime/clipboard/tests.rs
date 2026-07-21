//! OSC 52 encoding tests: exact bytes, base64 roundtrip, the empty and
//! non-ASCII payloads, paste byte translation, and the runtime copy path that
//! queues the OSC 52 write for the client's outer terminal.

use std::sync::{mpsc, Arc};

use koshi_core::geometry::Direction;
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;

use super::*;

/// A bare runtime over a fake backend. The sender keeps the inbox open.
fn new_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        Direction::Right,
    );
    (runtime, tx)
}

#[test]
fn the_sequence_is_osc_52_c_base64_bel() {
    assert_eq!(osc52_copy("hello"), b"\x1b]52;c;aGVsbG8=\x07");
}

#[test]
fn the_payload_roundtrips_through_base64() {
    let text = "line one\nline two\t– wide 世界";
    let sequence = osc52_copy(text);
    let payload = &sequence[b"\x1b]52;c;".len()..sequence.len() - 1];
    let decoded = STANDARD.decode(payload).expect("valid base64");
    assert_eq!(String::from_utf8(decoded).expect("utf-8"), text);
}

#[test]
fn empty_text_encodes_an_empty_payload() {
    assert_eq!(osc52_copy(""), b"\x1b]52;c;\x07");
}

#[test]
fn the_osc52_writer_produces_the_sequence() {
    let mut clipboard = Osc52Clipboard::default();

    assert!(clipboard.write("hello"));
    assert_eq!(clipboard.bytes, b"\x1b]52;c;aGVsbG8=\x07");
}

#[test]
fn paste_bytes_writes_every_line_break_as_one_return() {
    // Clipboard text from Windows or a browser carries `\r\n`; a paste must
    // send ONE Enter per line break, never two.
    assert_eq!(paste_bytes("a\r\nb", false), b"a\rb");
    assert_eq!(paste_bytes("a\nb", false), b"a\rb");
    assert_eq!(paste_bytes("a\rb", false), b"a\rb");
}

#[test]
fn paste_bytes_folds_every_line_break_spelling_in_one_string_to_returns() {
    assert_eq!(paste_bytes("a\r\nb\nc\rd", false), b"a\rb\rc\rd");
}

#[test]
fn empty_paste_is_empty_bytes_when_unbracketed() {
    assert_eq!(paste_bytes("", false), b"");
}

#[test]
fn a_bracketed_paste_wraps_the_payload_in_the_paste_markers() {
    assert_eq!(paste_bytes("ab", true), b"\x1b[200~ab\x1b[201~");
}

#[test]
fn a_bracketed_paste_still_folds_line_breaks_to_returns_inside_the_markers() {
    assert_eq!(paste_bytes("a\r\nb", true), b"\x1b[200~a\rb\x1b[201~");
}

#[test]
fn an_empty_bracketed_paste_is_just_the_two_markers() {
    assert_eq!(paste_bytes("", true), b"\x1b[200~\x1b[201~");
}

#[test]
fn copying_queues_the_osc_52_sequence_for_the_clients_outer_terminal() {
    let (mut rt, _tx) = new_runtime();
    let client = ClientId::new();

    rt.copy_to_clipboard(client, "hello");

    assert_eq!(rt.take_host_writes(client), Some(osc52_copy("hello")));
    // The queue is drained by the take, so a second take finds nothing.
    assert_eq!(rt.take_host_writes(client), None);
}

#[test]
fn two_copies_to_one_client_queue_both_sequences_in_order() {
    let (mut rt, _tx) = new_runtime();
    let client = ClientId::new();

    rt.copy_to_clipboard(client, "one");
    rt.copy_to_clipboard(client, "two");

    let mut expected = osc52_copy("one");
    expected.extend_from_slice(&osc52_copy("two"));
    assert_eq!(rt.take_host_writes(client), Some(expected));
}
