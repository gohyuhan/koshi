//! Tests for discovery output rendering: exact JSON schema snapshots (the
//! stable scripting surface) and exact table/field renderings, all over
//! fixed fake data.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use koshi_config::types::{BoundAction, ModeName};
use koshi_core::action::{
    core_action_seeds, ActionHandlerRef, ActionRef, ActionScope, ActionStatus, TargetKind,
};
use koshi_core::discovery::{ClientInfo, PaneInfo, PaneState, SessionInfo, TabInfo};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, PluginId, SessionId, TabId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::lock::LockMode;
use koshi_core::resolve::ActionArgs;
use uuid::Uuid;

use super::*;
use crate::cli::FormatArg;

/// The fixed UUID every fake id uses, so snapshots are byte-stable.
fn fixed_uuid() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// A fixed timestamp: 1234 seconds after the Unix epoch.
fn fixed_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1234)
}

fn session_info() -> SessionInfo {
    SessionInfo {
        id: SessionId::from_uuid(fixed_uuid()),
        name: "quiet-lake".to_string(),
        created_at: fixed_time(),
        attached_clients: vec![ClientId::from_uuid(fixed_uuid())],
        pane_count: 3,
    }
}

fn tab_info() -> TabInfo {
    TabInfo {
        id: TabId::from_uuid(fixed_uuid()),
        session_id: SessionId::from_uuid(fixed_uuid()),
        name: "amber-fox".to_string(),
        index: 1,
        active_pane: Some(PaneId::from_uuid(fixed_uuid())),
        pane_count: 2,
    }
}

fn pane_info() -> PaneInfo {
    PaneInfo {
        id: PaneId::from_uuid(fixed_uuid()),
        tab_id: TabId::from_uuid(fixed_uuid()),
        session_id: SessionId::from_uuid(fixed_uuid()),
        title: Some("htop".to_string()),
        cwd: Some(PathBuf::from("/home/user")),
        command: Some(vec!["htop".to_string(), "--tree".to_string()]),
        state: PaneState::Running,
        focused_by_clients: vec![ClientId::from_uuid(fixed_uuid())],
    }
}

fn session_row() -> SessionRow {
    SessionRow {
        id: SessionId::from_uuid(fixed_uuid()),
        name: "quiet-lake".to_string(),
    }
}

fn tab_row() -> TabRow {
    TabRow {
        id: TabId::from_uuid(fixed_uuid()),
        name: "amber-fox".to_string(),
        session: SessionId::from_uuid(fixed_uuid()),
        session_name: "quiet-lake".to_string(),
    }
}

fn pane_row() -> PaneRow {
    PaneRow {
        id: PaneId::from_uuid(fixed_uuid()),
        name: Some("htop".to_string()),
        tab: TabId::from_uuid(fixed_uuid()),
        tab_name: "amber-fox".to_string(),
        session: SessionId::from_uuid(fixed_uuid()),
        session_name: "quiet-lake".to_string(),
    }
}

fn client_row() -> ClientRow {
    ClientRow {
        id: ClientId::from_uuid(fixed_uuid()),
        session: SessionId::from_uuid(fixed_uuid()),
        session_name: "quiet-lake".to_string(),
    }
}

fn client_info() -> ClientInfo {
    ClientInfo {
        id: ClientId::from_uuid(fixed_uuid()),
        session_id: SessionId::from_uuid(fixed_uuid()),
        attached_at: fixed_time(),
        viewport_size: Size {
            cols: 120,
            rows: 40,
        },
        active_tab: TabId::from_uuid(fixed_uuid()),
        focused_pane: None,
        lock_state: LockMode::Normal,
    }
}

// --- JSON schema snapshots ---

#[test]
fn session_json_schema_is_stable() {
    let expected = r#"{
  "id": "00000000-0000-0000-0000-000000000001",
  "name": "quiet-lake",
  "created_at": {
    "secs_since_epoch": 1234,
    "nanos_since_epoch": 0
  },
  "attached_clients": [
    "00000000-0000-0000-0000-000000000001"
  ],
  "pane_count": 3
}
"#;
    assert_eq!(render_session(&session_info(), FormatArg::Json), expected);
}

#[test]
fn session_list_json_is_an_array_of_id_and_name() {
    let expected = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "name": "quiet-lake"
  }
]
"#;
    assert_eq!(render_sessions(&[session_row()], FormatArg::Json), expected);
}

