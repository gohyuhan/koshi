//! Plugin SDK for koshi. A plugin uses this crate.
//!
//! This crate is empty. It will hold the ABI data types, the event
//! subscription types, the command request types, and the capability
//! definitions.
//!
//! This crate must not depend on `wasmtime`.

/// Error types.
pub mod error;

/// Shared types.
pub mod types;

/// API surface.
pub mod api;
