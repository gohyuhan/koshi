//! Tests for stock frame composition.
//!
//! The three zones render into a ratatui buffer. Tabs show their marker, and
//! the mode tag tracks the client lock mode, its mouse-select state and a
//! reconnecting viewer. Pane borders draw with focus and hover highlighting.
//! Terminal cells paint into pane content rects with their styles, wide-glyph
//! handling and highlight spans. Collapsed stack members render as
//! theme-filled title strips. The committed region solve decides which chrome
//! rows draw and where the pane rectangle sits.
//!
//! The focused pane's cursor cell is reported, clamped inside its content
//! area, and hidden for unfocused, plugin, hidden, or app-hidden cursors. The
//! cursor style follows the focused pane. A centered too-small overlay
//! replaces the frame when the tab has no room for any pane. A viewport larger
//! than the effective size centers the layout and letterboxes the margin, with
//! the cursor shifted to match. Degenerate sizes are safe, including a buffer
//! shorter than the laid-out frame.

use super::*;

use std::sync::Arc;

use koshi_core::geometry::{Point, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::mouse::MouseTracking;
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::{Color as TermColor, Style as TermStyle};

use koshi_terminal::state::CursorShape;

use crate::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, CursorStyle, GridView, KeymapHints, PaneSlot,
    PaneSnapshot, PluginUiSnapshot, ScrollbackMeta, SelectionSpans, SessionSnapshot, TabMeta,
    TabSnapshot, ViewerChrome,
};
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::{solve_region_rects, Edge, RegionGeometry, SolvedRegions};
use koshi_layout::solver::StackHeader;
use koshi_pane::pane::state::PaneKind;

/// A cell rectangle: origin `(origin_column, origin_row)`, size `column_count x row_count`.
fn build_cell_rect(origin_column: u16, origin_row: u16, column_count: u16, row_count: u16) -> Rect {
    Rect {
        origin: Point {
            column: origin_column,
            row: origin_row,
        },
        cell_size: Size {
            column_count,
            row_count,
        },
    }
}

/// Build a snapshot from explicit pieces. `pane_layouts` are `(pane_id, outer rect, visible)`;
/// a visible pane's content rect is the outer rect inset by its one-cell border.
fn build_render_snapshot(
    session_name: &str,
    tab_names_and_activity: &[(&str, bool)],
    pane_layouts: &[(PaneId, Rect, bool)],
    focused_pane_id: Option<PaneId>,
    lock_mode: LockMode,
    viewport_size: Size,
) -> RenderSnapshot {
    let tab_id = TabId::new();

    let pane_slots = pane_layouts
        .iter()
        .map(|(pane_id, outer_rect, is_visible)| PaneSlot {
            pane_id: *pane_id,
            outer_rect: *outer_rect,
            content_rect: is_visible.then(|| outer_rect.compute_inner_with_border()),
            pane_kind: PaneKind::Terminal,
            is_visible: *is_visible,
            is_suppressed: false,
            is_dead: false,
        })
        .collect();

    let pane_snapshots = pane_layouts
        .iter()
        .map(|(pane_id, _, _)| PaneSnapshot {
            view_top_row_index: 0,
            pane_id: *pane_id,
            pane_title: None,
            cursor_snapshot: CursorSnapshot {
                row_index: 0,
                column_index: 0,
                is_visible: true,
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
            scrollback_meta: ScrollbackMeta {
                is_truncated: false,
                retained_line_count: 0,
            },
        })
        .collect();

    let tabs_metadata = tab_names_and_activity
        .iter()
        .enumerate()
        .map(|(tab_index, (tab_name, is_active))| TabMeta {
            tab_id: TabId::new(),
            tab_name: (*tab_name).to_string(),
            tab_index,
            is_active: *is_active,
        })
        .collect();

    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: session_name.to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "active".to_string(),
                pane_slots,
                effective_cell_size: viewport_size,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata,
        },
        pane_snapshots,
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            client_revision: 0,
            viewport_size,
            active_tab_id: tab_id,
            focused_pane_id,
            lock_mode,
            is_mouse_selection_enabled: false,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

/// The compiled-in region solve for a viewport-sized test area.
fn build_core_regions(column_count: u16, row_count: u16) -> CommittedRegions {
    CommittedRegions::core(
        Size {
            column_count,
            row_count,
        },
        0,
    )
}

/// The whole-area geometry: the top row as the first region, the bottom row as
/// the second, and the whole `column_count x row_count` viewport as the pane rectangle. A
/// one-row viewport gets an empty second region.
fn build_legacy_regions(column_count: u16, row_count: u16) -> CommittedRegions {
    let viewport_size = Size {
        column_count,
        row_count,
    };
    let top_region = Rect::from_origin_and_size(
        Point { column: 0, row: 0 },
        Size {
            column_count,
            row_count: row_count.min(1),
        },
    );
    let bottom_region = if row_count >= 2 {
        Rect::from_origin_and_size(
            Point {
                column: 0,
                row: row_count - 1,
            },
            Size {
                column_count,
                row_count: 1,
            },
        )
    } else {
        Rect::empty_at_origin()
    };
    CommittedRegions::from_solved_regions(
        viewport_size,
        SolvedRegions {
            region_rects: vec![top_region, bottom_region],
            pane_rect: Rect::from_size_at_origin(viewport_size),
        },
        0,
    )
}

/// Render a snapshot into a fresh `column_count x row_count` buffer.
fn render_test_snapshot(snapshot: &RenderSnapshot, column_count: u16, row_count: u16) -> Buffer {
    render_test_snapshot_with_theme(snapshot, &Theme::default(), column_count, row_count)
}

/// Paint `snapshot` in `theme`'s colors, for the tests that check which color
/// a surface takes rather than where it sits.
fn render_test_snapshot_with_theme(
    snapshot: &RenderSnapshot,
    theme: &Theme,
    column_count: u16,
    row_count: u16,
) -> Buffer {
    let viewport_area = RatatuiRect {
        x: 0,
        y: 0,
        width: column_count,
        height: row_count,
    };
    let mut render_buffer = Buffer::empty(viewport_area);
    let regions = build_legacy_regions(column_count, row_count);
    render_frame(
        snapshot,
        &regions,
        theme,
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        viewport_area,
        &mut render_buffer,
    );
    render_buffer
}

/// Paint `snapshot` with the viewer's tab strip peeking, for the tests that
/// check which tabs the strip shows.
fn render_snapshot_with_peeking(
    snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
    column_count: u16,
    row_count: u16,
) -> Buffer {
    let viewport_area = RatatuiRect {
        x: 0,
        y: 0,
        width: column_count,
        height: row_count,
    };
    let mut render_buffer = Buffer::empty(viewport_area);
    let regions = build_legacy_regions(column_count, row_count);
    render_frame(
        snapshot,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        viewer_chrome,
        viewport_area,
        &mut render_buffer,
    );
    render_buffer
}

/// Paint `snapshot` with the viewer's pointer over `hovered_pane_id`, for the tests
/// that check which pane's border wears the hover color.
fn render_snapshot_with_hover(
    snapshot: &RenderSnapshot,
    hovered_pane_id: Option<PaneId>,
    column_count: u16,
    row_count: u16,
) -> Buffer {
    let viewport_area = RatatuiRect {
        x: 0,
        y: 0,
        width: column_count,
        height: row_count,
    };
    let mut render_buffer = Buffer::empty(viewport_area);
    let regions = build_legacy_regions(column_count, row_count);
    render_frame(
        snapshot,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome {
            hovered_pane_id,
            placement_handle_pane_id: None,
            active_input_mode: None,
            tabline_offset: None,
            reconnecting: None,
        },
        viewport_area,
        &mut render_buffer,
    );
    render_buffer
}

/// Paint `snapshot` with `keymap_hints` in the bottom bar, for the tests that check
/// what the hint row says.
fn render_snapshot_with_hints(
    snapshot: &RenderSnapshot,
    keymap_hints: &KeymapHints,
    column_count: u16,
    row_count: u16,
) -> Buffer {
    let viewport_area = RatatuiRect {
        x: 0,
        y: 0,
        width: column_count,
        height: row_count,
    };
    let mut render_buffer = Buffer::empty(viewport_area);
    let regions = build_legacy_regions(column_count, row_count);
    render_frame(
        snapshot,
        &regions,
        &Theme::default(),
        keymap_hints,
        None,
        ViewerChrome::default(),
        viewport_area,
        &mut render_buffer,
    );
    render_buffer
}

/// Paint `snapshot` over `committed_regions`' viewport with no hints.
fn render_snapshot_with_regions(
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
) -> Buffer {
    render_snapshot_with_regions_and_hints(snapshot, committed_regions, &KeymapHints::default())
}

/// Paint `snapshot` over `committed_regions`' viewport with `keymap_hints` for the second
/// region, for the tests that check which chrome rows a region solve leaves
/// room for.
fn render_snapshot_with_regions_and_hints(
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    keymap_hints: &KeymapHints,
) -> Buffer {
    let viewport_area = RatatuiRect {
        x: 0,
        y: 0,
        width: committed_regions.viewport_size.column_count,
        height: committed_regions.viewport_size.row_count,
    };
    let mut render_buffer = Buffer::empty(viewport_area);
    render_frame(
        snapshot,
        committed_regions,
        &Theme::default(),
        keymap_hints,
        None,
        ViewerChrome::default(),
        viewport_area,
        &mut render_buffer,
    );
    render_buffer
}

/// One `Ctrl + l` → `Lock` hint, the row the statusline draws when it has one.
fn build_lock_hint_keymap() -> KeymapHints {
    KeymapHints {
        hint_bindings: Arc::new(vec![crate::snapshot::HintBinding {
            key_sequence: KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('l'))),
            action_display_name: "Lock".to_string(),
            is_user_authored: false,
            is_pinned: false,
        }]),
        ..KeymapHints::default()
    }
}

#[test]
fn committed_core_regions_keep_the_default_frame_byte_identical() {
    let pane_id = PaneId::new();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let mut snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane_id, build_cell_rect(0, 0, 80, 22), true)],
        Some(pane_id),
        LockMode::Normal,
        viewport_size,
    );
    snapshot
        .session_snapshot
        .active_tab_snapshot
        .effective_cell_size = Size {
        column_count: 80,
        row_count: 22,
    };

    let committed_regions = build_core_regions(viewport_size.column_count, viewport_size.row_count);
    assert_eq!(
        render_test_snapshot(&snapshot, 80, 24),
        render_snapshot_with_regions(&snapshot, &committed_regions)
    );
}

#[test]
fn committed_regions_keep_panes_and_cursor_inside_a_side_region() {
    let pane_id = PaneId::new();
    let viewport_size = Size {
        column_count: 120,
        row_count: 40,
    };
    let effective_cell_size = Size {
        column_count: 100,
        row_count: 38,
    };
    let mut snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(
            pane_id,
            build_cell_rect(
                0,
                0,
                effective_cell_size.column_count,
                effective_cell_size.row_count,
            ),
            true,
        )],
        Some(pane_id),
        LockMode::Normal,
        viewport_size,
    );
    snapshot
        .session_snapshot
        .active_tab_snapshot
        .effective_cell_size = effective_cell_size;
    snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(36, 98, TermStyle::default())),
        view_row_offset: 0,
    });
    let committed_regions = CommittedRegions::from_solved_regions(
        viewport_size,
        solve_region_rects(
            viewport_size,
            &[
                RegionGeometry {
                    edge: Edge::Top,
                    extent_cell_count: 1,
                },
                RegionGeometry {
                    edge: Edge::Bottom,
                    extent_cell_count: 1,
                },
                RegionGeometry {
                    edge: Edge::Left,
                    extent_cell_count: 20,
                },
            ],
        ),
        3,
    );

    let render_buffer = render_snapshot_with_regions(&snapshot, &committed_regions);
    assert_eq!(render_buffer[(20, 1)].symbol(), "┌");
    assert_eq!(render_buffer[(10, 1)].symbol(), " ");
    assert_eq!(
        get_cursor_position(
            &snapshot,
            &committed_regions,
            RatatuiRect {
                x: 0,
                y: 0,
                width: viewport_size.column_count,
                height: viewport_size.row_count,
            },
        ),
        Some(Position::new(21, 2))
    );
}