#[test]
fn tab_list_json_carries_the_owning_session() {
    let expected = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "name": "amber-fox",
    "session": "00000000-0000-0000-0000-000000000001",
    "session_name": "quiet-lake"
  }
]
"#;
    assert_eq!(render_tabs(&[tab_row()], FormatArg::Json), expected);
}

#[test]
fn pane_list_json_carries_the_whole_id_chain() {
    let expected = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "name": "htop",
    "tab": "00000000-0000-0000-0000-000000000001",
    "tab_name": "amber-fox",
    "session": "00000000-0000-0000-0000-000000000001",
    "session_name": "quiet-lake"
  }
]
"#;
    assert_eq!(render_panes(&[pane_row()], FormatArg::Json), expected);
}

#[test]
fn an_untitled_pane_lists_a_null_name_in_json() {
    let pane = PaneRow {
        name: None,
        ..pane_row()
    };
    let rendered = render_panes(&[pane], FormatArg::Json);
    assert!(
        rendered.contains("\"name\": null,"),
        "unexpected name form: {rendered}"
    );
}

#[test]
fn client_list_json_carries_the_owning_session() {
    let expected = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "session": "00000000-0000-0000-0000-000000000001",
    "session_name": "quiet-lake"
  }
]
"#;
    assert_eq!(render_clients(&[client_row()], FormatArg::Json), expected);
}

#[test]
fn tab_json_schema_is_stable() {
    let expected = r#"{
  "id": "00000000-0000-0000-0000-000000000001",
  "session_id": "00000000-0000-0000-0000-000000000001",
  "name": "amber-fox",
  "index": 1,
  "active_pane": "00000000-0000-0000-0000-000000000001",
  "pane_count": 2
}
"#;
    assert_eq!(render_tab(&tab_info(), FormatArg::Json), expected);
}

#[test]
fn pane_json_schema_is_stable() {
    let expected = r#"{
  "id": "00000000-0000-0000-0000-000000000001",
  "tab_id": "00000000-0000-0000-0000-000000000001",
  "session_id": "00000000-0000-0000-0000-000000000001",
  "title": "htop",
  "cwd": "/home/user",
  "command": [
    "htop",
    "--tree"
  ],
  "state": "running",
  "focused_by_clients": [
    "00000000-0000-0000-0000-000000000001"
  ]
}
"#;
    assert_eq!(render_pane(&pane_info(), FormatArg::Json), expected);
}

#[test]
fn non_utf8_cwd_renders_lossily_in_json() {
    let mut pane = pane_info();
    pane.cwd = Some(non_utf8_path());
    let expected = r#"{
  "id": "00000000-0000-0000-0000-000000000001",
  "tab_id": "00000000-0000-0000-0000-000000000001",
  "session_id": "00000000-0000-0000-0000-000000000001",
  "title": "htop",
  "cwd": "/tmp/f�oo",
  "command": [
    "htop",
    "--tree"
  ],
  "state": "running",
  "focused_by_clients": [
    "00000000-0000-0000-0000-000000000001"
  ]
}
"#;
    assert_eq!(render_pane(&pane, FormatArg::Json), expected);
}

/// A path containing bytes that are not valid UTF-8; its lossy form is
/// `/tmp/f\u{FFFD}oo` on every platform.
fn non_utf8_path() -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/f\x80oo".to_vec()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        // `/tmp/f` + an unpaired surrogate (invalid UTF-16) + `oo`.
        PathBuf::from(std::ffi::OsString::from_wide(&[
            0x2F, 0x74, 0x6D, 0x70, 0x2F, 0x66, 0xD800, 0x6F, 0x6F,
        ]))
    }
}

#[test]
fn exited_pane_state_json_carries_the_code() {
    let mut pane = pane_info();
    pane.state = PaneState::Exited { code: Some(0) };
    let rendered = render_pane(&pane, FormatArg::Json);
    assert!(
        rendered.contains("\"state\": {\n    \"exited\": {\n      \"code\": 0\n    }\n  }"),
        "unexpected state form: {rendered}"
    );
}

#[test]
fn client_json_schema_is_stable() {
    let expected = r#"{
  "id": "00000000-0000-0000-0000-000000000001",
  "session_id": "00000000-0000-0000-0000-000000000001",
  "attached_at": {
    "secs_since_epoch": 1234,
    "nanos_since_epoch": 0
  },
  "viewport_size": {
    "cols": 120,
    "rows": 40
  },
  "active_tab": "00000000-0000-0000-0000-000000000001",
  "focused_pane": null,
  "lock_state": "Normal"
}
"#;
    assert_eq!(render_client(&client_info(), FormatArg::Json), expected);
}

// --- Table renderings ---

