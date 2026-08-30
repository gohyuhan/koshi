//! The render-snapshot builder: freezing live [`Server`] state into the
//! read-only [`RenderSnapshot`] the renderer draws.
//!
//! [`Server::build_snapshot`] takes a `client_id` and produces the world the
//! way that one client sees it: its viewed tab solved into pane rectangles, and
//! each of that tab's panes' terminal grids, cursors, and scrollback tallies
//! copied out. A pane the client follows live travels by reference — the
//! per-pane [`Arc<Grid>`](koshi_terminal::grid::state::Grid) handle from
//! [`TerminalState::active_grid_arc`](koshi_terminal::state::TerminalState::active_grid_arc)
//! — and copies no cells; the next write to that pane clones its buffer once
//! (copy-on-write). A pane the client has scrolled back in carries a grid
//! composed for that window instead.
//!
//! The snapshot is per-client, not session-global: `session.active_tab` holds
//! *this* client's viewed tab, and always names the same tab as
//! `client.active_tab`, while `session.name`/`tabs_metadata` are the true
//! session-wide data.
//!
//! `Server::build_layout` is the same work stopping short of the panes: it
//! yields the [`OwnedFrameLayout`] that says where every surface sits, with no
//! grid, title, or highlight. Writing a mouse report to a pane reads only that
//! much.
//!
//! A snapshot carries no hint-bar data: the viewer draws that bar from its own
//! keymap.

use std::collections::HashSet;
use std::sync::OnceLock;

use koshi_core::command::{Selection, SelectionKind};
use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::{ClientId, PaneId};
use koshi_core::mouse::MouseTracking;
use koshi_layout::content::content_rects;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::{solve_with_mode_min, PaneSizing, SolveResult};
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_pane::pane::state::PaneKind;
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, OwnedFrameLayout, PaneSlot, PaneSnapshot,
    PluginUiSnapshot, RenderSnapshot, ScrollbackMeta, SelectionSpans, SessionSnapshot, TabMeta,
    TabSnapshot,
};
use koshi_session::session::state::Tab;
use koshi_terminal::grid::state::Grid;
use koshi_terminal::scrollback::Scrollback;
use koshi_terminal::selection::order;
use koshi_terminal::state::Screen;

use crate::server::Server;

impl Server {
    /// Freeze the world the way `client_id` sees it into a [`RenderSnapshot`].
    ///
    /// Returns `None` when no attached client has that id, or its viewed tab has
    /// gone — the caller skips the frame. On success, `session.active_tab` is the
    /// client's own viewed tab, solved over the tab's effective size (the
    /// per-axis-minimum pane area across every client viewing it), so the
    /// renderer letterboxes it (centers it with padding) into this client's
    /// larger viewport. A tab whose every viewer reports
    /// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving) solves
    /// at `0x0`: every pane is suppressed and the frame carries `all_suppressed`.
    pub fn build_snapshot(&self, client_id: ClientId) -> Option<RenderSnapshot> {
        let layout = self.build_layout(client_id)?;
        let session = self.session_for_client(client_id)?;
        let client = session.clients.get(client_id)?;

        // One content snapshot per solved slot, in slot order.
        let panes: Vec<PaneSnapshot> = layout
            .session
            .active_tab
            .layout_solved
            .iter()
            .map(|slot| {
                self.pane_snapshot(
                    slot.pane_id,
                    client.scroll_offset(slot.pane_id),
                    client.selection(slot.pane_id),
                )
            })
            .collect();

        Some(RenderSnapshot {
            session: layout.session,
            panes,
            client: layout.client,
            plugin_ui: PluginUiSnapshot::default(),
        })
    }