#[test]
fn one_region_solution_paints_no_statusline() {
    // The statusline draws in the solve's second region. A solve that names only
    // the tabline leaves the bottom row to the pane area, and the hints the
    // caller passes are drawn nowhere.
    let pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane_id, build_cell_rect(0, 0, 40, 6), true)],
        Some(pane_id),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let committed_regions = CommittedRegions::from_solved_regions(
        Size {
            column_count: 40,
            row_count: 8,
        },
        SolvedRegions {
            region_rects: vec![Rect::from_origin_and_size(
                Point { column: 0, row: 0 },
                Size {
                    column_count: 40,
                    row_count: 1,
                },
            )],
            pane_rect: Rect::from_origin_and_size(
                Point { column: 0, row: 1 },
                Size {
                    column_count: 40,
                    row_count: 7,
                },
            ),
        },
        0,
    );
    let render_buffer = render_snapshot_with_regions_and_hints(
        &render_snapshot,
        &committed_regions,
        &build_lock_hint_keymap(),
    );

    assert_eq!(
        format_rendered_row_text(&render_buffer, 0),
        format_session_shell_tabline(40)
    );
    // Rows 1..=6 are the pane box, shifted into the pane rectangle, and row 7
    // is the bottom of the pane area: every row is blank of the `Lock` hint.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 1),
        format!("┌{}┐", "─".repeat(38))
    );
    for row_index in 2..=5 {
        assert_eq!(
            format_rendered_row_text(&render_buffer, row_index),
            format!("│{}│", " ".repeat(38)),
            "row {row_index}"
        );
    }
    assert_eq!(
        format_rendered_row_text(&render_buffer, 6),
        format!("└{}┘", "─".repeat(38))
    );
    assert_eq!(format_rendered_row_text(&render_buffer, 7), " ".repeat(40));
}

#[test]
fn an_empty_region_solution_paints_neither_chrome_row() {
    // No regions at all: the pane rectangle is the whole viewport, and both the
    // tabline and the statusline are skipped.
    let pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane_id, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane_id),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let committed_regions = CommittedRegions::from_solved_regions(
        Size {
            column_count: 40,
            row_count: 8,
        },
        SolvedRegions {
            region_rects: Vec::new(),
            pane_rect: Rect::from_size_at_origin(Size {
                column_count: 40,
                row_count: 8,
            }),
        },
        0,
    );
    let render_buffer = render_snapshot_with_regions_and_hints(
        &render_snapshot,
        &committed_regions,
        &build_lock_hint_keymap(),
    );

    assert_eq!(format_rendered_row_text(&render_buffer, 0), " ".repeat(40));
    assert_eq!(format_rendered_row_text(&render_buffer, 7), " ".repeat(40));
    // The pane box still draws, in the whole-viewport pane rectangle.
    assert_eq!(render_buffer[(0, 1)].symbol(), "┌");
    assert_eq!(render_buffer[(39, 6)].symbol(), "┘");
}

#[test]
fn a_solve_that_leaves_no_pane_rectangle_letterboxes_everything_but_the_chrome() {
    // A solve can hand the regions the whole viewport and leave a zero-size
    // pane rectangle. The panes still draw where the layout put them, then the
    // letterbox fills the whole frame around a zero-size content rect, and the
    // two chrome rows paint their own bar background over it.
    let pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane_id, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane_id),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let committed_regions = CommittedRegions::from_solved_regions(
        Size {
            column_count: 40,
            row_count: 8,
        },
        SolvedRegions {
            region_rects: vec![
                Rect::from_origin_and_size(
                    Point { column: 0, row: 0 },
                    Size {
                        column_count: 40,
                        row_count: 1,
                    },
                ),
                Rect::from_origin_and_size(
                    Point { column: 0, row: 7 },
                    Size {
                        column_count: 40,
                        row_count: 1,
                    },
                ),
            ],
            pane_rect: Rect::empty_at_origin(),
        },
        0,
    );
    let render_buffer = render_snapshot_with_regions(&render_snapshot, &committed_regions);

    // The pane box is drawn, and its cells wear the letterbox background.
    assert_eq!(render_buffer[(0, 1)].symbol(), "┌");
    assert_eq!(render_buffer[(0, 1)].bg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(
        format_rendered_row_text(&render_buffer, 2),
        format!("│{}│", " ".repeat(38)),
        "a pane box row"
    );
    // Both chrome rows paint over the fill with the bar background.
    assert_eq!(render_buffer[(0, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert_eq!(render_buffer[(0, 7)].bg, Color::Rgb(0x00, 0x00, 0x00));
}

/// The client's viewport as an origin-`(0, 0)` render area, matching what
/// [`render_test_snapshot`] paints into — the `viewport_area` [`cursor_position`] takes.
fn build_viewport_area(snapshot: &RenderSnapshot) -> RatatuiRect {
    RatatuiRect {
        x: 0,
        y: 0,
        width: snapshot.client_snapshot.viewport_size.column_count,
        height: snapshot.client_snapshot.viewport_size.row_count,
    }
}

/// The cursor cell [`cursor_position`] reports for `snapshot` over the
/// whole-area geometry, in the client's own viewport.
fn get_legacy_cursor_position(snapshot: &RenderSnapshot) -> Option<Position> {
    let viewport_area = build_viewport_area(snapshot);
    let committed_regions = build_legacy_regions(viewport_area.width, viewport_area.height);
    get_cursor_position(snapshot, &committed_regions, viewport_area)
}

/// The visible text of a render buffer row.
fn format_rendered_row_text(render_buffer: &Buffer, row_index: u16) -> String {
    (0..render_buffer.area().width)
        .map(|column_index| {
            render_buffer[(column_index, row_index)]
                .symbol()
                .to_string()
        })
        .collect()
}

#[test]
fn renders_tabline_pane_border_and_reserved_hint_bar() {
    let pane = PaneId::new();
    let column_count = version_badge_column_count() + 31;
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, column_count, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, column_count, 8);

    // Tabline (row 0): the session block ` sess ` and the version badge, one
    // gap cell, the tab ribbon ` #1  shell `, blanks, then the right-aligned
    // ` BASE ` mode tag filling the last six cells.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 0),
        format_session_shell_tabline(column_count)
    );

    // Pane border box on rows 1..=6, spanning the full width.
    assert_eq!(render_buffer[(0, 1)].symbol(), "┌");
    assert_eq!(render_buffer[(column_count - 1, 1)].symbol(), "┐");
    assert_eq!(render_buffer[(0, 6)].symbol(), "└");
    assert_eq!(render_buffer[(column_count - 1, 6)].symbol(), "┘");
    assert_eq!(render_buffer[(1, 1)].symbol(), "─");
    assert_eq!(render_buffer[(0, 2)].symbol(), "│");

    // Bottom row (row 7): the statusline row is koshi-owned chrome. This
    // snapshot carries no hint data, and every cell of the row is a space.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 7),
        " ".repeat(column_count as usize)
    );
}

#[test]
fn hint_bar_paints_the_bottom_row_from_the_hints_it_is_given() {
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let hints = KeymapHints {
        hint_bindings: Arc::new(vec![crate::snapshot::HintBinding {
            key_sequence: KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('l'))),
            action_display_name: "Lock".to_string(),
            is_user_authored: false,
            is_pinned: false,
        }]),
        ..KeymapHints::default()
    };
    let render_buffer = render_snapshot_with_hints(&render_snapshot, &hints, 40, 8);

    // Hint row is outside pane area: border bottom remains intact above it.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 7),
        format!(" Ctrl +  l  Lock{}", " ".repeat(24))
    );
    assert_eq!(render_buffer[(0, 6)].symbol(), "└");
    assert_eq!(render_buffer[(39, 6)].symbol(), "┘");
}

#[test]
fn two_rows_is_enough_for_both_chrome_rows() {
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[],
        None,
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 2,
        },
    );
    let hints = KeymapHints {
        hint_bindings: Arc::new(vec![crate::snapshot::HintBinding {
            key_sequence: KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('l'))),
            action_display_name: "Lock".to_string(),
            is_user_authored: false,
            is_pinned: false,
        }]),
        ..KeymapHints::default()
    };
    let render_buffer = render_snapshot_with_hints(&render_snapshot, &hints, 40, 2);

    // Row 0 is the tabline, row 1 the hint row: the last height that fits both.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 0),
        format_session_shell_tabline(40)
    );
    assert_eq!(
        format_rendered_row_text(&render_buffer, 1),
        format!(" Ctrl +  l  Lock{}", " ".repeat(24))
    );
}

#[test]
fn tabline_lists_tabs_with_active_marker() {
    let pane = PaneId::new();
    let column_count = version_badge_column_count() + 51;
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("code", true), ("logs", false)],
        &[(pane, build_cell_rect(0, 1, column_count, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, column_count, 8);

    // The session block ` sess `, then the version badge, a gap, each padded
    // tab with one blank cell between them, blanks, and the ` BASE ` mode tag
    // on the last six cells.
    let version_badge_text = crate::render::create_version_badge_text();
    assert_eq!(
        format_rendered_row_text(&render_buffer, 0),
        format!(
            " sess {version_badge_text}  #1  code   #2  logs {}BASE ",
            " ".repeat(18)
        )
    );

    // Where each tab landed, read from the same solve the paint used, so the
    // The version badge width never has to be spelled out here.
    let visible_tab_spans = solve_tabline_layout(
        render_snapshot
            .build_frame_layout(ViewerChrome::default())
            .get_tabline_inputs(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 60,
            height: 1,
        },
    )
    .visible_tab_spans;
    let (active_tab_column, inactive_tab_column) = (
        visible_tab_spans[0].start_column + 1,
        visible_tab_spans[1].start_column + 1,
    );

    // The active tab is inverted: its ramp stop as the TEXT color over the
    // bar background the row is filled with; an inactive tab's blocks sit on
    // its dimmed stop. Two tabs → the stops are the ramp's purple and blue
    // ends.
    assert_eq!(
        render_buffer[(active_tab_column, 0)].fg,
        Color::Rgb(0xd0, 0xa5, 0xff)
    );
    assert_eq!(
        render_buffer[(active_tab_column, 0)].bg,
        Color::Rgb(0x00, 0x00, 0x00)
    );
    assert_eq!(
        render_buffer[(inactive_tab_column, 0)].bg,
        Color::Rgb(0x44, 0x67, 0x8c)
    );
}

#[test]
fn tabline_scrolls_overflowing_tabs_behind_a_right_arrow() {
    let pane = PaneId::new();
    let column_count = version_badge_column_count() + 31;
    let render_snapshot = build_render_snapshot(
        "sess",
        &[
            ("alpha", true),
            ("bravo", false),
            ("charlie", false),
            ("delta", false),
            ("echo", false),
        ],
        &[(pane, build_cell_rect(0, 1, column_count, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, column_count, 8);

    // The session block and the mode tag always render whole. The active tab
    // (alpha, index 0) fits from the left, so the window starts there and the
    // four tabs hidden off the right sit behind a `▶` scroll arrow. The blank
    // cell where a `◀` would sit stays blank: nothing is hidden to the left.
    let version_badge_text = crate::render::create_version_badge_text();
    assert_eq!(
        format_rendered_row_text(&render_buffer, 0),
        format!(" sess {version_badge_text}   #1  alpha      ▶ BASE ")
    );
}

/// Cells the tabline's version badge takes, measured from the left edge of the tabline
/// actually paints. A semver version is ASCII, so counting characters counts
/// display cells.
///
/// A test that needs room beside the version badge asks for `version_badge_column_count() + <room>`
/// rather than a fixed count: the room beside the version badge stays the same however
/// long the version string is.
fn version_badge_column_count() -> u16 {
    crate::render::create_version_badge_text().chars().count() as u16
}

/// The whole tabline row a session named `sess` with the single active tab
/// `shell` paints into a `column_count`-wide row, with ` BASE ` as the mode tag.
///
/// `column_count` must leave room for all of it — at least `version_badge_column_count() + 24`.
fn format_session_shell_tabline(column_count: u16) -> String {
    format_session_shell_tabline_with_mode_tag(column_count, " BASE ")
}

/// The whole tabline row a session named `sess` with the single active tab
/// `shell` paints into a `column_count`-wide row: the ` sess ` block, the version
/// version badge, one gap cell, the ` #1  shell ` ribbon, blank cells, then the mode tag
/// right-aligned on the last mode-tag cells.
///
/// `mode_tag_text` is the mode block with its own padding spaces, such as ` BASE ` or
/// ` LOCK `. `column_count` must leave room for all of it.
fn format_session_shell_tabline_with_mode_tag(column_count: u16, mode_tag_text: &str) -> String {
    let version_badge_text = crate::render::create_version_badge_text();
    let blanks = column_count as usize
        - 6
        - version_badge_text.chars().count()
        - 1
        - 11
        - mode_tag_text.chars().count();
    [
        " sess ".to_string(),
        version_badge_text,
        " ".to_string(),
        " #1  shell ".to_string(),
        " ".repeat(blanks),
        mode_tag_text.to_string(),
    ]
    .concat()
}

/// Overflowing tabs, offset unset: the window scrolls to reveal the active tab
/// even when it lands deep in the tail, and both sides show a scroll arrow.
#[test]
fn tabline_follows_focus_into_the_overflow() {
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "s",
        &[
            ("t0", false),
            ("t1", false),
            ("t2", false),
            ("t3", false),
            ("t4", false),
            ("t5", true),
            ("t6", false),
            ("t7", false),
        ],
        &[(
            pane,
            build_cell_rect(0, 1, version_badge_column_count() + 21, 6),
            true,
        )],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: version_badge_column_count() + 21,
            row_count: 8,
        },
    );
    let tabline = format_rendered_row_text(
        &render_test_snapshot(&render_snapshot, version_badge_column_count() + 21, 8),
        0,
    );

    // Only the active tab `t5` fits, as tab six: `t0`..`t4` sit behind the `◀`
    // arrow and `t6`, `t7` behind the `▶` one.
    let version_badge_text = crate::render::create_version_badge_text();
    assert_eq!(
        tabline,
        format!(" s {version_badge_text} ◀ #6  t5  ▶ BASE ")
    );
}

/// A peek offset windows the strip from that index, not the active tab: the
/// active tab may stay hidden while peeking, and a left offset of 0 shows no
/// left arrow.
#[test]
fn tabline_peek_offset_ignores_the_active_tab() {
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "s",
        &[
            ("t0", false),
            ("t1", false),
            ("t2", false),
            ("t3", false),
            ("t4", false),
            ("t5", true),
            ("t6", false),
            ("t7", false),
        ],
        &[(
            pane,
            build_cell_rect(0, 1, version_badge_column_count() + 21, 6),
            true,
        )],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: version_badge_column_count() + 21,
            row_count: 8,
        },
    );
    let peeking = ViewerChrome {
        hovered_pane_id: None,
        placement_handle_pane_id: None,
        active_input_mode: None,
        tabline_offset: Some(0),
        reconnecting: None,
    };
    let tabline = format_rendered_row_text(
        &render_snapshot_with_peeking(
            &render_snapshot,
            peeking,
            version_badge_column_count() + 21,
            8,
        ),
        0,
    );

    // The strip windows from index 0, so only `t0` shows and the active `t5`
    // stays hidden behind the `▶` arrow. The `◀` cell stays blank: nothing is
    // hidden to the left of index 0.
    let version_badge_text = crate::render::create_version_badge_text();
    assert_eq!(
        tabline,
        format!(" s {version_badge_text}   #1  t0  ▶ BASE ")
    );
}

