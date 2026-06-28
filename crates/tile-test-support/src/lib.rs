//! `tile-test-support` — testing utilities: fake PTY backend, deterministic
//! scheduler, layout assertions, fixture shells, and golden render helpers.

/// Deterministic event-sequence recorder for command-transaction tests.
///
/// Records ordered bursts of [`tile_core::event::Event`]s and provides consuming
/// assertions that pretty-print diffs when sequences diverge.
pub mod event_queue;

/// In-memory fake PTY backend for isolation testing.
///
/// Implements the [`tile_pty::backend::state::PtyBackend`] trait entirely in
/// memory, capturing spawns, writes, resizes, and kills for assertion, and
/// allowing tests to drive output and child-exit on demand.
pub mod fake_pty;

/// Test fixture utilities (placeholder).
pub mod fixtures;

/// Layout invariant assertions for pure-layout tests.
///
/// Validates that placed panes maintain geometric invariants: exact tiling of
/// the tab area, no overlaps, no spills, and respect for minimum cell sizes.
pub mod layout_assert;
