//! Tests for CLI output rendering — discovery, action and keymap
//! introspection, and the `debug` dumps: exact JSON schema snapshots (the
//! stable scripting surface) and exact table/field renderings, all over fixed
//! fake data.

use koshi_core::client::ClientOrigin;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use koshi_config::types::{BoundAction, ModeName};
use koshi_core::action::{
    core_action_seeds, ActionHandlerRef, ActionRef, ActionScope, ActionStatus, TargetKind,
};
use koshi_core::discovery::{ClientInfo, PaneInfo, PaneState, SessionInfo, TabInfo};
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
        server: None,
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
        origin: Some(ClientOrigin::Local),
        pane_area: None,
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
fn session_list_json_is_an_array_of_id_name_and_server() {
    let expected = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "name": "quiet-lake",
    "server": null
  }
]
"#;
    assert_eq!(render_sessions(&[session_row()], FormatArg::Json), expected);
}

#[test]
fn session_list_json_names_the_server_of_a_remote_row() {
    let mut row = session_row();
    row.server = Some("desk".to_string());
    let expected = r#"[
  {
    "id": "00000000-0000-0000-0000-000000000001",
    "name": "quiet-lake",
    "server": "desk"
  }
]
"#;
    assert_eq!(render_sessions(&[row], FormatArg::Json), expected);
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
  "lock_state": "Normal",
  "origin": "Local",
  "pane_area": null
}
"#;
    assert_eq!(render_client(&client_info(), FormatArg::Json), expected);
}

#[test]
fn a_reported_pane_area_json_is_a_tagged_size() {
    let reported = ClientInfo {
        pane_area: Some(PaneArea::Reported(Size {
            cols: 100,
            rows: 30,
        })),
        ..client_info()
    };

    let rendered = render_client(&reported, FormatArg::Json);

    assert!(
        rendered.contains(
            "\"pane_area\": {\n    \"Reported\": {\n      \"cols\": 100,\n      \"rows\": 30\n    }\n  }"
        ),
        "unexpected pane_area form: {rendered}"
    );
}

// --- Table renderings ---

#[test]
fn session_table_marks_where_each_session_runs() {
    let mut remote = session_row();
    remote.server = Some("desk".to_string());
    let expected = "\
id                                            name        server
session-00000000-0000-0000-0000-000000000001  quiet-lake  local
session-00000000-0000-0000-0000-000000000001  quiet-lake  desk
";
    assert_eq!(
        render_sessions(&[session_row(), remote], FormatArg::Table),
        expected
    );
}

#[test]
fn empty_list_table_is_just_the_header() {
    assert_eq!(render_sessions(&[], FormatArg::Table), "id  name  server\n");
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
pane_area: -
active_tab: tab-00000000-0000-0000-0000-000000000001
focused_pane: -
lock: Normal
";
    assert_eq!(render_client(&client_info(), FormatArg::Table), expected);
}

#[test]
fn a_starving_client_prints_starving_in_the_pane_area_column() {
    let starving = ClientInfo {
        pane_area: Some(PaneArea::Starving),
        ..client_info()
    };

    let rendered = render_client(&starving, FormatArg::Table);

    assert!(
        rendered.contains("\npane_area: starving\n"),
        "unexpected pane_area line: {rendered}"
    );
}

