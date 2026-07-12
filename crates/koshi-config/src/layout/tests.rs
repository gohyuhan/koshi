//! Tests for layout file parsing: the full schema on valid files, and one
//! diagnostic per violation on invalid ones.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use koshi_core::geometry::SplitDirection;
use koshi_layout::size::{SizeConstraint, SizeWeight};
use koshi_layout::template::{
    CommandTemplate, LayoutTemplate, LeafTemplate, PluginTemplate, TabTemplate, TemplateChild,
    TemplateNode, TemplateSplit, TerminalTemplate,
};

use super::*;

fn parse(source: &str) -> Result<LayoutTemplate, LayoutError> {
    parse_layout(Path::new("layouts/dev.kdl"), source)
}

/// The diagnostics of an `Invalid` outcome, as their messages.
fn messages(source: &str) -> Vec<String> {
    match parse(source) {
        Err(LayoutError::Invalid { diagnostics, .. }) => diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message().to_string())
            .collect(),
        Err(LayoutError::Syntax(_)) => panic!("expected schema diagnostics, got syntax error"),
        Ok(_) => panic!("expected schema diagnostics, got a template"),
    }
}

fn shell_leaf() -> TemplateNode {
    TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate::default()))
}

fn flex() -> SizeWeight {
    SizeWeight::default()
}

// ---------------------------------------------------------------- valid files

#[test]
fn minimal_layout_is_one_shell_tab() {
    let template = parse("version 1\ntab { pane }").unwrap();
    assert_eq!(
        template,
        LayoutTemplate {
            tabs: vec![TabTemplate {
                root: shell_leaf(),
                focused_leaf: 0,
            }],
            focused_tab: 0,
        }
    );
}

#[test]
fn nested_layout_parses_every_config_kind() {
    let source = r#"
version 1

tab {
    horizontal {
        pane {
            command "nvim" "+42" "src/main.rs"
            cwd "~/proj"
            env "RUST_LOG" "debug"
            env "NO_COLOR" "1"
            size "60%"
            focus
        }
        vertical {
            size "40%"
            pane {
                min 5
                preferred 20
            }
            stack {
                weight 2
                pane {
                    command "htop"
                    expanded
                }
                plugin "session-manager"
            }
        }
    }
}
"#;
    let template = parse(source).unwrap();

    let editor = TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate {
        command: Some(CommandTemplate {
            program: PathBuf::from("nvim"),
            args: vec!["+42".to_string(), "src/main.rs".to_string()],
        }),
        cwd: Some(PathBuf::from("~/proj")),
        env: BTreeMap::from([
            ("RUST_LOG".to_string(), "debug".to_string()),
            ("NO_COLOR".to_string(), "1".to_string()),
        ]),
    }));
    let monitor = TemplateNode::Leaf(LeafTemplate::Terminal(TerminalTemplate {
        command: Some(CommandTemplate {
            program: PathBuf::from("htop"),
            args: Vec::new(),
        }),
        cwd: None,
        env: BTreeMap::new(),
    }));
    let stack = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Stacked,
        children: vec![
            TemplateChild {
                node: monitor,
                collapsed: false,
            },
            TemplateChild {
                node: TemplateNode::Leaf(LeafTemplate::Plugin(PluginTemplate {
                    name: "session-manager".to_string(),
                })),
                collapsed: true,
            },
        ],
        weights: vec![flex(), flex()],
        active: 0,
    });
    let right = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Vertical,
        children: vec![
            TemplateChild {
                node: shell_leaf(),
                collapsed: false,
            },
            TemplateChild {
                node: stack,
                collapsed: false,
            },
        ],
        weights: vec![
            SizeWeight {
                primary: SizeConstraint::Flex(1),
                min: Some(5),
                preferred: Some(20),
                resize_delta: 0,
            },
            SizeWeight::new(SizeConstraint::Flex(2)),
        ],
        active: 0,
    });
    let expected = LayoutTemplate {
        tabs: vec![TabTemplate {
            root: TemplateNode::Split(TemplateSplit {
                direction: SplitDirection::Horizontal,
                children: vec![
                    TemplateChild {
                        node: editor,
                        collapsed: false,
                    },
                    TemplateChild {
                        node: right,
                        collapsed: false,
                    },
                ],
                weights: vec![
                    SizeWeight::new(SizeConstraint::Percent(60)),
                    SizeWeight::new(SizeConstraint::Percent(40)),
                ],
                active: 0,
            }),
            focused_leaf: 0,
        }],
        focused_tab: 0,
    };
    assert_eq!(template, expected);
}