#[test]
fn session_table_aligns_columns() {
    let expected = "\
id                                            name
session-00000000-0000-0000-0000-000000000001  quiet-lake
";
    assert_eq!(
        render_sessions(&[session_row()], FormatArg::Table),
        expected
    );
}

#[test]
fn empty_list_table_is_just_the_header() {
    assert_eq!(render_sessions(&[], FormatArg::Table), "id  name\n");
}

#[test]
fn tab_table_names_the_owning_session() {
    let expected = "\
id                                        name       session                                       session_name
tab-00000000-0000-0000-0000-000000000001  amber-fox  session-00000000-0000-0000-0000-000000000001  quiet-lake
";
    assert_eq!(render_tabs(&[tab_row()], FormatArg::Table), expected);
}

#[test]
fn pane_table_names_the_owning_tab_and_session() {
    let expected = "\
id                                         name  tab                                       tab_name   session                                       session_name
pane-00000000-0000-0000-0000-000000000001  htop  tab-00000000-0000-0000-0000-000000000001  amber-fox  session-00000000-0000-0000-0000-000000000001  quiet-lake
";
    assert_eq!(render_panes(&[pane_row()], FormatArg::Table), expected);
}

#[test]
fn an_untitled_pane_lists_a_dash_for_its_name() {
    let pane = PaneRow {
        name: None,
        ..pane_row()
    };
    let rendered = render_panes(&[pane], FormatArg::Table);
    let row = rendered.lines().nth(1).expect("one data row");
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(
        cells,
        vec![
            "pane-00000000-0000-0000-0000-000000000001",
            "-",
            "tab-00000000-0000-0000-0000-000000000001",
            "amber-fox",
            "session-00000000-0000-0000-0000-000000000001",
            "quiet-lake",
        ]
    );
}

#[test]
fn absent_values_render_as_dashes() {
    let mut pane = pane_info();
    pane.title = None;
    pane.cwd = None;
    pane.command = None;
    pane.state = PaneState::Exited { code: None };
    let rendered = render_pane(&pane, FormatArg::Table);
    assert_eq!(
        rendered,
        "\
id: pane-00000000-0000-0000-0000-000000000001
tab: tab-00000000-0000-0000-0000-000000000001
session: session-00000000-0000-0000-0000-000000000001
title: -
cwd: -
command: -
state: exited(-)
focused_by: 1
"
    );
}

#[test]
fn client_fields_render_as_lines() {
    let expected = "\
id: client-00000000-0000-0000-0000-000000000001
session: session-00000000-0000-0000-0000-000000000001
attached_at: 1234
viewport: 120x40
active_tab: tab-00000000-0000-0000-0000-000000000001
focused_pane: -
lock: Normal
";
    assert_eq!(render_client(&client_info(), FormatArg::Table), expected);
}

// --- Action introspection ---

/// The count of seeded actions the runtime supports today.
fn available_seed_count() -> usize {
    core_action_seeds()
        .iter()
        .filter(|(_, metadata)| metadata.status == ActionStatus::Available)
        .count()
}

#[test]
fn actions_list_table_shows_only_supported_actions() {
    let rendered = render_actions_list(FormatArg::Table);
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), available_seed_count() + 1);
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        vec!["action", "command", "scope"]
    );
    // The first supported action is new-pane.
    assert_eq!(
        lines[1].split_whitespace().collect::<Vec<_>>(),
        vec!["core:new-pane", "NewPane", "pane-session"]
    );
    // Coming-soon actions never appear.
    assert!(
        !rendered.contains("copy-selection") && !rendered.contains("plugin-"),
        "coming-soon actions leaked into the list:\n{rendered}"
    );
}

#[test]
fn actions_list_json_is_an_array_of_supported_summaries() {
    let rendered = render_actions_list(FormatArg::Json);
    assert!(rendered.starts_with("[\n"), "not an array: {rendered}");
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    let array = value.as_array().expect("a JSON array");
    assert_eq!(array.len(), available_seed_count());
    assert_eq!(array[0]["action"], "core:new-pane");
    assert_eq!(array[0]["command"], "NewPane");
    assert_eq!(array[0]["scope"], "pane-session");
    assert!(
        !rendered.contains("copy-selection") && !rendered.contains("plugin-"),
        "coming-soon actions leaked into JSON:\n{rendered}"
    );
}

#[test]
fn explain_new_pane_fields_are_exact() {
    let expected = "\
action: core:new-pane
display_name: New Pane
description: Split the focused pane and start a shell in the new one
scope: pane-session
targets: pane
command: NewPane
examples: core:new-pane, koshi new-pane
";
    assert_eq!(
        render_action_explain("new-pane", FormatArg::Table),
        Some(expected.to_string())
    );
}

