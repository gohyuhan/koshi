//! Tests for the state a session server carries across a process-image swap:
//! a populated server drained into a resume file and rebuilt from it, what the
//! drain leaves behind, a sequence the swap cut in half finishing in the next
//! image, what the header still yields when the body cannot be read, what a
//! body format this build does not know is answered with, what a body written
//! in an older format reads back as, what a carried pane's size, exit
//! status and applied quit read back as, what the saved views and
//! resume-token hashes the body carries read back as, and what a write over an
//! existing file and a write into a directory that is not there each do.

use std::collections::BTreeMap;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime};

use crate::runtime::command::tests::{
    attach_client_to_only_tab, build_floating_new_pane_args, create_floating_pane,
};
use crate::runtime::event::RuntimeEvent;
use crate::runtime::pty_inbox::InboxSink;
use crate::runtime::saved_view::SavedView;
use crate::server::Server;
use koshi_config::layer::{PartialKoshiConfig, PartialScrollbackConfig};
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, FocusPaneArgs, FocusTarget,
    GridPosition, NewPaneArgs, NewPanePlacement, NewTabArgs, Selection, SelectionKind,
};
use koshi_core::geometry::Rect;
use koshi_core::geometry::{
    AxisPercent, Direction, FloatingPaneDimension, FloatingPanePosition, FloatingPaneSize,
    PixelCellSize, Point, Size,
};
use koshi_core::ids::{ClientId, CommandId, TabId};
use koshi_core::notification::{NotificationQueueOwner, NotificationSeverity};
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, SpawnSpec};
use koshi_layout::content::find_content_rect;
use koshi_layout::solver::solve_layout_with_sizing;
use koshi_layout::tree::LayoutNode;
use koshi_pty::backend::state::{CarriedPtyPane, PtyBackend};
use koshi_session::client::{Client, ClientOrigin, ClientRegistry, FloatingPaneView};
use koshi_terminal::engine::GraphicsEvent;
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};
use koshi_terminal::grid::state::Cell;
use koshi_test_support::fake_pty::FakePtyBackend;
use tempfile::TempDir;

use super::*;

fn read_released_resume_fixture(fixture_bytes: &[u8]) -> (ResumeHeader, ResumeBody) {
    let resume_test_directory = TempDir::new().expect("create resume test directory");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    std::fs::write(&resume_file_path, fixture_bytes).expect("write released resume fixture");
    let (resume_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read released resume header");
    let resume_body = read_resume_body(resume_header.resume_format, &raw_body)
        .expect("migrate released resume body");
    (resume_header, resume_body)
}

#[test]
fn migrate_format_two_restores_tabs_and_screens_with_no_client() {
    let (resume_header, resume_body) =
        read_released_resume_fixture(include_bytes!("fixtures/format_two.json"));
    assert_eq!(resume_header.resume_format, 2);
    assert_eq!(resume_header.session_name, "carried");
    assert_eq!(resume_header.carried_panes.len(), 4);
    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 4);
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    let session = &resumed_server.session_by_id[&resume_header.session_id];
    assert_eq!(session.tabs.len(), 2);
    assert_eq!(session.panes.count_pane_records(), 4);
    assert_eq!(session.clients.count_clients(), 0);
    assert!(!resumed_server.is_awaiting_client());
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
    let first_pane_id = resume_header.carried_panes[0].pane_id;
    assert_eq!(
        get_joined_screen_text(
            resumed_server.terminal_engine_by_pane_id[&first_pane_id].get_terminal_state()
        ),
        "pane 0 output"
    );
}

#[test]
fn migrate_format_four_restores_the_session_with_no_floating_panes_and_no_client() {
    let (resume_header, resume_body) =
        read_released_resume_fixture(include_bytes!("fixtures/format_four.json"));
    assert_eq!(resume_header.resume_format, 4);
    assert_eq!(resume_header.session_name, "carried");
    assert_eq!(resume_header.carried_panes.len(), 4);
    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 4);
    assert_eq!(resume_body.carried_quit, None);
    assert_eq!(
        resume_body.left_running_child_process_ids,
        Vec::<u32>::new()
    );
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    let session = &resumed_server.session_by_id[&resume_header.session_id];
    assert_eq!(session.tabs.len(), 2);
    assert_eq!(session.panes.count_pane_records(), 4);
    assert_eq!(session.clients.count_clients(), 0);
    assert!(!resumed_server.is_awaiting_client());
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
    assert_eq!(session.validate_session_consistency(), Ok(()));
    assert_eq!(session.starting_tab_id, None);
    assert_eq!(
        session.notification_queue.get_owner(),
        NotificationQueueOwner::Session(resume_header.session_id)
    );
    assert_eq!(session.notification_queue.get_revision(), 0);
    assert_eq!(session.notification_queue.list_queued_entries().count(), 0);
    let session_json = serde_json::to_value(session).expect("serialize migrated session");
    assert_eq!(
        session_json["floating_set"],
        serde_json::json!({"members": []})
    );
    let first_pane_id = resume_header.carried_panes[0].pane_id;
    assert_eq!(
        get_joined_screen_text(
            resumed_server.terminal_engine_by_pane_id[&first_pane_id].get_terminal_state()
        ),
        "pane 0 output"
    );
}

#[test]
fn migrate_format_four_with_a_carried_quit_holds_the_quit_and_awaits_no_client() {
    let (resume_header, resume_body) = read_edited_format_four_fixture(|fixture_json| {
        fixture_json["raw_body"]["carried_quit"] = serde_json::json!("Graceful");
    });
    assert_eq!(resume_body.carried_quit, Some(CarriedQuit::Graceful));

    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);

    assert!(resumed_server.is_quit_requested());
    assert!(!resumed_server.should_shutdown_immediately);
    assert!(!resumed_server.is_awaiting_client());
}

#[test]
fn migrate_format_four_then_an_attach_with_no_token_is_the_client_an_external_new_tab_acts_through()
{
    let (resume_header, resume_body) =
        read_released_resume_fixture(include_bytes!("fixtures/format_four.json"));
    let session_id = resume_header.session_id;
    let (mut resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    let carried_tab_ids: Vec<TabId> = resumed_server.session_by_id[&session_id]
        .tabs
        .keys()
        .copied()
        .collect();

    let attachment = resumed_server
        .handle_ipc_attach(
            None,
            None,
            FIRST_CLIENT_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::UNIX_EPOCH,
            false,
            60,
            None,
        )
        .expect("the resumed session accepts the attach");
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    let command_result = resumed_server.submit_command(command_envelope);

    let CommandResult::Ok {
        command_id: applied_command_id,
        ..
    } = command_result
    else {
        panic!("the new tab must be applied, got {command_result:?}");
    };
    assert_eq!(applied_command_id, command_id);
    let session = &resumed_server.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .list_attached_clients()
            .map(Client::get_client_id)
            .collect::<Vec<ClientId>>(),
        vec![attachment.client_id]
    );
    let new_tab_ids: Vec<TabId> = session
        .tabs
        .keys()
        .copied()
        .filter(|tab_id| !carried_tab_ids.contains(tab_id))
        .collect();
    assert_eq!(new_tab_ids.len(), 1);
    assert_eq!(
        session
            .clients
            .get_client_by_id(attachment.client_id)
            .expect("the attached client")
            .get_active_tab_id(),
        new_tab_ids[0]
    );
}

/// The key of the format-4 fixture's session.
const FORMAT_FOUR_SESSION_KEY: &str = "00000000-0000-7000-8000-abcd00000001";

/// The key of the format-4 fixture's first tab, which holds panes 4, 5 and 6.
const FORMAT_FOUR_FIRST_TAB_KEY: &str = "00000000-0000-7000-8000-abcd00000003";

/// Read the format-4 fixture after `edit_fixture_json` changes its JSON.
fn read_edited_format_four_fixture(
    edit_fixture_json: impl FnOnce(&mut serde_json::Value),
) -> (ResumeHeader, ResumeBody) {
    let mut fixture_json: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/format_four.json"))
            .expect("the format-4 fixture is JSON");
    edit_fixture_json(&mut fixture_json);
    read_released_resume_fixture(
        &serde_json::to_vec(&fixture_json).expect("the edited fixture serializes"),
    )
}

/// The format-4 fixture's first tab id.
fn get_format_four_first_tab_id() -> TabId {
    serde_json::from_value(serde_json::json!(FORMAT_FOUR_FIRST_TAB_KEY)).expect("a tab id")
}

#[test]
fn migrate_format_four_with_a_shown_recovery_notice_restores_the_session_not_restored_error() {
    let (resume_header, resume_body) = read_edited_format_four_fixture(|fixture_json| {
        fixture_json["raw_body"]["session_by_id"][FORMAT_FOUR_SESSION_KEY]
            ["is_recovery_notice_visible"] = serde_json::json!(true);
    });
    assert_eq!(resume_header.resume_format, 4);

    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);

    let notification_queue =
        &resumed_server.session_by_id[&resume_header.session_id].notification_queue;
    assert_eq!(
        notification_queue
            .list_queued_entries()
            .map(|notification_entry| (
                notification_entry.severity,
                notification_entry.text.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![(
            NotificationSeverity::Error,
            "the session could not be restored after the restart; a new shell started"
        )]
    );
}

/// Each `resize_delta` of the root split of `layout_tree`.
fn list_root_resize_deltas(layout_tree: &LayoutNode) -> Vec<i32> {
    let LayoutNode::Split(root_split) = layout_tree else {
        panic!("the tab holds a split");
    };
    root_split
        .weights
        .iter()
        .map(|size_weight| size_weight.resize_delta)
        .collect()
}

#[test]
fn migrate_format_four_keeps_the_pane_sizes_beside_a_preferred_size() {
    // The first tab splits pane 4 beside a pair, 22 columns wide. With pane 4
    // aiming for 6 columns and resize deltas `+3` and `-3`, the format-4 solve
    // gave 6 and 16 of 22 columns: 4 and 14 inside the borders, as the header
    // carries them. The deltas are rewritten to `0` and `0`, and the panes
    // keep 6 and 16 columns. Left as `+3` and `-3`, the solve would give 9 and
    // 13.
    let (resume_header, resume_body) = read_edited_format_four_fixture(|fixture_json| {
        let root_weights = &mut fixture_json["raw_body"]["session_by_id"][FORMAT_FOUR_SESSION_KEY]
            ["tabs"][FORMAT_FOUR_FIRST_TAB_KEY]["layout"]["Split"]["weights"];
        root_weights[0]["preferred_cell_count"] = serde_json::json!(6);
        root_weights[0]["resize_delta"] = serde_json::json!(3);
        root_weights[1]["resize_delta"] = serde_json::json!(-3);
        let carried_panes = &mut fixture_json["header"]["carried_panes"];
        carried_panes[0]["column_count"] = serde_json::json!(4);
        carried_panes[1]["column_count"] = serde_json::json!(14);
        carried_panes[2]["column_count"] = serde_json::json!(14);
    });
    assert_eq!(resume_header.resume_format, 4);

    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);

    let first_layout_tree = resumed_server.session_by_id[&resume_header.session_id].tabs
        [&get_format_four_first_tab_id()]
        .get_layout_tree();
    assert_eq!(list_root_resize_deltas(first_layout_tree), [0, 0]);
    let layout_solve = solve_layout_with_sizing(
        first_layout_tree,
        Rect::from_size_at_origin(Size {
            column_count: 22,
            row_count: 9,
        }),
        resumed_server.get_pane_sizing(),
    );
    let first_pane_id = resume_header.carried_panes[0].pane_id;
    assert_eq!(
        layout_solve
            .find_pane_rect(first_pane_id)
            .expect("pane 4 is solved")
            .size
            .column_count,
        6
    );
}

#[test]
fn migrate_format_four_with_no_client_keeps_the_sizes_the_header_carries() {
    // The session holds no client, and its first tab lays panes 4, 5 and 6
    // side by side: resize deltas `+60`, `-60` and `0`, pane 5 aiming for 10
    // columns. The format-4 solve drew them at 80, 6 and 4 of 90 columns, and
    // the header carries 78, 4 and 2 by 22 rows. The deltas become `+50`, `-4`
    // and `-46`. Without the carried sizes they would read `+60`, `0` and
    // `-60`.
    let (resume_header, resume_body) = read_edited_format_four_fixture(|fixture_json| {
        let session_json = &mut fixture_json["raw_body"]["session_by_id"][FORMAT_FOUR_SESSION_KEY];
        session_json["clients"]["client_by_id"] = serde_json::json!({});
        let pane_keys = [
            "00000000-0000-7000-8000-abcd00000004",
            "00000000-0000-7000-8000-abcd00000005",
            "00000000-0000-7000-8000-abcd00000006",
        ];
        session_json["tabs"][FORMAT_FOUR_FIRST_TAB_KEY]["layout"] = serde_json::json!({
            "Split": {
                "direction": "Horizontal",
                "children": pane_keys.map(|pane_key| serde_json::json!({"Pane": pane_key})),
                "weights": [
                    {"primary_constraint": {"Flex": 1}, "minimum_cell_count": null, "preferred_cell_count": null, "resize_delta": 60},
                    {"primary_constraint": {"Flex": 1}, "minimum_cell_count": null, "preferred_cell_count": 10, "resize_delta": -60},
                    {"primary_constraint": {"Flex": 1}, "minimum_cell_count": null, "preferred_cell_count": null, "resize_delta": 0},
                ],
                "active_child_index": 0,
            }
        });
        let carried_panes = &mut fixture_json["header"]["carried_panes"];
        for (carried_pane_index, column_count) in [78, 4, 2].into_iter().enumerate() {
            carried_panes[carried_pane_index]["column_count"] = serde_json::json!(column_count);
            carried_panes[carried_pane_index]["row_count"] = serde_json::json!(22);
        }
    });

    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);

    let session = &resumed_server.session_by_id[&resume_header.session_id];
    assert_eq!(session.clients.list_attached_clients().count(), 0);
    let first_layout_tree = session.tabs[&get_format_four_first_tab_id()].get_layout_tree();
    assert_eq!(list_root_resize_deltas(first_layout_tree), [50, -4, -46]);
    let layout_solve = solve_layout_with_sizing(
        first_layout_tree,
        Rect::from_size_at_origin(Size {
            column_count: 90,
            row_count: 24,
        }),
        resumed_server.get_pane_sizing(),
    );
    assert_eq!(
        resume_header.carried_panes[..3]
            .iter()
            .map(|carried_pane| {
                find_content_rect(&layout_solve, carried_pane.pane_id)
                    .expect("every pane shows content")
                    .size
                    .column_count
            })
            .collect::<Vec<u16>>(),
        [78, 4, 2]
    );
}

