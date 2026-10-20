//! Resizing PTYs to match a solved layout.
//!
//! The input is the layout crate's `(PaneId, Option<Rect>)` content rectangles: the
//! pane border is already removed, and `None` means the pane shows no content.
//! This module floors each `Some` rect to a PTY-legal size, applies every size
//! with one [`crate::backend::state::PtyBackend::resize_panes`] call, and
//! reports per pane what it did. It does no border math.

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
            .size
            .column_count
            .max(MIN_PTY_SIZE.column_count),
        row_count: content_rect.size.row_count.max(MIN_PTY_SIZE.row_count),
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
/// - A `None` rect is a pane showing no content: the backend is not asked to
///   resize it, and the result carries `applied_pty_size: None`.
/// - A `Some` rect is floored by [`compute_pty_size`]. Every floored size goes
///   to the backend in one [`crate::backend::state::PtyBackend::resize_panes`]
///   call, in input order. The result carries the floored size in
///   `applied_pty_size`.
/// - A backend error on a pane is dropped: the result carries
///   `applied_pty_size: None`, and every other pane is still resized.
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
    let pane_content_rects: Vec<(PaneId, Option<Rect>)> = pane_content_rects.into_iter().collect();
    let pane_pty_sizes: Vec<(PaneId, PtySize)> = pane_content_rects
        .iter()
        .filter_map(|&(pane_id, content_rect)| {
            content_rect.map(|content_rect| (pane_id, compute_pty_size(content_rect)))
        })
        .collect();
    let mut applied_pty_sizes = backend
        .resize_panes(&pane_pty_sizes)
        .into_iter()
        .zip(pane_pty_sizes)
        .map(|(resize_result, (_, pty_size))| resize_result.ok().map(|()| pty_size));
    pane_content_rects
        .into_iter()
        .map(|(pane_id, content_rect)| ResizeResult {
            pane_id,
            applied_pty_size: match content_rect {
                Some(_) => applied_pty_sizes.next().flatten(),
                None => None,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests;
