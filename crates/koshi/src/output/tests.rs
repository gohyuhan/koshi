//! Tests for CLI output rendering — discovery, action and keymap
//! introspection, and the `debug` dumps: exact JSON schema snapshots (the
//! stable scripting surface) and exact table/field renderings, all over fixed
//! fake data.

use koshi_core::client::ClientOrigin;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use koshi_config::types::{BoundAction, ModeName};
use koshi_core::action::{
    build_core_action_seeds, ActionHandlerReference, ActionReference, ActionScope, ActionStatus,
    TargetKind,
};
use koshi_core::discovery::{
    ClientDiscovery, PaneDiscovery, PaneLifecycle, SessionDiscovery, TabDiscovery,
};
use koshi_core::event::{
    Event, PaneCreated, PaneEnterPressed, PaneTyped, SubmittedLinePayload, TypedPayload,
};
use koshi_core::geometry::{PaneArea, Point, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, PluginId, SessionId, TabId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::lock::LockMode;
use koshi_core::recent_event::{self, RecentEvent};
use koshi_core::resolve::ActionArgs;
use koshi_ipc::layout::{ClientFocus, SessionLayout, SolvedPane, SolvedTab, TabLayout};
use koshi_layout::mode::LayoutMode;
use koshi_layout::size::SizeWeight;
use koshi_layout::solver::StackHeader;
use koshi_layout::tree::{LayoutNode, SplitNode};
use uuid::Uuid;

use super::*;
use crate::cli::OutputFormat;

/// The fixed UUID every fake id uses, so snapshots are byte-stable.
fn build_fixed_test_uuid() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// A fixed timestamp: 1234 seconds after the Unix epoch.
fn build_fixed_test_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1234)
}

fn build_test_session_discovery() -> SessionDiscovery {
    SessionDiscovery {
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        session_name: "quiet-lake".to_string(),
        created_at: build_fixed_test_time(),
        attached_client_ids: vec![ClientId::from_uuid(build_fixed_test_uuid())],
        pane_count: 3,
    }
}

fn build_test_tab_discovery() -> TabDiscovery {
    TabDiscovery {
        tab_id: TabId::from_uuid(build_fixed_test_uuid()),
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        tab_name: "amber-fox".to_string(),
        tab_index: 1,
        active_pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
        pane_count: 2,
    }
}

fn build_test_pane_discovery() -> PaneDiscovery {
    PaneDiscovery {
        pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
        tab_id: TabId::from_uuid(build_fixed_test_uuid()),
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        pane_title: Some("htop".to_string()),
        working_directory: Some(PathBuf::from("/home/user")),
        command_argv: Some(vec!["htop".to_string(), "--tree".to_string()]),
        lifecycle: PaneLifecycle::Running,
        focused_by_client_ids: vec![ClientId::from_uuid(build_fixed_test_uuid())],
    }
}

fn build_test_session_row() -> SessionRow {
    SessionRow {
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        session_name: "quiet-lake".to_string(),
        server_name_or_address: None,
    }
}

fn build_test_tab_row() -> TabRow {
    TabRow {
        tab_id: TabId::from_uuid(build_fixed_test_uuid()),
        tab_name: "amber-fox".to_string(),
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        session_name: "quiet-lake".to_string(),
    }
}

fn build_test_pane_row() -> PaneRow {
    PaneRow {
        pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
        pane_name: Some("htop".to_string()),
        tab_id: TabId::from_uuid(build_fixed_test_uuid()),
        tab_name: "amber-fox".to_string(),
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        session_name: "quiet-lake".to_string(),
    }
}

fn build_test_client_row() -> ClientRow {
    ClientRow {
        client_id: ClientId::from_uuid(build_fixed_test_uuid()),
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        session_name: "quiet-lake".to_string(),
    }
}

fn build_test_client_discovery() -> ClientDiscovery {
    ClientDiscovery {
        client_id: ClientId::from_uuid(build_fixed_test_uuid()),
        session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        attached_at: build_fixed_test_time(),
        viewport_size: Size {
            column_count: 120,
            row_count: 40,
        },
        active_tab_id: TabId::from_uuid(build_fixed_test_uuid()),
        focused_pane_id: None,
        lock_mode: LockMode::Normal,
        origin: Some(ClientOrigin::Local),
        pane_area: None,
    }
}

// --- JSON schema snapshots ---

#[test]
fn session_json_schema_is_stable() {
    let expected_text = r#"{
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
    assert_eq!(
        render_session(&build_test_session_discovery(), OutputFormat::Json),
        expected_text
    );
}

#[test]
fn session_list_json_is_an_array_of_id_name_and_server() {
    let expected_text = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "name": "quiet-lake",
    "server": null
  }
]
"#;
    assert_eq!(
        render_sessions(&[build_test_session_row()], OutputFormat::Json),
        expected_text
    );
}

#[test]
fn session_list_json_names_the_server_of_a_remote_row() {
    let mut remote_session_row = build_test_session_row();
    remote_session_row.server_name_or_address = Some("desk".to_string());
    let expected_text = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "name": "quiet-lake",
    "server": "desk"
  }
]
"#;
    assert_eq!(
        render_sessions(&[remote_session_row], OutputFormat::Json),
        expected_text
    );
}

#[test]
fn tab_list_json_carries_the_owning_session() {
    let expected_text = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "name": "amber-fox",
    "session": "00000000-0000-0000-0000-000000000001",
    "session_name": "quiet-lake"
  }
]
"#;
    assert_eq!(
        render_tabs(&[build_test_tab_row()], OutputFormat::Json),
        expected_text
    );
}

#[test]
fn pane_list_json_carries_the_whole_id_chain() {
    let expected_text = r#"[
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
    assert_eq!(
        render_panes(&[build_test_pane_row()], OutputFormat::Json),
        expected_text
    );
}

#[test]
fn an_untitled_pane_lists_a_null_name_in_json() {
    let pane = PaneRow {
        pane_name: None,
        ..build_test_pane_row()
    };
    let rendered_text = render_panes(&[pane], OutputFormat::Json);
    assert!(
        rendered_text.contains("\"name\": null,"),
        "unexpected name form: {rendered_text}"
    );
}

#[test]
fn client_list_json_carries_the_owning_session() {
    let expected_text = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "session": "00000000-0000-0000-0000-000000000001",
    "session_name": "quiet-lake"
  }
]
"#;
    assert_eq!(
        render_clients(&[build_test_client_row()], OutputFormat::Json),
        expected_text
    );
}

#[test]
fn tab_json_schema_is_stable() {
    let expected_text = r#"{
  "id": "00000000-0000-0000-0000-000000000001",
  "session_id": "00000000-0000-0000-0000-000000000001",
  "name": "amber-fox",
  "index": 1,
  "active_pane": "00000000-0000-0000-0000-000000000001",
  "pane_count": 2
}
"#;
    assert_eq!(
        render_tab(&build_test_tab_discovery(), OutputFormat::Json),
        expected_text
    );
}

#[test]
fn pane_json_schema_is_stable() {
    let expected_text = r#"{
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
    assert_eq!(
        render_pane(&build_test_pane_discovery(), OutputFormat::Json),
        expected_text
    );
}

#[test]
fn non_utf8_cwd_renders_lossily_in_json() {
    let mut pane = build_test_pane_discovery();
    pane.working_directory = Some(build_non_utf8_path());
    let expected_text = r#"{
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
    assert_eq!(render_pane(&pane, OutputFormat::Json), expected_text);
}

/// A path containing bytes that are not valid UTF-8; its lossy form is
/// `/tmp/f\u{FFFD}oo` on every platform.
fn build_non_utf8_path() -> PathBuf {
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
    let mut pane = build_test_pane_discovery();
    pane.lifecycle = PaneLifecycle::Exited { exit_code: Some(0) };
    let rendered_text = render_pane(&pane, OutputFormat::Json);
    assert!(
        rendered_text.contains("\"state\": {\n    \"exited\": {\n      \"code\": 0\n    }\n  }"),
        "unexpected state form: {rendered_text}"
    );
}

#[test]
fn client_json_schema_is_stable() {
    let expected_text = r#"{
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
  "lock_state": "Normal",
  "origin": "Local",
  "pane_area": null
}
"#;
    assert_eq!(
        render_client(&build_test_client_discovery(), OutputFormat::Json),
        expected_text
    );
}

#[test]
fn a_reported_pane_area_json_is_a_tagged_size() {
    let reported_client = ClientDiscovery {
        pane_area: Some(PaneArea::Reported(Size {
            column_count: 100,
            row_count: 30,
        })),
        ..build_test_client_discovery()
    };

    let rendered_text = render_client(&reported_client, OutputFormat::Json);

    assert!(
        rendered_text.contains(
            "\"pane_area\": {\n    \"Reported\": {\n      \"cols\": 100,\n      \"rows\": 30\n    }\n  }"
        ),
        "unexpected pane_area form: {rendered_text}"
    );
}

