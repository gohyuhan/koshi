//! Tests for layout templates: leaf ordering and instantiation into live
//! layout trees.

use super::*;
use crate::size::SizeConstraint;

/// A terminal leaf running the default shell.
fn build_shell_template() -> LeafTemplate {
    LeafTemplate::Terminal(TerminalTemplate::default())
}

/// A terminal leaf running `program` with no arguments.
fn build_command_template(program: &str) -> LeafTemplate {
    LeafTemplate::Terminal(TerminalTemplate {
        command: Some(CommandTemplate {
            program: PathBuf::from(program),
            arguments: Vec::new(),
        }),
        working_directory: None,
        environment_variables: BTreeMap::new(),
    })
}

/// A plugin leaf named `plugin_name`.
fn build_plugin_template(plugin_name: &str) -> LeafTemplate {
    LeafTemplate::Plugin(PluginTemplate {
        plugin_name: plugin_name.to_string(),
    })
}

fn build_shell_leaf() -> TemplateNode {
    TemplateNode::Leaf(build_shell_template())
}

fn build_command_template_node(program: &str) -> TemplateNode {
    TemplateNode::Leaf(build_command_template(program))
}

fn build_plugin_template_node(plugin_name: &str) -> TemplateNode {
    TemplateNode::Leaf(build_plugin_template(plugin_name))
}

/// A split of `direction` with one default weight per child.
fn build_template_split(
    direction: SplitDirection,
    children: Vec<TemplateNode>,
    active_child_index: usize,
) -> TemplateNode {
    TemplateNode::Split(TemplateSplit {
        direction,
        weights: vec![SizeWeight::default(); children.len()],
        children,
        active_child_index,
    })
}

/// A horizontal split with no children.
fn build_empty_split() -> TemplateNode {
    build_template_split(SplitDirection::Horizontal, Vec::new(), 0)
}

/// A horizontal split with a nested vertical split:
/// `horizontal(nvim, vertical(shell, plugin))`, weighted 60/40.
fn build_nested_template() -> TemplateNode {
    let nested_vertical_template = build_template_split(
        SplitDirection::Vertical,
        vec![
            build_shell_leaf(),
            build_plugin_template_node("session-manager"),
        ],
        0,
    );
    TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Horizontal,
        children: vec![
            build_command_template_node("nvim"),
            nested_vertical_template,
        ],
        weights: vec![
            SizeWeight::from_primary_constraint(SizeConstraint::Percent(60)),
            SizeWeight::from_primary_constraint(SizeConstraint::Percent(40)),
        ],
        active_child_index: 0,
    })
}

#[test]
fn leaves_are_depth_first_in_layout_order() {
    let template = build_nested_template();
    let (nvim_template, default_shell_template, session_manager_template) = (
        build_command_template("nvim"),
        build_shell_template(),
        build_plugin_template("session-manager"),
    );
    assert_eq!(
        template.list_leaf_templates(),
        [
            &nvim_template,
            &default_shell_template,
            &session_manager_template
        ]
    );
}

#[test]
fn leaves_of_a_bare_leaf_is_that_leaf() {
    let default_shell = build_shell_template();
    assert_eq!(build_shell_leaf().list_leaf_templates(), [&default_shell]);
}

#[test]
fn leaves_of_an_empty_split_is_empty() {
    assert_eq!(
        build_empty_split().list_leaf_templates(),
        Vec::<&LeafTemplate>::new()
    );
}

#[test]
fn build_layout_node_mirrors_structure_weights_and_direction() {
    let template = build_nested_template();
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = template
        .build_layout_node(&[first_pane_id, second_pane_id, third_pane_id])
        .unwrap();

    let expected_layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            LayoutNode::Pane(first_pane_id),
            LayoutNode::Split(SplitNode {
                direction: SplitDirection::Vertical,
                children: vec![
                    LayoutNode::Pane(second_pane_id),
                    LayoutNode::Pane(third_pane_id),
                ],
                weights: vec![SizeWeight::default(), SizeWeight::default()],
                active_child_index: 0,
            }),
        ],
        weights: vec![
            SizeWeight::from_primary_constraint(SizeConstraint::Percent(60)),
            SizeWeight::from_primary_constraint(SizeConstraint::Percent(40)),
        ],
        active_child_index: 0,
    });
    assert_eq!(layout_tree, expected_layout_tree);
}

