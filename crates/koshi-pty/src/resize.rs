//! Resizing PTYs to match a solved layout.
//!
//! The input is the layout crate's `(PaneId, Option<Rect>)` content rects: the
//! pane border is already removed, and `None` means the pane shows no content.
//! This module floors each `Some` rect to a PTY-legal size, calls
//! [`crate::backend::state::PtyBackend::resize`], and reports per pane what it
//! did. It does no border math.

use koshi_core::{geometry::Rect, ids::PaneId, process::PtySize};

use crate::backend::state::PtyBackend;

/// The smallest size a PTY is set to: 2 columns by 1 row.
///
/// Applied to the content rect, after the border is removed.
const MIN_PTY_SIZE: PtySize = PtySize { cols: 2, rows: 1 };

/// Floor a content rect to a PTY-legal [`PtySize`].
///
/// Each dimension is raised to the 2×1 minimum on its own: `1×24` becomes
/// `2×24`, `80×0` becomes `80×1`, and `80×24` is returned unchanged. `content`
/// is the inner content area, with the border already removed.
#[must_use]
pub fn compute_pty_size(content: Rect) -> PtySize {
    PtySize {
        cols: content.size.cols.max(MIN_PTY_SIZE.cols),
        rows: content.size.rows.max(MIN_PTY_SIZE.rows),
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
    pub applied: Option<PtySize>,
}

/// Resize every pane's PTY to match a freshly solved layout.
///
/// Walks `pane_items` (the `(PaneId, Option<Rect>)` output of the layout
/// crate's `content_rects`) in order:
///
/// - A `None` rect is a pane showing no content: no backend call, and the
///   result carries `applied: None`.
/// - A `Some` rect is floored by [`compute_pty_size`] and applied through
///   [`crate::backend::state::PtyBackend::resize`]. The result carries the
///   floored size in `applied`.
/// - A backend error on a pane is dropped: the result carries `applied: None`,
///   and the walk continues with the next pane.
///
/// Holds no per-pane state: the caller picks which panes to pass, and reads
/// `applied` to learn each pane's new size.
///
/// Returns one [`ResizeResult`] per input pane, in input order.
#[must_use]
pub fn resize_for_layout_change(
    backend: &dyn PtyBackend,
    pane_items: impl IntoIterator<Item = (PaneId, Option<Rect>)>,
) -> Vec<ResizeResult> {
    pane_items
        .into_iter()
        .map(|(pane_id, content)| ResizeResult {
            pane_id,
            applied: content.and_then(|rect| {
                let computed = compute_pty_size(rect);
                backend
                    .resize(pane_id, computed)
                    .is_ok()
                    .then_some(computed)
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests;
