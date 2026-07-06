//! `koshi-plugin-api` — guest-facing plugin SDK: ABI DTOs, event subscription
//! types, command request types, and capability definitions. Must NOT depend on
//! `wasmtime`.

/// Error types.
pub mod error;

/// Shared types.
pub mod types;

/// API surface.
pub mod api;
