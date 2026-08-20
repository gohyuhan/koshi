//! `koshi-ipc` — the messages koshi's processes send each other, and the links
//! those messages travel on.
//!
//! Three request protocols share one frame shape — a 4-byte big-endian length,
//! then that many bytes of JSON: a client to a session server, a client to the
//! router, and a session server to the supervisor holding its panes. Each opens
//! with a Hello that settles the version both ends speak and presents a secret,
//! and each refuses a request kind this build has no name for without closing
//! the link.
//!
//! A link on this machine is a Unix socket or a Windows named pipe. A link from
//! another machine is TLS, and the dialling side pins the certificate the server
//! presented on the first connection.
//!
//! The crate also holds the records these processes keep on disk: the endpoint
//! file a session advertises its socket in, the remote access token store, the
//! servers a dialling user has saved, this machine's own certificate, and the
//! record that the operator switched remote access on.

/// The session structure a client receives when it attaches.
pub mod attach;
/// Bytes on the wire: one base64 string per byte payload.
pub mod bytes;
/// Endpoint file: how a running koshi advertises its socket and token.
pub mod endpoint;
/// Error types.
pub mod error;
/// The events an attached client receives after the attach reply.
pub mod event;
/// One painted frame: the pane content a client draws.
pub mod frame;
/// Connection handshake checks.
pub mod handshake;
/// One session's layout: each tab's split tree and the rectangles it solves to.
pub mod layout;
/// What every server does the same way, on whichever protocol it speaks: the
/// framing faults, the unknown request kind, and the Hello.
pub mod plane;
pub mod protocol;
/// The servers a dialling user has connected to: the address, the secret, the
/// pinned certificate fingerprint, and the name the user chose for each.
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
/// the certificate pinning that recognises a server on the second connection.
pub mod tls;
/// Framed messages over a local socket or named pipe, and the same frame shape
/// on any other pair of byte streams.
pub mod transport;
/// Socket-address trust checks and stale-socket reclaim.
pub mod validate;
/// Reading a message whose variant this build may not have.
pub mod wire;
