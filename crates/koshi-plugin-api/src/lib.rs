//! Plugin SDK for koshi. A plugin depends on this crate.
//!
//! Scope: the ABI data types, the event subscription types, the command
//! request types, and the capability definitions. Every module is empty.
//!
//! `cargo xtask dep-guard` fails if this crate declares `wasmtime`,
//! `koshi-client`, or `koshi-renderer` as a direct dependency.

pub mod error;

pub mod types;

pub mod api;
