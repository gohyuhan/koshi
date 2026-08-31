//! Tests for layout templates: leaf ordering and instantiation into live
//! layout trees.

use super::*;
use crate::size::SizeConstraint;

/// A terminal leaf running the default shell.
fn shell() -> LeafTemplate {
    LeafTemplate::Terminal(TerminalTemplate::default())
}

/// A terminal leaf running `program` with no arguments.
fn command(program: &str) -> LeafTemplate {
    LeafTemplate::Terminal(TerminalTemplate {
        command: Some(CommandTemplate {
            program: PathBuf::from(program),
            args: Vec::new(),
        }),
        cwd: None,
        env: BTreeMap::new(),
    })
}

/// A plugin leaf named `name`.
fn plugin(name: &str) -> LeafTemplate {
    LeafTemplate::Plugin(PluginTemplate {
        name: name.to_string(),
    })
}

fn shell_leaf() -> TemplateNode {
    TemplateNode::Leaf(shell())
}

fn command_leaf(program: &str) -> TemplateNode {
    TemplateNode::Leaf(command(program))
}

fn plugin_leaf(name: &str) -> TemplateNode {
    TemplateNode::Leaf(plugin(name))
}

/// A split of `direction` with one default weight per child.
fn split(direction: SplitDirection, children: Vec<TemplateNode>, active: usize) -> TemplateNode {
    TemplateNode::Split(TemplateSplit {
        direction,
        weights: vec![SizeWeight::default(); children.len()],
        children,
        active,
    })
}

/// A horizontal split with no children.
fn empty_split() -> TemplateNode {
    split(SplitDirection::Horizontal, Vec::new(), 0)
}

/// A horizontal split with a nested vertical split:
/// `horizontal(nvim, vertical(shell, plugin))`, weighted 60/40.
fn nested_template() -> TemplateNode {
    let inner = split(
        SplitDirection::Vertical,
        vec![shell_leaf(), plugin_leaf("session-manager")],
        0,
    );
    TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Horizontal,
        children: vec![command_leaf("nvim"), inner],
        weights: vec![
            SizeWeight::new(SizeConstraint::Percent(60)),
            SizeWeight::new(SizeConstraint::Percent(40)),
        ],
        active: 0,
    })
}

#[test]
fn leaves_are_depth_first_in_layout_order() {
    let template = nested_template();
    let (nvim, default_shell, session_manager) =
        (command("nvim"), shell(), plugin("session-manager"));
    assert_eq!(template.leaves(), [&nvim, &default_shell, &session_manager]);
}

#[test]
fn leaves_of_a_bare_leaf_is_that_leaf() {
    let default_shell = shell();
    assert_eq!(shell_leaf().leaves(), [&default_shell]);
}

#[test]
fn leaves_of_an_empty_split_is_empty() {
    assert_eq!(empty_split().leaves(), Vec::<&LeafTemplate>::new());
}

#[test]
fn to_layout_node_mirrors_structure_weights_and_direction() {
    let template = nested_template();
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = template.to_layout_node(&[a, b, c]).unwrap();

    let expected = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            LayoutNode::Pane(a),
            LayoutNode::Split(SplitNode {
                direction: SplitDirection::Vertical,
                children: vec![LayoutNode::Pane(b), LayoutNode::Pane(c)],
                weights: vec![SizeWeight::default(), SizeWeight::default()],
                active: 0,
            }),
        ],
        weights: vec![
            SizeWeight::new(SizeConstraint::Percent(60)),
            SizeWeight::new(SizeConstraint::Percent(40)),
        ],
        active: 0,
    });
    assert_eq!(tree, expected);
}

#[test]
fn to_layout_node_assigns_ids_in_leaf_order() {
    let template = nested_template();
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = template.to_layout_node(&[a, b, c]).unwrap();
    assert_eq!(tree.leaf_panes(), vec![a, b, c]);
}

#[test]
fn stacked_template_preserves_its_active_member() {
    let template = split(
        SplitDirection::Stacked,
        vec![command_leaf("htop"), shell_leaf()],
        1,
    );
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = template.to_layout_node(&[a, b]).unwrap();
    let expected = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![LayoutNode::Pane(a), LayoutNode::Pane(b)],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 1,
    });
    assert_eq!(tree, expected);
}

#[test]
fn to_layout_node_copies_an_out_of_range_active_unchanged() {
    let template = split(
        SplitDirection::Stacked,
        vec![shell_leaf(), command_leaf("htop")],
        9,
    );
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = template.to_layout_node(&[a, b]).unwrap();
    let expected = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![LayoutNode::Pane(a), LayoutNode::Pane(b)],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 9,
    });
    assert_eq!(tree, expected);
}

#[test]
fn single_leaf_template_instantiates_to_bare_pane() {
    let template = shell_leaf();
    let id = PaneId::new();
    let tree = template.to_layout_node(&[id]).unwrap();
    assert_eq!(tree, LayoutNode::Pane(id));
}