// --- Table renderings ---

#[test]
fn session_table_marks_where_each_session_runs() {
    let mut remote_session_row = build_test_session_row();
    remote_session_row.server_name_or_address = Some("desk".to_string());
    let expected_text = "\
id                                            name        server
session-00000000-0000-0000-0000-000000000001  quiet-lake  local
session-00000000-0000-0000-0000-000000000001  quiet-lake  desk
";
    assert_eq!(
        render_sessions(
            &[build_test_session_row(), remote_session_row],
            OutputFormat::Table,
        ),
        expected_text
    );
}

#[test]
fn empty_list_table_is_just_the_header() {
    assert_eq!(
        render_sessions(&[], OutputFormat::Table),
        "id  name  server\n"
    );
}

#[test]
fn tab_table_names_the_owning_session() {
    let expected_text = "\
id                                        name       session                                       session_name
tab-00000000-0000-0000-0000-000000000001  amber-fox  session-00000000-0000-0000-0000-000000000001  quiet-lake
";
    assert_eq!(
        render_tabs(&[build_test_tab_row()], OutputFormat::Table),
        expected_text
    );
}

#[test]
fn pane_table_names_the_owning_tab_and_session() {
    let expected_text = "\
id                                         name  tab                                       tab_name   session                                       session_name
pane-00000000-0000-0000-0000-000000000001  htop  tab-00000000-0000-0000-0000-000000000001  amber-fox  session-00000000-0000-0000-0000-000000000001  quiet-lake
";
    assert_eq!(
        render_panes(&[build_test_pane_row()], OutputFormat::Table),
        expected_text
    );
}

#[test]
fn an_untitled_pane_lists_a_dash_for_its_name() {
    let pane = PaneRow {
        pane_name: None,
        ..build_test_pane_row()
    };
    let rendered_text = render_panes(&[pane], OutputFormat::Table);
    let rendered_row = rendered_text.lines().nth(1).expect("one data row");
    let rendered_cells: Vec<&str> = rendered_row.split_whitespace().collect();
    assert_eq!(
        rendered_cells,
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
    let mut pane = build_test_pane_discovery();
    pane.pane_title = None;
    pane.working_directory = None;
    pane.command_argv = None;
    pane.lifecycle = PaneLifecycle::Exited { exit_code: None };
    let rendered_text = render_pane(&pane, OutputFormat::Table);
    assert_eq!(
        rendered_text,
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
    let expected_text = "\
id: client-00000000-0000-0000-0000-000000000001
session: session-00000000-0000-0000-0000-000000000001
attached_at: 1234
viewport: 120x40
pane_area: -
active_tab: tab-00000000-0000-0000-0000-000000000001
focused_pane: -
lock: Normal
";
    assert_eq!(
        render_client(&build_test_client_discovery(), OutputFormat::Table),
        expected_text
    );
}

#[test]
fn a_starving_client_prints_starving_in_the_pane_area_column() {
    let starving_client = ClientDiscovery {
        pane_area: Some(PaneArea::Starving),
        ..build_test_client_discovery()
    };

    let rendered_text = render_client(&starving_client, OutputFormat::Table);

    assert!(
        rendered_text.contains("\npane_area: starving\n"),
        "unexpected pane_area line: {rendered_text}"
    );
}

#[test]
fn a_reported_pane_area_prints_as_cols_by_rows() {
    let reported_client = ClientDiscovery {
        pane_area: Some(PaneArea::Reported(Size {
            column_count: 100,
            row_count: 30,
        })),
        ..build_test_client_discovery()
    };

    let rendered_text = render_client(&reported_client, OutputFormat::Table);

    assert!(
        rendered_text.contains("\npane_area: 100x30\n"),
        "unexpected pane_area line: {rendered_text}"
    );
}

// --- Action introspection ---

/// The count of seeded actions the runtime supports today.
fn count_available_seeded_actions() -> usize {
    build_core_action_seeds()
        .iter()
        .filter(|(_, metadata)| metadata.action_status == ActionStatus::Available)
        .count()
}

#[test]
fn actions_list_table_shows_only_supported_actions() {
    let rendered_text = render_actions_list(OutputFormat::Table);
    let rendered_lines: Vec<&str> = rendered_text.lines().collect();
    assert_eq!(rendered_lines.len(), count_available_seeded_actions() + 1);
    assert_eq!(
        rendered_lines[0].split_whitespace().collect::<Vec<_>>(),
        vec!["action", "command", "scope"]
    );
    // The first supported action is new-pane.
    assert_eq!(
        rendered_lines[1].split_whitespace().collect::<Vec<_>>(),
        vec!["core:new-pane", "NewPane", "pane-session"]
    );
    // Coming-soon actions never appear.
    assert!(
        !rendered_text.contains("copy-selection") && !rendered_text.contains("plugin-"),
        "coming-soon actions leaked into the list:\n{rendered_text}"
    );
}

#[test]
fn actions_list_json_is_an_array_of_supported_summaries() {
    let rendered_text = render_actions_list(OutputFormat::Json);
    assert!(
        rendered_text.starts_with("[\n"),
        "not an array: {rendered_text}"
    );
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    let action_records = json_document.as_array().expect("a JSON array");
    assert_eq!(action_records.len(), count_available_seeded_actions());
    assert_eq!(action_records[0]["action"], "core:new-pane");
    assert_eq!(action_records[0]["command"], "NewPane");
    assert_eq!(action_records[0]["scope"], "pane-session");
    assert!(
        !rendered_text.contains("copy-selection") && !rendered_text.contains("plugin-"),
        "coming-soon actions leaked into JSON:\n{rendered_text}"
    );
}

#[test]
fn explain_new_pane_fields_are_exact() {
    let expected_text = "\
action: core:new-pane
display_name: New Pane
description: Split the focused pane and start a shell in the new one
scope: pane-session
targets: pane
command: NewPane
examples: core:new-pane, koshi new-pane
";
    assert_eq!(
        render_action_explain("new-pane", OutputFormat::Table),
        Some(expected_text.to_string())
    );
}

#[test]
fn explain_new_pane_json_is_exact() {
    let expected_text = r#"{
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
        render_action_explain("new-pane", OutputFormat::Json),
        Some(expected_text.to_string())
    );
}

#[test]
fn explain_accepts_a_full_core_ref() {
    assert_eq!(
        render_action_explain("core:new-pane", OutputFormat::Json),
        render_action_explain("new-pane", OutputFormat::Json),
    );
}

#[test]
fn explain_run_omits_the_koshi_example() {
    // run is supported but `koshi run` needs a command, so no CLI example is
    // shown — only the config reference.
    let expected_text = r#"{
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
        render_action_explain("run", OutputFormat::Json),
        Some(expected_text.to_string())
    );
}

#[test]
fn explain_of_a_coming_soon_action_is_hidden() {
    // The selection and plugin actions are registered but have no
    // runtime handler yet, so explain treats them as unknown — by bare name and
    // by full ref. These are seeded actions on purpose: an unregistered name is
    // hidden_session_overview too, but for a different reason, which
    // `explain_of_an_unknown_action_is_none` covers.
    assert_eq!(
        render_action_explain("copy-selection", OutputFormat::Json),
        None
    );
    assert_eq!(
        render_action_explain("core:copy-selection", OutputFormat::Json),
        None
    );
    assert_eq!(
        render_action_explain("plugin-install", OutputFormat::Json),
        None
    );
}

#[test]
fn explain_of_an_unknown_action_is_none() {
    assert_eq!(
        render_action_explain("does-not-exist", OutputFormat::Json),
        None
    );
}

#[test]
fn explain_renders_multiple_targets_joined() {
    // focus-pane targets a pane and a client; both join into one cell. It needs
    // a --pane flag, so no bare CLI example is shown.
    let expected_text = "\
action: core:focus-pane
display_name: Focus Pane
description: Move the issuing client's focus to a pane
scope: client
targets: pane, client
command: FocusPane
examples: core:focus-pane
";
    assert_eq!(
        render_action_explain("focus-pane", OutputFormat::Table),
        Some(expected_text.to_string())
    );
}

#[test]
fn an_empty_target_list_renders_as_a_dash() {
    // Every supported action has at least one target today, so exercise the
    // join helper directly to keep the empty branch covered.
    assert_eq!(render_joined_text_cell(&[]), "-");
    assert_eq!(
        render_joined_text_cell(&["pane".to_string(), "client".to_string()]),
        "pane, client"
    );
}

// --- Cell helpers not reachable through the fixed fake data above ---

#[test]
fn format_pane_state_cell_renders_spawning_and_closing() {
    // Running and both Exited forms are covered via the pane table tests
    // above; Spawning and Closing are not exercised by any fixed fixture.
    assert_eq!(format_pane_state_cell(PaneLifecycle::Spawning), "spawning");
    assert_eq!(format_pane_state_cell(PaneLifecycle::Closing), "closing");
}

