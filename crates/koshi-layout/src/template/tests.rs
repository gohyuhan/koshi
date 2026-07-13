//! Tests for layout templates: leaf ordering and instantiation into live
//! layout trees.

use std::path::Path;

use super::*;
use crate::size::SizeConstraint;

fn shell_leaf() -> TemplateNode {
    TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate::default()))
}

fn command_leaf(program: &str) -> TemplateNode {
    TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate {
        command: Some(CommandTemplate {
            program: PathBuf::from(program),
            args: Vec::new(),
        }),
        cwd: None,
        env: BTreeMap::new(),
    }))
}

fn plugin_leaf(name: &str) -> TemplateNode {
    TemplateNode::Leaf(LeafTemplate::Plugin(PluginTemplate {
        name: name.to_string(),
    }))
}

/// A horizontal split with a nested vertical split:
/// `horizontal(nvim, vertical(shell, plugin))`.
fn nested_template() -> TemplateNode {
    let inner = TemplateSplit {
        direction: SplitDirection::Vertical,
        children: vec![
            TemplateChild {
                node: shell_leaf(),
                collapsed: false,
            },
            TemplateChild {
                node: plugin_leaf("session-manager"),
                collapsed: false,
            },
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 0,
    };
    TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Horizontal,
        children: vec![
            TemplateChild {
                node: command_leaf("nvim"),
                collapsed: false,
            },
            TemplateChild {
                node: TemplateNode::Split(inner),
                collapsed: false,
            },
        ],
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
    let leaves = template.leaves();
    assert_eq!(leaves.len(), 3);
    assert!(matches!(
        leaves[0],
        LeafTemplate::Terminal(TerminalTemplate {
            command: Some(command),
            ..
        }) if command.program == Path::new("nvim")
    ));
    assert!(matches!(
        leaves[1],
        LeafTemplate::Terminal(TerminalTemplate { command: None, .. })
    ));
    assert!(matches!(
        leaves[2],
        LeafTemplate::Plugin(PluginTemplate { name }) if name == "session-manager"
    ));
}

#[test]
fn to_layout_node_mirrors_structure_weights_and_direction() {
    let template = nested_template();
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = template.to_layout_node(&[a, b, c]).unwrap();

    let expected = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            LayoutChild::new(LayoutNode::Pane(a)),
            LayoutChild::new(LayoutNode::Split(SplitNode {
                direction: SplitDirection::Vertical,
                children: vec![
                    LayoutChild::new(LayoutNode::Pane(b)),
                    LayoutChild::new(LayoutNode::Pane(c)),
                ],
                weights: vec![SizeWeight::default(), SizeWeight::default()],
                active: 0,
            })),
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
fn stacked_template_preserves_active_and_collapsed() {
    let template = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Stacked,
        children: vec![
            TemplateChild {
                node: command_leaf("htop"),
                collapsed: true,
            },
            TemplateChild {
                node: shell_leaf(),
                collapsed: false,
            },
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 1,
    });
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = template.to_layout_node(&[a, b]).unwrap();
    let expected = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutChild {
                node: LayoutNode::Pane(a),
                collapsed: true,
            },
            LayoutChild {
                node: LayoutNode::Pane(b),
                collapsed: false,
            },
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 1,
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
fn first_visible_leaf_skips_collapsed_stack_members() {
    // A directional split whose first child is a stack expanding its second
    // member: leaves are [stack member 0, stack member 1, trailing pane],
    // and the first VISIBLE one is the expanded member at index 1.
    let stack = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Stacked,
        children: vec![
            TemplateChild {
                node: shell_leaf(),
                collapsed: true,
            },
            TemplateChild {
                node: command_leaf("htop"),
                collapsed: false,
            },
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 1,
    });
    let root = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Horizontal,
        children: vec![
            TemplateChild {
                node: stack,
                collapsed: false,
            },
            TemplateChild {
                node: shell_leaf(),
                collapsed: false,
            },
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 0,
    });
    assert_eq!(root.first_visible_leaf(), 1);
}

#[test]
fn first_visible_leaf_of_an_empty_split_is_zero() {
    // Hand-built: a split with no children, representable directly though
    // no file parse or edit path produces it.
    let empty = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Horizontal,
        children: Vec::new(),
        weights: Vec::new(),
        active: 0,
    });
    assert_eq!(empty.first_visible_leaf(), 0);
}

#[test]
fn first_visible_leaf_with_out_of_range_active_falls_back_to_zero() {
    // Hand-built: a stacked template whose active index is out of bounds.
    // Unlike the solver, this walk does not clamp; an unreachable pick
    // index falls back to zero rather than panicking.
    let stack = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Stacked,
        children: vec![
            TemplateChild {
                node: shell_leaf(),
                collapsed: false,
            },
            TemplateChild {
                node: command_leaf("htop"),
                collapsed: true,
            },
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 9,
    });
    assert_eq!(stack.first_visible_leaf(), 0);
}

#[test]
fn empty_split_template_instantiates_with_no_ids() {
    let empty = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Horizontal,
        children: Vec::new(),
        weights: Vec::new(),
        active: 0,
    });
    let tree = empty.to_layout_node(&[]).unwrap();
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