#[test]
fn a_reported_pane_area_prints_as_cols_by_rows() {
    let reported = ClientInfo {
        pane_area: Some(PaneArea::Reported(Size {
            cols: 100,
            rows: 30,
        })),
        ..client_info()
    };

    let rendered = render_client(&reported, FormatArg::Table);

    assert!(
        rendered.contains("\npane_area: 100x30\n"),
        "unexpected pane_area line: {rendered}"
    );
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
fn explain_quit_reports_its_client_scope_and_both_target_kinds() {
    // `quit` is seeded with `ActionScope::Client` and both `ClientTarget`
    // and `Session` targets, so it exercises the session target label no
    // other explain test covers end-to-end.
    let expected = "\
action: core:quit
display_name: Quit
description: Leave the session, ending it when auto-close-session is on and no other client stays
scope: client
targets: client, session
command: Quit
examples: core:quit
";
    assert_eq!(
        render_action_explain("quit", FormatArg::Table),
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
    assert_eq!(
        render_keys_describe(&view, "Ctrl-g", FormatArg::Table),
        Err(
            "invalid key `Ctrl-g`: a multi-character key must be bracketed, as in `<Tab>`"
                .to_string()
        )
    );
}

#[test]
fn keys_describe_reports_the_user_entry_alone_when_it_displaced_a_default() {
    let view = view_with_binding("<A-f>", "core:close-pane");
    let rendered = render_keys_describe(&view, "<A-f>", FormatArg::Json)
        .expect("sequence parses")
        .expect("bound in normal mode");
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    let details = value.as_array().expect("a JSON array");

    assert_eq!(details.len(), 1);
    assert_eq!(details[0]["action"], serde_json::json!("core:close-pane"));
    assert_eq!(details[0]["source"], serde_json::json!("user"));
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

// --- Debug dumps ---

/// A fixed UUID ending in `tail`, so the ids inside one dump stay
/// distinguishable.
fn uuid_ending(tail: u8) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-0000-0000-0000000000{tail:02}"))
        .expect("literal UUID parses")
}

/// One session's whole record: itself, one tab, one pane, and one client.
fn overview() -> SessionOverview {
    SessionOverview {
        session: session_info(),
        tabs: vec![tab_info()],
        panes: vec![pane_info()],
        clients: vec![client_info()],
    }
}

/// The session id every layout fixture below carries.
fn layout_session() -> SessionId {
    SessionId::from_uuid(uuid_ending(1))
}

/// The tab id every layout fixture below carries.
fn layout_tab() -> TabId {
    TabId::from_uuid(uuid_ending(2))
}

/// The client id every layout fixture below carries.
fn layout_client() -> ClientId {
    ClientId::from_uuid(uuid_ending(3))
}

/// The first pane of every layout fixture below.
fn first_pane() -> PaneId {
    PaneId::from_uuid(uuid_ending(4))
}

/// The second pane of every layout fixture below.
fn second_pane() -> PaneId {
    PaneId::from_uuid(uuid_ending(5))
}

/// A layout of one session holding one tab with `tree`, solved as `solved`,
/// viewed by one client focused on `focused`.
fn layout_of(tree: LayoutNode, solved: Vec<SolvedTab>, focused: Option<PaneId>) -> SessionLayout {
    SessionLayout {
        id: layout_session(),
        name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            id: layout_tab(),
            name: "editor".to_string(),
            index: 0,
            tree,
            solved,
        }],
        clients: vec![ClientFocus {
            id: layout_client(),
            active_tab: layout_tab(),
            focused_pane: focused,
        }],
    }
}

/// A left-right split of the two fixture panes.
fn side_by_side() -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(first_pane()),
            LayoutNode::Pane(second_pane()),
        ],
    ))
}

/// One client's tiled solve of [`side_by_side`] over an 80x22 tab.
fn tiled_solve() -> SolvedTab {
    SolvedTab {
        client: layout_client(),
        viewport: Size { cols: 80, rows: 22 },
        mode: LayoutMode::Tiled,
        panes: vec![
            SolvedPane {
                id: first_pane(),
                rect: Rect::at_origin(Size { cols: 40, rows: 22 }),
            },
            SolvedPane {
                id: second_pane(),
                rect: Rect::new(Point { x: 40, y: 0 }, Size { cols: 40, rows: 22 }),
            },
        ],
        suppressed: Vec::new(),
        all_suppressed: false,
        stack_headers: Vec::new(),
    }
}

#[test]
fn dump_state_table_prints_one_named_table_per_record_kind() {
    // Each section is its own table, and a blank line closes it.
    let expected = "\
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
    assert_eq!(render_dump_state(&[overview()], FormatArg::Table), expected);
}