#[test]
fn format_time_cell_before_the_unix_epoch_renders_as_a_dash() {
    // `duration_since` fails for a time earlier than the epoch; the cell
    // falls back to "-" rather than panicking or underflowing.
    let before_epoch = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
    assert_eq!(format_time_cell(before_epoch), "-");
}

#[test]
fn format_scope_label_renders_tab_and_global() {
    // PaneSession and Client are covered indirectly by the `new-pane` and
    // `focus-pane` explain tests above; Tab and Global are not.
    assert_eq!(format_scope_label(ActionScope::Tab), "tab");
    assert_eq!(format_scope_label(ActionScope::Global), "global");
}

#[test]
fn format_target_label_renders_session_and_tab() {
    // Pane and Client are covered indirectly by the `focus-pane` explain test
    // above; Session and Tab are not.
    assert_eq!(format_target_label(TargetKind::Session), "session");
    assert_eq!(format_target_label(TargetKind::Tab), "tab");
}

#[test]
fn format_command_label_renders_plugin_host_and_sequence() {
    // Every seeded core action dispatches through `CoreCommand`, so the
    // plugin-host and sequence handler kinds are never reachable through
    // `render_actions_list`/`render_action_explain` today; exercise the
    // helper directly so those two arms stay covered.
    assert_eq!(
        format_command_label(&ActionHandlerReference::PluginHostCall(PluginId::new())),
        "plugin-host"
    );
    assert_eq!(
        format_command_label(&ActionHandlerReference::Sequence(vec![])),
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
        render_table(&["name"], vec![vec!["文字文字".to_string()]]),
        "name\n文字文字\n"
    );
}

#[test]
fn explain_new_tab_reports_tab_scope_and_target() {
    // `new-tab` is seeded with `ActionScope::Tab` and `TargetKind::Tab`,
    // neither of which any other explain test exercises end-to-end.
    let expected_text = "\
action: core:new-tab
display_name: New Tab
description: Create a new tab
scope: tab
targets: tab
command: NewTab
examples: core:new-tab, koshi new-tab
";
    assert_eq!(
        render_action_explain("new-tab", OutputFormat::Table),
        Some(expected_text.to_string())
    );
}

#[test]
fn explain_quit_reports_its_client_scope_and_both_target_kinds() {
    // `quit` is seeded with `ActionScope::Client` and both `ClientTarget`
    // and `Session` targets, so it exercises the session target label no
    // other explain test covers end-to-end.
    let expected_text = "\
action: core:quit
display_name: Quit
description: Leave the session, ending it when auto-close-session is on and no other client stays
scope: client
targets: client, session
command: Quit
examples: core:quit
";
    assert_eq!(
        render_action_explain("quit", OutputFormat::Table),
        Some(expected_text.to_string())
    );
}

// --- Keys rendering ---

/// Parse a test key sequence with the default leader and depth.
fn parse_test_key_sequence(key_sequence: &str) -> koshi_core::key::KeySequence {
    koshi_config::key_sequence::parse_sequence(
        key_sequence,
        koshi_config::types::KeybindingsConfig::default().leader,
        8,
    )
    .expect("test sequence parses")
}

/// Build the offline view for one `normal`-mode user binding.
fn build_test_keymap_view_with_binding(
    key_text: &str,
    action_name: &str,
) -> crate::keymap::KeymapView {
    use std::collections::BTreeMap;
    use std::str::FromStr;
    let mut key_binding_by_sequence = BTreeMap::new();
    key_binding_by_sequence.insert(
        parse_test_key_sequence(key_text),
        koshi_config::types::BoundAction {
            action_reference: koshi_core::action::ActionReference::from_str(action_name)
                .expect("valid ref"),
            action_arguments: koshi_core::resolve::ActionArgs::None,
        },
    );
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        koshi_config::types::ModeName::from_text("normal"),
        koshi_config::types::ModeBindings {
            bound_action_by_key_sequence: key_binding_by_sequence,
            removed_key_sequences: Default::default(),
        },
    );
    crate::keymap::build_keymap_view_from_partial(
        Some(koshi_config::layer::PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..Default::default()
        }),
        None,
        None,
    )
}

#[test]
fn keys_list_shows_a_steal_and_its_unbound_default() {
    let keymap_view = build_test_keymap_view_with_binding("<A-f>", "core:close-pane");
    let rendered_text = render_keys_list(&keymap_view, Some("normal"), None, OutputFormat::Json);
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    let binding_records = json_document["bindings"].as_array().expect("array");
    assert!(
        binding_records.contains(&serde_json::json!({
            "mode": "normal",
            "key": "<A-f>",
            "action": "core:close-pane",
            "source": "user",
        })),
        "got: {rendered_text}"
    );
    assert!(
        binding_records.contains(&serde_json::json!({
            "mode": "normal",
            "key": "<A-f>",
            "action": "core:toggle-pane-fullscreen",
            "source": "defaults (unbound)",
        })),
        "got: {rendered_text}"
    );
}

#[test]
fn keys_list_scope_filter_keeps_only_the_named_layer() {
    let keymap_view = build_test_keymap_view_with_binding("<C-y>", "core:new-tab");
    let rendered_text = render_keys_list(
        &keymap_view,
        None,
        Some(KeymapScope::User),
        OutputFormat::Table,
    );
    let rendered_lines: Vec<&str> = rendered_text.lines().collect();
    assert_eq!(
        rendered_lines.len(),
        2,
        "header plus the one user row: {rendered_text}"
    );
    assert_eq!(rendered_lines[1], "normal  <C-y>  core:new-tab  user");
}

#[test]
fn keys_list_mode_filter_keeps_only_the_named_mode() {
    let keymap_view = crate::keymap::build_keymap_view_from_partial(None, None, None);
    let rendered_text = render_keys_list(&keymap_view, Some("locked"), None, OutputFormat::Json);
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    assert_eq!(json_document["reverted"], serde_json::json!(false));
    let binding_records = json_document["bindings"].as_array().expect("array");
    assert!(!binding_records.is_empty());
    assert!(binding_records
        .iter()
        .all(|binding| binding["mode"] == serde_json::json!("locked")));
}

#[test]
fn keys_recommended_is_empty_until_plugins_exist() {
    assert_eq!(render_keys_recommended(OutputFormat::Json), "[]\n");
    assert_eq!(
        render_keys_recommended(OutputFormat::Table),
        "key  action  plugin\n"
    );
}

#[test]
fn keys_describe_renders_the_binding_and_source() {
    let keymap_view = crate::keymap::build_keymap_view_from_partial(None, None, None);
    let rendered_text = render_keys_describe(&keymap_view, "<C-p> x", OutputFormat::Table)
        .expect("sequence parses")
        .expect("bound in normal mode");
    let expected_text = "\
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
    assert_eq!(rendered_text, expected_text);
}

#[test]
fn keys_describe_renders_system_authored_args_as_json() {
    // No shipped binding carries arguments; system-authored layers (plugin
    // manifests) may. Build that state directly to pin the args rendering.
    let mut keymap_view = crate::keymap::build_keymap_view_from_partial(None, None, None);
    let key_sequence = KeySequence::from(KeyChord::from_parts(ModFlags::ALT, Key::Char('r')));
    keymap_view
        .merged_keymap
        .mode_map_by_name
        .get_mut(&ModeName::from_text("normal"))
        .expect("normal mode is merged")
        .default_bindings_by_key_sequence
        .insert(
            key_sequence,
            BoundAction {
                action_reference: ActionReference::from_core_action_name("run")
                    .expect("valid name"),
                action_arguments: ActionArgs::Run {
                    program: PathBuf::from("/usr/bin/htop"),
                    arguments: vec!["--tree".to_string()],
                    direction: None,
                    should_stack: false,
                },
            },
        );
    let rendered_text = render_keys_describe(&keymap_view, "<A-r>", OutputFormat::Json)
        .expect("sequence parses")
        .expect("bound in normal mode");
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    assert_eq!(json_document[0]["action"], serde_json::json!("core:run"));
    assert_eq!(
        json_document[0]["args"],
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
    let keymap_view = crate::keymap::build_keymap_view_from_partial(None, None, None);
    let rendered_text = render_keys_describe(&keymap_view, "<A-f>", OutputFormat::Json)
        .expect("sequence parses")
        .expect("bound in normal mode");
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    assert_eq!(json_document[0]["args"], serde_json::Value::Null);
    assert_eq!(
        json_document[0]["action"],
        serde_json::json!("core:toggle-pane-fullscreen")
    );
}

#[test]
fn keys_describe_reports_unbound_and_malformed_sequences() {
    let keymap_view = crate::keymap::build_keymap_view_from_partial(None, None, None);
    assert_eq!(
        render_keys_describe(&keymap_view, "<C-z>", OutputFormat::Table),
        Ok(None)
    );
    assert_eq!(
        render_keys_describe(&keymap_view, "Ctrl-g", OutputFormat::Table),
        Err(
            "invalid key `Ctrl-g`: a multi-character key must be bracketed, as in `<Tab>`"
                .to_string()
        )
    );
}

#[test]
fn keys_describe_reports_the_user_entry_alone_when_it_displaced_a_default() {
    let keymap_view = build_test_keymap_view_with_binding("<A-f>", "core:close-pane");
    let rendered_text = render_keys_describe(&keymap_view, "<A-f>", OutputFormat::Json)
        .expect("sequence parses")
        .expect("bound in normal mode");
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    let action_details = json_document.as_array().expect("a JSON array");

    assert_eq!(action_details.len(), 1);
    assert_eq!(
        action_details[0]["action"],
        serde_json::json!("core:close-pane")
    );
    assert_eq!(action_details[0]["source"], serde_json::json!("user"));
}

#[test]
fn keys_conflicts_renders_the_verdict_and_findings() {
    // Binding an unregistered action is an orphan warning; the verdict
    // still applies.
    let keymap_view = build_test_keymap_view_with_binding("<C-y>", "core:not-a-real-action");
    let rendered_text = render_keys_conflicts(&keymap_view, OutputFormat::Json);
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    assert_eq!(json_document["verdict"], serde_json::json!("apply"));
    assert_eq!(json_document["file_error"], serde_json::Value::Null);
    let findings = json_document["findings"].as_array().expect("array");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["severity"], serde_json::json!("warning"));
}