#[test]
fn migrate_format_four_keeps_other_panes_when_one_pane_state_is_unreadable() {
    let mut fixture_json: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/format_four.json"))
            .expect("format four fixture is JSON");
    let damaged_pane_key = fixture_json["header"]["carried_panes"][0]["pane_id"]
        .as_str()
        .expect("released pane key")
        .to_string();
    let damaged_pane_id: PaneId =
        serde_json::from_value(serde_json::Value::String(damaged_pane_key.clone()))
            .expect("released pane id");
    fixture_json["raw_body"]["carried_pane_state_by_pane_id"]
        .get_mut(&damaged_pane_key)
        .expect("released pane state")["terminal_state"] = serde_json::json!("unreadable");
    let damaged_fixture_bytes = serde_json::to_vec(&fixture_json).expect("encode damaged pane");

    let (resume_header, resume_body) = read_released_resume_fixture(&damaged_fixture_bytes);

    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 3);
    assert!(!resume_body
        .carried_pane_state_by_pane_id
        .contains_key(&damaged_pane_id));
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    let session = &resumed_server.session_by_id[&resume_header.session_id];
    assert_eq!(session.tabs.len(), 2);
    assert_eq!(session.panes.count_pane_records(), 4);
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
}

#[test]
fn migrate_format_three_without_image_fields_restores_running_panes_and_no_client() {
    let mut fixture_json: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/format_three.json"))
            .expect("parse format-three fixture");
    for terminal_state in fixture_json["body"]["engines"]
        .as_object_mut()
        .expect("released terminal engines")
        .values_mut()
    {
        terminal_state
            .as_object_mut()
            .expect("released terminal state")
            .retain(|field_name, _| {
                matches!(
                    field_name.as_str(),
                    "primary"
                        | "alternate"
                        | "active"
                        | "primary_cursor"
                        | "alternate_cursor"
                        | "primary_render"
                        | "alternate_render"
                        | "modes"
                        | "tab_stops"
                        | "title"
                        | "reported_cwd"
                        | "shell_integration_state"
                        | "shell_integration_facts"
                        | "scrollback"
                        | "primary_scroll_region"
                        | "alternate_scroll_region"
                        | "cluster"
                        | "cluster_base"
                        | "replies"
                )
            });
    }
    fixture_json["body"]
        .as_object_mut()
        .expect("released resume body")
        .retain(|field_name, _| {
            matches!(
                field_name.as_str(),
                "sessions" | "engines" | "undecoded" | "quit"
            )
        });
    let fixture_bytes = serde_json::to_vec(&fixture_json).expect("encode format-three fixture");

    let (resume_header, resume_body) = read_released_resume_fixture(&fixture_bytes);
    assert_eq!(resume_header.resume_format, 3);
    assert_eq!(resume_header.carried_panes.len(), 4);
    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 4);
    let first_pane_id = resume_header.carried_panes[0].pane_id;
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
    let session = &resumed_server.session_by_id[&resume_header.session_id];
    assert_eq!(session.panes.count_pane_records(), 4);
    assert_eq!(session.clients.count_clients(), 0);
    assert!(!resumed_server.is_awaiting_client());
    assert_eq!(
        get_joined_screen_text(
            resumed_server.terminal_engine_by_pane_id[&first_pane_id].get_terminal_state()
        ),
        "pane 0 outputred"
    );
}

#[test]
fn migrate_format_one_and_two_restores_reported_directory_and_saved_cursor_with_no_client() {
    for released_resume_format in [1, 2] {
        let mut released_fixture: serde_json::Value =
            serde_json::from_slice(include_bytes!("fixtures/format_two.json"))
                .expect("parse released resume fixture");
        released_fixture["header"]["format"] = serde_json::json!(released_resume_format);
        let pane_key = released_fixture["header"]["panes"][0]["pane_id"]
            .as_str()
            .expect("first carried pane id")
            .to_string();
        let pane_id: PaneId = serde_json::from_value(serde_json::Value::String(pane_key.clone()))
            .expect("released pane id");
        let released_terminal_state = released_fixture["body"]["engines"]
            .get_mut(&pane_key)
            .expect("first released terminal state");
        released_terminal_state["reported_cwd"] =
            serde_json::json!({"host": null, "path": "/workspace"});
        let saved_render = released_terminal_state["primary_render"].clone();
        released_terminal_state["primary_cursor"]["saved"] = serde_json::json!({
            "row": 0,
            "col": 1,
            "pending_wrap": false,
            "render": saved_render
        });
        let fixture_bytes =
            serde_json::to_vec(&released_fixture).expect("encode released resume fixture");

        let (resume_header, resume_body) = read_released_resume_fixture(&fixture_bytes);
        assert_eq!(resume_header.resume_format, released_resume_format);
        assert_eq!(resume_body.session_by_id.len(), 1);
        assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 4);
        let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
        assert_eq!(
            resumed_server.session_by_id[&resume_header.session_id]
                .clients
                .count_clients(),
            0
        );
        assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
        let terminal_state =
            resumed_server.terminal_engine_by_pane_id[&pane_id].get_terminal_state();
        assert_eq!(get_joined_screen_text(terminal_state), "pane 0 output");
        assert_eq!(
            terminal_state
                .get_current_working_directory()
                .expect("reported working directory")
                .get_working_directory_path(),
            Path::new("/workspace")
        );
        let terminal_json =
            serde_json::to_value(terminal_state).expect("serialize restored terminal");
        assert_eq!(terminal_json["primary_cursor"]["saved"]["column"], 1);
        assert_eq!(
            terminal_json["primary_cursor"]["saved"]["is_wrap_pending"],
            false
        );
    }
}

#[test]
fn migrate_format_three_restores_image_and_open_parser_sequences() {
    let (resume_header, resume_body) =
        read_released_resume_fixture(include_bytes!("fixtures/format_three.json"));
    assert_eq!(resume_header.resume_format, 3);
    assert_eq!(resume_header.carried_panes.len(), 4);
    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 4);
    let first_pane_id = resume_header.carried_panes[0].pane_id;
    let second_pane_id = resume_header.carried_panes[1].pane_id;
    let third_pane_id = resume_header.carried_panes[2].pane_id;
    let first_pane_state = &resume_body.carried_pane_state_by_pane_id[&first_pane_id];
    let terminal_json = serde_json::to_value(&first_pane_state.terminal_state)
        .expect("serialize migrated terminal");
    assert_eq!(
        terminal_json["image_contents"][0]["decoded_image"]["rgba_bytes"],
        serde_json::json!([255, 0, 0, 255])
    );
    let migrated_image_record = serde_json::json!({
        "protocol": "Kitty",
        "action": "TransmitAndDisplay",
        "display": {
            "requested_width": null,
            "requested_height": null,
            "is_aspect_ratio_preserved": true,
            "sixel_background": null,
            "image_id": null,
            "image_number": null,
            "placement_id": null,
            "usage_hints": 0,
            "is_unicode_placeholder": false,
            "z_index": 0,
            "relative_image_id": null,
            "relative_placement_id": null,
            "relative_column_offset": 0,
            "relative_row_offset": 0,
            "requested_column_count": 1,
            "requested_row_count": 1,
            "source_pixel_offset_x": null,
            "source_pixel_offset_y": null,
            "cell_pixel_offset_x": null,
            "cell_pixel_offset_y": null,
            "should_move_cursor": false,
            "response_suppression_level": 0
        },
        "anchor": [1, 7]
    });
    assert_eq!(
        terminal_json["primary_image_placements"],
        serde_json::json!([{
            "image_placement_id": 1,
            "image_record": migrated_image_record,
            "image_content_id": 1,
            "anchor": [1, 7],
            "column_count": 1,
            "row_count": 1,
            "plan": {
                "geometry": {
                    "full_size": {"column_count": 1, "row_count": 1},
                    "cell_offset": {"column": 0, "row": 0}
                },
                "source_rect": [0, 0, 1, 1],
                "target_size": [1, 1],
                "canvas_size": [1, 1],
                "pixel_offset": [0, 0],
                "needs_raster": true
            }
        }])
    );
    let mut migrated_event_record = migrated_image_record.clone();
    migrated_event_record["image"] = serde_json::json!({
        "pixel_width": 1,
        "pixel_height": 1,
        "rgba_bytes": [255, 0, 0, 255]
    });
    assert_eq!(
        serde_json::to_value(&first_pane_state.graphics_events).expect("encode graphics events"),
        serde_json::json!([{"Ok": migrated_event_record}])
    );
    assert_eq!(
        resume_body.carried_pane_state_by_pane_id[&second_pane_id].undecoded_bytes,
        b"\x1b]7;file://host/home/user/Proj"
    );
    assert_eq!(
        resume_body.carried_pane_state_by_pane_id[&third_pane_id]
            .graphics_transport
            .as_ref()
            .expect("graphics parser state")
            .carry_bytes,
        b"\x1b_Gf=32,s=1,v=1;"
    );
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    assert_eq!(
        resumed_server.session_by_id[&resume_header.session_id]
            .tabs
            .len(),
        2
    );
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
}

/// Parses `fixtures/format_three.json` and returns it with the key and the
/// pane id of its first terminal engine.
fn parse_format_three_fixture_with_first_pane() -> (serde_json::Value, String, PaneId) {
    let fixture_json: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/format_three.json"))
            .expect("parse released resume fixture");
    let first_pane_key = fixture_json["body"]["engines"]
        .as_object()
        .expect("released terminal engines")
        .keys()
        .next()
        .expect("released pane")
        .clone();
    let first_pane_id: PaneId =
        serde_json::from_value(serde_json::Value::String(first_pane_key.clone()))
            .expect("released pane id");
    (fixture_json, first_pane_key, first_pane_id)
}

#[test]
fn migrate_format_three_restores_reported_directory_and_screen() {
    let (mut fixture_json, first_pane_key, pane_id) = parse_format_three_fixture_with_first_pane();
    let released_terminal_state = &mut fixture_json["body"]["engines"][&first_pane_key];
    released_terminal_state["reported_cwd"] = serde_json::json!({
        "host": null,
        "path": "/workspace"
    });
    let fixture_bytes = serde_json::to_vec(&fixture_json).expect("encode released resume fixture");

    let (resume_header, resume_body) = read_released_resume_fixture(&fixture_bytes);
    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 4);
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    let terminal_state = resumed_server.terminal_engine_by_pane_id[&pane_id].get_terminal_state();
    assert_eq!(get_joined_screen_text(terminal_state), "pane 0 outputred");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported working directory")
            .get_working_directory_path(),
        Path::new("/workspace")
    );
}

#[test]
fn migrate_format_three_restores_saved_cursor_and_screen() {
    let (mut fixture_json, first_pane_key, pane_id) = parse_format_three_fixture_with_first_pane();
    let released_terminal_state = &mut fixture_json["body"]["engines"][&first_pane_key];
    let mut saved_render = released_terminal_state["primary_render"].clone();
    saved_render["style"]["fg"] = serde_json::json!({"Indexed": 9});
    released_terminal_state["primary_cursor"]["saved"] = serde_json::json!({
        "row": 0,
        "col": 1,
        "pending_wrap": false,
        "render": saved_render
    });
    let fixture_bytes = serde_json::to_vec(&fixture_json).expect("encode released resume fixture");

    let (resume_header, resume_body) = read_released_resume_fixture(&fixture_bytes);
    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 4);
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    let terminal_state = resumed_server.terminal_engine_by_pane_id[&pane_id].get_terminal_state();
    assert_eq!(get_joined_screen_text(terminal_state), "pane 0 outputred");
    let terminal_json = serde_json::to_value(terminal_state).expect("serialize restored terminal");
    assert_eq!(terminal_json["primary_cursor"]["saved"]["column"], 1);
    assert_eq!(
        terminal_json["primary_cursor"]["saved"]["is_wrap_pending"],
        false
    );
    assert_eq!(
        terminal_json["primary_cursor"]["saved"]["render"]["style"]["foreground_color"],
        serde_json::json!({"Indexed": 9})
    );
}

#[test]
fn migrate_format_three_restores_pane_pixel_cell_size() {
    let (mut fixture_json, first_pane_key, pane_id) = parse_format_three_fixture_with_first_pane();
    let released_terminal_state = &mut fixture_json["body"]["engines"][&first_pane_key];
    released_terminal_state["cell_size"] = serde_json::json!({"width": 10, "height": 20});
    let fixture_bytes = serde_json::to_vec(&fixture_json).expect("encode released resume fixture");

    let (resume_header, resume_body) = read_released_resume_fixture(&fixture_bytes);
    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 4);
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    let terminal_state = resumed_server.terminal_engine_by_pane_id[&pane_id].get_terminal_state();
    assert_eq!(
        terminal_state.get_cell_size(),
        PixelCellSize::from_pixel_dimensions(10, 20)
    );
}

