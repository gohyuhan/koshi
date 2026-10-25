//! The render-snapshot builder: freezing live [`Server`] state into the
//! read-only [`RenderSnapshot`] the renderer draws.
//!
//! [`Server::build_snapshot`] takes a `client_id` and produces the world the way that one client
//! sees it: its viewed tab solved into pane rectangles, and each of that tab's panes' terminal
//! grids, cursors, and scrollback line counts copied out. Each pane travels as a
//! [`GridView`]: one shared
//! [`Arc<RowSnapshot>`](koshi_terminal::grid::row_snapshot::RowSnapshot) per visible row, and no
//! copied cell. A pane the client follows live lists its screen rows' snapshots. A write to a row
//! after the snapshot gives that row a new snapshot and leaves the frozen one unchanged. A pane
//! the client has scrolled back in lists the history lines' snapshots above the screen rows'
//! snapshots for that window.
//!
//! The snapshot is per-client, not session-global:
//! `session_snapshot.active_tab_snapshot` holds *this* client's viewed tab, and
//! always names the same tab as `client_snapshot.active_tab_id`, and
//! `session_snapshot.floating_pane_slots` holds the floating panes *this*
//! client shows, at its own positions, while
//! `session_snapshot.session_name`/`tabs_metadata` are the session-wide data.
//!
//! A snapshot carries no hint-bar data: the viewer draws that bar from its own
//! keymap.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use koshi_core::command::{Selection, SelectionKind};
use koshi_core::geometry::FloatingPanePosition;
use koshi_core::ids::{ClientId, PaneId};
use koshi_renderer::snapshot::{
    list_pane_slots, ClientSnapshot, CursorSnapshot, FloatingPane, FloatingPaneSlot, GridView,
    ImagePlacementSnapshot, PaneSnapshot, RenderSnapshot, ScrollbackMetadata, SelectionSpans,
    SessionSnapshot, TabMetadata, TabSnapshot,
};
use koshi_session::client::Client;
use koshi_session::session::state::Session;
use koshi_terminal::scrollback::Scrollback;
use koshi_terminal::selection::order_selection_positions;
use koshi_terminal::state::{Screen, TerminalState};

use crate::server::Server;

