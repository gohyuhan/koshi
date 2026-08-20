//! Stand-ins the runtime holds where a subsystem has no concrete type wired
//! yet. Each trait here carries no methods, so nothing calls through them.

/// Source of render snapshots for attach.
pub trait SnapshotProvider {}

/// Session persistence backend.
pub trait Storage {}

/// A [`SnapshotProvider`] that holds nothing.
/// [`Server::resume`](crate::server::Server::resume) builds on it, and tests
/// use it when they build a runtime without exercising snapshots.
pub struct NullSnapshotProvider;
impl SnapshotProvider for NullSnapshotProvider {}

/// A [`Storage`] that persists nothing.
/// [`Server::resume`](crate::server::Server::resume) builds on it, and tests
/// use it when they build a runtime without exercising persistence.
pub struct NullStorage;
impl Storage for NullStorage {}