#[test]
fn multiple_tabs_with_tab_focus_and_per_tab_pane_focus() {
    let source = r#"
version 1
tab {
    horizontal {
        pane
        pane { focus }
    }
}
tab {
    focus
    pane { command "htop" }
}
"#;
    let template = parse(source).unwrap();
    assert_eq!(template.tabs.len(), 2);
    assert_eq!(template.focused_tab, 1);
    assert_eq!(template.tabs[0].focused_leaf, 1);
    assert_eq!(template.tabs[1].focused_leaf, 0);
}

#[test]
fn fixed_cell_size_parses_as_fixed_constraint() {
    let template = parse("version 1\ntab { horizontal { pane { size 30 }; pane } }").unwrap();
    let TemplateNode::Split(split) = &template.tabs[0].root else {
        panic!("expected split root");
    };
    assert_eq!(
        split.weights,
        vec![SizeWeight::new(SizeConstraint::Fixed(30)), flex()]
    );
}

#[test]
fn stack_defaults_to_first_member_expanded() {
    let template = parse("version 1\ntab { stack { pane; pane; pane } }").unwrap();
    let TemplateNode::Split(split) = &template.tabs[0].root else {
        panic!("expected stack root");
    };
    assert_eq!(split.direction, SplitDirection::Stacked);
    assert_eq!(split.active, 0);
    let collapsed: Vec<bool> = split.children.iter().map(|child| child.collapsed).collect();
    assert_eq!(collapsed, [false, true, true]);
}

#[test]
fn expanded_member_becomes_active_and_may_hold_focus() {
    let source = r#"
version 1
tab {
    stack {
        pane
        pane { command "htop"; expanded; focus }
    }
}
"#;
    let template = parse(source).unwrap();
    let TemplateNode::Split(split) = &template.tabs[0].root else {
        panic!("expected stack root");
    };
    assert_eq!(split.active, 1);
    assert_eq!(template.tabs[0].focused_leaf, 1);
}

#[test]
fn default_focus_skips_collapsed_stack_members() {
    // No `focus` marker anywhere: initial focus must land on the expanded
    // member (leaf 1), never the collapsed leaf 0.
    let template = parse("version 1\ntab { stack { pane; pane { expanded } } }").unwrap();
    assert_eq!(template.tabs[0].focused_leaf, 1);
}

#[test]
fn default_focus_descends_into_a_nested_stack() {
    // First child of the horizontal split is a stack expanding its second
    // member: the visible pane is leaf 1, so default focus is 1.
    let source = r#"
version 1
tab {
    horizontal {
        stack {
            pane
            pane { expanded }
        }
        pane
    }
}
"#;
    let template = parse(source).unwrap();
    assert_eq!(template.tabs[0].focused_leaf, 1);
}

#[test]
fn focus_on_first_stack_member_without_expanded_is_allowed() {
    // The first member is the default expanded one, so focusing it is
    // consistent without an explicit `expanded`.
    let template = parse("version 1\ntab { stack { pane { focus }; pane } }").unwrap();
    assert_eq!(template.tabs[0].focused_leaf, 0);
}

#[test]
fn older_version_is_accepted() {
    assert!(parse("version 0\ntab { pane }").is_ok());
}

#[test]
fn plugin_carries_sizing_and_focus_in_a_split() {
    let source = r#"
version 1
tab {
    horizontal {
        pane
        plugin "session-manager" {
            size "30%"
            focus
        }
    }
}
"#;
    let template = parse(source).unwrap();
    let TemplateNode::Split(split) = &template.tabs[0].root else {
        panic!("expected split root");
    };
    assert_eq!(
        split.weights,
        vec![flex(), SizeWeight::new(SizeConstraint::Percent(30))]
    );
    assert_eq!(template.tabs[0].focused_leaf, 1);
}

