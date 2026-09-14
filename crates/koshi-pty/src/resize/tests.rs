//! Tests for PTY resizing: size flooring, batch application, and error handling.
//!
//! [`compute_pty_size`] floors layout dimensions to the PTY minimum (2 cols,
//! 1 row). [`resize_for_layout_change`] applies PTY resizes in input order, a
//! backend error on one pane never stops the rest, and a pane with no content
//! never reaches the recording backend.

use std::sync::Mutex;

use koshi_core::geometry::Size;
use koshi_core::process::{KillPolicy, SpawnSpec};

use super::*;
use crate::backend::state::PtyHandle;
use crate::error::PtyError;

/// A content rect at the origin with the given size.
fn build_content_rect(column_count: u16, row_count: u16) -> Rect {
    Rect::from_size_at_origin(Size {
        column_count,
        row_count,
    })
}

/// A [`PtyBackend`] that records every `resize` it accepts and refuses every
/// `resize` naming `failed_pane_id`.
struct RecordingBackend {
    /// Every accepted resize, oldest first.
    accepted_resize_calls: Mutex<Vec<(PaneId, PtySize)>>,
    /// The pane whose resizes are refused with [`PtyError::UnknownPane`].
    failed_pane_id: Option<PaneId>,
}

impl RecordingBackend {
    /// A backend that accepts every resize.
    fn new() -> Self {
        Self {
            accepted_resize_calls: Mutex::new(Vec::new()),
            failed_pane_id: None,
        }
    }

    /// A backend that refuses every resize of `failed_pane_id` with
    /// [`PtyError::UnknownPane`] and accepts every other one.
    fn with_resize_failure_for(failed_pane_id: PaneId) -> Self {
        Self {
            accepted_resize_calls: Mutex::new(Vec::new()),
            failed_pane_id: Some(failed_pane_id),
        }
    }

    /// Every accepted resize, oldest first.
    fn list_resize_calls(&self) -> Vec<(PaneId, PtySize)> {
        self.accepted_resize_calls
            .lock()
            .expect("resize log lock")
            .clone()
    }
}

impl PtyBackend for RecordingBackend {
    fn spawn_pane(
        &self,
        _pane_id: PaneId,
        _spawn_spec: SpawnSpec,
        _pty_size: PtySize,
    ) -> Result<PtyHandle, PtyError> {
        unreachable!("resize tests never spawn")
    }

    fn resize_pane(&self, pane_id: PaneId, pty_size: PtySize) -> Result<(), PtyError> {
        if self.failed_pane_id == Some(pane_id) {
            return Err(PtyError::UnknownPane { pane_id });
        }
        self.accepted_resize_calls
            .lock()
            .expect("resize log lock")
            .push((pane_id, pty_size));
        Ok(())
    }

    fn write_pane_input(&self, _pane_id: PaneId, _input_bytes: &[u8]) -> Result<(), PtyError> {
        unreachable!("resize tests never write")
    }

    fn kill_pane(&self, _pane_id: PaneId, _kill_policy: KillPolicy) -> Result<(), PtyError> {
        unreachable!("resize tests never kill")
    }

    fn find_live_working_directory(&self, _pane_id: PaneId) -> Option<std::path::PathBuf> {
        unreachable!("resize tests never ask for a working directory")
    }
}

#[test]
fn compute_pty_size_passes_a_large_rect_through_unchanged() {
    assert_eq!(
        compute_pty_size(build_content_rect(80, 24)),
        PtySize {
            column_count: 80,
            row_count: 24
        }
    );
}

#[test]
fn compute_pty_size_floors_each_dimension_independently() {
    // cols below the floor, rows above: only cols clamps.
    assert_eq!(
        compute_pty_size(build_content_rect(1, 24)),
        PtySize {
            column_count: 2,
            row_count: 24
        }
    );
    // rows below the floor, cols above: only rows clamps.
    assert_eq!(
        compute_pty_size(build_content_rect(80, 0)),
        PtySize {
            column_count: 80,
            row_count: 1
        }
    );
    // both below: clamps to the full minimum.
    assert_eq!(
        compute_pty_size(build_content_rect(0, 0)),
        PtySize {
            column_count: 2,
            row_count: 1
        }
    );
}

#[test]
fn compute_pty_size_leaves_the_exact_minimum_unchanged() {
    assert_eq!(
        compute_pty_size(build_content_rect(2, 1)),
        PtySize {
            column_count: 2,
            row_count: 1
        }
    );
}