impl Server {
    /// Freeze the world the way `client_id` sees it into a [`RenderSnapshot`].
    ///
    /// Returns `None` when no attached client has that id, or its viewed tab has
    /// gone — the caller skips the frame. On success,
    /// `session_snapshot.active_tab_snapshot` is the client's own viewed tab in
    /// the solve the session holds for the client's layout mode
    /// ([`Session::compute_client_solve`]), at the tab's solved size
    /// ([`Tab::get_solved_tab_size`](koshi_session::session::state::Tab::get_solved_tab_size)), so the renderer letterboxes it (centers
    /// it with padding) into this client's larger viewport. A tab whose every
    /// viewer reports
    /// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving) is
    /// solved at `0x0`: every pane is suppressed and the frame carries
    /// `is_every_pane_suppressed`. `session_snapshot.floating_pane_slots`
    /// holds the floating panes the client shows, placed for it.
    /// `session_snapshot.floating_panes` lists every floating pane of the
    /// session, in the order the panes became floating: `Shown` while the
    /// client shows it, and `Minimized` with its title while the client
    /// minimized it. `pane_snapshots` holds one entry per tiled slot, then one
    /// per floating pane slot, then one per pane of the client's placement
    /// preview that neither lists
    /// (`ClientPlacementPreview::list_preview_pane_ids`).
    /// `placement_preview` is the client's preview while it names a target in
    /// this session, else `None`.
    pub fn build_snapshot(&self, client_id: ClientId) -> Option<RenderSnapshot> {
        let session = self.get_session_for_client(client_id)?;
        let client = session.clients.get_client_by_id(client_id)?;
        let active_tab_id = client.get_active_tab_id();
        let tab = session.tabs.get(&active_tab_id)?;
        let layout_mode = client.get_layout_mode(active_tab_id);
        let client_solve = session.compute_client_solve(client, active_tab_id)?;

        let active_tab_snapshot = TabSnapshot {
            tab_id: tab.get_tab_id(),
            tab_name: tab.get_tab_name().to_owned(),
            pane_slots: list_pane_slots(&client_solve),
            tab_size: tab.get_solved_tab_size(),
            stack_headers: client_solve.stack_headers.clone(),
            layout_mode,
            is_every_pane_suppressed: client_solve.is_every_pane_suppressed,
            gap_cell_count: session.get_pane_sizing().gap_cell_count,
        };

        // Metadata for every tab in the session, in display (index) order.
        let mut tabs_metadata: Vec<TabMetadata> = session
            .tabs
            .values()
            .map(|tab| TabMetadata {
                tab_id: tab.get_tab_id(),
                tab_name: tab.get_tab_name().to_owned(),
                tab_index: tab.get_tab_index(),
                is_active: tab.get_tab_id() == active_tab_id,
            })
            .collect();
        tabs_metadata.sort_by_key(|tab_metadata| tab_metadata.tab_index);

        let session_snapshot = SessionSnapshot {
            session_id: session.session_id,
            session_name: session.session_name.clone(),
            active_tab_snapshot,
            tabs_metadata,
            floating_pane_slots: list_floating_pane_slots(session, client),
            floating_panes: session
                .floating_set
                .list_pane_ids()
                .into_iter()
                .map(|pane_id| {
                    if client.is_floating_pane_shown(pane_id) {
                        return FloatingPane::Shown { pane_id };
                    }
                    FloatingPane::Minimized {
                        pane_id,
                        pane_title: self.terminal_engine_by_pane_id.get(&pane_id).and_then(
                            |terminal_engine| {
                                resolve_pane_title(terminal_engine.get_terminal_state())
                            },
                        ),
                    }
                })
                .collect(),
        };

        let client_placement_preview = self.placement_preview_by_client_id.get(&client_id).filter(
            |client_placement_preview| {
                client_placement_preview.get_session_id() == session.session_id
            },
        );
        // One content snapshot per solved slot, in slot order, then one per
        // floating pane slot, back to front, then one per preview pane not
        // listed yet.
        let mut pane_ids: Vec<PaneId> = session_snapshot
            .active_tab_snapshot
            .pane_slots
            .iter()
            .map(|pane_slot| pane_slot.pane_id)
            .chain(
                session_snapshot
                    .floating_pane_slots
                    .iter()
                    .map(|floating_pane_slot| floating_pane_slot.pane_id),
            )
            .collect();
        if let Some(client_placement_preview) = client_placement_preview {
            let mut listed_pane_ids: HashSet<PaneId> = pane_ids.iter().copied().collect();
            for &preview_pane_id in client_placement_preview.list_preview_pane_ids() {
                if listed_pane_ids.insert(preview_pane_id) {
                    pane_ids.push(preview_pane_id);
                }
            }
        }
        let pane_snapshots: Vec<PaneSnapshot> = pane_ids
            .into_iter()
            .map(|pane_id| {
                self.build_pane_snapshot(
                    pane_id,
                    client.get_scroll_offset(pane_id),
                    client.get_selection(pane_id),
                )
            })
            .collect();

        Some(RenderSnapshot {
            is_recovery_notice_visible: session.is_recovery_notice_visible(),
            session_snapshot,
            pane_snapshots,
            client_snapshot: build_client_snapshot(client),
            placement_preview: client_placement_preview.map(|client_placement_preview| {
                Arc::clone(client_placement_preview.get_placement_preview())
            }),
        })
    }

