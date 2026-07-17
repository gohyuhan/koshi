//! The render-snapshot builder: freezing live [`Runtime`] state into the
//! read-only [`RenderSnapshot`] the renderer draws.
//!
//! [`Runtime::build_snapshot`] takes a `client_id` and produces the world the
//! way that one client sees it: its viewed tab solved into pane rectangles, and
//! each of that tab's panes' terminal grids, cursors, and scrollback tallies
//! copied out. The grid itself travels by reference — a per-pane
//! [`Arc<Grid>`](koshi_terminal::grid::state::Grid) handle from
//! [`TerminalState::active_grid_arc`](koshi_terminal::state::TerminalState::active_grid_arc),
//! so freezing a frame does not copy any cells; the next write to a pane clones
//! its buffer once (copy-on-write) instead.
//!
//! The snapshot is per-client, not session-global: `session.active_tab` holds
//! *this* client's viewed tab (the renderer asserts the two agree), while
//! `session.name`/`tabs_metadata` are the true session-wide data.

use std::collections::HashSet;

use koshi_config::types::{RgbColor, ThemeConfig};
use koshi_core::command::{Selection, SelectionKind};
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId};
use koshi_layout::content::content_rects;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::{solve_with_mode, SolveResult};
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_pane::pane::state::PaneKind;
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, PaneSlot, PaneSnapshot, PluginUiSnapshot,
    RenderSnapshot, ScrollbackMeta, SelectionSpans, SessionSnapshot, TabMeta, TabSnapshot,
};
use koshi_renderer::theme::Theme;
use koshi_session::session::state::{Session, Tab};
use koshi_terminal::grid::state::Grid;
use koshi_terminal::scrollback::Scrollback;
use koshi_terminal::selection::order;
use koshi_terminal::state::Screen;
use ratatui::style::Color;

use crate::runtime::state::Runtime;

/// Resolve a config theme into the renderer [`Theme`] the snapshot carries:
/// each palette role's `#RRGGBB` value becomes the matching truecolor field.
/// For example, a theme with `ramp_start "#ff0000"` yields a `Theme` whose
/// first tab ribbon paints red. Resolving the default config theme yields
/// exactly [`Theme::default`], so a default config reproduces the stock look.
#[must_use]
pub fn resolve_theme(config: &ThemeConfig) -> Theme {
    let colors = &config.colors;
    Theme {
        ramp_start: rgb_channels(colors.ramp_start),
        ramp_end: rgb_channels(colors.ramp_end),
        on_ramp: rgb_color(colors.on_ramp),
        on_ramp_dim: rgb_color(colors.on_ramp_dim),
        accent: rgb_color(colors.accent),
        on_accent: rgb_color(colors.on_accent),
        border_focused: rgb_color(colors.border_focused),
        border_unfocused: rgb_color(colors.border_unfocused),
        stack_header_fg: rgb_color(colors.stack_header_fg),
        stack_header_bg: rgb_color(colors.stack_header_bg),
        letterbox: rgb_color(colors.letterbox),
    }
}

/// A config color's `(r, g, b)` channels, for the theme's ramp endpoints.
fn rgb_channels(color: RgbColor) -> (u8, u8, u8) {
    (color.r, color.g, color.b)
}

