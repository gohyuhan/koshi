//! Tests for profile file parsing: the full schema on valid files, and one
//! diagnostic per violation on invalid ones.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use koshi_core::geometry::SplitDirection;
use koshi_layout::size::{SizeConstraint, SizeWeight};
use koshi_layout::template::{
    CommandTemplate, LeafTemplate, PluginTemplate, ProfileTemplate, TabTemplate, TemplateNode,
    TemplateSplit, TerminalTemplate,
};

use super::*;

fn parse(source: &str) -> Result<ProfileTemplate, ProfileError> {
    parse_profile(Path::new("profile/dev.kdl"), source)
}

/// The diagnostics of an `Invalid` outcome, as their messages.
fn messages(source: &str) -> Vec<String> {
    match parse(source) {
        Err(ProfileError::Invalid { diagnostics, .. }) => diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message().to_string())
            .collect(),
        Err(ProfileError::Syntax(_)) => panic!("expected schema diagnostics, got syntax error"),
        Ok(_) => panic!("expected schema diagnostics, got a template"),
    }
}

/// The diagnostics of an `Invalid` outcome, as the exact source text each
/// one's caret span covers.
fn span_texts(source: &str) -> Vec<String> {
    match parse(source) {
        Err(ProfileError::Invalid { diagnostics, .. }) => diagnostics
            .iter()
            .map(|diagnostic| {
                let span = diagnostic.span();
                source[span.offset()..span.offset() + span.len()].to_string()
            })
            .collect(),
        Err(ProfileError::Syntax(_)) => panic!("expected schema diagnostics, got syntax error"),
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
fn minimal_profile_is_one_shell_tab() {
    let template = parse("version 1\ntab {pane}").unwrap();
    assert_eq!(
        template,
        ProfileTemplate {
            tabs: vec![TabTemplate {
                root: shell_leaf(),
                focused_leaf: 0,
            }],
            focused_tab: 0,
            locked: false,
        }
    );
}

#[test]
fn nested_profile_parses_every_config_kind() {
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
            pane {min 5
                preferred 20}
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
            monitor,
            TemplateNode::Leaf(LeafTemplate::Plugin(PluginTemplate {
                name: "session-manager".to_string(),
            })),
        ],
        weights: vec![flex(), flex()],
        active: 0,
    });
    let right = TemplateNode::Split(TemplateSplit {
        direction: SplitDirection::Vertical,
        children: vec![shell_leaf(), stack],
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
    let expected = ProfileTemplate {
        tabs: vec![TabTemplate {
            root: TemplateNode::Split(TemplateSplit {
                direction: SplitDirection::Horizontal,
                children: vec![editor, right],
                weights: vec![
                    SizeWeight::new(SizeConstraint::Percent(60)),
                    SizeWeight::new(SizeConstraint::Percent(40)),
                ],
                active: 0,
            }),
            focused_leaf: 0,
        }],
        focused_tab: 0,
        locked: false,
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
        pane {focus}
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
    let template = parse("version 1\ntab { horizontal { pane {size 30}; pane } }").unwrap();
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
    assert_eq!(split.children.len(), 3);
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
    let template = parse("version 1\ntab { stack { pane; pane {expanded} } }").unwrap();
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
            pane {expanded}
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
    let template = parse("version 1\ntab { stack { pane {focus}; pane } }").unwrap();
    assert_eq!(template.tabs[0].focused_leaf, 0);
}

#[test]
fn older_version_is_accepted() {
    assert_eq!(
        messages("version 0\ntab {pane}"),
        ["config schema version must be at least 1"]
    );
}

#[test]
fn min_and_max_percent_size_are_accepted() {
    // 0% and 101% are already proven invalid; 1% and 100% are the boundary
    // just inside the valid range on either side.
    let low = parse("version 1\ntab { horizontal { pane { size \"1%\" }; pane } }").unwrap();
    let TemplateNode::Split(split) = &low.tabs[0].root else {
        panic!("expected split root");
    };
    assert_eq!(
        split.weights[0],
        SizeWeight::new(SizeConstraint::Percent(1))
    );

    let high = parse("version 1\ntab { horizontal { pane { size \"100%\" }; pane } }").unwrap();
    let TemplateNode::Split(split) = &high.tabs[0].root else {
        panic!("expected split root");
    };
    assert_eq!(
        split.weights[0],
        SizeWeight::new(SizeConstraint::Percent(100))
    );
}

#[test]
fn min_and_max_cell_size_are_accepted() {
    // 0 and 70000 are already proven invalid; 1 and 65535 (u16::MAX) are the
    // boundary just inside the valid range on either side.
    let low = parse("version 1\ntab { horizontal { pane {size 1}; pane } }").unwrap();
    let TemplateNode::Split(split) = &low.tabs[0].root else {
        panic!("expected split root");
    };
    assert_eq!(split.weights[0], SizeWeight::new(SizeConstraint::Fixed(1)));

    let high = parse("version 1\ntab { horizontal { pane {size 65535}; pane } }").unwrap();
    let TemplateNode::Split(split) = &high.tabs[0].root else {
        panic!("expected split root");
    };
    assert_eq!(
        split.weights[0],
        SizeWeight::new(SizeConstraint::Fixed(65535))
    );
}

#[test]
fn unicode_plugin_name_is_accepted() {
    let template = parse("version 1\ntab { horizontal { plugin \"\u{1f389}\"; pane } }").unwrap();
    let TemplateNode::Split(split) = &template.tabs[0].root else {
        panic!("expected split root");
    };
    assert_eq!(
        split.children[0],
        TemplateNode::Leaf(LeafTemplate::Plugin(PluginTemplate {
            name: "\u{1f389}".to_string(),
        }))
    );
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
        plugin "session-manager" {expanded}
    }
}
"#;
    let template = parse(source).unwrap();
    let TemplateNode::Split(split) = &template.tabs[0].root else {
        panic!("expected stack root");
    };
    assert_eq!(split.active, 1);
    assert_eq!(
        split.children[1],
        TemplateNode::Leaf(LeafTemplate::Plugin(PluginTemplate {
            name: "session-manager".to_string(),
        }))
    );
    assert_eq!(split.children.len(), 2);
}