#[test]
fn expanded_plugin_member_becomes_the_active_one() {
    let source = r#"
version 1
tab {
    stack {
        pane
        plugin "session-manager" { expanded }
    }
}
"#;
    let template = parse(source).unwrap();
    let TemplateNode::Split(split) = &template.tabs[0].root else {
        panic!("expected stack root");
    };
    assert_eq!(split.active, 1);
    assert!(matches!(
        &split.children[1].node,
        TemplateNode::Leaf(LeafTemplate::Plugin(PluginTemplate { name }))
            if name == "session-manager"
    ));
}

// -------------------------------------------------------------- invalid files

#[test]
fn syntax_error_is_the_syntax_variant() {
    let err = parse("tab {").unwrap_err();
    assert!(matches!(err, LayoutError::Syntax(_)));
}

#[test]
fn invalid_report_names_the_file() {
    let err = parse("version 1").unwrap_err();
    assert_eq!(err.to_string(), "invalid layout file layouts/dev.kdl");
}

#[test]
fn missing_version_is_reported() {
    assert_eq!(
        messages("tab { pane }"),
        ["layout file must declare `version`"]
    );
}

#[test]
fn newer_version_is_reported() {
    assert_eq!(
        messages("version 999\ntab { pane }"),
        ["config schema version 999 is newer than this koshi supports (1)"]
    );
}

#[test]
fn duplicate_version_is_reported() {
    assert_eq!(
        messages("version 1\nversion 1\ntab { pane }"),
        ["`version` is declared more than once"]
    );
}

#[test]
fn non_integer_version_is_reported() {
    assert_eq!(
        messages("version \"one\"\ntab { pane }"),
        ["`version` must be a non-negative integer"]
    );
}

#[test]
fn version_with_children_is_reported() {
    assert_eq!(
        messages("version 1 { }\ntab { pane }"),
        ["`version` takes no children"]
    );
}

#[test]
fn version_as_property_is_reported() {
    assert_eq!(
        messages("version v=1\ntab { pane }"),
        ["`version` takes exactly one integer argument"]
    );
}

#[test]
fn missing_tabs_is_reported() {
    assert_eq!(
        messages("version 1"),
        ["layout file must define at least one `tab`"]
    );
}

#[test]
fn unknown_top_level_node_is_reported() {
    assert_eq!(
        messages("version 1\npane\ntab { pane }"),
        ["unknown node `pane`; a layout holds `version` and `tab` nodes"]
    );
}

#[test]
fn empty_tab_is_reported() {
    assert_eq!(
        messages("version 1\ntab { }"),
        ["`tab` needs one layout node (`pane`, `plugin`, `horizontal`, `vertical`, or `stack`)"]
    );
}

#[test]
fn two_tab_roots_are_reported() {
    assert_eq!(
        messages("version 1\ntab { pane; pane }"),
        [
            "`tab` holds one root node; wrap multiple panes in `horizontal`, `vertical`, or \
             `stack`"
        ]
    );
}

#[test]
fn tab_arguments_are_reported() {
    assert_eq!(
        messages("version 1\ntab \"main\" { pane }"),
        ["`tab` takes no arguments or properties; its layout goes in the children block"]
    );
}

#[test]
fn two_focused_tabs_are_reported() {
    assert_eq!(
        messages("version 1\ntab { focus; pane }\ntab { focus; pane }"),
        ["another tab already carries `focus`; only one tab starts focused"]
    );
}

#[test]
fn two_focused_panes_in_one_tab_are_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { focus }; pane { focus } } }"),
        ["this tab already focuses another pane"]
    );
}

#[test]
fn duplicate_tab_focus_marker_is_reported() {
    assert_eq!(
        messages("version 1\ntab { focus; focus; pane }"),
        ["`focus` is declared more than once"]
    );
}

#[test]
fn duplicate_pane_focus_marker_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { focus; focus } }"),
        ["`focus` is declared more than once"]
    );
}

#[test]
fn unknown_node_in_tab_is_reported() {
    assert_eq!(
        messages("version 1\ntab { theme \"dark\"; pane }"),
        [
            "unknown node `theme` in `tab`; expected `focus` or a layout node (`pane`, \
             `plugin`, `horizontal`, `vertical`, `stack`)"
        ]
    );
}

#[test]
fn pane_properties_are_reported() {
    assert_eq!(
        messages("version 1\ntab { pane command=\"nvim\" }"),
        [
            "`pane` takes no arguments or properties; its configuration goes in the children \
             block"
        ]
    );
}

