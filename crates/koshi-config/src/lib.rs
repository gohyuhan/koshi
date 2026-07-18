//! `koshi-config` — koshi's configuration system.
//!
//! What lives here: the typed config schema and its built-in defaults
//! ([`types`]), folding user override layers onto them ([`layer`]), the
//! keybinding chord/sequence/leader syntax ([`key`], [`key_sequence`]),
//! keybinding files parsed into keymap layers ([`keybinding`]), conflict
//! detection across those layers ([`conflict`]), merging them into the
//! per-mode lookup tables ([`keymap_merge`]), layout files parsed into
//! templates ([`layout`]), and the config error types ([`error`]).
//! Discovering config files on disk, full validation, and migrating older
//! files forward belong to this system too.

pub mod conflict;
pub mod error;
pub mod key;
pub mod key_sequence;
pub mod keybinding;
pub mod keymap_merge;
pub mod layer;
pub mod layout;
pub mod parser;
pub mod types;

pub mod config;
