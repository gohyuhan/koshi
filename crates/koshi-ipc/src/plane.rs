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
/// A request-kind validation that accepts a Hello leaves the agreed protocol
/// version as `Some`.
pub trait Gate {
    /// The request kind this protocol carries.
    type RequestKind;

    /// The protocol version this connection settled on, or `None` while no
    /// Hello has been accepted.
    fn get_agreed_protocol_version(&self) -> Option<u32>;

    /// The refusal for a request kind this build does not have, named `name`.
    fn build_unknown_request_kind_error(&self, request_kind_name: &str) -> IpcErrorPayload;

    /// Check one incoming request kind against the connection's state.
    ///
    /// `Ok(())` means the caller serves the request. An `Err` carries the
    /// refusal to send back, and the gate keeps the state it had.
    fn validate_request_kind(
        &mut self,
        request_kind: &Self::RequestKind,
    ) -> Result<(), IpcErrorPayload>;

    /// Whether `request_kind` is the Hello that opens a connection. [`next_request`]
    /// answers a Hello itself, with the settled version.
    fn is_hello(request_kind: &Self::RequestKind) -> bool;
}

/// One request protocol: the vocabulary a [`Gate`] guards, and the two answers
/// a serve loop builds without asking the caller.
pub trait Plane {
    /// What a caller may ask for.
    type RequestKind: DeserializeOwned + WireVariants;
    /// What an answer looks like.
    type Response: Serialize;
    /// This protocol's handshake gate.
    type Gate: Gate<RequestKind = Self::RequestKind>;

    /// The refusal `error_payload` travels back as.
    fn build_refusal_response(error_payload: IpcErrorPayload) -> Self::Response;

    /// The answer to an accepted Hello. `agreed_protocol_version` is the
    /// version both sides settled on. `build_version` is the answering
    /// program's own version, e.g.
    /// `"0.3.0"`.
    fn build_hello_response(agreed_protocol_version: u32, build_version: &str) -> Self::Response;
}

/// What a serve loop does next, after [`next_request`] has made every decision
/// that is the same on every protocol.
#[derive(Debug, PartialEq, Eq)]
pub enum RequestDisposition<RequestKind> {
    /// The request was answered here. Read the next one.
    Answered,
    /// A request the caller's own dispatch decides, already checked by the
    /// gate, with its `request_id` for the answer.
    Dispatch {
        /// The `request_id` to repeat in the answer.
        request_id: u64,
        /// What is being asked.
        request_kind: RequestKind,
    },
    /// This connection is finished: the peer hung up, the stream lost its
    /// frame boundaries, a write failed, or `is_admitted` answered `false`. The
    /// caller returns.
    Stop,
}

/// Read one request, make every decision that is the same on every protocol,
/// and hand back what is left.
///
/// The four decisions are the ones the module doc lists. What this function
/// answers itself is [`RequestDisposition::Answered`]; what it cannot decide is
/// [`RequestDisposition::Dispatch`]; what ends the connection is
/// [`RequestDisposition::Stop`].
///
/// `is_admitted` is asked after a request decodes and before its answer is
/// written: `false` ends the connection with nothing written for that
/// request. A malformed frame is answered before `is_admitted` is asked. The
/// session server passes the live read of `allow-other-users` for a
/// connection from another local user; a server whose peers cannot lose
/// access passes [`is_always_admitted`].
///
/// `build_version` is the answering program's own version, repeated in the Hello
/// answer.
///
/// # Panics
///
/// When the gate accepts a Hello and [`Gate::get_agreed_protocol_version`] still returns `None`.
///
/// Example — a caller that sends `{"request_id":4,"kind":"Discovery"}` on an
/// open connection gets `RequestDisposition::Dispatch { request_id: 4, kind: Discovery }`,
/// and the same bytes before any Hello are answered here with
/// [`HelloRequired`](crate::protocol::IpcErrorCode::HelloRequired) and read as
/// `RequestDisposition::Answered`.
pub fn next_request<Protocol: Plane>(
    connection: &mut Connection,
    gate: &mut Protocol::Gate,
    build_version: &str,
    is_admitted: &dyn Fn() -> bool,
) -> RequestDisposition<Protocol::RequestKind> {
    let incoming_request: Envelope<MaybeKnown<Protocol::RequestKind>> = match connection.recv() {
        Ok(incoming_request) => incoming_request,
        Err(IpcError::MalformedFrame { .. }) => {
            // The frame arrived whole and its bytes did not decode. The answer
            // carries `request_id: None`, and the connection keeps serving.
            let refusal = Protocol::build_refusal_response(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the bytes received are not a request this build can read".to_string(),
            });
            return send_answer::<Protocol>(connection, None, refusal);
        }
        // An oversize frame leaves its payload unread and the stream off its
        // frame boundaries; a disconnect and a transport fault leave no
        // stream. All three close this connection.
        Err(_) => return RequestDisposition::Stop,
    };

    // Asked once the request has arrived and before any answer is written. The
    // malformed-frame answer above is written without asking.
    if !is_admitted() {
        return RequestDisposition::Stop;
    }

    let request_id = incoming_request.request_id;
    let request_kind = match incoming_request.request_kind {
        MaybeKnown::Known(request_kind) => request_kind,
        MaybeKnown::Unknown { variant_name } => {
            let refusal = Protocol::build_refusal_response(
                gate.build_unknown_request_kind_error(&variant_name),
            );
            return send_answer::<Protocol>(connection, Some(request_id), refusal);
        }
    };

    if let Err(refusal) = gate.validate_request_kind(&request_kind) {
        return send_answer::<Protocol>(
            connection,
            Some(request_id),
            Protocol::build_refusal_response(refusal),
        );
    }

    if Protocol::Gate::is_hello(&request_kind) {
        let agreed_protocol_version = gate
            .get_agreed_protocol_version()
            .expect("an accepted Hello settles the connection's version");
        return send_answer::<Protocol>(
            connection,
            Some(request_id),
            Protocol::build_hello_response(agreed_protocol_version, build_version),
        );
    }

    RequestDisposition::Dispatch {
        request_id,
        request_kind,
    }
}

/// Always `true`: the `is_admitted` argument to [`next_request`] for a server
/// whose peers cannot lose access while their connection is open. The router
/// passes it.
#[must_use]
pub fn is_always_admitted() -> bool {
    true
}

/// Send one answer this module built itself: [`RequestDisposition::Answered`]
/// once the bytes are written, and [`RequestDisposition::Stop`] when the write
/// fails.
fn send_answer<Protocol: Plane>(
    connection: &mut Connection,
    request_id: Option<u64>,
    answer_payload: Protocol::Response,
) -> RequestDisposition<Protocol::RequestKind> {
    let outgoing_answer = Answer {
        request_id,
        answer_result: answer_payload,
    };
    if connection.send(&outgoing_answer).is_err() {
        return RequestDisposition::Stop;
    }
    RequestDisposition::Answered
}

#[cfg(test)]
mod tests;