#[test]
fn keys_conflicts_carries_an_ignored_file_on_both_formats() {
    // An unparseable file leaves the defaults running; the answer itself
    // says so, so a stdout-only consumer never mistakes it for a clean file.
    let keymap_view =
        crate::keymap::build_keymap_view_from_partial(None, None, Some("boom".to_string()));
    let table_rendered = render_keys_conflicts(&keymap_view, OutputFormat::Table);
    assert_eq!(table_rendered, "file: ignored (boom)\nverdict: apply\n");
    let json_document: serde_json::Value =
        serde_json::from_str(&render_keys_conflicts(&keymap_view, OutputFormat::Json))
            .expect("valid JSON");
    assert_eq!(json_document["file_error"], serde_json::json!("boom"));
    assert_eq!(json_document["verdict"], serde_json::json!("apply"));
}

#[test]
fn keys_validate_renders_both_outcome_shapes() {
    let failed = crate::keymap::KeymapValidationOutcome::ParseFailed(vec!["bad node".to_string()]);
    assert_eq!(
        render_keys_validate(&failed, OutputFormat::Table),
        "invalid: the file does not parse\nerror: bad node\n"
    );
    let failed_json: serde_json::Value =
        serde_json::from_str(&render_keys_validate(&failed, OutputFormat::Json))
            .expect("valid JSON");
    assert_eq!(
        failed_json,
        serde_json::json!({
            "valid": false,
            "applies": false,
            "errors": ["bad node"],
            "findings": [],
        })
    );

    let clean_keymap_view = crate::keymap::build_keymap_view_from_partial(None, None, None);
    let checked = crate::keymap::KeymapValidationOutcome::Checked {
        report: clean_keymap_view.report,
        is_applicable: true,
    };
    assert_eq!(
        render_keys_validate(&checked, OutputFormat::Table),
        "valid: a reload would apply this file\n"
    );
    assert!(does_validation_apply(&checked));
    assert!(!does_validation_apply(&failed));
}

/// The offline view for a user file whose `unlock_alternative` sits on a chord
/// plain typing produces, which detection rejects as fatal.
fn build_keymap_view_with_typeable_unlock_alternative() -> crate::keymap::KeymapView {
    crate::keymap::build_keymap_view_from_partial(
        Some(koshi_config::layer::PartialKeybindingsConfig {
            unlock_alternative: Some(Some(KeyChord::from_parts(ModFlags::NONE, Key::Char('u')))),
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
    let keymap_view = build_keymap_view_with_typeable_unlock_alternative();
    let rendered_text = render_keys_conflicts(&keymap_view, OutputFormat::Json);
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    assert_eq!(json_document["verdict"], serde_json::json!("reject"));
    assert_eq!(json_document["file_error"], serde_json::Value::Null);
    let findings = json_document["findings"].as_array().expect("array");
    assert!(
        findings
            .iter()
            .any(|finding| finding["severity"] == serde_json::json!("fatal")),
        "expected a fatal finding: {rendered_text}"
    );
}

#[test]
fn keys_list_marks_a_rejected_user_file_as_reverted() {
    // The rejected file drops the view back to the defaults, so every listed
    // binding is a shipped one and none is sourced to the user layer.
    let keymap_view = build_keymap_view_with_typeable_unlock_alternative();
    let rendered_text = render_keys_list(&keymap_view, None, None, OutputFormat::Json);
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    assert_eq!(json_document["reverted"], serde_json::json!(true));
    let binding_records = json_document["bindings"].as_array().expect("array");
    assert!(
        !binding_records.is_empty(),
        "defaults still list: {rendered_text}"
    );
    assert!(
        binding_records
            .iter()
            .all(|binding| binding["source"] != serde_json::json!("user")),
        "a rejected file must contribute no user bindings: {rendered_text}"
    );
}

#[test]
fn keys_describe_renders_one_field_block_per_mode_the_key_is_bound_in() {
    // The same key bound in two modes prints two field blocks, separated by a
    // blank line, in mode-name order (`locked` before `normal`).
    let mut keymap_view = crate::keymap::build_keymap_view_from_partial(None, None, None);
    let key_sequence = KeySequence::from(KeyChord::from_parts(ModFlags::ALT, Key::Char('y')));
    for mode_name in ["locked", "normal"] {
        keymap_view
            .merged_keymap
            .mode_map_by_name
            .get_mut(&ModeName::from_text(mode_name))
            .expect("built-in mode is merged")
            .default_bindings_by_key_sequence
            .insert(
                key_sequence.clone(),
                BoundAction {
                    action_reference: ActionReference::from_core_action_name("new-tab")
                        .expect("valid name"),
                    action_arguments: ActionArgs::None,
                },
            );
    }
    let rendered_text = render_keys_describe(&keymap_view, "<A-y>", OutputFormat::Table)
        .expect("sequence parses")
        .expect("bound in two modes");
    let expected_text = "\
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
    assert_eq!(rendered_text, expected_text);
}

#[test]
fn keys_list_scope_filter_for_defaults_keeps_only_shipped_bindings() {
    let keymap_view = crate::keymap::build_keymap_view_from_partial(None, None, None);
    let rendered_text = render_keys_list(
        &keymap_view,
        None,
        Some(KeymapScope::Default),
        OutputFormat::Json,
    );
    let json_document: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("valid JSON");
    let binding_records = json_document["bindings"].as_array().expect("array");
    assert!(
        !binding_records.is_empty(),
        "defaults exist: {rendered_text}"
    );
    assert!(
        binding_records
            .iter()
            .all(|binding| binding["source"] == serde_json::json!("defaults")),
        "the defaults filter keeps only defaults: {rendered_text}"
    );
}

#[test]
fn keys_list_scope_filter_for_session_or_layout_is_empty_offline() {
    // No session or layout layer is visible offline, so filtering to either
    // leaves an empty listing: the table is just its header row.
    let keymap_view = crate::keymap::build_keymap_view_from_partial(None, None, None);
    let header = "mode  key  action  source\n";
    assert_eq!(
        render_keys_list(
            &keymap_view,
            None,
            Some(KeymapScope::Session),
            OutputFormat::Table,
        ),
        header
    );
    assert_eq!(
        render_keys_list(
            &keymap_view,
            None,
            Some(KeymapScope::Layout),
            OutputFormat::Table,
        ),
        header
    );
}

#[test]
fn keys_validate_checked_carries_the_conflict_findings() {
    // A binding on an unregistered action is an orphan warning; the file still
    // applies, and the answer carries the finding on both formats.
    let keymap_view = build_test_keymap_view_with_binding("<C-y>", "core:not-a-real-action");
    let is_applicable = !keymap_view.is_reverted_to_defaults;
    let checked = crate::keymap::KeymapValidationOutcome::Checked {
        report: keymap_view.report,
        is_applicable,
    };
    let json_document: serde_json::Value =
        serde_json::from_str(&render_keys_validate(&checked, OutputFormat::Json))
            .expect("valid JSON");
    assert_eq!(json_document["valid"], serde_json::json!(true));
    assert_eq!(json_document["applies"], serde_json::json!(true));
    assert_eq!(json_document["errors"], serde_json::json!([]));
    let findings = json_document["findings"].as_array().expect("array");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["severity"], serde_json::json!("warning"));

    let table_rendered = render_keys_validate(&checked, OutputFormat::Table);
    let rendered_lines: Vec<&str> = table_rendered.lines().collect();
    assert_eq!(rendered_lines[0], "valid: a reload would apply this file");
    assert_eq!(
        rendered_lines[1].split_whitespace().collect::<Vec<_>>(),
        ["severity", "finding"]
    );
    assert_eq!(rendered_lines[2].split_whitespace().next(), Some("warning"));
}

// --- Entity inspect (single-item) renderings ---

#[test]
fn session_inspect_renders_as_field_lines() {
    let expected_text = "\
id: session-00000000-0000-0000-0000-000000000001
name: quiet-lake
created_at: 1234
clients: 1
panes: 3
";
    assert_eq!(
        render_session(&build_test_session_discovery(), OutputFormat::Table),
        expected_text
    );
}

#[test]
fn tab_inspect_renders_as_field_lines() {
    let expected_text = "\
id: tab-00000000-0000-0000-0000-000000000001
session: session-00000000-0000-0000-0000-000000000001
name: amber-fox
index: 1
active_pane: pane-00000000-0000-0000-0000-000000000001
panes: 2
";
    assert_eq!(
        render_tab(&build_test_tab_discovery(), OutputFormat::Table),
        expected_text
    );
}

#[test]
fn pane_inspect_renders_as_field_lines() {
    let expected_text = "\
id: pane-00000000-0000-0000-0000-000000000001
tab: tab-00000000-0000-0000-0000-000000000001
session: session-00000000-0000-0000-0000-000000000001
title: htop
cwd: /home/user
command: htop --tree
state: running
focused_by: 1
";
    assert_eq!(
        render_pane(&build_test_pane_discovery(), OutputFormat::Table),
        expected_text
    );
}

#[test]
fn client_list_table_widens_columns_to_the_widest_row() {
    // Two clients in differently named sessions, so the `session_name`
    // column widens to the longer name and the shorter cell pads out.
    let longer = ClientRow {
        session_name: "wandering-heron".to_string(),
        ..build_test_client_row()
    };
    let expected_text = "\
id                                           session                                       session_name
client-00000000-0000-0000-0000-000000000001  session-00000000-0000-0000-0000-000000000001  quiet-lake
client-00000000-0000-0000-0000-000000000001  session-00000000-0000-0000-0000-000000000001  wandering-heron
";
    assert_eq!(
        render_clients(&[build_test_client_row(), longer], OutputFormat::Table),
        expected_text
    );
}

#[test]
fn empty_client_list_table_is_just_the_header() {
    assert_eq!(
        render_clients(&[], OutputFormat::Table),
        "id  session  session_name\n"
    );
}

#[test]
fn explain_of_an_empty_or_blank_action_name_is_none() {
    assert_eq!(render_action_explain("", OutputFormat::Json), None);
    assert_eq!(render_action_explain("   ", OutputFormat::Json), None);
}

// --- Debug dumps ---

/// A fixed UUID ending in `suffix_byte`, so the ids inside one dump stay
/// distinguishable.
fn build_test_uuid_with_suffix(suffix_byte: u8) -> Uuid {
    Uuid::parse_str(&format!(
        "00000000-0000-0000-0000-0000000000{suffix_byte:02}"
    ))
    .expect("literal UUID parses")
}

/// One session's whole record: itself, one tab, one pane, and one client.
fn build_test_session_overview() -> SessionOverview {
    SessionOverview {
        session: build_test_session_discovery(),
        tabs: vec![build_test_tab_discovery()],
        panes: vec![build_test_pane_discovery()],
        clients: vec![build_test_client_discovery()],
    }
}

/// The session id every layout fixture below carries.
fn build_test_layout_session_id() -> SessionId {
    SessionId::from_uuid(build_test_uuid_with_suffix(1))
}

/// The tab id every layout fixture below carries.
fn build_test_layout_tab_id() -> TabId {
    TabId::from_uuid(build_test_uuid_with_suffix(2))
}

/// The client id every layout fixture below carries.
fn build_test_layout_client_id() -> ClientId {
    ClientId::from_uuid(build_test_uuid_with_suffix(3))
}

/// The first pane of every layout fixture below.
fn build_test_first_pane_id() -> PaneId {
    PaneId::from_uuid(build_test_uuid_with_suffix(4))
}

/// The second pane of every layout fixture below.
fn build_test_second_pane_id() -> PaneId {
    PaneId::from_uuid(build_test_uuid_with_suffix(5))
}

/// A layout of one session holding one tab with `layout_tree`, solved as
/// `solved_tabs`, viewed by one client focused on `focused_pane_id`.
fn build_test_session_layout(
    layout_tree: LayoutNode,
    solved_tabs: Vec<SolvedTab>,
    focused_pane_id: Option<PaneId>,
) -> SessionLayout {
    SessionLayout {
        session_id: build_test_layout_session_id(),
        session_name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            tab_id: build_test_layout_tab_id(),
            tab_name: "editor".to_string(),
            tab_index: 0,
            layout_tree,
            solved_tabs,
        }],
        clients: vec![ClientFocus {
            client_id: build_test_layout_client_id(),
            active_tab_id: build_test_layout_tab_id(),
            focused_pane_id,
        }],
    }
}