#[test]
fn first_visible_leaf_of_a_leaf_is_zero() {
    assert_eq!(shell_leaf().first_visible_leaf(), 0);
}

#[test]
fn first_visible_leaf_of_a_directional_split_is_its_first_leaf() {
    assert_eq!(nested_template().first_visible_leaf(), 0);
}

#[test]
fn first_visible_leaf_ignores_active_on_a_directional_split() {
    let root = split(
        SplitDirection::Horizontal,
        vec![shell_leaf(), command_leaf("htop")],
        1,
    );
    assert_eq!(root.first_visible_leaf(), 0);
}

#[test]
fn first_visible_leaf_skips_collapsed_stack_members() {
    // A directional split whose first child is a stack expanding its second
    // member: leaves are [stack member 0, stack member 1, trailing pane],
    // and the first VISIBLE one is the expanded member at index 1.
    let stack = split(
        SplitDirection::Stacked,
        vec![shell_leaf(), command_leaf("htop")],
        1,
    );
    let root = split(SplitDirection::Horizontal, vec![stack, shell_leaf()], 0);
    assert_eq!(root.first_visible_leaf(), 1);
}

#[test]
fn first_visible_leaf_counts_every_leaf_of_earlier_stack_members() {
    // stack(vertical(shell, htop) collapsed, plugin expanded): the expanded
    // member comes after the two leaves of the collapsed member.
    let pair = split(
        SplitDirection::Vertical,
        vec![shell_leaf(), command_leaf("htop")],
        0,
    );
    let stack = split(
        SplitDirection::Stacked,
        vec![pair, plugin_leaf("session-manager")],
        1,
    );
    assert_eq!(stack.first_visible_leaf(), 2);
}

#[test]
fn first_visible_leaf_descends_into_a_nested_stack() {
    // stack(shell collapsed, stack(htop collapsed, plugin expanded) expanded):
    // leaves are [shell, htop, plugin] and the visible one is plugin.
    let inner = split(
        SplitDirection::Stacked,
        vec![command_leaf("htop"), plugin_leaf("session-manager")],
        1,
    );
    let outer = split(SplitDirection::Stacked, vec![shell_leaf(), inner], 1);
    assert_eq!(outer.first_visible_leaf(), 2);
}

#[test]
fn first_visible_leaf_of_an_empty_split_is_zero() {
    assert_eq!(empty_split().first_visible_leaf(), 0);
}

#[test]
fn first_visible_leaf_with_out_of_range_active_names_the_last_member() {
    // A stacked template whose active index is past its last member: the walk
    // clamps it to the last child, the same member the solver expands once the
    // template is instantiated.
    let stack = split(
        SplitDirection::Stacked,
        vec![shell_leaf(), command_leaf("htop")],
        9,
    );
    assert_eq!(stack.first_visible_leaf(), 1);
}

#[test]
fn empty_split_template_instantiates_with_no_ids() {
    let tree = empty_split().to_layout_node(&[]).unwrap();
    assert_eq!(
        tree,
        LayoutNode::Split(SplitNode {
            direction: SplitDirection::Horizontal,
            children: Vec::new(),
            weights: Vec::new(),
            active: 0,
        })
    );
}

#[test]
fn an_empty_split_child_consumes_no_ids() {
    let template = split(
        SplitDirection::Horizontal,
        vec![shell_leaf(), empty_split(), plugin_leaf("session-manager")],
        0,
    );
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = template.to_layout_node(&[a, b]).unwrap();
    let expected = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            LayoutNode::Pane(a),
            LayoutNode::Split(SplitNode {
                direction: SplitDirection::Horizontal,
                children: Vec::new(),
                weights: Vec::new(),
                active: 0,
            }),
            LayoutNode::Pane(b),
        ],
        weights: vec![SizeWeight::default(); 3],
        active: 0,
    });
    assert_eq!(tree, expected);
}

#[test]
fn too_few_ids_is_a_count_mismatch() {
    let template = nested_template();
    let err = template.to_layout_node(&[PaneId::new()]).unwrap_err();
    assert_eq!(
        err,
        TemplateError::PaneCountMismatch {
            expected: 3,
            got: 1
        }
    );
}

#[test]
fn no_ids_for_a_leaf_is_a_count_mismatch() {
    let err = shell_leaf().to_layout_node(&[]).unwrap_err();
    assert_eq!(
        err,
        TemplateError::PaneCountMismatch {
            expected: 1,
            got: 0
        }
    );
}

#[test]
fn too_many_ids_is_a_count_mismatch() {
    let template = shell_leaf();
    let err = template
        .to_layout_node(&[PaneId::new(), PaneId::new()])
        .unwrap_err();
    assert_eq!(
        err,
        TemplateError::PaneCountMismatch {
            expected: 1,
            got: 2
        }
    );
}

#[test]
fn a_count_mismatch_names_both_counts() {
    let err = nested_template()
        .to_layout_node(&[PaneId::new()])
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "template has 3 pane slots but 1 pane ids were supplied"
    );
}
