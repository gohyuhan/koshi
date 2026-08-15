//! Tests for the four decisions every server makes the same way.
//!
//! Each one runs over a real socket, the way every other transport test in
//! this crate does: a listener in the temp directory, one connected caller,
//! and [`next_request`] driven on the server's end.
//!
//! The session plane stands in for all of them. What is under test is the
//! shell, not any one protocol's vocabulary — the same four answers come back
//! whichever plane is plugged in.

use super::*;

use std::sync::mpsc;
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
/// The receiver yields once, right after `accept` returns. A test that means
/// to drop its caller without sending anything waits for that first: on
/// Windows a caller that connects and gives up before it is accepted occupies
/// the pipe until the next `accept` clears it, and this helper accepts once,
/// so the drop has to land on a connection the server already holds.
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

/// Run one test's body on a worker thread and fail if it has not finished
/// within [`ANSWER_WAIT`].
///
/// [`Connection`] has no read deadline, so a regression that stops the server
/// answering would block on a socket read for as long as the suite is allowed
/// to run. This turns that into a named failure instead of a run that never
/// finishes — verified by breaking the unknown-kind arm on purpose, which
/// hangs without this and fails in ten seconds with it.
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
        assert_eq!(accepted.request_id, Some(2));
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
fn a_peer_no_longer_admitted_is_answered_nothing_at_all() {
    within_deadline(|| {
        // The setting is read once a request has arrived, so a peer whose access
        // was withdrawn while its connection sat open is not served the Hello it
        // just sent.
        let addr = test_addr("withdrawn");
        let token = ConnectionToken::new("secret");
        let server = served(&addr, token.clone(), || false);

        let mut caller = Connection::connect(&addr).expect("connect");
        caller.send(&hello(1, token)).expect("send hello");
        let answer: Result<IpcResponse, _> = caller.recv();
        drop(caller);

        assert!(
            answer.is_err(),
            "a withdrawn peer reads no answer, got {answer:?}"
        );
        assert_eq!(server.join().expect("server ends"), vec![Next::Stop]);
    });
}

#[test]
fn a_malformed_frame_is_answered_even_while_the_peer_is_not_admitted() {
    within_deadline(|| {
        // That answer names no session state, so it goes out before the setting is
        // read — the same order the session server has always used.
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
        // Drop only once the server holds the connection, so this is a peer
        // going away mid-serve rather than one that gave up before it was
        // accepted — a case Windows treats differently.
        accepted.recv().expect("the server accepted the caller");
        drop(caller);

        assert_eq!(server.join().expect("server ends"), vec![Next::Stop]);
    });
}

#[test]
fn an_attach_carries_its_payload_through_to_the_callers_dispatch() {
    within_deadline(|| {
        // The one request whose answer the caller keeps the connection for. The
        // shell hands it over whole rather than answering it.
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
                    },
                },
                Next::Stop,
            ]
        );
    });
}
