//! Per-pane terminal state: screen buffers, cursor, pen style (the
//! foreground/background color and attributes applied to newly written
//! text), modes, title, reported working directory, scrollback, and the
//! device-reply queue.
//!
//! One [`TerminalState`] backs a single terminal pane; panes never share
//! buffers. The state travels inside a per-pane
//! [`TerminalEngine`](crate::engine::TerminalEngine) — the runtime owns the
//! `PaneId → TerminalEngine` map — so the state itself carries no identity.
//! The VTE performer (see the `perform` submodule) mutates this model as PTY
//! output arrives; device queries in that output (DA/DSR/DECRQM — Device
//! Attributes, Device Status Report, and Request Mode queries) queue their
//! answer bytes on the state, which the runtime drains back into the PTY.
//!
//! The state's component types live in sibling submodules — the active
//! [`Screen`], the per-screen [`RenderState`] and its [`Charset`] slots, the
//! [`Cursor`] and its [`SavedCursor`] snapshot, the [`TerminalModes`] flags with
//! their [`MouseTracking`]/[`MouseEncoding`] levels, the [`ReportedCwd`], and the
//! [`ClippedRow`] render view — and are re-exported here so the whole model is
//! reachable as `koshi_terminal::state::*`.

use std::cmp::min;
use std::sync::Arc;

use koshi_core::process::PtySize;

use crate::grid::state::{Cell, Grid};
use crate::scrollback::{Scrollback, ScrollbackLimit};
use crate::style::Style;

mod clipped_row;
mod cursor;
mod cwd;
mod modes;
mod perform;
mod reflow;
mod render;
mod screen;

pub use clipped_row::ClippedRow;
pub use cursor::{Cursor, SavedCursor};
pub use cwd::ReportedCwd;
pub use modes::{MouseEncoding, MouseTracking, TerminalModes};
pub use render::{Charset, RenderState};
pub use screen::Screen;

/// The full emulation state of one terminal pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalState {
    /// The primary (normal, scrolling) screen buffer, reference-counted so a
    /// render snapshot can share it without copying; a write clones it once on
    /// demand (copy-on-write via [`Arc::make_mut`] in [`active_grid_mut`]).
    ///
    /// [`active_grid_mut`]: Self::active_grid_mut
    primary: Arc<Grid>,
    /// The alternate screen buffer used by full-screen apps; swapped in via DEC
    /// mode `?1049`/`?47` and never appended to the `scrollback`. Reference-counted
    /// like `primary`.
    alternate: Arc<Grid>,
    /// Which buffer — `primary` or `alternate` — output currently writes to and
    /// the renderer displays.
    active: Screen,
    /// The cursor for the primary screen, holding its own position, visibility,
    /// wrap latch, and saved snapshot.
    primary_cursor: Cursor,
    /// The cursor for the alternate screen, independent of the primary cursor
    /// so that position and wrap state do not leak across screen switches.
    alternate_cursor: Cursor,
    /// The primary screen's [`RenderState`] (pen, charsets, GL slot).
    primary_render: RenderState,
    /// The alternate screen's [`RenderState`], cloned from `primary_render` on
    /// each alternate-screen entry.
    alternate_render: RenderState,
    /// Active terminal modes (bracketed paste, mouse tracking, …).
    modes: TerminalModes,
    /// The window/tab title set via OSC 0/1/2; `None` until the app sets one.
    title: Option<String>,
    /// The working directory last reported by the shell via OSC 7 (host +
    /// decoded path), or `None` until the shell reports one. Consumed by cwd
    /// inheritance so a newly split pane can open in the same directory.
    reported_cwd: Option<ReportedCwd>,
    /// Lines that have scrolled off the top of the primary screen.
    scrollback: Scrollback,
    /// Primary screen's DECSTBM scroll-region margins, 0-based inclusive
    /// `(top, bottom)`; `None` scrolls the whole screen. Kept per screen (not
    /// shared) so an alt-screen app's margins do not leak onto the primary
    /// after it exits.
    primary_scroll_region: Option<(u16, u16)>,
    /// Alternate screen's scroll-region margins; see `primary_scroll_region`.
    alternate_scroll_region: Option<(u16, u16)>,
    /// The grapheme cluster currently being built at the cursor — the run of
    /// printed code points that fold into one cell (a base plus its combining
    /// marks and any emoji continuation: ZWJ-joined parts, variation selectors,
    /// skin-tone modifiers, regional-indicator flags). Empty when no run is
    /// active; any non-printing event resets it.
    cluster: String,
    /// The `(row, col)` of the cell holding `cluster`'s base, or `None` when no
    /// run is active. Continuations attach here and width promotion widens it.
    cluster_base: Option<(u16, u16)>,
    /// Bytes queued for the running app in answer to its device queries
    /// (DA/DSR/DECRQM). The performer appends replies here; the runtime drains
    /// them via [`take_replies`](Self::take_replies) and writes them back into
    /// the pane's PTY. Device-global: one queue regardless of the active screen.
    replies: Vec<u8>,
}

