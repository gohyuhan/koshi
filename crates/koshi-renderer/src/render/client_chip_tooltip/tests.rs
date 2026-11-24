//! Tests for the client chip tooltip: the chip and mark cells that open it,
//! the title and text rows of one client and of a hidden group, where the box
//! opens, how it cuts rows, and the rows a frame redraws when it opens, moves,
//! changes or closes.
//!
//! Most frames are the viewer Alice (index 0, label `alice`) over one tiled
//! pane in an 80x12 viewport. The tab strip is buffer row 0 and the pane fills
//! the rows below it.

use super::*;

use crate::hit_test::{compute_tabline_area, compute_viewport_area, resolve_hit_region, HitRegion};
use crate::render::client_chip::HiddenClientMark;
use crate::render::tests::{
    build_core_regions, build_one_pane_floating_pane_slot, build_peer_snapshot,
    build_render_snapshot, render_frame_to_buffer,
};
use crate::render::{
    build_frame_geometry, list_changed_screen_rows, solve_painted_client_chip_groups,
    solve_tabline_layout,
};
use crate::snapshot::{CommittedRegions, KeymapHints, RenderSnapshot, ViewerChrome};

use koshi_core::geometry::{Point, Size};
use koshi_core::lock::LockMode;
use koshi_test_support::fixtures::build_cell_rect;

const VIEWPORT_COLUMN_COUNT: u16 = 80;
const VIEWPORT_ROW_COUNT: u16 = 12;

fn build_tooltip_frame(
    peer_count: u32,
    viewport_column_count: u16,
    viewport_row_count: u16,
) -> (RenderSnapshot, CommittedRegions, PaneId) {
    let pane_id = PaneId::new();
    let viewport_size = Size {
        column_count: viewport_column_count,
        row_count: viewport_row_count,
    };
    let mut render_snapshot = build_render_snapshot(
        "work",
        &[("shell", true)],
        &[(
            pane_id,
            build_cell_rect(0, 0, viewport_column_count, viewport_row_count - 2),
            true,
        )],
        Some(pane_id),
        LockMode::Normal,
        viewport_size,
    );
    render_snapshot.client_snapshot.label = "alice".to_string();
    render_snapshot.session_snapshot.peers = (1..=peer_count)
        .map(|color_index| build_peer_snapshot(color_index, None))
        .collect();
    let committed_regions = build_core_regions(viewport_column_count, viewport_row_count);
    (render_snapshot, committed_regions, pane_id)
}

fn build_frame_layout<'a>(
    render_snapshot: &'a RenderSnapshot,
    committed_regions: &'a CommittedRegions,
) -> FrameLayout<'a> {
    render_snapshot.build_painted_frame_layout(ViewerChrome::default(), committed_regions)
}

fn build_client_chip_groups(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewport_area: RatatuiRect,
) -> PaintedClientChipGroups {
    solve_painted_client_chip_groups(
        render_snapshot,
        committed_regions,
        ViewerChrome::default(),
        viewport_area,
        &Theme::default(),
    )
}

