//! Driving PTY resizes from a solved layout.
//!
//! The layout crate has already removed the 1-cell pane border and decided
//! which panes show content, handing out `(PaneId, Option<Rect>)` content
//! rects (`None` ⇔ no content shown). This module is the thin, border-agnostic
//! executor: it floors each visible rect to a PTY-legal size and calls
//! [`PtyBackend::resize`], reporting per pane what it did. It does **no**
//! border math and does not depend on `tile-layout` (the two are siblings).

use std::cmp::max;

use tile_core::{geometry::Rect, ids::PaneId, process::PtySize};

use crate::{backend::state::PtyBackend, error::PtyError};

/// Smallest PTY a child is ever sized to: 2 columns by 1 row.
///
/// Distinct from the layout crate's outer `MIN_PANE_SIZE`; this is the
/// PTY-validity floor applied to the *content* rect after the border is gone.
const MIN_PTY_SIZE: PtySize = PtySize { cols: 2, rows: 1 };

/// Floor a content rect to a PTY-legal [`PtySize`].
///
/// The rect is already the inner content area (border removed upstream), so
/// this only clamps each dimension up to the 2×1 PTY minimum — no border math.
#[must_use]
pub fn compute_pty_size(content: Rect) -> PtySize {
    PtySize {
        cols: max(content.size.cols, MIN_PTY_SIZE.cols),
        rows: max(content.size.rows, MIN_PTY_SIZE.rows),
    }
}

/// What [`resize_for_layout_change`] did for a single pane.
///
/// Transient runtime metadata (never persisted or sent over IPC — the
/// `PtyResized` event carries the wire form), so it is `Copy` and not `serde`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeResult {
    /// The pane this result describes.
    pub pane_id: PaneId,
    /// The size the PTY was resized to, or `None` if the pane was skipped.
    pub applied: Option<PtySize>,
    /// `true` when the pane was skipped (no content) and kept its last size.
    pub kept_last_valid: bool,
}

/// Resize every pane's PTY to match a freshly solved layout.
///
/// Walks `pane_items` (the `(PaneId, Option<Rect>)` output of the layout
/// crate's `content_rects`) in order. A `None` rect means the pane shows no
/// content: it is skipped with `kept_last_valid` set and no backend call. A
/// `Some` rect is floored via [`compute_pty_size`] and applied through
/// [`PtyBackend::resize`]. The first backend error aborts the batch; panes
/// already resized stay resized on the OS.
///
/// # Errors
///
/// Returns the first [`PtyError`] a [`PtyBackend::resize`] call reports.
pub fn resize_for_layout_change(
    backend: &dyn PtyBackend,
    pane_items: impl IntoIterator<Item = (PaneId, Option<Rect>)>,
) -> Result<Vec<ResizeResult>, PtyError> {
    let mut updated_pane_result = Vec::new();

    for (pane_id, pane_size) in pane_items {
        match pane_size {
            None => updated_pane_result.push(ResizeResult {
                pane_id,
                applied: None,
                kept_last_valid: true,
            }),
            Some(rect) => {
                let computed = compute_pty_size(rect);
                backend.resize(pane_id, computed)?;
                updated_pane_result.push(ResizeResult {
                    pane_id,
                    applied: Some(computed),
                    kept_last_valid: false,
                });
            }
        }
    }

    Ok(updated_pane_result)
}

#[cfg(test)]
mod tests;