#[test]
fn mode_tag_reflects_lock_mode() {
    let pane = PaneId::new();
    let create_snapshot_for_lock_mode = |lock_mode| {
        build_render_snapshot(
            "sess",
            &[("shell", true)],
            &[(pane, build_cell_rect(0, 1, 40, 6), true)],
            Some(pane),
            lock_mode,
            Size {
                column_count: 40,
                row_count: 8,
            },
        )
    };

    let normal_mode_render_buffer =
        render_test_snapshot(&create_snapshot_for_lock_mode(LockMode::Normal), 40, 8);
    assert_eq!(
        format_rendered_row_text(&normal_mode_render_buffer, 0),
        format_session_shell_tabline_with_mode_tag(40, " BASE ")
    );

    // The lock mode tag replaces the base one in the same six right-aligned cells.
    let locked_mode_render_buffer =
        render_test_snapshot(&create_snapshot_for_lock_mode(LockMode::Locked), 40, 8);
    assert_eq!(
        format_rendered_row_text(&locked_mode_render_buffer, 0),
        format_session_shell_tabline_with_mode_tag(40, " LOCK ")
    );
}

#[test]
fn a_reconnecting_viewer_puts_the_dial_tag_in_the_tabline() {
    // The mode block is right-aligned and takes whatever room it needs. The
    // the reconnecting mode tag is 37 cells wide, so on a 46-cell-plus-version-badge row it
    // leaves nothing for the tab ribbon.
    let pane = PaneId::new();
    let column_count = version_badge_column_count() + 46;
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, column_count, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count,
            row_count: 8,
        },
    );
    let dialing = ViewerChrome {
        hovered_pane_id: None,
        placement_handle_pane_id: None,
        active_input_mode: None,
        tabline_offset: None,
        reconnecting: Some(Reconnecting {
            attempt: 3,
            retry_in_seconds: 8,
        }),
    };
    let render_buffer = render_snapshot_with_peeking(&render_snapshot, dialing, column_count, 8);

    let version_badge_text = crate::render::create_version_badge_text();
    assert_eq!(
        format_rendered_row_text(&render_buffer, 0),
        format!(" sess {version_badge_text}  RECONNECTING (attempt 3, retry in 8s) ")
    );
}

#[test]
fn focused_pane_border_is_highlighted() {
    let focused_pane_id = PaneId::new();
    let unfocused_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (focused_pane_id, build_cell_rect(0, 1, 20, 6), true),
            (unfocused_pane_id, build_cell_rect(20, 1, 20, 6), true),
        ],
        Some(focused_pane_id),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // Focused pane_id: the theme's focus color, bold border corner.
    assert_eq!(render_buffer[(0, 1)].fg, Color::Rgb(0x00, 0xaf, 0xd7));
    assert_eq!(render_buffer[(0, 1)].modifier, Modifier::BOLD);
    // Unfocused pane_id: dim border corner, no modifier at all.
    assert_eq!(render_buffer[(20, 1)].fg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(render_buffer[(20, 1)].modifier, Modifier::empty());
}

#[test]
fn hidden_pane_draws_no_border() {
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), false)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // No border cell anywhere the box would have been: rows 1..=6 are blank.
    for row_index in 1..=6 {
        assert_eq!(
            format_rendered_row_text(&render_buffer, row_index),
            " ".repeat(40),
            "row {row_index}"
        );
    }
}

#[test]
fn scroll_indicator_shown_only_when_scrolled_back() {
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );

    // At the live tail (offset 0): the pane's bottom border (row 6 — the box
    // spans rows 1..=6) is unbroken, and the tabline carries no indicator.
    let live_tail_render_buffer = render_test_snapshot(&render_snapshot, 40, 8);
    assert_eq!(
        format_rendered_row_text(&live_tail_render_buffer, 6),
        format!("└{}┘", "─".repeat(38))
    );
    assert_eq!(
        format_rendered_row_text(&live_tail_render_buffer, 0),
        format_session_shell_tabline(40)
    );

    // Scrolled back three lines with 100 retained: the count sits right-aligned
    // in this pane's own bottom border. The tabline keeps the ` BASE ` mode tag.
    render_snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(6, 40, TermStyle::default())),
        view_row_offset: 3,
    });
    render_snapshot.pane_snapshots[0]
        .scrollback_meta
        .retained_line_count = 100;
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);
    assert_eq!(
        format_rendered_row_text(&render_buffer, 6),
        format!("└{} 3/100 ┘", "─".repeat(31)),
        "the count is right-aligned and the corner glyph survives"
    );
    assert_eq!(
        format_rendered_row_text(&render_buffer, 0),
        format_session_shell_tabline(40),
        "no global indicator"
    );
}

#[test]
fn each_pane_shows_its_own_scroll_position() {
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (left_pane_id, build_cell_rect(0, 1, 20, 6), true),
            (right_pane_id, build_cell_rect(20, 1, 20, 6), true),
        ],
        Some(left_pane_id),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    // A is scrolled 3 up of 100; B is scrolled 7 up of 50 — different views.
    render_snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(6, 20, TermStyle::default())),
        view_row_offset: 3,
    });
    render_snapshot.pane_snapshots[0]
        .scrollback_meta
        .retained_line_count = 100;
    render_snapshot.pane_snapshots[1].terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(6, 20, TermStyle::default())),
        view_row_offset: 7,
    });
    render_snapshot.pane_snapshots[1]
        .scrollback_meta
        .retained_line_count = 50;

    // Both bottom borders are row 6; each carries its own count, right-aligned
    // in its own box.
    assert_eq!(
        format_rendered_row_text(&render_test_snapshot(&render_snapshot, 40, 8), 6),
        format!("└{} 3/100 ┘└{} 7/50 ┘", "─".repeat(11), "─".repeat(12))
    );
}

/// A scrolled-back pane whose box is `column_count` wide, in a viewport of the same
/// width: one tabline row, four box rows, one hint row.
fn build_narrow_scrolled_render_snapshot(column_count: u16) -> RenderSnapshot {
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "s",
        &[("t", true)],
        &[(pane, build_cell_rect(0, 1, column_count, 4), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count,
            row_count: 6,
        },
    );
    render_snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(2, column_count - 2, TermStyle::default())),
        view_row_offset: 3,
    });
    render_snapshot.pane_snapshots[0]
        .scrollback_meta
        .retained_line_count = 100;
    render_snapshot
}

#[test]
fn a_box_too_narrow_for_the_scroll_position_shows_none_of_it() {
    // ` 3/100 ` takes seven cells and never covers a corner glyph, so it needs
    // a box nine cells wide. An eight-wide box keeps its bottom border whole.
    let render_buffer = render_test_snapshot(&build_narrow_scrolled_render_snapshot(8), 8, 6);
    assert_eq!(format_rendered_row_text(&render_buffer, 4), "└──────┘");

    // One cell wider, and it sits between the two corners.
    let render_buffer = render_test_snapshot(&build_narrow_scrolled_render_snapshot(9), 9, 6);
    assert_eq!(format_rendered_row_text(&render_buffer, 4), "└ 3/100 ┘");
}

#[test]
fn a_scrolled_pane_that_retained_nothing_shows_a_zero_total() {
    // The indicator reports the pane's own retained-line count verbatim. A pane
    // scrolled three lines up whose scrollback retained none reads ` 3/0 `.
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(4, 38, TermStyle::default())),
        view_row_offset: 3,
    });
    render_snapshot.pane_snapshots[0]
        .scrollback_meta
        .retained_line_count = 0;
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(
        format_rendered_row_text(&render_buffer, 6),
        format!("└{} 3/0 ┘", "─".repeat(33))
    );
}

#[test]
fn a_pane_with_no_grid_shows_no_scroll_position() {
    // The scroll position comes from the pane's grid view. A pane that carries
    // scrollback metadata but no grid — a plugin pane — reads as the live tail,
    // so its bottom border stays unbroken.
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0]
        .scrollback_meta
        .retained_line_count = 100;
    assert_eq!(render_snapshot.pane_snapshots[0].terminal_grid_view, None);
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(
        format_rendered_row_text(&render_buffer, 6),
        format!("└{}┘", "─".repeat(38))
    );
}

#[test]
fn reused_buffer_is_blanked_before_painting() {
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "s",
        &[("t", true)],
        &[(pane, build_cell_rect(0, 1, 20, 4), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: version_badge_column_count() + 15,
            row_count: 6,
        },
    );

    // A buffer reused across frames holds the previous frame's cells; simulate
    // that with a full grid of stale glyphs before rendering.
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: version_badge_column_count() + 15,
        height: 6,
    };
    let mut render_buffer = Buffer::empty(area);
    for row_index in 0..area.height {
        for column_index in 0..area.width {
            render_buffer[(column_index, row_index)].set_symbol("X");
        }
    }

    let regions = build_legacy_regions(area.width, area.height);
    render_frame(
        &render_snapshot,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        area,
        &mut render_buffer,
    );

    // Tabline gap between the left tab list and the right status: blanked.
    assert_eq!(
        render_buffer[(version_badge_column_count() + 3, 0)].symbol(),
        " "
    );
    // A cell outside every pane box: blanked, not the stale glyph.
    assert_eq!(
        render_buffer[(version_badge_column_count() + 13, 2)].symbol(),
        " "
    );
    // Reserved hint row (bottom): every cell a space.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 5),
        " ".repeat(area.width as usize)
    );
}

