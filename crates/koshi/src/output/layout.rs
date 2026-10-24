//! Renderer for `koshi debug dump-layout`: each tab's split tree, the
//! rectangles every viewing client solves that tree to, the panes with no
//! room, the stack header strips, each floating pane with what each client
//! draws for it, and each client's focus.
//!
//! Each session layout is built once into the reports below. The JSON form
//! serializes them, and the table lines read them.

use super::*;

use koshi_core::geometry::{
    FloatingPaneDimension, FloatingPanePosition, Point, Rect, SplitDirection,
};
use koshi_core::text::format_counted_noun;
use koshi_ipc::layout::{
    ClientFocus, FloatingPaneLayout, FloatingPaneVisibility, SessionLayout, SolvedTab, TabLayout,
};
use koshi_layout::mode::LayoutMode;
use koshi_layout::size::{SizeConstraint, SizeWeight};
use koshi_layout::tree::LayoutNode;

/// Render a `debug dump-layout` answer. One session split left and right over
/// an 80x22 tab, viewed by one client, with one floating pane that client
/// shows at the default placement, results in:
///
/// ```text
/// session session-… quiet-lake
///   tab tab-… editor index 0
///     tree
///       horizontal split
///         pane pane-…4, weight 1
///         pane pane-…5, weight 1
///     client client-… tiled viewport 80x22
///       pane pane-…4 rect 0,0 40x22
///       pane pane-…5 rect 40,0 40x22
///   floating panes
///     pane pane-…6 asks 60%x60% size 48x13
///       client client-… rect 16,4 48x13 layer 0 position default
///   clients
///     client-… tab tab-… focus pane-…6
/// ```
///
/// The `floating panes` section is left out when the session holds no
/// floating pane.
#[must_use]
pub fn render_layouts(session_layouts: &[SessionLayout], output_format: OutputFormat) -> String {
    let session_layout_reports: Vec<SessionLayoutReport> = session_layouts
        .iter()
        .map(build_session_layout_report)
        .collect();
    match output_format {
        OutputFormat::Json => render_json(&session_layout_reports),
        OutputFormat::Table => {
            let mut rendered_output = String::new();
            for session_layout_report in &session_layout_reports {
                rendered_output.push_str(&format!(
                    "session {} {}\n",
                    session_layout_report.session_id, session_layout_report.session_name
                ));
                for tab_layout_report in &session_layout_report.tabs {
                    rendered_output.push_str(&format!(
                        "  tab {} {} index {}\n",
                        tab_layout_report.tab_id,
                        tab_layout_report.tab_name,
                        tab_layout_report.tab_index
                    ));
                    rendered_output.push_str("    tree\n");
                    render_layout_tree(
                        &mut rendered_output,
                        &tab_layout_report.layout_tree,
                        3,
                        None,
                    );
                    for solved_tab_report in &tab_layout_report.solved_tabs {
                        render_solved_tab(&mut rendered_output, solved_tab_report);
                    }
                    if tab_layout_report.solved_tabs.is_empty() {
                        rendered_output.push_str("    no client views this tab\n");
                    }
                }
                if !session_layout_report.floating_panes.is_empty() {
                    rendered_output.push_str("  floating panes\n");
                }
                for floating_pane_report in &session_layout_report.floating_panes {
                    render_floating_pane(&mut rendered_output, floating_pane_report);
                }
                rendered_output.push_str("  clients\n");
                for client_focus_report in &session_layout_report.clients {
                    rendered_output.push_str(&format!(
                        "    {} tab {} focus {}\n",
                        client_focus_report.client_id,
                        client_focus_report.active_tab_id,
                        format_optional_cell(client_focus_report.focused_pane_id.as_ref())
                    ));
                }
            }
            rendered_output
        }
    }
}

/// One session's layout as both formats of `debug dump-layout` print it.
/// Every id is in its prefixed form, such as `pane-<uuid>`.
#[derive(Serialize)]
struct SessionLayoutReport {
    /// The session id.
    session_id: String,
    /// The session's display name.
    session_name: String,
    /// The tabs described, in tab-bar order.
    tabs: Vec<TabLayoutReport>,
    /// Every floating pane of the session, in the order the panes became
    /// floating.
    floating_panes: Vec<FloatingPaneReport>,
    /// Every attached client, and what it has focused.
    clients: Vec<ClientFocusReport>,
}

