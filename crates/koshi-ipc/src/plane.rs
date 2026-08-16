//! What every server does the same way, on whichever protocol it speaks.
//!
//! koshi runs more than one request protocol over the same framing: a session
//! server answers its session's control socket, and the router answers the
//! control socket. Four decisions are the same on both, and were written out
//! once per server before this module existed:
//!
//! 1. A frame that arrives whole but cannot be read is answered with
//!    [`MalformedRequest`](crate::protocol::IpcErrorCode::MalformedRequest), and the connection
//!    keeps serving — the stream is still on a frame boundary.
//! 2. A frame whose payload could not even be read leaves the stream off its
//!    boundaries, and that one connection closes. Disconnects and transport
//!    faults land here too, since they leave no stream at all.
//! 3. A request kind this build does not have comes from a newer koshi. It is
//!    refused by name and the connection keeps serving, so one unfamiliar verb
//!    does not cost the caller its other verbs.
//! 4. A Hello is answered with the version the two sides settled on; every
//!    other kind is refused until one has opened the gate.
//!
//! The [`next_request`](crate::plane::next_request) function makes those four decisions and hands back what is left:
//! either a checked request for the caller's own dispatch, or the news that
//! this connection is finished. What a request *means* stays with the caller,
//! which is where the two servers genuinely differ — the router reports a
//! delivered `Restarting`, and the session server hands an answered `Attach`
//! to its event stream.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::IpcError;
use crate::protocol::{IpcErrorCode, IpcErrorPayload};
use crate::transport::Connection;
use crate::wire::{Answer, Envelope, MaybeKnown, WireVariants};

/// One connection's handshake gate, whichever protocol it is for.
///
/// The rule itself is one shared version gate; this names the three questions
/// a serve loop asks it, so one loop can drive any protocol's gate.
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

    /// Whether `kind` is the Hello that opens a connection.
    ///
    /// The serve loop answers a Hello itself, since the answer is the settled
    /// version and nothing else.
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

    /// The answer to an accepted Hello: the version both sides settled on,
    /// and `build` — the answering program's own version, e.g. `"0.3.0"`.
    ///
    /// `build` comes from the server rather than from here, so each server
    /// reports the version of the binary it is, not the version of this
    /// crate. A protocol whose Hello carries no build version ignores it.
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
    /// frame boundaries, or a write failed. The caller returns.
    Stop,
}

/// Read one request, make every decision that is the same on every protocol,
/// and hand back what is left.
///
/// The four decisions are the ones the module doc lists. Anything this
/// function answers itself is [`Next::Answered`]; anything it cannot decide
/// is [`Next::Dispatch`]; anything that ends the connection is [`Next::Stop`].
///
/// `admitted` is asked once a request has arrived and before any answer goes
/// out: `false` ends the connection with nothing written. A server whose peer
/// may lose access while its connection sits open reads that access here, so
/// the answer reflects the setting as it stands now. A server whose peers
/// cannot lose access passes [`always_admitted`].
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
            // The frame was read whole, so the stream is still aligned; only
            // its bytes were unreadable. `request_id: None` tells the caller
            // the answer belongs to no request of its own.
            let refusal = P::refusal(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the bytes received are not a request this build can read".to_string(),
            });
            return answer::<P>(connection, None, refusal);
        }
        // An oversize frame's payload was never read, so the stream's framing
        // is lost; disconnects and transport faults have no stream left. All
        // close this one connection.
        Err(_) => return Next::Stop,
    };

    // Asked once a request has arrived and before any answer is written, so a
    // peer whose admission was withdrawn while its connection sat open is not
    // served the request it just sent. A malformed frame is answered above
    // without asking, since that answer names no session state.
    if !admitted() {
        return Next::Stop;
    }

    let request_id = request.request_id;
    // A kind this build does not have comes from a newer koshi.
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

/// The admission answer for a server whose peers cannot lose access while
/// their connection is open: everyone who got in stays in.
///
/// Pass it as [`next_request`]'s `admitted` argument. The router uses it —
/// only the user who started the router can reach its socket at all.
#[must_use]
pub fn always_admitted() -> bool {
    true
}

/// Send one answer this module built itself. A write that fails means the peer
/// is gone, which ends the connection.
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