#[test]
fn migrate_format_three_keeps_other_screens_and_layout_when_one_pane_is_unreadable() {
    let fixture_bytes = include_bytes!("fixtures/format_three.json");
    let (original_header, original_resume_body) = read_released_resume_fixture(fixture_bytes);
    let damaged_pane_id = original_header.carried_panes[0].pane_id;
    let preserved_pane_id = original_header.carried_panes[1].pane_id;
    let preserved_screen = serde_json::to_value(
        &original_resume_body.carried_pane_state_by_pane_id[&preserved_pane_id].terminal_state,
    )
    .expect("serialize the preserved screen");
    let mut fixture_json: serde_json::Value =
        serde_json::from_slice(fixture_bytes).expect("format three fixture is JSON");
    let damaged_pane_key = fixture_json["header"]["panes"][0]["pane_id"]
        .as_str()
        .expect("released pane key")
        .to_string();
    fixture_json["body"]["engines"]
        .get_mut(&damaged_pane_key)
        .expect("released terminal engine")["tab_stops"] = serde_json::json!("unreadable");
    let damaged_fixture_bytes = serde_json::to_vec(&fixture_json).expect("encode damaged pane");

    let (resume_header, resume_body) = read_released_resume_fixture(&damaged_fixture_bytes);

    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 3);
    assert!(!resume_body
        .carried_pane_state_by_pane_id
        .contains_key(&damaged_pane_id));
    assert_eq!(
        serde_json::to_value(
            &resume_body.carried_pane_state_by_pane_id[&preserved_pane_id].terminal_state
        )
        .expect("serialize the restored screen"),
        preserved_screen
    );
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    let session = &resumed_server.session_by_id[&resume_header.session_id];
    assert_eq!(session.tabs.len(), 2);
    assert_eq!(session.panes.count_pane_records(), 4);
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
    assert_eq!(
        serde_json::to_value(
            resumed_server.terminal_engine_by_pane_id[&preserved_pane_id].get_terminal_state()
        )
        .expect("serialize the running screen"),
        preserved_screen
    );
}

#[test]
fn migrate_format_three_keeps_other_panes_when_one_screen_has_duplicate_fields() {
    let mut fixture_json: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/format_three.json"))
            .expect("format three fixture is JSON");
    let terminal_engines = fixture_json["body"]["engines"]
        .as_object_mut()
        .expect("released terminal engines");
    let damaged_pane_key = terminal_engines
        .keys()
        .next()
        .expect("one released terminal engine")
        .clone();
    let damaged_pane_id: PaneId =
        serde_json::from_value(serde_json::Value::String(damaged_pane_key))
            .expect("released pane id");
    let fixture_text = serde_json::to_string(&fixture_json).expect("encode released fixture");
    let damaged_fixture_text =
        fixture_text.replacen("\"tab_stops\":", "\"tab_stops\":[],\"tab_stops\":", 1);
    assert_ne!(damaged_fixture_text, fixture_text);

    let (resume_header, resume_body) =
        read_released_resume_fixture(damaged_fixture_text.as_bytes());

    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 3);
    assert!(!resume_body
        .carried_pane_state_by_pane_id
        .contains_key(&damaged_pane_id));
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    assert_eq!(
        resumed_server.session_by_id[&resume_header.session_id]
            .panes
            .count_pane_records(),
        4
    );
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
}

#[test]
fn migrate_format_three_keeps_other_panes_when_one_engine_key_repeats() {
    let fixture_json: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/format_three.json"))
            .expect("format three fixture is JSON");
    let terminal_engines = fixture_json["body"]["engines"]
        .as_object()
        .expect("released terminal engines");
    let (damaged_pane_key, terminal_engine) = terminal_engines
        .iter()
        .next()
        .expect("one released terminal engine");
    let damaged_pane_id: PaneId =
        serde_json::from_value(serde_json::Value::String(damaged_pane_key.clone()))
            .expect("released pane id");
    let repeated_engine_field = format!(
        "{}:{}",
        serde_json::to_string(damaged_pane_key).expect("encode pane key"),
        serde_json::to_string(terminal_engine).expect("encode terminal engine")
    );
    let fixture_text = serde_json::to_string(&fixture_json).expect("encode released fixture");
    let damaged_fixture_text = fixture_text.replacen(
        &repeated_engine_field,
        &format!("{repeated_engine_field},{repeated_engine_field}"),
        1,
    );
    assert_ne!(damaged_fixture_text, fixture_text);

    let (resume_header, resume_body) =
        read_released_resume_fixture(damaged_fixture_text.as_bytes());

    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 3);
    assert!(!resume_body
        .carried_pane_state_by_pane_id
        .contains_key(&damaged_pane_id));
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    assert_eq!(
        resumed_server.session_by_id[&resume_header.session_id]
            .panes
            .count_pane_records(),
        4
    );
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
}

#[test]
fn migrate_format_three_keeps_other_panes_when_one_graphics_queue_is_unreadable() {
    let mut fixture_json: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/format_three.json"))
            .expect("format three fixture is JSON");
    let damaged_pane_key = fixture_json["header"]["panes"][0]["pane_id"]
        .as_str()
        .expect("released pane key")
        .to_string();
    let damaged_pane_id: PaneId =
        serde_json::from_value(serde_json::Value::String(damaged_pane_key.clone()))
            .expect("released pane id");
    *fixture_json["body"]["graphics_events"]
        .get_mut(&damaged_pane_key)
        .expect("released graphics queue") = serde_json::json!("unreadable");
    let damaged_fixture_bytes = serde_json::to_vec(&fixture_json).expect("encode damaged queue");

    let (resume_header, resume_body) = read_released_resume_fixture(&damaged_fixture_bytes);

    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 3);
    assert!(!resume_body
        .carried_pane_state_by_pane_id
        .contains_key(&damaged_pane_id));
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    assert_eq!(
        resumed_server.session_by_id[&resume_header.session_id]
            .panes
            .count_pane_records(),
        4
    );
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
}

#[test]
fn migrate_format_three_restores_large_image_bytes_and_running_panes() {
    let released_fixture = include_str!("fixtures/format_three.json");
    let previous_image =
        r#""Ok":{"protocol":"Kitty","image":{"width":1,"height":1,"rgba":[255,0,0,255]}"#;
    let image_bytes = format!("[{}]", vec!["255"; 512 * 512 * 4].join(","));
    let saved_image = format!(
        r#""Ok":{{"protocol":"Kitty","image":{{"width":512,"height":512,"rgba":{image_bytes}}}"#
    );
    let upgraded_fixture = released_fixture.replacen(previous_image, &saved_image, 1);
    assert_ne!(upgraded_fixture, released_fixture);

    let (resume_header, resume_body) = read_released_resume_fixture(upgraded_fixture.as_bytes());

    assert_eq!(resume_header.resume_format, 3);
    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 4);
    let first_pane_id = resume_header.carried_panes[0].pane_id;
    let image_record = resume_body.carried_pane_state_by_pane_id[&first_pane_id].graphics_events[0]
        .as_ref()
        .expect("the queued image reads");
    assert_eq!(image_record.image.rgba_bytes, vec![255; 512 * 512 * 4]);
    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);
    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 4);
}

#[test]
fn migrate_format_three_discards_both_screens_with_the_same_pane_id() {
    let mut released_fixture: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/format_three.json"))
            .expect("released fixture is JSON");
    let terminal_engines = released_fixture["body"]["engines"]
        .as_object_mut()
        .expect("released terminal engines");
    let pane_key = terminal_engines
        .keys()
        .next()
        .expect("one released terminal engine")
        .clone();
    let uppercase_pane_key = pane_key.to_ascii_uppercase();
    assert_ne!(pane_key, uppercase_pane_key);
    let repeated_terminal_engine = terminal_engines[&pane_key].clone();
    terminal_engines.get_mut(&pane_key).expect("first engine")["tab_stops"] =
        serde_json::json!("unreadable");
    terminal_engines.insert(uppercase_pane_key, repeated_terminal_engine);
    let repeated_pane_id: PaneId = serde_json::from_value(serde_json::Value::String(pane_key))
        .expect("released pane id reads");
    let fixture_bytes = serde_json::to_vec(&released_fixture).expect("encode duplicate pane id");

    let (resume_header, resume_body) = read_released_resume_fixture(&fixture_bytes);

    assert_eq!(resume_header.carried_panes.len(), 4);
    assert_eq!(resume_body.carried_pane_state_by_pane_id.len(), 3);
    assert!(!resume_body
        .carried_pane_state_by_pane_id
        .contains_key(&repeated_pane_id));
}

#[test]
fn migrate_format_three_preserves_sixel_source_and_palette() {
    let (mut fixture_json, first_pane_key, first_pane_id) =
        parse_format_three_fixture_with_first_pane();
    let released_terminal_state = &mut fixture_json["body"]["engines"][&first_pane_key];
    let mut sixel_palette = released_terminal_state["sixel_palette"].clone();
    sixel_palette[2] = serde_json::json!([255, 0, 0]);
    released_terminal_state["sixel_palette"] = sixel_palette.clone();
    released_terminal_state["image_contents"][0]["sixel"] = serde_json::json!({
        "indexed": {"width": 1, "height": 1, "indices": [2],
                    "aspect_vertical": 1, "aspect_horizontal": 1},
        "palette": sixel_palette,
        "shared_palette": false
    });
    let fixture_bytes = serde_json::to_vec(&fixture_json).expect("encode saved Sixel image");

    let (resume_header, resume_body) = read_released_resume_fixture(&fixture_bytes);

    assert_eq!(resume_header.resume_format, 3);
    let terminal_json = serde_json::to_value(
        &resume_body.carried_pane_state_by_pane_id[&first_pane_id].terminal_state,
    )
    .expect("serialize the migrated terminal");
    assert_eq!(
        terminal_json["image_contents"][0]["sixel"],
        serde_json::json!({
            "indexed_image": {
                "width_pixels": 1,
                "height_pixels": 1,
                "pixel_register_indices": [2],
                "pixel_aspect_vertical": 1,
                "pixel_aspect_horizontal": 1
            },
            "sixel_palette": sixel_palette,
            "is_shared_palette": false
        })
    );
}

#[test]
fn migrate_format_three_preserves_retained_animation_frames() {
    let (mut fixture_json, first_pane_key, first_pane_id) =
        parse_format_three_fixture_with_first_pane();
    fixture_json["body"]["engines"][&first_pane_key]["image_contents"][0]["animation"] = serde_json::json!({
        "frames": [{
            "image": {"width": 1, "height": 1, "rgba": [255, 0, 0, 255]},
            "delay": {"numerator_ms": 100, "denominator_ms": 1},
            "gapless": false
        }],
        "loop_policy": "Infinite"
    });
    let fixture_bytes = serde_json::to_vec(&fixture_json).expect("encode saved animation");

    let (_resume_header, resume_body) = read_released_resume_fixture(&fixture_bytes);

    let terminal_json = serde_json::to_value(
        &resume_body.carried_pane_state_by_pane_id[&first_pane_id].terminal_state,
    )
    .expect("serialize the migrated terminal");
    assert_eq!(
        terminal_json["image_contents"][0]["animation"],
        serde_json::json!({
            "frames": [{
                "decoded_image": {"pixel_width": 1, "pixel_height": 1, "rgba_bytes": [255, 0, 0, 255]},
                "frame_delay": {"numerator_ms": 100, "denominator_ms": 1},
                "is_gapless": false
            }],
            "loop_policy": "Infinite"
        })
    );
}

/// The viewport of the first client, 80×24. The session is bootstrapped with
/// this client.
const FIRST_CLIENT_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The viewport of the second client, 100×30.
const SECOND_CLIENT_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 30,
};

/// A server built by [`build_populated_server`], with the ids of the session
/// it serves, its two clients, and its two tabs.
struct PopulatedServer {
    server: Server,
    /// A sender of the server's runtime inbox, held for the life of the test.
    _inbox_sender: mpsc::Sender<RuntimeEvent>,
    session_id: SessionId,
    first_client_id: ClientId,
    second_client_id: ClientId,
    first_tab_id: TabId,
    second_tab_id: TabId,
}

/// Return row `row_index` of a screen as text; a blank cell reads as a space.
fn get_terminal_row(terminal_state: &TerminalState, row_index: u16) -> String {
    let (_, column_count) = terminal_state.get_active_grid().get_grid_dimensions();
    (0..column_count)
        .map(|column_index| {
            terminal_state
                .get_active_grid()
                .get_cell(row_index, column_index)
                .map_or(' ', Cell::get_character)
        })
        .collect()
}

/// Return every row of a screen joined into one line, with the trailing blank
/// cells of the last row dropped. A line that wrapped across rows reads back
/// whole.
fn get_joined_screen_text(terminal_state: &TerminalState) -> String {
    let (row_count, _) = terminal_state.get_active_grid().get_grid_dimensions();
    (0..row_count)
        .map(|row_index| get_terminal_row(terminal_state, row_index))
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// A pane state holding a blank 80×24 screen and nothing else.
fn build_blank_carried_pane_state() -> CarriedPaneState {
    CarriedPaneState {
        terminal_state: koshi_terminal::engine::TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        })
        .into_terminal_state(),
        undecoded_bytes: Vec::new(),
        graphics_events: Vec::new(),
        graphics_transport: None,
        synchronized_output: None,
    }
}

/// [`build_blank_carried_pane_state`] as JSON.
fn build_blank_carried_pane_state_json() -> serde_json::Value {
    serde_json::to_value(build_blank_carried_pane_state()).expect("the pane state encodes")
}

/// The sentence decoding `pane_state_json` as a [`CarriedPaneState`] fails
/// with, up to the position it names. Panics when the state reads.
fn format_pane_state_parse_error(pane_state_json: &serde_json::Value) -> String {
    let parse_error = serde_json::from_str::<CarriedPaneState>(&pane_state_json.to_string())
        .expect_err("the pane state is refused");
    parse_error
        .to_string()
        .split(" at line ")
        .next()
        .expect("a sentence")
        .to_owned()
}