    /// Content snapshot for one pane at scrollback view `scrollback_offset`:
    /// the lines the viewing client has scrolled up from the live bottom, `0`
    /// following live output. The offset is clamped to the pane's retained
    /// line count, and the listed rows and the scroll indicator both read that
    /// clamped value. At `0` the rows are the live screen's row snapshots; a
    /// scrolled-back offset lists a window of history rows above the top of the
    /// live screen. No cell is copied in either case.
    ///
    /// `pane_selection` is the viewing client's highlight in this pane,
    /// resolved here from absolute line numbers to the rows this frame shows.
    ///
    /// A pane with no terminal engine, one not yet spawned, gets
    /// [`PaneSnapshot::from_empty_pane`]: no grid, a hidden cursor, and mouse
    /// tracking off. The renderer draws no cells for it, and a wheel over it
    /// asks nothing of a program.
    #[allow(clippy::needless_pass_by_value)]
    fn build_pane_snapshot(
        &self,
        pane_id: PaneId,
        scrollback_offset: usize,
        pane_selection: Option<Selection>,
    ) -> PaneSnapshot {
        let Some(terminal_engine) = self.terminal_engine_by_pane_id.get(&pane_id) else {
            return PaneSnapshot::from_empty_pane(pane_id);
        };

        let terminal_state = terminal_engine.get_terminal_state();
        let (row_index, column_index) = terminal_state.get_active_cursor_position();
        let scrollback_state = terminal_state.get_scrollback();
        // The engine resolves the requested offset to the rows shown and its
        // effective offset: `0` while following live output or on the alternate
        // screen. The listed rows, the indicator and the cursor suppression all
        // read that effective offset.
        let (row_snapshots, effective_scrollback_offset) =
            terminal_state.get_scrolled_view(scrollback_offset);
        let (_, column_count) = terminal_state.get_active_grid().get_grid_dimensions();
        let terminal_grid_view = GridView {
            row_snapshots,
            column_count,
            view_row_offset: effective_scrollback_offset,
        };
        let image_placement_snapshots = terminal_state
            .list_image_placements_for_view(effective_scrollback_offset)
            .iter()
            .map(ImagePlacementSnapshot::from_placement)
            .collect();
        PaneSnapshot {
            pane_id,
            pane_title: resolve_pane_title(terminal_state),
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
                    &terminal_grid_view,
                    scrollback_state,
                    effective_scrollback_offset,
                )
            }),
            // The same line number `compute_selection_spans` resolves its rows against:
            // the window's top row shows line `total_pushed - scrollback_offset`.
            view_top_row_index: scrollback_state
                .get_total_pushed_line_count()
                .saturating_sub(effective_scrollback_offset as u64),
            terminal_grid_view: Some(terminal_grid_view),
            is_reverse_video: terminal_state.is_reverse_video_enabled(),
            mouse_tracking: terminal_state.get_mouse_tracking(),
            is_alternate_scroll_enabled: terminal_state.is_alternate_scroll_enabled(),
            is_on_alternate_screen: terminal_state.get_active_screen() == Screen::Alternate,
            scrollback_metadata: ScrollbackMetadata {
                retained_line_count: scrollback_state.get_retained_line_count(),
            },
        }
    }
}

/// The floating panes `client` shows, back to front
/// ([`Client::list_shown_floating_pane_ids`]), each placed for it. A slot's
/// outer rect is the rectangle the session places the pane in for the client
/// ([`Session::find_client_floating_pane_rect`]); its content rect comes from
/// that outer rect ([`FloatingPaneSlot::from_outer_rect`]). Empty when the
/// client has no floating viewport ([`Client::get_floating_viewport`]).
///
/// A `72x22` pane at the default position, alone in the session, on a client
/// whose floating viewport is `120x38` → outer rect `(24, 8)` `72x22`, content
/// rect `(25, 11)` `70x18`.
fn list_floating_pane_slots(session: &Session, client: &Client) -> Vec<FloatingPaneSlot> {
    client
        .list_shown_floating_pane_ids()
        .iter()
        .filter_map(|&pane_id| {
            let floating_pane_view = client.get_floating_pane_view(pane_id)?;
            let outer_rect = session.find_client_floating_pane_rect(client, pane_id)?;
            Some(FloatingPaneSlot::from_outer_rect(
                pane_id,
                outer_rect,
                matches!(floating_pane_view.position, FloatingPanePosition::Pinned(_)),
            ))
        })
        .collect()
}

