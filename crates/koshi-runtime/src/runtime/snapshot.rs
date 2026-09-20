//! The render-snapshot builder: freezing live [`Server`] state into the
//! read-only [`RenderSnapshot`] the renderer draws.
//!
//! [`Server::build_snapshot`] takes a `client_id` and produces the world the
//! way that one client sees it: its viewed tab solved into pane rectangles, and
//! each of that tab's panes' terminal grids, cursors, and scrollback tallies
//! copied out. A pane the client follows live travels by reference — the
//! per-pane [`Arc<Grid>`](koshi_terminal::grid::state::Grid) handle from
//! [`TerminalState::get_active_grid_arc`](koshi_terminal::state::TerminalState::get_active_grid_arc)
//! — and copies no cells; the next write to that pane clones its buffer once
//! (copy-on-write). A pane the client has scrolled back in carries a grid
//! composed for that window instead.
//!
//! The snapshot is per-client, not session-global: `session.active_tab` holds
//! *this* client's viewed tab, and always names the same tab as
//! `client.active_tab`, while `session.session_name`/`tabs_metadata` are the true
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
use koshi_layout::content::list_content_rects;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::{solve_layout_with_mode, LayoutSolve, PaneSizing};
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_pane::pane::state::PaneKind;
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, ImagePlacementSnapshot, OwnedFrameLayout, PaneSlot,
    PaneSnapshot, PluginUiSnapshot, RenderSnapshot, ScrollbackMeta, SelectionSpans,
    SessionSnapshot, TabMeta, TabSnapshot,
};
use koshi_session::session::state::Tab;
use koshi_terminal::grid::state::Grid;
use koshi_terminal::scrollback::Scrollback;
use koshi_terminal::selection::order_selection_positions;
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
        let owned_frame_layout = self.build_frame_layout(client_id)?;
        let session = self.get_session_for_client(client_id)?;
        let client = session.clients.get_client_by_id(client_id)?;

        // One content snapshot per solved slot, in slot order.
        let pane_snapshots: Vec<PaneSnapshot> = owned_frame_layout
            .session_snapshot
            .active_tab_snapshot
            .pane_slots
            .iter()
            .map(|pane_slot| {
                self.build_pane_snapshot(
                    pane_slot.pane_id,
                    client.get_scroll_offset(pane_slot.pane_id),
                    client.get_selection(pane_slot.pane_id),
                )
            })
            .collect();

        Some(RenderSnapshot {
            session_snapshot: owned_frame_layout.session_snapshot,
            pane_snapshots,
            client_snapshot: owned_frame_layout.client_snapshot,
            plugin_ui_snapshot: PluginUiSnapshot::default(),
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
    pub(crate) fn build_frame_layout(&self, client_id: ClientId) -> Option<OwnedFrameLayout> {
        let session = self.get_session_for_client(client_id)?;
        let client = session.clients.get_client_by_id(client_id)?;
        let active_tab_id = client.get_active_tab();
        let tab_record = session.tabs.get(&active_tab_id)?;

        // Solve the active tab's layout over a rect at origin (0, 0) sized to the
        // shared effective size; the renderer offsets it into the client viewport.
        // A tab whose every viewer is starving solves at 0x0, which suppresses
        // every pane.
        //
        // The solve uses THIS client's layout mode: zoom is per-client, so a pane
        // filling the tab for this client can be one tile among several for
        // another client viewing the same tab at the same moment.
        let effective_cell_size = session.get_tab_viewport(active_tab_id).unwrap_or(Size {
            column_count: 0,
            row_count: 0,
        });
        let layout_mode = client.get_layout_mode(active_tab_id);
        let pane_sizing = self.get_pane_sizing();
        let layout_solve =
            solve_tab_layout(tab_record, layout_mode, effective_cell_size, pane_sizing);
        let computed_content_rects = list_content_rects(&layout_solve);

        // One `PaneSlot` per leaf: outer rect from the solve, inner (content)
        // rect from `computed_content_rects`, both in the same solve order. A tab with
        // no room suppresses every pane it holds.
        let suppressed_pane_ids: HashSet<PaneId> =
            layout_solve.suppressed_pane_ids.iter().copied().collect();
        let pane_slots: Vec<PaneSlot> = layout_solve
            .pane_rects
            .iter()
            .zip(computed_content_rects.iter())
            .map(
                |(&(pane_id, outer_rect), &(content_pane_id, content_rect))| {
                    debug_assert_eq!(pane_id, content_pane_id);
                    let pane_record = session.panes.get_pane_record_by_id(pane_id);
                    PaneSlot {
                        pane_id,
                        outer_rect,
                        content_rect,
                        pane_kind: pane_record.map_or(PaneKind::Terminal, |pane_record| {
                            *pane_record.get_pane_kind()
                        }),
                        is_visible: content_rect.is_some(),
                        is_suppressed: suppressed_pane_ids.contains(&pane_id),
                        is_dead: pane_record.is_some_and(|pane_record| {
                            matches!(pane_record.get_lifecycle(), PaneLifecycle::Exited { .. })
                        }),
                    }
                },
            )
            .collect();

        let active_tab_snapshot = TabSnapshot {
            tab_id: tab_record.get_tab_id(),
            tab_name: tab_record.get_tab_name().to_owned(),
            pane_slots,
            effective_cell_size,
            stack_headers: layout_solve.stack_headers,
            layout_mode,
            are_all_panes_suppressed: layout_solve.is_all_panes_suppressed,
            gap_cell_count: pane_sizing.gap_cell_count,
        };

        // Metadata for every tab in the session, in display (index) order.
        let mut tabs_metadata: Vec<TabMeta> = session
            .tabs
            .values()
            .map(|tab_record| TabMeta {
                tab_id: tab_record.get_tab_id(),
                tab_name: tab_record.get_tab_name().to_owned(),
                tab_index: tab_record.get_tab_index(),
                is_active: tab_record.get_tab_id() == active_tab_id,
            })
            .collect();
        tabs_metadata.sort_by_key(|tab_metadata| tab_metadata.tab_index);

        Some(OwnedFrameLayout {
            session_snapshot: SessionSnapshot {
                session_id: session.session_id,
                session_revision: session.get_placement_revision(),
                session_name: session.session_name.clone(),
                active_tab_snapshot,
                tabs_metadata,
            },
            client_snapshot: ClientSnapshot {
                client_id: client.get_client_id(),
                client_revision: client.get_placement_revision(),
                viewport_size: client.get_viewport_size(),
                active_tab_id,
                focused_pane_id: client.get_focused_pane(active_tab_id),
                lock_mode: client.get_lock_mode(),
                is_mouse_selection_enabled: client.is_mouse_selection_enabled(),
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
    fn build_pane_snapshot(
        &self,
        pane_id: PaneId,
        scrollback_offset: usize,
        pane_selection: Option<Selection>,
    ) -> PaneSnapshot {
        let Some(terminal_engine) = self.terminal_engine_by_pane_id.get(&pane_id) else {
            return PaneSnapshot {
                pane_id,
                pane_title: None,
                cursor_snapshot: CursorSnapshot {
                    row_index: 0,
                    column_index: 0,
                    is_visible: false,
                    is_blinking: false,
                    shape: None,
                },
                terminal_grid_view: None,
                image_placement_snapshots: Vec::new(),
                is_reverse_video: false,
                mouse_tracking: MouseTracking::Off,
                is_alternate_scroll_enabled: false,
                is_on_alternate_screen: false,
                selection_spans: None,
                has_selection: false,
                view_top_row_index: 0,
                scrollback_meta: ScrollbackMeta {
                    is_truncated: false,
                    retained_line_count: 0,
                },
            };
        };

        let terminal_state = terminal_engine.get_terminal_state();
        let (row_index, column_index) = terminal_state.get_active_cursor_position();
        let scrollback_state = terminal_state.get_scrollback();
        // The engine resolves the requested offset to the grid actually shown and
        // its effective offset (0 while following live or on the alternate
        // screen), so the composed grid, the indicator, and cursor suppression
        // all agree on how far the view is scrolled.
        let (terminal_grid, effective_scrollback_offset) =
            terminal_state.scrolled_view(scrollback_offset);
        let image_placement_snapshots = terminal_state
            .list_image_placements_for_view(effective_scrollback_offset)
            .iter()
            .map(ImagePlacementSnapshot::from_placement)
            .collect();
        // On the alternate screen the pane's name is the app's OSC 0/1/2 title.
        // On the primary screen it is the shell's OSC 7 working directory,
        // `~`-shortened, falling back to the OSC title when none was reported.
        let pane_title = match terminal_state.get_active_screen() {
            Screen::Alternate => terminal_state.get_title().map(str::to_owned),
            Screen::Primary => terminal_state
                .get_current_working_directory()
                .map(|reported_working_directory| {
                    format_display_path(reported_working_directory.get_working_directory_path())
                })
                .or_else(|| terminal_state.get_title().map(str::to_owned)),
        };
        PaneSnapshot {
            pane_id,
            pane_title,
            cursor_snapshot: CursorSnapshot {
                row_index,
                column_index,
                is_visible: terminal_state.is_cursor_visible(),
                is_blinking: terminal_state.is_cursor_blink_enabled(),
                shape: terminal_state.get_cursor_shape(),
            },
            has_selection: pane_selection.is_some(),
            image_placement_snapshots,
            selection_spans: pane_selection.and_then(|pane_selection| {
                compute_selection_spans(
                    &pane_selection,
                    &terminal_grid,
                    scrollback_state,
                    effective_scrollback_offset,
                )
            }),
            // The same line number `compute_selection_spans` resolves its rows against:
            // the window's top row shows line `total_pushed - scrollback_offset`.
            view_top_row_index: scrollback_state
                .get_total_pushed_line_count()
                .saturating_sub(effective_scrollback_offset as u64),
            terminal_grid_view: Some(GridView {
                grid: terminal_grid,
                view_row_offset: effective_scrollback_offset,
            }),
            is_reverse_video: terminal_state.is_reverse_video_enabled(),
            mouse_tracking: terminal_state.get_mouse_tracking(),
            is_alternate_scroll_enabled: terminal_state.is_alternate_scroll_enabled(),
            is_on_alternate_screen: terminal_state.get_active_screen() == Screen::Alternate,
            scrollback_meta: ScrollbackMeta {
                is_truncated: scrollback_state.get_dropped_line_count() > 0,
                retained_line_count: scrollback_state.get_retained_line_count(),
            },
        }
    }
}

/// One path as pane-title text: the user's home directory prefix shortened to
/// `~`, then bounded and filtered by
/// [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text).
///
/// `/tmp/a\u{7f}b` results in `/tmp/ab`.
fn format_display_path(display_path: &std::path::Path) -> String {
    koshi_core::text::sanitize_reported_text(&shorten_home_path(display_path, get_home_path_text()))
}
/// The home directory as display text, read from the environment on the first
/// call and reused — `HOME`, or `USERPROFILE` on Windows. `None` when neither
/// is set, which leaves every path whole. A change to either variable after the
/// first call does not alter the stored value.
fn get_home_path_text() -> Option<&'static str> {
    static HOME: OnceLock<Option<String>> = OnceLock::new();
    HOME.get_or_init(|| {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|home_path| std::path::Path::new(&home_path).display().to_string())
    })
    .as_deref()
}

/// The `~`-shortening behind [`format_display_path`], with the home directory passed
/// in. The prefix must end on a path boundary — a sibling like `/Users/ab2`
/// next to home `/Users/ab` stays whole.
fn shorten_home_path(display_path: &std::path::Path, home_path_text: Option<&str>) -> String {
    let display_path_text = display_path.display().to_string();
    if let Some(home_path_text) = home_path_text {
        if let Some(relative_path_text) = display_path_text.strip_prefix(home_path_text) {
            if relative_path_text.is_empty()
                || relative_path_text.starts_with('/')
                || relative_path_text.starts_with('\\')
            {
                return format!("~{relative_path_text}");
            }
        }
    }
    display_path_text
}

/// Solve `tab`'s current layout in `mode` over a `viewport`-sized rect at origin
/// `(0, 0)` — the space `PaneSlot`/content rects live in.
///
/// `mode` is a viewing client's, never the tab's: the tab holds only the tree,
/// and whether a pane is zoomed is a fact about one client's view. Two clients
/// on this tab can pass different modes for the same tree in the same frame.
pub(crate) fn solve_tab_layout(
    tab: &Tab,
    layout_mode: LayoutMode,
    effective_cell_size: Size,
    pane_sizing: PaneSizing,
) -> LayoutSolve {
    solve_layout_with_mode(
        tab.get_layout_tree(),
        layout_mode,
        Rect::from_size_at_origin(effective_cell_size),
        pane_sizing,
    )
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
fn compute_selection_spans(
    selection: &Selection,
    terminal_grid: &Grid,
    scrollback: &Scrollback,
    scrollback_offset: usize,
) -> Option<SelectionSpans> {
    let (row_count, column_count) = terminal_grid.get_grid_dimensions();
    if row_count == 0 || column_count == 0 {
        return None;
    }
    // The absolute line number the window's top row is showing.
    let top_row_index = scrollback.get_total_pushed_line_count() as i64 - scrollback_offset as i64;
    let ordered_selection_positions = order_selection_positions(selection.anchor, selection.cursor);
    let first_visible_row_index =
        ordered_selection_positions.start_position.row_index as i64 - top_row_index;
    let last_visible_row_index =
        ordered_selection_positions.end_position.row_index as i64 - top_row_index;
    let bottom_row_index = i64::from(row_count) - 1;
    if last_visible_row_index < 0 || first_visible_row_index > bottom_row_index {
        return None;
    }
    let last_column_index = column_count - 1;
    let mut row_spans = Vec::new();
    for visible_row_index in
        first_visible_row_index.max(0)..=last_visible_row_index.min(bottom_row_index)
    {
        let (start_column_index, end_column_index) = match selection.selection_kind {
            // A block is the same columns on every row it covers.
            SelectionKind::Block => (
                ordered_selection_positions
                    .start_position
                    .column_index
                    .min(ordered_selection_positions.end_position.column_index),
                ordered_selection_positions
                    .start_position
                    .column_index
                    .max(ordered_selection_positions.end_position.column_index),
            ),
            // The others run with the text: from the start column on the first
            // row, through whole rows, to the end column on the last.
            SelectionKind::Character | SelectionKind::Word | SelectionKind::Line => {
                let start_column_index = if visible_row_index == first_visible_row_index {
                    ordered_selection_positions.start_position.column_index
                } else {
                    0
                };
                let end_column_index = if visible_row_index == last_visible_row_index {
                    ordered_selection_positions.end_position.column_index
                } else {
                    last_column_index
                };
                (start_column_index, end_column_index)
            }
        };
        let end_column_index = end_column_index.min(last_column_index);
        if start_column_index <= end_column_index {
            row_spans.push((
                visible_row_index as u16,
                start_column_index,
                end_column_index,
            ));
        }
    }
    (!row_spans.is_empty()).then_some(SelectionSpans { row_spans })
}

#[cfg(test)]
mod tests;
