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
pub fn render_layouts(session_layouts: &[SessionLayout], output_format: OutputFormat) -> String {
    match output_format {
        OutputFormat::Json => render_json(&session_layouts),
        OutputFormat::Table => {
            let mut rendered_output = String::new();
            for session_layout in session_layouts {
                rendered_output.push_str(&format!(
                    "session {} {}\n",
                    session_layout.session_id, session_layout.session_name
                ));
                for tab_layout in &session_layout.tabs {
                    rendered_output.push_str(&format!(
                        "  tab {} {} index {}\n",
                        tab_layout.tab_id, tab_layout.tab_name, tab_layout.tab_index
                    ));
                    rendered_output.push_str("    tree\n");
                    render_layout_tree(&mut rendered_output, &tab_layout.layout_tree, 3, false);
                    for solved_tab in &tab_layout.solved_tabs {
                        render_solved_tab(&mut rendered_output, solved_tab);
                    }
                    if tab_layout.solved_tabs.is_empty() {
                        rendered_output.push_str("    no client views this tab\n");
                    }
                }
                rendered_output.push_str("  clients\n");
                for client_discovery in &session_layout.clients {
                    rendered_output.push_str(&format!(
                        "    {} tab {} focus {}\n",
                        client_discovery.client_id,
                        client_discovery.active_tab_id,
                        format_optional_cell(client_discovery.focused_pane_id.as_ref())
                    ));
                }
            }
            rendered_output
        }
    }
}

/// Append one client's solve of one tab: the client line, one line per pane
/// rectangle, the panes with no room, then the stack header strips.
fn render_solved_tab(rendered_output: &mut String, solved_tab: &SolvedTab) {
    rendered_output.push_str(&format!(
        "    client {} {} viewport {}\n",
        solved_tab.client_id,
        format_layout_mode_cell(solved_tab.layout_mode),
        format_size_cell(solved_tab.viewport_size)
    ));
    for pane_rect in &solved_tab.pane_rects {
        rendered_output.push_str(&format!(
            "      pane {} rect {}\n",
            pane_rect.pane_id,
            format_rect_cell(pane_rect.outer_rect)
        ));
    }
    if !solved_tab.suppressed_pane_ids.is_empty() {
        let suppressed_pane_names: Vec<String> = solved_tab
            .suppressed_pane_ids
            .iter()
            .map(ToString::to_string)
            .collect();
        rendered_output.push_str(&format!(
            "      no room: {}\n",
            suppressed_pane_names.join(", ")
        ));
    }
    if solved_tab.is_every_pane_suppressed {
        rendered_output.push_str("      no room for any pane\n");
    }
    for stack_header in &solved_tab.stack_headers {
        rendered_output.push_str(&format!(
            "      stack header {} rect {} [{}/{}]\n",
            stack_header.pane_id,
            format_rect_cell(stack_header.header_rect),
            stack_header.member_index + 1,
            stack_header.member_count
        ));
    }
}

/// Append `layout_node`'s label at `depth`, then the labels of everything under it,
/// one line each and two spaces per level. `is_collapsed` says whether the stack
/// member holding `layout_node` is collapsed to its header.
fn render_layout_tree(
    rendered_output: &mut String,
    layout_node: &LayoutNode,
    depth: usize,
    is_collapsed: bool,
) {
    let node_label = match layout_node {
        LayoutNode::Pane(pane_id) => format!("pane {pane_id}"),
        LayoutNode::Split(split) => match split.direction {
            SplitDirection::Horizontal => "horizontal split".to_string(),
            SplitDirection::Vertical => "vertical split".to_string(),
            SplitDirection::Stacked => {
                format!("stacked split, active member {}", split.active_child_index)
            }
        },
    };
    rendered_output.push_str(&"  ".repeat(depth));
    rendered_output.push_str(&node_label);
    if is_collapsed {
        rendered_output.push_str(" (collapsed)");
    }
    rendered_output.push('\n');

    if let LayoutNode::Split(split) = layout_node {
        for (child_index, child_node) in split.children.iter().enumerate() {
            render_layout_tree(
                rendered_output,
                child_node,
                depth + 1,
                split.is_child_collapsed(child_index),
            );
        }
    }
}

/// One rectangle as `x,y colsxrows`.
fn format_rect_cell(rectangle: Rect) -> String {
    let Rect { origin, cell_size } = rectangle;
    format!(
        "{},{} {}",
        origin.column,
        origin.row,
        format_size_cell(cell_size)
    )
}

/// One client's layout mode: `tiled`, or `fullscreen <pane-id>`.
fn format_layout_mode_cell(layout_mode: LayoutMode) -> String {
    match layout_mode {
        LayoutMode::Tiled => "tiled".to_string(),
        LayoutMode::Fullscreen { focused_pane_id } => format!("fullscreen {focused_pane_id}"),
    }
}