impl TerminalState {
    /// Create per-pane state for a terminal of `size`: both screen buffers
    /// blank, the cursor at the top-left and visible, default pen, no title.
    pub fn new(size: PtySize) -> Self {
        let terminal_size = Grid::blank(size.rows, size.cols, Style::default());
        let terminal_cursor = Cursor {
            row: 0,
            col: 0,
            is_visible: true,
            pending_wrap: false,
            saved: None,
        };
        TerminalState {
            primary: Arc::new(terminal_size.clone()),
            alternate: Arc::new(terminal_size),
            active: Screen::Primary,
            primary_cursor: terminal_cursor,
            alternate_cursor: terminal_cursor,
            primary_render: RenderState::fresh(),
            alternate_render: RenderState::fresh(),
            modes: TerminalModes::default(),
            title: None,
            reported_cwd: None,
            scrollback: Scrollback::new(ScrollbackLimit::default()),
            primary_scroll_region: None,
            alternate_scroll_region: None,
            cluster: String::new(),
            cluster_base: None,
            replies: Vec::new(),
        }
    }

    /// Resize both screen buffers to `size`, preserving their contents.
    ///
    /// The primary screen REFLOWS: soft-wrapped rows re-join into logical
    /// lines ([`RowEnd`](crate::grid::state::RowEnd)) and re-wrap to the new
    /// width — text wider than the new width wraps onto continuation rows
    /// instead of being cut off, and widening re-joins what an earlier
    /// narrow width wrapped. Rows past the new height scroll into history
    /// (trailing blank padding rows drop instead), a taller screen pulls
    /// history back in, and the cursor stays on its logical line at its
    /// content offset. The alternate screen has no history and its apps
    /// repaint on resize: each row crops on the right or pads with the
    /// screen's own background (a wide glyph whose right half is cut off is
    /// blanked), and a height shrink crops off the top. Scroll margins index
    /// the old geometry and are dropped until the app issues DECSTBM again.
    pub fn resize(&mut self, size: PtySize) {
        let alternate_fill = self.alternate_render.style.bg_fill();

        self.reflow_primary(size);

        // The alternate screen keeps what fits: crop off the top, pad at the
        // bottom, no history on either side.
        let mut rows: Vec<Vec<Cell>> = self.alternate.rows().to_vec();
        crop_columns(&mut rows, size.cols, alternate_fill);
        while rows.len() > size.rows as usize {
            rows.remove(0);
            self.alternate_cursor.row = self.alternate_cursor.row.saturating_sub(1);
        }
        rows.resize(
            size.rows as usize,
            vec![Cell::blank_with(alternate_fill); size.cols as usize],
        );
        self.alternate = Arc::new(Grid::from_rows(rows, size.cols, alternate_fill));

        // Clamp both cursors to the new bounds.
        self.primary_cursor.row = min(self.primary_cursor.row, size.rows.saturating_sub(1));
        self.primary_cursor.col = min(self.primary_cursor.col, size.cols.saturating_sub(1));
        self.primary_cursor.pending_wrap = false;

        self.alternate_cursor.row = min(self.alternate_cursor.row, size.rows.saturating_sub(1));
        self.alternate_cursor.col = min(self.alternate_cursor.col, size.cols.saturating_sub(1));
        self.alternate_cursor.pending_wrap = false;

        // Margins index the old geometry; drop the region so the resized screen
        // scrolls in full until the app issues DECSTBM again.
        self.primary_scroll_region = None;
        self.alternate_scroll_region = None;

        // The surviving cells moved rows and columns, so any in-progress
        // cluster's recorded base position is stale; drop the run.
        self.cluster.clear();
        self.cluster_base = None;
    }

