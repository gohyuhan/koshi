//! Text selection by mouse: turning a drag over a pane's content into a
//! highlight, and scrolling the view while a drag is held past an edge.
//!
//! The gesture is three steps. A press on the focused pane's content records
//! where the drag started and which shape the run of clicks picked — one click
//! characters, two words, three lines, `Alt` a block — and drops any highlight
//! that pane already had. Each drag that follows resolves the pointer to a
//! position in the pane's text and asks for a highlight from the anchor to
//! there, through the ordinary command pipeline
//! ([`VisualCommand::SetSelection`]). The release ends the gesture and leaves
//! the highlight standing.
//!
//! **Nothing is highlighted until the pointer actually moves.** A press stores
//! an anchor and no highlight, so a plain click leaves nothing behind — and in
//! particular leaves no empty highlight, which would hold the pane's view
//! against live output forever with nothing on screen to explain why.
//!
//! # Holding a drag past the edge
//!
//! Dragging below a pane's last row keeps selecting: the view scrolls and the
//! highlight follows. The pointer sitting still outside the pane sends no
//! events, though — so the scroll cannot be driven by drag events, and a timer
//! carries it instead. While the pointer is outside, the drag arms a wakeup
//! every 15ms; each firing scrolls one line and re-extends the highlight from
//! the pointer's last known cell, then re-arms. Moving back inside disarms it,
//! and so does reaching a limit — the oldest retained line, or the live
//! bottom — where a firing would move nothing; the next drag event re-arms.

use std::time::{Duration, Instant};

use koshi_core::command::{
    ClearSelectionArgs, GridPos, Selection, SelectionKind, SetSelectionArgs, VisualCommand,
};
use koshi_core::geometry::Point;
use koshi_core::ids::{ClientId, PaneId};
use koshi_core::key::ModFlags;
use koshi_session::client::{ClickCount, SelectionDragState};

use crate::runtime::state::Runtime;

/// How long after a press a second press still counts as a double click, and a
/// third as a triple.
///
/// A mouse reports a double click as two ordinary presses — no terminal protocol
/// carries a click count — so this gap is the only thing that tells one from
/// two deliberate clicks. Matches the 400ms alacritty settled on.
pub(crate) const CLICK_THRESHOLD: Duration = Duration::from_millis(400);

/// How often the view scrolls while a selection drag is held past a pane's top
/// or bottom edge. Matches alacritty's selection-scrolling interval.
const SELECTION_SCROLL_INTERVAL: Duration = Duration::from_millis(15);

/// Lines the view scrolls per firing while a drag is held past an edge.
const SELECTION_SCROLL_LINES: usize = 1;

impl Runtime {
    /// Begin a selection drag in `pane_id`: record where it started and the
    /// shape `clicks` picked, drop any highlight the pane already had, and — for
    /// a double or triple click — highlight the word or line straight away.
    ///
    /// The old highlight goes now rather than on the first drag, so a plain
    /// click — press and release with no movement — clears the pane's highlight
    /// and leaves nothing in its place, which is how a click behaves in an
    /// editor or a browser.
    ///
    /// **Whether the press itself highlights depends on the shape**, because the
    /// gestures end differently:
    ///
    /// - One click names a *point*, so it needs a drag to name any text. A press
    ///   alone highlights nothing — an empty highlight would still be an entry
    ///   in the client's map, holding the pane's view against live output with
    ///   nothing on screen to explain why.
    /// - A double or triple click names *text on its own* — the word or line
    ///   under the pointer — and is complete without a drag. Waiting for one
    ///   would mean double-clicking a word did nothing at all. What it names is
    ///   never empty, so it cannot leave a view held over nothing.
    ///
    /// A drag afterwards extends from the same anchor either way.
    ///
    /// `Alt` held at the press makes it a block selection whatever the run of
    /// clicks was: a rectangle is a different shape, not a different amount of
    /// text, so it does not compete with word or line — and like a plain drag it
    /// names a point until the pointer moves.
    pub(crate) fn begin_selection_drag(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        at: Point,
        clicks: ClickCount,
        mods: ModFlags,
    ) {
        let Some(anchor) = self.text_pos_at(client_id, pane_id, at) else {
            return;
        };
        let kind = if mods.contains(ModFlags::ALT) {
            SelectionKind::Block
        } else {
            clicks.selection_kind()
        };
        self.dispatch_visual(
            client_id,
            VisualCommand::ClearSelection(ClearSelectionArgs { pane: pane_id }),
        );
        let drag = SelectionDragState {
            pane: pane_id,
            kind,
            anchor,
            at,
            scroll_at: None,
        };
        if let Some(client) = self.client_mut(client_id) {
            client.set_selection_drag(Some(drag));
        }
        if matches!(kind, SelectionKind::Word | SelectionKind::Line) {
            // Both ends are the press, which snapping then grows outward to the
            // whole word or line.
            self.extend_selection(client_id, drag, at);
        }
    }