#[test]
fn explain_new_pane_json_is_exact() {
    let expected = r#"{
  "action": "core:new-pane",
  "display_name": "New Pane",
  "description": "Split the focused pane and start a shell in the new one",
  "scope": "pane-session",
  "targets": [
    "pane"
  ],
  "command": "NewPane",
  "examples": [
    "core:new-pane",
    "koshi new-pane"
  ]
}
"#;
    assert_eq!(
        render_action_explain("new-pane", FormatArg::Json),
        Some(expected.to_string())
    );
}

#[test]
fn explain_accepts_a_full_core_ref() {
    assert_eq!(
        render_action_explain("core:new-pane", FormatArg::Json),
        render_action_explain("new-pane", FormatArg::Json),
    );
}

#[test]
fn explain_run_omits_the_koshi_example() {
    // run is supported but `koshi run` needs a command, so no CLI example is
    // shown — only the config reference.
    let expected = r#"{
  "action": "core:run",
  "display_name": "Run Command",
  "description": "Spawn a command in a new pane",
  "scope": "pane-session",
  "targets": [
    "pane"
  ],
  "command": "RunCommandPane",
  "examples": [
    "core:run"
  ]
}
"#;
    assert_eq!(
        render_action_explain("run", FormatArg::Json),
        Some(expected.to_string())
    );
}

#[test]
fn explain_of_a_coming_soon_action_is_hidden() {
    // The selection and plugin actions are registered but have no
    // runtime handler yet, so explain treats them as unknown — by bare name and
    // by full ref. These are seeded actions on purpose: an unregistered name is
    // hidden too, but for a different reason, which
    // `explain_of_an_unknown_action_is_none` covers.
    assert_eq!(
        render_action_explain("copy-selection", FormatArg::Json),
        None
    );
    assert_eq!(
        render_action_explain("core:copy-selection", FormatArg::Json),
        None
    );
    assert_eq!(
        render_action_explain("plugin-install", FormatArg::Json),
        None
    );
}

#[test]
fn explain_of_an_unknown_action_is_none() {
    assert_eq!(
        render_action_explain("does-not-exist", FormatArg::Json),
        None
    );
}

#[test]
fn explain_renders_multiple_targets_joined() {
    // focus-pane targets a pane and a client; both join into one cell. It needs
    // a --pane flag, so no bare CLI example is shown.
    let expected = "\
action: core:focus-pane
display_name: Focus Pane
description: Move the issuing client's focus to a pane
scope: client
targets: pane, client
command: FocusPane
examples: core:focus-pane
";
    assert_eq!(
        render_action_explain("focus-pane", FormatArg::Table),
        Some(expected.to_string())
    );
}

#[test]
fn an_empty_target_list_renders_as_a_dash() {
    // Every supported action has at least one target today, so exercise the
    // join helper directly to keep the empty branch covered.
    assert_eq!(join_cell(&[]), "-");
    assert_eq!(
        join_cell(&["pane".to_string(), "client".to_string()]),
        "pane, client"
    );
}

// --- Cell helpers not reachable through the fixed fake data above ---

#[test]
fn state_cell_renders_spawning_and_closing() {
    // Running and both Exited forms are covered via the pane table tests
    // above; Spawning and Closing are not exercised by any fixed fixture.
    assert_eq!(state_cell(PaneState::Spawning), "spawning");
    assert_eq!(state_cell(PaneState::Closing), "closing");
}

#[test]
fn time_cell_before_the_unix_epoch_renders_as_a_dash() {
    // `duration_since` fails for a time earlier than the epoch; the cell
    // falls back to "-" rather than panicking or underflowing.
    let before_epoch = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
    assert_eq!(time_cell(before_epoch), "-");
}

#[test]
fn scope_label_renders_tab_and_global() {
    // PaneSession and Client are covered indirectly by the `new-pane` and
    // `focus-pane` explain tests above; Tab and Global are not.
    assert_eq!(scope_label(ActionScope::Tab), "tab");
    assert_eq!(scope_label(ActionScope::Global), "global");
}

#[test]
fn target_label_renders_session_and_tab() {
    // Pane and Client are covered indirectly by the `focus-pane` explain test
    // above; Session and Tab are not.
    assert_eq!(target_label(TargetKind::Session), "session");
    assert_eq!(target_label(TargetKind::Tab), "tab");
}

