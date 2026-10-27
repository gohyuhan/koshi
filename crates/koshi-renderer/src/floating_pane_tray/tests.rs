//! Tests for where the floating pane tray and its tabline button sit: the
//! button three cells left of the mode tag, the tray hanging from the pane
//! area's top row with its right border under the button's last cell, the
//! slots per row the pane area's width allows, the titlebar's tile title,
//! label and close button and their room, and every frame that has no room
//! for the tray.

use super::*;

use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;
use koshi_test_support::fixtures::build_cell_rect;

use crate::snapshot::{
    ClientSnapshot, CommittedRegions, FloatingPane, FloatingPaneSlot, FloatingPaneTray, PaneSlot,
    PaneSnapshot, RenderSnapshot, SessionSnapshot, TabMetadata, TabSnapshot, ViewerChrome,
};

/// A frame for a client with a `viewport_size` viewport, whose tab is one
/// focused tiled pane filling the pane area under the tabline, and whose
/// session holds one floating pane per title of `floating_pane_titles`, in
/// that order, each minimized for the client.
pub(crate) fn build_tray_test_render_snapshot(
    viewport_size: Size,
    floating_pane_titles: &[&str],
) -> RenderSnapshot {
    let tab_id = TabId::new();
    let tiled_pane_id = PaneId::new();
    let tab_size = Size {
        column_count: viewport_size.column_count,
        row_count: viewport_size.row_count.saturating_sub(2),
    };
    let tiled_outer_rect = Rect::from_size_at_origin(tab_size);
    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: "s".to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "t".to_string(),
                pane_slots: vec![PaneSlot {
                    pane_id: tiled_pane_id,
                    outer_rect: tiled_outer_rect,
                    content_rect: Some(tiled_outer_rect.compute_inner_with_border()),
                    is_visible: true,
                    is_suppressed: false,
                }],
                tab_size,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMetadata {
                tab_id,
                tab_name: "t".to_string(),
                tab_index: 0,
                is_active: true,
            }],
            floating_pane_slots: Vec::new(),
            floating_panes: floating_pane_titles
                .iter()
                .map(|floating_pane_title| FloatingPane::Minimized {
                    pane_id: PaneId::new(),
                    pane_title: Some(floating_pane_title.to_string()),
                })
                .collect(),
        },
        pane_snapshots: vec![PaneSnapshot::from_empty_pane(tiled_pane_id)],
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size,
            active_tab_id: tab_id,
            focused_pane_id: Some(tiled_pane_id),
            active_tab_focused_pane_id: Some(tiled_pane_id),
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            is_mouse_selection_enabled: false,
            last_applied_placement_command_id: None,
        },
        placement_preview: None,
    }
}

/// Show the session's floating pane `tile_index` of `render_snapshot` for the
/// client in the box `outer_rect`, in pane-area cells, on top of the floating
/// panes it shows, with its title as its content's title and no grid. Returns
/// its pane id.
pub(crate) fn show_tray_test_floating_pane(
    render_snapshot: &mut RenderSnapshot,
    tile_index: usize,
    outer_rect: Rect,
) -> PaneId {
    let floating_pane = &mut render_snapshot.session_snapshot.floating_panes[tile_index];
    let FloatingPane::Minimized {
        pane_id,
        pane_title,
    } = floating_pane.clone()
    else {
        panic!("tile {tile_index} shows a minimized pane before it is shown");
    };
    *floating_pane = FloatingPane::Shown { pane_id };
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            pane_id, outer_rect, false,
        ));
    render_snapshot.pane_snapshots.push(PaneSnapshot {
        pane_title,
        ..PaneSnapshot::from_empty_pane(pane_id)
    });
    pane_id
}

/// The viewer chrome of a client whose tray is open on tile
/// `selected_tile_index`, with no tile under the pointer.
pub(crate) fn build_open_tray_viewer_chrome(selected_tile_index: usize) -> ViewerChrome {
    ViewerChrome {
        active_input_mode: Some(LockMode::FloatingPaneTray),
        floating_pane_tray: Some(FloatingPaneTray {
            selected_tile_index,
            hovered_tile_index: None,
        }),
        ..ViewerChrome::default()
    }
}

/// [`compute_floating_pane_tray_layout`] of `render_snapshot` painted with the
/// default viewer chrome over the compiled-in region solve of its viewport.
fn compute_core_tray_layout(render_snapshot: &RenderSnapshot) -> Option<FloatingPaneTrayLayout> {
    let committed_regions =
        CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0);
    compute_floating_pane_tray_layout(
        render_snapshot.build_painted_frame_layout(ViewerChrome::default(), &committed_regions),
    )
}

/// [`find_floating_pane_tray_button_rect`] of `render_snapshot` painted with
/// the default viewer chrome over the compiled-in region solve of its
/// viewport.
fn find_core_button_rect(render_snapshot: &RenderSnapshot) -> Option<Rect> {
    let committed_regions =
        CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0);
    find_floating_pane_tray_button_rect(
        render_snapshot.build_painted_frame_layout(ViewerChrome::default(), &committed_regions),
    )
}

