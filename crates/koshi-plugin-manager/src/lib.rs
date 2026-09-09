//! Plugin manager module boundaries.
//!
//! Public modules for errors and core types are empty. The public `manager`
//! module exposes empty command, event, and state modules. Its private `tests`
//! module is empty and compiled only for tests.
//!
//! This crate declares no dependencies on `koshi-runtime`, `koshi-ipc`, or
//! `koshi-plugin-host`; `cargo xtask dep-guard` rejects those direct dependencies.

pub mod error;
pub mod types;

pub mod manager;