#[test]
fn dump_state_table_with_no_sessions_prints_four_empty_tables() {
    let expected = "\
sessions
id  name  created_at  clients  panes

tabs
id  session  name  index  active_pane  panes

panes
id  tab  session  title  cwd  command  state  focused_by

clients
id  session  attached_at  viewport  pane_area  active_tab  focused_pane  lock

";
    assert_eq!(render_dump_state(&[], FormatArg::Table), expected);
}

#[test]
fn dump_state_table_prints_a_hidden_argument_as_it_was_given() {
    let hidden = SessionOverview {
        panes: vec![PaneInfo {
            command: Some(vec!["mysql".to_string(), "***".to_string()]),
            ..pane_info()
        }],
        ..overview()
    };

    let rendered = render_dump_state(&[hidden], FormatArg::Table);

    assert!(
        rendered.contains("  mysql ***  running  1\n"),
        "the command column must print the hidden argv verbatim: {rendered}",
    );
}

#[test]
fn dump_state_table_spans_every_session_given() {
    let second = SessionOverview {
        session: SessionInfo {
            name: "wandering-heron".to_string(),
            ..session_info()
        },
        ..overview()
    };

    let rendered = render_dump_state(&[overview(), second], FormatArg::Table);

    assert_eq!(rendered.matches("quiet-lake").count(), 1);
    assert_eq!(rendered.matches("wandering-heron").count(), 1, "{rendered}");
    assert_eq!(
        rendered
            .matches("pane-00000000-0000-0000-0000-000000000001  tab-")
            .count(),
        2,
        "both sessions' panes are listed: {rendered}",
    );
}