#[test]
fn lock_marker_sets_the_starting_lock() {
    let template = parse("version 1\nlock\ntab {pane}").unwrap();
    assert!(template.locked);
}

#[test]
fn the_lock_marker_is_read_before_version_too() {
    let template = parse("lock\nversion 1\ntab {pane}").unwrap();
    assert!(template.locked);
}

#[test]
fn a_profile_without_the_lock_marker_starts_unlocked() {
    let template = parse("version 1\ntab {pane}").unwrap();
    assert!(!template.locked);
}

// -------------------------------------------------------------- invalid files

#[test]
fn syntax_error_is_the_syntax_variant() {
    let err = parse("tab {").unwrap_err();

    let ProfileError::Syntax(diagnostic) = err else {
        panic!("expected a syntax error, got {err:?}");
    };
    assert_eq!(
        diagnostic.to_string(),
        "config parse error in profile/dev.kdl"
    );
}

#[test]
fn invalid_report_names_the_file() {
    let err = parse("version 1").unwrap_err();
    assert_eq!(err.to_string(), "invalid profile file profile/dev.kdl");
}

#[test]
fn missing_version_is_reported() {
    assert_eq!(
        messages("tab {pane}"),
        ["profile file must declare `version`"]
    );
}

#[test]
fn newer_version_is_reported() {
    assert_eq!(
        messages("version 999\ntab {pane}"),
        ["config schema version 999 is newer than this koshi supports (1)"]
    );
}

#[test]
fn duplicate_version_is_reported() {
    assert_eq!(
        messages("version 1\nversion 1\ntab {pane}"),
        ["`version` is declared more than once"]
    );
}

#[test]
fn non_integer_version_is_reported() {
    assert_eq!(
        messages("version \"one\"\ntab {pane}"),
        ["`version` must be an integer from 1 to 4294967295"]
    );
}

#[test]
fn negative_version_is_reported() {
    assert_eq!(
        messages("version -1\ntab {pane}"),
        ["`version` must be an integer from 1 to 4294967295"]
    );
}

#[test]
fn version_with_children_is_reported() {
    assert_eq!(
        messages("version 1 {}\ntab {pane}"),
        ["`version` takes no children"]
    );
}

#[test]
fn version_as_property_is_reported() {
    assert_eq!(
        messages("version v=1\ntab {pane}"),
        ["`version` takes exactly one integer argument"]
    );
}

#[test]
fn version_with_two_arguments_is_reported() {
    assert_eq!(
        messages("version 1 2\ntab {pane}"),
        ["`version` takes exactly one integer argument"]
    );
}

#[test]
fn version_above_the_u32_ceiling_is_reported() {
    // 4294967296 is one past `u32::MAX`. The conversion fails before any
    // schema comparison runs.
    assert_eq!(
        messages("version 4294967296\ntab {pane}"),
        ["`version` must be an integer from 1 to 4294967295"]
    );
}

