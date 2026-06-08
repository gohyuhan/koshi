//! `tile-plugin-api` — guest-facing plugin SDK: ABI DTOs, event subscription
//! types, command request types, and capability definitions. Must NOT depend on
//! `wasmtime`.

pub mod error;
pub mod types;

pub mod api;