#[test]
fn split_with_one_child_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane } }"),
        ["`horizontal` needs at least two children to divide space between"]
    );
}

#[test]
fn stack_with_one_member_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane } }"),
        ["`stack` needs at least two members"]
    );
}

#[test]
fn split_arguments_are_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal \"wide\" { pane; pane } }"),
        ["`horizontal` takes no arguments or properties"]
    );
}

#[test]
fn stack_arguments_are_reported() {
    assert_eq!(
        messages("version 1\ntab { stack active=1 { pane; pane } }"),
        ["`stack` takes no arguments or properties"]
    );
}

#[test]
fn unknown_node_in_split_is_reported() {
    assert_eq!(
        messages("version 1\ntab { vertical { border 2; pane; pane } }"),
        [
            "unknown node `border` in `vertical`; expected a layout node (`pane`, `plugin`, \
             `horizontal`, `vertical`, `stack`) or sizing (`size`, `weight`, `min`, \
             `preferred`)"
        ]
    );
}

#[test]
fn unknown_node_in_stack_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { focus; pane; pane } }"),
        [
            "unknown node `focus` in `stack`; expected `pane`, `plugin`, or sizing (`size`, \
             `weight`, `min`, `preferred`)"
        ]
    );
}

#[test]
fn split_inside_stack_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane; vertical { pane; pane } } }"),
        ["`vertical` cannot be a stack member; stack members are `pane` or `plugin`"]
    );
}

#[test]
fn focus_inside_an_invalid_stack_member_adds_no_extra_diagnostic() {
    // The invalid `vertical` member is the one and only problem; the focused
    // pane inside it must not also be judged as a collapsed-member focus.
    assert_eq!(
        messages(
            "version 1\ntab { stack { pane { expanded }; vertical { pane { focus }; pane } } }"
        ),
        ["`vertical` cannot be a stack member; stack members are `pane` or `plugin`"]
    );
}

#[test]
fn two_expanded_members_are_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane { expanded }; pane { expanded } } }"),
        ["another member is already `expanded`; a stack expands exactly one"]
    );
}

#[test]
fn duplicate_expanded_on_one_member_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane { expanded; expanded }; pane } }"),
        ["`expanded` is declared more than once"]
    );
}

#[test]
fn expanded_outside_stack_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { expanded }; pane } }"),
        ["`expanded` applies only to members of a `stack`"]
    );
}

#[test]
fn focus_on_collapsed_stack_member_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane; pane { focus } } }"),
        ["a collapsed stack member cannot hold focus; mark it `expanded`"]
    );
}

#[test]
fn sizing_on_stack_member_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane { size 30 }; pane } }"),
        ["sizing applies only to children of `horizontal` or `vertical`"]
    );
}

#[test]
fn sizing_on_tab_root_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { size 30 } }"),
        ["sizing applies only to children of `horizontal` or `vertical`"]
    );
}

#[test]
fn size_and_weight_together_are_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size 30; weight 2 }; pane } }"),
        ["this node already has `size` or `weight`; give one of the two, once"]
    );
}

#[test]
fn bad_size_string_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size \"wide\" }; pane } }"),
        ["`size` is a cell count like `size 30` or a percentage like `size \"60%\"`"]
    );
}

#[test]
fn zero_percent_size_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size \"0%\" }; pane } }"),
        ["percent must be between 1 and 100, got 0"]
    );
}

#[test]
fn over_hundred_percent_size_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size \"101%\" }; pane } }"),
        ["percent must be between 1 and 100, got 101"]
    );
}

#[test]
fn zero_cell_size_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size 0 }; pane } }"),
        ["fixed size must be at least one cell"]
    );
}

#[test]
fn oversized_cell_size_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size 70000 }; pane } }"),
        ["`size` cells must fit 1-65535"]
    );
}

#[test]
fn zero_weight_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { weight 0 }; pane } }"),
        ["flex weight must be at least 1"]
    );
}

#[test]
fn zero_min_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { min 0 }; pane } }"),
        ["`min` must be at least one cell"]
    );
}

#[test]
fn zero_preferred_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { preferred 0 }; pane } }"),
        ["`preferred` must be at least one cell"]
    );
}

#[test]
fn duplicate_min_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { min 5; min 6 }; pane } }"),
        ["`min` is declared more than once"]
    );
}

