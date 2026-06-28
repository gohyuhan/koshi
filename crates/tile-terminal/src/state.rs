//! Per-pane terminal state: screen buffers, cursor, pen style, modes, title,
//! reported working directory, and scrollback.
//!
//! One [`TerminalState`] backs a single terminal pane; panes never share
//! buffers. The runtime owns the `PaneId → TerminalState` map, so the state
//! itself carries no identity. The VTE performer (see the `perform` submodule)
//! mutates this model as PTY output arrives.

use std::cmp::min;
use std::path::{Path, PathBuf};

use tile_core::process::PtySize;

use crate::grid::state::{Cell, Grid};
use crate::scrollback::{Scrollback, ScrollbackLimit};
use crate::style::Style;
mod perform;

/// Which of the two screen buffers is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The normal, scrolling screen.
    #[default]
    Primary,
    /// The alternate screen used by full-screen apps (e.g. vim, htop).
    Alternate,
}

/// A character set a `G0`–`G3` slot can be designated to, selected into the
/// active GL range by `SI`/`SO` and applied to printed bytes.
///
/// Part of the per-screen [`RenderState`]. Only the three sets real applications
/// use are modeled; an unrecognized designation final byte falls back to
/// [`Ascii`](Charset::Ascii) (a passthrough).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charset {
    /// US-ASCII (`ESC ( B`): every byte prints as itself. The default.
    #[default]
    Ascii,
    /// DEC Special Character and Line Drawing (`ESC ( 0`): the bytes `0x5F`–
    /// `0x7E` print as box-drawing and symbol glyphs (`q` → `─`, `x` → `│`, …),
    /// so a TUI's `lqqqk` renders `┌───┐`.
    DecLineDrawing,
    /// United Kingdom (`ESC ( A`): identical to ASCII except `#` (`0x23`) prints
    /// as `£`.
    Uk,
}

/// The rendering state that turns a printed byte into a styled glyph: the pen,
/// the active GL slot, and the `G0`–`G3` charset designations.
///
/// Held per screen — the primary and the alternate each own one. Every
/// alternate-screen entry (`?47`/`?1047`/`?1049`) clones the primary's render
/// state into the alternate. DECSC snapshots the active screen's render state
/// into a [`SavedCursor`]; DECRC restores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderState {
    /// The pen applied to printed cells (colors + text attributes).
    style: Style,
    /// The `G0`–`G3` charset designations (`ESC ( ) * +`), indexed by slot.
    charsets: [Charset; 4],
    /// Which `G0`–`G3` slot is invoked into the GL range for printing: `0` after
    /// `SI`, `1` after `SO`.
    gl: usize,
}

impl RenderState {
    /// A fresh render state: default pen, all four slots ASCII, GL on `G0`.
    fn fresh() -> Self {
        RenderState {
            style: Style::default(),
            charsets: [Charset::Ascii; 4],
            gl: 0,
        }
    }
}

/// A cursor position and the render state captured by DECSC, restored by DECRC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedCursor {
    /// Saved zero-based row within the grid.
    row: u16,
    /// Saved zero-based column within the grid.
    col: u16,
    /// The deferred-wrap latch at save time, restored alongside the position so
    /// a glyph parked at the last column still wraps after a save/restore.
    pending_wrap: bool,
    /// Snapshot of the active screen's [`RenderState`] (pen, charsets, GL slot)
    /// at save time. DECSC/DECRC carry the whole render state with the cursor, so
    /// an app that changes the pen or a designation, saves, changes it again,
    /// then restores gets the original back.
    render: RenderState,
}

