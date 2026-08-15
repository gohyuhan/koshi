//! `koshi-ipc` — control channel: local socket/named pipe transport, versioned IPC
//! messages, ownership checks, and CLI-to-session command forwarding.

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
/// Transport layer.
pub mod transport;
/// Shared types.
pub mod types;
/// Socket-address trust checks and stale-socket reclaim.
pub mod validate;
/// Reading a message whose variant this build may not have.
pub mod wire;