#[test]
fn missing_tabs_is_reported() {
    assert_eq!(
        messages("version 1"),
        ["profile file must define at least one `tab`"]
    );
}

#[test]
fn unknown_top_level_node_is_reported() {
    assert_eq!(
        messages("version 1\npane\ntab {pane}"),
        ["unknown key `pane`; did you mean `tab`?"]
    );
}

#[test]
fn empty_tab_is_reported() {
    assert_eq!(
        messages("version 1\ntab {}"),
        ["`tab` needs one layout node (`pane`, `plugin`, `horizontal`, `vertical`, or `stack`)"]
    );
}

#[test]
fn a_tab_without_a_children_block_is_reported() {
    // A bare `tab` carries no children block at all, where `tab {}` carries an
    // empty one; both reach the same missing-root report.
    assert_eq!(
        messages("version 1\ntab"),
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
        messages("version 1\ntab \"main\" {pane}"),
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
        messages("version 1\ntab { horizontal { pane {focus}; pane {focus} } }"),
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
        ["unknown key `tab.theme`; did you mean `tab.pane`?"]
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
        messages("version 1\ntab { horizontal {pane} }"),
        ["`horizontal` needs at least two children to divide space between"]
    );
}

#[test]
fn stack_with_one_member_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack {pane} }"),
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
        ["unknown key `vertical.border`; did you mean `vertical.pane`?"]
    );
}

#[test]
fn unknown_node_in_stack_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { focus; pane; pane } }"),
        ["unknown key `stack.focus`; did you mean `stack.pane`?"]
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
        messages("version 1\ntab { stack { pane {expanded}; vertical { pane {focus}; pane } } }"),
        ["`vertical` cannot be a stack member; stack members are `pane` or `plugin`"]
    );
}

#[test]
fn two_expanded_members_are_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane {expanded}; pane {expanded} } }"),
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
        messages("version 1\ntab { horizontal { pane {expanded}; pane } }"),
        ["`expanded` applies only to members of a `stack`"]
    );
}

#[test]
fn focus_on_collapsed_stack_member_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane; pane {focus} } }"),
        ["a collapsed stack member cannot hold focus; mark it `expanded`"]
    );
}

#[test]
fn sizing_on_stack_member_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane {size 30}; pane } }"),
        ["sizing applies only to children of `horizontal` or `vertical`"]
    );
}

#[test]
fn sizing_on_tab_root_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane {size 30} }"),
        ["sizing applies only to children of `horizontal` or `vertical`"]
    );
}

#[test]
fn sizing_on_a_split_at_the_tab_root_is_reported() {
    // A split carries sizing for its own slot in a parent split. The tab root
    // sits in no parent split.
    assert_eq!(
        messages("version 1\ntab { horizontal { size 30; pane; pane } }"),
        ["sizing applies only to children of `horizontal` or `vertical`"]
    );
}

#[test]
fn sizing_on_a_stack_at_the_tab_root_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { weight 2; pane; pane } }"),
        ["sizing applies only to children of `horizontal` or `vertical`"]
    );
}

#[test]
fn expanded_on_the_tab_root_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane {expanded} }"),
        ["`expanded` applies only to members of a `stack`"]
    );
}

#[test]
fn expanded_with_a_value_is_reported() {
    assert_eq!(
        messages("version 1\ntab { stack { pane { expanded #true }; pane } }"),
        ["`expanded` is a bare marker and takes no values or children"]
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
fn a_percent_over_255_reports_the_shape_not_the_range() {
    // The percent digits are read as a `u8`. 256 does not fit one, and the
    // value reports as an unreadable `size` rather than an out-of-range percent.
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size \"256%\" }; pane } }"),
        ["`size` is a cell count like `size 30` or a percentage like `size \"60%\"`"]
    );
}

#[test]
fn zero_cell_size_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane {size 0}; pane } }"),
        ["fixed size must be at least one cell"]
    );
}

#[test]
fn oversized_cell_size_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane {size 70000}; pane } }"),
        ["`size` cells must fit 1-65535"]
    );
}

#[test]
fn zero_weight_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane {weight 0}; pane } }"),
        ["flex weight must be at least 1"]
    );
}

#[test]
fn zero_min_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane {min 0}; pane } }"),
        ["`min` must be at least one cell"]
    );
}

