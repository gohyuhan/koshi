//! OSC 52 encoding tests: exact bytes, base64 roundtrip and padding, the empty
//! and non-ASCII payloads, an escape inside the payload, the targets, paste
//! byte translation, and the runtime copy paths: koshi's own copy for one
//! client, a pane's `OSC 52` copy for every client that focuses the pane, and a
//! stand-in clipboard tool's copy for the clients on other machines. Each path
//! is also run against a client whose waiting host writes replace or refuse
//! the copy.

use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use crate::runtime::pty_inbox::InboxSink;
use koshi_config::types::PaneClipboardWriteConfig;
use koshi_core::geometry::Size;
use koshi_core::ids::SessionId;
use koshi_ipc::event::MAX_HOST_WRITE_BYTE_COUNT;
use koshi_pty::backend::state::PtyBackend;
use koshi_renderer::snapshot::Delivery;
use koshi_test_support::fake_pty::FakePtyBackend;

use super::*;

/// A bare runtime over a fake backend.
fn build_test_runtime() -> Server {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender),
    )));
    Server::from_runtime_parts(pty_backend, runtime_event_receiver)
}

#[test]
fn the_sequence_is_osc_52_targets_base64_bel() {
    assert_eq!(encode_osc52_copy("c", b"hello"), b"\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(
        encode_osc52_copy("pq7", b"hello"),
        b"\x1b]52;pq7;aGVsbG8=\x07"
    );
}

#[test]
fn empty_targets_leave_the_first_field_empty() {
    assert_eq!(encode_osc52_copy("", b"hello"), b"\x1b]52;;aGVsbG8=\x07");
}

#[test]
fn bytes_that_are_not_utf8_encode_as_they_are() {
    assert_eq!(
        encode_osc52_copy("c", &[0xff, 0x00, 0xfe]),
        b"\x1b]52;c;/wD+\x07"
    );
}

#[test]
fn the_payload_roundtrips_through_base64() {
    let clipboard_text = "line one\nline two\t– wide 世界";
    let clipboard_sequence = encode_osc52_copy("c", clipboard_text.as_bytes());
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
    assert_eq!(encode_osc52_copy("c", b""), b"\x1b]52;c;\x07");
    assert_eq!(encode_osc52_copy("", b""), b"\x1b]52;;\x07");
}

#[test]
fn the_base64_padding_follows_the_text_length() {
    assert_eq!(encode_osc52_copy("c", b"a"), b"\x1b]52;c;YQ==\x07");
    assert_eq!(encode_osc52_copy("c", b"ab"), b"\x1b]52;c;YWI=\x07");
    assert_eq!(encode_osc52_copy("c", b"abc"), b"\x1b]52;c;YWJj\x07");
}

#[test]
fn an_escape_in_the_text_is_base64_and_cannot_end_the_sequence() {
    let clipboard_text = "\x1b]52;c;x\x07";

    let clipboard_sequence = encode_osc52_copy("c", clipboard_text.as_bytes());

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
    let mut server = build_test_runtime();
    let client_id = ClientId::new();
    let (delivery_receiver, _) = server.subscribe(client_id);

    server.copy_to_clipboard(client_id, "hello");

    assert_eq!(
        server.take_host_writes(client_id, &delivery_receiver),
        Some(encode_osc52_copy("c", b"hello"))
    );
    // The queue is drained by the take, so a second take finds nothing.
    assert_eq!(server.take_host_writes(client_id, &delivery_receiver), None);
}

#[test]
fn a_second_copy_to_one_client_replaces_the_first_while_it_waits() {
    let mut server = build_test_runtime();
    let client_id = ClientId::new();
    let (delivery_receiver, _) = server.subscribe(client_id);

    server.copy_to_clipboard(client_id, "one");
    server.copy_to_clipboard(client_id, "two");

    assert_eq!(
        server.take_host_writes(client_id, &delivery_receiver),
        Some(encode_osc52_copy("c", b"two"))
    );
}

