//! `koshi-core` — the shared vocabulary every other koshi crate reads: what
//! koshi can be asked to do, what it reports back, and the pieces both are
//! written in. Its only dependencies are `serde` and `uuid`; it depends on no
//! other koshi crate.
//!
//! - [`ids`] — one typed id per entity, [`geometry`] — cell coordinates and
//!   rectangles, [`error`] — failure category and severity, [`constant`] —
//!   numeric bounds several crates share.
//! - [`command`] — every mutation that can be requested, with the envelope one
//!   travels in; [`event`] — every completed fact the runtime emits, with the
//!   privacy tier an input event carries.
//! - [`action`] — the action names a user binds or types, [`registry`] — the
//!   live table of them, [`resolve`] — turning one into a command.
//! - [`key`] — keyboard chords, [`lock`] — a client's modal input state,
//!   [`mouse`] — mouse events and the reporting level a pane's program asked
//!   for.
//! - [`process`] — spawn, kill, and exit types; [`naming`] — generated names
//!   for sessions, tabs, and clients; [`client`] — where a client connected
//!   from.
//! - [`discovery`] — the read-only snapshots the list and inspect queries
//!   answer with; [`redact`] — scrubbing secrets out of text; [`log`] — the
//!   log level and format a config file names.
//! - [`recent_event`] — one event reduced to its name and ids, for the
//!   recent-events ring `koshi debug events` prints.
//! - [`text`] — bounding and filtering what a pane or a remote peer reports
//!   about itself: titles, working directories, and names.
//!
//! [`compat`] holds the table of every versioned surface koshi carries — the
//! wire protocols two builds speak and the files one build writes for another
//! to read — with the cadence rule their numbers follow.

pub mod action;
pub mod client;
pub mod command;
pub mod compat;
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
pub mod recent_event;
pub mod redact;
pub mod registry;
pub mod resolve;
pub mod selection;
pub mod text;
