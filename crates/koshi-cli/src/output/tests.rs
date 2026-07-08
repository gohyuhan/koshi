//! Tests for discovery output rendering: exact JSON schema snapshots (the
//! stable scripting surface) and exact table/field renderings, all over
//! fixed fake data.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use koshi_core::discovery::{ClientInfo, PaneInfo, PaneState, SessionInfo, TabInfo};
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
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
        layout_rect: Some(Rect {
            origin: Point { x: 0, y: 1 },
            size: Size { cols: 80, rows: 23 },
        }),
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
fn session_list_json_is_an_array() {
    let rendered = render_sessions(&[session_info()], FormatArg::Json);
    assert!(rendered.starts_with("[\n"), "not an array: {rendered}");
    assert_eq!(
        rendered.trim_end(),
        format!(
            "[\n{}\n]",
            render_session(&session_info(), FormatArg::Json)
                .trim_end()
                .lines()
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    );
}

#[test]
fn tab_json_schema_is_stable() {
    let expected = r#"{
  "id": "00000000-0000-0000-0000-000000000001",
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
  ],
  "layout_rect": {
    "origin": {
      "x": 0,
      "y": 1
    },
    "size": {
      "cols": 80,
      "rows": 23
    }
  }
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
  ],
  "layout_rect": {
    "origin": {
      "x": 0,
      "y": 1
    },
    "size": {
      "cols": 80,
      "rows": 23
    }
  }
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
id                                            name        created_at  clients  panes
session-00000000-0000-0000-0000-000000000001  quiet-lake  1234        1        3
";
    assert_eq!(
        render_sessions(&[session_info()], FormatArg::Table),
        expected
    );
}

#[test]
fn empty_list_table_is_just_the_header() {
    assert_eq!(
        render_sessions(&[], FormatArg::Table),
        "id  name  created_at  clients  panes\n"
    );
}

#[test]
fn tab_table_renders_the_active_pane_id() {
    let expected = "\
id                                        name       index  active_pane                                panes
tab-00000000-0000-0000-0000-000000000001  amber-fox  1      pane-00000000-0000-0000-0000-000000000001  2
";
    assert_eq!(render_tabs(&[tab_info()], FormatArg::Table), expected);
}

#[test]
fn pane_table_renders_command_state_and_rect() {
    let expected = "\
id                                         tab                                       session                                       title  cwd         command      state    focused_by  rect
pane-00000000-0000-0000-0000-000000000001  tab-00000000-0000-0000-0000-000000000001  session-00000000-0000-0000-0000-000000000001  htop   /home/user  htop --tree  running  1           80x23@0,1
";
    assert_eq!(render_panes(&[pane_info()], FormatArg::Table), expected);
}

#[test]
fn absent_values_render_as_dashes() {
    let mut pane = pane_info();
    pane.title = None;
    pane.cwd = None;
    pane.command = None;
    pane.layout_rect = None;
    pane.state = PaneState::Exited { code: None };
    let rendered = render_panes(&[pane], FormatArg::Table);
    let row = rendered.lines().nth(1).expect("one data row");
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(
        cells,
        vec![
            "pane-00000000-0000-0000-0000-000000000001",
            "tab-00000000-0000-0000-0000-000000000001",
            "session-00000000-0000-0000-0000-000000000001",
            "-",
            "-",
            "-",
            "exited(-)",
            "1",
            "-",
        ]
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
