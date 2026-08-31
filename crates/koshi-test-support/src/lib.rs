//! `koshi-test-support` — testing utilities shared across the workspace.
//!
//! The crate holds an event-sequence assertion, an in-memory fake PTY
//! (pseudo-terminal, the virtual terminal a shell process runs inside)
//! backend, layout invariant checks, a rate-bounded byte pump, and the
//! shared runtime-directory fixture.

/// Event-sequence assertion.
///
/// [`event_assert::assert_events`] compares an emitted burst of
/// [`koshi_core::event::Event`]s against the expected sequence and panics with
/// an index-aligned diff when they differ.
pub mod event_assert;

/// In-memory fake PTY backend for isolation testing.
///
/// Implements the [`koshi_pty::backend::state::PtyBackend`] trait entirely in
/// memory, capturing spawns, writes, resizes, and kills for assertion, and
/// allowing tests to drive output and child-exit on demand.
pub mod fake_pty;

/// Test fixtures shared across the suite: the temporary runtime directory.
pub mod fixtures;

/// Layout invariant checks for pure-layout tests.
///
/// Checks that placed panes hold the geometric invariants: exact tiling of
/// the tab area, no overlaps, no spills, and respect for minimum cell sizes.
/// Also checks that every layout leaf references a live pane. Each check
/// returns `Result`, never panics.
pub mod layout_assert;

/// Rate-bounded byte pump for tests that need a slow link.
///
/// [`throttle::pump_throttled`] copies bytes from one stream to another on its
/// own thread, at most a fixed number of bytes per time slice, and stops at a
/// deadline.
pub mod throttle;