#[test]
fn dump_state_json_is_an_array_of_whole_overviews() {
    let rendered = render_dump_state(&[overview()], FormatArg::Json);
    let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("the dump is JSON");

    assert_eq!(
        parsed,
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
    assert_eq!(render_dump_state(&[], FormatArg::Json), "[]\n");
}

#[test]
fn dump_layout_table_shows_the_tree_the_solve_and_the_focus() {
    let expected = "\
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
    let layout = layout_of(side_by_side(), vec![tiled_solve()], Some(first_pane()));

    assert_eq!(render_layouts(&[layout], FormatArg::Table), expected);
}

#[test]
fn dump_layout_table_marks_a_tab_no_client_views() {
    let expected = "\
session session-00000000-0000-0000-0000-000000000001 quiet-lake
  tab tab-00000000-0000-0000-0000-000000000002 editor index 0
    tree
      pane pane-00000000-0000-0000-0000-000000000004
    no client views this tab
  clients
";
    let layout = SessionLayout {
        clients: Vec::new(),
        ..layout_of(LayoutNode::Pane(first_pane()), Vec::new(), None)
    };

    assert_eq!(render_layouts(&[layout], FormatArg::Table), expected);
}

#[test]
fn dump_layout_table_lists_the_panes_with_no_room() {
    let solved = SolvedTab {
        panes: vec![
            SolvedPane {
                id: first_pane(),
                rect: Rect::at_origin(Size { cols: 6, rows: 4 }),
            },
            SolvedPane {
                id: second_pane(),
                rect: Rect::zero(),
            },
        ],
        suppressed: vec![second_pane()],
        viewport: Size { cols: 6, rows: 4 },
        ..tiled_solve()
    };
    let layout = layout_of(side_by_side(), vec![solved], Some(first_pane()));

    let rendered = render_layouts(&[layout], FormatArg::Table);

    assert!(
        rendered.contains(
            "      pane pane-00000000-0000-0000-0000-000000000005 rect 0,0 0x0\n      no room: pane-00000000-0000-0000-0000-000000000005\n"
        ),
        "{rendered}",
    );
    assert!(
        !rendered.contains("no room for any pane"),
        "one pane still has room: {rendered}",
    );
}

#[test]
fn dump_layout_table_says_when_no_pane_has_room() {
    let solved = SolvedTab {
        panes: vec![SolvedPane {
            id: first_pane(),
            rect: Rect::zero(),
        }],
        suppressed: vec![first_pane()],
        all_suppressed: true,
        viewport: Size { cols: 3, rows: 3 },
        ..tiled_solve()
    };
    let layout = layout_of(
        LayoutNode::Pane(first_pane()),
        vec![solved],
        Some(first_pane()),
    );

    let rendered = render_layouts(&[layout], FormatArg::Table);

    assert!(
        rendered.contains(
            "      no room: pane-00000000-0000-0000-0000-000000000004\n      no room for any pane\n"
        ),
        "{rendered}",
    );
}

#[test]
fn dump_layout_table_shows_a_stack_with_its_collapsed_member_and_header() {
    let expected = "\
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
    let solved = SolvedTab {
        panes: vec![
            SolvedPane {
                id: first_pane(),
                rect: Rect::at_origin(Size { cols: 80, rows: 21 }),
            },
            SolvedPane {
                id: second_pane(),
                rect: Rect::new(Point { x: 0, y: 21 }, Size { cols: 80, rows: 1 }),
            },
        ],
        stack_headers: vec![StackHeader {
            pane: second_pane(),
            rect: Rect::new(Point { x: 0, y: 21 }, Size { cols: 80, rows: 1 }),
            position: 1,
            total: 2,
        }],
        ..tiled_solve()
    };
    let stack = LayoutNode::Split(SplitNode::stack(vec![first_pane(), second_pane()], 0));
    let layout = layout_of(stack, vec![solved], Some(first_pane()));

    assert_eq!(render_layouts(&[layout], FormatArg::Table), expected);
}

#[test]
fn dump_layout_table_marks_every_member_but_the_active_one() {
    // `active` alone decides the mark: an index past the last child names
    // the last child active, so member 0 is the collapsed one.
    let stack = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutNode::Pane(first_pane()),
            LayoutNode::Pane(second_pane()),
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 9,
    });
    let layout = layout_of(stack, Vec::new(), None);

    let rendered = render_layouts(&[layout], FormatArg::Table);

    assert!(
        rendered.contains(
            "      stacked split, active member 9\n        pane pane-00000000-0000-0000-0000-000000000004 (collapsed)\n        pane pane-00000000-0000-0000-0000-000000000005\n"
        ),
        "{rendered}",
    );
}

#[test]
fn dump_layout_table_shows_a_vertical_split_by_name() {
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            LayoutNode::Pane(first_pane()),
            LayoutNode::Pane(second_pane()),
        ],
    ));
    let layout = layout_of(tree, Vec::new(), None);

    let rendered = render_layouts(&[layout], FormatArg::Table);

    assert!(rendered.contains("      vertical split\n"), "{rendered}");
}

#[test]
fn dump_layout_table_shows_a_fullscreen_client_and_its_pane() {
    let solved = SolvedTab {
        mode: LayoutMode::Fullscreen {
            focused: second_pane(),
        },
        ..tiled_solve()
    };
    let layout = layout_of(side_by_side(), vec![solved], Some(second_pane()));

    let rendered = render_layouts(&[layout], FormatArg::Table);

    assert!(
        rendered.contains(
            "    client client-00000000-0000-0000-0000-000000000003 fullscreen pane-00000000-0000-0000-0000-000000000005 viewport 80x22\n"
        ),
        "{rendered}",
    );
}

#[test]
fn dump_layout_table_shows_a_dash_for_a_client_that_has_focused_nothing() {
    let layout = layout_of(LayoutNode::Pane(first_pane()), Vec::new(), None);

    let rendered = render_layouts(&[layout], FormatArg::Table);

    assert!(
        rendered.contains(
            "    client-00000000-0000-0000-0000-000000000003 tab tab-00000000-0000-0000-0000-000000000002 focus -\n"
        ),
        "{rendered}",
    );
}