/// One tab: its split tree, and the rectangles each viewing client solved
/// that tree to.
#[derive(Serialize)]
struct TabLayoutReport {
    /// The tab id.
    tab_id: String,
    /// The tab's display name.
    tab_name: String,
    /// The tab's position in the bar, starting at 0.
    tab_index: usize,
    /// The tab's split tree, unsolved.
    layout_tree: LayoutNodeReport,
    /// One entry per client viewing the tab. Empty when no client views it.
    solved_tabs: Vec<SolvedTabReport>,
}

/// One node of a split tree. JSON writes the case as `"node"`: `"pane"` or
/// `"split"`.
#[derive(Serialize)]
#[serde(tag = "node", rename_all = "lowercase")]
enum LayoutNodeReport {
    /// A pane leaf.
    Pane {
        /// The pane id.
        pane_id: String,
        /// Whether the stack member holding the pane is collapsed to its
        /// header.
        is_collapsed: bool,
    },
    /// A split over `children`.
    Split {
        /// How the children divide the split's area, as
        /// [`format_split_direction`] names it.
        split_direction: &'static str,
        /// The member a `stacked` split shows open, or `None` for a
        /// `horizontal` or `vertical` split.
        active_child_index: Option<usize>,
        /// How each child is sized, in layout order, as
        /// [`format_size_weight`] writes it: one entry per child.
        weights: Vec<String>,
        /// Whether the stack member holding the split is collapsed to its
        /// header.
        is_collapsed: bool,
        /// The child nodes, in layout order.
        children: Vec<LayoutNodeReport>,
    },
}

/// One client's solve of one tab's tree.
#[derive(Serialize)]
struct SolvedTabReport {
    /// The client the solve belongs to.
    client_id: String,
    /// The size the tree solves against.
    viewport_size: Size,
    /// Whether the client sees the tab tiled or one pane fullscreen.
    #[serde(flatten)]
    layout_mode: LayoutModeReport,
    /// Every leaf pane exactly once, in layout order, with its rectangle.
    pane_rects: Vec<SolvedPaneReport>,
    /// The panes clipped to zero area when the layout does not fit.
    suppressed_pane_ids: Vec<String>,
    /// `true` when `suppressed_pane_ids` is not empty and every rectangle in
    /// `pane_rects` is zero-area.
    is_every_pane_suppressed: bool,
    /// One header strip per collapsed stack member, in layout order.
    stack_headers: Vec<StackHeaderReport>,
}

/// How one client sees a tab. JSON writes the case as `"layout_mode"`:
/// `"tiled"`, or `"fullscreen"` with `"fullscreen_pane_id"`.
#[derive(Serialize)]
#[serde(tag = "layout_mode", rename_all = "lowercase")]
enum LayoutModeReport {
    /// Every pane gets its tiled rectangle.
    Tiled,
    /// One pane fills the whole tab.
    Fullscreen {
        /// The pane that fills the tab.
        fullscreen_pane_id: String,
    },
}

/// One pane's place in a solve.
#[derive(Serialize)]
struct SolvedPaneReport {
    /// The pane id.
    pane_id: String,
    /// The pane's rectangle, including its 1-cell border on each side.
    outer_rect: Rect,
}

/// One collapsed stack member's header strip.
#[derive(Serialize)]
struct StackHeaderReport {
    /// The collapsed pane the header represents.
    pane_id: String,
    /// The strip: one row spanning the stack's width.
    header_rect: Rect,
    /// Zero-based position of the member within its stack.
    member_index: usize,
    /// Total members in the stack, the open one included.
    member_count: usize,
}

/// One floating pane: the size it asks for, the size it holds, and what each
/// attached client draws for it.
#[derive(Serialize)]
struct FloatingPaneReport {
    /// The pane id.
    pane_id: String,
    /// The size the pane asks for, per axis.
    desired_size: FloatingPaneSizeReport,
    /// The pane's outer size in cells, its chrome included.
    solved_size: Size,
    /// One view per attached client, in client id order.
    client_views: Vec<FloatingPaneClientViewReport>,
}

