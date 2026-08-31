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
const BUILD: &str = "9.9.9";

/// A socket address of this crate's own, named for the test that binds it.
fn test_addr(tag: &str) -> String {
    let unique = format!("koshi-plane-{}-{tag}", std::process::id());
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

/// A gate for a same-user local connection expecting `token`.
fn gate(token: ConnectionToken) -> Handshake {
    Handshake::new(
        token,
        Peer::Local {
            same_user: true,
            other_users_allowed: false,
        },
    )
}

/// Serve one connection at `addr`, running `next_request` until it says stop,
/// and hand back every outcome it produced in order.
///
/// The dispatch arm answers nothing: this module owns the decisions before
/// dispatch, so a `Dispatch` outcome is recorded and the loop moves on.
fn outcomes(addr: &str, token: ConnectionToken) -> thread::JoinHandle<Vec<Next<IpcRequestKind>>> {
    served(addr, token, || true)
}

/// The same, with `admitted` deciding whether each arrived request is served.
fn served(
    addr: &str,
    token: ConnectionToken,
    admitted: impl Fn() -> bool + Send + 'static,
) -> thread::JoinHandle<Vec<Next<IpcRequestKind>>> {
    let (server, accepted) = served_announcing(addr, token, admitted);
    drop(accepted);
    server
}

/// The same again, and it says when it has the connection.
///
/// The receiver yields once, right after `accept` returns. On Windows a caller
/// that connects and drops before it is accepted occupies the pipe until the
/// next `accept`, and this helper accepts once: a test that drops its caller
/// without sending anything waits for the receiver first.
fn served_announcing(
    addr: &str,
    token: ConnectionToken,
    admitted: impl Fn() -> bool + Send + 'static,
) -> (
    thread::JoinHandle<Vec<Next<IpcRequestKind>>>,
    mpsc::Receiver<()>,
) {
    let listener = Listener::bind(addr).expect("bind the test listener");
    let (announce, accepted) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let _ = announce.send(());
        let mut gate = gate(token);
        let mut seen = Vec::new();
        loop {
            let next = next_request::<SessionPlane>(&mut connection, &mut gate, BUILD, &admitted);
            let stop = next == Next::Stop;
            seen.push(next);
            if stop {
                return seen;
            }
        }
    });
    (server, accepted)
}

/// How long a test waits for its body to finish before calling the server
/// broken.
const ANSWER_WAIT: Duration = Duration::from_secs(10);

/// Run one test's body on a worker thread. A body that has not finished
/// within [`ANSWER_WAIT`] fails with a message naming the wait; a body that
/// panics fails with its own message.
///
/// [`Connection`] has no read deadline: a server that stops answering blocks
/// the body on a socket read, and the deadline turns that into a failure.
fn within_deadline(body: impl FnOnce() + Send + 'static) {
    let (done, finished) = mpsc::channel();
    let worker = thread::spawn(move || {
        body();
        let _ = done.send(());
    });
    match finished.recv_timeout(ANSWER_WAIT) {
        Ok(()) => worker.join().expect("the test body finished"),
        // The body panicked, so its own message is this test's failure.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            worker.join().expect("the test body panicked");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("the server did not answer within {ANSWER_WAIT:?}")
        }
    }
}

/// The Hello this build's caller opens with.
fn hello(request_id: u64, token: ConnectionToken) -> IpcRequest {
    IpcRequest {
        request_id,
        kind: IpcRequestKind::hello(token),
    }
}

#[test]
fn a_hello_is_answered_here_with_the_settled_version_and_the_build() {
    within_deadline(|| {
        let addr = test_addr("hello");
        let token = ConnectionToken::new("secret");
        let server = outcomes(&addr, token.clone());

        let mut caller = Connection::connect(&addr).expect("connect");
        caller.send(&hello(1, token)).expect("send hello");
        let answer: IpcResponse = caller.recv().expect("read the hello answer");
        drop(caller);

        assert_eq!(
            answer,
            IpcResponse {
                request_id: Some(1),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: BUILD.to_string(),
                },
            }
        );
        assert_eq!(
            server.join().expect("server ends"),
            vec![Next::Answered, Next::Stop]
        );
    });
}

