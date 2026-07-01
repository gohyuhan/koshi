//! Tests for PTY resizing: size clamping, batch application, and error handling.
//!
//! [`compute_pty_size`] floors layout dimensions to PTY minima (2 cols, 1 row).
//! [`resize_for_layout_change`] applies PTY resizes in order best-effort (a
//! backend error on one pane never stops the rest) and tracks which panes kept
//! their last valid size (for invisible/hidden panes).

use std::sync::Mutex;

use tile_core::geometry::{Point, Size};
use tile_core::process::{KillPolicy, SpawnSpec};

use super::*;
use crate::backend::state::PtyHandle;
use crate::error::PtyError;

/// A content rect at the origin — only the size matters to resize.
fn rect(cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x: 0, y: 0 }, Size { cols, rows })
}

/// A [`PtyBackend`] that records every `resize` and can be told to fail one
/// pane, so the tests can assert exact sizes, call order, and abort behavior.
struct RecordingBackend {
    resizes: Mutex<Vec<(PaneId, PtySize)>>,
    fail_on: Option<PaneId>,
}

impl RecordingBackend {
    /// Create a backend that records resize calls without errors.
    fn new() -> Self {
        Self {
            resizes: Mutex::new(Vec::new()),
            fail_on: None,
        }
    }

    /// Create a backend that rejects resize calls for a specific pane with `UnknownPane` error.
    fn failing_on(pane: PaneId) -> Self {
        Self {
            resizes: Mutex::new(Vec::new()),
            fail_on: Some(pane),
        }
    }

    /// Return a copy of all resize calls recorded on this backend in order.
    fn calls(&self) -> Vec<(PaneId, PtySize)> {
        self.resizes.lock().expect("resize log lock").clone()
    }
}

impl PtyBackend for RecordingBackend {
    fn spawn(
        &self,
        _pane_id: PaneId,
        _spec: SpawnSpec,
        _size: PtySize,
    ) -> Result<PtyHandle, PtyError> {
        unreachable!("resize tests never spawn")
    }

    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError> {
        if self.fail_on == Some(pane) {
            return Err(PtyError::UnknownPane { pane });
        }
        self.resizes
            .lock()
            .expect("resize log lock")
            .push((pane, size));
        Ok(())
    }

    fn write(&self, _pane: PaneId, _bytes: &[u8]) -> Result<(), PtyError> {
        unreachable!("resize tests never write")
    }

    fn kill(&self, _pane: PaneId, _kill_policy: KillPolicy) -> Result<(), PtyError> {
        unreachable!("resize tests never kill")
    }
}

#[test]
fn compute_pty_size_passes_a_large_rect_through_unchanged() {
    assert_eq!(
        compute_pty_size(rect(80, 24)),
        PtySize { cols: 80, rows: 24 }
    );
}

#[test]
fn compute_pty_size_floors_each_dimension_independently() {
    // cols below the floor, rows above: only cols clamps.
    assert_eq!(compute_pty_size(rect(1, 24)), PtySize { cols: 2, rows: 24 });
    // rows below the floor, cols above: only rows clamps.
    assert_eq!(compute_pty_size(rect(80, 0)), PtySize { cols: 80, rows: 1 });
    // both below: clamps to the full minimum.
    assert_eq!(compute_pty_size(rect(0, 0)), PtySize { cols: 2, rows: 1 });
}

#[test]
fn a_none_pane_is_skipped_without_a_backend_call() {
    let backend = RecordingBackend::new();
    let pane = PaneId::new();

    let results = resize_for_layout_change(&backend, vec![(pane, None)]);

    assert_eq!(
        results,
        vec![ResizeResult {
            pane_id: pane,
            applied: None,
            kept_last_valid: true,
        }]
    );
    assert!(backend.calls().is_empty());
}

#[test]
fn a_visible_pane_resizes_to_its_floored_size() {
    let backend = RecordingBackend::new();
    let pane = PaneId::new();

    let results = resize_for_layout_change(&backend, vec![(pane, Some(rect(10, 5)))]);

    assert_eq!(
        results,
        vec![ResizeResult {
            pane_id: pane,
            applied: Some(PtySize { cols: 10, rows: 5 }),
            kept_last_valid: false,
        }]
    );
    assert_eq!(backend.calls(), vec![(pane, PtySize { cols: 10, rows: 5 })]);
}

#[test]
fn a_tiny_visible_pane_is_floored_before_resizing() {
    let backend = RecordingBackend::new();
    let pane = PaneId::new();

    let results = resize_for_layout_change(&backend, vec![(pane, Some(rect(0, 0)))]);

    assert_eq!(results[0].applied, Some(PtySize { cols: 2, rows: 1 }));
    assert_eq!(backend.calls(), vec![(pane, PtySize { cols: 2, rows: 1 })]);
}

#[test]
fn a_mixed_batch_preserves_order_and_skips_none_panes() {
    let backend = RecordingBackend::new();
    let first = PaneId::new();
    let skipped = PaneId::new();
    let last = PaneId::new();

    let results = resize_for_layout_change(
        &backend,
        vec![
            (first, Some(rect(10, 5))),
            (skipped, None),
            (last, Some(rect(20, 8))),
        ],
    );

    assert_eq!(
        results,
        vec![
            ResizeResult {
                pane_id: first,
                applied: Some(PtySize { cols: 10, rows: 5 }),
                kept_last_valid: false,
            },
            ResizeResult {
                pane_id: skipped,
                applied: None,
                kept_last_valid: true,
            },
            ResizeResult {
                pane_id: last,
                applied: Some(PtySize { cols: 20, rows: 8 }),
                kept_last_valid: false,
            },
        ]
    );
    // Only the two visible panes hit the backend, in order.
    assert_eq!(
        backend.calls(),
        vec![
            (first, PtySize { cols: 10, rows: 5 }),
            (last, PtySize { cols: 20, rows: 8 }),
        ]
    );
}

#[test]
fn a_backend_error_on_one_pane_does_not_stop_the_rest() {
    let first = PaneId::new();
    let failing = PaneId::new();
    let after = PaneId::new();
    let backend = RecordingBackend::failing_on(failing);

    let results = resize_for_layout_change(
        &backend,
        vec![
            (first, Some(rect(10, 5))),
            (failing, Some(rect(10, 5))),
            (after, Some(rect(20, 8))),
        ],
    );

    // The failing pane is recorded with no applied size (and is not a no-content
    // skip); the panes before and after it are both resized.
    assert_eq!(
        results,
        vec![
            ResizeResult {
                pane_id: first,
                applied: Some(PtySize { cols: 10, rows: 5 }),
                kept_last_valid: false,
            },
            ResizeResult {
                pane_id: failing,
                applied: None,
                kept_last_valid: false,
            },
            ResizeResult {
                pane_id: after,
                applied: Some(PtySize { cols: 20, rows: 8 }),
                kept_last_valid: false,
            },
        ]
    );
    // Both non-failing panes reached the backend, in order.
    assert_eq!(
        backend.calls(),
        vec![
            (first, PtySize { cols: 10, rows: 5 }),
            (after, PtySize { cols: 20, rows: 8 }),
        ]
    );
}