#[test]
fn a_copy_that_the_waiting_host_writes_refuse_is_skipped_and_they_stay_whole() {
    let mut server = build_test_runtime();
    let client_id = ClientId::new();
    let (delivery_receiver, _) = server.subscribe(client_id);
    let waiting_copied_bytes = vec![b'a'; MAX_HOST_WRITE_BYTE_COUNT];
    assert!(server.queue_host_write(
        client_id,
        &HostWrite::from_clipboard_copy("p", &waiting_copied_bytes)
    ));

    server.copy_to_clipboard(client_id, "hello");

    assert_eq!(
        server.take_host_writes(client_id, &delivery_receiver),
        Some(encode_osc52_copy("p", &waiting_copied_bytes))
    );
}

#[test]
fn copying_an_empty_selection_queues_the_empty_sequence() {
    let mut server = build_test_runtime();
    let client_id = ClientId::new();
    let (delivery_receiver, _) = server.subscribe(client_id);

    server.copy_to_clipboard(client_id, "");

    assert_eq!(
        server.take_host_writes(client_id, &delivery_receiver),
        Some(b"\x1b]52;c;\x07".to_vec())
    );
}

#[test]
fn each_client_takes_only_the_sequence_its_own_copy_queued() {
    let mut server = build_test_runtime();
    let client_with_first_copy = ClientId::new();
    let client_with_second_copy = ClientId::new();
    let (first_receiver, _) = server.subscribe(client_with_first_copy);
    let (second_receiver, _) = server.subscribe(client_with_second_copy);

    server.copy_to_clipboard(client_with_first_copy, "one");
    server.copy_to_clipboard(client_with_second_copy, "two");

    assert_eq!(
        server.take_host_writes(client_with_first_copy, &first_receiver),
        Some(encode_osc52_copy("c", b"one"))
    );
    assert_eq!(
        server.take_host_writes(client_with_second_copy, &second_receiver),
        Some(encode_osc52_copy("c", b"two"))
    );
}

/// One session whose one pane both clients focus: the client on this machine
/// that the bootstrap made, and a client on another machine. Each client has
/// a subscription.
struct ClipboardRoutingFixture {
    /// The server, with `pane-clipboard-writes` as the fixture was built with.
    server: Server,
    /// The session's one pane.
    pane_id: PaneId,
    /// The client on this machine.
    local_client_id: ClientId,
    /// The client on another machine.
    remote_client_id: ClientId,
    /// The subscription of the client on this machine.
    local_receiver: mpsc::Receiver<Delivery>,
    /// The subscription of the client on another machine.
    remote_receiver: mpsc::Receiver<Delivery>,
}

/// [`ClipboardRoutingFixture`] over a server whose `pane-clipboard-writes`
/// is `pane_clipboard_writes` from before the session starts.
fn build_clipboard_routing_fixture(
    pane_clipboard_writes: PaneClipboardWriteConfig,
) -> ClipboardRoutingFixture {
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let mut server = build_test_runtime();
    server.config.pane_clipboard_writes = pane_clipboard_writes;
    let session_id = SessionId::new();
    let local_client_id = server
        .bootstrap_local(session_id, viewport_size, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    let pane_id = *server.live_pane_ids.iter().next().expect("one pane");
    let active_tab_id = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(local_client_id)
        .expect("the bootstrapped client")
        .get_active_tab_id();
    let remote_client_id = ClientId::new();
    let _ = server.handle_client_attach(
        session_id,
        remote_client_id,
        viewport_size,
        None,
        active_tab_id,
        None,
        SystemTime::UNIX_EPOCH,
        true,
    );
    let (local_receiver, _) = server.subscribe(local_client_id);
    let (remote_receiver, _) = server.subscribe(remote_client_id);
    ClipboardRoutingFixture {
        server,
        pane_id,
        local_client_id,
        remote_client_id,
        local_receiver,
        remote_receiver,
    }
}

/// Writes allowed, with copies of at most 8 bytes.
const ALLOWED_PANE_CLIPBOARD_WRITES: PaneClipboardWriteConfig = PaneClipboardWriteConfig {
    should_allow_writes: true,
    maximum_copy_byte_count: 8,
};

#[test]
fn both_clients_of_the_fixture_focus_its_pane_from_where_they_connected() {
    let fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);

    let mut focusing_clients = fixture.server.list_clients_focusing_pane(fixture.pane_id);
    focusing_clients.sort_by_key(|(_, client_origin)| *client_origin == ClientOrigin::Remote);
    assert_eq!(
        focusing_clients,
        vec![
            (fixture.local_client_id, ClientOrigin::Local),
            (fixture.remote_client_id, ClientOrigin::Remote),
        ]
    );
}

