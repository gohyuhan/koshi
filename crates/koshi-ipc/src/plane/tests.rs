//! Tests for the four decisions every server makes the same way.
//!
//! Each one runs over a real socket, the way every other transport test in
//! this crate does: a listener in the temp directory, one connected caller,
//! and [`next_request`] driven on the server's end.
//!
//! Every test drives [`next_request`] with the session protocol's
//! [`SessionPlane`]; the four decisions are the same on any plane.

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use koshi_core::geometry::Size;

use crate::handshake::{Handshake, Peer};
use crate::protocol::{
    ConnectionToken, EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult,
    SessionPlane, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use crate::transport::Listener;

/// The build version a test server reports in its Hello answer.
const TEST_BUILD_VERSION: &str = "9.9.9";

/// A socket address of this crate's own, named for the test that binds it.
fn build_test_socket_address(test_name: &str) -> String {
    let socket_name = format!("koshi-plane-{}-{test_name}", std::process::id());
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(socket_name)
            .with_extension("sock")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        socket_name
    }
}

/// A gate for a same-user local connection expecting `connection_token`.
fn build_test_handshake(connection_token: ConnectionToken) -> Handshake {
    Handshake::from_expected_token_and_peer(
        connection_token,
        Peer::Local {
            is_same_user: true,
            is_other_user_access_allowed: false,
        },
    )
}

/// Serve one connection at `socket_address`, running `next_request` until it says stop,
/// and hand back every outcome it produced in order.
///
/// The dispatch arm answers nothing: this module owns the decisions before
/// dispatch, so a `Dispatch` outcome is recorded and the loop moves on.
fn run_test_request_loop(
    socket_address: &str,
    connection_token: ConnectionToken,
) -> thread::JoinHandle<Vec<RequestDisposition<IpcRequestKind>>> {
    serve_test_connection(socket_address, connection_token, || true)
}

/// The same, with `should_admit_connection` deciding whether each arrived request is served.
fn serve_test_connection(
    socket_address: &str,
    connection_token: ConnectionToken,
    should_admit_connection: impl Fn() -> bool + Send + 'static,
) -> thread::JoinHandle<Vec<RequestDisposition<IpcRequestKind>>> {
    let (server_thread, connection_accepted_rx) =
        serve_connection_and_announce(socket_address, connection_token, should_admit_connection);
    drop(connection_accepted_rx);
    server_thread
}

/// The same again, and it says when it has the connection.
///
/// The receiver yields once, right after `accept` returns. On Windows a caller
/// that connects and drops before it is accepted occupies the pipe until the
/// next `accept`, and this helper accepts once: a test that drops its caller
/// without sending anything waits for the receiver first.
fn serve_connection_and_announce(
    socket_address: &str,
    connection_token: ConnectionToken,
    should_admit_connection: impl Fn() -> bool + Send + 'static,
) -> (
    thread::JoinHandle<Vec<RequestDisposition<IpcRequestKind>>>,
    mpsc::Receiver<()>,
) {
    let listener = Listener::bind(socket_address).expect("bind the test listener");
    let (connection_accepted_tx, connection_accepted_rx) = mpsc::channel();
    let server_thread = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let _ = connection_accepted_tx.send(());
        let mut request_handshake = build_test_handshake(connection_token);
        let mut request_dispositions = Vec::new();
        loop {
            let request_disposition = next_request::<SessionPlane>(
                &mut connection,
                &mut request_handshake,
                TEST_BUILD_VERSION,
                &should_admit_connection,
            );
            let should_stop = request_disposition == RequestDisposition::Stop;
            request_dispositions.push(request_disposition);
            if should_stop {
                return request_dispositions;
            }
        }
    });
    (server_thread, connection_accepted_rx)
}

/// How long a test waits for its body to finish before calling the server
/// broken.
const ANSWER_WAIT_DURATION: Duration = Duration::from_secs(10);