    /// Which screen (primary or alternate) is currently displayed and written to.
    pub fn active_screen(&self) -> Screen {
        self.active
    }

    /// The screen buffer currently displayed and written to — `primary` or
    /// `alternate`, per the active screen.
    pub fn active_grid(&self) -> &Grid {
        match self.active {
            Screen::Primary => self.primary.as_ref(),
            Screen::Alternate => self.alternate.as_ref(),
        }
    }

    /// Mutable access to the active screen buffer, for writing cells. Clones the
    /// buffer once (copy-on-write) if a render snapshot still shares it, so the
    /// snapshot keeps the pre-write contents.
    pub fn active_grid_mut(&mut self) -> &mut Grid {
        match self.active {
            Screen::Primary => Arc::make_mut(&mut self.primary),
            Screen::Alternate => Arc::make_mut(&mut self.alternate),
        }
    }

    /// A reference-counted handle to the active screen buffer for the render
    /// snapshot: clones the `Arc`, not the grid. The next write to this screen
    /// clones the buffer once ([`active_grid_mut`]), leaving this handle pointing
    /// at the frozen contents.
    ///
    /// [`active_grid_mut`]: Self::active_grid_mut
    pub fn active_grid_arc(&self) -> Arc<Grid> {
        match self.active {
            Screen::Primary => Arc::clone(&self.primary),
            Screen::Alternate => Arc::clone(&self.alternate),
        }
    }

    /// The active screen buffer the renderer should draw at scrollback view
    /// `offset` — lines scrolled up from the live bottom, `0` following live
    /// output — paired with the *effective* offset actually shown.
    ///
    /// The effective offset is the single source of truth for how far the view is
    /// scrolled: it is `0` (and the buffer travels by reference, no copy) when
    /// `offset` is `0`, on the alternate screen (which keeps no scrollback), or
    /// with empty history. In every other case it is `offset` clamped to the
    /// retained line count, so an over-scrolled or stale value stops at the
    /// oldest line and never indexes past it. Returning it here keeps the
    /// composed grid, the scroll indicator, and cursor suppression from ever
    /// disagreeing about whether the view is scrolled.
    ///
    /// A non-zero effective offset composes a fresh window `rows` tall from the
    /// primary screen: its top rows are the newest scrollback lines, its lower
    /// rows the top of the live grid, so a view scrolled that many lines up shows
    /// that much history with the rest of the live screen below. History rows
    /// captured at a narrower width are padded to the current width with the
    /// primary screen's background ([`Style::bg_fill`]), the same fill every
    /// erase and scroll uses.
    pub fn scrolled_view(&self, offset: usize) -> (Arc<Grid>, usize) {
        if offset == 0 || !matches!(self.active, Screen::Primary) {
            return (self.active_grid_arc(), 0);
        }

        let grid = self.primary.as_ref();
        let (rows, cols) = grid.dimensions();
        let history = self.scrollback.lines();
        let retained = history.len();
        let scrolled = offset.min(retained);
        if scrolled == 0 {
            return (self.active_grid_arc(), 0);
        }

        // The visible window: the `scrolled` newest history rows, then the live
        // rows, capped at the screen height. The live grid alone is `rows` tall,
        // so the chain always yields a full window.
        let window: Vec<Vec<Cell>> = history
            .iter()
            .skip(retained - scrolled)
            .map(|(cells, _)| cells.clone())
            .chain(grid.rows().iter().cloned())
            .take(rows as usize)
            .collect();
        (
            Arc::new(Grid::from_rows(
                window,
                cols,
                self.primary_render.style.bg_fill(),
            )),
            scrolled,
        )
    }