#[test]
fn command_label_renders_plugin_host_and_sequence() {
    // Every seeded core action dispatches through `CoreCommand`, so the
    // plugin-host and sequence handler kinds are never reachable through
    // `render_actions_list`/`render_action_explain` today; exercise the
    // helper directly so those two arms stay covered.
    assert_eq!(
        command_label(&ActionHandlerRef::PluginHostCall(PluginId::new())),
        "plugin-host"
    );
    assert_eq!(
        command_label(&ActionHandlerRef::Sequence(vec![])),
        "sequence"
    );
}

#[test]
fn table_column_width_counts_characters_not_display_width() {
    // The table layout pads by `.chars().count()`, not visual/display width.
    // "文字文字" is 4 Rust chars (each a double-width CJK glyph, 8 terminal
    // columns), the same char count as the 4-char header "name" — so the
    // implementation adds no padding, even though the two would not align in
    // a real terminal. This locks in the actual (character-count) behavior.
    assert_eq!(
        table(&["name"], vec![vec!["文字文字".to_string()]]),
        "name\n文字文字\n"
    );
}

#[test]
fn explain_new_tab_reports_tab_scope_and_target() {
    // `new-tab` is seeded with `ActionScope::Tab` and `TargetKind::Tab`,
    // neither of which any other explain test exercises end-to-end.
    let expected = "\
action: core:new-tab
display_name: New Tab
description: Create a new tab
scope: tab
targets: tab
command: NewTab
examples: core:new-tab, koshi new-tab
";
    assert_eq!(
        render_action_explain("new-tab", FormatArg::Table),
        Some(expected.to_string())
    );
}

#[test]
fn explain_rename_session_reports_global_scope_and_session_target() {
    // `rename-session` is seeded with `ActionScope::Global` and
    // `TargetKind::Session`, neither of which any other explain test
    // exercises end-to-end.
    let expected = "\
action: core:rename-session
display_name: Rename Session
description: Assign a fresh generated name to the current session, or one named by id
scope: global
targets: session
command: RenameSession
examples: core:rename-session, koshi rename-session
";
    assert_eq!(
        render_action_explain("rename-session", FormatArg::Table),
        Some(expected.to_string())
    );
}

// --- Keys rendering ---

/// Parse a test key sequence with the default leader and depth.
fn keyseq(s: &str) -> koshi_core::key::KeySequence {
    koshi_config::key_sequence::parse_sequence(
        s,
        koshi_config::types::KeybindingsConfig::default().leader,
        8,
    )
    .expect("test sequence parses")
}

/// The offline view for one `normal`-mode user binding of `key` to `action`.
fn view_with_binding(key: &str, action: &str) -> crate::keymap::KeymapView {
    use std::collections::BTreeMap;
    use std::str::FromStr;
    let mut keys = BTreeMap::new();
    keys.insert(
        keyseq(key),
        koshi_config::types::BoundAction {
            action: koshi_core::action::ActionRef::from_str(action).expect("valid ref"),
            args: koshi_core::resolve::ActionArgs::None,
        },
    );
    let mut modes = BTreeMap::new();
    modes.insert(
        koshi_config::types::ModeName::new("normal"),
        koshi_config::types::ModeBindings {
            keys,
            removed: Default::default(),
        },
    );
    crate::keymap::view_from_partial(
        Some(koshi_config::layer::PartialKeybindingsConfig {
            modes: Some(modes),
            ..Default::default()
        }),
        None,
        None,
    )
}

#[test]
fn keys_list_shows_a_steal_and_its_unbound_default() {
    let view = view_with_binding("<A-f>", "core:close-pane");
    let rendered = render_keys_list(&view, Some("normal"), None, FormatArg::Json);
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    let bindings = value["bindings"].as_array().expect("array");
    assert!(
        bindings.contains(&serde_json::json!({
            "mode": "normal",
            "key": "<A-f>",
            "action": "core:close-pane",
            "source": "user",
        })),
        "got: {rendered}"
    );
    assert!(
        bindings.contains(&serde_json::json!({
            "mode": "normal",
            "key": "<A-f>",
            "action": "core:toggle-pane-fullscreen",
            "source": "defaults (unbound)",
        })),
        "got: {rendered}"
    );
}

#[test]
fn keys_list_scope_filter_keeps_only_the_named_layer() {
    let view = view_with_binding("<C-y>", "core:new-tab");
    let rendered = render_keys_list(&view, None, Some(ScopeArg::User), FormatArg::Table);
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), 2, "header plus the one user row: {rendered}");
    assert_eq!(lines[1], "normal  <C-y>  core:new-tab  user");
}

