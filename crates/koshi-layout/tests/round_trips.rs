//! Round trips across the whole crate: a template becomes a tree, the tree
//! is solved, and the solve becomes per-pane content rects.
//!
//! The unit tests of each module drive one module at a time and hand-build
//! the input the module before it would have produced. These tests chain the
//! real modules together and pin the exact rects the chain ends on.

use koshi_core::geometry::{Point, Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use koshi_layout::content::content_rects;
use koshi_layout::mode::LayoutMode;
use koshi_layout::size::SizeWeight;
use koshi_layout::solver::{solve, solve_with_min, solve_with_mode_min, PaneSizing};
use koshi_layout::template::{LeafTemplate, TemplateNode, TemplateSplit, TerminalTemplate};

/// A terminal leaf running the default shell.
fn leaf() -> TemplateNode {
    TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate::default()))
}

/// `horizontal(leaf, stacked(leaf collapsed, leaf expanded))`: three leaves,
/// the third one the stack's active member.
fn template() -> TemplateNode {
    let stack = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Stacked,
        children: vec![leaf(), leaf()],
        weights: vec![SizeWeight::default(); 2],
        active: 1,
    });
    TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Horizontal,
        children: vec![leaf(), stack],
        weights: vec![SizeWeight::default(); 2],
        active: 0,
    })
}

fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x, y }, Size { cols, rows })
}

fn tab() -> Rect {
    Rect::at_origin(Size { cols: 80, rows: 24 })
}

#[test]
fn a_template_instantiates_solves_and_yields_content_rects() {
    let template = template();
    assert_eq!(template.leaves().len(), 3);
    // The root is directional, so the first visible leaf is the first one.
    assert_eq!(template.first_visible_leaf(), 0);

    let ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let tree = template
        .to_layout_node(&ids)
        .expect("three ids fill three leaves");
    assert_eq!(tree.leaf_panes(), ids);

    // The two root children split 80 columns evenly. The stack spends one of
    // its 24 rows on the collapsed member's header and gives 23 to the
    // active member.
    let solved = solve(&tree, tab());
    assert_eq!(
        solved.panes,
        vec![
            (ids[0], rect(0, 0, 40, 24)),
            (ids[1], rect(40, 0, 40, 1)),
            (ids[2], rect(40, 1, 40, 23)),
        ]
    );
    assert_eq!(solved.stack_headers.len(), 1);
    assert_eq!(solved.stack_headers[0].pane, ids[1]);
    assert!(solved.suppressed.is_empty());

    // Every showing pane loses one cell per side to its border; the
    // collapsed member stands on a header strip and shows nothing.
    assert_eq!(
        content_rects(&solved),
        vec![
            (ids[0], Some(rect(1, 1, 38, 22))),
            (ids[1], None),
            (ids[2], Some(rect(41, 2, 38, 21))),
        ]
    );
}

#[test]
fn a_gap_between_split_children_reaches_the_content_rects() {
    let ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let tree = template()
        .to_layout_node(&ids)
        .expect("three ids fill three leaves");
    let sizing = PaneSizing {
        gap: 2,
        ..PaneSizing::default()
    };

    // Two columns come off the axis before either child is sized: 78 columns
    // split into 39 and 39, with columns 39 and 40 belonging to no pane.
    let solved = solve_with_min(&tree, tab(), sizing);
    assert_eq!(
        solved.panes,
        vec![
            (ids[0], rect(0, 0, 39, 24)),
            (ids[1], rect(41, 0, 39, 1)),
            (ids[2], rect(41, 1, 39, 23)),
        ]
    );
    assert_eq!(
        content_rects(&solved),
        vec![
            (ids[0], Some(rect(1, 1, 37, 22))),
            (ids[1], None),
            (ids[2], Some(rect(42, 2, 37, 21))),
        ]
    );
}

#[test]
fn fullscreen_gives_a_collapsed_stack_member_the_only_content_rect() {
    let ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let tree = template()
        .to_layout_node(&ids)
        .expect("three ids fill three leaves");

    let mode = LayoutMode::Fullscreen { focused: ids[1] };
    let solved = solve_with_mode_min(&tree, mode, tab(), PaneSizing::default());
    assert_eq!(
        solved.panes,
        vec![
            (ids[0], Rect::zero()),
            (ids[1], tab()),
            (ids[2], Rect::zero()),
        ]
    );
    assert!(solved.stack_headers.is_empty());
    assert!(!solved.all_suppressed);

    assert_eq!(
        content_rects(&solved),
        vec![
            (ids[0], None),
            (ids[1], Some(rect(1, 1, 78, 22))),
            (ids[2], None),
        ]
    );
}

#[test]
fn a_tab_below_the_pane_floor_suppresses_the_fullscreen_pane() {
    let ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let tree = template()
        .to_layout_node(&ids)
        .expect("three ids fill three leaves");

    // The default floor is 2 by 1 content plus one border cell per side:
    // 4 by 3. A 4 by 2 tab is one row short.
    let small = Rect::at_origin(Size { cols: 4, rows: 2 });
    let mode = LayoutMode::Fullscreen { focused: ids[1] };
    let solved = solve_with_mode_min(&tree, mode, small, PaneSizing::default());
    assert_eq!(solved.suppressed, vec![ids[1]]);
    assert!(solved.all_suppressed);
    assert_eq!(
        content_rects(&solved),
        vec![(ids[0], None), (ids[1], None), (ids[2], None)]
    );
}