/// Run one test's body on a worker thread. A body that has not finished
/// within [`ANSWER_WAIT_DURATION`] fails with a message naming the wait; a body that
/// panics fails with its own message.
///
/// [`Connection`] has no read deadline: a server that stops answering blocks
/// the body on a socket read, and the deadline turns that into a failure.
fn within_deadline(test_body: impl FnOnce() + Send + 'static) {
    let (test_finished_tx, test_finished_rx) = mpsc::channel();
    let test_thread = thread::spawn(move || {
        test_body();
        let _ = test_finished_tx.send(());
    });
    match test_finished_rx.recv_timeout(ANSWER_WAIT_DURATION) {
        Ok(()) => test_thread.join().expect("the test body finished"),
        // The body panicked, so its own message is this test's failure.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            test_thread.join().expect("the test body panicked");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("the server did not answer within {ANSWER_WAIT_DURATION:?}")
        }
    }
}

/// The Hello this build's caller opens with.
fn build_test_hello_request(request_id: u64, connection_token: ConnectionToken) -> IpcRequest {
    IpcRequest {
        request_id,
        request_kind: IpcRequestKind::build_hello_request(connection_token),
    }
}

#[test]
fn a_hello_is_answered_here_with_the_settled_version_and_the_build() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("hello");
        let connection_token = ConnectionToken::from_secret("secret");
        let server_thread = run_test_request_loop(&socket_address, connection_token.clone());

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&build_test_hello_request(1, connection_token))
            .expect("send hello");
        let ipc_response: IpcResponse = caller_connection.recv().expect("read the hello answer");
        drop(caller_connection);

        assert_eq!(
            ipc_response,
            IpcResponse {
                request_id: Some(1),
                answer_result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: TEST_BUILD_VERSION.to_string(),
                },
            }
        );
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![RequestDisposition::Answered, RequestDisposition::Stop]
        );
    });
}

#[test]
fn a_request_after_the_hello_is_handed_to_the_callers_dispatch() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("dispatch");
        let connection_token = ConnectionToken::from_secret("secret");
        let server_thread = run_test_request_loop(&socket_address, connection_token.clone());

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&build_test_hello_request(1, connection_token))
            .expect("send hello");
        let _: IpcResponse = caller_connection.recv().expect("read the hello answer");
        caller_connection
            .send(&IpcRequest {
                request_id: 2,
                request_kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery");
        drop(caller_connection);

        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![
                RequestDisposition::Answered,
                RequestDisposition::Dispatch {
                    request_id: 2,
                    request_kind: IpcRequestKind::Discovery,
                },
                RequestDisposition::Stop,
            ]
        );
    });
}

#[test]
fn a_request_before_the_hello_is_refused_here_and_the_connection_keeps_serving() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("gated");
        let connection_token = ConnectionToken::from_secret("secret");
        let server_thread = run_test_request_loop(&socket_address, connection_token.clone());

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&IpcRequest {
                request_id: 1,
                request_kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery first");
        let refusal_response: IpcResponse = caller_connection.recv().expect("read the refusal");
        // The connection is still open, so the Hello that follows is served.
        caller_connection
            .send(&build_test_hello_request(2, connection_token))
            .expect("send hello");
        let hello_response: IpcResponse = caller_connection.recv().expect("read the hello answer");
        drop(caller_connection);

        assert_eq!(
            refusal_response,
            IpcResponse {
                request_id: Some(1),
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::HelloRequired,
                    message: "Discovery arrived before a Hello opened the connection".to_string(),
                }),
            }
        );
        assert_eq!(
            hello_response.answer_result,
            IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                build_version: TEST_BUILD_VERSION.to_string(),
            }
        );
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![
                RequestDisposition::Answered,
                RequestDisposition::Answered,
                RequestDisposition::Stop
            ]
        );
    });
}

#[test]
fn a_kind_this_build_does_not_have_is_refused_by_name_and_the_connection_keeps_serving() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("unknown-kind");
        let connection_token = ConnectionToken::from_secret("secret");
        let server_thread = run_test_request_loop(&socket_address, connection_token.clone());

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&build_test_hello_request(1, connection_token))
            .expect("send hello");
        let _: IpcResponse = caller_connection.recv().expect("read the hello answer");
        // A newer koshi's verb, spelled straight onto the wire.
        caller_connection
            .send(&serde_json::json!({"request_id": 2, "request_kind": {"Rehome": {"pane_id": 3}}}))
            .expect("send an unfamiliar kind");
        let refusal_response: IpcResponse = caller_connection.recv().expect("read the refusal");
        // Still serving: the next familiar request is dispatched as usual.
        caller_connection
            .send(&IpcRequest {
                request_id: 3,
                request_kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery");
        drop(caller_connection);

        assert_eq!(
            refusal_response,
            IpcResponse {
                request_id: Some(2),
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::UnsupportedKind,
                    message: "this Koshi has no request kind named Rehome".to_string(),
                }),
            }
        );
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![
                RequestDisposition::Answered,
                RequestDisposition::Answered,
                RequestDisposition::Dispatch {
                    request_id: 3,
                    request_kind: IpcRequestKind::Discovery,
                },
                RequestDisposition::Stop,
            ]
        );
    });
}