    /// Freeze only where `client_id`'s surfaces sit: the solved layout, the tab
    /// bar's metadata, and the client's own view state.
    ///
    /// Returns `None` on the same terms as
    /// [`build_snapshot`](Self::build_snapshot) — no attached client with that
    /// id, or its viewed tab has gone.
    ///
    /// This is [`build_snapshot`](Self::build_snapshot) without the per-pane
    /// content: no grid, no title, no highlight resolution. Placing a forwarded
    /// mouse report in its pane reads only these fields.
    pub(crate) fn build_layout(&self, client_id: ClientId) -> Option<OwnedFrameLayout> {
        let session = self.session_for_client(client_id)?;
        let client = session.clients.get(client_id)?;
        let active_tab_id = client.active_tab();
        let tab = session.tabs.get(&active_tab_id)?;

        // Solve the active tab's layout over a rect at origin (0, 0) sized to the
        // shared effective size; the renderer offsets it into the client viewport.
        // A tab whose every viewer is starving solves at 0x0, which suppresses
        // every pane.
        //
        // The solve uses THIS client's layout mode: zoom is per-client, so a pane
        // filling the tab for this client can be one tile among several for
        // another client viewing the same tab at the same moment.
        let effective_size = session
            .tab_viewport(active_tab_id)
            .unwrap_or(Size { cols: 0, rows: 0 });
        let layout_mode = client.layout_mode(active_tab_id);
        let sizing = self.pane_sizing();
        let solve = solve_tab(tab, layout_mode, effective_size, sizing);
        let content = content_rects(&solve);

        // One `PaneSlot` per leaf: outer rect from the solve, inner (content)
        // rect from `content_rects`, both in the same solve order. A tab with
        // no room suppresses every pane it holds.
        let suppressed: HashSet<PaneId> = solve.suppressed.iter().copied().collect();
        let layout_solved: Vec<PaneSlot> = solve
            .panes
            .iter()
            .zip(content.iter())
            .map(|(&(pane_id, rect), &(_, inner_rect))| {
                let record = session.panes.get(pane_id);
                PaneSlot {
                    pane_id,
                    rect,
                    inner_rect,
                    kind: record.map_or(PaneKind::Terminal, |record| *record.kind()),
                    visible: inner_rect.is_some(),
                    suppressed: suppressed.contains(&pane_id),
                    dead: record.is_some_and(|record| {
                        matches!(record.lifecycle(), PaneLifecycle::Exited { .. })
                    }),
                }
            })
            .collect();

        let active_tab = TabSnapshot {
            id: tab.id(),
            name: tab.name().to_owned(),
            layout_solved,
            effective_size,
            stack_headers: solve.stack_headers,
            layout_mode,
            all_suppressed: solve.all_suppressed,
            gap: sizing.gap,
        };

        // Metadata for every tab in the session, in display (index) order.
        let mut tabs_metadata: Vec<TabMeta> = session
            .tabs
            .values()
            .map(|t| TabMeta {
                id: t.id(),
                name: t.name().to_owned(),
                index: t.index(),
                active: t.id() == active_tab_id,
            })
            .collect();
        tabs_metadata.sort_by_key(|meta| meta.index);

        Some(OwnedFrameLayout {
            session: SessionSnapshot {
                id: session.id,
                name: session.name.clone(),
                active_tab,
                tabs_metadata,
            },
            client: ClientSnapshot {
                id: client.id(),
                viewport: client.viewport(),
                active_tab: active_tab_id,
                focused_pane: client.focused_pane(active_tab_id),
                lock_mode: client.lock_mode(),
                mouse_select: client.mouse_select(),
            },
        })
    }

    /// Content snapshot for one pane at scrollback view `view_offset` — lines the
    /// viewing client has scrolled up from the live bottom, `0` following live
    /// output. The offset is clamped to the pane's retained line count, and that
    /// clamped value drives both the composed grid and the scroll indicator, so
    /// the two never disagree. At `0` the grid travels by reference (no copy); a
    /// scrolled-back offset composes a window of history over the live screen.
    ///
    /// `selection` is the viewing client's highlight in this pane, resolved here
    /// from absolute line numbers to the rows this frame actually shows.
    ///
    /// A pane with no terminal engine — a plugin pane, or one not yet spawned —
    /// gets `grid_view = None`, a hidden cursor, and no mouse mode at all: the
    /// renderer draws no cells for it, and a wheel over it asks nothing of a
    /// program.
    #[allow(clippy::needless_pass_by_value)]
    fn pane_snapshot(
        &self,
        pane_id: PaneId,
        view_offset: usize,
        selection: Option<Selection>,
    ) -> PaneSnapshot {
        let Some(engine) = self.terminal_engines.get(&pane_id) else {
            return PaneSnapshot {
                id: pane_id,
                title: None,
                cursor: CursorSnapshot {
                    row: 0,
                    col: 0,
                    visible: false,
                    blink: false,
                    shape: None,
                },
                grid_view: None,
                reverse_video: false,
                mouse_tracking: MouseTracking::Off,
                alt_scroll: false,
                on_alt_screen: false,
                selection: None,
                has_selection: false,
                view_top_row: 0,
                scrollback: ScrollbackMeta {
                    truncated: false,
                    retained_lines: 0,
                },
            };
        };

        let state = engine.state();
        let (row, col) = state.active_cursor_position();
        let scrollback = state.scrollback();
        // The engine resolves the requested offset to the grid actually shown and
        // its effective offset (0 while following live or on the alternate
        // screen), so the composed grid, the indicator, and cursor suppression
        // all agree on how far the view is scrolled.
        let (grid, view_offset) = state.scrolled_view(view_offset);
        // On the alternate screen the pane's name is the app's OSC 0/1/2 title.
        // On the primary screen it is the shell's OSC 7 working directory,
        // `~`-shortened, falling back to the OSC title when none was reported.
        let title = match state.active_screen() {
            Screen::Alternate => state.title().map(str::to_owned),
            Screen::Primary => state
                .current_cwd()
                .map(|cwd| display_path(cwd.path()))
                .or_else(|| state.title().map(str::to_owned)),
        };
        PaneSnapshot {
            id: pane_id,
            title,
            cursor: CursorSnapshot {
                row,
                col,
                visible: state.cursor_visible(),
                blink: state.cursor_blink(),
                shape: state.cursor_shape(),
            },
            has_selection: selection.is_some(),
            selection: selection
                .and_then(|selection| selection_spans(&selection, &grid, scrollback, view_offset)),
            // The same line number `selection_spans` resolves its rows against:
            // the window's top row shows line `total_pushed - view_offset`.
            view_top_row: scrollback.total_pushed().saturating_sub(view_offset as u64),
            grid_view: Some(GridView { grid, view_offset }),
            reverse_video: state.reverse_video(),
            mouse_tracking: state.mouse_tracking(),
            alt_scroll: state.alt_scroll(),
            on_alt_screen: state.active_screen() == Screen::Alternate,
            scrollback: ScrollbackMeta {
                truncated: scrollback.dropped_lines() > 0,
                retained_lines: scrollback.len(),
            },
        }
    }
}

