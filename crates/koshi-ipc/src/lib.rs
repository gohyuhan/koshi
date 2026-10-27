//! `koshi-ipc` — the messages koshi's processes send each other, and the links
//! those messages travel on.
//!
//! Three request protocols share one frame shape — a 4-byte big-endian length,
//! then that many bytes of JSON: a client to a session server, a client to the
//! router, and a session server to the supervisor holding its panes. A session
//! server also sends its attached clients binary frames: bit 31 of the length
//! marks one, and the first payload byte names its kind, `1` for a painted
//! update and `2` for an image chunk. Each protocol opens
//! with a Hello that settles the version both ends speak and presents a secret,
//! and each refuses a request kind this build has no name for without closing
//! the link.
//!
//! A link on this machine is a Unix socket or a Windows named pipe. A link from
//! another machine is TLS 1.3, and the dialling side refuses a server whose
//! certificate does not have the fingerprint the dial expects.
//!
//! The crate also holds the records these processes keep on disk: the endpoint
//! file a session advertises its socket in, the remote access token store, the
//! servers a dialling user has saved, this machine's own certificate, and the
//! record that the operator switched remote access on.

/// Bytes on the wire: one base64 string per byte payload.
pub mod bytes;
/// Endpoint file: how a running koshi advertises its socket and token.
pub mod endpoint;
/// Error types.
pub mod error;
/// The events an attached client receives after the attach reply.
pub mod event;
/// One painted frame: the structure of the picture a client draws.
pub mod frame;
/// Connection handshake checks.
pub mod handshake;
/// The binary frames that carry the raw RGBA bytes of each image record.
pub mod image_chunk;
/// One session's layout: each tab's split tree and the rectangles it solves to,
/// and each floating pane with what each attached client draws for it.
pub mod layout;
/// The binary frames that carry each attached client's screen changes.
pub mod painted_update;
/// Bounded read-only placement previews for attached clients.
pub mod placement;
/// What every server does the same way, on whichever protocol it speaks: the
/// framing faults, the unknown request kind, and the Hello.
pub mod plane;
/// The session wire of koshi 0.2.0 to 0.4.0, cut down to the Hello and the
/// Restart that move a session such a release started to this build.
pub mod previous_release;
/// Wire messages between a client and a session server: the requests, the
/// answer that repeats a request's `request_id`, and the Hello that opens a
/// connection.
pub mod protocol;
/// One-way conversion of remote access files written by version 0.4.0.
pub mod remote_migration;
/// The servers a dialling user has saved: the address, the secret, the
/// certificate fingerprint, and the name the user chose for each.
pub mod remote_servers;
/// The certificate this machine presents to remote clients, and the record
/// that the operator switched remote access on.
pub mod remote_state;
/// The machine's remote access tokens: what a grant records, where it is
/// stored, and what a presented token reaches.
pub mod remote_tokens;
/// What a remote client and the machine serving it say to each other before
/// any session is reached.
pub mod remote_wire;
/// The control-plane protocol: the messages that create, find, list and kill
/// sessions, the handshake that opens a router connection, and the fixed
/// names the router serves under.
pub mod router;
/// The pane-supervisor protocol: the messages a session server drives the
/// process holding its panes with, the events that process sends back, the
/// handshake that opens the link, and the address it listens on.
pub mod supervisor;
/// The TLS stream a remote client and the machine serving it talk over, and
/// the check that the server presents the certificate a dial expects.
pub mod tls;
/// Framed messages over a local socket or named pipe, and the same frame shape
/// on any other pair of byte streams.
pub mod transport;
/// Socket-address trust checks and stale-socket reclaim.
pub mod validate;
/// Reading a message whose variant this build may not have.
pub mod wire;