#[test]
fn duplicate_preferred_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { preferred 5; preferred 6 }; pane } }"),
        ["`preferred` is declared more than once"]
    );
}

#[test]
fn negative_min_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { min -3 }; pane } }"),
        ["`min` must be an integer between 1 and 65535"]
    );
}

#[test]
fn weight_arity_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { weight 1 2 }; pane } }"),
        ["`weight` takes exactly one value"]
    );
}

#[test]
fn size_with_children_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size 30 { } }; pane } }"),
        ["`size` takes no children"]
    );
}

#[test]
fn boolean_size_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size #true }; pane } }"),
        ["`size` is a cell count like `size 30` or a percentage like `size \"60%\"`"]
    );
}

#[test]
fn command_without_program_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { command } }"),
        ["`command` names a program, like `command \"nvim\" \"file.txt\"`"]
    );
}

#[test]
fn non_string_command_argument_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { command \"nvim\" 42 } }"),
        ["`command` arguments must be strings"]
    );
}

#[test]
fn duplicate_command_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { command \"a\"; command \"b\" } }"),
        ["`command` is declared more than once"]
    );
}

#[test]
fn command_property_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { command \"nvim\" wait=#true } }"),
        ["`command` takes arguments, not properties"]
    );
}

#[test]
fn command_with_children_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { command \"nvim\" { } } }"),
        ["`command` takes no children"]
    );
}

#[test]
fn env_arity_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { env \"RUST_LOG\" } }"),
        ["`env` takes a name and a value, both strings, like `env \"RUST_LOG\" \"debug\"`"]
    );
}

#[test]
fn duplicate_env_name_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { env \"A\" \"1\"; env \"A\" \"2\" } }"),
        ["`env` sets `A` more than once"]
    );
}

#[test]
fn env_with_children_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { env \"A\" \"1\" { } } }"),
        ["`env` takes no children"]
    );
}

#[test]
fn empty_env_name_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { env \"\" \"1\" } }"),
        ["`env` name must not be empty"]
    );
}

#[test]
fn cwd_arity_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { cwd \"a\" \"b\" } }"),
        ["`cwd` takes exactly one value"]
    );
}

#[test]
fn duplicate_cwd_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { cwd \"a\"; cwd \"b\" } }"),
        ["`cwd` is declared more than once"]
    );
}

#[test]
fn non_string_cwd_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { cwd 42 } }"),
        ["`cwd` takes one non-empty string"]
    );
}

#[test]
fn empty_cwd_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { cwd \"\" } }"),
        ["`cwd` takes one non-empty string"]
    );
}

#[test]
fn unknown_pane_config_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { colour \"red\" } }"),
        [
            "unknown node `colour` in `pane`; expected `command`, `cwd`, `env`, `size`, \
             `weight`, `min`, `preferred`, `focus`, or `expanded`"
        ]
    );
}

#[test]
fn plugin_without_name_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { plugin; pane } }"),
        ["`plugin` takes exactly one name string, like `plugin \"session-manager\"`"]
    );
}

#[test]
fn empty_plugin_name_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { plugin \"\"; pane } }"),
        ["`plugin` takes one non-empty name string"]
    );
}

#[test]
fn plugin_with_extra_arguments_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { plugin \"files\" \"tree\"; pane } }"),
        ["`plugin` takes exactly one name string, like `plugin \"session-manager\"`"]
    );
}

#[test]
fn command_inside_plugin_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { plugin \"files\" { command \"ls\" }; pane } }"),
        [
            "unknown node `command` in `plugin`; expected `size`, `weight`, `min`, \
             `preferred`, `focus`, or `expanded`"
        ]
    );
}

#[test]
fn focus_with_arguments_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { focus #true } }"),
        ["`focus` is a bare marker and takes no values or children"]
    );
}

#[test]
fn every_violation_is_collected_not_just_the_first() {
    let source = r#"
version 999
tab { pane; pane }
tab { stack { pane } }
"#;
    let found = messages(source);
    assert_eq!(
        found,
        [
            "config schema version 999 is newer than this koshi supports (1)",
            "`tab` holds one root node; wrap multiple panes in `horizontal`, `vertical`, or \
             `stack`",
            "`stack` needs at least two members",
        ]
    );
}