    /// Extend the in-flight selection drag to the pointer at `at`.
    ///
    /// A pointer inside the pane highlights from the anchor to the cell under
    /// it. A pointer past the top or bottom edge highlights to the pane's
    /// nearest row and arms the scroll timer, so holding it there keeps pulling
    /// more text in. A pointer only to the left or right of the pane clamps to
    /// the edge column without scrolling — there is no more text sideways.
    pub(crate) fn drag_selection_to(&mut self, client_id: ClientId, at: Point, now: Instant) {
        let Some(drag) = self
            .client_mut(client_id)
            .and_then(|client| client.selection_drag())
        else {
            return;
        };
        let scroll_at = self
            .edge_scroll_direction(client_id, drag.pane, at)
            .map(|_| now + SELECTION_SCROLL_INTERVAL);
        if let Some(client) = self.client_mut(client_id) {
            client.set_selection_drag(Some(SelectionDragState {
                at,
                scroll_at,
                ..drag
            }));
        }
        self.extend_selection(client_id, drag, at);
    }

    /// End the in-flight selection drag: copy what it highlighted, leaving the
    /// highlight standing.
    ///
    /// The clipboard follows the OS, with no copy key — releasing the
    /// selection IS the copy, as zellij ships it. The highlighted text is
    /// read at this instant, while it is exactly what the user saw, and goes
    /// to the client's outer terminal as OSC 52, which sets the OS clipboard.
    /// Pressing the OS copy key afterward is a harmless no-op (the clipboard
    /// is already filled) — and being a key, it reaches the pane and so
    /// clears the highlight, like any input reaching the pane's child. A
    /// plain click, whose press highlighted nothing, copies nothing.
    pub(crate) fn end_selection_drag(&mut self, client_id: ClientId) {
        let Some(drag) = self
            .client_mut(client_id)
            .and_then(|client| client.selection_drag())
        else {
            return;
        };
        if let Some(client) = self.client_mut(client_id) {
            client.set_selection_drag(None);
        }
        let Some(selection) = self
            .client_mut(client_id)
            .and_then(|client| client.selection(drag.pane))
        else {
            return;
        };
        if let Some(engine) = self.terminal_engines.get(&drag.pane) {
            let text =
                koshi_terminal::selection::selection_text(&engine.state().text_view(), &selection);
            if !text.is_empty() {
                let sequence = crate::runtime::clipboard::osc52_copy(&text);
                self.queue_host_write(client_id, &sequence);
            }
        }
    }

