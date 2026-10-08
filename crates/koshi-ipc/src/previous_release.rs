//! The session wire that koshi 0.2.0 to 0.4.0 speaks, cut down to the Hello
//! and the Restart that move a session such a release started to this build.
//!
//! A request travels as `{"request_id": …, "kind": …}` and an answer as
//! `{"request_id": …, "result": …}`. An error code is written in snake case,
//! such as `unsupported_kind`.
//!
//! Example — the Hello this build sends such a session is
//! `{"request_id":1,"kind":{"Hello":{"min_protocol_version":2,"max_protocol_version":3,"token":"<token>"}}}`,
//! and a koshi 0.4.0 session answers
//! `{"request_id":1,"result":{"Hello":{"protocol_version":3,"version":"0.4.0"}}}`.

use serde::{Deserialize, Serialize};

use crate::protocol::ConnectionToken;

#[cfg(test)]
mod tests;

/// The lowest session protocol version of koshi 0.2.0 to 0.4.0: koshi 0.2.0
/// and 0.3.0 speak version 2.
const PREVIOUS_RELEASE_MINIMUM_SESSION_PROTOCOL_VERSION: u32 = 2;

/// The highest session protocol version of koshi 0.2.0 to 0.4.0: koshi 0.4.0
/// speaks version 3.
const PREVIOUS_RELEASE_MAXIMUM_SESSION_PROTOCOL_VERSION: u32 = 3;

/// One request to a session that koshi 0.2.0 to 0.4.0 started.
#[derive(Debug, Serialize)]
pub struct PreviousReleaseRequest {
    /// The id the session repeats in its answer.
    pub request_id: u64,
    /// What is asked.
    #[serde(rename = "kind")]
    pub request_kind: PreviousReleaseRequestKind,
}

/// What a request to a session of koshi 0.2.0 to 0.4.0 asks.
#[derive(Debug, Serialize)]
pub enum PreviousReleaseRequestKind {
    /// Opens the connection: the range of session protocol versions the caller
    /// speaks, and the token the session's endpoint file carries.
    Hello {
        /// The lowest session protocol version the caller speaks.
        #[serde(rename = "min_protocol_version")]
        minimum_protocol_version: u32,
        /// The highest session protocol version the caller speaks.
        #[serde(rename = "max_protocol_version")]
        maximum_protocol_version: u32,
        /// The token the session's endpoint file carries.
        #[serde(rename = "token")]
        connection_token: ConnectionToken,
    },
    /// Asks the session to answer, then replace its process image with the
    /// program file it started from. koshi 0.2.0 has no such request.
    Restart,
}

impl PreviousReleaseRequestKind {
    /// The Hello presenting `connection_token`, for session protocol versions
    /// 2 to 3: every version koshi 0.2.0 to 0.4.0 speaks.
    #[must_use]
    pub fn build_hello_request(connection_token: ConnectionToken) -> Self {
        Self::Hello {
            minimum_protocol_version: PREVIOUS_RELEASE_MINIMUM_SESSION_PROTOCOL_VERSION,
            maximum_protocol_version: PREVIOUS_RELEASE_MAXIMUM_SESSION_PROTOCOL_VERSION,
            connection_token,
        }
    }
}

/// One answer from a session that koshi 0.2.0 to 0.4.0 started. The
/// `request_id` beside the answer is not read. Such a session answers the
/// requests of one connection in the order they arrived.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PreviousReleaseAnswer {
    /// The answer itself.
    #[serde(rename = "result")]
    pub answer_result: PreviousReleaseResult,
}

/// What a session of koshi 0.2.0 to 0.4.0 answered.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum PreviousReleaseResult {
    /// Answers the Hello: the connection is open.
    Hello {
        /// The build the session runs, such as `0.4.0`. Empty from koshi
        /// 0.2.0, which names no build.
        #[serde(default, rename = "version")]
        build_version: String,
    },
    /// Answers the Restart: the session replaces its process image next.
    Restarting,
    /// The request was refused.
    Error(PreviousReleaseErrorPayload),
}

/// Why a session of koshi 0.2.0 to 0.4.0 refused a request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PreviousReleaseErrorPayload {
    /// The refusal, as a value a caller can branch on.
    pub code: PreviousReleaseErrorCode,
    /// The sentence the session wrote, such as `this Koshi has no request kind
    /// named Restart`.
    pub message: String,
}

/// Every refusal code koshi 0.2.0 to 0.4.0 and their pre-releases send,
/// written in snake case on the wire, such as `bad_token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviousReleaseErrorCode {
    /// The token presented does not match the session's.
    BadToken,
    /// The session speaks none of the protocol versions the caller named.
    UnsupportedVersion,
    /// The session has no request kind by the name the caller sent.
    UnsupportedKind,
    /// The session could not read the request.
    MalformedRequest,
    /// The caller named a target the session does not have.
    NotFound,
    /// A request arrived before a Hello opened the connection.
    HelloRequired,
    /// The caller is another user of the machine, and the session serves only
    /// the user who started it.
    OtherUsersOff,
    /// The code the session gives a refusal it has no name for.
    Unknown,
}