#[test]
fn stack_headers_render_collapsed_strips() {
    let active_pane_id = PaneId::new();
    let first_collapsed_pane_id = PaneId::new();
    let second_collapsed_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (active_pane_id, build_cell_rect(0, 3, 30, 4), true),
            (first_collapsed_pane_id, build_cell_rect(0, 1, 30, 1), false),
            (
                second_collapsed_pane_id,
                build_cell_rect(0, 2, 30, 1),
                false,
            ),
        ],
        Some(active_pane_id),
        LockMode::Normal,
        Size {
            column_count: 30,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[1].pane_title = Some("editor".to_string());
    render_snapshot.pane_snapshots[2].pane_title = Some("logs".to_string());
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .stack_headers = vec![
        StackHeader {
            pane_id: first_collapsed_pane_id,
            header_rect: build_cell_rect(0, 1, 30, 1),
            member_index: 1,
            member_count: 3,
        },
        StackHeader {
            pane_id: second_collapsed_pane_id,
            header_rect: build_cell_rect(0, 2, 30, 1),
            member_index: 2,
            member_count: 3,
        },
    ];
    let render_buffer = render_test_snapshot(&render_snapshot, 30, 8);

    // Row 1: B's strip — arrow + title on the left, [2/3] right-aligned.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 1),
        format!("▸ editor{}[2/3]", " ".repeat(17))
    );
    // Row 2: C's strip.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 2),
        format!("▸ logs{}[3/3]", " ".repeat(19))
    );

    // The whole strip row carries the theme's strip colors (the koshi-owned
    // marker), gap included.
    for column_index in 0..30 {
        assert_eq!(
            render_buffer[(column_index, 1)].fg,
            Color::Rgb(0xf4, 0xf1, 0xfa),
            "col {column_index} of strip"
        );
        assert_eq!(
            render_buffer[(column_index, 1)].bg,
            Color::Rgb(0x30, 0x0f, 0x4a),
            "col {column_index} of strip"
        );
    }
}

#[test]
fn the_hover_color_marks_an_unfocused_pane_but_never_the_focused_one() {
    let focused_pane_id = PaneId::new();
    let unfocused_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (focused_pane_id, build_cell_rect(0, 1, 20, 6), true),
            (unfocused_pane_id, build_cell_rect(20, 1, 20, 6), true),
        ],
        Some(focused_pane_id),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );

    // Hovering the focused pane changes nothing: it keeps the focus color.
    let render_buffer = render_snapshot_with_hover(&render_snapshot, Some(focused_pane_id), 40, 8);
    assert_eq!(
        render_buffer[(0, 1)].fg,
        Theme::default().focused_border_color,
        "the focused pane keeps its focus color even when hovered"
    );

    // Hovering the unfocused pane paints its border the hover color, and the
    // focused pane is untouched.
    let render_buffer =
        render_snapshot_with_hover(&render_snapshot, Some(unfocused_pane_id), 40, 8);
    assert_eq!(
        render_buffer[(20, 1)].fg,
        Theme::default().hover_border_color,
        "an unfocused pane under the pointer takes the hover color"
    );
    assert_eq!(
        render_buffer[(0, 1)].fg,
        Theme::default().focused_border_color,
        "the focused pane's border is unaffected by hovering elsewhere"
    );
}

#[test]
fn move_pane_hover_uses_a_bright_border_and_gray_content_tint() {
    let focused_pane_id = PaneId::new();
    let hovered_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("a", true), ("b", true)],
        &[
            (focused_pane_id, build_cell_rect(0, 1, 20, 6), true),
            (hovered_pane_id, build_cell_rect(20, 1, 20, 6), true),
        ],
        Some(focused_pane_id),
        LockMode::Locked,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let viewport_area = RatatuiRect::new(0, 0, 40, 8);
    let mut render_buffer = Buffer::empty(viewport_area);
    render_frame(
        &render_snapshot,
        &build_legacy_regions(40, 8),
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome {
            hovered_pane_id: Some(hovered_pane_id),
            placement_handle_pane_id: None,
            active_input_mode: Some(LockMode::MovePane),
            tabline_offset: None,
            reconnecting: None,
        },
        viewport_area,
        &mut render_buffer,
    );

    assert_eq!(render_buffer[(20, 1)].fg, Theme::default().accent_color);
    assert_eq!(render_buffer[(20, 1)].modifier, Modifier::BOLD);
    assert_eq!(render_buffer[(21, 2)].bg, Color::Rgb(0x3a, 0x3a, 0x3a));
    assert_eq!(
        render_buffer[(0, 1)].fg,
        Theme::default().focused_border_color
    );
}

#[test]
fn five_child_stack_shows_n_minus_one_headers() {
    let active_pane_id = PaneId::new();
    let first_member_pane_id = PaneId::new();
    let second_member_pane_id = PaneId::new();
    let third_member_pane_id = PaneId::new();
    let fourth_member_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (active_pane_id, build_cell_rect(0, 5, 30, 3), true),
            (first_member_pane_id, build_cell_rect(0, 1, 30, 1), false),
            (second_member_pane_id, build_cell_rect(0, 2, 30, 1), false),
            (third_member_pane_id, build_cell_rect(0, 3, 30, 1), false),
            (fourth_member_pane_id, build_cell_rect(0, 4, 30, 1), false),
        ],
        Some(active_pane_id),
        LockMode::Normal,
        Size {
            column_count: 30,
            row_count: 10,
        },
    );
    let member_pane_ids = [
        first_member_pane_id,
        second_member_pane_id,
        third_member_pane_id,
        fourth_member_pane_id,
    ];
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .stack_headers = member_pane_ids
        .iter()
        .enumerate()
        .map(|(member_index, &pane_id)| StackHeader {
            pane_id,
            header_rect: build_cell_rect(0, (member_index + 1) as u16, 30, 1),
            member_index: member_index + 1,
            member_count: 5,
        })
        .collect();
    let render_buffer = render_test_snapshot(&render_snapshot, 30, 10);

    // Four collapsed strips (rows 1..=4). None of the members carries a title,
    // so each reads as the arrow, blanks, then its own right-aligned [k/5].
    for (row_index, stack_member_number) in (2..=5).enumerate() {
        assert_eq!(
            format_rendered_row_text(&render_buffer, (row_index + 1) as u16),
            format!("▸ {}[{stack_member_number}/5]", " ".repeat(23)),
            "row {}",
            row_index + 1
        );
    }
}

#[test]
fn stack_header_without_title_still_shows_arrow_and_indicator() {
    let active_pane_id = PaneId::new();
    let collapsed_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (active_pane_id, build_cell_rect(0, 2, 30, 4), true),
            (collapsed_pane_id, build_cell_rect(0, 1, 30, 1), false),
        ],
        Some(active_pane_id),
        LockMode::Normal,
        Size {
            column_count: 30,
            row_count: 8,
        },
    );
    // The collapsed member carries no title (None from `build`).
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .stack_headers = vec![StackHeader {
        pane_id: collapsed_pane_id,
        header_rect: build_cell_rect(0, 1, 30, 1),
        member_index: 0,
        member_count: 2,
    }];
    let render_buffer = render_test_snapshot(&render_snapshot, 30, 8);

    assert_eq!(
        format_rendered_row_text(&render_buffer, 1),
        format!("▸ {}[1/2]", " ".repeat(23))
    );
}

#[test]
fn narrow_stack_header_indicator_does_not_bleed_left() {
    let active_pane_id = PaneId::new();
    let collapsed_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (active_pane_id, build_cell_rect(0, 2, 20, 4), true),
            (collapsed_pane_id, build_cell_rect(10, 1, 3, 1), false),
        ],
        Some(active_pane_id),
        LockMode::Normal,
        Size {
            column_count: 20,
            row_count: 8,
        },
    );
    // A 3-wide strip at x=10 with a 7-wide indicator "[10/10]".
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .stack_headers = vec![StackHeader {
        pane_id: collapsed_pane_id,
        header_rect: build_cell_rect(10, 1, 3, 1),
        member_index: 9,
        member_count: 10,
    }];
    let render_buffer = render_test_snapshot(&render_snapshot, 20, 8);

    // The indicator clips inside the strip: its first three cells land on
    // x=10..13 and nothing is written left of x=10.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 1),
        format!("{}[10{}", " ".repeat(10), " ".repeat(7))
    );
    for column_index in 0..10 {
        assert_ne!(
            render_buffer[(column_index, 1)].bg,
            Color::Rgb(0x30, 0x0f, 0x4a),
            "col {column_index} styled outside strip"
        );
    }
    // The strip's own cells (x=10..13) carry the strip background.
    for column_index in 10..13 {
        assert_eq!(
            render_buffer[(column_index, 1)].bg,
            Color::Rgb(0x30, 0x0f, 0x4a)
        );
    }
}

#[test]
fn a_stack_header_naming_a_pane_the_frame_dropped_shows_an_empty_title() {
    // A header can name a pane id absent from `panes` (the pane exited and was
    // pruned between the layout solve and the snapshot build): the title falls
    // back to empty and the strip still draws its arrow and indicator.
    let active_pane_id = PaneId::new();
    let pruned_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(active_pane_id, build_cell_rect(0, 2, 30, 4), true)],
        Some(active_pane_id),
        LockMode::Normal,
        Size {
            column_count: 30,
            row_count: 8,
        },
    );
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .stack_headers = vec![StackHeader {
        pane_id: pruned_pane_id,
        header_rect: build_cell_rect(0, 1, 30, 1),
        member_index: 0,
        member_count: 2,
    }];
    let render_buffer = render_test_snapshot(&render_snapshot, 30, 8);

    assert_eq!(
        format_rendered_row_text(&render_buffer, 1),
        format!("▸ {}[1/2]", " ".repeat(23))
    );
    assert_eq!(render_buffer[(0, 1)].bg, Color::Rgb(0x30, 0x0f, 0x4a));
}

#[test]
fn a_zero_size_stack_header_strip_draws_nothing() {
    // A strip solved to zero columns or zero rows is skipped whole: its row
    // keeps the blank cells and the default background it was cleared to.
    let active_pane_id = PaneId::new();
    let collapsed_pane_id = PaneId::new();
    for header_rect in [build_cell_rect(0, 1, 0, 1), build_cell_rect(0, 1, 30, 0)] {
        let mut render_snapshot = build_render_snapshot(
            "sess",
            &[("shell", true)],
            &[(active_pane_id, build_cell_rect(0, 2, 30, 4), true)],
            Some(active_pane_id),
            LockMode::Normal,
            Size {
                column_count: 30,
                row_count: 8,
            },
        );
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .stack_headers = vec![StackHeader {
            pane_id: collapsed_pane_id,
            header_rect,
            member_index: 0,
            member_count: 2,
        }];
        let render_buffer = render_test_snapshot(&render_snapshot, 30, 8);

        assert_eq!(
            format_rendered_row_text(&render_buffer, 1),
            " ".repeat(30),
            "header_rect {header_rect:?}"
        );
        assert_eq!(
            render_buffer[(0, 1)].bg,
            Color::Reset,
            "header_rect {header_rect:?}"
        );
    }
}

/// A one-pane snapshot whose single visible pane shows `grid`.
fn build_content_render_snapshot(
    grid: Grid,
    outer: Rect,
    is_reverse_video: bool,
    viewport_size: Size,
) -> RenderSnapshot {
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, outer, true)],
        Some(pane),
        LockMode::Normal,
        viewport_size,
    );
    render_snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(grid),
        view_row_offset: 0,
    });
    render_snapshot.pane_snapshots[0].is_reverse_video = is_reverse_video;
    render_snapshot
}