#[test]
fn a_frame_read_whole_but_unreadable_is_refused_and_the_connection_keeps_serving() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("malformed");
        let connection_token = ConnectionToken::from_secret("secret");
        let server_thread = run_test_request_loop(&socket_address, connection_token.clone());

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        // Whole frame, aligned stream, bytes that are not a request.
        caller_connection.send(&"not a request").expect("send junk");
        let refusal_response: IpcResponse = caller_connection.recv().expect("read the refusal");
        caller_connection
            .send(&build_test_hello_request(2, connection_token))
            .expect("send hello");
        let hello_response: IpcResponse = caller_connection.recv().expect("read the hello answer");
        drop(caller_connection);

        assert_eq!(
            refusal_response,
            IpcResponse {
                request_id: None,
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::MalformedRequest,
                    message: "the bytes received are not a request this build can read".to_string(),
                }),
            }
        );
        assert_eq!(
            hello_response,
            IpcResponse {
                request_id: Some(2),
                answer_result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: TEST_BUILD_VERSION.to_string(),
                },
            }
        );
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![
                RequestDisposition::Answered,
                RequestDisposition::Answered,
                RequestDisposition::Stop
            ]
        );
    });
}

#[test]
fn a_kind_this_build_does_not_have_before_the_hello_is_refused_as_hello_required() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("unknown-kind-gated");
        let connection_token = ConnectionToken::from_secret("secret");
        let server_thread = run_test_request_loop(&socket_address, connection_token.clone());

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&serde_json::json!({"request_id": 1, "request_kind": {"Rehome": {"pane_id": 3}}}))
            .expect("send an unfamiliar kind first");
        let refusal_response: IpcResponse = caller_connection.recv().expect("read the refusal");
        caller_connection
            .send(&build_test_hello_request(2, connection_token))
            .expect("send hello");
        let hello_response: IpcResponse = caller_connection.recv().expect("read the hello answer");
        drop(caller_connection);

        // A closed gate answers an unknown kind with `HelloRequired`, the
        // same as a known kind.
        assert_eq!(
            refusal_response,
            IpcResponse {
                request_id: Some(1),
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::HelloRequired,
                    message: "Rehome arrived before a Hello opened the connection".to_string(),
                }),
            }
        );
        assert_eq!(
            hello_response,
            IpcResponse {
                request_id: Some(2),
                answer_result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: TEST_BUILD_VERSION.to_string(),
                },
            }
        );
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![
                RequestDisposition::Answered,
                RequestDisposition::Answered,
                RequestDisposition::Stop
            ]
        );
    });
}

#[test]
fn a_hello_naming_a_version_range_this_build_does_not_share_is_refused_here() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("version");
        let connection_token = ConnectionToken::from_secret("secret");
        let server_thread = run_test_request_loop(&socket_address, connection_token.clone());

        let unsupported_protocol_version = PROTOCOL_VERSION + 5;
        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&IpcRequest {
                request_id: 1,
                request_kind: IpcRequestKind::Hello {
                    min_protocol_version: unsupported_protocol_version,
                    max_protocol_version: unsupported_protocol_version,
                    connection_token,
                    is_remote: false,
                },
            })
            .expect("send a hello from the future");
        let refusal_response: IpcResponse = caller_connection.recv().expect("read the refusal");
        drop(caller_connection);

        assert_eq!(
            refusal_response,
            IpcResponse {
                request_id: Some(1),
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::UnsupportedVersion,
                    message: format!(
                        "the caller speaks protocol versions {unsupported_protocol_version} to {unsupported_protocol_version}, this Koshi speaks \
                         {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}"
                    ),
                }),
            }
        );
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![RequestDisposition::Answered, RequestDisposition::Stop]
        );
    });
}