#[test]
fn zero_preferred_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane {preferred 0}; pane } }"),
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
fn min_above_the_cell_ceiling_is_reported() {
    // 65536 is one past `u16::MAX`, the largest cell count a min may name.
    assert_eq!(
        messages("version 1\ntab { horizontal { pane {min 65536}; pane } }"),
        ["`min` must be an integer between 1 and 65535"]
    );
}

#[test]
fn non_integer_preferred_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { preferred \"5\" }; pane } }"),
        ["`preferred` must be an integer between 1 and 65535"]
    );
}

#[test]
fn weight_above_the_u32_ceiling_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane {weight 4294967296}; pane } }"),
        ["`weight` must be an integer between 1 and 4294967295"]
    );
}

#[test]
fn weight_arity_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane {weight 1 2}; pane } }"),
        ["`weight` takes exactly one value"]
    );
}

#[test]
fn min_as_a_property_is_reported() {
    // A named property (`m=5`) is not a positional argument: `single_argument`
    // must reject it the same way it rejects extra positional arguments.
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { min m=5 }; pane } }"),
        ["`min` takes exactly one value"]
    );
}

#[test]
fn two_sizing_violations_outside_directional_context_are_both_reported() {
    // Each sizing child of a leaf outside a directional split is checked and
    // reset independently, so two sizing nodes on the same leaf draw two
    // separate diagnostics, not one aggregated report.
    assert_eq!(
        messages("version 1\ntab { stack { pane { size 30; min 5 }; pane } }"),
        [
            "sizing applies only to children of `horizontal` or `vertical`",
            "sizing applies only to children of `horizontal` or `vertical`",
        ]
    );
}

#[test]
fn each_sizing_violation_points_its_caret_at_its_own_node() {
    // The per-node sizing reset keeps every diagnostic's caret on the node
    // that raised it: the second report points at `min 5`, not back at the
    // first node's `size 30`. (A kdl node's span runs to the next node or
    // closing brace, so the last node carries its trailing space.)
    assert_eq!(
        span_texts("version 1\ntab { stack { pane { size 30; min 5 }; pane } }"),
        ["size 30", "min 5 "]
    );
}

#[test]
fn size_with_children_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { pane { size 30 {} }; pane } }"),
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
        messages("version 1\ntab { pane {command} }"),
        ["`command` names a program, like `command \"nvim\" \"file.txt\"`"]
    );
}

#[test]
fn empty_command_program_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { command \"\" } }"),
        ["`command` program must not be empty"]
    );
}

#[test]
fn empty_command_program_with_arguments_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { command \"\" \"file.txt\" } }"),
        ["`command` program must not be empty"]
    );
}

#[test]
fn empty_command_argument_is_allowed() {
    // Only the program word must be non-empty; `""` is a legitimate
    // argument value for programs that take one.
    let template = parse("version 1\ntab { pane { command \"printf\" \"\" } }").unwrap();
    let TemplateNode::Leaf(LeafTemplate::Terminal(terminal)) = &template.tabs[0].root else {
        panic!("expected terminal leaf root");
    };
    let command = terminal.command.as_ref().unwrap();
    assert_eq!(command.program, PathBuf::from("printf"));
    assert_eq!(command.args, vec![String::new()]);
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
        messages("version 1\ntab { pane { command \"nvim\" {} } }"),
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
fn non_string_env_name_is_reported() {
    // A non-string entry is filtered out of the collected values, so its
    // count no longer matches `node.entries().len()`, and the mismatch
    // reports as the generic arity error rather than a name-specific one.
    assert_eq!(
        messages("version 1\ntab { pane { env 1 \"x\" } }"),
        ["`env` takes a name and a value, both strings, like `env \"RUST_LOG\" \"debug\"`"]
    );
}

#[test]
fn a_third_non_string_entry_is_reported_even_though_two_valid_strings_remain() {
    // Filtering the non-string third entry leaves exactly two strings, which
    // would coincidentally match the name/value shape; the entry-count
    // cross-check must still catch the extra entry rather than silently
    // accepting the first two and dropping it.
    assert_eq!(
        messages("version 1\ntab { pane { env \"A\" \"B\" 5 } }"),
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
fn case_variant_env_duplicate_is_reported() {
    // `Path` and `PATH` are one variable to a Windows child, so a layout
    // setting both must fail on every platform rather than behave
    // differently per OS.
    assert_eq!(
        messages("version 1\ntab { pane { env \"Path\" \"a\"; env \"PATH\" \"b\" } }"),
        [
            "`env` already sets `Path`; env names match case-insensitively (Windows folds \
             environment keys by case)"
        ]
    );
}

#[test]
fn env_name_with_equals_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { env \"A=B\" \"x\" } }"),
        ["`env` name must not contain `=`"]
    );
}

#[test]
fn env_nul_in_name_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { env \"A\\u{0}B\" \"x\" } }"),
        ["`env` name and value must not contain a NUL character"]
    );
}