#[test]
fn a_request_after_the_hello_is_handed_to_the_callers_dispatch() {
    within_deadline(|| {
        let addr = test_addr("dispatch");
        let token = ConnectionToken::new("secret");
        let server = outcomes(&addr, token.clone());

        let mut caller = Connection::connect(&addr).expect("connect");
        caller.send(&hello(1, token)).expect("send hello");
        let _: IpcResponse = caller.recv().expect("read the hello answer");
        caller
            .send(&IpcRequest {
                request_id: 2,
                kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery");
        drop(caller);

        assert_eq!(
            server.join().expect("server ends"),
            vec![
                Next::Answered,
                Next::Dispatch {
                    request_id: 2,
                    kind: IpcRequestKind::Discovery,
                },
                Next::Stop,
            ]
        );
    });
}

#[test]
fn a_request_before_the_hello_is_refused_here_and_the_connection_keeps_serving() {
    within_deadline(|| {
        let addr = test_addr("gated");
        let token = ConnectionToken::new("secret");
        let server = outcomes(&addr, token.clone());

        let mut caller = Connection::connect(&addr).expect("connect");
        caller
            .send(&IpcRequest {
                request_id: 1,
                kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery first");
        let refusal: IpcResponse = caller.recv().expect("read the refusal");
        // The connection is still open, so the Hello that follows is served.
        caller.send(&hello(2, token)).expect("send hello");
        let accepted: IpcResponse = caller.recv().expect("read the hello answer");
        drop(caller);

        assert_eq!(
            refusal,
            IpcResponse {
                request_id: Some(1),
                result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::HelloRequired,
                    message: "Discovery arrived before a Hello opened the connection".to_string(),
                }),
            }
        );
        assert_eq!(
            accepted.result,
            IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION,
                version: BUILD.to_string(),
            }
        );
        assert_eq!(
            server.join().expect("server ends"),
            vec![Next::Answered, Next::Answered, Next::Stop]
        );
    });
}

#[test]
fn a_kind_this_build_does_not_have_is_refused_by_name_and_the_connection_keeps_serving() {
    within_deadline(|| {
        let addr = test_addr("unknown-kind");
        let token = ConnectionToken::new("secret");
        let server = outcomes(&addr, token.clone());

        let mut caller = Connection::connect(&addr).expect("connect");
        caller.send(&hello(1, token)).expect("send hello");
        let _: IpcResponse = caller.recv().expect("read the hello answer");
        // A newer koshi's verb, spelled straight onto the wire.
        caller
            .send(&serde_json::json!({"request_id": 2, "kind": {"Rehome": {"pane": 3}}}))
            .expect("send an unfamiliar kind");
        let refusal: IpcResponse = caller.recv().expect("read the refusal");
        // Still serving: the next familiar request is dispatched as usual.
        caller
            .send(&IpcRequest {
                request_id: 3,
                kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery");
        drop(caller);

        assert_eq!(
            refusal,
            IpcResponse {
                request_id: Some(2),
                result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::UnsupportedKind,
                    message: "this Koshi has no request kind named Rehome".to_string(),
                }),
            }
        );
        assert_eq!(
            server.join().expect("server ends"),
            vec![
                Next::Answered,
                Next::Answered,
                Next::Dispatch {
                    request_id: 3,
                    kind: IpcRequestKind::Discovery,
                },
                Next::Stop,
            ]
        );
    });
}

#[test]
fn a_frame_read_whole_but_unreadable_is_refused_and_the_connection_keeps_serving() {
    within_deadline(|| {
        let addr = test_addr("malformed");
        let token = ConnectionToken::new("secret");
        let server = outcomes(&addr, token.clone());

        let mut caller = Connection::connect(&addr).expect("connect");
        // Whole frame, aligned stream, bytes that are not a request.
        caller.send(&"not a request").expect("send junk");
        let refusal: IpcResponse = caller.recv().expect("read the refusal");
        caller.send(&hello(2, token)).expect("send hello");
        let accepted: IpcResponse = caller.recv().expect("read the hello answer");
        drop(caller);

        assert_eq!(
            refusal,
            IpcResponse {
                request_id: None,
                result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::MalformedRequest,
                    message: "the bytes received are not a request this build can read".to_string(),
                }),
            }
        );
        assert_eq!(
            accepted,
            IpcResponse {
                request_id: Some(2),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: BUILD.to_string(),
                },
            }
        );
        assert_eq!(
            server.join().expect("server ends"),
            vec![Next::Answered, Next::Answered, Next::Stop]
        );
    });
}

#[test]
fn a_kind_this_build_does_not_have_before_the_hello_is_refused_as_hello_required() {
    within_deadline(|| {
        let addr = test_addr("unknown-kind-gated");
        let token = ConnectionToken::new("secret");
        let server = outcomes(&addr, token.clone());

        let mut caller = Connection::connect(&addr).expect("connect");
        caller
            .send(&serde_json::json!({"request_id": 1, "kind": {"Rehome": {"pane": 3}}}))
            .expect("send an unfamiliar kind first");
        let refusal: IpcResponse = caller.recv().expect("read the refusal");
        caller.send(&hello(2, token)).expect("send hello");
        let accepted: IpcResponse = caller.recv().expect("read the hello answer");
        drop(caller);

        // A closed gate answers an unknown kind with `HelloRequired`, the
        // same as a known kind.
        assert_eq!(
            refusal,
            IpcResponse {
                request_id: Some(1),
                result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::HelloRequired,
                    message: "Rehome arrived before a Hello opened the connection".to_string(),
                }),
            }
        );
        assert_eq!(
            accepted,
            IpcResponse {
                request_id: Some(2),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: BUILD.to_string(),
                },
            }
        );
        assert_eq!(
            server.join().expect("server ends"),
            vec![Next::Answered, Next::Answered, Next::Stop]
        );
    });
}

