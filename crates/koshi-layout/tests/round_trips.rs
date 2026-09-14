//! Round trips across the whole crate: a template becomes a tree, the tree
//! is solved, and the solve becomes per-pane content rects.
//!
//! The unit tests of each module drive one module at a time and hand-build
//! the input the module before it would have produced. These tests chain the
//! real modules together and pin the exact rects the chain ends on.

use koshi_core::geometry::{Point, Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use koshi_layout::content::list_content_rects;
use koshi_layout::mode::LayoutMode;
use koshi_layout::size::SizeWeight;
use koshi_layout::solver::{
    solve_layout, solve_layout_with_mode, solve_layout_with_sizing, PaneSizing,
};
use koshi_layout::template::{LeafTemplate, TemplateNode, TemplateSplit, TerminalTemplate};

/// A terminal leaf running the default shell.
fn build_template_leaf() -> TemplateNode {
    TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate::default()))
}

/// `horizontal(leaf, stacked(leaf collapsed, leaf expanded))`: three leaves,
/// the third one the stack's active member.
fn build_test_template() -> TemplateNode {
    let stack = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Stacked,
        children: vec![build_template_leaf(), build_template_leaf()],
        weights: vec![SizeWeight::default(); 2],
        active_child_index: 1,
    });
    TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Horizontal,
        children: vec![build_template_leaf(), stack],
        weights: vec![SizeWeight::default(); 2],
        active_child_index: 0,
    })
}

fn build_cell_rect(column_index: u16, row_index: u16, column_count: u16, row_count: u16) -> Rect {
    Rect::from_origin_and_size(
        Point {
            column: column_index,
            row: row_index,
        },
        Size {
            column_count,
            row_count,
        },
    )
}

fn build_tab_rect() -> Rect {
    Rect::from_size_at_origin(Size {
        column_count: 80,
        row_count: 24,
    })
}

#[test]
fn a_template_instantiates_solves_and_yields_content_rects() {
    let template = build_test_template();
    assert_eq!(template.list_leaf_templates().len(), 3);
    // The root is directional, so the first visible leaf is the first one.
    assert_eq!(template.find_first_visible_leaf_index(), 0);

    let pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let layout_tree = template
        .build_layout_node(&pane_ids)
        .expect("three ids fill three leaves");
    assert_eq!(layout_tree.list_leaf_pane_ids(), pane_ids);

    // The two root children split 80 columns evenly. The stack spends one of
    // its 24 rows on the collapsed member's header and gives 23 to the
    // active member.
    let layout_solution = solve_layout(&layout_tree, build_tab_rect());
    assert_eq!(
        layout_solution.pane_rects,
        vec![
            (pane_ids[0], build_cell_rect(0, 0, 40, 24)),
            (pane_ids[1], build_cell_rect(40, 0, 40, 1)),
            (pane_ids[2], build_cell_rect(40, 1, 40, 23)),
        ]
    );
    assert_eq!(layout_solution.stack_headers.len(), 1);
    assert_eq!(layout_solution.stack_headers[0].pane_id, pane_ids[1]);
    assert!(layout_solution.suppressed_pane_ids.is_empty());

    // Every showing pane loses one cell per side to its border; the
    // collapsed member stands on a header strip and shows nothing.
    assert_eq!(
        list_content_rects(&layout_solution),
        vec![
            (pane_ids[0], Some(build_cell_rect(1, 1, 38, 22))),
            (pane_ids[1], None),
            (pane_ids[2], Some(build_cell_rect(41, 2, 38, 21))),
        ]
    );
}

#[test]
fn a_gap_between_split_children_reaches_the_content_rects() {
    let pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let layout_tree = build_test_template()
        .build_layout_node(&pane_ids)
        .expect("three ids fill three leaves");
    let sizing = PaneSizing {
        gap_cell_count: 2,
        ..PaneSizing::default()
    };

    // Two columns come off the axis before either child is sized: 78 columns
    // split into 39 and 39, with columns 39 and 40 belonging to no pane.
    let layout_solution = solve_layout_with_sizing(&layout_tree, build_tab_rect(), sizing);
    assert_eq!(
        layout_solution.pane_rects,
        vec![
            (pane_ids[0], build_cell_rect(0, 0, 39, 24)),
            (pane_ids[1], build_cell_rect(41, 0, 39, 1)),
            (pane_ids[2], build_cell_rect(41, 1, 39, 23)),
        ]
    );
    assert_eq!(
        list_content_rects(&layout_solution),
        vec![
            (pane_ids[0], Some(build_cell_rect(1, 1, 37, 22))),
            (pane_ids[1], None),
            (pane_ids[2], Some(build_cell_rect(42, 2, 37, 21))),
        ]
    );
}

#[test]
fn fullscreen_gives_a_collapsed_stack_member_the_only_content_rect() {
    let pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let layout_tree = build_test_template()
        .build_layout_node(&pane_ids)
        .expect("three ids fill three leaves");

    let fullscreen_layout_mode = LayoutMode::Fullscreen {
        focused_pane_id: pane_ids[1],
    };
    let layout_solution = solve_layout_with_mode(
        &layout_tree,
        fullscreen_layout_mode,
        build_tab_rect(),
        PaneSizing::default(),
    );
    assert_eq!(
        layout_solution.pane_rects,
        vec![
            (pane_ids[0], Rect::empty_at_origin()),
            (pane_ids[1], build_tab_rect()),
            (pane_ids[2], Rect::empty_at_origin()),
        ]
    );
    assert!(layout_solution.stack_headers.is_empty());
    assert!(!layout_solution.is_all_panes_suppressed);

    assert_eq!(
        list_content_rects(&layout_solution),
        vec![
            (pane_ids[0], None),
            (pane_ids[1], Some(build_cell_rect(1, 1, 78, 22))),
            (pane_ids[2], None),
        ]
    );
}

#[test]
fn a_tab_below_the_pane_floor_suppresses_the_fullscreen_pane() {
    let pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let layout_tree = build_test_template()
        .build_layout_node(&pane_ids)
        .expect("three ids fill three leaves");

    // The default floor is 2 by 1 content plus one border cell per side:
    // 4 by 3. A 4 by 2 tab is one row short.
    let undersized_tab_rect = Rect::from_size_at_origin(Size {
        column_count: 4,
        row_count: 2,
    });
    let fullscreen_layout_mode = LayoutMode::Fullscreen {
        focused_pane_id: pane_ids[1],
    };
    let layout_solution = solve_layout_with_mode(
        &layout_tree,
        fullscreen_layout_mode,
        undersized_tab_rect,
        PaneSizing::default(),
    );
    assert_eq!(layout_solution.suppressed_pane_ids, vec![pane_ids[1]]);
    assert!(layout_solution.is_all_panes_suppressed);
    assert_eq!(
        list_content_rects(&layout_solution),
        vec![
            (pane_ids[0], None),
            (pane_ids[1], None),
            (pane_ids[2], None)
        ]
    );
}