/// The body text naming no session and carrying `pane_states_text`, the text
/// of one JSON object, as the map of pane states.
fn build_raw_resume_body(pane_states_text: &str) -> Box<serde_json::value::RawValue> {
    serde_json::value::RawValue::from_string(format!(
        r#"{{"session_by_id":{{}},"carried_pane_state_by_pane_id":{pane_states_text},"carried_quit":null,"saved_view_store":{{"connection_token_hash_by_client_id":{{}},"saved_view_records":[]}},"left_running_child_process_ids":[]}}"#
    ))
    .expect("the body is json")
}

/// Run `command` as a keybinding of `client_id`, and panic unless it was applied.
fn apply_keybinding_command(server: &mut Server, client_id: ClientId, command: Command) {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        command,
    );
    let command_id = command_envelope.command_id;
    match server.submit_command(command_envelope) {
        CommandResult::Ok {
            command_id: applied_command_id,
            ..
        } => assert_eq!(applied_command_id, command_id),
        unexpected_command_result => {
            panic!("the command must be applied, got {unexpected_command_result:?}")
        }
    }
}

/// A server holding one session with two tabs and four panes — the first tab
/// split twice so its tree nests a split inside a split — two clients on
/// different tabs with their own focus, zoom, scroll offset and selection, and
/// output fed into every pane's engine.
fn build_populated_server() -> PopulatedServer {
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(inbox_sender.clone()),
    )));
    let mut server = Server::from_runtime_parts(pty_backend, inbox_receiver, Arc::default());

    let session_id = SessionId::new();
    server
        .bootstrap_session(
            session_id,
            "carried".to_string(),
            FIRST_CLIENT_VIEWPORT_SIZE,
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap the session");
    let first_client_id = attach_client_to_only_tab(
        &mut server,
        session_id,
        FIRST_CLIENT_VIEWPORT_SIZE,
        SystemTime::UNIX_EPOCH,
    );
    let first_tab_id = *server.session_by_id[&session_id]
        .tabs
        .keys()
        .next()
        .expect("the bootstrapped tab");

    // Two splits on the first tab: rightward, then downward inside the pane the
    // first split created. The tree holds a split inside a split.
    for direction in [Direction::Right, Direction::Down] {
        apply_keybinding_command(
            &mut server,
            first_client_id,
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Split {
                    source_pane_id: None,
                    tab_id: None,
                    direction,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        );
    }

    // A second tab, which moves the first client onto it and gives the session
    // its fourth pane.
    apply_keybinding_command(
        &mut server,
        first_client_id,
        Command::NewTab(NewTabArgs::default()),
    );
    let second_tab_id = *server.session_by_id[&session_id]
        .tabs
        .keys()
        .find(|&&tab_id| tab_id != first_tab_id)
        .expect("the created tab");

    // A second client on the first tab. The two clients hold different active
    // tabs.
    let second_client_id = ClientId::new();
    server.handle_client_attach(
        session_id,
        second_client_id,
        SECOND_CLIENT_VIEWPORT_SIZE,
        None,
        first_tab_id,
        None,
        SystemTime::UNIX_EPOCH,
        false,
    );

    let first_tab_pane_ids = list_tab_pane_ids(&server, session_id, first_tab_id);
    // The second client focuses the last pane of the first tab. The two clients
    // hold different focus and different tabs.
    apply_keybinding_command(
        &mut server,
        second_client_id,
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(first_tab_pane_ids[2]),
            client_id: None,
        }),
    );
    let session = server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session");
    let first_client = session
        .clients
        .get_client_mut_by_id(first_client_id)
        .expect("the first client");
    first_client.zoom_pane(first_tab_id, first_tab_pane_ids[0]);
    first_client.set_scroll_offset(first_tab_pane_ids[1], 7);
    let second_client = session
        .clients
        .get_client_mut_by_id(second_client_id)
        .expect("the second client");
    second_client.zoom_pane(first_tab_id, first_tab_pane_ids[2]);
    second_client.set_scroll_offset(first_tab_pane_ids[0], 12);
    second_client.set_selection(
        first_tab_pane_ids[1],
        Selection {
            selection_kind: SelectionKind::Word,
            anchor: GridPosition {
                row_index: 3,
                column_index: 4,
            },
            cursor: GridPosition {
                row_index: 3,
                column_index: 9,
            },
        },
    );

    // Pane `n` in tab order then layout order is fed `pane n output`.
    for (pane_index, pane_id) in list_session_pane_ids(&server, session_id)
        .into_iter()
        .enumerate()
    {
        server.handle_pty_output(pane_id, format!("pane {pane_index} output").as_bytes());
    }

    PopulatedServer {
        server,
        _inbox_sender: inbox_sender,
        session_id,
        first_client_id,
        second_client_id,
        first_tab_id,
        second_tab_id,
    }
}

/// Builds [`build_populated_server`], carries its session out over the
/// panes [`build_carried_pty_panes`] reports, and returns the drained server,
/// those carried PTY panes, and the header and body the carry produced.
fn carry_out_populated_server() -> (
    PopulatedServer,
    Vec<CarriedPtyPane>,
    ResumeHeader,
    ResumeBody,
) {
    let mut populated_server = build_populated_server();
    let carried_pty_panes =
        build_carried_pty_panes(&populated_server.server, populated_server.session_id);
    let (resume_header, resume_body) = populated_server
        .server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");
    (
        populated_server,
        carried_pty_panes,
        resume_header,
        resume_body,
    )
}

/// Reads the JSON of the resume file at `resume_file_path`, applies
/// `update_file_json` to it, and writes the result back to the same path.
fn update_resume_file_json(
    resume_file_path: &Path,
    update_file_json: impl FnOnce(&mut serde_json::Value),
) {
    let mut resume_file_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(resume_file_path).expect("read the resume file"))
            .expect("the resume file is JSON");
    update_file_json(&mut resume_file_json);
    std::fs::write(
        resume_file_path,
        serde_json::to_vec(&resume_file_json).expect("encode the resume file"),
    )
    .expect("rewrite the resume file");
}

/// Builds a carried pane with process id `4242`, a 78×20 size, descriptor `9`
/// on `/dev/pts/9`, and `exit_status`.
fn build_test_carried_pane(exit_status: Option<ExitStatus>) -> CarriedPane {
    CarriedPane {
        pane_id: PaneId::new(),
        process_id: 4242,
        row_count: 20,
        column_count: 78,
        terminal_fd: Some(9),
        terminal_name: Some("/dev/pts/9".to_string()),
        exit_status,
    }
}

/// Return the pane ids of `tab_id`, in layout order.
fn list_tab_pane_ids(server: &Server, session_id: SessionId, tab_id: TabId) -> Vec<PaneId> {
    server.session_by_id[&session_id].tabs[&tab_id]
        .get_layout_tree()
        .list_leaf_pane_ids()
}

/// Return every pane id of `session_id`: the tiled panes in tab order then
/// layout order, then the floating panes in the order they became floating.
fn list_session_pane_ids(server: &Server, session_id: SessionId) -> Vec<PaneId> {
    let session = &server.session_by_id[&session_id];
    session
        .tabs
        .values()
        .flat_map(|tab| tab.get_layout_tree().list_leaf_pane_ids())
        .chain(session.floating_set.list_pane_ids())
        .collect()
}

/// Build one carried pane per live pane, as the concrete PTY backend reports them:
/// a made-up process id and descriptor per pane, and a size the server's own
/// carried pane overrides.
fn build_carried_pty_panes(server: &Server, session_id: SessionId) -> Vec<CarriedPtyPane> {
    list_session_pane_ids(server, session_id)
        .into_iter()
        .enumerate()
        .map(|(pane_index, pane_id)| CarriedPtyPane {
            pane_id,
            #[cfg(unix)]
            terminal_fd: Some(20 + pane_index as i32),
            process_id: 5000 + pane_index as u32,
            pty_size: PtySize {
                column_count: 1,
                row_count: 1,
            },
            exit_status: None,
        })
        .collect()
}

/// Build a header for `session_id` named `carried`, in the format this build
/// writes, naming `carried_panes`.
fn build_resume_header(session_id: SessionId, carried_panes: Vec<CarriedPane>) -> ResumeHeader {
    ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id,
        session_name: "carried".to_string(),
        carried_panes,
    }
}

/// Build a body holding no session, no screen and no held bytes, carrying
/// `carried_quit`.
fn build_resume_body_with_quit(carried_quit: Option<CarriedQuit>) -> ResumeBody {
    ResumeBody {
        session_by_id: HashMap::new(),
        carried_pane_state_by_pane_id: HashMap::new(),
        carried_quit,
        saved_view_store: SavedViewStore::default(),
        left_running_child_process_ids: Vec::new(),
    }
}

/// Build a resumed server from `resume_body`, driving every pane
/// `resume_header` names at the size it carries, with no startup config and no
/// received exit.
fn build_resumed_server(
    resume_header: &ResumeHeader,
    resume_body: ResumeBody,
) -> (Server, mpsc::Sender<RuntimeEvent>) {
    build_resumed_server_with_config_and_exit_statuses(
        resume_header,
        resume_body,
        None,
        HashMap::new(),
    )
}

/// Build a resumed server from `resume_body`, driving every pane
/// `resume_header` names at the size it carries. `startup_app_config` is the
/// config the server starts with. `exit_status_by_pane_id` holds the exits
/// received before the resume.
fn build_resumed_server_with_config_and_exit_statuses(
    resume_header: &ResumeHeader,
    resume_body: ResumeBody,
    startup_app_config: Option<PartialKoshiConfig>,
    exit_status_by_pane_id: HashMap<PaneId, ExitStatus>,
) -> (Server, mpsc::Sender<RuntimeEvent>) {
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(inbox_sender.clone()),
    )));
    let pty_size_by_pane_id: HashMap<PaneId, PtySize> = resume_header
        .carried_panes
        .iter()
        .map(|carried_pane| (carried_pane.pane_id, carried_pane.get_pty_size()))
        .collect();
    let server = Server::resume(
        pty_backend,
        inbox_receiver,
        Arc::default(),
        startup_app_config,
        resume_body,
        resume_header.resume_format,
        pty_size_by_pane_id,
        exit_status_by_pane_id,
    );
    (server, inbox_sender)
}

#[test]
fn a_carried_session_reads_back_with_every_tab_pane_client_and_screen() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated_server = build_populated_server();
    let session_id = populated_server.session_id;
    let carried_pty_panes = build_carried_pty_panes(&populated_server.server, session_id);
    let expected_tabs = populated_server.server.session_by_id[&session_id]
        .tabs
        .clone();
    let expected_pane_registry = populated_server.server.session_by_id[&session_id]
        .panes
        .clone();
    let expected_pty_size_by_pane_id = populated_server.server.pty_size_by_pane_id.clone();
    let notification_queue = &mut populated_server
        .server
        .session_by_id
        .get_mut(&session_id)
        .expect("the populated session")
        .notification_queue;
    notification_queue.post_core_entry(
        NotificationSeverity::Error,
        "pane …a1b2 killed by signal 9",
        NonZeroU32::MIN,
        SystemTime::UNIX_EPOCH,
    );
    notification_queue.post_core_entry(
        NotificationSeverity::Error,
        "pane …a1b2 killed by signal 9",
        NonZeroU32::MIN,
        SystemTime::UNIX_EPOCH,
    );
    let expected_notification_queue = notification_queue.clone();

    let (resume_header, resume_body) = populated_server
        .server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");
    let (resumed_server, _inbox_sender) = build_resumed_server(&read_header, read_body);

    assert_eq!(
        read_header, resume_header,
        "the header must read back unchanged"
    );
    assert_eq!(read_header.session_id, session_id);
    assert_eq!(read_header.session_name, "carried");
    assert_eq!(
        resumed_server.session_by_id.len(),
        1,
        "one session must come back"
    );
    let session = &resumed_server.session_by_id[&session_id];
    assert_eq!(session.session_name, "carried");
    assert_eq!(
        session.notification_queue, expected_notification_queue,
        "the notification queue keeps its epoch, revision and entries"
    );
    assert_eq!(session.notification_queue.get_revision(), 2);
    assert_eq!(session.tabs, expected_tabs, "every tab and its layout tree");
    assert_eq!(
        session.panes, expected_pane_registry,
        "every pane carried by the header"
    );
    assert_eq!(
        session.clients.count_clients(),
        2,
        "both clients must come back"
    );

    let resumed_first_client = session
        .clients
        .get_client_by_id(populated_server.first_client_id)
        .expect("the first client");
    let first_tab_pane_ids = expected_tabs[&populated_server.first_tab_id]
        .get_layout_tree()
        .list_leaf_pane_ids();
    assert_eq!(
        resumed_first_client.get_active_tab_id(),
        populated_server.second_tab_id
    );
    assert_eq!(
        resumed_first_client.get_viewport_size(),
        FIRST_CLIENT_VIEWPORT_SIZE
    );
    assert_eq!(
        resumed_first_client.get_zoomed_pane_id(populated_server.first_tab_id),
        Some(first_tab_pane_ids[0])
    );
    assert_eq!(
        resumed_first_client.get_scroll_offset(first_tab_pane_ids[1]),
        7
    );
    assert_eq!(
        resumed_first_client.get_selection(first_tab_pane_ids[1]),
        None
    );

    let resumed_second_client = session
        .clients
        .get_client_by_id(populated_server.second_client_id)
        .expect("the second client");
    assert_eq!(
        resumed_second_client.get_active_tab_id(),
        populated_server.first_tab_id
    );
    assert_eq!(
        resumed_second_client.get_viewport_size(),
        SECOND_CLIENT_VIEWPORT_SIZE
    );
    assert_eq!(
        resumed_second_client.get_focused_pane_id(populated_server.first_tab_id),
        Some(first_tab_pane_ids[2])
    );
    assert_eq!(
        resumed_second_client.get_zoomed_pane_id(populated_server.first_tab_id),
        Some(first_tab_pane_ids[2])
    );
    assert_eq!(
        resumed_second_client.get_scroll_offset(first_tab_pane_ids[0]),
        12
    );
    assert_eq!(
        resumed_second_client.get_selection(first_tab_pane_ids[1]),
        Some(Selection {
            selection_kind: SelectionKind::Word,
            anchor: GridPosition {
                row_index: 3,
                column_index: 4
            },
            cursor: GridPosition {
                row_index: 3,
                column_index: 9
            },
        })
    );

    assert_eq!(
        resume_body.carried_pane_state_by_pane_id.len(),
        4,
        "four panes must have a screen"
    );
    for (pane_id, carried_pane_state) in &resume_body.carried_pane_state_by_pane_id {
        assert_eq!(
            resumed_server.terminal_engine_by_pane_id[pane_id].get_terminal_state(),
            &carried_pane_state.terminal_state,
            "pane {pane_id} must come back with the screen it went out with"
        );
    }
    for (pane_index, carried_pty_pane) in carried_pty_panes.iter().enumerate() {
        assert_eq!(
            get_terminal_row(
                resumed_server.terminal_engine_by_pane_id[&carried_pty_pane.pane_id]
                    .get_terminal_state(),
                0
            )
            .trim_end(),
            format!("pane {pane_index} output")
        );
    }
    assert_eq!(
        resumed_server.pty_size_by_pane_id, expected_pty_size_by_pane_id,
        "every pane's size"
    );
    assert_eq!(
        resumed_server.live_pane_ids,
        list_carried_pty_pane_ids(&carried_pty_panes),
        "every carried pane must come back live"
    );
}