#[test]
fn a_hello_presenting_the_wrong_token_is_refused_here() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("token");
        let server_thread = run_test_request_loop(
            &socket_address,
            ConnectionToken::from_secret("the real secret"),
        );

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&build_test_hello_request(
                1,
                ConnectionToken::from_secret("a guess"),
            ))
            .expect("send hello");
        let refusal_response: IpcResponse = caller_connection.recv().expect("read the refusal");
        drop(caller_connection);

        assert_eq!(
            refusal_response,
            IpcResponse {
                request_id: Some(1),
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::BadToken,
                    message: "the token presented does not match this Koshi's".to_string(),
                }),
            }
        );
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![RequestDisposition::Answered, RequestDisposition::Stop]
        );
    });
}

#[test]
fn a_refused_hello_keeps_the_connection_serving_for_the_next_hello() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("token-retry");
        let connection_token = ConnectionToken::from_secret("the real secret");
        let server_thread = run_test_request_loop(&socket_address, connection_token.clone());

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&build_test_hello_request(
                1,
                ConnectionToken::from_secret("a guess"),
            ))
            .expect("send a wrong hello");
        let refusal_response: IpcResponse = caller_connection.recv().expect("read the refusal");
        caller_connection
            .send(&build_test_hello_request(2, connection_token))
            .expect("send the right hello");
        let hello_response: IpcResponse = caller_connection.recv().expect("read the hello answer");
        drop(caller_connection);

        assert_eq!(
            refusal_response,
            IpcResponse {
                request_id: Some(1),
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::BadToken,
                    message: "the token presented does not match this Koshi's".to_string(),
                }),
            }
        );
        assert_eq!(
            hello_response,
            IpcResponse {
                request_id: Some(2),
                answer_result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: TEST_BUILD_VERSION.to_string(),
                },
            }
        );
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![
                RequestDisposition::Answered,
                RequestDisposition::Answered,
                RequestDisposition::Stop
            ]
        );
    });
}

#[test]
fn a_peer_no_longer_admitted_is_answered_nothing_at_all() {
    within_deadline(|| {
        // `should_admit_connection` is read after the Hello arrives: a peer whose access was
        // withdrawn while its connection sat open is not served that Hello.
        let socket_address = build_test_socket_address("withdrawn");
        let connection_token = ConnectionToken::from_secret("secret");
        let server_thread =
            serve_test_connection(&socket_address, connection_token.clone(), || false);

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&build_test_hello_request(1, connection_token))
            .expect("send hello");
        let response_result: Result<IpcResponse, _> = caller_connection.recv();
        drop(caller_connection);

        let Err(IpcError::Disconnected) = response_result else {
            panic!("a withdrawn peer reads no answer, got {response_result:?}");
        };
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![RequestDisposition::Stop]
        );
    });
}

#[test]
fn a_peer_whose_access_is_withdrawn_after_the_hello_is_answered_nothing_more() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("withdrawn-after-hello");
        let connection_token = ConnectionToken::from_secret("secret");
        let is_connection_admitted = Arc::new(AtomicBool::new(true));
        let should_admit_connection = Arc::clone(&is_connection_admitted);
        let server_thread =
            serve_test_connection(&socket_address, connection_token.clone(), move || {
                should_admit_connection.load(Ordering::SeqCst)
            });

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&build_test_hello_request(1, connection_token))
            .expect("send hello");
        let hello_response: IpcResponse = caller_connection.recv().expect("read the hello answer");
        // The setting turns off between one request and the next.
        is_connection_admitted.store(false, Ordering::SeqCst);
        caller_connection
            .send(&IpcRequest {
                request_id: 2,
                request_kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery");
        let response_result: Result<IpcResponse, _> = caller_connection.recv();
        drop(caller_connection);

        assert_eq!(
            hello_response,
            IpcResponse {
                request_id: Some(1),
                answer_result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: TEST_BUILD_VERSION.to_string(),
                },
            }
        );
        let Err(IpcError::Disconnected) = response_result else {
            panic!("a withdrawn peer reads no answer, got {response_result:?}");
        };
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![RequestDisposition::Answered, RequestDisposition::Stop]
        );
    });
}

