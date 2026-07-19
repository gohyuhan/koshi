//! `koshi-core` — shared dependency-light types: IDs, commands, events, geometry,
//! lifecycle states, protocol DTOs, input-privacy DTOs, error categories, and
//! redaction helpers.
//!
//! Also home to keyboard chords and the client lock mode they interact with,
//! generated names for sessions/tabs/panes, process spawn and exit types, and
//! the action vocabulary's live registry plus its resolution into runtime
//! commands.

pub mod action;
pub mod command;
pub mod constant;
pub mod discovery;
pub mod error;
pub mod event;
pub mod geometry;
pub mod ids;
pub mod key;
pub mod lock;
pub mod log;
pub mod mouse;
pub mod naming;
pub mod process;
pub mod redact;
pub mod registry;
pub mod resolve;
pub mod types;
