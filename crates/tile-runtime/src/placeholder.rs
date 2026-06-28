//! Forward-declared stand-ins the runtime holds before their real subsystems
//! exist. Each is replaced in place once its owning crate ships the concrete
//! type; the runtime field and these names change together at that point.

/// Event fan-out hub. Stand-in until the bounded subscriber queue is built.
#[derive(Debug)]
pub struct EventBus;

/// Control-socket server handle. Stand-in until the IPC layer is built.
#[derive(Debug)]
pub struct IpcServer;

/// Source of render snapshots for attach and overflow resync. Stand-in until
/// the snapshot type is built.
pub trait SnapshotProvider {}

/// Session persistence backend. Stand-in until the storage layer is built.
pub trait Storage {}
