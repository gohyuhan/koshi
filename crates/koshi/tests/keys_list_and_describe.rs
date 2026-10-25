//! `koshi keys list` and `koshi keys describe` as a user runs them: the
//! `--mode` and `--scope` values the binary refuses, the listing each scope
//! keeps, the bindings a described prefix opens, and the user `bind` or
//! `remove` a described default names as what displaced it.

use std::fs;

use koshi_config::types::SCHEMA_VERSION;
use tempfile::TempDir;

mod common;

use common::{create_keybinding_file_path, run_koshi_under_home, KoshiRunOutput};

#[test]
fn keys_list_refuses_a_mode_that_is_not_built_in_and_names_the_built_in_ones() {
    let home_directory = TempDir::new().expect("create test home");
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "list", "--mode", "bogus"]),
        KoshiRunOutput {
            standard_output: String::new(),
            standard_error: "\
error: invalid value 'bogus' for '--mode <MODE>'
  [possible values: normal, locked, pane-placement, pane-move-choice, floating-pane-move, dock-undock, floating-pane-tray]

For more information, try '--help'.
"
            .to_string(),
            exit_code: Some(2),
        }
    );
}

#[test]
fn keys_list_refuses_the_session_and_layout_scopes() {
    let home_directory = TempDir::new().expect("create test home");
    for scope_name in ["session", "layout"] {
        assert_eq!(
            run_koshi_under_home(
                home_directory.path(),
                &["keys", "list", "--scope", scope_name]
            ),
            KoshiRunOutput {
                standard_output: String::new(),
                standard_error: format!(
                    "\
error: invalid value '{scope_name}' for '--scope <SCOPE>'
  [possible values: default, user]

For more information, try '--help'.
"
                ),
                exit_code: Some(2),
            }
        );
    }
}

