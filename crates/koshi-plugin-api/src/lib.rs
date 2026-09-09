//! Plugin SDK for koshi. A plugin depends on this crate.
//!
//! Scope: ABI data types, event subscription types, command request types, and
//! capability definitions. All modules are empty.
//!
//! `cargo xtask dep-guard` fails if this crate declares any of `wasmtime`,
//! `koshi-client`, or `koshi-renderer` as a direct dependency.

pub mod error;

pub mod types;

pub mod api;