#[test]
fn dump_layout_table_renders_a_split_with_no_children_as_the_split_alone() {
    let expected = "\
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
        active: 0,
    });
    let layout = SessionLayout {
        clients: Vec::new(),
        ..layout_of(empty_split, Vec::new(), None)
    };

    assert_eq!(render_layouts(&[layout], FormatArg::Table), expected);
}

#[test]
fn dump_layout_table_renders_a_session_with_no_tabs_as_its_name_and_no_clients() {
    let expected = "\
session session-00000000-0000-0000-0000-000000000001 quiet-lake
  clients
";
    let layout = SessionLayout {
        id: layout_session(),
        name: "quiet-lake".to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    };

    assert_eq!(render_layouts(&[layout], FormatArg::Table), expected);
}

#[test]
fn dump_layout_table_of_no_sessions_is_empty() {
    assert_eq!(render_layouts(&[], FormatArg::Table), "");
}

#[test]
fn dump_layout_table_renders_every_session_given() {
    let first = layout_of(LayoutNode::Pane(first_pane()), Vec::new(), None);
    let second = SessionLayout {
        name: "amber-fox".to_string(),
        ..layout_of(LayoutNode::Pane(second_pane()), Vec::new(), None)
    };

    let rendered = render_layouts(&[first, second], FormatArg::Table);

    assert_eq!(rendered.matches("session session-").count(), 2);
    assert_eq!(rendered.matches("quiet-lake").count(), 1);
    assert_eq!(rendered.matches("amber-fox").count(), 1);
}