/// The text cursor: position, visibility, and the deferred-wrap latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Zero-based row within the active grid (internally 0-based despite
    /// 1-based ANSI addressing).
    row: u16,
    /// Zero-based column within the active grid.
    col: u16,
    /// Whether the cursor is currently shown (toggled by DEC mode `?25`).
    is_visible: bool,
    /// Deferred-wrap latch (xterm-style): set when a glyph is printed into the
    /// last column, leaving the cursor parked there instead of advancing. The
    /// next printable glyph first wraps to the following line, so a row that
    /// exactly fills the width does not scroll early. Any cursor-moving
    /// operation clears it.
    pending_wrap: bool,
    /// Saved cursor position and style from DECSC/DECRC (xterm form) or
    /// SCOSC/SCORC (ANSI form), kept per screen so each screen buffer has its
    /// own snapshot independent of the other.
    saved: Option<SavedCursor>,
}

/// One screen row trimmed to the renderer's inner width, with a flag for a
/// wide glyph clipped at the right edge.
///
/// Produced by [`TerminalState::clip_row`]. Borrows the live grid row, so it
/// lives only as long as that borrow. A wide glyph (CJK, emoji) occupies two
/// columns; when the inner rect ends between its halves, drawing only the left
/// half would show a broken glyph. `clip_row` instead drops that base from
/// `cells` and sets `right_pad`, telling the renderer to fill the freed column
/// with a blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClippedRow<'a> {
    /// The visible cells, left to right. When `right_pad` is set this stops one
    /// column short of the inner width — the clipped wide base is excluded.
    cells: &'a [Cell],
    /// `true` when the last visible column would have shown only the left half
    /// of a wide glyph; the renderer draws one blank pad cell there instead.
    right_pad: bool,
}

impl<'a> ClippedRow<'a> {
    /// The visible cells, left to right. The renderer draws these, then one
    /// blank pad cell when `right_pad` is set. The slice borrows the underlying
    /// grid row, so it outlives this `ClippedRow`.
    pub fn cells(&self) -> &'a [Cell] {
        self.cells
    }

    /// Whether the renderer should draw one blank pad cell after `cells` to
    /// fill the column a clipped wide glyph would have half-occupied.
    pub fn right_pad(&self) -> bool {
        self.right_pad
    }
}

/// Which mouse events the running app has asked to be reported, set via the DEC
/// private modes `?9`/`?1000`/`?1002`/`?1003`. The levels form a ladder (each
/// reports strictly more than the one above); an app enables exactly one, and
/// the last enabling sequence wins. Independent of [`MouseEncoding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseTracking {
    /// No mouse reporting (default).
    #[default]
    Off,
    /// `?9` X10 compatibility — button presses only, no releases.
    X10,
    /// `?1000` normal tracking — button presses and releases.
    Normal,
    /// `?1002` button-event tracking — presses, releases, and motion while a
    /// button is held (drag).
    ButtonMotion,
    /// `?1003` any-event tracking — all motion, whether or not a button is held.
    AnyMotion,
}

/// How a mouse report's coordinate bytes are encoded, set via the DEC private
/// modes `?1005`/`?1006`/`?1015`. Orthogonal to [`MouseTracking`]: an app sets a
/// tracking level and an encoding independently (e.g. `?1000h` then `?1006h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// Legacy X10 single-byte coordinates (default).
    #[default]
    Default,
    /// `?1005` UTF-8 extended coordinates.
    Utf8,
    /// `?1006` SGR form (`CSI < … M`/`m`) — the encoding modern apps use.
    Sgr,
    /// `?1015` urxvt decimal form.
    Urxvt,
}