#[test]
fn build_layout_node_assigns_ids_in_leaf_order() {
    let template = build_nested_template();
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = template
        .build_layout_node(&[first_pane_id, second_pane_id, third_pane_id])
        .unwrap();
    assert_eq!(
        layout_tree.list_leaf_pane_ids(),
        vec![first_pane_id, second_pane_id, third_pane_id]
    );
}

#[test]
fn stacked_template_preserves_its_active_member() {
    let template = build_template_split(
        SplitDirection::Stacked,
        vec![build_command_template_node("htop"), build_shell_leaf()],
        1,
    );
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = template
        .build_layout_node(&[first_pane_id, second_pane_id])
        .unwrap();
    let expected_layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutNode::Pane(first_pane_id),
            LayoutNode::Pane(second_pane_id),
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active_child_index: 1,
    });
    assert_eq!(layout_tree, expected_layout_tree);
}

#[test]
fn build_layout_node_copies_an_out_of_range_active_unchanged() {
    let template = build_template_split(
        SplitDirection::Stacked,
        vec![build_shell_leaf(), build_command_template_node("htop")],
        9,
    );
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = template
        .build_layout_node(&[first_pane_id, second_pane_id])
        .unwrap();
    let expected_layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutNode::Pane(first_pane_id),
            LayoutNode::Pane(second_pane_id),
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active_child_index: 9,
    });
    assert_eq!(layout_tree, expected_layout_tree);
}

#[test]
fn single_leaf_template_instantiates_to_bare_pane() {
    let template = build_shell_leaf();
    let pane_id = PaneId::new();
    let tree = template.build_layout_node(&[pane_id]).unwrap();
    assert_eq!(tree, LayoutNode::Pane(pane_id));
}

#[test]
fn first_visible_leaf_of_a_leaf_is_zero() {
    assert_eq!(build_shell_leaf().find_first_visible_leaf_index(), 0);
}

#[test]
fn first_visible_leaf_of_a_directional_split_is_its_first_leaf() {
    assert_eq!(build_nested_template().find_first_visible_leaf_index(), 0);
}

#[test]
fn first_visible_leaf_ignores_active_on_a_directional_split() {
    let root = build_template_split(
        SplitDirection::Horizontal,
        vec![build_shell_leaf(), build_command_template_node("htop")],
        1,
    );
    assert_eq!(root.find_first_visible_leaf_index(), 0);
}

#[test]
fn first_visible_leaf_skips_collapsed_stack_members() {
    // A directional split whose first child is a stack expanding its second
    // member: leaves are [stack member 0, stack member 1, trailing pane],
    // and the first VISIBLE one is the expanded member at index 1.
    let stack_template = build_template_split(
        SplitDirection::Stacked,
        vec![build_shell_leaf(), build_command_template_node("htop")],
        1,
    );
    let root_template = build_template_split(
        SplitDirection::Horizontal,
        vec![stack_template, build_shell_leaf()],
        0,
    );
    assert_eq!(root_template.find_first_visible_leaf_index(), 1);
}

#[test]
fn first_visible_leaf_counts_every_leaf_of_earlier_stack_members() {
    // stack(vertical(shell, htop) collapsed, plugin expanded): the expanded
    // member comes after the two leaves of the collapsed member.
    let collapsed_member_template = build_template_split(
        SplitDirection::Vertical,
        vec![build_shell_leaf(), build_command_template_node("htop")],
        0,
    );
    let stack_template = build_template_split(
        SplitDirection::Stacked,
        vec![
            collapsed_member_template,
            build_plugin_template_node("session-manager"),
        ],
        1,
    );
    assert_eq!(stack_template.find_first_visible_leaf_index(), 2);
}