    /// Highlight from `drag`'s anchor to the pointer at `at`, snapped to whole
    /// words or lines when the drag's shape asks for it.
    ///
    /// Goes through [`Runtime::dispatch`] like every other mutation, so the
    /// highlight lands in one place and emits its event there.
    fn extend_selection(&mut self, client_id: ClientId, drag: SelectionDragState, at: Point) {
        let Some(cursor) = self.text_pos_at(client_id, drag.pane, at) else {
            return;
        };
        let Some((anchor, cursor)) = self.snap(drag.pane, drag.kind, drag.anchor, cursor) else {
            return;
        };
        self.dispatch_visual(
            client_id,
            VisualCommand::SetSelection(SetSelectionArgs {
                pane: drag.pane,
                selection: Selection {
                    kind: drag.kind,
                    anchor,
                    cursor,
                },
            }),
        );
    }

    /// Grow `anchor` and `cursor` outward to whole words or whole lines, per
    /// `kind`. Each end grows away from the other, so the pair always covers the
    /// text between them however the drag runs.
    ///
    /// `hello world` with a word drag from the `e` of `hello` to the `o` of
    /// `world`: the anchor falls back to the `h` and the cursor runs on to the
    /// `d`, giving `hello world` entire. Character and block selections snap
    /// nothing — they mean exactly the cells the pointer named.
    fn snap(
        &self,
        pane_id: PaneId,
        kind: SelectionKind,
        anchor: GridPos,
        cursor: GridPos,
    ) -> Option<(GridPos, GridPos)> {
        if matches!(kind, SelectionKind::Character | SelectionKind::Block) {
            return Some((anchor, cursor));
        }
        let engine = self.terminal_engines.get(&pane_id)?;
        let view = engine.state().text_view();
        // Which end leads decides which way each one grows.
        let forward = (anchor.row, anchor.col) <= (cursor.row, cursor.col);
        let (first, last) = if forward {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        let (first, last) = match kind {
            SelectionKind::Word => {
                let (start_row, start_col) = view.word_start(first.row, first.col);
                let (end_row, end_col) = view.word_end(last.row, last.col);
                (
                    GridPos {
                        row: start_row,
                        col: start_col,
                    },
                    GridPos {
                        row: end_row,
                        col: end_col,
                    },
                )
            }
            SelectionKind::Line => (
                GridPos {
                    row: view.line_start(first.row),
                    col: 0,
                },
                GridPos {
                    row: view.line_end(last.row),
                    col: view.cols().saturating_sub(1),
                },
            ),
            SelectionKind::Character | SelectionKind::Block => (first, last),
        };
        Some(if forward {
            (first, last)
        } else {
            (last, first)
        })
    }

    /// The position in `pane_id`'s text that the client cell `at` names, with a
    /// point outside the pane pulled to its nearest edge so a drag that left the
    /// pane still selects up to it.
    ///
    /// The row is absolute — it counts every line the pane has ever pushed into
    /// scrollback — so it keeps meaning the same line as output arrives and as
    /// the oldest history is dropped. A rendered row `r` at effective view
    /// offset `v` is line `total_pushed - v + r`: at the live bottom (`v = 0`)
    /// the screen's top row is `total_pushed`, and scrolling up `v` lines walks
    /// the whole window back by `v`.
    fn text_pos_at(&mut self, client_id: ClientId, pane_id: PaneId, at: Point) -> Option<GridPos> {
        let (col, row) = self.pane_cell_clamped(client_id, pane_id, at)?;
        let offset = self.view_offset(client_id, pane_id);
        let engine = self.terminal_engines.get(&pane_id)?;
        let state = engine.state();
        let effective = state.effective_view_offset(offset) as u64;
        let total_pushed = state.scrollback().total_pushed();
        let row = (total_pushed + u64::from(row)).saturating_sub(effective);
        // The blank right half of a wide (CJK/emoji) glyph is a width-0 cell
        // the renderer never paints; the glyph's text lives in its left half.
        // A pointer on either half names the glyph's own cell, so a highlight
        // never covers only an invisible cell.
        let view = state.text_view();
        let mut col = col;
        while col > 0 && view.cell(row, col).is_some_and(|cell| cell.width() == 0) {
            col -= 1;
        }
        Some(GridPos { row, col })
    }

    /// The 0-based cell inside `pane_id`'s content that `at` names, pulled to the
    /// nearest edge when `at` is outside it.
    fn pane_cell_clamped(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        at: Point,
    ) -> Option<(u16, u16)> {
        let snapshot = self.build_snapshot(client_id)?;
        let rect = koshi_renderer::pane_content_rect(&snapshot, pane_id)?;
        let right = rect.origin.x + rect.size.cols.saturating_sub(1);
        let bottom = rect.origin.y + rect.size.rows.saturating_sub(1);
        Some((
            at.x.clamp(rect.origin.x, right) - rect.origin.x,
            at.y.clamp(rect.origin.y, bottom) - rect.origin.y,
        ))
    }

    /// This client's scrollback view offset for `pane_id`.
    fn view_offset(&self, client_id: ClientId, pane_id: PaneId) -> usize {
        self.session_for_client(client_id)
            .and_then(|session| session.clients.get(client_id))
            .map_or(0, |client| client.scroll_offset(pane_id))
    }

    /// Which way the view must scroll for a drag held at `at`: `-1` above the
    /// pane's first row, `1` below its last, and `None` while the pointer is
    /// level with the pane.
    ///
    /// Only the vertical edges scroll. Past the left or right edge there is no
    /// further text to reach, so the highlight clamps to the edge column and
    /// stays put.
    fn edge_scroll_direction(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        at: Point,
    ) -> Option<i8> {
        let snapshot = self.build_snapshot(client_id)?;
        let rect = koshi_renderer::pane_content_rect(&snapshot, pane_id)?;
        let bottom = rect.origin.y + rect.size.rows.saturating_sub(1);
        if at.y < rect.origin.y {
            Some(-1)
        } else if at.y > bottom {
            Some(1)
        } else {
            None
        }
    }

    /// How long until a held selection drag must scroll again, or `None` when no
    /// client has one held past an edge. The event loop blocks no longer than
    /// this, so a still pointer keeps pulling text in.
    pub fn next_selection_scroll_wakeup(&self, now: Instant) -> Option<Duration> {
        self.sessions
            .values()
            .flat_map(|session| session.clients.list_attached())
            .filter_map(|client| client.selection_drag()?.scroll_at)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    /// Scroll and re-extend every selection drag whose scroll is due at `now`.
    ///
    /// Each firing moves the view one line toward the pointer and re-extends the
    /// highlight from the pointer's last known cell — which is still outside the
    /// pane, so the extension clamps to the edge row and the highlight grows by
    /// the line the scroll just revealed.
    pub fn expire_selection_scrolls(&mut self, now: Instant) {
        let due: Vec<ClientId> = self
            .sessions
            .values()
            .flat_map(|session| session.clients.list_attached())
            .filter(|client| {
                client
                    .selection_drag()
                    .and_then(|drag| drag.scroll_at)
                    .is_some_and(|at| at <= now)
            })
            .map(koshi_session::client::Client::id)
            .collect();
        for client_id in due {
            self.scroll_selection_drag(client_id, now);
        }
    }

    /// One scroll step for `client_id`'s held drag: move the view a line toward
    /// the pointer, re-extend the highlight, and re-arm the next step. A view
    /// already at its limit — the oldest retained line, or the live bottom —
    /// moves nothing and disarms instead; the next drag event re-arms.
    fn scroll_selection_drag(&mut self, client_id: ClientId, now: Instant) {
        let Some(drag) = self
            .client_mut(client_id)
            .and_then(|client| client.selection_drag())
        else {
            return;
        };
        let Some(direction) = self.edge_scroll_direction(client_id, drag.pane, drag.at) else {
            // The pane moved out from under a pointer that is no longer outside
            // it; stop scrolling and let the next drag event drive.
            self.disarm_selection_scroll(client_id, drag);
            return;
        };
        let before = self.view_offset(client_id, drag.pane);
        if direction < 0 {
            self.scroll_up(client_id, drag.pane, SELECTION_SCROLL_LINES);
        } else {
            self.scroll_down(client_id, drag.pane, SELECTION_SCROLL_LINES);
        }
        if self.view_offset(client_id, drag.pane) == before {
            // Nowhere left to go: the highlight already reaches this edge, so
            // firing again every 15ms would only repeat the same highlight.
            self.disarm_selection_scroll(client_id, drag);
            return;
        }
        if let Some(client) = self.client_mut(client_id) {
            client.set_selection_drag(Some(SelectionDragState {
                scroll_at: Some(now + SELECTION_SCROLL_INTERVAL),
                ..drag
            }));
        }
        self.extend_selection(client_id, drag, drag.at);
    }

    /// Stop `client_id`'s drag from scrolling, leaving the drag itself in place.
    fn disarm_selection_scroll(&mut self, client_id: ClientId, drag: SelectionDragState) {
        if let Some(client) = self.client_mut(client_id) {
            client.set_selection_drag(Some(SelectionDragState {
                scroll_at: None,
                ..drag
            }));
        }
    }

    /// Drop every highlight in `pane_id` whose lines have all been dropped from
    /// the pane's text — erased by the child (`CSI 3 J`) or evicted by the
    /// scrollback cap.
    ///
    /// Such a highlight can never draw again, yet it would keep holding its
    /// client's view against live output ([`Client::is_view_held`]) with
    /// nothing on screen to explain why; dropping it lets the view follow live
    /// output again. A highlight with any line still retained keeps what
    /// remains.
    ///
    /// The scroll offset stays. After the drop, `offset > 0` with no highlight
    /// is exactly the state of a client who scrolled up by hand, and it behaves
    /// the same way: the view stays where it is until the client scrolls down.
    ///
    /// [`Client::is_view_held`]: koshi_session::client::Client::is_view_held
    pub(crate) fn drop_evicted_selections(&mut self, pane_id: PaneId) {
        let Some(engine) = self.terminal_engines.get(&pane_id) else {
            return;
        };
        let first_row = engine.state().text_view().first_row();
        let Some(session) = self.session_for_pane_mut(pane_id) else {
            return;
        };
        for client in session.clients.list_attached_mut() {
            let dead = client.selection(pane_id).is_some_and(|selection| {
                koshi_terminal::selection::order(selection.anchor, selection.cursor)
                    .end
                    .row
                    < first_row
            });
            if dead {
                client.clear_selection(pane_id);
            }
        }
    }

    /// Drop `client_id`'s highlight in `pane_id` because its input reached the
    /// pane's child: the key or click belongs to the program running there, so
    /// the highlight gets out of the way, the way typing replaces a selection
    /// in an editor. A client with no highlight there dispatches nothing.
    pub(crate) fn clear_selection_on_pane_input(&mut self, client_id: ClientId, pane_id: PaneId) {
        let highlighted = self
            .client_mut(client_id)
            .is_some_and(|client| client.selection(pane_id).is_some());
        if highlighted {
            self.dispatch_visual(
                client_id,
                VisualCommand::ClearSelection(ClearSelectionArgs { pane: pane_id }),
            );
        }
    }

    /// Drop every client's highlight in `pane_id`, and any drag selecting in it.
    ///
    /// Called when the pane switches between its primary and alternate screens.
    /// A row number counts the lines the pane pushed into scrollback, which the
    /// alternate screen does not have and does not share — so a highlight made
    /// on one screen names nothing on the other, and the text it was on is not
    /// displayed either way.
    pub(crate) fn clear_pane_selections(&mut self, pane_id: PaneId) {
        let Some(session) = self.session_for_pane_mut(pane_id) else {
            return;
        };
        for client in session.clients.list_attached_mut() {
            client.clear_selection(pane_id);
            if client
                .selection_drag()
                .is_some_and(|drag| drag.pane == pane_id)
            {
                client.set_selection_drag(None);
            }
        }
    }
}

#[cfg(test)]
mod tests;