#[test]
fn a_hello_naming_a_version_range_this_build_does_not_share_is_refused_here() {
    within_deadline(|| {
        let addr = test_addr("version");
        let token = ConnectionToken::new("secret");
        let server = outcomes(&addr, token.clone());

        let above = PROTOCOL_VERSION + 5;
        let mut caller = Connection::connect(&addr).expect("connect");
        caller
            .send(&IpcRequest {
                request_id: 1,
                kind: IpcRequestKind::Hello {
                    min_protocol_version: above,
                    max_protocol_version: above,
                    token,
                    remote: false,
                },
            })
            .expect("send a hello from the future");
        let refusal: IpcResponse = caller.recv().expect("read the refusal");
        drop(caller);

        assert_eq!(
            refusal,
            IpcResponse {
                request_id: Some(1),
                result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::UnsupportedVersion,
                    message: format!(
                        "the caller speaks protocol versions {above} to {above}, this Koshi speaks \
                         {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}"
                    ),
                }),
            }
        );
        assert_eq!(
            server.join().expect("server ends"),
            vec![Next::Answered, Next::Stop]
        );
    });
}

#[test]
fn a_hello_presenting_the_wrong_token_is_refused_here() {
    within_deadline(|| {
        let addr = test_addr("token");
        let server = outcomes(&addr, ConnectionToken::new("the real secret"));

        let mut caller = Connection::connect(&addr).expect("connect");
        caller
            .send(&hello(1, ConnectionToken::new("a guess")))
            .expect("send hello");
        let refusal: IpcResponse = caller.recv().expect("read the refusal");
        drop(caller);

        assert_eq!(
            refusal,
            IpcResponse {
                request_id: Some(1),
                result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::BadToken,
                    message: "the token presented does not match this Koshi's".to_string(),
                }),
            }
        );
        assert_eq!(
            server.join().expect("server ends"),
            vec![Next::Answered, Next::Stop]
        );
    });
}

#[test]
fn a_refused_hello_keeps_the_connection_serving_for_the_next_hello() {
    within_deadline(|| {
        let addr = test_addr("token-retry");
        let token = ConnectionToken::new("the real secret");
        let server = outcomes(&addr, token.clone());

        let mut caller = Connection::connect(&addr).expect("connect");
        caller
            .send(&hello(1, ConnectionToken::new("a guess")))
            .expect("send a wrong hello");
        let refusal: IpcResponse = caller.recv().expect("read the refusal");
        caller.send(&hello(2, token)).expect("send the right hello");
        let accepted: IpcResponse = caller.recv().expect("read the hello answer");
        drop(caller);

        assert_eq!(
            refusal,
            IpcResponse {
                request_id: Some(1),
                result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::BadToken,
                    message: "the token presented does not match this Koshi's".to_string(),
                }),
            }
        );
        assert_eq!(
            accepted,
            IpcResponse {
                request_id: Some(2),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: BUILD.to_string(),
                },
            }
        );
        assert_eq!(
            server.join().expect("server ends"),
            vec![Next::Answered, Next::Answered, Next::Stop]
        );
    });
}

#[test]
fn a_peer_no_longer_admitted_is_answered_nothing_at_all() {
    within_deadline(|| {
        // `admitted` is read after the Hello arrives: a peer whose access was
        // withdrawn while its connection sat open is not served that Hello.
        let addr = test_addr("withdrawn");
        let token = ConnectionToken::new("secret");
        let server = served(&addr, token.clone(), || false);

        let mut caller = Connection::connect(&addr).expect("connect");
        caller.send(&hello(1, token)).expect("send hello");
        let answer: Result<IpcResponse, _> = caller.recv();
        drop(caller);

        let Err(IpcError::Disconnected) = answer else {
            panic!("a withdrawn peer reads no answer, got {answer:?}");
        };
        assert_eq!(server.join().expect("server ends"), vec![Next::Stop]);
    });
}