#[test]
fn dump_layout_json_is_an_array_of_whole_layouts() {
    let layout = layout_of(
        LayoutNode::Pane(first_pane()),
        vec![SolvedTab {
            panes: vec![SolvedPane {
                id: first_pane(),
                rect: Rect::at_origin(Size { cols: 80, rows: 22 }),
            }],
            ..tiled_solve()
        }],
        Some(first_pane()),
    );

    let rendered = render_layouts(&[layout], FormatArg::Json);
    let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("the dump is JSON");

    assert_eq!(
        parsed,
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
    assert_eq!(render_layouts(&[], FormatArg::Json), "[]\n");
}

// --- debug events ---

/// One session's remembered events, over the fixed ids the layout fixtures use.
fn session_events(events: Vec<RecentEvent>) -> SessionEvents {
    SessionEvents {
        session: layout_session(),
        name: "quiet-lake".to_string(),
        events,
    }
}

/// The record a `PaneCreated` for the first pane of the fixture tab makes.
fn pane_created_record() -> RecentEvent {
    recent_event::record(
        &Event::PaneCreated(PaneCreated {
            pane_id: first_pane(),
            tab_id: layout_tab(),
        }),
        fixed_time(),
    )
}

#[test]
fn debug_events_table_shows_when_what_and_which_ids() {
    let expected = "\
session                                       name        at    event        ids
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  PaneCreated  tab-00000000-0000-0000-0000-000000000002 pane-00000000-0000-0000-0000-000000000004
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  Quit         -
";
    let rendered = render_recent_events(
        &[session_events(vec![
            pane_created_record(),
            recent_event::record(&Event::Quit, fixed_time()),
        ])],
        FormatArg::Table,
    );

    assert_eq!(rendered, expected);
}

#[test]
fn debug_events_table_of_a_session_that_remembers_nothing_is_the_header_alone() {
    assert_eq!(
        render_recent_events(&[session_events(Vec::new())], FormatArg::Table),
        "session  name  at  event  ids\n"
    );
}

#[test]
fn debug_events_table_tells_two_sessions_sharing_a_name_apart() {
    let twin = SessionEvents {
        session: SessionId::from_uuid(uuid_ending(9)),
        name: "quiet-lake".to_string(),
        events: vec![recent_event::record(&Event::Restarting, fixed_time())],
    };

    let rendered = render_recent_events(
        &[session_events(vec![pane_created_record()]), twin],
        FormatArg::Table,
    );

    assert_eq!(
        rendered,
        "\
session                                       name        at    event        ids
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  PaneCreated  tab-00000000-0000-0000-0000-000000000002 pane-00000000-0000-0000-0000-000000000004
session-00000000-0000-0000-0000-000000000009  quiet-lake  1234  Restarting   -
"
    );
}

#[test]
fn debug_events_json_carries_the_name_the_ids_and_the_time() {
    let rendered = render_recent_events(
        &[session_events(vec![pane_created_record()])],
        FormatArg::Json,
    );
    let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("the listing is JSON");

    assert_eq!(
        parsed,
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
    let typed = recent_event::record(
        &Event::PaneTyped(PaneTyped {
            pane_id: first_pane(),
            tab_id: layout_tab(),
            session_id: layout_session(),
            client_id: layout_client(),
            payload: TypedPayload::SafePublic('%'),
            timestamp: fixed_time(),
        }),
        fixed_time(),
    );
    let submitted = recent_event::record(
        &Event::PaneEnterPressed(PaneEnterPressed {
            pane_id: first_pane(),
            tab_id: layout_tab(),
            session_id: layout_session(),
            client_id: layout_client(),
            line: SubmittedLinePayload::SafePublic("mysql -u root -phunter2".to_string()),
            timestamp: fixed_time(),
        }),
        fixed_time(),
    );

    let rendered =
        render_recent_events(&[session_events(vec![typed, submitted])], FormatArg::Table);

    assert_eq!(
        rendered,
        "\
session                                       name        at    event             ids
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  PaneTyped         session-00000000-0000-0000-0000-000000000001 client-00000000-0000-0000-0000-000000000003 tab-00000000-0000-0000-0000-000000000002 pane-00000000-0000-0000-0000-000000000004
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234  PaneEnterPressed  session-00000000-0000-0000-0000-000000000001 client-00000000-0000-0000-0000-000000000003 tab-00000000-0000-0000-0000-000000000002 pane-00000000-0000-0000-0000-000000000004
"
    );
    assert!(!rendered.contains('%'), "{rendered}");
    assert!(!rendered.contains("hunter2"), "{rendered}");
}

/// A record for `Event::TabCreated`, stamped `at`.
fn tab_created_at(at: SystemTime) -> RecentEvent {
    recent_event::record(
        &Event::TabCreated(koshi_core::event::TabCreated {
            tab_id: layout_tab(),
        }),
        at,
    )
}

#[test]
fn no_since_flag_keeps_every_event() {
    assert_eq!(oldest_kept(fixed_time(), None), None);
}

#[test]
fn a_since_window_counts_back_from_now() {
    assert_eq!(
        oldest_kept(fixed_time(), Some(Duration::from_secs(34))),
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1200))
    );
}

#[test]
fn a_since_window_older_than_the_clock_can_reach_keeps_every_event() {
    assert_eq!(
        oldest_kept(fixed_time(), Some(Duration::from_secs(u64::MAX))),
        None
    );
}

#[test]
fn narrowing_with_no_flags_keeps_every_event() {
    let events = vec![pane_created_record(), tab_created_at(fixed_time())];

    assert_eq!(narrow(events.clone(), None, None), events);
}

#[test]
fn narrowing_by_name_ignores_case_and_matches_any_part_of_it() {
    let events = vec![pane_created_record(), tab_created_at(fixed_time())];

    let kept = narrow(events.clone(), None, Some("pane"));
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].name, "PaneCreated");

    let kept = narrow(events.clone(), None, Some("TABCREATED"));
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].name, "TabCreated");

    assert_eq!(narrow(events, None, Some("Copied")), Vec::new());
}