#[test]
fn first_visible_leaf_descends_into_a_nested_stack() {
    // stack(shell collapsed, stack(htop collapsed, plugin expanded) expanded):
    // leaves are [shell, htop, plugin] and the visible one is plugin.
    let inner_stack_template = build_template_split(
        SplitDirection::Stacked,
        vec![
            build_command_template_node("htop"),
            build_plugin_template_node("session-manager"),
        ],
        1,
    );
    let outer_stack_template = build_template_split(
        SplitDirection::Stacked,
        vec![build_shell_leaf(), inner_stack_template],
        1,
    );
    assert_eq!(outer_stack_template.find_first_visible_leaf_index(), 2);
}

#[test]
fn first_visible_leaf_of_an_empty_split_is_zero() {
    assert_eq!(build_empty_split().find_first_visible_leaf_index(), 0);
}

#[test]
fn first_visible_leaf_with_out_of_range_active_names_the_last_member() {
    // A stacked template whose active index is past its last member: the walk
    // clamps it to the last child, the same member the solver expands once the
    // template is instantiated.
    let stack_template = build_template_split(
        SplitDirection::Stacked,
        vec![build_shell_leaf(), build_command_template_node("htop")],
        9,
    );
    assert_eq!(stack_template.find_first_visible_leaf_index(), 1);
}

#[test]
fn empty_split_template_instantiates_with_no_ids() {
    let layout_tree = build_empty_split().build_layout_node(&[]).unwrap();
    assert_eq!(
        layout_tree,
        LayoutNode::Split(SplitNode {
            direction: SplitDirection::Horizontal,
            children: Vec::new(),
            weights: Vec::new(),
            active_child_index: 0,
        })
    );
}

#[test]
fn an_empty_split_child_consumes_no_ids() {
    let template = build_template_split(
        SplitDirection::Horizontal,
        vec![
            build_shell_leaf(),
            build_empty_split(),
            build_plugin_template_node("session-manager"),
        ],
        0,
    );
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = template
        .build_layout_node(&[first_pane_id, second_pane_id])
        .unwrap();
    let expected_layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            LayoutNode::Pane(first_pane_id),
            LayoutNode::Split(SplitNode {
                direction: SplitDirection::Horizontal,
                children: Vec::new(),
                weights: Vec::new(),
                active_child_index: 0,
            }),
            LayoutNode::Pane(second_pane_id),
        ],
        weights: vec![SizeWeight::default(); 3],
        active_child_index: 0,
    });
    assert_eq!(layout_tree, expected_layout_tree);
}

#[test]
fn too_few_ids_is_a_count_mismatch() {
    let template = build_nested_template();
    let template_count_error = template.build_layout_node(&[PaneId::new()]).unwrap_err();
    assert_eq!(
        template_count_error,
        TemplateError::PaneCountMismatch {
            expected_leaf_count: 3,
            provided_pane_id_count: 1
        }
    );
}

#[test]
fn no_ids_for_a_leaf_is_a_count_mismatch() {
    let template_count_error = build_shell_leaf().build_layout_node(&[]).unwrap_err();
    assert_eq!(
        template_count_error,
        TemplateError::PaneCountMismatch {
            expected_leaf_count: 1,
            provided_pane_id_count: 0
        }
    );
}

#[test]
fn too_many_ids_is_a_count_mismatch() {
    let template = build_shell_leaf();
    let template_count_error = template
        .build_layout_node(&[PaneId::new(), PaneId::new()])
        .unwrap_err();
    assert_eq!(
        template_count_error,
        TemplateError::PaneCountMismatch {
            expected_leaf_count: 1,
            provided_pane_id_count: 2
        }
    );
}

#[test]
fn a_count_mismatch_names_both_counts() {
    let template_count_error = build_nested_template()
        .build_layout_node(&[PaneId::new()])
        .unwrap_err();
    assert_eq!(
        template_count_error.to_string(),
        "template has 3 pane slots but 1 pane ids were supplied"
    );
}