/// Return the pane id of every pane in `carried_pty_panes`.
fn list_carried_pty_pane_ids(carried_pty_panes: &[CarriedPtyPane]) -> HashSet<PaneId> {
    carried_pty_panes
        .iter()
        .map(|carried_pty_pane| carried_pty_pane.pane_id)
        .collect()
}

/// A `60%x100%` floating pane the first client pinned at `(10, 3)` and a
/// `40x10` floating pane it minimized cross the resume file. Both come back as
/// members at their desired sizes, in the order they became floating, live at
/// their carried PTY sizes with their screens. The first client's pin,
/// minimize, shown order and floating focus come back as they went out.
#[test]
fn floating_panes_and_a_clients_floating_view_come_back_whole_after_the_resume_file() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated_server = build_populated_server();
    let session_id = populated_server.session_id;
    let first_client_id = populated_server.first_client_id;
    let percent_floating_pane_size = FloatingPaneSize {
        width: FloatingPaneDimension::Percent(AxisPercent::try_from(60).expect("60 is a percent")),
        height: FloatingPaneDimension::Percent(
            AxisPercent::try_from(100).expect("100 is a percent"),
        ),
    };
    let cells_floating_pane_size = FloatingPaneSize {
        width: FloatingPaneDimension::Cells(NonZeroU16::new(40).expect("40 is nonzero")),
        height: FloatingPaneDimension::Cells(NonZeroU16::new(10).expect("10 is nonzero")),
    };
    let pinned_pane_id = create_floating_pane(
        &mut populated_server.server,
        CommandSource::from_key_binding(first_client_id),
        build_floating_new_pane_args(Some(percent_floating_pane_size), None, false),
    );
    let minimized_pane_id = create_floating_pane(
        &mut populated_server.server,
        CommandSource::from_key_binding(first_client_id),
        build_floating_new_pane_args(Some(cells_floating_pane_size), None, false),
    );
    {
        let first_client = populated_server
            .server
            .get_client_mut(first_client_id)
            .expect("the first client");
        first_client.pin_floating_pane(pinned_pane_id, Point { column: 10, row: 3 });
        assert!(first_client.minimize_floating_pane(minimized_pane_id));
    }
    populated_server
        .server
        .handle_pty_output(pinned_pane_id, b"pinned output");
    populated_server
        .server
        .handle_pty_output(minimized_pane_id, b"minimized output");
    let expected_floating_members = populated_server.server.session_by_id[&session_id]
        .floating_set
        .list_members()
        .to_vec();
    assert_eq!(
        expected_floating_members
            .iter()
            .map(|floating_member| (floating_member.pane_id, floating_member.desired_size))
            .collect::<Vec<_>>(),
        [
            (pinned_pane_id, percent_floating_pane_size),
            (minimized_pane_id, cells_floating_pane_size),
        ]
    );
    let first_client = populated_server.server.session_by_id[&session_id]
        .clients
        .get_client_by_id(first_client_id)
        .expect("the first client");
    let expected_floating_pane_views = first_client.list_floating_pane_views().clone();
    assert_eq!(
        expected_floating_pane_views,
        HashMap::from([
            (
                pinned_pane_id,
                FloatingPaneView {
                    position: FloatingPanePosition::Pinned(Point { column: 10, row: 3 }),
                    is_minimized: false,
                },
            ),
            (
                minimized_pane_id,
                FloatingPaneView {
                    position: FloatingPanePosition::Default,
                    is_minimized: true,
                },
            ),
        ])
    );
    let expected_shown_floating_pane_ids = first_client.list_shown_floating_pane_ids().to_vec();
    assert_eq!(expected_shown_floating_pane_ids, [pinned_pane_id]);
    let expected_focused_floating_pane_id = first_client.get_focused_floating_pane_id();
    assert_eq!(expected_focused_floating_pane_id, Some(pinned_pane_id));
    let expected_pty_size_by_pane_id = populated_server.server.pty_size_by_pane_id.clone();
    let carried_pty_panes = build_carried_pty_panes(&populated_server.server, session_id);

    let (resume_header, resume_body) = populated_server
        .server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");
    let (resumed_server, _inbox_sender) = build_resumed_server(&read_header, read_body);

    let resumed_session = &resumed_server.session_by_id[&session_id];
    assert_eq!(
        resumed_session.floating_set.list_members(),
        expected_floating_members.as_slice()
    );
    let resumed_first_client = resumed_session
        .clients
        .get_client_by_id(first_client_id)
        .expect("the first client");
    assert_eq!(
        resumed_first_client.list_floating_pane_views(),
        &expected_floating_pane_views
    );
    assert_eq!(
        resumed_first_client.list_shown_floating_pane_ids(),
        expected_shown_floating_pane_ids
    );
    assert_eq!(
        resumed_first_client.get_focused_floating_pane_id(),
        expected_focused_floating_pane_id
    );
    assert_eq!(resumed_session.validate_session_consistency(), Ok(()));
    assert_eq!(
        resumed_server.pty_size_by_pane_id, expected_pty_size_by_pane_id,
        "every pane's size, the floating panes' included"
    );
    assert!(resumed_server.live_pane_ids.contains(&pinned_pane_id));
    assert!(resumed_server.live_pane_ids.contains(&minimized_pane_id));
    for (pane_id, expected_row_text) in [
        (pinned_pane_id, "pinned output"),
        (minimized_pane_id, "minimized output"),
    ] {
        assert_eq!(
            get_terminal_row(
                resumed_server.terminal_engine_by_pane_id[&pane_id].get_terminal_state(),
                0
            )
            .trim_end(),
            expected_row_text
        );
    }
}

#[test]
fn carrying_the_state_out_leaves_the_server_holding_nothing() {
    let (populated_server, _carried_pty_panes, _resume_header, resume_body) =
        carry_out_populated_server();

    assert_eq!(
        populated_server.server.terminal_engine_by_pane_id.len(),
        0,
        "every engine must have moved out"
    );
    assert_eq!(
        populated_server.server.session_by_id.len(),
        0,
        "every session must have moved out"
    );
    assert_eq!(
        resume_body.carried_pane_state_by_pane_id.len(),
        4,
        "every engine must be in the body"
    );
    assert_eq!(
        resume_body.session_by_id.len(),
        1,
        "the session must be in the body"
    );
    for (pane_id, carried_pane_state) in &resume_body.carried_pane_state_by_pane_id {
        assert_eq!(
            carried_pane_state.undecoded_bytes,
            Vec::<u8>::new(),
            "pane {pane_id}'s parser was not mid-sequence and holds nothing"
        );
    }
}

#[test]
fn a_report_the_swap_cut_in_half_finishes_in_the_next_image() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated_server = build_populated_server();
    let session_id = populated_server.session_id;
    let reporting_pane_id = list_session_pane_ids(&populated_server.server, session_id)[0];
    let carried_pty_panes = build_carried_pty_panes(&populated_server.server, session_id);

    // The shell reports /home/user/Projects/koshi through OSC 7, and the last
    // chunk before the swap ends after `/Proj`.
    populated_server
        .server
        .handle_pty_output(reporting_pane_id, b"\x1b]7;file://host/home/user/Proj");

    let (resume_header, resume_body) = populated_server
        .server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");
    let undecoded_bytes_by_pane_id: HashMap<PaneId, Vec<u8>> = resume_body
        .carried_pane_state_by_pane_id
        .iter()
        .filter(|(_, carried_pane_state)| !carried_pane_state.undecoded_bytes.is_empty())
        .map(|(pane_id, carried_pane_state)| (*pane_id, carried_pane_state.undecoded_bytes.clone()))
        .collect();
    assert_eq!(
        undecoded_bytes_by_pane_id,
        HashMap::from([(
            reporting_pane_id,
            b"\x1b]7;file://host/home/user/Proj".to_vec()
        )]),
        "only the pane mid-report holds bytes, and it holds all of them"
    );
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");
    let (mut resumed_server, _inbox_sender) = build_resumed_server(&read_header, read_body);

    assert_eq!(
        resumed_server.terminal_engine_by_pane_id[&reporting_pane_id]
            .get_terminal_state()
            .get_current_working_directory(),
        None,
        "a report with no terminator sets no directory"
    );
    let initial_terminal_row = get_terminal_row(
        resumed_server.terminal_engine_by_pane_id[&reporting_pane_id].get_terminal_state(),
        0,
    );

    resumed_server.handle_pty_output(reporting_pane_id, b"ects/koshi\x07");

    let terminal_state =
        resumed_server.terminal_engine_by_pane_id[&reporting_pane_id].get_terminal_state();
    let reported_working_directory = terminal_state
        .get_current_working_directory()
        .expect("the report finished");
    assert_eq!(reported_working_directory.get_host(), Some("host"));
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/home/user/Projects/koshi")
    );
    assert_eq!(
        get_terminal_row(terminal_state, 0),
        initial_terminal_row,
        "the rest of the report joined the sequence instead of printing"
    );
}

#[test]
fn a_body_missing_any_required_field_the_writer_emits_is_refused() {
    let (_populated_server, _carried_pty_panes, _resume_header, resume_body) =
        carry_out_populated_server();
    let body_json = serde_json::to_value(&resume_body).expect("the body encodes");

    for field_name in [
        "session_by_id",
        "carried_pane_state_by_pane_id",
        "saved_view_store",
        "left_running_child_process_ids",
    ] {
        let mut incomplete_body_json = body_json.clone();
        incomplete_body_json
            .as_object_mut()
            .expect("the body is a map")
            .remove(field_name)
            .expect("the writer emits every field");
        let incomplete_body_text = incomplete_body_json.to_string();
        let incomplete_body_column_count = incomplete_body_text.len();
        let raw_resume_body = serde_json::value::RawValue::from_string(incomplete_body_text)
            .expect("the body is json");

        match read_resume_body(RESUME_FORMAT, &raw_resume_body) {
            Err(StorageError::Corrupt { detail }) => assert_eq!(
                detail,
                format!(
                    "resume body is unreadable: missing field `{field_name}` at line 1 column {incomplete_body_column_count}"
                )
            ),
            unexpected_read_result => panic!(
                "expected a body without {field_name} to be corrupt, got {unexpected_read_result:?}"
            ),
        }
    }
}

#[test]
fn the_header_names_every_pane_with_the_size_the_server_holds_for_it() {
    let mut populated_server = build_populated_server();
    let carried_pty_panes =
        build_carried_pty_panes(&populated_server.server, populated_server.session_id);
    let held_pty_size_by_pane_id = populated_server.server.pty_size_by_pane_id.clone();

    let (resume_header, _resume_body) = populated_server
        .server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");

    assert_eq!(resume_header.resume_format, RESUME_FORMAT);
    assert_eq!(
        resume_header.carried_panes.len(),
        4,
        "one carried pane per live pane"
    );
    for (pane_index, carried_pane) in resume_header.carried_panes.iter().enumerate() {
        assert_eq!(carried_pane.pane_id, carried_pty_panes[pane_index].pane_id);
        assert_eq!(carried_pane.process_id, 5000 + pane_index as u32);
        #[cfg(unix)]
        assert_eq!(carried_pane.terminal_fd, Some(20 + pane_index as i32));
        #[cfg(windows)]
        assert_eq!(carried_pane.terminal_fd, None);
        assert_eq!(
            carried_pane.get_pty_size(),
            held_pty_size_by_pane_id[&carried_pane.pane_id],
            "the header carries the size the server holds, not the backend's"
        );
    }
}

#[test]
fn a_pane_the_server_holds_no_size_for_takes_the_size_the_backend_reports() {
    let mut populated_server = build_populated_server();
    let carried_pty_panes =
        build_carried_pty_panes(&populated_server.server, populated_server.session_id);
    let unsized_pane_id = carried_pty_panes[2].pane_id;
    populated_server
        .server
        .pty_size_by_pane_id
        .remove(&unsized_pane_id);

    let (resume_header, _resume_body) = populated_server
        .server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");

    let carried_pane = resume_header
        .carried_panes
        .iter()
        .find(|carried_pane| carried_pane.pane_id == unsized_pane_id)
        .expect("the pane the server holds no size for");
    assert_eq!((carried_pane.column_count, carried_pane.row_count), (1, 1));
}