#[test]
fn a_pane_copy_reaches_every_client_that_focuses_the_pane_in_order() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let clipboard_copies = [
        ClipboardCopy {
            clipboard_targets: String::from("p"),
            copied_bytes: b"one".to_vec(),
        },
        ClipboardCopy {
            clipboard_targets: String::new(),
            copied_bytes: b"two".to_vec(),
        },
    ];

    fixture
        .server
        .copy_pane_clipboard_to_clients(fixture.pane_id, &clipboard_copies);

    let mut expected_host_writes = encode_osc52_copy("p", b"one");
    expected_host_writes.extend_from_slice(&encode_osc52_copy("", b"two"));
    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.local_client_id, &fixture.local_receiver),
        Some(expected_host_writes.clone())
    );
    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(expected_host_writes)
    );
}

#[test]
fn a_pane_copy_for_a_pane_no_client_focuses_reaches_no_client() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);

    fixture.server.copy_pane_clipboard_to_clients(
        PaneId::new(),
        &[ClipboardCopy {
            clipboard_targets: String::from("c"),
            copied_bytes: b"one".to_vec(),
        }],
    );

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.local_client_id, &fixture.local_receiver),
        None
    );
    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        None
    );
}

#[test]
fn a_pane_copy_that_one_clients_waiting_host_writes_refuse_still_reaches_the_other() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let waiting_copied_bytes = vec![b'a'; MAX_HOST_WRITE_BYTE_COUNT];
    assert!(fixture.server.queue_host_write(
        fixture.remote_client_id,
        &HostWrite::from_clipboard_copy("p", &waiting_copied_bytes)
    ));

    fixture.server.copy_pane_clipboard_to_clients(
        fixture.pane_id,
        &[ClipboardCopy {
            clipboard_targets: String::from("c"),
            copied_bytes: b"one".to_vec(),
        }],
    );

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.local_client_id, &fixture.local_receiver),
        Some(encode_osc52_copy("c", b"one"))
    );
    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(encode_osc52_copy("p", &waiting_copied_bytes))
    );
}

#[test]
fn pane_output_with_an_osc_52_copy_reaches_each_focusing_client_while_writes_are_allowed() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);

    fixture
        .server
        .handle_pty_output(fixture.pane_id, b"a\x1b]52;c;aGk=\x07b");

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.local_client_id, &fixture.local_receiver),
        Some(encode_osc52_copy("c", b"hi"))
    );
    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(encode_osc52_copy("c", b"hi"))
    );
}

#[test]
fn pane_output_with_an_osc_52_copy_reaches_no_client_while_writes_are_off() {
    let mut fixture = build_clipboard_routing_fixture(PaneClipboardWriteConfig::default());

    fixture
        .server
        .handle_pty_output(fixture.pane_id, b"a\x1b]52;c;aGk=\x07b");

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.local_client_id, &fixture.local_receiver),
        None
    );
    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        None
    );
}

#[test]
fn a_tool_copy_with_only_a_client_on_another_machine_focusing_reaches_it_and_is_routed() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let _ = fixture.server.handle_client_detach(fixture.local_client_id);

    let is_routed = fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "c", b"hello");

    assert!(is_routed);
    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(encode_osc52_copy("c", b"hello"))
    );
}

#[test]
fn a_tool_copy_with_a_client_on_this_machine_focusing_is_not_routed_and_still_reaches_the_other() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);

    let is_routed = fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "c", b"hello");

    assert!(!is_routed);
    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(encode_osc52_copy("c", b"hello"))
    );
    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.local_client_id, &fixture.local_receiver),
        None
    );
}

#[test]
fn a_tool_copy_keeps_only_the_clipboard_targets_once_each() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let _ = fixture.server.handle_client_detach(fixture.local_client_id);

    assert!(fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "pXcp", b"hello"));

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(encode_osc52_copy("pc", b"hello"))
    );
}