    /// The window/tab title set by OSC 0/1/2, or `None` if the app has not set
    /// one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The working directory last reported by the shell via OSC 7 (its host and
    /// decoded path), or `None` if none has been reported. Used by cwd
    /// inheritance when spawning a new pane: the spawn layer compares the host
    /// to the local machine before inheriting the path, so a directory reported
    /// from a remote host (e.g. over SSH) is not opened locally.
    pub fn current_cwd(&self) -> Option<&ReportedCwd> {
        self.reported_cwd.as_ref()
    }

    /// Whether the cursor should be drawn — toggled by DECTCEM (`?25`).
    pub fn cursor_visible(&self) -> bool {
        self.active_cursor().is_visible
    }

    /// Whether bracketed-paste mode (`?2004`) is active — the input layer reads
    /// this to decide whether to bracket a paste in `ESC[200~`…`ESC[201~`.
    pub fn bracketed_paste(&self) -> bool {
        self.modes.bracketed_paste
    }

    /// The active mouse tracking level (`?9`/`?1000`/`?1002`/`?1003`) — the
    /// mouse layer reads this to decide which events to report to the app.
    pub fn mouse_tracking(&self) -> MouseTracking {
        self.modes.mouse_tracking
    }

    /// The active mouse report encoding (`?1005`/`?1006`/`?1015`) — the mouse
    /// layer reads this to format the coordinates of a report.
    pub fn mouse_encoding(&self) -> MouseEncoding {
        self.modes.mouse_encoding
    }

    /// Whether alternate-scroll mode (`?1007`) is active — the mouse layer reads
    /// this to translate wheel motion into arrow keys on the alternate screen.
    pub fn alt_scroll(&self) -> bool {
        self.modes.alt_scroll
    }

    /// Whether autowrap (DECAWM `?7`) is active — `print` reads this to decide
    /// whether a glyph at the last column wraps onto a new line. Default on.
    pub fn autowrap(&self) -> bool {
        self.modes.autowrap
    }

    /// Whether application-cursor-keys mode (DECCKM `?1`) is active — the input
    /// layer reads this to pick the arrow-key byte form.
    pub fn app_cursor_keys(&self) -> bool {
        self.modes.app_cursor_keys
    }

    /// Whether reverse-video mode (DECSCNM `?5`) is active — the renderer reads
    /// this to swap foreground and background across the screen.
    pub fn reverse_video(&self) -> bool {
        self.modes.reverse_video
    }

    /// Whether cursor-blink mode (`?12`) is active — the renderer reads this to
    /// blink the cursor cell.
    pub fn cursor_blink(&self) -> bool {
        self.modes.cursor_blink
    }

    /// The pane's scrollback history. The runtime reads its truncation tallies
    /// to emit `PaneScrollbackTruncated`, and the renderer reads its lines to
    /// compose a scrolled-back view.
    pub fn scrollback(&self) -> &Scrollback {
        &self.scrollback
    }

    /// Drain the queued device-query replies (DA/DSR/DECRQM answers), leaving
    /// the queue empty. The caller writes the returned bytes back into the
    /// pane's PTY so the querying app receives its answer.
    #[must_use = "undelivered replies hang the querying app"]
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// The scroll region (top and bottom margins) for the active screen, or
    /// `None` if scrolling uses the full height. Margins are zero-based and
    /// inclusive.
    pub fn scroll_region(&self) -> Option<(u16, u16)> {
        match self.active {
            Screen::Primary => self.primary_scroll_region,
            Screen::Alternate => self.alternate_scroll_region,
        }
    }