#[test]
fn an_unreadable_body_still_leaves_every_pane_descriptor_and_process_id() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let (_populated_server, _carried_pty_panes, resume_header, resume_body) =
        carry_out_populated_server();
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    update_resume_file_json(&resume_file_path, |resume_file_json| {
        resume_file_json["raw_body"] = serde_json::json!({ "session_by_id": "not-a-map" });
    });

    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");

    assert_eq!(
        read_header, resume_header,
        "the header survives a broken body"
    );
    match read_resume_body(read_header.resume_format, &raw_body) {
        Err(StorageError::Corrupt { detail }) => {
            assert_eq!(
                detail,
                "resume body is unreadable: invalid type: string \"not-a-map\", expected a map at line 1 column 28"
            );
        }
        unexpected_read_result => panic!("expected a corrupt body, got {unexpected_read_result:?}"),
    }
}

#[test]
fn a_body_format_this_build_does_not_know_is_refused_by_both_numbers() {
    let newer_resume_format = RESUME_FORMAT + 1;

    match read_resume_body(newer_resume_format, serde_json::value::RawValue::NULL) {
        Err(StorageError::Corrupt { detail }) => {
            assert_eq!(
                detail,
                format!(
                    "resume body format {newer_resume_format} is outside the {RESUME_FORMAT_MIN} to {RESUME_FORMAT} range this build reads"
                )
            );
        }
        unexpected_read_result => {
            panic!("expected a refused format, got {unexpected_read_result:?}")
        }
    }
}

#[test]
fn a_pane_state_missing_any_required_field_the_writer_emits_does_not_read() {
    let pane_state_json = build_blank_carried_pane_state_json();

    for field_name in ["terminal_state", "undecoded_bytes", "graphics_events"] {
        let mut incomplete_pane_state_json = pane_state_json.clone();
        incomplete_pane_state_json
            .as_object_mut()
            .expect("the pane state is a map")
            .remove(field_name)
            .expect("the writer emits every field");

        assert_eq!(
            format_pane_state_parse_error(&incomplete_pane_state_json),
            format!("missing field `{field_name}`")
        );
    }
}

#[test]
fn a_pane_state_with_graphics_transport_deeper_than_the_wrapper_limit_does_not_read() {
    let mut nested_graphics_transport = serde_json::json!({ "carry_bytes": [] });
    for _ in 0..9 {
        nested_graphics_transport =
            serde_json::json!({ "screen_inner_transport": nested_graphics_transport });
    }
    let mut pane_state_json = build_blank_carried_pane_state_json();
    pane_state_json["graphics_transport"] = nested_graphics_transport;

    assert_eq!(
        format_pane_state_parse_error(&pane_state_json),
        "graphics wrapper nesting exceeds the supported limit"
    );
}

#[test]
fn a_pane_state_with_graphics_transport_carrying_too_many_bytes_does_not_read() {
    let mut pane_state_json = build_blank_carried_pane_state_json();
    pane_state_json["graphics_transport"] =
        serde_json::json!({ "carry_bytes": vec![0u8; 64 * 1024 + 1] });

    assert_eq!(
        format_pane_state_parse_error(&pane_state_json),
        "graphics carry exceeds 65536 bytes"
    );
}

#[test]
fn a_pane_state_with_queued_image_bytes_that_do_not_match_dimensions_does_not_read() {
    let mut image_event_json = build_queued_image_event_json();
    image_event_json["Ok"]["image"]["rgba_bytes"] = serde_json::json!([255, 0, 0]);
    let mut pane_state_json = build_blank_carried_pane_state_json();
    pane_state_json["graphics_events"] = serde_json::json!([image_event_json]);

    assert_eq!(
        format_pane_state_parse_error(&pane_state_json),
        "decoded image RGBA length does not match its dimensions"
    );
}

#[test]
fn a_pane_state_with_graphics_error_text_over_the_control_limit_does_not_read() {
    let error_event: GraphicsEvent =
        Err(koshi_terminal::graphics::GraphicsError::UnsupportedAction {
            protocol: GraphicsProtocol::Kitty,
            action: String::new(),
        });
    let mut error_event_json =
        serde_json::to_value(error_event).expect("the graphics error is json");
    error_event_json["Err"]["UnsupportedAction"]["action"] = serde_json::Value::String(
        "x".repeat(koshi_terminal::graphics::MAX_GRAPHICS_CONTROL_BYTE_COUNT + 1),
    );
    let mut pane_state_json = build_blank_carried_pane_state_json();
    pane_state_json["graphics_events"] = serde_json::json!([error_event_json]);

    assert_eq!(
        format_pane_state_parse_error(&pane_state_json),
        "graphics error text exceeds 8192 bytes"
    );
}

/// One queued one-pixel red Kitty image, as JSON.
fn build_queued_image_event_json() -> serde_json::Value {
    let image_event: GraphicsEvent = Ok(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255, 0, 0, 255],
        })
        .into(),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    });
    serde_json::to_value(image_event).expect("the image event is json")
}

#[test]
fn a_pane_state_with_a_graphics_event_list_over_the_engine_limit_does_not_read() {
    let mut pane_state_json = build_blank_carried_pane_state_json();
    pane_state_json["graphics_events"] = serde_json::json!(vec![
        build_queued_image_event_json();
        koshi_terminal::engine::MAX_GRAPHICS_EVENT_COUNT
            + 1
    ]);

    assert_eq!(
        format_pane_state_parse_error(&pane_state_json),
        "graphics event count exceeds 64"
    );
}

#[test]
fn a_pane_state_reads_the_queue_full_report_after_queued_events() {
    let pane_id = PaneId::new();
    let mut graphics_event_jsons =
        vec![build_queued_image_event_json(); koshi_terminal::engine::MAX_GRAPHICS_EVENT_COUNT];
    graphics_event_jsons.push(serde_json::json!({
        "Err": {
            "QueueFull": { "dropped_event_count": 2 }
        }
    }));
    assert_eq!(
        graphics_event_jsons.len(),
        koshi_terminal::engine::MAX_GRAPHICS_EVENT_BATCH_COUNT
    );
    let mut pane_state_json = build_blank_carried_pane_state_json();
    pane_state_json["graphics_events"] = serde_json::json!(graphics_event_jsons);
    let raw_resume_body = build_raw_resume_body(
        &serde_json::json!({ pane_id.get_uuid().to_string(): pane_state_json }).to_string(),
    );

    let parsed_resume_body =
        read_resume_body(RESUME_FORMAT, &raw_resume_body).expect("the valid queue batch reads");

    assert_eq!(
        serde_json::to_value(
            &parsed_resume_body.carried_pane_state_by_pane_id[&pane_id].graphics_events
        )
        .expect("encode the graphics events"),
        serde_json::json!(graphics_event_jsons)
    );
}

#[test]
fn a_body_leaves_out_the_one_pane_whose_state_does_not_read_and_keeps_the_others() {
    let readable_pane_id = PaneId::new();
    let unreadable_pane_id = PaneId::new();
    let mut unreadable_pane_state_json = build_blank_carried_pane_state_json();
    unreadable_pane_state_json["graphics_transport"] =
        serde_json::json!({ "carry_bytes": vec![0u8; 64 * 1024 + 1] });
    let raw_resume_body = build_raw_resume_body(
        &serde_json::json!({
            readable_pane_id.get_uuid().to_string(): build_blank_carried_pane_state_json(),
            unreadable_pane_id.get_uuid().to_string(): unreadable_pane_state_json,
        })
        .to_string(),
    );

    let parsed_resume_body =
        read_resume_body(RESUME_FORMAT, &raw_resume_body).expect("the body reads");

    assert_eq!(
        parsed_resume_body
            .carried_pane_state_by_pane_id
            .keys()
            .copied()
            .collect::<Vec<PaneId>>(),
        vec![readable_pane_id]
    );
}

#[test]
fn a_body_leaves_out_a_pane_key_named_twice_and_a_key_naming_no_pane() {
    let repeated_pane_id = PaneId::new();
    let readable_pane_id = PaneId::new();
    let pane_state_text = build_blank_carried_pane_state_json().to_string();
    let raw_resume_body = build_raw_resume_body(&format!(
        r#"{{"{repeated_pane_uuid}":{pane_state_text},"not-a-pane-id":{pane_state_text},"{repeated_pane_uuid}":{pane_state_text},"{readable_pane_uuid}":{pane_state_text},"{repeated_pane_uuid}":{pane_state_text}}}"#,
        repeated_pane_uuid = repeated_pane_id.get_uuid(),
        readable_pane_uuid = readable_pane_id.get_uuid(),
    ));

    let parsed_resume_body =
        read_resume_body(RESUME_FORMAT, &raw_resume_body).expect("the body reads");

    assert_eq!(
        parsed_resume_body
            .carried_pane_state_by_pane_id
            .keys()
            .copied()
            .collect::<Vec<PaneId>>(),
        vec![readable_pane_id]
    );
}

#[test]
fn the_children_left_running_read_back_from_the_body() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT,
        session_id: SessionId::new(),
        session_name: "carried".to_string(),
        carried_panes: vec![build_test_carried_pane(None)],
    };
    let mut resume_body = build_resume_body_with_quit(None);
    resume_body.left_running_child_process_ids = vec![5000, 5001];
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");

    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");

    assert_eq!(read_body.left_running_child_process_ids, vec![5000, 5001]);
}

#[test]
fn a_header_naming_an_unknown_format_still_reads_back_whole() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let resume_header = ResumeHeader {
        resume_format: RESUME_FORMAT + 1,
        session_id: SessionId::new(),
        session_name: "from-a-newer-build".to_string(),
        carried_panes: vec![build_test_carried_pane(None)],
    };
    write_resume_file(
        &resume_file_path,
        &resume_header,
        &build_resume_body_with_quit(None),
    )
    .expect("write the resume file");

    let (read_header, _raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");

    assert_eq!(
        read_header, resume_header,
        "any build reads the header of any other"
    );
}

#[test]
fn a_resumed_server_starts_with_no_socket_and_no_shutdown_pending() {
    let (_populated_server, _carried_pty_panes, resume_header, resume_body) =
        carry_out_populated_server();

    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);

    assert!(!resumed_server.is_quit_requested, "no quit is pending");
    assert!(
        !resumed_server.should_shutdown_immediately,
        "no zero-grace quit is pending"
    );
    assert!(
        resumed_server.get_ipc_server().is_none(),
        "the control socket is bound after the swap, not carried through it"
    );
    assert_eq!(
        resumed_server.subscriptions.len(),
        0,
        "no subscriber is carried"
    );
}

#[test]
fn reading_a_resume_file_that_is_not_there_is_an_io_failure() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("missing.resume");
    let missing_file_error =
        std::fs::read(&resume_file_path).expect_err("the resume file is not there");

    match read_resume_header(&resume_file_path) {
        Err(StorageError::Io { detail }) => assert_eq!(
            detail,
            format!(
                "read resume state at {}: {missing_file_error}",
                resume_file_path.display()
            )
        ),
        unexpected_read_result => {
            panic!("expected an io failure, got {unexpected_read_result:?}")
        }
    }
}

#[test]
fn reading_bytes_that_are_not_a_resume_file_is_a_corrupt_failure() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("junk.resume");
    std::fs::write(&resume_file_path, b"not json").expect("write junk");

    match read_resume_header(&resume_file_path) {
        Err(StorageError::Corrupt { detail }) => {
            assert_eq!(
                detail,
                format!(
                    "resume state at {} is unreadable: expected ident at line 1 column 2",
                    resume_file_path.display()
                )
            );
        }
        unexpected_read_result => {
            panic!("expected a corrupt failure, got {unexpected_read_result:?}")
        }
    }
}

#[test]
fn a_body_format_below_the_oldest_this_build_reads_is_refused_by_both_numbers() {
    // A format one below `RESUME_FORMAT_MIN` is refused with the same message
    // as a format above `RESUME_FORMAT`.
    let below_minimum_resume_format = RESUME_FORMAT_MIN - 1;

    match read_resume_body(
        below_minimum_resume_format,
        serde_json::value::RawValue::NULL,
    ) {
        Err(StorageError::Corrupt { detail }) => {
            assert_eq!(
                detail,
                format!(
                    "resume body format {below_minimum_resume_format} is outside the {RESUME_FORMAT_MIN} to {RESUME_FORMAT} range this build reads"
                )
            );
        }
        unexpected_read_result => {
            panic!("expected a refused format, got {unexpected_read_result:?}")
        }
    }
}

#[test]
fn a_previous_shape_header_reads_formats_one_to_three_and_refuses_every_other_format() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("previous.resume");
    let session_id = SessionId::new();
    let write_previous_shape_file = |previous_header_format: u32| {
        let previous_resume_file_json = serde_json::json!({
            "header": {
                "format": previous_header_format,
                "session_id": session_id,
                "session_name": "carried",
                "panes": []
            },
            "body": {}
        });
        std::fs::write(
            &resume_file_path,
            serde_json::to_vec(&previous_resume_file_json).expect("encode the previous shape"),
        )
        .expect("write the previous shape");
    };

    for read_header_format in [1, 2, 3] {
        write_previous_shape_file(read_header_format);
        let (resume_header, raw_body) =
            read_resume_header(&resume_file_path).expect("a previous-shape header reads");
        assert_eq!(
            resume_header,
            ResumeHeader {
                resume_format: read_header_format,
                session_id,
                session_name: "carried".to_string(),
                carried_panes: Vec::new(),
            }
        );
        assert_eq!(raw_body.get(), "{}");
    }
    for refused_header_format in [0, 4, 5, 6, u32::MAX] {
        write_previous_shape_file(refused_header_format);
        match read_resume_header(&resume_file_path) {
            Err(StorageError::Corrupt { detail }) => assert_eq!(
                detail,
                format!(
                    "resume format {refused_header_format} is outside the 1 to 3 range of the previous header shape"
                )
            ),
            unexpected_read_result => {
                panic!("expected a refused format, got {unexpected_read_result:?}")
            }
        }
    }
}