#[test]
fn pane_cells_render_with_glyphs_and_styles() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut style = TermStyle::default();
    style.set_foreground_color(TermColor::Rgb(10, 20, 30));
    style.set_background_color(TermColor::Indexed(4));
    style.set_bold(true);
    style.set_italic(true);
    *grid.get_cell_mut(0, 0).unwrap() = Cell::from_character('A', 1, style);
    let render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // Styled glyph at the content origin (inside the one-cell border).
    assert_eq!(render_buffer[(1, 2)].symbol(), "A");
    assert_eq!(render_buffer[(1, 2)].fg, Color::Rgb(10, 20, 30));
    assert_eq!(render_buffer[(1, 2)].bg, Color::Indexed(4));
    assert_eq!(
        render_buffer[(1, 2)].modifier,
        Modifier::BOLD | Modifier::ITALIC
    );

    // A default blank grid cell: a space in the terminal-default (reset) colors.
    assert_eq!(render_buffer[(2, 2)].symbol(), " ");
    assert_eq!(render_buffer[(2, 2)].fg, Color::Reset);
    assert_eq!(render_buffer[(2, 2)].bg, Color::Reset);
}

#[test]
fn wide_glyph_spans_two_columns_without_splitting() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    *grid.get_cell_mut(0, 0).unwrap() = Cell::from_character('中', 2, TermStyle::default());
    // The continuation half of the wide glyph (width 0).
    *grid.get_cell_mut(0, 1).unwrap() = Cell::from_character(' ', 0, TermStyle::default());
    *grid.get_cell_mut(0, 2).unwrap() = Cell::from_character('x', 1, TermStyle::default());
    let render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // The wide glyph sits whole in its base column; its continuation column is
    // left blank, and the next real cell keeps its own grid column (no drift).
    assert_eq!(render_buffer[(1, 2)].symbol(), "中");
    assert_eq!(render_buffer[(2, 2)].symbol(), " ");
    assert_eq!(render_buffer[(3, 2)].symbol(), "x");
}

#[test]
fn wide_glyph_at_right_edge_is_padded() {
    // The content rect is 5 wide (outer 7 minus borders); a wide glyph in the
    // last column has no room for its second half.
    let mut grid = Grid::blank(1, 5, TermStyle::default());
    *grid.get_cell_mut(0, 4).unwrap() = Cell::from_character('中', 2, TermStyle::default());
    let render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 7, 3),
        false,
        Size {
            column_count: 7,
            row_count: 4,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 7, 4);

    // Padded to a blank; a half-glyph never bleeds onto the right border.
    assert_eq!(render_buffer[(5, 2)].symbol(), " ");
    assert_eq!(render_buffer[(6, 2)].symbol(), "│");
}

#[test]
fn combining_marks_join_the_base_into_one_symbol() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut cell = Cell::from_character('e', 1, TermStyle::default());
    cell.push_combining('\u{0301}'); // combining acute accent
    *grid.get_cell_mut(0, 0).unwrap() = cell;
    let render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(render_buffer[(1, 2)].symbol(), "e\u{0301}");
}

#[test]
fn several_marks_join_one_base_into_one_symbol_in_push_order() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut cell = Cell::from_character('e', 1, TermStyle::default());
    cell.push_combining('\u{0301}'); // combining acute accent
    cell.push_combining('\u{0308}'); // combining diaeresis
    *grid.get_cell_mut(0, 0).unwrap() = cell;
    let render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(render_buffer[(1, 2)].symbol(), "e\u{0301}\u{0308}");
}

#[test]
fn every_cell_attribute_maps_to_its_own_modifier() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut every = TermStyle::default();
    every.set_bold(true);
    every.set_faint(true);
    every.set_italic(true);
    every.set_underline(UnderlineStyle::Single);
    every.set_blink(true);
    every.set_conceal(true);
    every.set_strike(true);
    every.set_reverse(true);
    *grid.get_cell_mut(0, 0).unwrap() = Cell::from_character('a', 1, every);

    // A curly underline is one of the five underline styles ratatui cannot tell
    // apart; it draws as the single underline ratatui has.
    let mut curly = TermStyle::default();
    curly.set_underline(UnderlineStyle::Curly);
    *grid.get_cell_mut(0, 1).unwrap() = Cell::from_character('b', 1, curly);

    // Overline and underline color have no ratatui modifier and draw nothing.
    let mut lines = TermStyle::default();
    lines.set_overline(true);
    lines.set_underline_color(Some(TermColor::Indexed(9)));
    *grid.get_cell_mut(0, 2).unwrap() = Cell::from_character('c', 1, lines);

    let render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(
        render_buffer[(1, 2)].modifier,
        Modifier::BOLD
            | Modifier::DIM
            | Modifier::ITALIC
            | Modifier::UNDERLINED
            | Modifier::SLOW_BLINK
            | Modifier::HIDDEN
            | Modifier::CROSSED_OUT
            | Modifier::REVERSED
    );
    assert_eq!(render_buffer[(2, 2)].modifier, Modifier::UNDERLINED);
    assert_eq!(render_buffer[(3, 2)].modifier, Modifier::empty());
}

#[test]
fn reverse_video_toggles_reverse_per_cell() {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    *grid.get_cell_mut(0, 0).unwrap() = Cell::from_character('a', 1, TermStyle::default());
    let mut reversed = TermStyle::default();
    reversed.set_reverse(true);
    *grid.get_cell_mut(0, 1).unwrap() = Cell::from_character('b', 1, reversed);
    let render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 40, 6),
        true,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // Screen reverse (DECSCNM) reverses a plain cell...
    assert_eq!(render_buffer[(1, 2)].modifier, Modifier::REVERSED);
    // ...and cancels a cell that is already reversed (reverse XOR reverse).
    assert_eq!(render_buffer[(2, 2)].modifier, Modifier::empty());
}

#[test]
fn visible_pane_without_grid_draws_no_content() {
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    // `grid_view` is None (a plugin pane or an empty slot): interior stays blank.
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);
    assert_eq!(
        format_rendered_row_text(&render_buffer, 2),
        format!("│{}│", " ".repeat(38))
    );
    assert_eq!(render_buffer[(1, 2)].fg, Color::Reset);
}

#[test]
fn grid_larger_than_content_rect_clips_without_bleeding() {
    // A grid wider and taller than the content header_rect: only the cells that fit are
    // drawn and nothing writes onto the border or past the pane.
    let mut grid = Grid::blank(20, 100, TermStyle::default());
    for column_index in 0..100u16 {
        *grid.get_cell_mut(0, column_index).unwrap() =
            Cell::from_character('#', 1, TermStyle::default());
    }
    let render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // Content fills the content columns (1..=38 of the first content row)...
    assert_eq!(render_buffer[(1, 2)].symbol(), "#");
    assert_eq!(render_buffer[(38, 2)].symbol(), "#");
    // ...and the right border (col 39) is untouched.
    assert_eq!(render_buffer[(39, 2)].symbol(), "│");
}

#[test]
fn grid_smaller_than_content_rect_leaves_remainder_blank() {
    let mut grid = Grid::blank(1, 2, TermStyle::default());
    *grid.get_cell_mut(0, 0).unwrap() = Cell::from_character('h', 1, TermStyle::default());
    *grid.get_cell_mut(0, 1).unwrap() = Cell::from_character('i', 1, TermStyle::default());
    let render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(render_buffer[(1, 2)].symbol(), "h");
    assert_eq!(render_buffer[(2, 2)].symbol(), "i");
    // Beyond the two-cell grid the content rect stays blank.
    assert_eq!(render_buffer[(3, 2)].symbol(), " ");
    assert_eq!(render_buffer[(1, 3)].symbol(), " ");
}

#[test]
fn cursor_at_focused_pane_maps_to_content_cell() {
    // Pane box (0,1) 40x6 → content origin (1,2). Cursor at row 2, col 5 within
    // the content area → absolute buffer cell (1+5, 2+2).
    let mut render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].cursor_snapshot = CursorSnapshot {
        row_index: 2,
        column_index: 5,
        is_visible: true,
        is_blinking: false,
        shape: None,
    };
    assert_eq!(
        get_legacy_cursor_position(&render_snapshot),
        Some(Position::new(6, 4))
    );
}

#[test]
fn cursor_past_content_rect_is_clamped_inside_it() {
    // A frozen cursor (e.g. a dead pane whose content rect later shrank) beyond
    // the content area: the returned cell is clamped to the last cell inside the
    // rect, never onto the border or a neighbour. Content rect origin (1,2),
    // 38x4 → last cell (38, 5).
    let mut render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].cursor_snapshot = CursorSnapshot {
        row_index: 99,
        column_index: 99,
        is_visible: true,
        is_blinking: false,
        shape: None,
    };
    assert_eq!(
        get_legacy_cursor_position(&render_snapshot),
        Some(Position::new(38, 5))
    );
}

#[test]
fn cursor_style_reports_the_focused_panes_shape_and_blink() {
    // vim in insert mode asked for a blinking bar; the caller passes that style
    // out to the terminal koshi is running in.
    let mut render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].cursor_snapshot = CursorSnapshot {
        row_index: 0,
        column_index: 0,
        is_visible: true,
        is_blinking: true,
        shape: Some(CursorShape::Bar),
    };
    assert_eq!(
        get_cursor_style(&render_snapshot),
        Some(CursorStyle::Shaped {
            shape: CursorShape::Bar,
            blink: true
        })
    );
}

#[test]
fn a_pane_that_asked_for_no_shape_leaves_the_users_own_cursor_alone() {
    // A plain shell never sends DECSCUSR. Focusing it must NOT stamp a block
    // over the cursor the user configured in their own terminal — it hands the
    // cursor back to them.
    let render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    assert_eq!(
        render_snapshot.pane_snapshots[0].cursor_snapshot.shape,
        None
    );
    assert_eq!(
        get_cursor_style(&render_snapshot),
        Some(CursorStyle::UserDefault)
    );
}

#[test]
fn cursor_style_is_none_without_a_focused_terminal_pane() {
    let mut render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    // No focused pane_id: nobody speaks for the cursor, so it is left as it is.
    let focused_pane_id = render_snapshot.client_snapshot.focused_pane_id.take();
    assert_eq!(get_cursor_style(&render_snapshot), None);

    // A plugin pane has no terminal, so it has no opinion on the cursor either.
    render_snapshot.client_snapshot.focused_pane_id = focused_pane_id;
    render_snapshot.pane_snapshots[0].terminal_grid_view = None;
    assert_eq!(get_cursor_style(&render_snapshot), None);
}