/// A config color as a ratatui truecolor.
fn rgb_color(color: RgbColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

impl Runtime {
    /// Freeze the world the way `client_id` sees it into a [`RenderSnapshot`].
    ///
    /// Returns `None` when no attached client has that id, or its viewed tab has
    /// gone — the caller skips the frame. On success, `session.active_tab` is the
    /// client's own viewed tab, solved over the tab's effective size (the
    /// per-axis-minimum viewport across every client viewing it), so the renderer
    /// letterboxes it (centers it with padding) into this client's larger
    /// viewport.
    pub fn build_snapshot(&self, client_id: ClientId) -> Option<RenderSnapshot> {
        let session = self.session_for_client(client_id)?;
        let client = session.clients.get(client_id)?;
        let active_tab_id = client.active_tab();
        let tab = session.tabs.get(&active_tab_id)?;

        // Solve the active tab's layout over a rect at origin (0, 0) sized to the
        // shared effective size; the renderer offsets it into the client viewport.
        //
        // The solve uses THIS client's layout mode: zoom is per-client, so a pane
        // filling the tab for this client can be one tile among several for
        // another client viewing the same tab at the same moment.
        let effective_size = session
            .tab_viewport(active_tab_id)
            .expect("the requesting client views its own active tab, so tab_viewport is Some");
        let layout_mode = client.layout_mode(active_tab_id);
        let solve = solve_tab(tab, layout_mode, effective_size);
        let content = content_rects(&solve);

        // One `PaneSlot` per leaf: outer rect from the solve, inner (content) rect
        // from `content_rects` — both in the same solve order, so they zip.
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
                    kind: record.map_or(PaneKind::Terminal, |record| record.kind().clone()),
                    visible: inner_rect.is_some(),
                    suppressed: suppressed.contains(&pane_id),
                    dead: record.is_some_and(|record| {
                        matches!(record.lifecycle(), PaneLifecycle::Exited { .. })
                    }),
                }
            })
            .collect();

        // Content for each of the active tab's panes, joined to the slots by id.
        let panes: Vec<PaneSnapshot> = solve
            .panes
            .iter()
            .map(|&(pane_id, _)| {
                self.pane_snapshot(
                    pane_id,
                    client.scroll_offset(pane_id),
                    client.selection(pane_id),
                )
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

        Some(RenderSnapshot {
            session: SessionSnapshot {
                id: session.id,
                name: session.name.clone(),
                active_tab,
                tabs_metadata,
            },
            panes,
            client: ClientSnapshot {
                id: client.id(),
                viewport: client.viewport(),
                active_tab: active_tab_id,
                focused_pane: client.focused_pane(active_tab_id),
                lock_mode: client.lock_mode(),
                pending_sequence: client
                    .pending_key_sequence()
                    .map(|pending| pending.sequence.clone()),
                tabline_offset: client.tabline_offset(),
            },
            plugin_ui: PluginUiSnapshot::default(),
            keymap_hints: self.keymap_hints.hints_for(client.lock_mode()),
            theme: self.theme,
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
    /// gets `grid_view = None` and a hidden cursor; the renderer draws no cells
    /// for it.
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
                selection: None,
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
        // On the alternate screen a full-screen app is running: its OSC 0/1/2
        // title names the pane. On the primary screen the shell's OSC 7 cwd
        // (`~`-shortened) is the more useful name, with the OSC title as the
        // fallback. An explicit `rename-pane` does not feed the rendered title.
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
            selection: selection
                .and_then(|selection| selection_spans(&selection, &grid, scrollback, view_offset)),
            grid_view: Some(GridView { grid, view_offset }),
            reverse_video: state.reverse_video(),
            scrollback: ScrollbackMeta {
                truncated: scrollback.dropped_lines() > 0,
                retained_lines: scrollback.len(),
            },
        }
    }

    /// The session that owns `client_id`, or `None` if no attached client has
    /// that id. Shared with command dispatch's `acting_session`, which resolves
    /// the same key-binding/mouse client to its session.
    pub(crate) fn session_for_client(&self, client_id: ClientId) -> Option<&Session> {
        self.sessions()
            .values()
            .find(|session| session.clients.get(client_id).is_some())
    }

    /// Mutable twin of [`session_for_client`](Self::session_for_client): the same
    /// client→session lookup, for callers that edit the client's view state (e.g.
    /// the scroll handlers).
    pub(crate) fn session_for_client_mut(&mut self, client_id: ClientId) -> Option<&mut Session> {
        self.sessions
            .values_mut()
            .find(|session| session.clients.get(client_id).is_some())
    }

    /// The session that owns `pane_id`, or `None` if no session's registry holds
    /// that pane. The single pane→session lookup, shared by pane-target
    /// resolution, child-exit routing, and the scroll re-anchor.
    pub(crate) fn session_for_pane(&self, pane_id: PaneId) -> Option<&Session> {
        self.sessions()
            .values()
            .find(|session| session.panes.get(pane_id).is_some())
    }

    /// Mutable twin of [`session_for_pane`](Self::session_for_pane), for callers
    /// that edit the owning session's state.
    pub(crate) fn session_for_pane_mut(&mut self, pane_id: PaneId) -> Option<&mut Session> {
        self.sessions
            .values_mut()
            .find(|session| session.panes.get(pane_id).is_some())
    }
}

/// One path as pane-title text: the user's home directory prefix shortened
/// to `~`, everything else verbatim.
fn display_path(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    shorten_home(path, home.as_deref())
}

/// The `~`-shortening behind [`display_path`], with the home directory passed
/// in. The prefix must end on a path boundary — a sibling like `/Users/ab2`
/// next to home `/Users/ab` stays whole.
fn shorten_home(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    let text = path.display().to_string();
    if let Some(home) = home {
        let home = home.display().to_string();
        if let Some(rest) = text.strip_prefix(&home) {
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
pub(crate) fn solve_tab(tab: &Tab, mode: LayoutMode, viewport: Size) -> SolveResult {
    solve_with_mode(
        tab.layout(),
        mode,
        Rect::new(Point { x: 0, y: 0 }, viewport),
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
/// A highlight only partly on screen keeps the part that is: its first visible
/// row starts at column 0 rather than at the selection's own start column,
/// because the real start is somewhere above the window.
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