#[test]
fn keys_list_mode_filter_keeps_only_the_named_mode() {
    let view = crate::keymap::view_from_partial(None, None, None);
    let rendered = render_keys_list(&view, Some("locked"), None, FormatArg::Json);
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value["reverted"], serde_json::json!(false));
    let bindings = value["bindings"].as_array().expect("array");
    assert!(!bindings.is_empty());
    assert!(bindings
        .iter()
        .all(|binding| binding["mode"] == serde_json::json!("locked")));
}

#[test]
fn keys_recommended_is_empty_until_plugins_exist() {
    assert_eq!(render_keys_recommended(FormatArg::Json), "[]\n");
    assert_eq!(
        render_keys_recommended(FormatArg::Table),
        "key  action  plugin\n"
    );
}

#[test]
fn keys_describe_renders_the_binding_and_source() {
    let view = crate::keymap::view_from_partial(None, None, None);
    let rendered = render_keys_describe(&view, "<C-p> x", FormatArg::Table)
        .expect("sequence parses")
        .expect("bound in normal mode");
    let expected = "\
key: <C-p> x
mode: normal
action: core:close-pane-tree
display_name: Close Pane Tree
description: Close the focused pane and kill every process it started
scope: pane-session
args: -
source: defaults
continuous: false
";
    assert_eq!(rendered, expected);
}

#[test]
fn keys_describe_renders_system_authored_args_as_json() {
    // No shipped binding carries arguments; system-authored layers (plugin
    // manifests) may. Build that state directly to pin the args rendering.
    let mut view = crate::keymap::view_from_partial(None, None, None);
    let key = KeySequence::from(KeyChord::new(ModFlags::ALT, Key::Char('r')));
    view.merged
        .modes
        .get_mut(&ModeName::new("normal"))
        .expect("normal mode is merged")
        .defaults
        .insert(
            key,
            BoundAction {
                action: ActionRef::core("run").expect("valid name"),
                args: ActionArgs::Run {
                    program: PathBuf::from("/usr/bin/htop"),
                    args: vec!["--tree".to_string()],
                    direction: None,
                    stacked: false,
                },
            },
        );
    let rendered = render_keys_describe(&view, "<A-r>", FormatArg::Json)
        .expect("sequence parses")
        .expect("bound in normal mode");
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value[0]["action"], serde_json::json!("core:run"));
    assert_eq!(
        value[0]["args"],
        serde_json::json!({
            "Run": {
                "program": "/usr/bin/htop",
                "args": ["--tree"],
                "direction": null,
                "stacked": false,
            }
        })
    );
}

#[test]
fn keys_describe_renders_missing_args_as_null() {
    let view = crate::keymap::view_from_partial(None, None, None);
    let rendered = render_keys_describe(&view, "<A-f>", FormatArg::Json)
        .expect("sequence parses")
        .expect("bound in normal mode");
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value[0]["args"], serde_json::Value::Null);
    assert_eq!(
        value[0]["action"],
        serde_json::json!("core:toggle-pane-fullscreen")
    );
}

#[test]
fn keys_describe_reports_unbound_and_malformed_sequences() {
    let view = crate::keymap::view_from_partial(None, None, None);
    assert_eq!(
        render_keys_describe(&view, "<C-z>", FormatArg::Table),
        Ok(None)
    );
    assert!(render_keys_describe(&view, "Ctrl-g", FormatArg::Table).is_err());
}

#[test]
fn keys_conflicts_renders_the_verdict_and_findings() {
    // Binding an unregistered action is an orphan warning; the verdict
    // still applies.
    let view = view_with_binding("<C-y>", "core:not-a-real-action");
    let rendered = render_keys_conflicts(&view, FormatArg::Json);
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value["verdict"], serde_json::json!("apply"));
    assert_eq!(value["file_error"], serde_json::Value::Null);
    let findings = value["findings"].as_array().expect("array");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["severity"], serde_json::json!("warning"));
}

#[test]
fn keys_conflicts_carries_an_ignored_file_on_both_formats() {
    // An unparseable file leaves the defaults running; the answer itself
    // says so, so a stdout-only consumer never mistakes it for a clean file.
    let view = crate::keymap::view_from_partial(None, None, Some("boom".to_string()));
    let table_rendered = render_keys_conflicts(&view, FormatArg::Table);
    assert_eq!(table_rendered, "file: ignored (boom)\nverdict: apply\n");
    let value: serde_json::Value =
        serde_json::from_str(&render_keys_conflicts(&view, FormatArg::Json)).expect("valid JSON");
    assert_eq!(value["file_error"], serde_json::json!("boom"));
    assert_eq!(value["verdict"], serde_json::json!("apply"));
}