#[test]
fn a_tool_copy_of_exactly_the_copy_limit_is_taken() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let _ = fixture.server.handle_client_detach(fixture.local_client_id);

    assert!(fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "c", b"12345678"));

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(encode_osc52_copy("c", b"12345678"))
    );
}

#[test]
fn a_tool_copy_past_the_copy_limit_is_not_routed_and_reaches_no_client() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let _ = fixture.server.handle_client_detach(fixture.local_client_id);

    assert!(!fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "c", b"123456789"));

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        None
    );
}

#[test]
fn a_tool_copy_while_writes_are_off_is_not_routed_and_reaches_no_client() {
    let mut fixture = build_clipboard_routing_fixture(PaneClipboardWriteConfig::default());
    let _ = fixture.server.handle_client_detach(fixture.local_client_id);

    assert!(!fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "c", b"hello"));

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        None
    );
}

#[test]
fn a_tool_copy_for_a_pane_no_client_focuses_is_not_routed() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);

    assert!(!fixture
        .server
        .route_clipboard_tool_copy(PaneId::new(), "c", b"hello"));

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        None
    );
}

#[test]
fn a_tool_copy_whose_client_on_another_machine_lost_its_receiver_is_not_routed() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let _ = fixture.server.handle_client_detach(fixture.local_client_id);
    drop(fixture.remote_receiver);

    assert!(!fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "c", b"hello"));
}

#[test]
fn a_tool_copy_that_one_client_on_another_machine_cannot_take_still_reaches_the_other() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let _ = fixture.server.handle_client_detach(fixture.local_client_id);
    let session_id = *fixture
        .server
        .session_by_id
        .keys()
        .next()
        .expect("one session");
    let active_tab_id = fixture.server.session_by_id[&session_id]
        .clients
        .get_client_by_id(fixture.remote_client_id)
        .expect("the client on another machine")
        .get_active_tab_id();
    let unsubscribed_client_id = ClientId::new();
    let _ = fixture.server.handle_client_attach(
        session_id,
        unsubscribed_client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        active_tab_id,
        None,
        SystemTime::UNIX_EPOCH,
        true,
    );

    assert!(!fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "c", b"hello"));

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(encode_osc52_copy("c", b"hello"))
    );
}

#[test]
fn a_tool_copy_that_the_waiting_host_writes_refuse_is_not_routed_and_they_stay_whole() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let _ = fixture.server.handle_client_detach(fixture.local_client_id);
    let waiting_copied_bytes = vec![b'a'; MAX_HOST_WRITE_BYTE_COUNT];
    assert!(fixture.server.queue_host_write(
        fixture.remote_client_id,
        &HostWrite::from_clipboard_copy("p", &waiting_copied_bytes)
    ));

    assert!(!fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "c", b"hello"));

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(encode_osc52_copy("p", &waiting_copied_bytes))
    );
}

#[test]
fn a_tool_copy_replaces_the_waiting_copy_for_its_clipboard_targets_kept_once_each() {
    let mut fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    let _ = fixture.server.handle_client_detach(fixture.local_client_id);
    let waiting_copied_bytes = vec![b'a'; MAX_HOST_WRITE_BYTE_COUNT];
    assert!(fixture.server.queue_host_write(
        fixture.remote_client_id,
        &HostWrite::from_clipboard_copy("p", &waiting_copied_bytes)
    ));

    assert!(fixture
        .server
        .route_clipboard_tool_copy(fixture.pane_id, "pXp", b"hello"));

    assert_eq!(
        fixture
            .server
            .take_host_writes(fixture.remote_client_id, &fixture.remote_receiver),
        Some(encode_osc52_copy("p", b"hello"))
    );
}

#[test]
fn the_session_takes_a_tool_copy_up_to_the_copy_limit_while_writes_are_allowed() {
    let allowed_fixture = build_clipboard_routing_fixture(ALLOWED_PANE_CLIPBOARD_WRITES);
    assert!(allowed_fixture.server.can_take_clipboard_tool_copy(0));
    assert!(allowed_fixture.server.can_take_clipboard_tool_copy(8));
    assert!(!allowed_fixture.server.can_take_clipboard_tool_copy(9));

    let off_fixture = build_clipboard_routing_fixture(PaneClipboardWriteConfig::default());
    assert!(!off_fixture.server.can_take_clipboard_tool_copy(0));
}