#[test]
fn a_resume_file_whose_bytes_stop_part_way_is_a_corrupt_failure_naming_the_path() {
    // The header and the body are one JSON document. A file cut in half fails
    // whole, with a corrupt error that names the path.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let (_populated_server, _carried_pty_panes, resume_header, resume_body) =
        carry_out_populated_server();
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    let resume_file_bytes = std::fs::read(&resume_file_path).expect("read the file back");
    let cut_resume_file_bytes = &resume_file_bytes[..resume_file_bytes.len() / 2];
    std::fs::write(&resume_file_path, cut_resume_file_bytes).expect("rewrite the file cut short");
    let Err(cut_file_parse_error) =
        serde_json::from_slice::<PreviousResumeFile>(cut_resume_file_bytes)
    else {
        panic!("a file cut in half must not parse");
    };

    match read_resume_header(&resume_file_path) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail,
            format!(
                "resume state at {} is unreadable: {cut_file_parse_error}",
                resume_file_path.display()
            )
        ),
        unexpected_read_result => {
            panic!("expected a corrupt failure, got {unexpected_read_result:?}")
        }
    }
}

#[test]
fn a_body_missing_its_pane_states_is_corrupt_while_the_header_still_reads() {
    // A body without `carried_pane_state_by_pane_id` is valid JSON and fails
    // the decode. The header and its carried panes still read.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let (_populated_server, _carried_pty_panes, resume_header, resume_body) =
        carry_out_populated_server();
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    update_resume_file_json(&resume_file_path, |resume_file_json| {
        resume_file_json["raw_body"]
            .as_object_mut()
            .expect("a body object")
            .remove("carried_pane_state_by_pane_id");
    });

    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");

    assert_eq!(
        read_header, resume_header,
        "the header survives a body without pane states"
    );
    match read_resume_body(read_header.resume_format, &raw_body) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail,
            format!(
                "resume body is unreadable: missing field `carried_pane_state_by_pane_id` at line 1 column {}",
                raw_body.get().len()
            )
        ),
        unexpected_read_result => panic!("expected a corrupt body, got {unexpected_read_result:?}"),
    }
}

#[test]
fn a_session_holding_no_pane_carries_out_and_reads_back_with_no_pane() {
    // A header naming no pane reads back with an empty `carried_panes` list.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("empty.resume");
    let mut populated_server = build_populated_server();
    let session_id = populated_server.session_id;

    let (resume_header, resume_body) = populated_server
        .server
        .carry_out(&[])
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");

    assert_eq!(
        read_header.carried_panes,
        Vec::new(),
        "no pane crosses the swap"
    );
    assert_eq!(read_header.resume_format, RESUME_FORMAT);
    assert_eq!(read_header.session_id, session_id);
    assert_eq!(read_header.session_name, "carried");
    assert_eq!(
        read_body.carried_pane_state_by_pane_id.len(),
        4,
        "every screen is carried"
    );

    let (resumed_server, _inbox_sender) = build_resumed_server(&read_header, read_body);
    assert_eq!(
        resumed_server.live_pane_ids.len(),
        0,
        "and no pane comes back live"
    );
    assert_eq!(
        resumed_server.pty_size_by_pane_id.len(),
        0,
        "and no size comes back"
    );
    assert_eq!(
        resumed_server.terminal_engine_by_pane_id.len(),
        0,
        "and no screen comes back"
    );
    assert_eq!(
        resumed_server.session_by_id.len(),
        1,
        "the session itself still does"
    );
    assert_eq!(
        resumed_server.session_by_id[&session_id]
            .panes
            .count_pane_records(),
        0,
        "with every pane nothing drives closed"
    );
}

#[test]
fn a_session_holding_many_panes_carries_every_one_of_them_in_order() {
    // The header names all 64 panes, in the order the backend reported them,
    // each with its own descriptor, process id and size.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("many.resume");
    let mut populated_server = build_populated_server();

    let carried_pty_panes: Vec<CarriedPtyPane> = (0..64)
        .map(|pane_index| CarriedPtyPane {
            pane_id: PaneId::new(),
            #[cfg(unix)]
            terminal_fd: Some(100 + pane_index),
            process_id: 7000 + pane_index as u32,
            pty_size: PtySize {
                column_count: 40 + pane_index as u16,
                row_count: 10 + pane_index as u16,
            },
            exit_status: None,
        })
        .collect();

    let (resume_header, resume_body) = populated_server
        .server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    let (read_header, _raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");

    assert_eq!(
        read_header.carried_panes.len(),
        64,
        "every pane must have a carried pane"
    );
    for (pane_index, carried_pane) in read_header.carried_panes.iter().enumerate() {
        assert_eq!(
            carried_pane.pane_id, carried_pty_panes[pane_index].pane_id,
            "carried pane {pane_index}"
        );
        assert_eq!(
            carried_pane.process_id,
            7000 + pane_index as u32,
            "carried pane {pane_index}"
        );
        #[cfg(unix)]
        assert_eq!(
            carried_pane.terminal_fd,
            Some(100 + pane_index as i32),
            "carried pane {pane_index}"
        );
        #[cfg(windows)]
        assert_eq!(carried_pane.terminal_fd, None, "carried pane {pane_index}");
        // The server holds no size for these panes. Each takes the size the
        // backend reported.
        assert_eq!(
            (carried_pane.column_count, carried_pane.row_count),
            (40 + pane_index as u16, 10 + pane_index as u16),
            "carried pane {pane_index}"
        );
    }
}

#[test]
fn a_screen_for_a_pane_no_session_holds_and_nothing_drives_is_dropped() {
    let mut populated_server = build_populated_server();
    let carried_pty_panes =
        build_carried_pty_panes(&populated_server.server, populated_server.session_id);
    let unlisted_pane_id = PaneId::new();
    populated_server.server.terminal_engine_by_pane_id.insert(
        unlisted_pane_id,
        koshi_terminal::engine::TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );
    let (resume_header, resume_body) = populated_server
        .server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");
    assert!(resume_body
        .carried_pane_state_by_pane_id
        .contains_key(&unlisted_pane_id));
    let header_pane_ids: HashSet<PaneId> = resume_header
        .carried_panes
        .iter()
        .map(|carried_pane| carried_pane.pane_id)
        .collect();

    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);

    assert_eq!(
        resumed_server
            .terminal_engine_by_pane_id
            .keys()
            .copied()
            .collect::<HashSet<PaneId>>(),
        header_pane_ids,
        "only the driven panes keep a screen"
    );
    assert_eq!(resumed_server.live_pane_ids, header_pane_ids);
    assert_eq!(
        resumed_server
            .pty_size_by_pane_id
            .keys()
            .copied()
            .collect::<HashSet<PaneId>>(),
        header_pane_ids
    );
}

#[test]
fn a_driven_pane_whose_screen_did_not_read_comes_back_blank_and_posts_a_warning() {
    let (populated_server, carried_pty_panes, resume_header, mut resume_body) =
        carry_out_populated_server();
    let blank_pane_id = carried_pty_panes[1].pane_id;
    resume_body
        .carried_pane_state_by_pane_id
        .remove(&blank_pane_id);

    let (resumed_server, _inbox_sender) = build_resumed_server(&resume_header, resume_body);

    assert_eq!(
        get_joined_screen_text(
            resumed_server.terminal_engine_by_pane_id[&blank_pane_id].get_terminal_state()
        ),
        "[koshi] This pane's screen could not be restored after the restart. The program in it is still running."
    );
    assert_eq!(
        resumed_server.session_by_id[&populated_server.session_id]
            .notification_queue
            .list_queued_entries()
            .map(|notification_entry| (notification_entry.severity, notification_entry.text.clone()))
            .collect::<Vec<_>>(),
        vec![(
            NotificationSeverity::Warning,
            format!(
                "{}: its screen could not be restored after the restart; its program is still running",
                blank_pane_id.format_short_label()
            )
        )]
    );
    let blank_pane_pty_size = resume_header.carried_panes[1].get_pty_size();
    assert_eq!(
        resumed_server.terminal_engine_by_pane_id[&blank_pane_id]
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (
            blank_pane_pty_size.row_count,
            blank_pane_pty_size.column_count
        )
    );
    assert_eq!(
        resumed_server.live_pane_ids,
        list_carried_pty_pane_ids(&carried_pty_panes),
        "every pane stays live"
    );
    assert_eq!(
        resumed_server.session_by_id[&populated_server.session_id]
            .panes
            .count_pane_records(),
        4,
        "every pane keeps its place in the layout"
    );
    for (pane_index, carried_pty_pane) in carried_pty_panes.iter().enumerate() {
        if carried_pty_pane.pane_id == blank_pane_id {
            continue;
        }
        assert_eq!(
            get_terminal_row(
                resumed_server.terminal_engine_by_pane_id[&carried_pty_pane.pane_id]
                    .get_terminal_state(),
                0
            )
            .trim_end(),
            format!("pane {pane_index} output"),
            "every other pane keeps its screen"
        );
    }
}

#[test]
fn a_blank_screen_after_a_restart_keeps_the_scrollback_limit_the_startup_config_names() {
    let (_populated_server, carried_pty_panes, resume_header, mut resume_body) =
        carry_out_populated_server();
    let blank_pane_id = carried_pty_panes[1].pane_id;
    resume_body
        .carried_pane_state_by_pane_id
        .remove(&blank_pane_id);
    let startup_app_config = PartialKoshiConfig {
        scrollback: Some(PartialScrollbackConfig {
            maximum_line_count: Some(3),
            maximum_byte_count: None,
            should_scroll_to_input: None,
        }),
        ..PartialKoshiConfig::default()
    };

    let (mut resumed_server, _inbox_sender) = build_resumed_server_with_config_and_exit_statuses(
        &resume_header,
        resume_body,
        Some(startup_app_config),
        HashMap::new(),
    );
    let scrolled_output_bytes: Vec<u8> = (0..60)
        .flat_map(|line_index| format!("line {line_index}\r\n").into_bytes())
        .collect();
    resumed_server.handle_pty_output(blank_pane_id, &scrolled_output_bytes);

    assert_eq!(
        resumed_server.terminal_engine_by_pane_id[&blank_pane_id]
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count(),
        3,
        "the blank screen keeps `scrollback {{ max-lines 3 }}`, not the built-in 10000"
    );
}

#[test]
fn a_driven_pane_no_session_holds_has_its_child_ended_and_is_not_recorded() {
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(inbox_sender),
    )));
    let stray_pane_id = PaneId::new();
    let stray_pty_size = PtySize {
        column_count: 80,
        row_count: 24,
    };
    fake_pty_backend
        .spawn_pane(
            stray_pane_id,
            SpawnSpec::build_default_shell(None, BTreeMap::new()),
            stray_pty_size,
        )
        .expect("spawn");
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();

    let resumed_server = Server::resume(
        pty_backend,
        inbox_receiver,
        Arc::default(),
        None,
        build_resume_body_with_quit(None),
        RESUME_FORMAT,
        HashMap::from([(stray_pane_id, stray_pty_size)]),
        HashMap::new(),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(stray_pane_id)
            .expect("the pane was spawned"),
        vec![KillPolicy::Tree]
    );
    assert!(resumed_server.live_pane_ids.is_empty());
    assert!(resumed_server.pty_size_by_pane_id.is_empty());
    assert!(resumed_server.terminal_engine_by_pane_id.is_empty());
}

#[test]
fn a_pane_a_session_holds_that_nothing_drives_closes_and_the_others_stay() {
    let (populated_server, carried_pty_panes, resume_header, resume_body) =
        carry_out_populated_server();
    let undriven_pane_id = carried_pty_panes[2].pane_id;
    let driven_header = ResumeHeader {
        carried_panes: resume_header
            .carried_panes
            .iter()
            .filter(|carried_pane| carried_pane.pane_id != undriven_pane_id)
            .cloned()
            .collect(),
        ..resume_header.clone()
    };
    let count_undriven_pane_exit_events = |resumed_server: &Server| {
        resumed_server
            .recent_event_ring
            .list_recent_events()
            .iter()
            .filter(|event_record| {
                event_record.pane_id == Some(undriven_pane_id)
                    && event_record.event_name == "PaneProcessExited"
            })
            .count()
    };

    let (mut resumed_server, _inbox_sender) = build_resumed_server_with_config_and_exit_statuses(
        &driven_header,
        resume_body,
        None,
        HashMap::from([(undriven_pane_id, ExitStatus::ExitCode(7))]),
    );

    let session = &resumed_server.session_by_id[&populated_server.session_id];
    assert_eq!(
        session.panes.get_pane_record_by_id(undriven_pane_id),
        None,
        "the pane nothing drives leaves the registry"
    );
    assert_eq!(
        session.find_tab_by_pane_id(undriven_pane_id),
        None,
        "and every layout"
    );
    assert!(!resumed_server
        .terminal_engine_by_pane_id
        .contains_key(&undriven_pane_id));
    assert_eq!(
        resumed_server.live_pane_ids,
        driven_header
            .carried_panes
            .iter()
            .map(|carried_pane| carried_pane.pane_id)
            .collect::<HashSet<PaneId>>(),
        "the other panes stay"
    );
    assert_eq!(
        count_undriven_pane_exit_events(&resumed_server),
        1,
        "the carried exit is published once"
    );
    let child_exit_events =
        resumed_server.handle_child_exit(undriven_pane_id, ExitStatus::ExitCode(7));
    resumed_server.publish_events(&child_exit_events);
    assert_eq!(
        count_undriven_pane_exit_events(&resumed_server),
        1,
        "the queued copy of the exit does not publish again"
    );
}