#[test]
fn compute_pty_size_passes_the_largest_rect_through_unchanged() {
    assert_eq!(
        compute_pty_size(build_content_rect(u16::MAX, u16::MAX)),
        PtySize {
            column_count: u16::MAX,
            row_count: u16::MAX,
        }
    );
}

#[test]
fn an_empty_batch_yields_no_results_and_no_backend_calls() {
    let recording_backend = RecordingBackend::new();

    let resize_results =
        resize_for_layout_change(&recording_backend, Vec::<(PaneId, Option<Rect>)>::new());

    assert_eq!(resize_results, Vec::new());
    assert_eq!(recording_backend.list_resize_calls(), Vec::new());
}

#[test]
fn a_pane_without_content_is_skipped_without_a_backend_call() {
    let recording_backend = RecordingBackend::new();
    let pane_id = PaneId::new();

    let resize_results = resize_for_layout_change(&recording_backend, vec![(pane_id, None)]);

    assert_eq!(
        resize_results,
        vec![ResizeResult {
            pane_id,
            applied_pty_size: None,
        }]
    );
    assert_eq!(recording_backend.list_resize_calls(), Vec::new());
}

#[test]
fn a_visible_pane_resizes_to_its_floored_size() {
    let recording_backend = RecordingBackend::new();
    let pane_id = PaneId::new();

    let resize_results = resize_for_layout_change(
        &recording_backend,
        vec![(pane_id, Some(build_content_rect(10, 5)))],
    );

    assert_eq!(
        resize_results,
        vec![ResizeResult {
            pane_id,
            applied_pty_size: Some(PtySize {
                column_count: 10,
                row_count: 5
            }),
        }]
    );
    assert_eq!(
        recording_backend.list_resize_calls(),
        vec![(
            pane_id,
            PtySize {
                column_count: 10,
                row_count: 5
            }
        )]
    );
}

#[test]
fn a_tiny_visible_pane_is_floored_before_resizing() {
    let recording_backend = RecordingBackend::new();
    let pane_id = PaneId::new();

    let resize_results = resize_for_layout_change(
        &recording_backend,
        vec![(pane_id, Some(build_content_rect(0, 0)))],
    );

    assert_eq!(
        resize_results,
        vec![ResizeResult {
            pane_id,
            applied_pty_size: Some(PtySize {
                column_count: 2,
                row_count: 1
            }),
        }]
    );
    assert_eq!(
        recording_backend.list_resize_calls(),
        vec![(
            pane_id,
            PtySize {
                column_count: 2,
                row_count: 1
            }
        )]
    );
}

#[test]
fn a_mixed_batch_preserves_order_and_skips_none_panes() {
    let recording_backend = RecordingBackend::new();
    let first_pane_id = PaneId::new();
    let skipped_pane_id = PaneId::new();
    let last_pane_id = PaneId::new();

    let resize_results = resize_for_layout_change(
        &recording_backend,
        vec![
            (first_pane_id, Some(build_content_rect(10, 5))),
            (skipped_pane_id, None),
            (last_pane_id, Some(build_content_rect(20, 8))),
        ],
    );

    assert_eq!(
        resize_results,
        vec![
            ResizeResult {
                pane_id: first_pane_id,
                applied_pty_size: Some(PtySize {
                    column_count: 10,
                    row_count: 5
                }),
            },
            ResizeResult {
                pane_id: skipped_pane_id,
                applied_pty_size: None,
            },
            ResizeResult {
                pane_id: last_pane_id,
                applied_pty_size: Some(PtySize {
                    column_count: 20,
                    row_count: 8
                }),
            },
        ]
    );
    // Only the two visible panes hit the recording_backend, in order.
    assert_eq!(
        recording_backend.list_resize_calls(),
        vec![
            (
                first_pane_id,
                PtySize {
                    column_count: 10,
                    row_count: 5
                }
            ),
            (
                last_pane_id,
                PtySize {
                    column_count: 20,
                    row_count: 8
                }
            ),
        ]
    );
}