/// The size a floating pane asks for, each axis in the form `--cols` and
/// `--rows` take: `80` for 80 cells, `60%` for 60 percent of the axis.
#[derive(Serialize)]
struct FloatingPaneSizeReport {
    /// The width.
    width: String,
    /// The height.
    height: String,
}

/// One attached client's view of one floating pane.
#[derive(Serialize)]
struct FloatingPaneClientViewReport {
    /// The client holding the view.
    client_id: String,
    /// The cell the client stored for the pane.
    #[serde(flatten)]
    position: FloatingPanePositionReport,
    /// What the client draws for the pane.
    #[serde(flatten)]
    visibility: FloatingPaneVisibilityReport,
}

/// The cell one client stored for one floating pane. JSON writes the case as
/// `"position"`: `"default"`, or `"moved"` or `"pinned"` with `"stored_cell"`.
#[derive(Serialize)]
#[serde(tag = "position", rename_all = "lowercase")]
enum FloatingPanePositionReport {
    /// The client stored no cell.
    Default,
    /// The pane's top-left cell, stored by a move, a resize, or the command
    /// that created the pane.
    Moved {
        /// The stored top-left cell.
        stored_cell: Point,
    },
    /// The pane's top-left cell, locked.
    Pinned {
        /// The stored top-left cell.
        stored_cell: Point,
    },
}

/// What one client draws for one floating pane. JSON writes the case as
/// `"visibility"`: `"drawn"` with `"top_left_cell"` and `"layer_index"`,
/// `"minimized"`, or `"no floating viewport"`.
#[derive(Serialize)]
#[serde(tag = "visibility")]
enum FloatingPaneVisibilityReport {
    /// The client draws the pane's box with its top-left cell at
    /// `top_left_cell`, at `layer_index` in its drawing order.
    #[serde(rename = "drawn")]
    Drawn {
        /// The top-left cell of the pane's box, counted from the client's
        /// pane-area origin.
        top_left_cell: Point,
        /// The pane's place in the order the client draws its floating panes:
        /// `0` draws first.
        layer_index: usize,
    },
    /// The client holds the pane minimized.
    #[serde(rename = "minimized")]
    Minimized,
    /// The client shows the pane and has no floating viewport to draw it in.
    #[serde(rename = "no floating viewport")]
    NoFloatingViewport,
}

/// One attached client's focus.
#[derive(Serialize)]
struct ClientFocusReport {
    /// The client id.
    client_id: String,
    /// The tab the client is viewing.
    active_tab_id: String,
    /// The pane that takes the client's input, or `None` until the client
    /// focuses one.
    focused_pane_id: Option<String>,
}

/// One [`SessionLayout`] as both formats print it.
fn build_session_layout_report(session_layout: &SessionLayout) -> SessionLayoutReport {
    SessionLayoutReport {
        session_id: session_layout.session_id.to_string(),
        session_name: session_layout.session_name.clone(),
        tabs: session_layout
            .tabs
            .iter()
            .map(build_tab_layout_report)
            .collect(),
        floating_panes: session_layout
            .floating_panes
            .iter()
            .map(build_floating_pane_report)
            .collect(),
        clients: session_layout
            .clients
            .iter()
            .map(build_client_focus_report)
            .collect(),
    }
}

/// One [`TabLayout`] as both formats print it.
fn build_tab_layout_report(tab_layout: &TabLayout) -> TabLayoutReport {
    TabLayoutReport {
        tab_id: tab_layout.tab_id.to_string(),
        tab_name: tab_layout.tab_name.clone(),
        tab_index: tab_layout.tab_index,
        layout_tree: build_layout_node_report(&tab_layout.layout_tree, false),
        solved_tabs: tab_layout
            .solved_tabs
            .iter()
            .map(build_solved_tab_report)
            .collect(),
    }
}

