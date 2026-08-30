//! What every server does the same way, on whichever protocol it speaks.
//!
//! The session server and the router run different request protocols over
//! the same framing. Four decisions are the same on both:
//!
//! 1. A frame that arrives whole but cannot be read is answered with
//!    [`MalformedRequest`](crate::protocol::IpcErrorCode::MalformedRequest),
//!    and the connection keeps serving: the stream is still on a frame
//!    boundary.
//! 2. A frame whose payload was not read leaves the stream off its frame
//!    boundaries, and that one connection closes. A disconnect and a
//!    transport fault close it the same way.
//! 3. A request kind this build does not have is refused by name, and the
//!    connection keeps serving.
//! 4. A Hello is answered with the version the two sides settled on. Every
//!    other kind is refused until a Hello has opened the gate.
//!
//! [`next_request`](crate::plane::next_request) makes those four decisions
//! and hands back what is left: a checked request for the caller's own
//! dispatch, or the news that this connection is finished. What a request
//! means stays with the caller: the router reports a delivered `Restarting`,
//! and the session server hands an answered `Attach` to its event stream.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::IpcError;
use crate::protocol::{IpcErrorCode, IpcErrorPayload};
use crate::transport::Connection;
use crate::wire::{Answer, Envelope, MaybeKnown, WireVariants};

/// One connection's handshake gate, on any protocol: what a serve loop asks
/// it.
///
/// A `check` that accepts a Hello leaves `agreed` as `Some`.
pub trait Gate {
    /// The request kind this protocol carries.
    type Kind;

    /// The protocol version this connection settled on, or `None` while no
    /// Hello has been accepted.
    fn agreed(&self) -> Option<u32>;

    /// The refusal for a request kind this build does not have, named `name`.
    fn refuse_unknown(&self, name: &str) -> IpcErrorPayload;

    /// Check one incoming request kind against the connection's state.
    ///
    /// `Ok(())` means the caller serves the request. An `Err` carries the
    /// refusal to send back, and the gate keeps the state it had.
    fn check(&mut self, kind: &Self::Kind) -> Result<(), IpcErrorPayload>;

    /// Whether `kind` is the Hello that opens a connection. [`next_request`]
    /// answers a Hello itself, with the settled version.
    fn is_hello(kind: &Self::Kind) -> bool;
}

/// One request protocol: the vocabulary a [`Gate`] guards, and the two answers
/// a serve loop builds without asking the caller.
pub trait Plane {
    /// What a caller may ask for.
    type Kind: DeserializeOwned + WireVariants;
    /// What an answer looks like.
    type Result: Serialize;
    /// This protocol's handshake gate.
    type Gate: Gate<Kind = Self::Kind>;

    /// The refusal `payload` travels back as.
    fn refusal(payload: IpcErrorPayload) -> Self::Result;

    /// The answer to an accepted Hello. `agreed` is the version both sides
    /// settled on. `build` is the answering program's own version, e.g.
    /// `"0.3.0"`.
    fn hello(agreed: u32, build: &str) -> Self::Result;
}

/// What a serve loop does next, after [`next_request`] has made every decision
/// that is the same on every protocol.
#[derive(Debug, PartialEq, Eq)]
pub enum Next<K> {
    /// The request was answered here. Read the next one.
    Answered,
    /// A request the caller's own dispatch decides, already checked by the
    /// gate, with its `request_id` for the answer.
    Dispatch {
        /// The `request_id` to repeat in the answer.
        request_id: u64,
        /// What is being asked.
        kind: K,
    },
    /// This connection is finished: the peer hung up, the stream lost its
    /// frame boundaries, a write failed, or `admitted` answered `false`. The
    /// caller returns.
    Stop,
}

/// Read one request, make every decision that is the same on every protocol,
/// and hand back what is left.
///
/// The four decisions are the ones the module doc lists. What this function
/// answers itself is [`Next::Answered`]; what it cannot decide is
/// [`Next::Dispatch`]; what ends the connection is [`Next::Stop`].
///
/// `admitted` is asked after a request decodes and before its answer is
/// written: `false` ends the connection with nothing written for that
/// request. A malformed frame is answered before `admitted` is asked. The
/// session server passes the live read of `allow-other-users` for a
/// connection from another local user; a server whose peers cannot lose
/// access passes [`always_admitted`].
///
/// `build` is the answering program's own version, repeated in the Hello
/// answer.
///
/// # Panics
///
/// When the gate accepts a Hello and [`Gate::agreed`] still returns `None`.
///
/// Example — a caller that sends `{"request_id":4,"kind":"Discovery"}` on an
/// open connection gets `Next::Dispatch { request_id: 4, kind: Discovery }`,
/// and the same bytes before any Hello are answered here with
/// [`HelloRequired`](crate::protocol::IpcErrorCode::HelloRequired) and read as
/// `Next::Answered`.
pub fn next_request<P: Plane>(
    connection: &mut Connection,
    gate: &mut P::Gate,
    build: &str,
    admitted: &dyn Fn() -> bool,
) -> Next<P::Kind> {
    let request: Envelope<MaybeKnown<P::Kind>> = match connection.recv() {
        Ok(request) => request,
        Err(IpcError::MalformedFrame { .. }) => {
            // The frame arrived whole and its bytes did not decode. The answer
            // carries `request_id: None`, and the connection keeps serving.
            let refusal = P::refusal(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the bytes received are not a request this build can read".to_string(),
            });
            return answer::<P>(connection, None, refusal);
        }
        // An oversize frame leaves its payload unread and the stream off its
        // frame boundaries; a disconnect and a transport fault leave no
        // stream. All three close this connection.
        Err(_) => return Next::Stop,
    };

    // Asked once the request has arrived and before any answer is written. The
    // malformed-frame answer above is written without asking.
    if !admitted() {
        return Next::Stop;
    }

    let request_id = request.request_id;
    let kind = match request.kind {
        MaybeKnown::Known(kind) => kind,
        MaybeKnown::Unknown { name } => {
            let refusal = P::refusal(gate.refuse_unknown(&name));
            return answer::<P>(connection, Some(request_id), refusal);
        }
    };

    if let Err(refusal) = gate.check(&kind) {
        return answer::<P>(connection, Some(request_id), P::refusal(refusal));
    }

    if P::Gate::is_hello(&kind) {
        let agreed = gate
            .agreed()
            .expect("an accepted Hello settles the connection's version");
        return answer::<P>(connection, Some(request_id), P::hello(agreed, build));
    }

    Next::Dispatch { request_id, kind }
}

/// Always `true`: the `admitted` argument to [`next_request`] for a server
/// whose peers cannot lose access while their connection is open. The router
/// passes it.
#[must_use]
pub fn always_admitted() -> bool {
    true
}

/// Send one answer this module built itself: [`Next::Answered`] once the bytes
/// are written, and [`Next::Stop`] when the write fails.
fn answer<P: Plane>(
    connection: &mut Connection,
    request_id: Option<u64>,
    result: P::Result,
) -> Next<P::Kind> {
    let response = Answer { request_id, result };
    if connection.send(&response).is_err() {
        return Next::Stop;
    }
    Next::Answered
}

#[cfg(test)]
mod tests;