/// The `80x24` viewport the tray tests paint: the tabline on row `0`, the
/// pane area at `(0, 1)` `80x22`, the mode tag ` BASE ` at column `74`, and
/// the tray of a session with a floating pane at `(5, 1)` `69x10`.
pub(crate) const EIGHTY_BY_TWENTY_FOUR_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

#[test]
fn the_button_sits_three_cells_left_of_the_mode_tag_with_its_glyph_in_the_middle() {
    let render_snapshot = build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop"]);
    let committed_regions = CommittedRegions::build_core(EIGHTY_BY_TWENTY_FOUR_SIZE, 0);
    let frame_layout =
        render_snapshot.build_painted_frame_layout(ViewerChrome::default(), &committed_regions);

    assert_eq!(
        find_floating_pane_tray_button_rect(frame_layout),
        Some(build_cell_rect(71, 0, 3, 1))
    );
    assert_eq!(
        find_floating_pane_tray_glyph_cell(frame_layout),
        Some(Point { column: 72, row: 0 })
    );
}

#[test]
fn a_session_with_no_floating_pane_draws_no_button_and_no_tray() {
    let render_snapshot = build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]);

    assert_eq!(find_core_button_rect(&render_snapshot), None);
    assert_eq!(compute_core_tray_layout(&render_snapshot), None);
}

#[test]
fn a_frame_with_every_pane_suppressed_draws_no_button_and_no_tray() {
    let mut render_snapshot =
        build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop"]);
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed = true;

    assert_eq!(find_core_button_rect(&render_snapshot), None);
    assert_eq!(compute_core_tray_layout(&render_snapshot), None);
}

#[test]
fn a_row_with_fewer_than_three_cells_left_of_the_mode_tag_draws_no_button() {
    // ` BASE ` takes columns 2 to 7 of an 8-column row, leaving 2 cells.
    let render_snapshot = build_tray_test_render_snapshot(
        Size {
            column_count: 8,
            row_count: 24,
        },
        &["htop"],
    );

    assert_eq!(find_core_button_rect(&render_snapshot), None);
}

#[test]
fn an_eighty_column_tray_holds_six_slots_per_row_under_the_button() {
    let mut render_snapshot =
        build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop", "logs"]);
    show_tray_test_floating_pane(&mut render_snapshot, 0, build_cell_rect(16, 4, 48, 13));

    let tray_layout = compute_core_tray_layout(&render_snapshot).expect("the tray fits");

    assert_eq!(tray_layout.tray_rect, build_cell_rect(5, 1, 69, 10));
    assert_eq!(tray_layout.slots_per_row, 6);
    assert_eq!(
        tray_layout.slot_rects,
        [
            build_cell_rect(7, 4, 10, 3),
            build_cell_rect(18, 4, 10, 3),
            build_cell_rect(29, 4, 10, 3),
            build_cell_rect(40, 4, 10, 3),
            build_cell_rect(51, 4, 10, 3),
            build_cell_rect(62, 4, 10, 3),
            build_cell_rect(7, 7, 10, 3),
            build_cell_rect(18, 7, 10, 3),
            build_cell_rect(29, 7, 10, 3),
            build_cell_rect(40, 7, 10, 3),
            build_cell_rect(51, 7, 10, 3),
            build_cell_rect(62, 7, 10, 3),
        ]
    );
    assert_eq!(
        tray_layout.tile_title_rect,
        Some(build_cell_rect(7, 2, 47, 1))
    );
    assert_eq!(
        tray_layout.toggle_minimize_all_rect,
        Some(build_cell_rect(56, 2, 12, 1))
    );
    assert_eq!(
        compute_floating_titlebar(
            tray_layout.tray_rect,
            FloatingTitlebarControls::CLOSE_BUTTON_ONLY
        )
        .close_button_rect,
        Some(build_cell_rect(70, 2, 1, 1))
    );
}

#[test]
fn the_restore_all_label_takes_one_cell_less_than_the_minimize_all_label() {
    let render_snapshot =
        build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop", "logs"]);

    let tray_layout = compute_core_tray_layout(&render_snapshot).expect("the tray fits");

    assert_eq!(
        tray_layout.tile_title_rect,
        Some(build_cell_rect(7, 2, 48, 1))
    );
    assert_eq!(
        tray_layout.toggle_minimize_all_rect,
        Some(build_cell_rect(57, 2, 11, 1))
    );
}

#[test]
fn the_label_reads_restore_all_only_while_every_floating_pane_is_minimized() {
    let mut render_snapshot =
        build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop", "logs"]);
    assert_eq!(
        select_toggle_minimize_all_label_text(&render_snapshot.session_snapshot),
        "restore all"
    );

    show_tray_test_floating_pane(&mut render_snapshot, 1, build_cell_rect(16, 4, 48, 13));

    assert_eq!(
        select_toggle_minimize_all_label_text(&render_snapshot.session_snapshot),
        "minimize all"
    );
}

