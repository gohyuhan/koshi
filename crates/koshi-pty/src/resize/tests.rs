//! Tests for PTY resizing: size flooring, batch application, and error handling.
//!
//! [`compute_pty_size`] floors layout dimensions to the PTY minimum (2 cols,
//! 1 row). [`resize_for_layout_change`] applies PTY resizes in input order, a
//! backend error on one pane never stops the rest, and a pane with no content
//! never reaches the backend.

use std::sync::Mutex;

use koshi_core::geometry::Size;
use koshi_core::process::{KillPolicy, SpawnSpec};

use super::*;
use crate::backend::state::PtyHandle;
use crate::error::PtyError;

/// A content rect at the origin with the given size.
fn rect(cols: u16, rows: u16) -> Rect {
    Rect::at_origin(Size { cols, rows })
}

/// A [`PtyBackend`] that records every `resize` it accepts and refuses every
/// `resize` naming `fail_on`.
struct RecordingBackend {
    /// Every accepted resize, oldest first.
    resizes: Mutex<Vec<(PaneId, PtySize)>>,
    /// The pane whose resizes are refused with [`PtyError::UnknownPane`].
    fail_on: Option<PaneId>,
}

impl RecordingBackend {
    /// A backend that accepts every resize.
    fn new() -> Self {
        Self {
            resizes: Mutex::new(Vec::new()),
            fail_on: None,
        }
    }

    /// A backend that refuses every resize of `pane` with
    /// [`PtyError::UnknownPane`] and accepts every other one.
    fn failing_on(pane: PaneId) -> Self {
        Self {
            resizes: Mutex::new(Vec::new()),
            fail_on: Some(pane),
        }
    }

    /// Every accepted resize, oldest first.
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

    fn live_cwd(&self, _pane: PaneId) -> Option<std::path::PathBuf> {
        unreachable!("resize tests never ask for a cwd")
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
fn compute_pty_size_leaves_the_exact_minimum_unchanged() {
    assert_eq!(compute_pty_size(rect(2, 1)), PtySize { cols: 2, rows: 1 });
}

#[test]
fn compute_pty_size_passes_the_largest_rect_through_unchanged() {
    assert_eq!(
        compute_pty_size(rect(u16::MAX, u16::MAX)),
        PtySize {
            cols: u16::MAX,
            rows: u16::MAX,
        }
    );
}

#[test]
fn an_empty_batch_yields_no_results_and_no_backend_calls() {
    let backend = RecordingBackend::new();

    let results = resize_for_layout_change(&backend, Vec::<(PaneId, Option<Rect>)>::new());

    assert_eq!(results, Vec::new());
    assert_eq!(backend.calls(), Vec::new());
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
        }]
    );
    assert_eq!(backend.calls(), Vec::new());
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
        }]
    );
    assert_eq!(backend.calls(), vec![(pane, PtySize { cols: 10, rows: 5 })]);
}

#[test]
fn a_tiny_visible_pane_is_floored_before_resizing() {
    let backend = RecordingBackend::new();
    let pane = PaneId::new();

    let results = resize_for_layout_change(&backend, vec![(pane, Some(rect(0, 0)))]);

    assert_eq!(
        results,
        vec![ResizeResult {
            pane_id: pane,
            applied: Some(PtySize { cols: 2, rows: 1 }),
        }]
    );
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
            },
            ResizeResult {
                pane_id: skipped,
                applied: None,
            },
            ResizeResult {
                pane_id: last,
                applied: Some(PtySize { cols: 20, rows: 8 }),
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
            },
            ResizeResult {
                pane_id: failing,
                applied: None,
            },
            ResizeResult {
                pane_id: after,
                applied: Some(PtySize { cols: 20, rows: 8 }),
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

#[test]
fn a_failing_pane_alone_yields_one_failed_result_and_no_backend_record() {
    let failing = PaneId::new();
    let backend = RecordingBackend::failing_on(failing);

    let results = resize_for_layout_change(&backend, vec![(failing, Some(rect(10, 5)))]);

    assert_eq!(
        results,
        vec![ResizeResult {
            pane_id: failing,
            applied: None,
        }]
    );
    assert_eq!(backend.calls(), Vec::new());
}

#[test]
fn a_failed_resize_and_a_no_content_skip_both_report_no_new_size() {
    let failing = PaneId::new();
    let hidden = PaneId::new();
    let backend = RecordingBackend::failing_on(failing);

    let results =
        resize_for_layout_change(&backend, vec![(failing, Some(rect(10, 5))), (hidden, None)]);

    assert_eq!(
        results,
        vec![
            ResizeResult {
                pane_id: failing,
                applied: None,
            },
            ResizeResult {
                pane_id: hidden,
                applied: None,
            },
        ]
    );
    assert_eq!(backend.calls(), Vec::new());
}

#[test]
fn a_pane_listed_twice_is_resized_twice_in_input_order() {
    let backend = RecordingBackend::new();
    let pane = PaneId::new();

    let results = resize_for_layout_change(
        &backend,
        vec![(pane, Some(rect(10, 5))), (pane, Some(rect(20, 8)))],
    );

    assert_eq!(
        results,
        vec![
            ResizeResult {
                pane_id: pane,
                applied: Some(PtySize { cols: 10, rows: 5 }),
            },
            ResizeResult {
                pane_id: pane,
                applied: Some(PtySize { cols: 20, rows: 8 }),
            },
        ]
    );
    assert_eq!(
        backend.calls(),
        vec![
            (pane, PtySize { cols: 10, rows: 5 }),
            (pane, PtySize { cols: 20, rows: 8 }),
        ]
    );
}

#[test]
fn a_batch_of_only_hidden_panes_touches_the_backend_for_none_of_them() {
    let backend = RecordingBackend::new();
    let first = PaneId::new();
    let second = PaneId::new();

    let results = resize_for_layout_change(&backend, vec![(first, None), (second, None)]);

    assert_eq!(
        results,
        vec![
            ResizeResult {
                pane_id: first,
                applied: None,
            },
            ResizeResult {
                pane_id: second,
                applied: None,
            },
        ]
    );
    assert_eq!(backend.calls(), Vec::new());
}