/// A left-right split of the two fixture panes.
fn build_test_horizontal_split() -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(build_test_first_pane_id()),
            LayoutNode::Pane(build_test_second_pane_id()),
        ],
    ))
}

/// One client's tiled solve of [`build_test_horizontal_split`] over an 80x22 tab.
fn build_test_tiled_solve() -> SolvedTab {
    SolvedTab {
        client_id: build_test_layout_client_id(),
        viewport_size: Size {
            column_count: 80,
            row_count: 22,
        },
        layout_mode: LayoutMode::Tiled,
        pane_rects: vec![
            SolvedPane {
                pane_id: build_test_first_pane_id(),
                outer_rect: Rect::from_size_at_origin(Size {
                    column_count: 40,
                    row_count: 22,
                }),
            },
            SolvedPane {
                pane_id: build_test_second_pane_id(),
                outer_rect: Rect::from_origin_and_size(
                    Point { column: 40, row: 0 },
                    Size {
                        column_count: 40,
                        row_count: 22,
                    },
                ),
            },
        ],
        suppressed_pane_ids: Vec::new(),
        is_every_pane_suppressed: false,
        stack_headers: Vec::new(),
    }
}

#[test]
fn dump_state_table_prints_one_named_table_per_record_kind() {
    // Each section is its own table, and a blank line closes it.
    let expected_text = "\
sessions
id                                            name        created_at  clients  panes
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234        1        3

tabs
id                                        session                                       name       index  active_pane                                panes
tab-00000000-0000-0000-0000-000000000001  session-00000000-0000-0000-0000-000000000001  amber-fox  1      pane-00000000-0000-0000-0000-000000000001  2

panes
id                                         tab                                       session                                       title  cwd         command      state    focused_by
pane-00000000-0000-0000-0000-000000000001  tab-00000000-0000-0000-0000-000000000001  session-00000000-0000-0000-0000-000000000001  htop   /home/user  htop --tree  running  1

clients
id                                           session                                       attached_at  viewport  pane_area  active_tab                                focused_pane  lock
client-00000000-0000-0000-0000-000000000001  session-00000000-0000-0000-0000-000000000001  1234         120x40    -          tab-00000000-0000-0000-0000-000000000001  -             Normal

";
    assert_eq!(
        render_dump_state(&[build_test_session_overview()], OutputFormat::Table),
        expected_text
    );
}

#[test]
fn dump_state_table_with_no_sessions_prints_four_empty_tables() {
    let expected_text = "\
sessions
id  name  created_at  clients  panes

tabs
id  session  name  index  active_pane  panes

panes
id  tab  session  title  cwd  command  state  focused_by

clients
id  session  attached_at  viewport  pane_area  active_tab  focused_pane  lock

";
    assert_eq!(render_dump_state(&[], OutputFormat::Table), expected_text);
}

#[test]
fn dump_state_table_prints_a_hidden_argument_as_it_was_given() {
    let hidden_session_overview = SessionOverview {
        panes: vec![PaneDiscovery {
            command_argv: Some(vec!["mysql".to_string(), "***".to_string()]),
            ..build_test_pane_discovery()
        }],
        ..build_test_session_overview()
    };

    let rendered_text = render_dump_state(&[hidden_session_overview], OutputFormat::Table);

    assert!(
        rendered_text.contains("  mysql ***  running  1\n"),
        "the command column must print the redacted pane command argv verbatim: {rendered_text}",
    );
}

#[test]
fn dump_state_table_spans_every_session_given() {
    let second_session_overview = SessionOverview {
        session: SessionDiscovery {
            session_name: "wandering-heron".to_string(),
            ..build_test_session_discovery()
        },
        ..build_test_session_overview()
    };

    let rendered_text = render_dump_state(
        &[build_test_session_overview(), second_session_overview],
        OutputFormat::Table,
    );

    assert_eq!(rendered_text.matches("quiet-lake").count(), 1);
    assert_eq!(
        rendered_text.matches("wandering-heron").count(),
        1,
        "{rendered_text}"
    );
    assert_eq!(
        rendered_text
            .matches("pane-00000000-0000-0000-0000-000000000001  tab-")
            .count(),
        2,
        "both sessions' panes are listed: {rendered_text}",
    );
}

