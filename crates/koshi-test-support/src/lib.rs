//! Shared test utilities for the workspace.
//!
//! The crate provides event-sequence assertions, an in-memory PTY
//! (pseudo-terminal) backend, layout checks, a rate-bounded byte pump, a
//! runtime-directory fixture, and on Unix a wait for a child process to exit.

/// Wait for a child process to exit, and leave the exit uncollected.
///
/// [`child_exit::wait_until_child_has_exited`] waits up to ten seconds for the
/// child to exit and leaves it a zombie for the code under test to reap.
#[cfg(unix)]
pub mod child_exit;

/// Assert ordered event sequences.
///
/// [`event_assert::assert_events`] compares actual events with expected events
/// and panics with an index-aligned diff when they differ.
pub mod event_assert;

/// In-memory fake PTY backend for tests.
///
/// Implements [`koshi_pty::backend::state::PtyBackend`] without starting a
/// shell. It records spawns, writes, resizes, and kills, and lets tests drive
/// output and child exit.
pub mod fake_pty;

/// Shared test fixtures, including the runtime directory and the complete key
/// event for one chord.
pub mod fixtures;

/// Layout invariant checks for pure-layout tests.
///
/// Checks exact tiling, overlaps, spills, minimum cell sizes, and live pane
/// references. Each check returns `Result` instead of panicking.
pub mod layout_assert;

/// Rate-bounded byte pump for tests that need a slow link.
///
/// [`throttle::pump_throttled`] copies bytes between streams on its own thread,
/// limits each time slice, and checks a deadline.
pub mod throttle;