#[test]
fn keys_list_scopes_split_a_displaced_default_from_the_user_binding_that_took_its_key() {
    // The user's `locked` block binds `<C-g>`, which the built-in
    // `core:mouse-select` held. The default scope lists that default as
    // unbound, and the user scope lists the user binding.
    let home_directory = TempDir::new().expect("create test home");
    fs::write(
        create_keybinding_file_path(home_directory.path()),
        format!("version {SCHEMA_VERSION}\nmode \"locked\" {{\n    bind \"<C-g>\" \"core:new-tab\"\n}}\n"),
    )
    .expect("write keybinding.kdl");

    assert_eq!(
        run_koshi_under_home(
            home_directory.path(),
            &["keys", "list", "--mode", "locked", "--scope", "default"]
        ),
        KoshiRunOutput {
            standard_output: "\
mode    key    action             source
locked  <C-g>  core:mouse-select  defaults (unbound)
locked  <C-l>  core:unlock        defaults
locked  <C-q>  core:quit          defaults
"
            .to_string(),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
    assert_eq!(
        run_koshi_under_home(
            home_directory.path(),
            &["keys", "list", "--mode", "locked", "--scope", "user"]
        ),
        KoshiRunOutput {
            standard_output: "\
mode    key    action        source
locked  <C-g>  core:new-tab  user
"
            .to_string(),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
}

#[test]
fn keys_list_names_the_keybinding_file_it_ignores_and_where_the_file_breaks() {
    let home_directory = TempDir::new().expect("create test home");
    let keybinding_file_path = create_keybinding_file_path(home_directory.path());
    fs::write(&keybinding_file_path, "version 2\nmode \"normal\" {\n")
        .expect("write keybinding.kdl");

    assert_eq!(
        run_koshi_under_home(
            home_directory.path(),
            &["keys", "list", "--mode", "locked", "--scope", "default"]
        ),
        KoshiRunOutput {
            standard_output: "\
mode    key    action             source
locked  <C-g>  core:mouse-select  defaults
locked  <C-l>  core:unlock        defaults
locked  <C-q>  core:quit          defaults
"
            .to_string(),
            standard_error: format!(
                "koshi: keybinding file {} ignored: line 2, column 15: No closing '}}' for child \
                 block\n",
                keybinding_file_path.display()
            ),
            exit_code: Some(0),
        }
    );
}

#[test]
fn keys_describe_lists_the_bindings_a_prefix_opens() {
    let home_directory = TempDir::new().expect("create test home");
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "describe", "<C-t>"]),
        KoshiRunOutput {
            standard_output: "\
key: <C-t> n
mode: normal
action: core:new-tab
display_name: New Tab
description: Create a new tab
scope: tab
source: defaults
displaced_by: -
continuous: false

key: <C-t> x
mode: normal
action: core:close-tab
display_name: Close Tab
description: Close the focused tab
scope: tab
source: defaults
displaced_by: -
continuous: false
"
            .to_string(),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
}

#[test]
fn keys_describe_marks_a_binding_whose_action_repeats_from_its_prefix_as_continuous() {
    let home_directory = TempDir::new().expect("create test home");
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "describe", "<C-p> <Left>"]),
        KoshiRunOutput {
            standard_output: "\
key: <C-p> <Left>
mode: normal
action: core:focus-pane-left
display_name: Focus Pane Left
description: Move the issuing client's focus to the pane on the left
scope: client
source: defaults
displaced_by: -
continuous: true
"
            .to_string(),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
}

/// The `core:mouse-select` block `koshi keys describe "<C-g>"` prints for
/// `mode_name`, with its `source` and `displaced_by` values.
fn build_mouse_select_detail_block(
    mode_name: &str,
    binding_source: &str,
    displaced_by: &str,
) -> String {
    format!(
        "\
key: <C-g>
mode: {mode_name}
action: core:mouse-select
display_name: Mouse Select
description: Toggle grabbing the mouse for text selection, so a drag highlights in koshi even over a program that asked for the mouse
scope: client
source: {binding_source}
displaced_by: {displaced_by}
continuous: false
"
    )
}

#[test]
fn keys_describe_names_the_user_bind_that_displaced_a_default() {
    // `<C-g>` is `core:mouse-select` in `locked` and `normal`. The user's
    // `locked` block binds it to `core:new-tab`.
    let home_directory = TempDir::new().expect("create test home");
    fs::write(
        create_keybinding_file_path(home_directory.path()),
        format!("version {SCHEMA_VERSION}\nmode \"locked\" {{\n    bind \"<C-g>\" \"core:new-tab\"\n}}\n"),
    )
    .expect("write keybinding.kdl");

    let user_detail_block = "\
key: <C-g>
mode: locked
action: core:new-tab
display_name: New Tab
description: Create a new tab
scope: tab
source: user
displaced_by: -
continuous: false
";
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "describe", "<C-g>"]),
        KoshiRunOutput {
            standard_output: format!(
                "{user_detail_block}\n{}\n{}",
                build_mouse_select_detail_block("locked", "defaults (unbound)", "user bind"),
                build_mouse_select_detail_block("normal", "defaults", "-"),
            ),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
}

#[test]
fn keys_describe_names_the_user_remove_that_cleared_a_default_and_exits_0() {
    let home_directory = TempDir::new().expect("create test home");
    fs::write(
        create_keybinding_file_path(home_directory.path()),
        format!("version {SCHEMA_VERSION}\nmode \"locked\" {{\n    remove \"<C-g>\"\n}}\n"),
    )
    .expect("write keybinding.kdl");

    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "describe", "<C-g>"]),
        KoshiRunOutput {
            standard_output: format!(
                "{}\n{}",
                build_mouse_select_detail_block("locked", "defaults (unbound)", "user remove"),
                build_mouse_select_detail_block("normal", "defaults", "-"),
            ),
            standard_error: String::new(),
            exit_code: Some(0),
        }
    );
}

#[test]
fn keys_describe_refuses_a_sequence_that_is_no_binding_and_starts_none() {
    let home_directory = TempDir::new().expect("create test home");
    assert_eq!(
        run_koshi_under_home(home_directory.path(), &["keys", "describe", "<C-z>"]),
        KoshiRunOutput {
            standard_output: String::new(),
            standard_error: "koshi: nothing is bound on `<C-z>` in any mode\n".to_string(),
            exit_code: Some(2),
        }
    );
}