#[test]
fn dump_state_json_is_an_array_of_whole_overviews() {
    let rendered_text = render_dump_state(&[build_test_session_overview()], OutputFormat::Json);
    let parsed_json: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("the dump is JSON");

    assert_eq!(
        parsed_json,
        serde_json::json!([{
            "session": {
                "id": "00000000-0000-0000-0000-000000000001",
                "name": "quiet-lake",
                "created_at": { "secs_since_epoch": 1234, "nanos_since_epoch": 0 },
                "attached_clients": ["00000000-0000-0000-0000-000000000001"],
                "pane_count": 3
            },
            "tabs": [{
                "id": "00000000-0000-0000-0000-000000000001",
                "session_id": "00000000-0000-0000-0000-000000000001",
                "name": "amber-fox",
                "index": 1,
                "active_pane": "00000000-0000-0000-0000-000000000001",
                "pane_count": 2
            }],
            "panes": [{
                "id": "00000000-0000-0000-0000-000000000001",
                "tab_id": "00000000-0000-0000-0000-000000000001",
                "session_id": "00000000-0000-0000-0000-000000000001",
                "title": "htop",
                "cwd": "/home/user",
                "command": ["htop", "--tree"],
                "state": "running",
                "focused_by_clients": ["00000000-0000-0000-0000-000000000001"]
            }],
            "clients": [{
                "id": "00000000-0000-0000-0000-000000000001",
                "session_id": "00000000-0000-0000-0000-000000000001",
                "attached_at": { "secs_since_epoch": 1234, "nanos_since_epoch": 0 },
                "viewport_size": { "cols": 120, "rows": 40 },
                "active_tab": "00000000-0000-0000-0000-000000000001",
                "focused_pane": null,
                "lock_state": "Normal",
                "origin": "Local",
                "pane_area": null
            }]
        }])
    );
}

#[test]
fn dump_state_json_with_no_sessions_is_an_empty_array() {
    assert_eq!(render_dump_state(&[], OutputFormat::Json), "[]\n");
}

#[test]
fn dump_layout_table_shows_the_tree_the_solve_and_the_focus() {
    let expected_text = "\
session session-00000000-0000-0000-0000-000000000001 quiet-lake
  tab tab-00000000-0000-0000-0000-000000000002 editor index 0
    tree
      horizontal split
        pane pane-00000000-0000-0000-0000-000000000004
        pane pane-00000000-0000-0000-0000-000000000005
    client client-00000000-0000-0000-0000-000000000003 tiled viewport 80x22
      pane pane-00000000-0000-0000-0000-000000000004 rect 0,0 40x22
      pane pane-00000000-0000-0000-0000-000000000005 rect 40,0 40x22
  clients
    client-00000000-0000-0000-0000-000000000003 tab tab-00000000-0000-0000-0000-000000000002 focus pane-00000000-0000-0000-0000-000000000004
";
    let layout = build_test_session_layout(
        build_test_horizontal_split(),
        vec![build_test_tiled_solve()],
        Some(build_test_first_pane_id()),
    );

    assert_eq!(
        render_layouts(&[layout], OutputFormat::Table),
        expected_text
    );
}

#[test]
fn dump_layout_table_marks_a_tab_no_client_views() {
    let expected_text = "\
session session-00000000-0000-0000-0000-000000000001 quiet-lake
  tab tab-00000000-0000-0000-0000-000000000002 editor index 0
    tree
      pane pane-00000000-0000-0000-0000-000000000004
    no client views this tab
  clients
";
    let layout = SessionLayout {
        clients: Vec::new(),
        ..build_test_session_layout(
            LayoutNode::Pane(build_test_first_pane_id()),
            Vec::new(),
            None,
        )
    };

    assert_eq!(
        render_layouts(&[layout], OutputFormat::Table),
        expected_text
    );
}

#[test]
fn dump_layout_table_lists_the_panes_with_no_room() {
    let solved_tab = SolvedTab {
        pane_rects: vec![
            SolvedPane {
                pane_id: build_test_first_pane_id(),
                outer_rect: Rect::from_size_at_origin(Size {
                    column_count: 6,
                    row_count: 4,
                }),
            },
            SolvedPane {
                pane_id: build_test_second_pane_id(),
                outer_rect: Rect::empty_at_origin(),
            },
        ],
        suppressed_pane_ids: vec![build_test_second_pane_id()],
        viewport_size: Size {
            column_count: 6,
            row_count: 4,
        },
        ..build_test_tiled_solve()
    };
    let layout = build_test_session_layout(
        build_test_horizontal_split(),
        vec![solved_tab],
        Some(build_test_first_pane_id()),
    );

    let rendered_text = render_layouts(&[layout], OutputFormat::Table);

    assert!(
        rendered_text.contains(
            "      pane pane-00000000-0000-0000-0000-000000000005 rect 0,0 0x0\n      no room: pane-00000000-0000-0000-0000-000000000005\n"
        ),
        "{rendered_text}",
    );
    assert!(
        !rendered_text.contains("no room for any pane"),
        "one pane still has room: {rendered_text}",
    );
}

#[test]
fn dump_layout_table_says_when_no_pane_has_room() {
    let solved_tab = SolvedTab {
        pane_rects: vec![SolvedPane {
            pane_id: build_test_first_pane_id(),
            outer_rect: Rect::empty_at_origin(),
        }],
        suppressed_pane_ids: vec![build_test_first_pane_id()],
        is_every_pane_suppressed: true,
        viewport_size: Size {
            column_count: 3,
            row_count: 3,
        },
        ..build_test_tiled_solve()
    };
    let layout = build_test_session_layout(
        LayoutNode::Pane(build_test_first_pane_id()),
        vec![solved_tab],
        Some(build_test_first_pane_id()),
    );

    let rendered_text = render_layouts(&[layout], OutputFormat::Table);

    assert!(
        rendered_text.contains(
            "      no room: pane-00000000-0000-0000-0000-000000000004\n      no room for any pane\n"
        ),
        "{rendered_text}",
    );
}

#[test]
fn dump_layout_table_shows_a_stack_with_its_collapsed_member_and_header() {
    let expected_text = "\
session session-00000000-0000-0000-0000-000000000001 quiet-lake
  tab tab-00000000-0000-0000-0000-000000000002 editor index 0
    tree
      stacked split, active member 0
        pane pane-00000000-0000-0000-0000-000000000004
        pane pane-00000000-0000-0000-0000-000000000005 (collapsed)
    client client-00000000-0000-0000-0000-000000000003 tiled viewport 80x22
      pane pane-00000000-0000-0000-0000-000000000004 rect 0,0 80x21
      pane pane-00000000-0000-0000-0000-000000000005 rect 0,21 80x1
      stack header pane-00000000-0000-0000-0000-000000000005 rect 0,21 80x1 [2/2]
  clients
    client-00000000-0000-0000-0000-000000000003 tab tab-00000000-0000-0000-0000-000000000002 focus pane-00000000-0000-0000-0000-000000000004
";
    let solved_tab = SolvedTab {
        pane_rects: vec![
            SolvedPane {
                pane_id: build_test_first_pane_id(),
                outer_rect: Rect::from_size_at_origin(Size {
                    column_count: 80,
                    row_count: 21,
                }),
            },
            SolvedPane {
                pane_id: build_test_second_pane_id(),
                outer_rect: Rect::from_origin_and_size(
                    Point { column: 0, row: 21 },
                    Size {
                        column_count: 80,
                        row_count: 1,
                    },
                ),
            },
        ],
        stack_headers: vec![StackHeader {
            pane_id: build_test_second_pane_id(),
            header_rect: Rect::from_origin_and_size(
                Point { column: 0, row: 21 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
            member_index: 1,
            member_count: 2,
        }],
        ..build_test_tiled_solve()
    };
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![build_test_first_pane_id(), build_test_second_pane_id()],
        0,
    ));
    let layout =
        build_test_session_layout(stack, vec![solved_tab], Some(build_test_first_pane_id()));

    assert_eq!(
        render_layouts(&[layout], OutputFormat::Table),
        expected_text
    );
}

#[test]
fn dump_layout_table_marks_every_member_but_the_active_one() {
    // `active` alone decides the mark: an index past the last child names
    // the last child active, so member 0 is the collapsed one.
    let stack = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutNode::Pane(build_test_first_pane_id()),
            LayoutNode::Pane(build_test_second_pane_id()),
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active_child_index: 9,
    });
    let layout = build_test_session_layout(stack, Vec::new(), None);

    let rendered_text = render_layouts(&[layout], OutputFormat::Table);

    assert!(
        rendered_text.contains(
            "      stacked split, active member 9\n        pane pane-00000000-0000-0000-0000-000000000004 (collapsed)\n        pane pane-00000000-0000-0000-0000-000000000005\n"
        ),
        "{rendered_text}",
    );
}

