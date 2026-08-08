//! Renderer for `koshi debug dump-layout`: each tab's split tree, the
//! rectangles every viewing client solves that tree to, the panes with no
//! room, the stack header strips, and each client's focus.

use super::*;

use koshi_core::geometry::{Rect, SplitDirection};
use koshi_ipc::layout::{SessionLayout, SolvedTab};
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::LayoutNode;

/// Render a `debug dump-layout` answer. One session split left and right over
/// an 80x22 tab, viewed by one client, results in:
///
/// ```text
/// session session-… quiet-lake
///   tab tab-… editor index 0
///     tree
///       horizontal split
///         pane pane-…4
///         pane pane-…5
///     client client-… tiled viewport 80x22
///       pane pane-…4 rect 0,0 40x22
///       pane pane-…5 rect 40,0 40x22
///   clients
///     client-… tab tab-… focus pane-…4
/// ```
#[must_use]
pub fn render_layouts(layouts: &[SessionLayout], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&layouts),
        FormatArg::Table => {
            let mut rendered = String::new();
            for layout in layouts {
                rendered.push_str(&format!("session {} {}\n", layout.id, layout.name));
                for tab in &layout.tabs {
                    rendered.push_str(&format!(
                        "  tab {} {} index {}\n",
                        tab.id, tab.name, tab.index
                    ));
                    rendered.push_str("    tree\n");
                    write_tree(&mut rendered, &tab.tree, 3, false);
                    for solved in &tab.solved {
                        write_solve(&mut rendered, solved);
                    }
                    if tab.solved.is_empty() {
                        rendered.push_str("    no client views this tab\n");
                    }
                }
                rendered.push_str("  clients\n");
                for client in &layout.clients {
                    rendered.push_str(&format!(
                        "    {} tab {} focus {}\n",
                        client.id,
                        client.active_tab,
                        opt_cell(client.focused_pane.as_ref())
                    ));
                }
            }
            rendered
        }
    }
}

/// Append one client's solve of one tab: the client line, one line per pane
/// rectangle, the panes with no room, then the stack header strips.
fn write_solve(out: &mut String, solved: &SolvedTab) {
    out.push_str(&format!(
        "    client {} {} viewport {}\n",
        solved.client,
        mode_cell(solved.mode),
        size_cell(solved.viewport)
    ));
    for pane in &solved.panes {
        out.push_str(&format!(
            "      pane {} rect {}\n",
            pane.id,
            rect_cell(pane.rect)
        ));
    }
    if !solved.suppressed.is_empty() {
        let panes: Vec<String> = solved.suppressed.iter().map(ToString::to_string).collect();
        out.push_str(&format!("      no room: {}\n", panes.join(", ")));
    }
    if solved.all_suppressed {
        out.push_str("      no room for any pane\n");
    }
    for header in &solved.stack_headers {
        out.push_str(&format!(
            "      stack header {} rect {} [{}/{}]\n",
            header.pane,
            rect_cell(header.rect),
            header.position + 1,
            header.total
        ));
    }
}

/// Append `node`'s label at `depth`, then the labels of everything under it,
/// one line each and two spaces per level. `collapsed` says whether the stack
/// member holding `node` is collapsed to its header.
fn write_tree(out: &mut String, node: &LayoutNode, depth: usize, collapsed: bool) {
    let label = match node {
        LayoutNode::Pane(id) => format!("pane {id}"),
        LayoutNode::Split(split) => match split.direction {
            SplitDirection::Horizontal => "horizontal split".to_string(),
            SplitDirection::Vertical => "vertical split".to_string(),
            SplitDirection::Stacked => {
                format!("stacked split, active member {}", split.active)
            }
        },
    };
    out.push_str(&"  ".repeat(depth));
    out.push_str(&label);
    if collapsed {
        out.push_str(" (collapsed)");
    }
    out.push('\n');

    if let LayoutNode::Split(split) = node {
        for child in &split.children {
            write_tree(out, &child.node, depth + 1, child.collapsed);
        }
    }
}

/// One rectangle as `x,y colsxrows`.
fn rect_cell(rect: Rect) -> String {
    let Rect { origin, size } = rect;
    format!("{},{} {}", origin.x, origin.y, size_cell(size))
}

/// One client's layout mode: `tiled`, or `fullscreen <pane-id>`.
fn mode_cell(mode: LayoutMode) -> String {
    match mode {
        LayoutMode::Tiled => "tiled".to_string(),
        LayoutMode::Fullscreen { focused } => format!("fullscreen {focused}"),
    }
}