#[test]
fn env_nul_in_value_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { env \"A\" \"x\\u{0}\" } }"),
        ["`env` name and value must not contain a NUL character"]
    );
}

#[test]
fn env_with_children_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { env \"A\" \"1\" {} } }"),
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
fn env_given_as_properties_is_reported() {
    // Properties are not positional arguments. Both are filtered out, and the
    // name/value pair never forms.
    assert_eq!(
        messages("version 1\ntab { pane { env name=\"A\" value=\"1\" } }"),
        ["`env` takes a name and a value, both strings, like `env \"RUST_LOG\" \"debug\"`"]
    );
}

#[test]
fn an_empty_env_value_is_allowed() {
    // Only the name must be non-empty. `""` sets the variable to the empty
    // string.
    let template = parse("version 1\ntab { pane { env \"A\" \"\" } }").unwrap();
    let TemplateNode::Leaf(LeafTemplate::Terminal(terminal)) = &template.tabs[0].root else {
        panic!("expected terminal leaf root");
    };
    assert_eq!(
        terminal.env,
        BTreeMap::from([("A".to_string(), String::new())])
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
        messages("version 1\ntab { pane {cwd 42} }"),
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
fn cwd_with_children_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { cwd \"a\" {} } }"),
        ["`cwd` takes no children"]
    );
}

#[test]
fn unknown_pane_config_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { colour \"red\" } }"),
        ["unknown key `pane.colour`; did you mean `pane.focus`?"]
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
fn non_string_plugin_name_is_reported() {
    // One positional argument of the wrong kind. The arity is right, and the
    // report names the value rather than the count.
    assert_eq!(
        messages("version 1\ntab { horizontal { plugin 42; pane } }"),
        ["`plugin` takes one non-empty name string"]
    );
}

#[test]
fn plugin_name_as_a_property_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { plugin name=\"files\"; pane } }"),
        ["`plugin` takes exactly one name string, like `plugin \"session-manager\"`"]
    );
}

#[test]
fn command_inside_plugin_is_reported() {
    assert_eq!(
        messages("version 1\ntab { horizontal { plugin \"files\" { command \"ls\" }; pane } }"),
        ["unknown key `plugin.command`; did you mean `plugin.min`?"]
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
tab { stack {pane} }
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

#[test]
fn lock_with_a_value_is_reported() {
    assert_eq!(
        messages("version 1\nlock #true\ntab {pane}"),
        ["`lock` is a bare marker and takes no values or children"]
    );
}

#[test]
fn lock_with_children_is_reported() {
    assert_eq!(
        messages("version 1\nlock {pane}\ntab {pane}"),
        ["`lock` is a bare marker and takes no values or children"]
    );
}

#[test]
fn a_malformed_second_lock_reports_its_shape_and_the_duplicate() {
    assert_eq!(
        messages("version 1\nlock\nlock #true\ntab {pane}"),
        [
            "`lock` is a bare marker and takes no values or children",
            "`lock` is declared more than once",
        ]
    );
}

#[test]
fn a_second_lock_marker_is_reported() {
    assert_eq!(
        messages("version 1\nlock\nlock\ntab {pane}"),
        ["`lock` is declared more than once"]
    );
}

#[test]
fn a_malformed_first_lock_still_reports_the_duplicate() {
    // The second `lock` is a duplicate whether or not the first one parsed,
    // so both faults reach the user in one run.
    assert_eq!(
        messages("version 1\nlock #true\nlock\ntab {pane}"),
        [
            "`lock` is a bare marker and takes no values or children",
            "`lock` is declared more than once",
        ]
    );
}

#[test]
fn cwd_with_a_nul_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { cwd \"/tmp\\u{0}x\" } }"),
        ["`cwd` must not contain a NUL character"]
    );
}

#[test]
fn command_program_with_a_nul_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { command \"nv\\u{0}im\" } }"),
        ["`command` program and arguments must not contain a NUL character"]
    );
}

#[test]
fn command_argument_with_a_nul_is_reported() {
    assert_eq!(
        messages("version 1\ntab { pane { command \"nvim\" \"fi\\u{0}le\" } }"),
        ["`command` program and arguments must not contain a NUL character"]
    );
}
