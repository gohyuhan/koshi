//! Resizing PTYs to match a solved layout.
//!
//! The input is the layout crate's `(PaneId, Option<Rect>)` content rectangles: the
//! pane border is already removed, and `None` means the pane shows no content.
//! This module floors each `Some` rect to a PTY-legal size, calls
//! [`crate::backend::state::PtyBackend::resize_pane`], and reports per pane what it
//! did. It does no border math.

use koshi_core::{geometry::Rect, ids::PaneId, process::PtySize};

use crate::backend::state::PtyBackend;

/// The smallest size a PTY is set to: 2 columns by 1 row.
///
/// Applied to the content rect, after the border is removed.
const MIN_PTY_SIZE: PtySize = PtySize {
    column_count: 2,
    row_count: 1,
};

/// Floor a content rect to a PTY-legal [`PtySize`].
///
/// Each dimension is raised to the 2×1 minimum on its own: `1×24` becomes
/// `2×24`, `80×0` becomes `80×1`, and `80×24` is returned unchanged. `content_rect`
/// is the inner content area, with the border already removed.
#[must_use]
pub fn compute_pty_size(content_rect: Rect) -> PtySize {
    PtySize {
        column_count: content_rect
            .cell_size
            .column_count
            .max(MIN_PTY_SIZE.column_count),
        row_count: content_rect.cell_size.row_count.max(MIN_PTY_SIZE.row_count),
    }
}

/// What [`resize_for_layout_change`] did for a single pane.
///
/// Lives in this process only: it is never persisted or sent over IPC. The
/// `PtyResized` event carries the wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeResult {
    /// The pane this result describes.
    pub pane_id: PaneId,
    /// The size the PTY was resized to. `None` when the pane was skipped or
    /// the backend refused the resize; the PTY keeps its last size in both
    /// cases.
    pub applied_pty_size: Option<PtySize>,
}

/// Resize every pane's PTY to match a freshly solved layout.
///
/// Walks `pane_content_rects` (the `(PaneId, Option<Rect>)` output of the layout
/// crate's `list_content_rects`) in order:
///
/// - A `None` rect is a pane showing no content: no backend call, and the
///   result carries `applied_pty_size: None`.
/// - A `Some` rect is floored by [`compute_pty_size`] and applied through
///   [`crate::backend::state::PtyBackend::resize_pane`]. The result carries the
///   floored size in `applied_pty_size`.
/// - A backend error on a pane is dropped: the result carries
///   `applied_pty_size: None`,
///   and the walk continues with the next pane.
///
/// Holds no per-pane state: the caller picks which panes to pass, and reads
/// `applied_pty_size` to learn each pane's new size.
///
/// Returns one [`ResizeResult`] per input pane, in input order.
#[must_use]
pub fn resize_for_layout_change(
    backend: &dyn PtyBackend,
    pane_content_rects: impl IntoIterator<Item = (PaneId, Option<Rect>)>,
) -> Vec<ResizeResult> {
    pane_content_rects
        .into_iter()
        .map(|(pane_id, content_rect)| ResizeResult {
            pane_id,
            applied_pty_size: content_rect.and_then(|content_rect| {
                let pty_size = compute_pty_size(content_rect);
                backend
                    .resize_pane(pane_id, pty_size)
                    .is_ok()
                    .then_some(pty_size)
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests;