#[test]
fn dump_layout_table_shows_a_vertical_split_by_name() {
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            LayoutNode::Pane(build_test_first_pane_id()),
            LayoutNode::Pane(build_test_second_pane_id()),
        ],
    ));
    let layout = build_test_session_layout(tree, Vec::new(), None);

    let rendered_text = render_layouts(&[layout], OutputFormat::Table);

    assert!(
        rendered_text.contains("      vertical split\n"),
        "{rendered_text}"
    );
}

#[test]
fn dump_layout_table_shows_a_fullscreen_client_and_its_pane() {
    let solved_tab = SolvedTab {
        layout_mode: LayoutMode::Fullscreen {
            focused_pane_id: build_test_second_pane_id(),
        },
        ..build_test_tiled_solve()
    };
    let layout = build_test_session_layout(
        build_test_horizontal_split(),
        vec![solved_tab],
        Some(build_test_second_pane_id()),
    );

    let rendered_text = render_layouts(&[layout], OutputFormat::Table);

    assert!(
        rendered_text.contains(
            "    client client-00000000-0000-0000-0000-000000000003 fullscreen pane-00000000-0000-0000-0000-000000000005 viewport 80x22\n"
        ),
        "{rendered_text}",
    );
}

#[test]
fn dump_layout_table_shows_a_dash_for_a_client_that_has_focused_nothing() {
    let layout = build_test_session_layout(
        LayoutNode::Pane(build_test_first_pane_id()),
        Vec::new(),
        None,
    );

    let rendered_text = render_layouts(&[layout], OutputFormat::Table);

    assert!(
        rendered_text.contains(
            "    client-00000000-0000-0000-0000-000000000003 tab tab-00000000-0000-0000-0000-000000000002 focus -\n"
        ),
        "{rendered_text}",
    );
}

#[test]
fn dump_layout_table_renders_a_split_with_no_children_as_the_split_alone() {
    let expected_text = "\
session session-00000000-0000-0000-0000-000000000001 quiet-lake
  tab tab-00000000-0000-0000-0000-000000000002 editor index 0
    tree
      horizontal split
    no client views this tab
  clients
";
    let empty_split = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: Vec::new(),
        weights: Vec::new(),
        active_child_index: 0,
    });
    let layout = SessionLayout {
        clients: Vec::new(),
        ..build_test_session_layout(empty_split, Vec::new(), None)
    };

    assert_eq!(
        render_layouts(&[layout], OutputFormat::Table),
        expected_text
    );
}

#[test]
fn dump_layout_table_renders_a_session_with_no_tabs_as_its_name_and_no_clients() {
    let expected_text = "\
session session-00000000-0000-0000-0000-000000000001 quiet-lake
  clients
";
    let layout = SessionLayout {
        session_id: build_test_layout_session_id(),
        session_name: "quiet-lake".to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    };

    assert_eq!(
        render_layouts(&[layout], OutputFormat::Table),
        expected_text
    );
}

#[test]
fn dump_layout_table_of_no_sessions_is_empty() {
    assert_eq!(render_layouts(&[], OutputFormat::Table), "");
}

#[test]
fn dump_layout_table_renders_every_session_given() {
    let first_session_layout = build_test_session_layout(
        LayoutNode::Pane(build_test_first_pane_id()),
        Vec::new(),
        None,
    );
    let second_session_layout = SessionLayout {
        session_name: "amber-fox".to_string(),
        ..build_test_session_layout(
            LayoutNode::Pane(build_test_second_pane_id()),
            Vec::new(),
            None,
        )
    };

    let rendered_text = render_layouts(
        &[first_session_layout, second_session_layout],
        OutputFormat::Table,
    );

    assert_eq!(rendered_text.matches("session session-").count(), 2);
    assert_eq!(rendered_text.matches("quiet-lake").count(), 1);
    assert_eq!(rendered_text.matches("amber-fox").count(), 1);
}

#[test]
fn dump_layout_json_is_an_array_of_whole_layouts() {
    let layout = build_test_session_layout(
        LayoutNode::Pane(build_test_first_pane_id()),
        vec![SolvedTab {
            pane_rects: vec![SolvedPane {
                pane_id: build_test_first_pane_id(),
                outer_rect: Rect::from_size_at_origin(Size {
                    column_count: 80,
                    row_count: 22,
                }),
            }],
            ..build_test_tiled_solve()
        }],
        Some(build_test_first_pane_id()),
    );

    let rendered_text = render_layouts(&[layout], OutputFormat::Json);
    let parsed_json: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("the dump is JSON");

    assert_eq!(
        parsed_json,
        serde_json::json!([{
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "quiet-lake",
            "tabs": [{
                "id": "00000000-0000-0000-0000-000000000002",
                "name": "editor",
                "index": 0,
                "tree": { "Pane": "00000000-0000-0000-0000-000000000004" },
                "solved": [{
                    "client": "00000000-0000-0000-0000-000000000003",
                    "viewport": { "cols": 80, "rows": 22 },
                    "mode": "Tiled",
                    "panes": [{
                        "id": "00000000-0000-0000-0000-000000000004",
                        "rect": {
                            "origin": { "x": 0, "y": 0 },
                            "size": { "cols": 80, "rows": 22 }
                        }
                    }],
                    "suppressed": [],
                    "all_suppressed": false,
                    "stack_headers": []
                }]
            }],
            "clients": [{
                "id": "00000000-0000-0000-0000-000000000003",
                "active_tab": "00000000-0000-0000-0000-000000000002",
                "focused_pane": "00000000-0000-0000-0000-000000000004"
            }]
        }])
    );
}

#[test]
fn dump_layout_json_of_no_sessions_is_an_empty_array() {
    assert_eq!(render_layouts(&[], OutputFormat::Json), "[]\n");
}

// --- debug events ---

/// One session's remembered events, over the fixed ids the layout fixtures use.
fn build_session_events(recent_events: Vec<RecentEvent>) -> SessionEvents {
    SessionEvents {
        session_id: build_test_layout_session_id(),
        session_name: "quiet-lake".to_string(),
        recent_events,
    }
}

/// The record a `PaneCreated` for the first pane of the fixture tab makes.
fn build_pane_created_event() -> RecentEvent {
    recent_event::record_event(
        &Event::PaneCreated(PaneCreated {
            pane_id: build_test_first_pane_id(),
            tab_id: build_test_layout_tab_id(),
        }),
        build_fixed_test_time(),
    )
}

#[test]
fn debug_events_table_shows_when_what_and_which_ids() {
    let expected_text = "\
session                                       name        at    event        ids
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  PaneCreated  tab-00000000-0000-0000-0000-000000000002 pane-00000000-0000-0000-0000-000000000004
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  Quit         -
";
    let rendered_text = render_recent_events(
        &[build_session_events(vec![
            build_pane_created_event(),
            recent_event::record_event(&Event::Quit, build_fixed_test_time()),
        ])],
        OutputFormat::Table,
    );

    assert_eq!(rendered_text, expected_text);
}

#[test]
fn debug_events_table_of_a_session_that_remembers_nothing_is_the_header_alone() {
    assert_eq!(
        render_recent_events(&[build_session_events(Vec::new())], OutputFormat::Table),
        "session  name  at  event  ids\n"
    );
}

#[test]
fn debug_events_table_tells_two_sessions_sharing_a_name_apart() {
    let second_session_events = SessionEvents {
        session_id: SessionId::from_uuid(build_test_uuid_with_suffix(9)),
        session_name: "quiet-lake".to_string(),
        recent_events: vec![recent_event::record_event(
            &Event::Restarting,
            build_fixed_test_time(),
        )],
    };

    let rendered_text = render_recent_events(
        &[
            build_session_events(vec![build_pane_created_event()]),
            second_session_events,
        ],
        OutputFormat::Table,
    );

    assert_eq!(
        rendered_text,
        "\
session                                       name        at    event        ids
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  PaneCreated  tab-00000000-0000-0000-0000-000000000002 pane-00000000-0000-0000-0000-000000000004
session-00000000-0000-0000-0000-000000000009  quiet-lake  1234  Restarting   -
"
    );
}

#[test]
fn debug_events_json_carries_the_name_the_ids_and_the_time() {
    let rendered_text = render_recent_events(
        &[build_session_events(vec![build_pane_created_event()])],
        OutputFormat::Json,
    );
    let parsed_json: serde_json::Value =
        serde_json::from_str(&rendered_text).expect("the listing is JSON");

    assert_eq!(
        parsed_json,
        serde_json::json!([{
            "session": "00000000-0000-0000-0000-000000000001",
            "name": "quiet-lake",
            "events": [{
                "at": { "secs_since_epoch": 1234, "nanos_since_epoch": 0 },
                "name": "PaneCreated",
                "session": null,
                "client": null,
                "tab": "00000000-0000-0000-0000-000000000002",
                "pane": "00000000-0000-0000-0000-000000000004",
                "plugin": null,
                "command": null,
                "subscriber": null
            }]
        }])
    );
}

