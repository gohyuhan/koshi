//! `koshi keys list` and `koshi keys describe` as a user runs them: the
//! `--mode` and `--scope` values the binary refuses, the listing each scope
//! keeps, and the bindings a described prefix opens.

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
  [possible values: normal, locked, resize, pane-placement, tab, scroll, pane-move-choice, floating-pane-move, dock-undock]

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
continuous: false

key: <C-t> x
mode: normal
action: core:close-tab
display_name: Close Tab
description: Close the focused tab
scope: tab
source: defaults
continuous: false
"
            .to_string(),
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