#[test]
fn hidden_cursor_places_nothing() {
    let mut render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].cursor_snapshot.is_visible = false;
    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn a_scrolled_back_view_places_no_cursor() {
    // The app's cursor is visible, but the view is scrolled into history, so the
    // live cursor cell is off-screen and no hardware cursor is placed.
    let mut render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    assert!(render_snapshot.pane_snapshots[0].cursor_snapshot.is_visible);
    render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_mut()
        .unwrap()
        .view_row_offset = 3;
    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn no_focused_pane_places_no_cursor() {
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "s",
        &[("t", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        None,
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn plugin_pane_places_no_cursor() {
    // A visible focused pane with a visible cursor but no grid is a plugin
    // pane_id: it places no cursor.
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "s",
        &[("t", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    assert_eq!(render_snapshot.pane_snapshots[0].terminal_grid_view, None);
    assert!(render_snapshot.pane_snapshots[0].cursor_snapshot.is_visible);
    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn invisible_focused_pane_places_no_cursor() {
    // Focused pane suppressed / hidden (no content rect): nowhere to place it.
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "s",
        &[("t", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), false)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn cursor_follows_focus_and_never_leaks_to_unfocused_panes() {
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "s",
        &[("t", true)],
        &[
            (left_pane_id, build_cell_rect(0, 1, 20, 6), true),
            (right_pane_id, build_cell_rect(20, 1, 20, 6), true),
        ],
        Some(right_pane_id),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    // Both panes carry a grid and a visible cursor at their own content origin.
    for pane in &mut render_snapshot.pane_snapshots {
        pane.terminal_grid_view = Some(GridView {
            grid: Arc::new(Grid::blank(4, 18, TermStyle::default())),
            view_row_offset: 0,
        });
    }

    // Focused on B (content origin (21,2)): the cursor sits in B, never in A.
    assert_eq!(
        get_legacy_cursor_position(&render_snapshot),
        Some(Position::new(21, 2))
    );

    // Refocus A (content origin (1,2)): the cursor jumps to A.
    render_snapshot.client_snapshot.focused_pane_id = Some(left_pane_id);
    assert_eq!(
        get_legacy_cursor_position(&render_snapshot),
        Some(Position::new(1, 2))
    );
}

#[test]
fn cursor_style_follows_focus_between_panes() {
    // Pane A runs vim in insert mode (it asked for a blinking bar); pane B runs
    // a plain shell (it asked for nothing). The style belongs to the outer
    // terminal, not to a pane's cells: moving focus hands it the newly focused
    // pane's answer, so focusing the shell drops vim's bar.
    let vim_pane_id = PaneId::new();
    let shell_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "s",
        &[("t", true)],
        &[
            (vim_pane_id, build_cell_rect(0, 1, 20, 6), true),
            (shell_pane_id, build_cell_rect(20, 1, 20, 6), true),
        ],
        Some(vim_pane_id),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    for pane in &mut render_snapshot.pane_snapshots {
        pane.terminal_grid_view = Some(GridView {
            grid: Arc::new(Grid::blank(4, 18, TermStyle::default())),
            view_row_offset: 0,
        });
    }
    render_snapshot.pane_snapshots[0].cursor_snapshot.shape = Some(CursorShape::Bar);
    render_snapshot.pane_snapshots[0]
        .cursor_snapshot
        .is_blinking = true;
    render_snapshot.pane_snapshots[1].cursor_snapshot.shape = None;

    assert_eq!(
        get_cursor_style(&render_snapshot),
        Some(CursorStyle::Shaped {
            shape: CursorShape::Bar,
            blink: true
        })
    );

    render_snapshot.client_snapshot.focused_pane_id = Some(shell_pane_id);
    assert_eq!(
        get_cursor_style(&render_snapshot),
        Some(CursorStyle::UserDefault)
    );
}

/// A snapshot whose active tab has no room for any pane_id: every slot suppressed
/// and `all_suppressed` set, as the layout solver produces on a too-small tab.
fn build_too_small_render_snapshot(viewport_size: Size) -> RenderSnapshot {
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), false)],
        Some(pane),
        LockMode::Normal,
        viewport_size,
    );
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .are_all_panes_suppressed = true;
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .is_suppressed = true;
    render_snapshot
}

#[test]
fn too_small_overlay_shown_when_all_suppressed() {
    let render_snapshot = build_too_small_render_snapshot(Size {
        column_count: 60,
        row_count: 10,
    });
    let render_buffer = render_test_snapshot(&render_snapshot, 60, 10);

    // Centered on the middle row (10/2 = 5); the 35-wide message is horizontally
    // centered, starting at col (60-35)/2 = 12, and drawn bold.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 5),
        format!(
            "{}Terminal too small — enlarge window{}",
            " ".repeat(12),
            " ".repeat(13)
        )
    );
    assert_eq!(render_buffer[(12, 5)].modifier, Modifier::BOLD);
}

#[test]
fn too_small_overlay_replaces_tabline_and_panes() {
    let render_snapshot = build_too_small_render_snapshot(Size {
        column_count: 60,
        row_count: 10,
    });
    let render_buffer = render_test_snapshot(&render_snapshot, 60, 10);

    // The overlay owns row 5 alone. Every other row is blank: no tabline, no
    // statusline, and no pane border anywhere.
    for row_index in (0..10).filter(|row_index| *row_index != 5) {
        assert_eq!(
            format_rendered_row_text(&render_buffer, row_index),
            " ".repeat(60),
            "row {row_index}"
        );
    }
}

#[test]
fn too_small_frame_places_no_cursor() {
    // Every pane is suppressed (no content area), so the overlay frame shows no
    // hardware cursor.
    let render_snapshot = build_too_small_render_snapshot(Size {
        column_count: 60,
        row_count: 10,
    });
    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn too_small_overlay_clips_on_narrow_screen() {
    // Viewport narrower than the 35-wide message: it clips to the width with no
    // panic and no write past the right edge.
    let render_snapshot = build_too_small_render_snapshot(Size {
        column_count: 10,
        row_count: 4,
    });
    let render_buffer = render_test_snapshot(&render_snapshot, 10, 4);

    // Centered on row 2; the message saturates to col 0 and shows its 10-cell
    // clipped prefix.
    assert_eq!(format_rendered_row_text(&render_buffer, 2), "Terminal t");
}

#[test]
fn small_and_zero_size_areas_are_safe() {
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 1,
        },
    );

    // One row tall: only the tabline, no bottom row, no panic.
    let one_row = render_test_snapshot(&render_snapshot, 40, 1);
    assert_eq!(
        format_rendered_row_text(&one_row, 0),
        format_session_shell_tabline(40)
    );

    // Widths narrower than the tabline content (the mode tag is 6 cells): the
    // right-aligned segment saturates to col 0 and clips instead of
    // underflowing, and it takes the row whole — no room is left for a tab.
    for (column_count, expected_mode_tag) in [(1, " "), (2, " B"), (3, " BA"), (6, " BASE ")] {
        assert_eq!(
            format_rendered_row_text(&render_test_snapshot(&render_snapshot, column_count, 4), 0),
            expected_mode_tag,
            "column_count {column_count}"
        );
    }

    // Zero area: nothing drawn, no panic.
    let mut empty = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    });
    let regions = build_legacy_regions(0, 0);
    render_frame(
        &render_snapshot,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        &mut empty,
    );
}

/// A letterbox snapshot: a client `viewport` larger than the effective middle
/// pane region, with one visible pane laid out from that region's origin.
fn build_letterbox_render_snapshot(
    pane_id: PaneId,
    viewport_size: Size,
    effective: Size,
) -> RenderSnapshot {
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(
            pane_id,
            build_cell_rect(0, 0, effective.column_count, effective.row_count),
            true,
        )],
        Some(pane_id),
        LockMode::Normal,
        viewport_size,
    );
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .effective_cell_size = effective;
    render_snapshot
}

#[test]
fn larger_viewport_centers_layout_and_letterboxes_margin() {
    let pane = PaneId::new();
    let render_snapshot = build_letterbox_render_snapshot(
        pane,
        Size {
            column_count: 60,
            row_count: 12,
        },
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 60, 12);

    // Effective 40x8 pane region centered in 60x12 → offset (10, 2).
    assert_eq!(render_buffer[(10, 2)].symbol(), "┌");
    assert_eq!(render_buffer[(49, 2)].symbol(), "┐");
    assert_eq!(render_buffer[(10, 9)].symbol(), "└");

    // Chrome stays on outer rows, independent of centered pane geometry.
    assert_eq!(
        format_rendered_row_text(&render_buffer, 0),
        format_session_shell_tabline(60)
    );

    // Margin cells around the pane region carry dim letterbox fill.
    for (column_index, row_index) in [(30, 1), (9, 5), (50, 5), (30, 10)] {
        assert_eq!(
            render_buffer[(column_index, row_index)].symbol(),
            " ",
            "margin ({column_index},{row_index})"
        );
        assert_eq!(
            render_buffer[(column_index, row_index)].bg,
            Color::Rgb(0x58, 0x58, 0x58),
            "margin ({column_index},{row_index})"
        );
    }

    // A cell inside the content rect keeps the default background: the fill
    // lands only in the margin, never over the layout.
    assert_eq!(render_buffer[(11, 3)].bg, Color::Reset);
}

#[test]
fn cursor_shifts_into_centered_content() {
    let pane = PaneId::new();
    let mut render_snapshot = build_letterbox_render_snapshot(
        pane,
        Size {
            column_count: 60,
            row_count: 12,
        },
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(4, 38, TermStyle::default())),
        view_row_offset: 0,
    });
    render_snapshot.pane_snapshots[0].cursor_snapshot = CursorSnapshot {
        row_index: 2,
        column_index: 5,
        is_visible: true,
        is_blinking: false,
        shape: None,
    };

    // Content origin offset (10,2); pane inner origin (1,1) places to (11,3);
    // cursor row 2, col 5 lands at (16,5).
    assert_eq!(
        get_legacy_cursor_position(&render_snapshot),
        Some(Position::new(16, 5))
    );
}

#[test]
fn a_pane_whose_content_rect_holds_no_cells_places_no_cursor() {
    // A pane box of two columns insets to a zero-width content rect and is
    // still marked is_visible: there is no cell inside it to put the cursor on.
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(4, 5, 2, 1), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 10,
        },
    );
    render_snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(4, 1, TermStyle::default())),
        view_row_offset: 0,
    });
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .pane_slots[0]
            .content_rect
            .expect("the slot is visible")
            .cell_size,
        Size {
            column_count: 0,
            row_count: 0
        }
    );

    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn letterbox_clips_to_a_buffer_smaller_than_the_area() {
    // A resize race can hand render_frame an `area` larger than the buffer. The
    // letterbox fill must clip to the buffer, not index out of bounds.
    let pane = PaneId::new();
    let render_snapshot = build_letterbox_render_snapshot(
        pane,
        Size {
            column_count: 60,
            row_count: 12,
        },
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let mut render_buffer = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 30,
        height: 6,
    });
    let regions = build_core_regions(60, 12);
    render_frame(
        &render_snapshot,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 60,
            height: 12,
        },
        &mut render_buffer,
    );

    // No panic, and a margin cell inside the smaller buffer still got the
    // fill (row 0 is the tabline's, so probe the margin band below it).
    assert_eq!(render_buffer[(0, 1)].bg, Color::Rgb(0x58, 0x58, 0x58));
}

#[test]
fn an_area_smaller_than_the_committed_regions_letterboxes_nothing_below_it() {
    // A terminal shrink between the session's last viewport report and this
    // paint: the committed solve is for 60x12, the render area only 30x6, so
    // the centered content rect reaches past the area's bottom and right.
    let pane = PaneId::new();
    let render_snapshot = build_letterbox_render_snapshot(
        pane,
        Size {
            column_count: 60,
            row_count: 12,
        },
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 30,
        height: 6,
    };
    let mut render_buffer = Buffer::empty(area);
    let regions = build_core_regions(60, 12);
    render_frame(
        &render_snapshot,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        area,
        &mut render_buffer,
    );

    // The content rect starts at column 10, row 2, so the band left of it
    // carries the fill and the cells inside it do not.
    assert_eq!(render_buffer[(9, 5)].bg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(render_buffer[(10, 5)].bg, Color::Reset);
}

#[test]
fn chrome_below_a_shrunk_buffer_is_skipped_not_panicked() {
    // Resize race: the snapshot's layout was solved for a taller frame than the
    // current buffer. Chrome rows (stack-header strips) laid out below the buffer
    // must be skipped, not written out of bounds.
    let active_pane_id = PaneId::new();
    let collapsed_pane_id = PaneId::new();
    let column_count = version_badge_column_count() + 16;
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (active_pane_id, build_cell_rect(0, 3, column_count, 6), true),
            (
                collapsed_pane_id,
                build_cell_rect(0, 8, column_count, 1),
                false,
            ),
        ],
        Some(active_pane_id),
        LockMode::Normal,
        Size {
            column_count,
            row_count: 10,
        },
    );
    render_snapshot.pane_snapshots[1].pane_title = Some("logs".to_string());
    // A strip at row 8 — below a buffer only 5 rows tall.
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .stack_headers = vec![StackHeader {
        pane_id: collapsed_pane_id,
        header_rect: build_cell_rect(0, 8, column_count, 1),
        member_index: 1,
        member_count: 2,
    }];

    // Buffer shorter than the solved layout; area matches the buffer.
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: column_count,
        height: 5,
    };
    let mut render_buffer = Buffer::empty(area);
    let regions = build_legacy_regions(column_count, 5);
    render_frame(
        &render_snapshot,
        &regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        area,
        &mut render_buffer,
    );

    // No panic, and the strip laid out at row 8 wrote nothing: row 0 is the
    // tabline (too narrow for the tab, so only the `▶` arrow and the mode tag),
    // row 3 the pane box's top border, row 4 the blanked hint row, and the rest
    // blank.
    let version_badge_text = crate::render::create_version_badge_text();
    assert_eq!(
        format_rendered_row_text(&render_buffer, 0),
        format!(" sess {version_badge_text}   ▶ BASE ")
    );
    assert_eq!(
        format_rendered_row_text(&render_buffer, 1),
        " ".repeat(column_count as usize)
    );
    assert_eq!(
        format_rendered_row_text(&render_buffer, 2),
        " ".repeat(column_count as usize)
    );
    assert_eq!(
        format_rendered_row_text(&render_buffer, 3),
        format!("┌{}┐", "─".repeat(column_count as usize - 2))
    );
    assert_eq!(
        format_rendered_row_text(&render_buffer, 4),
        " ".repeat(column_count as usize)
    );
}