/// Terminal mode flags the renderer and input/mouse layers consult: autowrap
/// (`?7`), application cursor keys (`?1`), reverse video (`?5`), cursor blink
/// (`?12`), bracketed paste (`?2004`), the mouse [tracking][MouseTracking] level
/// and [encoding][MouseEncoding] (`?9`/`?1000`/`?1002`/`?1003` and
/// `?1005`/`?1006`/`?1015`), and alternate-scroll (`?1007`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalModes {
    /// `?2004` — wrap pasted text in `ESC[200~`…`ESC[201~` so the app can tell
    /// typed input from a paste.
    pub bracketed_paste: bool,
    /// Which mouse events are reported; see [`MouseTracking`].
    mouse_tracking: MouseTracking,
    /// How mouse reports are encoded; see [`MouseEncoding`].
    mouse_encoding: MouseEncoding,
    /// `?1007` — on the alternate screen, translate wheel motion into cursor
    /// arrow keys instead of emitting a mouse report.
    alt_scroll: bool,
    /// `?7` (DECAWM) — autowrap. When off, a glyph at the last column overwrites
    /// in place instead of parking to wrap onto a new line. Default on.
    autowrap: bool,
    /// `?1` (DECCKM) — application cursor keys: the input layer sends `ESC O A`
    /// rather than `ESC [ A` for the arrow keys.
    app_cursor_keys: bool,
    /// `?5` (DECSCNM) — reverse video: the renderer swaps foreground and
    /// background across the whole screen.
    reverse_video: bool,
    /// `?12` (att610) — cursor blink: the renderer blinks the cursor cell.
    cursor_blink: bool,
}

impl Default for TerminalModes {
    fn default() -> Self {
        TerminalModes {
            bracketed_paste: false,
            mouse_tracking: MouseTracking::Off,
            mouse_encoding: MouseEncoding::Default,
            alt_scroll: false,
            autowrap: true,
            app_cursor_keys: false,
            reverse_video: false,
            cursor_blink: false,
        }
    }
}

/// A working directory reported by the shell via OSC 7: the decoded `path`
/// together with the `host` the shell named in the URI authority.
///
/// The host is kept rather than discarded so the pane-spawn layer can compare
/// it to the local machine and refuse to inherit a directory reported from a
/// *remote* host — e.g. a shell running over SSH reports `file://remote/…`, and
/// opening that path on the local machine would land in the wrong place. The
/// parser stores the report verbatim and makes no local/remote decision; that
/// admission check belongs at the spawn layer that owns the new pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedCwd {
    /// The URI authority (the part between `//` and the path), or `None` when
    /// it was empty (`file:///path`). `localhost` and the local machine's own
    /// hostname both denote the local machine.
    host: Option<String>,
    /// The decoded working-directory path.
    path: PathBuf,
}

impl ReportedCwd {
    /// The host the shell named, or `None` for an empty authority.
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// The decoded working-directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The full emulation state of one terminal pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalState {
    /// The primary (normal, scrolling) screen buffer.
    primary: Grid,
    /// The alternate screen buffer used by full-screen apps; swapped in via DEC
    /// mode `?1049`/`?47` and never appended to the `scrollback`.
    alternate: Grid,
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
            primary: terminal_size.clone(),
            alternate: terminal_size.clone(),
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
        }
    }

    /// Resize both screen buffers to `size` and clamp the cursor into the new
    /// bounds. Existing cell contents are discarded — reflow is not done here.
    pub fn resize(&mut self, size: PtySize) {
        // Blank each screen with its own render background.
        let primary_fill = self.primary_render.style.bg_fill();
        let alternate_fill = self.alternate_render.style.bg_fill();
        self.primary = Grid::blank(size.rows, size.cols, primary_fill);
        self.alternate = Grid::blank(size.rows, size.cols, alternate_fill);

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

        // The resized buffers discard their cells, so any in-progress cluster's
        // base cell is gone; drop the run.
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
            Screen::Primary => &self.primary,
            Screen::Alternate => &self.alternate,
        }
    }

    /// Mutable access to the active screen buffer, for writing cells.
    pub fn active_grid_mut(&mut self) -> &mut Grid {
        match self.active {
            Screen::Primary => &mut self.primary,
            Screen::Alternate => &mut self.alternate,
        }
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
        let cursor_position = (self.active_cursor().row, self.active_cursor().col);
        cursor_position
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
    /// `right_pad` is set so the renderer blanks the freed column rather than
    /// drawing half a glyph. An out-of-range `row`, a zero `inner_width`, or an
    /// empty row yields no cells and no pad. `inner_width` is clamped to the
    /// row length, so a width past the grid is harmless.
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

#[cfg(test)]
mod tests;