#[test]
fn a_forty_column_tray_holds_two_slots_per_row_and_still_twelve_slots() {
    let render_snapshot = build_tray_test_render_snapshot(
        Size {
            column_count: 40,
            row_count: 24,
        },
        &["htop"],
    );

    let tray_layout = compute_core_tray_layout(&render_snapshot).expect("the tray fits");

    // ` BASE ` starts at column 34, so the button takes 31 to 33 and the tray
    // ends at column 34: 3 slots per row would take 36 columns, 2 slots per
    // row take 25, and 6 rows take 22 rows, the pane area's height.
    assert_eq!(tray_layout.tray_rect, build_cell_rect(9, 1, 25, 22));
    assert_eq!(tray_layout.slots_per_row, 2);
    assert_eq!(tray_layout.slot_rects[0], build_cell_rect(11, 4, 10, 3));
    assert_eq!(tray_layout.slot_rects[3], build_cell_rect(22, 7, 10, 3));
    assert_eq!(tray_layout.slot_rects[11], build_cell_rect(22, 19, 10, 3));
    assert_eq!(
        tray_layout.tile_title_rect,
        Some(build_cell_rect(11, 2, 4, 1))
    );
    assert_eq!(
        tray_layout.toggle_minimize_all_rect,
        Some(build_cell_rect(17, 2, 11, 1))
    );
    assert_eq!(
        compute_floating_titlebar(
            tray_layout.tray_rect,
            FloatingTitlebarControls::CLOSE_BUTTON_ONLY
        )
        .close_button_rect,
        Some(build_cell_rect(30, 2, 1, 1))
    );
}

#[test]
fn a_one_slot_tray_fits_a_pane_area_of_exactly_its_height_and_drops_the_label() {
    // ` BASE ` starts at column 14 of a 20-column row, so the button takes 11
    // to 13 and 14 columns hold one slot per row: 12 rows of slots under the
    // titlebar are 40 rows tall, the pane area's height.
    let render_snapshot = build_tray_test_render_snapshot(
        Size {
            column_count: 20,
            row_count: 42,
        },
        &["htop"],
    );

    let tray_layout = compute_core_tray_layout(&render_snapshot).expect("the tray fits");

    assert_eq!(tray_layout.tray_rect, build_cell_rect(0, 1, 14, 40));
    assert_eq!(tray_layout.slots_per_row, 1);
    assert_eq!(tray_layout.slot_rects[11], build_cell_rect(2, 37, 10, 3));
    assert_eq!(
        tray_layout.tile_title_rect,
        Some(build_cell_rect(2, 2, 6, 1))
    );
    assert_eq!(tray_layout.toggle_minimize_all_rect, None);
    assert_eq!(
        compute_floating_titlebar(
            tray_layout.tray_rect,
            FloatingTitlebarControls::CLOSE_BUTTON_ONLY
        )
        .close_button_rect,
        Some(build_cell_rect(10, 2, 1, 1))
    );
}

#[test]
fn a_pane_area_shorter_than_the_tray_has_no_room_for_it() {
    // An 11-row viewport leaves a 9-row pane area, and a six-slot tray is 10
    // rows tall.
    let short_render_snapshot = build_tray_test_render_snapshot(
        Size {
            column_count: 80,
            row_count: 11,
        },
        &["htop"],
    );
    let exact_render_snapshot = build_tray_test_render_snapshot(
        Size {
            column_count: 80,
            row_count: 12,
        },
        &["htop"],
    );

    assert_eq!(
        find_core_button_rect(&short_render_snapshot),
        Some(build_cell_rect(71, 0, 3, 1))
    );
    assert_eq!(compute_core_tray_layout(&short_render_snapshot), None);
    assert_eq!(
        compute_core_tray_layout(&exact_render_snapshot).map(|tray_layout| tray_layout.tray_rect),
        Some(build_cell_rect(5, 1, 69, 10))
    );
}

#[test]
fn a_pane_area_narrower_than_a_one_slot_tray_left_of_the_border_has_no_room_for_it() {
    // ` BASE ` starts at column 13 of a 19-column row, so the tray would end
    // at column 13: 13 columns, one short of a one-slot tray.
    let render_snapshot = build_tray_test_render_snapshot(
        Size {
            column_count: 19,
            row_count: 40,
        },
        &["htop"],
    );

    assert_eq!(
        find_core_button_rect(&render_snapshot),
        Some(build_cell_rect(10, 0, 3, 1))
    );
    assert_eq!(compute_core_tray_layout(&render_snapshot), None);
}

#[test]
fn the_drawn_tray_follows_the_viewers_tray_state() {
    let render_snapshot = build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop"]);
    let committed_regions = CommittedRegions::build_core(EIGHTY_BY_TWENTY_FOUR_SIZE, 0);

    assert_eq!(
        find_drawn_floating_pane_tray_layout(
            render_snapshot.build_painted_frame_layout(ViewerChrome::default(), &committed_regions)
        ),
        None
    );
    assert_eq!(
        find_drawn_floating_pane_tray_layout(
            render_snapshot
                .build_painted_frame_layout(build_open_tray_viewer_chrome(0), &committed_regions)
        )
        .map(|tray_layout| tray_layout.tray_rect),
        Some(build_cell_rect(5, 1, 69, 10))
    );
}