#[test]
fn a_file_whose_header_and_body_are_swapped_is_corrupt_before_any_pane_is_touched() {
    // A header part that holds the body names no pane. The read fails and
    // returns no descriptor and no process id.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("swapped.resume");
    let (_populated_server, _carried_pty_panes, resume_header, resume_body) =
        carry_out_populated_server();
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    update_resume_file_json(&resume_file_path, |resume_file_json| {
        let body_json = resume_file_json["raw_body"].take();
        let header_json = resume_file_json["header"].take();
        *resume_file_json = serde_json::json!({
            "header": body_json,
            "raw_body": header_json,
        });
    });
    let swapped_file_bytes = std::fs::read(&resume_file_path).expect("read the swapped file");
    let Err(swapped_file_parse_error) =
        serde_json::from_slice::<PreviousResumeFile>(&swapped_file_bytes)
    else {
        panic!("a swapped file must not parse");
    };

    match read_resume_header(&resume_file_path) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail,
            format!(
                "resume state at {} is unreadable: {swapped_file_parse_error}",
                resume_file_path.display()
            )
        ),
        unexpected_read_result => {
            panic!("expected a corrupt header, got {unexpected_read_result:?}")
        }
    }
    assert_eq!(
        swapped_file_parse_error
            .to_string()
            .split(" at line ")
            .next(),
        Some("unknown field `carried_pane_state_by_pane_id`, expected one of `format`, `session_id`, `session_name`, `panes`")
    );
}

#[test]
fn a_carried_session_with_its_client_comes_back_whole() {
    // A client written by this build reads back with the identity it went out
    // with, from a local and from a remote origin.
    for (origin, written_origin) in [
        (ClientOrigin::Local, "Local"),
        (ClientOrigin::Remote, "Remote"),
    ] {
        let resume_test_directory = TempDir::new().expect("create temp dir");
        let resume_file_path = resume_test_directory.path().join("client.resume");
        let session_id = SessionId::new();
        let client_id = ClientId::new();
        let tab_id = TabId::new();
        let mut session = Session::from_identity_and_client_registry(
            session_id,
            "carried".to_string(),
            SystemTime::UNIX_EPOCH,
            ClientRegistry::new(),
        );
        session.attach_client(Client::from_attachment(
            client_id,
            session_id,
            SystemTime::UNIX_EPOCH,
            FIRST_CLIENT_VIEWPORT_SIZE,
            None,
            tab_id,
            origin,
            "C-swift-otter".to_string(),
            3,
        ));
        let resume_header = build_resume_header(session_id, Vec::new());
        let resume_body = ResumeBody {
            session_by_id: HashMap::from([(session_id, session)]),
            ..build_resume_body_with_quit(None)
        };
        write_resume_file(&resume_file_path, &resume_header, &resume_body)
            .expect("write the resume file");

        let (read_header, raw_body) =
            read_resume_header(&resume_file_path).expect("read the header back");

        // Format 5 writes the origin and no authority key, and the header
        // names format 5.
        let body_json: serde_json::Value =
            serde_json::from_str(raw_body.get()).expect("the body is json");
        let client_record_json = &body_json["session_by_id"][session_id.get_uuid().to_string()]
            ["clients"]["client_by_id"][client_id.get_uuid().to_string()];
        assert_eq!(
            client_record_json["origin"],
            serde_json::Value::String(written_origin.to_string())
        );
        assert_eq!(
            client_record_json.get("tier"),
            None,
            "format {RESUME_FORMAT} writes no authority key"
        );

        let read_body =
            read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");

        assert_eq!(read_header.resume_format, RESUME_FORMAT);
        assert_eq!(RESUME_FORMAT, 5);
        let resumed_session = &read_body.session_by_id[&session_id];
        assert_eq!(resumed_session.session_id, session_id);
        let resumed_client = resumed_session
            .clients
            .get_client_by_id(client_id)
            .expect("the carried client");
        assert_eq!(resumed_client.get_client_id(), client_id);
        assert_eq!(resumed_client.get_origin(), origin);
        assert_eq!(resumed_client.get_label(), "C-swift-otter");
        assert_eq!(resumed_client.get_color_index(), 3);
        assert_eq!(resumed_client.get_active_tab_id(), tab_id);
    }
}

#[test]
fn a_carried_pane_reports_the_size_its_row_and_column_counts_name() {
    let carried_pane = build_test_carried_pane(None);

    assert_eq!(
        carried_pane.get_pty_size(),
        PtySize {
            row_count: 20,
            column_count: 78
        }
    );
}

#[test]
fn an_applied_quit_crosses_the_file_with_the_kind_it_was_asked_for() {
    for quit_kind in [CarriedQuit::Graceful, CarriedQuit::Immediate] {
        let resume_test_directory = TempDir::new().expect("create temp dir");
        let resume_file_path = resume_test_directory.path().join("quit.resume");
        let resume_header = build_resume_header(SessionId::new(), Vec::new());
        let resume_body = build_resume_body_with_quit(Some(quit_kind));
        write_resume_file(&resume_file_path, &resume_header, &resume_body)
            .expect("write the resume file");

        let (read_header, raw_body) =
            read_resume_header(&resume_file_path).expect("read the header back");
        let read_body =
            read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");

        assert_eq!(read_body.carried_quit, Some(quit_kind));
    }
}

#[test]
fn a_saved_view_store_crosses_the_file_with_its_waiting_hash_and_its_filed_view() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("saved-view.resume");
    let waiting_client_id = ClientId::new();
    let filed_tab_id = TabId::new();
    let filed_client = Client::from_attachment(
        ClientId::new(),
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        FIRST_CLIENT_VIEWPORT_SIZE,
        None,
        filed_tab_id,
        ClientOrigin::Local,
        "C-swift-otter".to_string(),
        3,
    );
    let mut saved_view_store = SavedViewStore::default();
    let waiting_resume_token = saved_view_store.mint_resume_token(waiting_client_id);
    let filed_resume_token = saved_view_store.mint_resume_token(filed_client.get_client_id());
    let filed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    saved_view_store.save_client_view(&filed_client, filed_at);
    let resume_header = build_resume_header(SessionId::new(), Vec::new());
    let resume_body = ResumeBody {
        saved_view_store,
        ..build_resume_body_with_quit(None)
    };
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");

    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let mut read_saved_view_store = read_resume_body(read_header.resume_format, &raw_body)
        .expect("read the body back")
        .saved_view_store;

    assert_eq!(
        read_saved_view_store.find_client_id_by_resume_token(&waiting_resume_token),
        Some(waiting_client_id)
    );
    assert_eq!(
        read_saved_view_store.take_saved_view(&filed_resume_token, filed_at),
        Some(SavedView {
            active_tab_id: filed_tab_id,
            focus_history_by_tab_id: HashMap::new(),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            scroll_offset_by_pane_id: HashMap::new(),
            floating_pane_view_by_pane_id: HashMap::new(),
            shown_floating_pane_ids: Vec::new(),
        })
    );
}

#[test]
fn a_body_written_without_a_quit_reads_back_with_none() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let (_populated_server, _carried_pty_panes, resume_header, resume_body) =
        carry_out_populated_server();
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    update_resume_file_json(&resume_file_path, |resume_file_json| {
        resume_file_json["raw_body"]
            .as_object_mut()
            .expect("the body is a map")
            .remove("carried_quit");
    });

    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");

    assert_eq!(read_body.carried_quit, None);
    assert_eq!(
        read_body.carried_pane_state_by_pane_id.len(),
        4,
        "every screen still reads back"
    );
}

#[test]
fn a_pane_whose_child_was_reaped_carries_that_exit_status_across_the_file() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("reaped.resume");
    let resume_header = build_resume_header(
        SessionId::new(),
        vec![
            build_test_carried_pane(Some(ExitStatus::ExitCode(3))),
            CarriedPane {
                process_id: 4243,
                terminal_fd: Some(10),
                terminal_name: Some("/dev/pts/10".to_string()),
                ..build_test_carried_pane(Some(ExitStatus::Signaled(9)))
            },
        ],
    );
    write_resume_file(
        &resume_file_path,
        &resume_header,
        &build_resume_body_with_quit(None),
    )
    .expect("write the resume file");

    let (read_header, _raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");

    assert_eq!(read_header, resume_header);
}

#[test]
fn writing_a_resume_file_replaces_the_bytes_already_there() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    std::fs::write(&resume_file_path, b"the bytes of an older write").expect("write the old file");
    let resume_header = build_resume_header(SessionId::new(), vec![build_test_carried_pane(None)]);

    write_resume_file(
        &resume_file_path,
        &resume_header,
        &build_resume_body_with_quit(None),
    )
    .expect("write the resume file");

    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    assert_eq!(read_header, resume_header);
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");
    assert_eq!(read_body.session_by_id.len(), 0);
    assert_eq!(read_body.carried_pane_state_by_pane_id.len(), 0);
}

#[test]
fn writing_into_a_directory_that_is_not_there_is_an_io_failure_naming_it() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let missing_parent_directory = resume_test_directory.path().join("gone");
    let resume_file_path = missing_parent_directory.join("session.resume");
    let resume_header = build_resume_header(SessionId::new(), Vec::new());

    match write_resume_file(
        &resume_file_path,
        &resume_header,
        &build_resume_body_with_quit(None),
    ) {
        Err(StorageError::Io { detail }) => assert!(
            detail.starts_with(&format!(
                "create temp in {}: ",
                missing_parent_directory.display()
            )),
            "the failure must name the directory, got {detail}"
        ),
        unexpected_write_result => {
            panic!("expected an io failure, got {unexpected_write_result:?}")
        }
    }
    assert!(
        !missing_parent_directory.exists(),
        "and the directory must not be created"
    );
}

/// A writer that keeps every byte it receives and the byte count of the
/// largest single piece.
#[derive(Default)]
struct PieceRecordingWriter {
    received_bytes: Vec<u8>,
    largest_piece_byte_count: usize,
}

impl std::io::Write for PieceRecordingWriter {
    fn write(&mut self, file_piece: &[u8]) -> std::io::Result<usize> {
        self.largest_piece_byte_count = self.largest_piece_byte_count.max(file_piece.len());
        self.received_bytes.extend_from_slice(file_piece);
        Ok(file_piece.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn ten_panes_with_full_history_encode_as_a_stream_of_small_pieces() {
    let carried_pane_state_by_pane_id: HashMap<PaneId, CarriedPaneState> = (0..10)
        .map(|_| {
            let mut terminal_engine =
                koshi_terminal::engine::TerminalEngine::from_pty_size(PtySize {
                    column_count: 80,
                    row_count: 24,
                });
            for line_number in 0..2_000 {
                let _ = terminal_engine.process_pty_output(
                    format!("line {line_number} of a pane carried across an update\r\n").as_bytes(),
                );
            }
            (
                PaneId::new(),
                CarriedPaneState {
                    terminal_state: terminal_engine.into_terminal_state(),
                    undecoded_bytes: Vec::new(),
                    graphics_events: Vec::new(),
                    graphics_transport: None,
                    synchronized_output: None,
                },
            )
        })
        .collect();
    let resume_body = ResumeBody {
        session_by_id: HashMap::new(),
        carried_pane_state_by_pane_id,
        carried_quit: None,
        saved_view_store: SavedViewStore::default(),
        left_running_child_process_ids: Vec::new(),
    };
    let resume_header = build_resume_header(SessionId::new(), Vec::new());
    let resume_file_reference = ResumeFileReference {
        header: &resume_header,
        raw_body: &resume_body,
    };
    let mut piece_recording_writer = PieceRecordingWriter::default();

    serde_json::to_writer(&mut piece_recording_writer, &resume_file_reference)
        .expect("the resume file encodes");

    assert_eq!(
        piece_recording_writer.received_bytes,
        serde_json::to_vec(&resume_file_reference).expect("the resume file encodes")
    );
    assert!(piece_recording_writer.received_bytes.len() > 1024 * 1024);
    assert!(
        piece_recording_writer.largest_piece_byte_count
            <= koshi_storage::atomic::STAGED_WRITE_BUFFER_BYTE_COUNT,
        "a piece of {} bytes",
        piece_recording_writer.largest_piece_byte_count
    );
}

#[test]
fn a_profile_session_carried_before_any_attach_keeps_its_starting_tab_and_pane() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let (inbox_sender, inbox_receiver) = mpsc::channel();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(inbox_sender),
    )));
    let mut server = Server::from_runtime_parts(pty_backend, inbox_receiver, Arc::default());
    let session_id = SessionId::new();
    let profile_template = koshi_config::profile::parse_profile(
        Path::new("profile/test.kdl"),
        "version 2\ntab {\n    pane\n}\n\
         tab {\n    focus\n    horizontal {\n        pane\n        pane {\n            focus\n        }\n    }\n}",
    )
    .expect("valid profile");
    server
        .bootstrap_profile(
            session_id,
            "carried".to_string(),
            profile_template,
            FIRST_CLIENT_VIEWPORT_SIZE,
            SystemTime::UNIX_EPOCH,
        )
        .expect("profile launches");
    let starting_tab_id = server.session_by_id[&session_id]
        .starting_tab_id
        .expect("the profile's starting tab");
    let starting_tab_pane_ids = server.session_by_id[&session_id].tabs[&starting_tab_id]
        .get_layout_tree()
        .list_leaf_pane_ids();
    let carried_pty_panes = build_carried_pty_panes(&server, session_id);
    let (resume_header, resume_body) = server
        .carry_out(&carried_pty_panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &resume_header, &resume_body)
        .expect("write the resume file");
    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");
    let (mut resumed_server, _inbox_sender) = build_resumed_server(&read_header, read_body);
    assert_eq!(
        resumed_server.session_by_id[&session_id].starting_tab_id,
        Some(starting_tab_id)
    );

    let attachment = resumed_server
        .handle_ipc_attach(
            None,
            None,
            FIRST_CLIENT_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::UNIX_EPOCH,
            false,
            60,
            None,
        )
        .expect("the resumed session accepts the attach");

    let client = resumed_server.session_by_id[&session_id]
        .clients
        .get_client_by_id(attachment.client_id)
        .expect("the attached client");
    assert_eq!(client.get_active_tab_id(), starting_tab_id);
    assert_eq!(
        client.get_focused_pane_id(starting_tab_id),
        Some(starting_tab_pane_ids[1])
    );
}