#[test]
fn keys_validate_renders_both_outcome_shapes() {
    let failed = crate::keymap::ValidationOutcome::ParseFailed(vec!["bad node".to_string()]);
    assert_eq!(
        render_keys_validate(&failed, FormatArg::Table),
        "invalid: the file does not parse\nerror: bad node\n"
    );
    let failed_json: serde_json::Value =
        serde_json::from_str(&render_keys_validate(&failed, FormatArg::Json)).expect("valid JSON");
    assert_eq!(
        failed_json,
        serde_json::json!({
            "valid": false,
            "applies": false,
            "errors": ["bad node"],
            "findings": [],
        })
    );

    let clean = crate::keymap::view_from_partial(None, None, None);
    let checked = crate::keymap::ValidationOutcome::Checked {
        report: clean.report,
        applies: true,
    };
    assert_eq!(
        render_keys_validate(&checked, FormatArg::Table),
        "valid: a reload would apply this file\n"
    );
    assert!(validation_applies(&checked));
    assert!(!validation_applies(&failed));
}

/// The offline view for a user file whose `unlock_alternative` sits on a chord
/// plain typing produces, which detection rejects as fatal.
fn view_with_typeable_unlock_alternative() -> crate::keymap::KeymapView {
    crate::keymap::view_from_partial(
        Some(koshi_config::layer::PartialKeybindingsConfig {
            unlock_alternative: Some(Some(KeyChord::new(ModFlags::NONE, Key::Char('u')))),
            ..Default::default()
        }),
        None,
        None,
    )
}

#[test]
fn keys_conflicts_reports_a_reject_verdict_and_a_fatal_finding() {
    // A typeable unlock alternative is a fatal finding, so the verdict rejects
    // the file and the offline listing keeps the defaults.
    let view = view_with_typeable_unlock_alternative();
    let rendered = render_keys_conflicts(&view, FormatArg::Json);
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value["verdict"], serde_json::json!("reject"));
    assert_eq!(value["file_error"], serde_json::Value::Null);
    let findings = value["findings"].as_array().expect("array");
    assert!(
        findings
            .iter()
            .any(|finding| finding["severity"] == serde_json::json!("fatal")),
        "expected a fatal finding: {rendered}"
    );
}

#[test]
fn keys_list_marks_a_rejected_user_file_as_reverted() {
    // The rejected file drops the view back to the defaults, so every listed
    // binding is a shipped one and none is sourced to the user layer.
    let view = view_with_typeable_unlock_alternative();
    let rendered = render_keys_list(&view, None, None, FormatArg::Json);
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value["reverted"], serde_json::json!(true));
    let bindings = value["bindings"].as_array().expect("array");
    assert!(!bindings.is_empty(), "defaults still list: {rendered}");
    assert!(
        bindings
            .iter()
            .all(|binding| binding["source"] != serde_json::json!("user")),
        "a rejected file must contribute no user bindings: {rendered}"
    );
}

#[test]
fn keys_describe_renders_one_field_block_per_mode_the_key_is_bound_in() {
    // The same key bound in two modes prints two field blocks, separated by a
    // blank line, in mode-name order (`locked` before `normal`).
    let mut view = crate::keymap::view_from_partial(None, None, None);
    let key = KeySequence::from(KeyChord::new(ModFlags::ALT, Key::Char('y')));
    for mode_name in ["locked", "normal"] {
        view.merged
            .modes
            .get_mut(&ModeName::new(mode_name))
            .expect("built-in mode is merged")
            .defaults
            .insert(
                key.clone(),
                BoundAction {
                    action: ActionRef::core("new-tab").expect("valid name"),
                    args: ActionArgs::None,
                },
            );
    }
    let rendered = render_keys_describe(&view, "<A-y>", FormatArg::Table)
        .expect("sequence parses")
        .expect("bound in two modes");
    let expected = "\
key: <A-y>
mode: locked
action: core:new-tab
display_name: New Tab
description: Create a new tab
scope: tab
args: -
source: defaults
continuous: false

key: <A-y>
mode: normal
action: core:new-tab
display_name: New Tab
description: Create a new tab
scope: tab
args: -
source: defaults
continuous: false
";
    assert_eq!(rendered, expected);
}