#[test]
fn a_malformed_frame_is_answered_even_while_the_peer_is_not_admitted() {
    within_deadline(|| {
        // The malformed-frame answer goes out before `should_admit_connection` is read.
        let socket_address = build_test_socket_address("withdrawn-junk");
        let server_thread = serve_test_connection(
            &socket_address,
            ConnectionToken::from_secret("secret"),
            || false,
        );

        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection.send(&"not a request").expect("send junk");
        let refusal_response: IpcResponse = caller_connection.recv().expect("read the refusal");
        drop(caller_connection);

        assert_eq!(
            refusal_response,
            IpcResponse {
                request_id: None,
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::MalformedRequest,
                    message: "the bytes received are not a request this build can read".to_string(),
                }),
            }
        );
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![RequestDisposition::Answered, RequestDisposition::Stop]
        );
    });
}

#[test]
fn a_peer_that_hangs_up_ends_the_connection() {
    within_deadline(|| {
        let socket_address = build_test_socket_address("hangup");
        let (server_thread, connection_accepted_rx) = serve_connection_and_announce(
            &socket_address,
            ConnectionToken::from_secret("secret"),
            || true,
        );

        let caller_connection = Connection::connect(&socket_address).expect("connect");
        // Dropped once the server holds the connection: a peer going away
        // mid-serve.
        connection_accepted_rx
            .recv()
            .expect("the server accepted the caller");
        drop(caller_connection);

        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![RequestDisposition::Stop]
        );
    });
}

/// A length prefix past [`MAX_FRAME_BYTE_COUNT`] ends the connection with nothing
/// written: the caller reads end of stream, and the server's payload is never
/// read. The prefix is written on the raw socket; [`Connection::send`]
/// refuses an oversize frame on the sending side.
#[cfg(unix)]
#[test]
fn an_oversize_length_prefix_ends_the_connection_with_nothing_written() {
    use std::io::{Read as _, Write as _};

    use crate::transport::MAX_FRAME_BYTE_COUNT;

    within_deadline(|| {
        let socket_address = build_test_socket_address("oversize");
        let (server_thread, connection_accepted_rx) = serve_connection_and_announce(
            &socket_address,
            ConnectionToken::from_secret("secret"),
            || true,
        );

        let mut caller_connection =
            std::os::unix::net::UnixStream::connect(&socket_address).expect("connect");
        connection_accepted_rx
            .recv()
            .expect("the server accepted the caller");
        caller_connection
            .write_all(&(MAX_FRAME_BYTE_COUNT + 1).to_be_bytes())
            .expect("write an oversize length prefix");

        let mut response_bytes = Vec::new();
        caller_connection
            .read_to_end(&mut response_bytes)
            .expect("read until the server closes");
        drop(caller_connection);

        assert_eq!(response_bytes, Vec::<u8>::new());
        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![RequestDisposition::Stop]
        );
    });
}

#[test]
fn an_attach_carries_its_payload_through_to_the_callers_dispatch() {
    within_deadline(|| {
        // `next_request` hands an Attach to dispatch whole, payload included.
        let socket_address = build_test_socket_address("attach");
        let connection_token = ConnectionToken::from_secret("secret");
        let server_thread = run_test_request_loop(&socket_address, connection_token.clone());

        let attach_viewport = Size {
            column_count: 80,
            row_count: 24,
        };
        let mut caller_connection = Connection::connect(&socket_address).expect("connect");
        caller_connection
            .send(&build_test_hello_request(1, connection_token))
            .expect("send hello");
        let _: IpcResponse = caller_connection.recv().expect("read the hello answer");
        caller_connection
            .send(&IpcRequest {
                request_id: 2,
                request_kind: IpcRequestKind::Attach {
                    viewport: attach_viewport,
                    event_filter: EventFilterSpec::All,
                    resume_client_id: None,
                    resume_token: None,
                    pane_area: None,
                    graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
                    cell_size: None,
                },
            })
            .expect("send attach");
        drop(caller_connection);

        assert_eq!(
            server_thread.join().expect("server ends"),
            vec![
                RequestDisposition::Answered,
                RequestDisposition::Dispatch {
                    request_id: 2,
                    request_kind: IpcRequestKind::Attach {
                        viewport: attach_viewport,
                        event_filter: EventFilterSpec::All,
                        resume_client_id: None,
                        resume_token: None,
                        pane_area: None,
                        graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
                        cell_size: None,
                    },
                },
                RequestDisposition::Stop,
            ]
        );
    });
}