#[test]
fn a_peer_whose_access_is_withdrawn_after_the_hello_is_answered_nothing_more() {
    within_deadline(|| {
        let addr = test_addr("withdrawn-after-hello");
        let token = ConnectionToken::new("secret");
        let still_on = Arc::new(AtomicBool::new(true));
        let admitted = Arc::clone(&still_on);
        let server = served(&addr, token.clone(), move || {
            admitted.load(Ordering::SeqCst)
        });

        let mut caller = Connection::connect(&addr).expect("connect");
        caller.send(&hello(1, token)).expect("send hello");
        let accepted: IpcResponse = caller.recv().expect("read the hello answer");
        // The setting turns off between one request and the next.
        still_on.store(false, Ordering::SeqCst);
        caller
            .send(&IpcRequest {
                request_id: 2,
                kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery");
        let answer: Result<IpcResponse, _> = caller.recv();
        drop(caller);

        assert_eq!(
            accepted,
            IpcResponse {
                request_id: Some(1),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: BUILD.to_string(),
                },
            }
        );
        let Err(IpcError::Disconnected) = answer else {
            panic!("a withdrawn peer reads no answer, got {answer:?}");
        };
        assert_eq!(
            server.join().expect("server ends"),
            vec![Next::Answered, Next::Stop]
        );
    });
}

#[test]
fn a_malformed_frame_is_answered_even_while_the_peer_is_not_admitted() {
    within_deadline(|| {
        // The malformed-frame answer goes out before `admitted` is read.
        let addr = test_addr("withdrawn-junk");
        let server = served(&addr, ConnectionToken::new("secret"), || false);

        let mut caller = Connection::connect(&addr).expect("connect");
        caller.send(&"not a request").expect("send junk");
        let refusal: IpcResponse = caller.recv().expect("read the refusal");
        drop(caller);

        assert_eq!(
            refusal,
            IpcResponse {
                request_id: None,
                result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::MalformedRequest,
                    message: "the bytes received are not a request this build can read".to_string(),
                }),
            }
        );
        assert_eq!(
            server.join().expect("server ends"),
            vec![Next::Answered, Next::Stop]
        );
    });
}

#[test]
fn a_peer_that_hangs_up_ends_the_connection() {
    within_deadline(|| {
        let addr = test_addr("hangup");
        let (server, accepted) = served_announcing(&addr, ConnectionToken::new("secret"), || true);

        let caller = Connection::connect(&addr).expect("connect");
        // Dropped once the server holds the connection: a peer going away
        // mid-serve.
        accepted.recv().expect("the server accepted the caller");
        drop(caller);

        assert_eq!(server.join().expect("server ends"), vec![Next::Stop]);
    });
}

/// A length prefix past [`MAX_FRAME_LEN`] ends the connection with nothing
/// written: the caller reads end of stream, and the server's payload is never
/// read. The prefix is written on the raw socket; [`Connection::send`]
/// refuses an oversize frame on the sending side.
#[cfg(unix)]
#[test]
fn an_oversize_length_prefix_ends_the_connection_with_nothing_written() {
    use std::io::{Read as _, Write as _};

    use crate::transport::MAX_FRAME_LEN;

    within_deadline(|| {
        let addr = test_addr("oversize");
        let (server, accepted) = served_announcing(&addr, ConnectionToken::new("secret"), || true);

        let mut caller = std::os::unix::net::UnixStream::connect(&addr).expect("connect");
        accepted.recv().expect("the server accepted the caller");
        caller
            .write_all(&(MAX_FRAME_LEN + 1).to_be_bytes())
            .expect("write an oversize length prefix");

        let mut answered = Vec::new();
        caller
            .read_to_end(&mut answered)
            .expect("read until the server closes");
        drop(caller);

        assert_eq!(answered, Vec::<u8>::new());
        assert_eq!(server.join().expect("server ends"), vec![Next::Stop]);
    });
}

#[test]
fn an_attach_carries_its_payload_through_to_the_callers_dispatch() {
    within_deadline(|| {
        // `next_request` hands an Attach to dispatch whole, payload included.
        let addr = test_addr("attach");
        let token = ConnectionToken::new("secret");
        let server = outcomes(&addr, token.clone());

        let viewport = Size { cols: 80, rows: 24 };
        let mut caller = Connection::connect(&addr).expect("connect");
        caller.send(&hello(1, token)).expect("send hello");
        let _: IpcResponse = caller.recv().expect("read the hello answer");
        caller
            .send(&IpcRequest {
                request_id: 2,
                kind: IpcRequestKind::Attach {
                    viewport,
                    filter: EventFilterSpec::All,
                    resume: None,
                    resume_token: None,
                    pane_area: None,
                },
            })
            .expect("send attach");
        drop(caller);

        assert_eq!(
            server.join().expect("server ends"),
            vec![
                Next::Answered,
                Next::Dispatch {
                    request_id: 2,
                    kind: IpcRequestKind::Attach {
                        viewport,
                        filter: EventFilterSpec::All,
                        resume: None,
                        resume_token: None,
                        pane_area: None,
                    },
                },
                Next::Stop,
            ]
        );
    });
}