#[test]
fn keys_list_scope_filter_for_defaults_keeps_only_shipped_bindings() {
    let view = crate::keymap::view_from_partial(None, None, None);
    let rendered = render_keys_list(&view, None, Some(ScopeArg::Default), FormatArg::Json);
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    let bindings = value["bindings"].as_array().expect("array");
    assert!(!bindings.is_empty(), "defaults exist: {rendered}");
    assert!(
        bindings
            .iter()
            .all(|binding| binding["source"] == serde_json::json!("defaults")),
        "the defaults filter keeps only defaults: {rendered}"
    );
}

#[test]
fn keys_list_scope_filter_for_session_or_layout_is_empty_offline() {
    // No session or layout layer is visible offline, so filtering to either
    // leaves an empty listing: the table is just its header row.
    let view = crate::keymap::view_from_partial(None, None, None);
    let header = "mode  key  action  source\n";
    assert_eq!(
        render_keys_list(&view, None, Some(ScopeArg::Session), FormatArg::Table),
        header
    );
    assert_eq!(
        render_keys_list(&view, None, Some(ScopeArg::Layout), FormatArg::Table),
        header
    );
}

#[test]
fn keys_validate_checked_carries_the_conflict_findings() {
    // A binding on an unregistered action is an orphan warning; the file still
    // applies, and the answer carries the finding on both formats.
    let view = view_with_binding("<C-y>", "core:not-a-real-action");
    let applies = !view.reverted;
    let checked = crate::keymap::ValidationOutcome::Checked {
        report: view.report,
        applies,
    };
    let value: serde_json::Value =
        serde_json::from_str(&render_keys_validate(&checked, FormatArg::Json)).expect("valid JSON");
    assert_eq!(value["valid"], serde_json::json!(true));
    assert_eq!(value["applies"], serde_json::json!(true));
    assert_eq!(value["errors"], serde_json::json!([]));
    let findings = value["findings"].as_array().expect("array");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["severity"], serde_json::json!("warning"));

    let table_rendered = render_keys_validate(&checked, FormatArg::Table);
    let lines: Vec<&str> = table_rendered.lines().collect();
    assert_eq!(lines[0], "valid: a reload would apply this file");
    assert_eq!(
        lines[1].split_whitespace().collect::<Vec<_>>(),
        ["severity", "finding"]
    );
    assert_eq!(lines[2].split_whitespace().next(), Some("warning"));
}

// --- Entity inspect (single-item) renderings ---

#[test]
fn session_inspect_renders_as_field_lines() {
    let expected = "\
id: session-00000000-0000-0000-0000-000000000001
name: quiet-lake
created_at: 1234
clients: 1
panes: 3
";
    assert_eq!(render_session(&session_info(), FormatArg::Table), expected);
}

#[test]
fn tab_inspect_renders_as_field_lines() {
    let expected = "\
id: tab-00000000-0000-0000-0000-000000000001
session: session-00000000-0000-0000-0000-000000000001
name: amber-fox
index: 1
active_pane: pane-00000000-0000-0000-0000-000000000001
panes: 2
";
    assert_eq!(render_tab(&tab_info(), FormatArg::Table), expected);
}

#[test]
fn pane_inspect_renders_as_field_lines() {
    let expected = "\
id: pane-00000000-0000-0000-0000-000000000001
tab: tab-00000000-0000-0000-0000-000000000001
session: session-00000000-0000-0000-0000-000000000001
title: htop
cwd: /home/user
command: htop --tree
state: running
focused_by: 1
";
    assert_eq!(render_pane(&pane_info(), FormatArg::Table), expected);
}

#[test]
fn client_list_table_widens_columns_to_the_widest_row() {
    // Two clients in differently named sessions, so the `session_name`
    // column widens to the longer name and the shorter cell pads out.
    let longer = ClientRow {
        session_name: "wandering-heron".to_string(),
        ..client_row()
    };
    let expected = "\
id                                           session                                       session_name
client-00000000-0000-0000-0000-000000000001  session-00000000-0000-0000-0000-000000000001  quiet-lake
client-00000000-0000-0000-0000-000000000001  session-00000000-0000-0000-0000-000000000001  wandering-heron
";
    assert_eq!(
        render_clients(&[client_row(), longer], FormatArg::Table),
        expected
    );
}

#[test]
fn empty_client_list_table_is_just_the_header() {
    assert_eq!(
        render_clients(&[], FormatArg::Table),
        "id  session  session_name\n"
    );
}

#[test]
fn explain_of_an_empty_or_blank_action_name_is_none() {
    assert_eq!(render_action_explain("", FormatArg::Json), None);
    assert_eq!(render_action_explain("   ", FormatArg::Json), None);
}