/// `layout_node` and everything under it as both formats print it.
/// `is_collapsed` says whether the stack member holding `layout_node` is
/// collapsed to its header.
fn build_layout_node_report(layout_node: &LayoutNode, is_collapsed: bool) -> LayoutNodeReport {
    match layout_node {
        LayoutNode::Pane(pane_id) => LayoutNodeReport::Pane {
            pane_id: pane_id.to_string(),
            is_collapsed,
        },
        LayoutNode::Split(split) => LayoutNodeReport::Split {
            split_direction: format_split_direction(split.direction),
            active_child_index: match split.direction {
                SplitDirection::Stacked => Some(split.active_child_index),
                SplitDirection::Horizontal | SplitDirection::Vertical => None,
            },
            weights: split
                .weights
                .iter()
                .copied()
                .map(format_size_weight)
                .collect(),
            is_collapsed,
            children: split
                .children
                .iter()
                .enumerate()
                .map(|(child_index, child_node)| {
                    build_layout_node_report(child_node, split.is_child_collapsed(child_index))
                })
                .collect(),
        },
    }
}

/// One [`SolvedTab`] as both formats print it.
fn build_solved_tab_report(solved_tab: &SolvedTab) -> SolvedTabReport {
    SolvedTabReport {
        client_id: solved_tab.client_id.to_string(),
        viewport_size: solved_tab.viewport_size,
        layout_mode: match solved_tab.layout_mode {
            LayoutMode::Tiled => LayoutModeReport::Tiled,
            LayoutMode::Fullscreen { focused_pane_id } => LayoutModeReport::Fullscreen {
                fullscreen_pane_id: focused_pane_id.to_string(),
            },
        },
        pane_rects: solved_tab
            .pane_rects
            .iter()
            .map(|solved_pane| SolvedPaneReport {
                pane_id: solved_pane.pane_id.to_string(),
                outer_rect: solved_pane.outer_rect,
            })
            .collect(),
        suppressed_pane_ids: solved_tab
            .suppressed_pane_ids
            .iter()
            .map(ToString::to_string)
            .collect(),
        is_every_pane_suppressed: solved_tab.is_every_pane_suppressed,
        stack_headers: solved_tab
            .stack_headers
            .iter()
            .map(|stack_header| StackHeaderReport {
                pane_id: stack_header.pane_id.to_string(),
                header_rect: stack_header.header_rect,
                member_index: stack_header.member_index,
                member_count: stack_header.member_count,
            })
            .collect(),
    }
}

/// One [`FloatingPaneLayout`] as both formats print it.
fn build_floating_pane_report(floating_pane_layout: &FloatingPaneLayout) -> FloatingPaneReport {
    FloatingPaneReport {
        pane_id: floating_pane_layout.pane_id.to_string(),
        desired_size: FloatingPaneSizeReport {
            width: format_floating_pane_dimension_cell(floating_pane_layout.desired_size.width),
            height: format_floating_pane_dimension_cell(floating_pane_layout.desired_size.height),
        },
        solved_size: floating_pane_layout.solved_size,
        client_views: floating_pane_layout
            .client_views
            .iter()
            .map(|client_view| FloatingPaneClientViewReport {
                client_id: client_view.client_id.to_string(),
                position: match client_view.position {
                    FloatingPanePosition::Default => FloatingPanePositionReport::Default,
                    FloatingPanePosition::Moved(stored_cell) => {
                        FloatingPanePositionReport::Moved { stored_cell }
                    }
                    FloatingPanePosition::Pinned(stored_cell) => {
                        FloatingPanePositionReport::Pinned { stored_cell }
                    }
                },
                visibility: match client_view.visibility {
                    FloatingPaneVisibility::Drawn {
                        top_left_cell,
                        layer_index,
                    } => FloatingPaneVisibilityReport::Drawn {
                        top_left_cell,
                        layer_index,
                    },
                    FloatingPaneVisibility::Minimized => FloatingPaneVisibilityReport::Minimized,
                    FloatingPaneVisibility::NoFloatingViewport => {
                        FloatingPaneVisibilityReport::NoFloatingViewport
                    }
                },
            })
            .collect(),
    }
}

/// One [`ClientFocus`] as both formats print it.
fn build_client_focus_report(client_focus: &ClientFocus) -> ClientFocusReport {
    ClientFocusReport {
        client_id: client_focus.client_id.to_string(),
        active_tab_id: client_focus.active_tab_id.to_string(),
        focused_pane_id: client_focus
            .focused_pane_id
            .map(|focused_pane_id| focused_pane_id.to_string()),
    }
}