fn build_tab_strip_chip_group(render_snapshot: &RenderSnapshot) -> ClientChipGroup {
    let committed_regions = build_core_regions(VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let frame_layout = build_frame_layout(render_snapshot, &committed_regions);
    let viewport_area = compute_viewport_area(frame_layout);
    let tabline_area = compute_tabline_area(frame_layout, viewport_area);
    solve_tabline_layout(frame_layout.get_tabline_inputs(), tabline_area)
        .client_chip_group
        .expect("the frame draws a client chip group")
}

fn build_tab_strip_hover(render_snapshot: &RenderSnapshot, span_index: usize) -> ClientChipHover {
    let committed_regions = build_core_regions(VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let frame_layout = build_frame_layout(render_snapshot, &committed_regions);
    let viewport_area = compute_viewport_area(frame_layout);
    let tabline_area = compute_tabline_area(frame_layout, viewport_area);
    let client_chip_group = build_tab_strip_chip_group(render_snapshot);
    let client_chip_span = client_chip_group.client_chip_spans[span_index];
    let target = if client_chip_span.is_viewer_chip {
        ClientChipHoverTarget::ViewerClient
    } else {
        let peer_snapshot = render_snapshot
            .session_snapshot
            .peers
            .iter()
            .find(|peer_snapshot| peer_snapshot.color_index == client_chip_span.color_index)
            .expect("every peer chip names a peer");
        ClientChipHoverTarget::PeerClient {
            client_id: peer_snapshot.client_id,
        }
    };
    ClientChipHover::TabStrip {
        target,
        row_index: tabline_area.y,
        start_column: client_chip_span.start_column,
    }
}

fn build_tab_strip_mark_hover(render_snapshot: &RenderSnapshot) -> ClientChipHover {
    let committed_regions = build_core_regions(VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let frame_layout = build_frame_layout(render_snapshot, &committed_regions);
    let viewport_area = compute_viewport_area(frame_layout);
    let tabline_area = compute_tabline_area(frame_layout, viewport_area);
    let client_chip_group = build_tab_strip_chip_group(render_snapshot);
    let hidden_client_mark = client_chip_group
        .hidden_client_mark
        .expect("the frame draws a hidden client mark");
    ClientChipHover::TabStrip {
        target: ClientChipHoverTarget::HiddenClients,
        row_index: tabline_area.y,
        start_column: hidden_client_mark.start_column,
    }
}

fn solve_tab_strip_tooltip(
    render_snapshot: &RenderSnapshot,
    hover: ClientChipHover,
) -> ClientChipTooltip {
    let committed_regions = build_core_regions(VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let frame_layout = build_frame_layout(render_snapshot, &committed_regions);
    let viewport_area = compute_viewport_area(frame_layout);
    let client_chip_groups =
        build_client_chip_groups(render_snapshot, &committed_regions, viewport_area);
    solve_client_chip_tooltip(
        render_snapshot,
        viewport_area,
        &Theme::default(),
        &client_chip_groups,
        hover,
    )
    .expect("the tooltip has room")
}

fn solve_tooltip_in_viewport(
    render_snapshot: &RenderSnapshot,
    viewport_area: RatatuiRect,
    hover: ClientChipHover,
) -> Option<ClientChipTooltip> {
    let committed_regions = build_core_regions(viewport_area.width, viewport_area.height.max(3));
    let client_chip_groups =
        build_client_chip_groups(render_snapshot, &committed_regions, viewport_area);
    solve_client_chip_tooltip(
        render_snapshot,
        viewport_area,
        &Theme::default(),
        &client_chip_groups,
        hover,
    )
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn set_peer(
    render_snapshot: &mut RenderSnapshot,
    peer_index: usize,
    label: &str,
    origin: ClientOrigin,
    typed_pane_id: Option<PaneId>,
) {
    let peer_snapshot = &mut render_snapshot.session_snapshot.peers[peer_index];
    peer_snapshot.label = label.to_string();
    peer_snapshot.origin = origin;
    peer_snapshot.typed_pane_id = typed_pane_id;
}

fn hovered_target(
    render_snapshot: &RenderSnapshot,
    hit_region: HitRegion,
    pointer_position: Point,
) -> Option<ClientChipHoverTarget> {
    let committed_regions = build_core_regions(VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let frame_layout = build_frame_layout(render_snapshot, &committed_regions);
    let viewport_area = compute_viewport_area(frame_layout);
    let client_chip_groups =
        build_client_chip_groups(render_snapshot, &committed_regions, viewport_area);
    find_client_chip_hover(
        frame_layout,
        &client_chip_groups,
        hit_region,
        pointer_position,
    )
    .map(|client_chip_hover| match client_chip_hover {
        ClientChipHover::TabStrip { target, .. } | ClientChipHover::Pane { target, .. } => target,
    })
}

// --- the cap on the tab strip ------------------------------------------------

#[test]
fn a_viewer_alone_draws_no_chip_group() {
    let (render_snapshot, committed_regions, _) =
        build_tooltip_frame(0, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let frame_layout = build_frame_layout(&render_snapshot, &committed_regions);
    let viewport_area = compute_viewport_area(frame_layout);
    let tabline_area = compute_tabline_area(frame_layout, viewport_area);

    assert_eq!(
        solve_tabline_layout(frame_layout.get_tabline_inputs(), tabline_area).client_chip_group,
        None
    );
}

#[test]
fn two_clients_draw_two_chips_and_no_mark() {
    let (render_snapshot, _, _) = build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);

    let client_chip_group = build_tab_strip_chip_group(&render_snapshot);

    assert_eq!(client_chip_group.client_chip_spans.len(), 2);
    assert_eq!(client_chip_group.hidden_client_mark, None);
}

#[test]
fn three_clients_draw_three_chips_and_no_mark() {
    let (render_snapshot, _, _) = build_tooltip_frame(2, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);

    let client_chip_group = build_tab_strip_chip_group(&render_snapshot);

    assert_eq!(client_chip_group.client_chip_spans.len(), 3);
    assert_eq!(client_chip_group.hidden_client_mark, None);
}

#[test]
fn four_clients_draw_three_chips_and_a_mark_for_the_other_one() {
    let (render_snapshot, _, _) = build_tooltip_frame(3, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);

    let client_chip_group = build_tab_strip_chip_group(&render_snapshot);

    assert_eq!(client_chip_group.client_chip_spans.len(), 3);
    assert_eq!(
        client_chip_group.hidden_client_mark,
        Some(HiddenClientMark {
            start_column: 66,
            client_count: 1,
        })
    );
}

#[test]
fn nineteen_clients_with_the_viewer_tenth_draw_the_viewer_the_first_two_peers_and_sixteen_more() {
    let (render_snapshot, _, _) =
        build_tooltip_frame(18, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);

    let client_chip_group = build_tab_strip_chip_group(&render_snapshot);

    assert_eq!(
        client_chip_group,
        ClientChipGroup {
            start_column: 53,
            client_chip_spans: vec![
                ClientChipSpan {
                    color_index: 0,
                    is_viewer_chip: true,
                    start_column: 53,
                },
                ClientChipSpan {
                    color_index: 1,
                    is_viewer_chip: false,
                    start_column: 57,
                },
                ClientChipSpan {
                    color_index: 2,
                    is_viewer_chip: false,
                    start_column: 61,
                },
            ],
            hidden_client_mark: Some(HiddenClientMark {
                start_column: 65,
                client_count: 16,
            }),
        }
    );
}

// --- which cells open the tooltip -------------------------------------------

#[test]
fn every_cell_of_a_tab_strip_chip_opens_its_client_tooltip() {
    let (render_snapshot, _, _) = build_tooltip_frame(2, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let client_chip_group = build_tab_strip_chip_group(&render_snapshot);
    let expected_targets = [
        ClientChipHoverTarget::ViewerClient,
        ClientChipHoverTarget::PeerClient {
            client_id: build_peer_snapshot(1, None).client_id,
        },
        ClientChipHoverTarget::PeerClient {
            client_id: build_peer_snapshot(2, None).client_id,
        },
    ];

    for (span_index, client_chip_span) in client_chip_group.client_chip_spans.iter().enumerate() {
        for column_offset in 0..3 {
            let pointer_position = Point {
                column: client_chip_span.start_column + column_offset,
                row: 0,
            };
            assert_eq!(
                hovered_target(&render_snapshot, HitRegion::Tabline, pointer_position),
                Some(expected_targets[span_index]),
                "span {span_index} column offset {column_offset}"
            );
        }
    }
}

#[test]
fn the_blank_cell_between_two_tab_strip_chips_opens_no_tooltip() {
    let (render_snapshot, _, _) = build_tooltip_frame(2, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let client_chip_group = build_tab_strip_chip_group(&render_snapshot);
    let first_chip_end_column = client_chip_group.client_chip_spans[0].start_column + 3;

    assert_eq!(
        symbol_at(
            &render_snapshot,
            first_chip_end_column,
            client_chip_group.client_chip_spans[1].start_column,
        ),
        " "
    );
    assert_eq!(
        hovered_target(
            &render_snapshot,
            HitRegion::Tabline,
            Point {
                column: first_chip_end_column,
                row: 0,
            },
        ),
        None
    );
}

#[test]
fn every_cell_of_the_hidden_client_mark_opens_the_hidden_list() {
    let (render_snapshot, _, _) = build_tooltip_frame(4, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let hidden_client_mark = build_tab_strip_chip_group(&render_snapshot)
        .hidden_client_mark
        .expect("the frame draws a hidden client mark");

    for column_offset in 0..compute_hidden_client_mark_width(hidden_client_mark.client_count) {
        assert_eq!(
            hovered_target(
                &render_snapshot,
                HitRegion::Tabline,
                Point {
                    column: hidden_client_mark.start_column + column_offset,
                    row: 0,
                },
            ),
            Some(ClientChipHoverTarget::HiddenClients)
        );
    }
}

fn symbol_at(render_snapshot: &RenderSnapshot, left_column: u16, right_column: u16) -> String {
    let committed_regions = build_core_regions(VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let buffer = render_frame_to_buffer(
        render_snapshot,
        &committed_regions,
        &Theme::default(),
        &KeymapHints::default(),
        ViewerChrome::default(),
    );
    (left_column..right_column)
        .map(|column_index| buffer[(column_index, 0)].symbol())
        .collect()
}

#[test]
fn a_pane_chip_under_the_open_tray_opens_no_tooltip() {
    use crate::floating_pane_tray::tests::{
        build_open_tray_viewer_chrome, build_tray_test_render_snapshot, EIGHTY_BY_TWENTY_FOUR_SIZE,
    };

    let mut render_snapshot =
        build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop"]);
    let pane_id = render_snapshot.pane_snapshots[0].pane_id;
    render_snapshot.session_snapshot.peers = (1..=3)
        .map(|color_index| build_peer_snapshot(color_index, Some(pane_id)))
        .collect();
    let committed_regions = build_core_regions(
        EIGHTY_BY_TWENTY_FOUR_SIZE.column_count,
        EIGHTY_BY_TWENTY_FOUR_SIZE.row_count,
    );
    let viewer_chrome = build_open_tray_viewer_chrome(0);
    let theme = Theme::default();
    let frame_layout =
        render_snapshot.build_painted_frame_layout(viewer_chrome, &committed_regions);
    let frame_geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        viewer_chrome,
        RatatuiRect::new(
            0,
            0,
            EIGHTY_BY_TWENTY_FOUR_SIZE.column_count,
            EIGHTY_BY_TWENTY_FOUR_SIZE.row_count,
        ),
        &theme,
    );
    let pane_chip_group = frame_geometry
        .get_pane_chip_groups()
        .find_pane_chip_group(pane_id, false)
        .expect("the pane draws a client chip group");
    let client_chip_span = pane_chip_group.client_chip_group.client_chip_spans[0];
    let pointer_position = Point {
        column: client_chip_span.start_column,
        row: pane_chip_group.row_index,
    };
    let hit_region = resolve_hit_region(frame_layout, pointer_position);

    assert_ne!(
        hit_region,
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Up,
            corner_horizontal_side: None,
        }
    );
    assert_eq!(
        find_client_chip_hover(
            frame_layout,
            frame_geometry.get_client_chip_groups(),
            hit_region,
            pointer_position
        ),
        None
    );
}
#[test]
fn a_pane_chip_under_a_toast_opens_no_tooltip() {
    use koshi_config::types::ToastPosition;

    use crate::notification::find_drawn_notification_toast_layout;
    use crate::snapshot::NotificationToast;

    let (mut render_snapshot, committed_regions, pane_id) = build_tooltip_frame(4, 80, 24);
    for peer_snapshot in &mut render_snapshot.session_snapshot.peers {
        peer_snapshot.typed_pane_id = Some(pane_id);
    }
    let viewer_chrome = ViewerChrome {
        notification_toast: Some(NotificationToast {
            toast_position: ToastPosition::TopRight,
            text_row_count: 3,
            hidden_per_mille: 0,
        }),
        ..ViewerChrome::default()
    };
    let frame_layout =
        render_snapshot.build_painted_frame_layout(viewer_chrome, &committed_regions);
    let theme = Theme::default();
    let frame_geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        viewer_chrome,
        RatatuiRect::new(0, 0, 80, 24),
        &theme,
    );
    let pane_chip_group = frame_geometry
        .get_pane_chip_groups()
        .find_pane_chip_group(pane_id, false)
        .expect("the pane draws a client chip group");
    let toast_layout =
        find_drawn_notification_toast_layout(frame_layout).expect("the toast has a drawn layout");
    let covered_chip_column = pane_chip_group
        .client_chip_group
        .client_chip_spans
        .iter()
        .flat_map(|client_chip_span| {
            let chip_end_column = client_chip_span.start_column
                + compute_client_chip_width(client_chip_span.color_index);
            client_chip_span.start_column..chip_end_column
        })
        .find(|column| {
            toast_layout.drawn_rect.is_point_inside(Point {
                column: *column,
                row: pane_chip_group.row_index,
            })
        })
        .expect("the toast covers one pane chip cell");
    let pointer_position = Point {
        column: covered_chip_column,
        row: pane_chip_group.row_index,
    };

    assert_eq!(
        find_client_chip_hover(
            frame_layout,
            frame_geometry.get_client_chip_groups(),
            HitRegion::PaneBorder {
                pane_id,
                side: Direction::Up,
                corner_horizontal_side: None,
            },
            pointer_position,
        ),
        None
    );
}

#[test]
fn a_chip_cell_outside_the_tabline_hit_region_opens_no_tooltip() {
    let (render_snapshot, _, _) = build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);

    assert_eq!(
        hovered_target(
            &render_snapshot,
            HitRegion::PaneContent {
                pane_id: PaneId::new(),
            },
            Point { column: 0, row: 0 },
        ),
        None
    );
}

#[test]
fn a_floating_handle_over_a_chip_mark_opens_no_tooltip() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let viewport_size = Size {
        column_count: VIEWPORT_COLUMN_COUNT,
        row_count: 24,
    };
    let mut render_snapshot = build_render_snapshot(
        "work",
        &[("shell", true)],
        &[(
            tiled_pane_id,
            build_cell_rect(0, 0, VIEWPORT_COLUMN_COUNT, 10),
            true,
        )],
        Some(tiled_pane_id),
        LockMode::Normal,
        viewport_size,
    );
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(build_one_pane_floating_pane_slot(
            floating_pane_id,
            build_cell_rect(0, 0, 5, 6),
            false,
        ));
    let mut floating_pane_snapshot = render_snapshot.pane_snapshots[0].clone();
    floating_pane_snapshot.pane_id = floating_pane_id;
    render_snapshot.pane_snapshots.push(floating_pane_snapshot);
    render_snapshot.session_snapshot.peers = (1..=4)
        .map(|color_index| {
            let mut peer = build_peer_snapshot(color_index, None);
            peer.typed_pane_id = Some(floating_pane_id);
            peer
        })
        .collect();
    let committed_regions = build_core_regions(VIEWPORT_COLUMN_COUNT, 24);
    let viewport_area = RatatuiRect::new(0, 0, VIEWPORT_COLUMN_COUNT, 24);
    let theme = Theme::default();
    let client_chip_groups = solve_painted_client_chip_groups(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        viewport_area,
        &theme,
    );
    let frame_layout =
        render_snapshot.build_painted_frame_layout(ViewerChrome::default(), &committed_regions);

    let handle_position = Point { column: 2, row: 1 };
    let handle_region = resolve_hit_region(frame_layout, handle_position);
    assert_eq!(
        handle_region,
        HitRegion::PlacementHandle {
            pane_id: floating_pane_id
        }
    );
    assert_eq!(
        find_client_chip_hover(
            frame_layout,
            &client_chip_groups,
            handle_region,
            handle_position
        ),
        None
    );
    assert_eq!(
        find_client_chip_hover(
            frame_layout,
            &client_chip_groups,
            HitRegion::PaneBorder {
                pane_id: floating_pane_id,
                side: Direction::Up,
                corner_horizontal_side: None,
            },
            Point { column: 2, row: 2 },
        ),
        Some(ClientChipHover::Pane {
            pane_id: floating_pane_id,
            is_on_stack_header: false,
            target: ClientChipHoverTarget::HiddenClients,
            row_index: 2,
            start_column: 1,
        })
    );
}

// --- the text of one client's tooltip ---------------------------------------

#[test]
fn a_remote_peer_names_its_remote_origin() {
    let (mut render_snapshot, _, _) =
        build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    set_peer(&mut render_snapshot, 0, "bob", ClientOrigin::Remote, None);
    let hover = build_tab_strip_hover(&render_snapshot, 1);

    let tooltip = solve_tab_strip_tooltip(&render_snapshot, hover);

    assert_eq!(line_text(&tooltip.title), " 2 bob ");
    assert_eq!(tooltip.text_rows.len(), 1);
    assert_eq!(line_text(&tooltip.text_rows[0]), "remote");
}

#[test]
fn a_peer_of_another_account_on_this_machine_names_that_account() {
    let (mut render_snapshot, _, _) =
        build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    set_peer(
        &mut render_snapshot,
        0,
        "bob",
        ClientOrigin::Local {
            is_same_user: false,
        },
        None,
    );
    let hover = build_tab_strip_hover(&render_snapshot, 1);

    let tooltip = solve_tab_strip_tooltip(&render_snapshot, hover);

    assert_eq!(
        line_text(&tooltip.text_rows[0]),
        "another account on this machine"
    );
}

#[test]
fn a_peer_of_this_account_names_this_account() {
    let (mut render_snapshot, _, _) =
        build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    set_peer(
        &mut render_snapshot,
        0,
        "bob",
        ClientOrigin::Local { is_same_user: true },
        None,
    );
    let hover = build_tab_strip_hover(&render_snapshot, 1);

    let tooltip = solve_tab_strip_tooltip(&render_snapshot, hover);

    assert_eq!(line_text(&tooltip.text_rows[0]), "this account");
}

#[test]
fn the_viewers_own_chip_names_you_with_one_text_row() {
    let (render_snapshot, _, _) = build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let hover = build_tab_strip_hover(&render_snapshot, 0);

    let tooltip = solve_tab_strip_tooltip(&render_snapshot, hover);

    assert_eq!(line_text(&tooltip.title), " 1 alice ");
    assert_eq!(tooltip.text_rows.len(), 1);
    assert_eq!(line_text(&tooltip.text_rows[0]), "you");
}

// --- the text of the hidden list --------------------------------------------

#[test]
fn the_hidden_list_names_each_hidden_client_in_the_groups_order() {
    let (mut render_snapshot, _, _) =
        build_tooltip_frame(4, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    set_peer(&mut render_snapshot, 0, "bob", ClientOrigin::Remote, None);
    set_peer(
        &mut render_snapshot,
        1,
        "carol",
        ClientOrigin::Local { is_same_user: true },
        None,
    );
    set_peer(
        &mut render_snapshot,
        2,
        "host-2",
        ClientOrigin::Local {
            is_same_user: false,
        },
        None,
    );
    set_peer(&mut render_snapshot, 3, "dave", ClientOrigin::Remote, None);

    let tooltip = solve_tab_strip_tooltip(
        &render_snapshot,
        build_tab_strip_mark_hover(&render_snapshot),
    );

    assert_eq!(line_text(&tooltip.title), " 2 more ");
    assert_eq!(tooltip.text_rows.len(), 2);
    assert_eq!(
        line_text(&tooltip.text_rows[0]),
        " 4 host-2 another account on this machine"
    );
    assert_eq!(line_text(&tooltip.text_rows[1]), " 5 dave   remote");
}

#[test]
fn a_hidden_label_takes_one_column_after_the_widest_hidden_label() {
    let (mut render_snapshot, _, _) =
        build_tooltip_frame(4, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    set_peer(&mut render_snapshot, 0, "bob", ClientOrigin::Remote, None);
    set_peer(&mut render_snapshot, 1, "carol", ClientOrigin::Remote, None);
    set_peer(
        &mut render_snapshot,
        2,
        &"L".repeat(32),
        ClientOrigin::Local { is_same_user: true },
        None,
    );
    set_peer(
        &mut render_snapshot,
        3,
        "界界",
        ClientOrigin::Local { is_same_user: true },
        None,
    );

    let tooltip = solve_tab_strip_tooltip(
        &render_snapshot,
        build_tab_strip_mark_hover(&render_snapshot),
    );

    assert_eq!(tooltip.rect.width, 52);
    assert_eq!(compute_line_cell_count(&tooltip.text_rows[0]), 48);
    assert_eq!(compute_line_cell_count(&tooltip.text_rows[1]), 48);
    assert_eq!(
        line_text(&tooltip.text_rows[1]),
        format!(" 5 界界{}this account", " ".repeat(29))
    );
}

#[test]
fn a_hidden_list_cuts_its_rows_and_counts_the_clients_left_out() {
    let (render_snapshot, _, _) =
        build_tooltip_frame(20, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let tooltip = solve_tooltip_in_viewport(
        &render_snapshot,
        RatatuiRect::new(0, 0, 80, 6),
        build_tab_strip_mark_hover(&render_snapshot),
    )
    .expect("three text rows fit below the mark");

    assert_eq!(tooltip.text_rows.len(), 3);
    assert_eq!(
        line_text(&tooltip.text_rows[2]),
        "…and 16 more · koshi list-clients"
    );
}

#[test]
fn a_hidden_list_takes_at_most_ten_text_rows() {
    let (render_snapshot, _, _) = build_tooltip_frame(20, VIEWPORT_COLUMN_COUNT, 13);
    let tooltip = solve_tooltip_in_viewport(
        &render_snapshot,
        RatatuiRect::new(0, 0, 80, 13),
        build_tab_strip_mark_hover(&render_snapshot),
    )
    .expect("ten text rows fit below the mark");

    assert_eq!(tooltip.text_rows.len(), 10);
    assert_eq!(
        line_text(&tooltip.text_rows[9]),
        "…and 9 more · koshi list-clients"
    );
}

// --- where the box opens ----------------------------------------------------

#[test]
fn the_box_opens_below_the_chip_at_the_chips_first_column() {
    let (mut render_snapshot, _, _) =
        build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    set_peer(&mut render_snapshot, 0, "bob", ClientOrigin::Remote, None);
    let hover = build_tab_strip_hover(&render_snapshot, 1);

    let tooltip = solve_tab_strip_tooltip(&render_snapshot, hover);

    assert_eq!(tooltip.rect.x, hover.get_start_column());
    assert_eq!(tooltip.rect.y, 1);
}

#[test]
fn the_box_moves_left_when_it_would_pass_the_viewport_right_edge() {
    let (mut render_snapshot, _, _) =
        build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    set_peer(
        &mut render_snapshot,
        0,
        "bob",
        ClientOrigin::Local {
            is_same_user: false,
        },
        None,
    );

    let tooltip = solve_tooltip_in_viewport(
        &render_snapshot,
        RatatuiRect::new(0, 0, 40, 10),
        ClientChipHover::TabStrip {
            target: ClientChipHoverTarget::PeerClient {
                client_id: build_peer_snapshot(1, None).client_id,
            },
            row_index: 0,
            start_column: 35,
        },
    )
    .expect("the box fits below the chip");

    assert_eq!(tooltip.rect.x, 5);
    assert_eq!(tooltip.rect.right(), 40);
}

#[test]
fn the_box_opens_above_the_chip_when_it_does_not_fit_below() {
    let (render_snapshot, _, _) = build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);

    let tooltip = solve_tooltip_in_viewport(
        &render_snapshot,
        RatatuiRect::new(0, 0, 40, 6),
        ClientChipHover::TabStrip {
            target: ClientChipHoverTarget::PeerClient {
                client_id: build_peer_snapshot(1, None).client_id,
            },
            row_index: 4,
            start_column: 10,
        },
    )
    .expect("the box fits above the chip");

    assert_eq!(tooltip.rect.y, 1);
    assert_eq!(tooltip.rect.height, 3);
}

#[test]
fn no_box_opens_when_neither_side_holds_one_text_row() {
    let (render_snapshot, _, _) = build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);

    let tooltip = solve_tooltip_in_viewport(
        &render_snapshot,
        RatatuiRect::new(0, 0, 40, 3),
        ClientChipHover::TabStrip {
            target: ClientChipHoverTarget::PeerClient {
                client_id: build_peer_snapshot(1, None).client_id,
            },
            row_index: 0,
            start_column: 10,
        },
    );

    assert_eq!(tooltip, None);
}

// --- what a frame redraws and draws -----------------------------------------

#[test]
fn opening_the_box_lists_the_rows_it_covers() {
    let (render_snapshot, committed_regions, _) =
        build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let viewer_chrome = ViewerChrome {
        client_chip_hover: Some(build_tab_strip_hover(&render_snapshot, 1)),
        ..ViewerChrome::default()
    };
    let viewport_area = RatatuiRect::new(0, 0, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let theme = Theme::default();
    let previous_geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        viewport_area,
        &theme,
    );
    let next_geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        viewer_chrome,
        viewport_area,
        &theme,
    );
    let tooltip = next_geometry
        .get_client_chip_tooltip()
        .expect("the hovered chip opens a box");

    let changed_screen_rows = list_changed_screen_rows(
        &render_snapshot,
        previous_geometry.get_peer_cursor_markers(),
        previous_geometry.get_pane_chip_groups(),
        previous_geometry.get_client_chip_tooltip(),
        &next_geometry,
    )
    .expect("the frame is compared row by row");

    assert_eq!(changed_screen_rows, tooltip.list_rows().collect::<Vec<_>>());
}

#[test]
fn a_tooltip_change_lists_the_rows_of_the_old_and_the_new_box() {
    let (mut render_snapshot, committed_regions, _) =
        build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    set_peer(&mut render_snapshot, 0, "bob", ClientOrigin::Remote, None);
    let viewer_chrome = ViewerChrome {
        client_chip_hover: Some(build_tab_strip_hover(&render_snapshot, 1)),
        ..ViewerChrome::default()
    };
    let viewport_area = RatatuiRect::new(0, 0, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let theme = Theme::default();
    let previous_geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        viewer_chrome,
        viewport_area,
        &theme,
    );
    let previous_tooltip = previous_geometry
        .get_client_chip_tooltip()
        .expect("the hovered chip opens a box")
        .clone();
    let mut next_render_snapshot = render_snapshot.clone();
    next_render_snapshot.session_snapshot.peers[0].label = "carol".to_string();
    let next_geometry = build_frame_geometry(
        &next_render_snapshot,
        &committed_regions,
        viewer_chrome,
        viewport_area,
        &theme,
    );
    let next_tooltip = next_geometry
        .get_client_chip_tooltip()
        .expect("the hovered chip opens a box");

    let changed_screen_rows = list_changed_screen_rows(
        &render_snapshot,
        previous_geometry.get_peer_cursor_markers(),
        previous_geometry.get_pane_chip_groups(),
        Some(&previous_tooltip),
        &next_geometry,
    )
    .expect("the frame is compared row by row");

    assert_ne!(previous_tooltip, *next_tooltip);
    assert_eq!(
        changed_screen_rows,
        previous_tooltip
            .list_rows()
            .chain(next_tooltip.list_rows())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_whole_frame_draws_the_open_box_over_the_tab_strip() {
    let (mut render_snapshot, committed_regions, _) =
        build_tooltip_frame(1, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    set_peer(&mut render_snapshot, 0, "bob", ClientOrigin::Remote, None);
    let viewer_chrome = ViewerChrome {
        client_chip_hover: Some(build_tab_strip_hover(&render_snapshot, 1)),
        ..ViewerChrome::default()
    };
    let viewport_area = RatatuiRect::new(0, 0, VIEWPORT_COLUMN_COUNT, VIEWPORT_ROW_COUNT);
    let theme = Theme::default();
    let geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        viewer_chrome,
        viewport_area,
        &theme,
    );
    let tooltip = geometry
        .get_client_chip_tooltip()
        .expect("the hovered chip opens a box");

    let buffer = render_frame_to_buffer(
        &render_snapshot,
        &committed_regions,
        &theme,
        &KeymapHints::default(),
        viewer_chrome,
    );

    let title_row: String = (tooltip.rect.x..tooltip.rect.right())
        .map(|column_index| buffer[(column_index, tooltip.rect.y)].symbol())
        .collect();
    assert_eq!(title_row, "┌ 2 bob ──┐");
    let origin_row: String = (tooltip.rect.x..tooltip.rect.right())
        .map(|column_index| buffer[(column_index, tooltip.rect.y + 1)].symbol())
        .collect();
    assert_eq!(origin_row, "│ remote  │");
}