#[test]
fn debug_events_table_of_typed_input_shows_ids_and_no_typed_content() {
    let typed_event = recent_event::record_event(
        &Event::PaneTyped(PaneTyped {
            pane_id: build_test_first_pane_id(),
            tab_id: build_test_layout_tab_id(),
            session_id: build_test_layout_session_id(),
            client_id: build_test_layout_client_id(),
            typed_payload: TypedPayload::SafePublic('%'),
            accepted_at: build_fixed_test_time(),
        }),
        build_fixed_test_time(),
    );
    let submitted_event = recent_event::record_event(
        &Event::PaneEnterPressed(PaneEnterPressed {
            pane_id: build_test_first_pane_id(),
            tab_id: build_test_layout_tab_id(),
            session_id: build_test_layout_session_id(),
            client_id: build_test_layout_client_id(),
            submitted_line: SubmittedLinePayload::SafePublic("mysql -u root -phunter2".to_string()),
            accepted_at: build_fixed_test_time(),
        }),
        build_fixed_test_time(),
    );

    let rendered_text = render_recent_events(
        &[build_session_events(vec![typed_event, submitted_event])],
        OutputFormat::Table,
    );

    assert_eq!(
        rendered_text,
        "\
session                                       name        at    event             ids
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  PaneTyped         session-00000000-0000-0000-0000-000000000001 client-00000000-0000-0000-0000-000000000003 tab-00000000-0000-0000-0000-000000000002 pane-00000000-0000-0000-0000-000000000004
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  PaneEnterPressed  session-00000000-0000-0000-0000-000000000001 client-00000000-0000-0000-0000-000000000003 tab-00000000-0000-0000-0000-000000000002 pane-00000000-0000-0000-0000-000000000004
"
    );
    assert!(!rendered_text.contains('%'), "{rendered_text}");
    assert!(!rendered_text.contains("hunter2"), "{rendered_text}");
}

/// A record for `Event::TabCreated`, stamped `at`.
fn build_tab_created_event_at(occurred_at: SystemTime) -> RecentEvent {
    recent_event::record_event(
        &Event::TabCreated(koshi_core::event::TabCreated {
            tab_id: build_test_layout_tab_id(),
        }),
        occurred_at,
    )
}

#[test]
fn no_since_flag_keeps_every_event() {
    assert_eq!(
        compute_oldest_event_time(build_fixed_test_time(), None),
        None
    );
}

#[test]
fn a_since_window_counts_back_from_now() {
    assert_eq!(
        compute_oldest_event_time(build_fixed_test_time(), Some(Duration::from_secs(34))),
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1200))
    );
}

#[test]
fn a_since_window_older_than_the_clock_can_reach_keeps_every_event() {
    assert_eq!(
        compute_oldest_event_time(build_fixed_test_time(), Some(Duration::from_secs(u64::MAX))),
        None
    );
}

#[test]
fn narrowing_with_no_flags_keeps_every_event() {
    let recent_events = vec![
        build_pane_created_event(),
        build_tab_created_event_at(build_fixed_test_time()),
    ];

    assert_eq!(
        filter_recent_events(recent_events.clone(), None, None),
        recent_events
    );
}

#[test]
fn narrowing_by_name_ignores_case_and_matches_any_part_of_it() {
    let recent_events = vec![
        build_pane_created_event(),
        build_tab_created_event_at(build_fixed_test_time()),
    ];

    let pane_events = filter_recent_events(recent_events.clone(), None, Some("pane"));
    assert_eq!(pane_events.len(), 1);
    assert_eq!(pane_events[0].event_name, "PaneCreated");

    let tab_events = filter_recent_events(recent_events.clone(), None, Some("TABCREATED"));
    assert_eq!(tab_events.len(), 1);
    assert_eq!(tab_events[0].event_name, "TabCreated");

    assert_eq!(
        filter_recent_events(recent_events, None, Some("Copied")),
        Vec::new()
    );
}

#[test]
fn narrowing_by_time_keeps_the_boundary_and_drops_what_is_older() {
    let older_event =
        build_tab_created_event_at(SystemTime::UNIX_EPOCH + Duration::from_secs(1233));
    let boundary_event = build_tab_created_event_at(build_fixed_test_time());
    let newer_event =
        build_tab_created_event_at(SystemTime::UNIX_EPOCH + Duration::from_secs(1235));

    let filtered_events = filter_recent_events(
        vec![older_event, boundary_event.clone(), newer_event.clone()],
        Some(build_fixed_test_time()),
        None,
    );

    assert_eq!(filtered_events, vec![boundary_event, newer_event]);
}

#[test]
fn narrowing_by_time_and_name_together_keeps_only_what_passes_both() {
    let old_pane_event = recent_event::record_event(
        &Event::PaneCreated(PaneCreated {
            pane_id: build_test_first_pane_id(),
            tab_id: build_test_layout_tab_id(),
        }),
        SystemTime::UNIX_EPOCH + Duration::from_secs(1233),
    );
    let new_pane_event = build_pane_created_event();
    let new_tab_event = build_tab_created_event_at(build_fixed_test_time());

    let filtered_events = filter_recent_events(
        vec![old_pane_event, new_pane_event.clone(), new_tab_event],
        Some(build_fixed_test_time()),
        Some("pane"),
    );

    assert_eq!(filtered_events, vec![new_pane_event]);
}

#[test]
fn debug_events_json_of_no_sessions_is_an_empty_array() {
    assert_eq!(render_recent_events(&[], OutputFormat::Json), "[]\n");
}

#[test]
fn narrowing_an_empty_listing_keeps_it_empty() {
    assert_eq!(
        filter_recent_events(Vec::new(), Some(build_fixed_test_time()), Some("pane")),
        Vec::new()
    );
}

#[test]
fn a_zero_length_since_window_keeps_only_what_was_recorded_at_that_moment() {
    let earlier_event =
        build_tab_created_event_at(SystemTime::UNIX_EPOCH + Duration::from_secs(1233));
    let current_event = build_tab_created_event_at(build_fixed_test_time());

    let filtered_events = filter_recent_events(
        vec![earlier_event, current_event.clone()],
        compute_oldest_event_time(build_fixed_test_time(), Some(Duration::ZERO)),
        None,
    );

    assert_eq!(filtered_events, vec![current_event]);
}

#[test]
fn a_filter_that_matches_no_event_name_keeps_nothing() {
    let recent_events = vec![
        build_pane_created_event(),
        build_tab_created_event_at(build_fixed_test_time()),
    ];

    for event_name_filter in [
        "'; DROP TABLE events",
        "../../etc/passwd",
        "パネル",
        "🦀",
        "%s%n",
    ] {
        assert_eq!(
            filter_recent_events(recent_events.clone(), None, Some(event_name_filter)),
            Vec::new(),
            "--filter {event_name_filter}"
        );
    }
}

#[test]
fn a_dotted_capital_i_does_not_match_an_ascii_i_in_an_event_name() {
    // "İ".to_lowercase() is "i" plus a combining dot, which no ASCII name holds.
    let recent_events = vec![recent_event::record_event(
        &Event::InputModeChanged(koshi_core::event::InputModeChanged {
            client_id: build_test_layout_client_id(),
            lock_mode: koshi_core::lock::LockMode::Normal,
        }),
        build_fixed_test_time(),
    )];

    assert_eq!(
        filter_recent_events(recent_events.clone(), None, Some("İ")),
        Vec::new()
    );
    assert_eq!(
        filter_recent_events(recent_events, None, Some("i")).len(),
        1
    );
}

#[test]
fn debug_events_table_pads_a_non_ascii_session_name_by_characters() {
    let wide_session_events = SessionEvents {
        session_id: SessionId::from_uuid(build_test_uuid_with_suffix(9)),
        session_name: "S-ふるい-みず".to_string(),
        recent_events: vec![recent_event::record_event(
            &Event::Quit,
            build_fixed_test_time(),
        )],
    };

    let rendered_text = render_recent_events(
        &[
            build_session_events(vec![build_pane_created_event()]),
            wide_session_events,
        ],
        OutputFormat::Table,
    );

    let session_name_column: Vec<&str> = rendered_text
        .lines()
        .map(|line| line.split_at(46).1)
        .map(|remaining_text| remaining_text.split("  ").next().unwrap_or(remaining_text))
        .collect();
    assert_eq!(
        session_name_column,
        ["name", "quiet-lake", "S-ふるい-みず"],
        "{rendered_text}"
    );
}

#[test]
fn debug_events_table_renders_a_row_for_every_event_a_full_ring_holds() {
    let recent_events = vec![build_pane_created_event(); 1000];

    let rendered_text =
        render_recent_events(&[build_session_events(recent_events)], OutputFormat::Table);

    assert_eq!(
        rendered_text.lines().count(),
        1001,
        "the header plus one row each"
    );
}