/// Append one client's solve of one tab: the client line, one line per pane
/// rectangle, the panes with no room, then the stack header strips.
fn render_solved_tab(rendered_output: &mut String, solved_tab_report: &SolvedTabReport) {
    let layout_mode_cell = match &solved_tab_report.layout_mode {
        LayoutModeReport::Tiled => "tiled".to_string(),
        LayoutModeReport::Fullscreen { fullscreen_pane_id } => {
            format!("fullscreen {fullscreen_pane_id}")
        }
    };
    rendered_output.push_str(&format!(
        "    client {} {layout_mode_cell} viewport {}\n",
        solved_tab_report.client_id,
        format_size_cell(solved_tab_report.viewport_size)
    ));
    for solved_pane_report in &solved_tab_report.pane_rects {
        rendered_output.push_str(&format!(
            "      pane {} rect {}\n",
            solved_pane_report.pane_id,
            format_rect_cell(solved_pane_report.outer_rect)
        ));
    }
    if !solved_tab_report.suppressed_pane_ids.is_empty() {
        rendered_output.push_str(&format!(
            "      no room: {}\n",
            solved_tab_report.suppressed_pane_ids.join(", ")
        ));
    }
    if solved_tab_report.is_every_pane_suppressed {
        rendered_output.push_str("      no room for any pane\n");
    }
    for stack_header_report in &solved_tab_report.stack_headers {
        rendered_output.push_str(&format!(
            "      stack header {} rect {} [{}/{}]\n",
            stack_header_report.pane_id,
            format_rect_cell(stack_header_report.header_rect),
            stack_header_report.member_index + 1,
            stack_header_report.member_count
        ));
    }
}

/// Append one floating pane: the pane line with the size it asks for and the
/// size it holds, then one line per client view with what the client draws
/// and the position it stored.
///
/// A view the client draws prints the pane's rectangle and its layer:
/// `client client-… rect 16,4 48x13 layer 0 position default`. A minimized view
/// prints `minimized`, and a view from a client with no floating viewport
/// prints `no floating viewport`: `client client-… minimized position pinned
/// 20,5`.
fn render_floating_pane(rendered_output: &mut String, floating_pane_report: &FloatingPaneReport) {
    rendered_output.push_str(&format!(
        "    pane {} asks {}x{} size {}\n",
        floating_pane_report.pane_id,
        floating_pane_report.desired_size.width,
        floating_pane_report.desired_size.height,
        format_size_cell(floating_pane_report.solved_size)
    ));
    for client_view_report in &floating_pane_report.client_views {
        let visibility_cell = match client_view_report.visibility {
            FloatingPaneVisibilityReport::Drawn {
                top_left_cell,
                layer_index,
            } => format!(
                "rect {} layer {layer_index}",
                format_rect_cell(Rect::from_origin_and_size(
                    top_left_cell,
                    floating_pane_report.solved_size
                ))
            ),
            FloatingPaneVisibilityReport::Minimized => "minimized".to_string(),
            FloatingPaneVisibilityReport::NoFloatingViewport => "no floating viewport".to_string(),
        };
        let position_cell = match client_view_report.position {
            FloatingPanePositionReport::Default => "default".to_string(),
            FloatingPanePositionReport::Moved { stored_cell } => {
                format!("moved {},{}", stored_cell.column, stored_cell.row)
            }
            FloatingPanePositionReport::Pinned { stored_cell } => {
                format!("pinned {},{}", stored_cell.column, stored_cell.row)
            }
        };
        rendered_output.push_str(&format!(
            "      client {} {visibility_cell} position {position_cell}\n",
            client_view_report.client_id
        ));
    }
}