/// The viewer state of `client` that a frame carries: its id, viewport size,
/// active tab, the pane that takes its input, its focused pane in the active
/// tab, lock mode, last applied lock command, floating pane move and pane
/// placement, and whether mouse-select is on.
fn build_client_snapshot(client: &Client) -> ClientSnapshot {
    ClientSnapshot {
        client_id: client.get_client_id(),
        viewport_size: client.get_viewport_size(),
        active_tab_id: client.get_active_tab_id(),
        focused_pane_id: client.get_active_focused_pane_id(),
        active_tab_focused_pane_id: client.get_focused_pane_id(client.get_active_tab_id()),
        lock_mode: client.get_lock_mode(),
        last_applied_lock_command_id: client.get_last_applied_lock_command_id(),
        last_applied_floating_pane_move_command_id: client
            .get_last_applied_floating_pane_move_command_id(),
        last_applied_placement_command_id: client.get_last_applied_placement_command_id(),
        is_mouse_selection_enabled: client.is_mouse_selection_enabled(),
    }
}

/// The display title of the pane whose terminal is `terminal_state`. On the
/// alternate screen it is the app's OSC 0/1/2 title. On the primary screen it
/// is the shell's OSC 7 working directory ([`format_display_path`]), else the
/// OSC title. `None` when the pane reported neither.
///
/// A shell in `/home/user/project` with home `/home/user` gives
/// `~/project`, and `htop` on the alternate screen titled `htop` gives `htop`.
fn resolve_pane_title(terminal_state: &TerminalState) -> Option<String> {
    match terminal_state.get_active_screen() {
        Screen::Alternate => terminal_state.get_title().map(str::to_owned),
        Screen::Primary => terminal_state
            .get_current_working_directory()
            .map(|reported_working_directory| {
                format_display_path(reported_working_directory.get_working_directory_path())
            })
            .or_else(|| terminal_state.get_title().map(str::to_owned)),
    }
}

/// Whether [`resolve_pane_title`] can give `terminal_state` another title than
/// it gave before one terminal advance. The advance started on
/// `active_screen_before`, changed the OSC 0/1/2 title when
/// `is_title_changed`, and changed the OSC 7 working directory when
/// `is_working_directory_changed`.
///
/// - A change of the active screen counts.
/// - On the alternate screen, a title change counts.
/// - On the primary screen, a working directory change counts, and a title
///   change counts while the pane reports no working directory.
///
/// A shell in `/home/user/project` that sets its title to `make` gives
/// `false`, and `cd /tmp` gives `true`.
pub(crate) fn is_pane_title_input_changed(
    terminal_state: &TerminalState,
    active_screen_before: Screen,
    is_title_changed: bool,
    is_working_directory_changed: bool,
) -> bool {
    let active_screen = terminal_state.get_active_screen();
    if active_screen != active_screen_before {
        return true;
    }
    match active_screen {
        Screen::Alternate => is_title_changed,
        Screen::Primary => {
            is_working_directory_changed
                || (is_title_changed && terminal_state.get_current_working_directory().is_none())
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
/// in. The prefix must end on a path boundary — a sibling like `/home/user2`
/// next to home `/home/user` stays whole.
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

/// Cut `selection` down to the rows this frame shows, as a column range per
/// visible row, or [`None`] when none of it is on screen.
///
/// A selection stores absolute line numbers — every line the pane ever pushed
/// into scrollback — while the renderer draws a window of rows numbered from its
/// own top. This is the one place the two meet: the window's top row is line
/// `total_pushed_line_count - scrollback_offset`, so a line `a` draws at row `a - (total_pushed_line_count -
/// scrollback_offset)`, and a row outside `0..rows` is not on screen.
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
    terminal_grid_view: &GridView,
    scrollback: &Scrollback,
    scrollback_offset: usize,
) -> Option<SelectionSpans> {
    let (row_count, column_count) = terminal_grid_view.get_view_dimensions();
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
pub(crate) mod tests;