/// One path as pane-title text: the user's home directory prefix shortened to
/// `~`, then bounded and filtered by
/// [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text).
///
/// `/tmp/a\u{7f}b` results in `/tmp/ab`.
fn display_path(path: &std::path::Path) -> String {
    koshi_core::text::sanitize_reported_text(&shorten_home(path, home_text()))
}
/// The home directory as display text, read from the environment on the first
/// call and reused after — `HOME`, or `USERPROFILE` on Windows. `None` when
/// neither is set, which leaves every path whole. A later change to either
/// variable does not alter the stored value.
fn home_text() -> Option<&'static str> {
    static HOME: OnceLock<Option<String>> = OnceLock::new();
    HOME.get_or_init(|| {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|home| std::path::Path::new(&home).display().to_string())
    })
    .as_deref()
}

/// The `~`-shortening behind [`display_path`], with the home directory passed
/// in. The prefix must end on a path boundary — a sibling like `/Users/ab2`
/// next to home `/Users/ab` stays whole.
fn shorten_home(path: &std::path::Path, home: Option<&str>) -> String {
    let text = path.display().to_string();
    if let Some(home) = home {
        if let Some(rest) = text.strip_prefix(home) {
            if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
                return format!("~{rest}");
            }
        }
    }
    text
}

/// Solve `tab`'s current layout in `mode` over a `viewport`-sized rect at origin
/// `(0, 0)` — the space `PaneSlot`/content rects live in.
///
/// `mode` is a viewing client's, never the tab's: the tab holds only the tree,
/// and whether a pane is zoomed is a fact about one client's view. Two clients
/// on this tab can pass different modes for the same tree in the same frame.
pub(crate) fn solve_tab(
    tab: &Tab,
    mode: LayoutMode,
    viewport: Size,
    sizing: PaneSizing,
) -> SolveResult {
    solve_with_mode_min(tab.layout(), mode, Rect::at_origin(viewport), sizing)
}

/// Cut `selection` down to the rows this frame shows, as a column range per
/// visible row, or [`None`] when none of it is on screen.
///
/// A selection stores absolute line numbers — every line the pane ever pushed
/// into scrollback — while the renderer draws a window of rows numbered from its
/// own top. This is the one place the two meet: the window's top row is line
/// `total_pushed - view_offset`, so a line `a` draws at row `a - (total_pushed -
/// view_offset)`, and a row outside `0..rows` is not on screen.
///
/// A highlight only partly on screen keeps the part that is: when its first
/// visible row is not the selection's own first row, that row starts at column
/// 0.
///
/// Example — a 5-row, 20-column pane at the live bottom (`view_offset = 0`) with
/// `total_pushed = 100`, and a character selection from line 101 column 12 to
/// line 103 column 4 → rows `[(1, 12, 19), (2, 0, 19), (3, 0, 4)]`: the first
/// row from column 12 to the edge, the middle row whole, the last row up to
/// column 4.
fn selection_spans(
    selection: &Selection,
    grid: &Grid,
    scrollback: &Scrollback,
    view_offset: usize,
) -> Option<SelectionSpans> {
    let (rows, cols) = grid.dimensions();
    if rows == 0 || cols == 0 {
        return None;
    }
    // The absolute line number the window's top row is showing.
    let top = scrollback.total_pushed() as i64 - view_offset as i64;
    let ordered = order(selection.anchor, selection.cursor);
    let first = ordered.start.row as i64 - top;
    let last = ordered.end.row as i64 - top;
    let bottom = i64::from(rows) - 1;
    if last < 0 || first > bottom {
        return None;
    }
    let last_col = cols - 1;
    let mut spans = Vec::new();
    for view_row in first.max(0)..=last.min(bottom) {
        let (start_col, end_col) = match selection.kind {
            // A block is the same columns on every row it covers.
            SelectionKind::Block => (
                ordered.start.col.min(ordered.end.col),
                ordered.start.col.max(ordered.end.col),
            ),
            // The others run with the text: from the start column on the first
            // row, through whole rows, to the end column on the last.
            SelectionKind::Character | SelectionKind::Word | SelectionKind::Line => {
                let start = if view_row == first {
                    ordered.start.col
                } else {
                    0
                };
                let end = if view_row == last {
                    ordered.end.col
                } else {
                    last_col
                };
                (start, end)
            }
        };
        let end_col = end_col.min(last_col);
        if start_col <= end_col {
            spans.push((view_row as u16, start_col, end_col));
        }
    }
    (!spans.is_empty()).then_some(SelectionSpans { rows: spans })
}

#[cfg(test)]
mod tests;