/// Append `layout_node_report`'s line at `depth`, then the lines of
/// everything under it, one line each and two spaces per level.
///
/// A line is the node's label, then `, <size_weight>` when the node sits in a
/// split, then ` (collapsed)` when its stack member is collapsed:
/// `pane pane-…5, weight 1 (collapsed)`. A split whose weights do not number
/// one per child adds the two counts to its label: `horizontal split, 3
/// weights for 2 children`. A child with no weight prints none.
fn render_layout_tree(
    rendered_output: &mut String,
    layout_node_report: &LayoutNodeReport,
    depth: usize,
    size_weight: Option<&str>,
) {
    let (mut node_label, is_collapsed, child_weights, child_node_reports) = match layout_node_report
    {
        LayoutNodeReport::Pane {
            pane_id,
            is_collapsed,
        } => (format!("pane {pane_id}"), *is_collapsed, &[][..], &[][..]),
        LayoutNodeReport::Split {
            split_direction,
            active_child_index,
            weights,
            is_collapsed,
            children,
        } => {
            let mut split_label = format!("{split_direction} split");
            if let Some(active_child_index) = active_child_index {
                split_label.push_str(&format!(", active member {active_child_index}"));
            }
            if weights.len() != children.len() {
                split_label.push_str(&format!(
                    ", {} for {}",
                    format_counted_noun(weights.len(), "weight", "weights"),
                    format_counted_noun(children.len(), "child", "children")
                ));
            }
            (
                split_label,
                *is_collapsed,
                weights.as_slice(),
                children.as_slice(),
            )
        }
    };
    if let Some(size_weight) = size_weight {
        node_label.push_str(&format!(", {size_weight}"));
    }
    rendered_output.push_str(&"  ".repeat(depth));
    rendered_output.push_str(&node_label);
    if is_collapsed {
        rendered_output.push_str(" (collapsed)");
    }
    rendered_output.push('\n');

    for (child_index, child_node_report) in child_node_reports.iter().enumerate() {
        render_layout_tree(
            rendered_output,
            child_node_report,
            depth + 1,
            child_weights.get(child_index).map(String::as_str),
        );
    }
}

/// One rectangle as `x,y colsxrows`.
fn format_rect_cell(rectangle: Rect) -> String {
    let Rect { origin, size } = rectangle;
    format!(
        "{},{} {}",
        origin.column,
        origin.row,
        format_size_cell(size)
    )
}

/// How a split divides its area: `horizontal`, `vertical` or `stacked`.
fn format_split_direction(split_direction: SplitDirection) -> &'static str {
    match split_direction {
        SplitDirection::Horizontal => "horizontal",
        SplitDirection::Vertical => "vertical",
        SplitDirection::Stacked => "stacked",
    }
}

/// How one split child is sized, in the words a layout file uses: `weight 2`,
/// `size 30` for 30 cells, or `size 60%`, then ` min <cells>` and
/// ` preferred <cells>` when the child has that floor or target, then
/// ` resize <signed cells>` for a user resize such as `resize -3`. A floor or
/// a target that is the whole constraint prints alone: `min 10`,
/// `preferred 20`.
///
/// Example: `Flex(2)` with a 10-cell floor and a 5-cell resize prints
/// `weight 2 min 10 resize +5`.
fn format_size_weight(size_weight: SizeWeight) -> String {
    let mut size_weight_text = match size_weight.primary_constraint {
        SizeConstraint::Flex(flex_weight) => format!("weight {flex_weight}"),
        SizeConstraint::Fixed(cell_count) => format!("size {cell_count}"),
        SizeConstraint::Percent(axis_percent) => format!("size {axis_percent}%"),
        SizeConstraint::Minimum(cell_count) => format!("min {cell_count}"),
        SizeConstraint::Preferred(cell_count) => format!("preferred {cell_count}"),
    };
    if let Some(minimum_cell_count) = size_weight.minimum_cell_count {
        size_weight_text.push_str(&format!(" min {minimum_cell_count}"));
    }
    if let Some(preferred_cell_count) = size_weight.preferred_cell_count {
        size_weight_text.push_str(&format!(" preferred {preferred_cell_count}"));
    }
    if size_weight.resize_delta != 0 {
        size_weight_text.push_str(&format!(" resize {:+}", size_weight.resize_delta));
    }
    size_weight_text
}

/// One axis of a floating pane's desired size: `80` for 80 cells, `60%` for
/// 60 percent of the axis.
fn format_floating_pane_dimension_cell(floating_pane_dimension: FloatingPaneDimension) -> String {
    match floating_pane_dimension {
        FloatingPaneDimension::Cells(cell_count) => cell_count.to_string(),
        FloatingPaneDimension::Percent(axis_percent) => format!("{}%", axis_percent.get_percent()),
    }
}