#[test]
fn equal_viewport_draws_no_letterbox() {
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // Effective size equals the viewport_size: the layout fills the frame and no cell
    // carries the letterbox background.
    for row_index in 0..8 {
        for column_index in 0..40 {
            assert_ne!(
                render_buffer[(column_index, row_index)].bg,
                Color::Rgb(0x58, 0x58, 0x58),
                "cell ({column_index},{row_index})"
            );
        }
    }
}

#[test]
fn an_effective_size_larger_than_the_pane_area_draws_no_letterbox() {
    // A client smaller than the size the tab was solved for: the content rect
    // is clamped to the pane area, so it fills the frame and no margin is left.
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .effective_cell_size = Size {
        column_count: 80,
        row_count: 20,
    };
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // The layout is not shifted, and no cell carries the letterbox fill.
    assert_eq!(render_buffer[(0, 1)].symbol(), "┌");
    assert_eq!(render_buffer[(39, 6)].symbol(), "┘");
    for row_index in 0..8 {
        for column_index in 0..40 {
            assert_ne!(
                render_buffer[(column_index, row_index)].bg,
                Color::Rgb(0x58, 0x58, 0x58),
                "cell ({column_index},{row_index})"
            );
        }
    }
}

#[test]
fn an_odd_letterbox_margin_is_one_cell_wider_right_and_below() {
    // 41x9 centered in 60x12 splits 19 spare columns and 3 spare rows unevenly:
    // the halves round down, so the left margin is 9 and the right 10, the top
    // margin 1 row and the bottom 2.
    let pane = PaneId::new();
    let render_snapshot = build_letterbox_render_snapshot(
        pane,
        Size {
            column_count: 60,
            row_count: 12,
        },
        Size {
            column_count: 41,
            row_count: 9,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 60, 12);

    // The pane box starts at (9, 1) and ends at (49, 9).
    assert_eq!(render_buffer[(9, 1)].symbol(), "┌");
    assert_eq!(render_buffer[(49, 1)].symbol(), "┐");
    assert_eq!(render_buffer[(49, 9)].symbol(), "┘");

    // Last margin column on the left, first on the right, and the first margin
    // row below the content.
    assert_eq!(render_buffer[(8, 5)].bg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(render_buffer[(50, 5)].bg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(render_buffer[(30, 10)].bg, Color::Rgb(0x58, 0x58, 0x58));
    // The border column just inside the left margin keeps the default fill.
    assert_eq!(render_buffer[(9, 5)].bg, Color::Reset);
}

/// A non-default palette on the snapshot recolors every chrome element the
/// theme names; the same frame under the default theme paints none of these
/// custom colors.
#[test]
fn a_custom_theme_recolors_the_chrome() {
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true), ("logs", false)],
        &[
            (
                left_pane_id,
                build_cell_rect(0, 1, (version_badge_column_count() + 31) / 2, 6),
                true,
            ),
            (
                right_pane_id,
                build_cell_rect(
                    (version_badge_column_count() + 31) / 2,
                    1,
                    version_badge_column_count() + 31 - (version_badge_column_count() + 31) / 2,
                    6,
                ),
                true,
            ),
        ],
        Some(left_pane_id),
        LockMode::Normal,
        Size {
            column_count: version_badge_column_count() + 31,
            row_count: 8,
        },
    );
    let theme = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ramp_end: (0x00, 0x00, 0xff),
        focused_border_color: Color::Rgb(0xff, 0x88, 0x00),
        unfocused_border_color: Color::Rgb(0x11, 0x22, 0x33),
        ..Theme::default()
    };
    let column_count = version_badge_column_count() + 31;
    let render_buffer = render_test_snapshot_with_theme(&render_snapshot, &theme, column_count, 8);

    // Borders take the theme's border colors.
    assert_eq!(render_buffer[(0, 1)].fg, Color::Rgb(0xff, 0x88, 0x00));
    assert_eq!(
        render_buffer[(column_count / 2, 1)].fg,
        Color::Rgb(0x11, 0x22, 0x33)
    );
    // The session name takes the custom ramp's start end, the mode tag its
    // other end.
    assert_eq!(render_buffer[(1, 0)].fg, Color::Rgb(0xff, 0x00, 0x00));
    assert_eq!(
        render_buffer[(column_count - 2, 0)].fg,
        Color::Rgb(0x00, 0x00, 0xff)
    );
    // The first tab's ribbon sits on the custom ramp's start stop.
    let tab_x = (0..column_count)
        .find(|&column_index| render_buffer[(column_index, 0)].symbol() == "#")
        .expect("tab marker drawn");
    assert_eq!(render_buffer[(tab_x, 0)].fg, Color::Rgb(0xff, 0x00, 0x00));
}

#[test]
fn overlapping_panes_draw_in_layout_order_last_wins() {
    // The layout solver normally tiles panes without overlap; this snapshot
    // forces two visible pane rects to overlap to pin down what the renderer
    // actually does with that input: later slots in `layout_solved` paint
    // over earlier ones, for both the border and the pane content.
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (first_pane_id, build_cell_rect(0, 1, 20, 6), true),
            (second_pane_id, build_cell_rect(15, 1, 20, 6), true),
        ],
        Some(first_pane_id),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let mut first_pane_grid = Grid::blank(4, 18, TermStyle::default());
    *first_pane_grid.get_cell_mut(0, 0).unwrap() =
        Cell::from_character('Z', 1, TermStyle::default());
    *first_pane_grid.get_cell_mut(0, 15).unwrap() =
        Cell::from_character('X', 1, TermStyle::default());
    render_snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(first_pane_grid),
        view_row_offset: 0,
    });
    let mut second_pane_grid = Grid::blank(4, 18, TermStyle::default());
    *second_pane_grid.get_cell_mut(0, 0).unwrap() =
        Cell::from_character('Y', 1, TermStyle::default());
    *second_pane_grid.get_cell_mut(0, 17).unwrap() =
        Cell::from_character('W', 1, TermStyle::default());
    render_snapshot.pane_snapshots[1].terminal_grid_view = Some(GridView {
        grid: Arc::new(second_pane_grid),
        view_row_offset: 0,
    });
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // A's own corner (outside B's rect) survives untouched...
    assert_eq!(render_buffer[(0, 1)].symbol(), "┌");
    assert_eq!(render_buffer[(0, 1)].fg, Color::Rgb(0x00, 0xaf, 0xd7));
    assert_eq!(render_buffer[(0, 1)].modifier, Modifier::BOLD);
    // ...but B (drawn second) overwrites A's right border where they overlap
    // (A's right border sits at x=19, inside B's top-border row): the glyph
    // and color are B's. The BOLD modifier is untouched by B's style (a
    // ratatui `Style` with no `add_modifier` patches, not replaces, so it
    // does not clear a modifier a previous style already set).
    assert_eq!(render_buffer[(19, 1)].symbol(), "─");
    assert_eq!(render_buffer[(19, 1)].fg, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(render_buffer[(19, 1)].modifier, Modifier::BOLD);

    // Content: each pane's own, non-overlapping cell keeps its own glyph...
    assert_eq!(render_buffer[(1, 2)].symbol(), "Z");
    assert_eq!(render_buffer[(33, 2)].symbol(), "W");
    // ...but in the overlap region (screen x=16..19) B's cell wins over A's.
    assert_eq!(render_buffer[(16, 2)].symbol(), "Y");
}

#[test]
fn pane_title_skipped_when_box_is_four_wide() {
    // `rect.width <= 4` guards the `rect.width - 4` subtraction the title
    // clip uses; at exactly 4 there is no room for the ` title ` padding.
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 4, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 10,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].pane_title = Some("editor".to_string());
    let render_buffer = render_test_snapshot(&render_snapshot, 10, 8);

    // Title drawing never ran: the top border keeps a plain dash at column 2,
    // the column a title starts on.
    assert_eq!(render_buffer[(2, 1)].symbol(), "─");
}

#[test]
fn pane_title_drawn_when_box_is_five_wide() {
    // One cell wider crosses the `<= 4` threshold: the title's leading space
    // takes column 2, in place of the dash.
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 5, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 10,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].pane_title = Some("editor".to_string());
    let render_buffer = render_test_snapshot(&render_snapshot, 10, 8);

    assert_eq!(render_buffer[(2, 1)].symbol(), " ");
}

#[test]
fn a_title_wider_than_the_box_is_clipped_short_of_the_corners() {
    // A 10-wide box gives the title six cells: it starts two cells in and
    // stops four short of the box width, so ` abcdefghij ` shows as ` abcde`
    // and the two corner glyphs plus the dash before them survive.
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 10, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 10,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].pane_title = Some("abcdefghij".to_string());
    let render_buffer = render_test_snapshot(&render_snapshot, 10, 8);

    assert_eq!(format_rendered_row_text(&render_buffer, 1), "┌─ abcde─┐");
}

#[test]
fn an_empty_pane_title_leaves_the_top_border_whole() {
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].pane_title = Some(String::new());
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(
        format_rendered_row_text(&render_buffer, 1),
        format!("┌{}┐", "─".repeat(38))
    );
}

#[test]
fn orphan_pane_slot_with_no_matching_snapshot_draws_border_only() {
    // A slot can reference a pane id absent from `panes` (e.g. the pane
    // exited and was pruned between layout solve and snapshot build).
    // `draw_panes` never looks up the pane for its box, so the border still
    // draws; `draw_pane_contents` must skip content without panicking.
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots.clear();
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(
        format_rendered_row_text(&render_buffer, 1),
        format!("┌{}┐", "─".repeat(38))
    );
    assert_eq!(
        format_rendered_row_text(&render_buffer, 2),
        format!("│{}│", " ".repeat(38))
    );
}

#[test]
fn cursor_position_with_focused_pane_absent_from_layout_returns_none() {
    // The client's focused_pane id still has a PaneSnapshot in `panes` (so
    // `find_pane` alone would not catch a missing-slot bug), but its slot was
    // dropped from `layout_solved` this frame (a stale handle after the
    // layout re-solved without it): the layout lookup itself finds nothing.
    let visible_pane_id = PaneId::new();
    let orphaned = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[
            (visible_pane_id, build_cell_rect(0, 1, 20, 6), true),
            (orphaned, build_cell_rect(20, 1, 20, 6), true),
        ],
        Some(orphaned),
        LockMode::Normal,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .retain(|slot| slot.pane_id != orphaned);
    // The orphaned pane still carries a live, visible-cursor grid, so a
    // lookup bug that silently grabs a different slot would still produce a
    // `Some` position (using the wrong slot's rect) rather than `None` by
    // coincidence of some other, unrelated guard.
    render_snapshot.pane_snapshots[1].terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(4, 18, TermStyle::default())),
        view_row_offset: 0,
    });
    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn a_visible_slot_with_no_content_rect_places_no_cursor() {
    // `visible` and `inner_rect` are separate fields on the wire. A slot that
    // says it is visible but carries no content rect has nowhere to put the
    // cursor, so none is placed.
    let mut render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .content_rect = None;
    assert!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .pane_slots[0]
            .is_visible
    );
    assert!(render_snapshot.pane_snapshots[0].cursor_snapshot.is_visible);
    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn a_slot_whose_pane_snapshot_is_gone_places_no_cursor() {
    // The focused pane still has a visible slot with a content rect, but the
    // frame carries no pane snapshot for it: nothing says where the cursor is,
    // so none is placed.
    let mut render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots.clear();
    // Every guard before the pane lookup passes: the slot is there, visible,
    // and carries a content rect.
    assert!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .pane_slots[0]
            .is_visible
    );
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .pane_slots[0]
            .content_rect,
        Some(build_cell_rect(1, 2, 38, 4))
    );
    assert_eq!(get_legacy_cursor_position(&render_snapshot), None);
}