    /// Mutable access to the scroll region for the active screen.
    pub fn scroll_region_mut(&mut self) -> &mut Option<(u16, u16)> {
        match self.active {
            Screen::Primary => &mut self.primary_scroll_region,
            Screen::Alternate => &mut self.alternate_scroll_region,
        }
    }

    /// The cursor position `(row, col)` on the active screen, both zero-based.
    pub fn active_cursor_position(&self) -> (u16, u16) {
        (self.active_cursor().row, self.active_cursor().col)
    }

    /// The cursor for the active screen.
    fn active_cursor(&self) -> &Cursor {
        match self.active {
            Screen::Primary => &self.primary_cursor,
            Screen::Alternate => &self.alternate_cursor,
        }
    }

    /// Mutable access to the cursor for the active screen.
    fn active_cursor_mut(&mut self) -> &mut Cursor {
        match self.active {
            Screen::Primary => &mut self.primary_cursor,
            Screen::Alternate => &mut self.alternate_cursor,
        }
    }

    /// The render state (pen, charsets, GL slot) for the active screen.
    fn active_render(&self) -> &RenderState {
        match self.active {
            Screen::Primary => &self.primary_render,
            Screen::Alternate => &self.alternate_render,
        }
    }

    /// Mutable access to the render state for the active screen.
    fn active_render_mut(&mut self) -> &mut RenderState {
        match self.active {
            Screen::Primary => &mut self.primary_render,
            Screen::Alternate => &mut self.alternate_render,
        }
    }

    /// Trim the active screen's `row` to the first `inner_width` columns for
    /// rendering, guarding the right edge against a half-drawn wide glyph.
    ///
    /// Returns the visible cells plus a `right_pad` flag. When the last visible
    /// column holds the left half of a wide glyph (its continuation falls
    /// outside the inner rect), that base is dropped from the returned cells and
    /// `right_pad` is set, telling the renderer to blank the freed column
    /// so it never draws a half glyph. An out-of-range `row`, a zero
    /// `inner_width`, or an empty row yields no cells and no pad. `inner_width`
    /// is clamped to the row length, so a width past the grid is harmless.
    pub fn clip_row(&self, row: u16, inner_width: u16) -> ClippedRow<'_> {
        let rows = self.active_grid().rows();
        let Some(r) = rows.get(row as usize) else {
            return ClippedRow {
                cells: &[],
                right_pad: false,
            };
        };

        let w = min(inner_width as usize, r.len());

        if w > 0 && r[w - 1].width() > 1 {
            ClippedRow {
                cells: &r[..w - 1],
                right_pad: true,
            }
        } else {
            ClippedRow {
                cells: &r[..w],
                right_pad: false,
            }
        }
    }
}

/// Normalize every row to exactly `cols` cells: truncate on the right or pad
/// with blanks in `fill`. A wide glyph whose right (width-0) half falls past
/// the new edge leaves its base as the last cell; that dangling base is
/// blanked so no half glyph survives the crop.
fn crop_columns(rows: &mut [Vec<Cell>], cols: u16, fill: Style) {
    for row in rows {
        row.resize(cols as usize, Cell::blank_with(fill));
        if let Some(last) = row.last_mut() {
            if last.width() > 1 {
                *last = Cell::blank_with(fill);
            }
        }
    }
}

/// True when every cell in `row` is an unadorned space — the only content a
/// blank or continuation cell carries — so the row shows nothing.
fn row_is_blank(row: &[Cell]) -> bool {
    row.iter()
        .all(|cell| cell.ch() == ' ' && cell.combining().is_empty())
}

#[cfg(test)]
mod tests;