#[test]
fn a_backend_error_on_one_pane_does_not_stop_the_rest() {
    let first_pane_id = PaneId::new();
    let failing_pane_id = PaneId::new();
    let following_pane_id = PaneId::new();
    let recording_backend = RecordingBackend::with_resize_failure_for(failing_pane_id);

    let resize_results = resize_for_layout_change(
        &recording_backend,
        vec![
            (first_pane_id, Some(build_content_rect(10, 5))),
            (failing_pane_id, Some(build_content_rect(10, 5))),
            (following_pane_id, Some(build_content_rect(20, 8))),
        ],
    );

    // The failing pane is recorded with no applied size (and is not a no-content
    // skip); the panes before and after it are both resized.
    assert_eq!(
        resize_results,
        vec![
            ResizeResult {
                pane_id: first_pane_id,
                applied_pty_size: Some(PtySize {
                    column_count: 10,
                    row_count: 5
                }),
            },
            ResizeResult {
                pane_id: failing_pane_id,
                applied_pty_size: None,
            },
            ResizeResult {
                pane_id: following_pane_id,
                applied_pty_size: Some(PtySize {
                    column_count: 20,
                    row_count: 8
                }),
            },
        ]
    );
    // Both non-failing panes reached the recording_backend, in order.
    assert_eq!(
        recording_backend.list_resize_calls(),
        vec![
            (
                first_pane_id,
                PtySize {
                    column_count: 10,
                    row_count: 5
                }
            ),
            (
                following_pane_id,
                PtySize {
                    column_count: 20,
                    row_count: 8
                }
            ),
        ]
    );
}

#[test]
fn a_failing_pane_alone_yields_one_failed_result_and_no_backend_record() {
    let failing_pane_id = PaneId::new();
    let recording_backend = RecordingBackend::with_resize_failure_for(failing_pane_id);

    let resize_results = resize_for_layout_change(
        &recording_backend,
        vec![(failing_pane_id, Some(build_content_rect(10, 5)))],
    );

    assert_eq!(
        resize_results,
        vec![ResizeResult {
            pane_id: failing_pane_id,
            applied_pty_size: None,
        }]
    );
    assert_eq!(recording_backend.list_resize_calls(), Vec::new());
}

#[test]
fn a_failed_resize_and_a_no_content_skip_both_report_no_new_size() {
    let failing_pane_id = PaneId::new();
    let hidden_pane_id = PaneId::new();
    let recording_backend = RecordingBackend::with_resize_failure_for(failing_pane_id);

    let resize_results = resize_for_layout_change(
        &recording_backend,
        vec![
            (failing_pane_id, Some(build_content_rect(10, 5))),
            (hidden_pane_id, None),
        ],
    );

    assert_eq!(
        resize_results,
        vec![
            ResizeResult {
                pane_id: failing_pane_id,
                applied_pty_size: None,
            },
            ResizeResult {
                pane_id: hidden_pane_id,
                applied_pty_size: None,
            },
        ]
    );
    assert_eq!(recording_backend.list_resize_calls(), Vec::new());
}

#[test]
fn a_pane_listed_twice_is_resized_twice_in_input_order() {
    let recording_backend = RecordingBackend::new();
    let pane_id = PaneId::new();

    let resize_results = resize_for_layout_change(
        &recording_backend,
        vec![
            (pane_id, Some(build_content_rect(10, 5))),
            (pane_id, Some(build_content_rect(20, 8))),
        ],
    );

    assert_eq!(
        resize_results,
        vec![
            ResizeResult {
                pane_id,
                applied_pty_size: Some(PtySize {
                    column_count: 10,
                    row_count: 5
                }),
            },
            ResizeResult {
                pane_id,
                applied_pty_size: Some(PtySize {
                    column_count: 20,
                    row_count: 8
                }),
            },
        ]
    );
    assert_eq!(
        recording_backend.list_resize_calls(),
        vec![
            (
                pane_id,
                PtySize {
                    column_count: 10,
                    row_count: 5
                }
            ),
            (
                pane_id,
                PtySize {
                    column_count: 20,
                    row_count: 8
                }
            ),
        ]
    );
}

#[test]
fn a_batch_of_only_hidden_panes_touches_the_backend_for_none_of_them() {
    let recording_backend = RecordingBackend::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();

    let resize_results = resize_for_layout_change(
        &recording_backend,
        vec![(first_pane_id, None), (second_pane_id, None)],
    );

    assert_eq!(
        resize_results,
        vec![
            ResizeResult {
                pane_id: first_pane_id,
                applied_pty_size: None,
            },
            ResizeResult {
                pane_id: second_pane_id,
                applied_pty_size: None,
            },
        ]
    );
    assert_eq!(recording_backend.list_resize_calls(), Vec::new());
}