#[test]
fn cursor_style_is_none_when_the_focused_pane_has_no_snapshot() {
    // The focused id names a pane the frame carries no content for: nothing
    // speaks for the cursor, so the outer terminal keeps the style it has.
    let mut render_snapshot = build_content_render_snapshot(
        Grid::blank(4, 38, TermStyle::default()),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots.clear();
    assert_eq!(get_cursor_style(&render_snapshot), None);
}

#[test]
fn one_by_one_viewport_draws_without_panicking() {
    // The smallest possible non-zero area: content_rect and the tabline draw
    // must degrade gracefully rather than underflow or panic. The mode tag
    // saturates the whole 1-cell row, leaving no room for the tab strip, so the
    // single cell falls to the mode block's clipped leading cell — a space.
    let pane = PaneId::new();
    let render_snapshot = build_render_snapshot(
        "sess",
        &[("shell", true)],
        &[(pane, build_cell_rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 1,
            row_count: 1,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 1, 1);

    assert_eq!(render_buffer[(0, 0)].symbol(), " ");
}

// ============================================================================
// Drawing the highlight
// ============================================================================

/// `build_content_render_snapshot` with `rows` highlighted.
fn build_highlighted_render_snapshot(grid: Grid, spans: Vec<(u16, u16, u16)>) -> RenderSnapshot {
    let mut render_snapshot = build_content_render_snapshot(
        grid,
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].selection_spans = Some(SelectionSpans { row_spans: spans });
    render_snapshot
}

/// A grid whose row 0 reads `abcdef`.
fn build_abcdef_grid() -> Grid {
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    for (column_index, character) in "abcdef".chars().enumerate() {
        *grid.get_cell_mut(0, column_index as u16).unwrap() =
            Cell::from_character(character, 1, TermStyle::default());
    }
    grid
}

#[test]
fn highlighted_cells_are_drawn_in_reverse_and_the_rest_are_not() {
    // Highlight columns 1..=3 of row 0: `bcd` of `abcdef`.
    let render_snapshot = build_highlighted_render_snapshot(build_abcdef_grid(), vec![(0, 1, 3)]);
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // Content origin is (1, 2): the one-cell border offsets it.
    assert_eq!(
        render_buffer[(1, 2)].modifier,
        Modifier::empty(),
        "`a` is outside the highlight"
    );
    for column_index in 2..=4 {
        assert_eq!(
            render_buffer[(column_index, 2)].modifier,
            Modifier::REVERSED,
            "column {column_index} is highlighted"
        );
    }
    assert_eq!(
        render_buffer[(5, 2)].modifier,
        Modifier::empty(),
        "`e` is past the highlight"
    );
}

#[test]
fn a_pane_with_no_highlight_draws_nothing_in_reverse() {
    let render_snapshot = build_content_render_snapshot(
        build_abcdef_grid(),
        build_cell_rect(0, 1, 40, 6),
        false,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    for column_index in 1..=6 {
        assert_eq!(
            render_buffer[(column_index, 2)].modifier,
            Modifier::empty(),
            "column {column_index}"
        );
    }
}

#[test]
fn a_highlight_span_that_ends_before_it_starts_highlights_nothing() {
    // A span is `(row, first column, last column)`, both inclusive. `(0, 3, 1)`
    // names an empty range, and no cell on the row is drawn in reverse.
    let render_snapshot = build_highlighted_render_snapshot(build_abcdef_grid(), vec![(0, 3, 1)]);
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    for column_index in 1..=6 {
        assert_eq!(
            render_buffer[(column_index, 2)].modifier,
            Modifier::empty(),
            "column {column_index}"
        );
    }
}

#[test]
fn only_the_highlighted_row_is_reversed() {
    let mut grid = build_abcdef_grid();
    for (column_index, character) in "ghijkl".chars().enumerate() {
        *grid.get_cell_mut(1, column_index as u16).unwrap() =
            Cell::from_character(character, 1, TermStyle::default());
    }
    // Row 1 is highlighted; row 0 is not.
    let render_snapshot = build_highlighted_render_snapshot(grid, vec![(1, 0, 2)]);
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(
        render_buffer[(1, 2)].modifier,
        Modifier::empty(),
        "row 0 is untouched"
    );
    assert_eq!(
        render_buffer[(1, 3)].modifier,
        Modifier::REVERSED,
        "row 1 is highlighted"
    );
}

#[test]
fn highlighting_a_cell_that_is_already_reverse_swaps_it_back() {
    // The highlight combines with the cell's own reverse by exclusive-or, so
    // highlighted reverse text still reads against its surroundings rather than
    // vanishing into them.
    let mut grid = Grid::blank(4, 38, TermStyle::default());
    let mut style = TermStyle::default();
    style.set_reverse(true);
    *grid.get_cell_mut(0, 0).unwrap() = Cell::from_character('a', 1, style);
    let render_snapshot = build_highlighted_render_snapshot(grid, vec![(0, 0, 0)]);
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(
        render_buffer[(1, 2)].modifier,
        Modifier::empty(),
        "already-reverse text highlighted swaps back to normal"
    );
}

#[test]
fn a_highlight_under_screen_wide_reverse_video_swaps_back() {
    // DECSCNM reverses the whole screen; a highlight on top of it swaps those
    // cells back, by the same exclusive-or.
    let mut render_snapshot = build_content_render_snapshot(
        build_abcdef_grid(),
        build_cell_rect(0, 1, 40, 6),
        true,
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    render_snapshot.pane_snapshots[0].selection_spans = Some(SelectionSpans {
        row_spans: vec![(0, 0, 1)],
    });
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    assert_eq!(
        render_buffer[(1, 2)].modifier,
        Modifier::empty(),
        "highlighted, so swapped back out of the screen-wide reverse"
    );
    assert_eq!(
        render_buffer[(3, 2)].modifier,
        Modifier::REVERSED,
        "not highlighted, so still reverse from DECSCNM"
    );
}

#[test]
fn a_highlight_span_wider_than_the_grid_draws_only_real_cells() {
    // A span naming columns past the grid's width cannot paint outside it.
    let render_snapshot = build_highlighted_render_snapshot(build_abcdef_grid(), vec![(0, 0, 200)]);
    let render_buffer = render_test_snapshot(&render_snapshot, 40, 8);

    // The pane's content is 38 wide from x=1, so x=38 is its last column.
    assert_eq!(render_buffer[(38, 2)].modifier, Modifier::REVERSED);
    // The border column past it keeps the focused border's own bold style.
    assert_eq!(render_buffer[(39, 2)].modifier, Modifier::BOLD);
    assert_eq!(render_buffer[(39, 2)].symbol(), "│");
}

#[test]
fn mode_indicator_joins_active_mode_labels() {
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "s",
        &[("t", true)],
        &[(pane, build_cell_rect(0, 1, 20, 4), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 20,
            row_count: 6,
        },
    );

    // Plain mode with the mouse ungrabbed reads BASE.
    assert_eq!(
        build_mode_tags(
            render_snapshot.client_snapshot.lock_mode,
            render_snapshot.client_snapshot.is_mouse_selection_enabled,
            None,
        ),
        "BASE"
    );

    // Mouse-select alone reads SELECT.
    render_snapshot.client_snapshot.is_mouse_selection_enabled = true;
    assert_eq!(
        build_mode_tags(
            render_snapshot.client_snapshot.lock_mode,
            render_snapshot.client_snapshot.is_mouse_selection_enabled,
            None,
        ),
        "SELECT"
    );

    // Locked and grabbing reads both, joined by ` · `.
    render_snapshot.client_snapshot.lock_mode = LockMode::Locked;
    assert_eq!(
        build_mode_tags(
            render_snapshot.client_snapshot.lock_mode,
            render_snapshot.client_snapshot.is_mouse_selection_enabled,
            None,
        ),
        "LOCK · SELECT"
    );

    // Locked alone reads LOCK.
    render_snapshot.client_snapshot.is_mouse_selection_enabled = false;
    assert_eq!(
        build_mode_tags(
            render_snapshot.client_snapshot.lock_mode,
            render_snapshot.client_snapshot.is_mouse_selection_enabled,
            None,
        ),
        "LOCK"
    );
}

#[test]
fn the_mode_indicator_names_every_lock_mode() {
    assert_eq!(build_mode_tags(LockMode::Normal, false, None), "BASE");
    assert_eq!(build_mode_tags(LockMode::Locked, false, None), "LOCK");
    assert_eq!(build_mode_tags(LockMode::Resize, false, None), "RESIZE");
    assert_eq!(
        build_mode_tags(LockMode::MovePane, false, None),
        "MOVE PANE"
    );
    assert_eq!(build_mode_tags(LockMode::TabMode, false, None), "TAB");
    assert_eq!(build_mode_tags(LockMode::ScrollMode, false, None), "SCROLL");

    // Every non-plain mode joins the mouse-select mode tag the same way.
    assert_eq!(
        build_mode_tags(LockMode::Resize, true, None),
        "RESIZE · SELECT"
    );
    assert_eq!(
        build_mode_tags(LockMode::MovePane, true, None),
        "MOVE PANE · SELECT"
    );
    assert_eq!(
        build_mode_tags(LockMode::TabMode, true, None),
        "TAB · SELECT"
    );
    assert_eq!(
        build_mode_tags(LockMode::ScrollMode, true, None),
        "SCROLL · SELECT"
    );
}

#[test]
fn mode_indicator_puts_the_reconnecting_tag_first_and_replaces_base() {
    let pane = PaneId::new();
    let mut render_snapshot = build_render_snapshot(
        "s",
        &[("t", true)],
        &[(pane, build_cell_rect(0, 1, 20, 4), true)],
        Some(pane),
        LockMode::Normal,
        Size {
            column_count: 20,
            row_count: 6,
        },
    );

    let dialing = Some(Reconnecting {
        attempt: 3,
        retry_in_seconds: 8,
    });

    // A reconnecting client in plain mode reads the link mode tag, never BASE, and
    // the mode tag carries the dial it waits for and the seconds left before it.
    assert_eq!(
        build_mode_tags(
            render_snapshot.client_snapshot.lock_mode,
            render_snapshot.client_snapshot.is_mouse_selection_enabled,
            dialing,
        ),
        "RECONNECTING (attempt 3, retry in 8s)"
    );

    // Reconnecting while locked and grabbing puts the link mode tag ahead of both.
    render_snapshot.client_snapshot.lock_mode = LockMode::Locked;
    render_snapshot.client_snapshot.is_mouse_selection_enabled = true;
    assert_eq!(
        build_mode_tags(
            render_snapshot.client_snapshot.lock_mode,
            render_snapshot.client_snapshot.is_mouse_selection_enabled,
            dialing,
        ),
        "RECONNECTING (attempt 3, retry in 8s) · LOCK · SELECT"
    );
}

#[test]
fn text_width_counts_display_cells_not_bytes_or_chars() {
    // Chrome text is placed in terminal cells, so measuring uses display
    // width. "漢字" is 2 chars and 6 bytes but occupies 4 cells; an emoji is
    // 1 char and 4 bytes but occupies 2; a combining mark adds none.
    assert_eq!(get_text_width("漢字"), 4);
    assert_eq!(get_text_width("🦀"), 2);
    assert_eq!(
        get_text_width("e\u{0301}"),
        1,
        "e + combining acute is one cell"
    );
    assert_eq!(get_text_width(""), 0);

    // Past `u16::MAX` cells the count is held there, which every width
    // comparison reads as wider than the row.
    let oversized_text = "x".repeat(usize::from(u16::MAX) + 64);
    assert_eq!(get_text_width(&oversized_text), u16::MAX);
}

#[test]
fn line_width_sums_span_display_cells_and_saturates() {
    // Spans add up in display cells, and styles never change the count.
    let line = Line::from(vec![
        Span::styled("漢字", Style::default().fg(Color::Red)),
        Span::raw("🦀"),
        Span::raw("e\u{0301}"),
    ]);
    assert_eq!(get_line_width(&line), 7);
    assert_eq!(get_line_width(&Line::from("")), 0);

    // Two spans that together pass `u16::MAX` cells are held at `u16::MAX`,
    // never wrapped to a small number that would read as fitting.
    let half_width_text = "x".repeat(usize::from(u16::MAX));
    let oversized_line = Line::from(vec![
        Span::raw(half_width_text.clone()),
        Span::raw(half_width_text),
    ]);
    assert_eq!(get_line_width(&oversized_line), u16::MAX);
}