#[test]
fn narrowing_by_time_keeps_the_boundary_and_drops_what_is_older() {
    let older = tab_created_at(SystemTime::UNIX_EPOCH + Duration::from_secs(1233));
    let boundary = tab_created_at(fixed_time());
    let newer = tab_created_at(SystemTime::UNIX_EPOCH + Duration::from_secs(1235));

    let kept = narrow(
        vec![older, boundary.clone(), newer.clone()],
        Some(fixed_time()),
        None,
    );

    assert_eq!(kept, vec![boundary, newer]);
}

#[test]
fn narrowing_by_time_and_name_together_keeps_only_what_passes_both() {
    let old_pane = recent_event::record(
        &Event::PaneCreated(PaneCreated {
            pane_id: first_pane(),
            tab_id: layout_tab(),
        }),
        SystemTime::UNIX_EPOCH + Duration::from_secs(1233),
    );
    let new_pane = pane_created_record();
    let new_tab = tab_created_at(fixed_time());

    let kept = narrow(
        vec![old_pane, new_pane.clone(), new_tab],
        Some(fixed_time()),
        Some("pane"),
    );

    assert_eq!(kept, vec![new_pane]);
}

#[test]
fn debug_events_json_of_no_sessions_is_an_empty_array() {
    assert_eq!(render_recent_events(&[], FormatArg::Json), "[]\n");
}

#[test]
fn narrowing_an_empty_listing_keeps_it_empty() {
    assert_eq!(
        narrow(Vec::new(), Some(fixed_time()), Some("pane")),
        Vec::new()
    );
}

#[test]
fn a_zero_length_since_window_keeps_only_what_was_recorded_at_that_moment() {
    let earlier = tab_created_at(SystemTime::UNIX_EPOCH + Duration::from_secs(1233));
    let now = tab_created_at(fixed_time());

    let kept = narrow(
        vec![earlier, now.clone()],
        oldest_kept(fixed_time(), Some(Duration::ZERO)),
        None,
    );

    assert_eq!(kept, vec![now]);
}

#[test]
fn a_filter_that_matches_no_event_name_keeps_nothing() {
    let events = vec![pane_created_record(), tab_created_at(fixed_time())];

    for wanted in [
        "'; DROP TABLE events",
        "../../etc/passwd",
        "パネル",
        "🦀",
        "%s%n",
    ] {
        assert_eq!(
            narrow(events.clone(), None, Some(wanted)),
            Vec::new(),
            "--filter {wanted}"
        );
    }
}

#[test]
fn a_dotted_capital_i_does_not_match_an_ascii_i_in_an_event_name() {
    // "İ".to_lowercase() is "i" plus a combining dot, which no ASCII name holds.
    let events = vec![recent_event::record(
        &Event::InputModeChanged(koshi_core::event::InputModeChanged {
            client_id: layout_client(),
            mode: koshi_core::lock::LockMode::Normal,
        }),
        fixed_time(),
    )];

    assert_eq!(narrow(events.clone(), None, Some("İ")), Vec::new());
    assert_eq!(narrow(events, None, Some("i")).len(), 1);
}

#[test]
fn debug_events_table_pads_a_non_ascii_session_name_by_characters() {
    let wide = SessionEvents {
        session: SessionId::from_uuid(uuid_ending(9)),
        name: "S-ふるい-みず".to_string(),
        events: vec![recent_event::record(&Event::Quit, fixed_time())],
    };

    let rendered = render_recent_events(
        &[session_events(vec![pane_created_record()]), wide],
        FormatArg::Table,
    );

    let name_column: Vec<&str> = rendered
        .lines()
        .map(|line| line.split_at(46).1)
        .map(|rest| rest.split("  ").next().unwrap_or(rest))
        .collect();
    assert_eq!(
        name_column,
        ["name", "quiet-lake", "S-ふるい-みず"],
        "{rendered}"
    );
}

#[test]
fn debug_events_table_renders_a_row_for_every_event_a_full_ring_holds() {
    let events = vec![pane_created_record(); 1000];

    let rendered = render_recent_events(&[session_events(events)], FormatArg::Table);

    assert_eq!(
        rendered.lines().count(),
        1001,
        "the header plus one row each"
    );
}
