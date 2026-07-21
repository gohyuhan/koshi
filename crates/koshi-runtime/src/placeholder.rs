//! Forward-declared stand-ins the runtime holds before their real subsystems
//! exist. Each is replaced in place once its owning crate ships the concrete
//! type; the runtime field and these names change together at that point.

/// Control-socket server handle. Stand-in until the IPC layer is built.
#[derive(Debug)]
pub struct IpcServer;

/// Source of render snapshots for attach and overflow resync. Stand-in until
/// the snapshot type is built.
pub trait SnapshotProvider {}

/// Session persistence backend. Stand-in until the storage layer is built.
pub trait Storage {}

/// A [`SnapshotProvider`] that holds nothing — the stock service until a real
/// snapshot source exists, and the one tests use when they build a runtime
/// without exercising snapshots.
pub struct NullSnapshotProvider;
impl SnapshotProvider for NullSnapshotProvider {}

/// A [`Storage`] that persists nothing — the stock service until a real storage
/// layer exists, and the one tests use when they build a runtime without
/// exercising persistence.
pub struct NullStorage;
impl Storage for NullStorage {}
