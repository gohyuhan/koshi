//! Tests for PTY output handling: bytes reach only the owning pane's engine,
//! a decode carries across chunks, output schedules a render, shell-integration
//! markers publish command lifecycle events, device-query replies are written
//! back to the pane's PTY, bytes for a pane with no engine are dropped, and a
//! cell pixel size query answers with the cell size of the pane's tab, or with
//! the floating cell size in a floating pane.

use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

use crate::runtime::pty_inbox::InboxSink;
use koshi_core::event::{Event, PaneCommandFinished, PaneCommandStarted};
use koshi_core::geometry::{FloatingPaneDimension, FloatingPaneSize, PixelCellSize, Size};
use koshi_core::ids::{ClientId, SessionId, TabId};
use koshi_core::process::{PtySize, ShellKind, SpawnSpec};
use koshi_pty::backend::state::PtyBackend;
use koshi_pty::error::PtyError;
use koshi_renderer::snapshot::Delivery;
use koshi_session::client::{Client, ClientOrigin, ClientRegistry};
use koshi_session::session::state::{FloatingMember, Session, Tab};
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::style::{Color, Style};
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::render_schedule::FRAME_INTERVAL_DURATION;

use super::*;

/// A bare runtime with stub services and no sessions, plus the fake PTY
/// backend for asserting on writes.
fn build_test_server() -> (Server, Arc<FakePtyBackend>) {
    let (event_sender, event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(event_sender),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let server = Server::from_runtime_parts(pty_backend, event_receiver);
    (server, fake_pty_backend)
}

/// Install an 8x3 terminal engine for a fresh pane id and return the id.
fn insert_test_terminal_engine(server: &mut Server) -> PaneId {
    let pane_id = PaneId::new();
    server.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 8,
            row_count: 3,
        }),
    );
    pane_id
}

/// Register `pane_id` with the fake backend so its writes are recorded.
fn spawn_test_pane(fake_pty_backend: &FakePtyBackend, pane_id: PaneId) {
    let spawn_spec = SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        arguments: Vec::new(),
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Zsh,
    };
    fake_pty_backend
        .spawn_pane(
            pane_id,
            spawn_spec,
            PtySize {
                column_count: 8,
                row_count: 3,
            },
        )
        .expect("fake spawn succeeds");
}

/// The character at (`row_index`, `column_index`) on `pane_id`'s active grid.
fn get_pane_cell_character(
    server: &Server,
    pane_id: PaneId,
    row_index: u16,
    column_index: u16,
) -> char {
    server.list_terminal_engines()[&pane_id]
        .get_terminal_state()
        .get_active_grid()
        .get_cell(row_index, column_index)
        .expect("cell in bounds")
        .get_character()
}

/// A local client of `session_id` attached at `attached_at`, viewing
/// `active_tab_id` and reporting `pixel_width` by `pixel_height` px cells.
fn build_measured_client(
    session_id: SessionId,
    active_tab_id: TabId,
    attached_at: SystemTime,
    pixel_width: u16,
    pixel_height: u16,
) -> Client {
    let mut client = Client::from_attachment(
        ClientId::new(),
        session_id,
        attached_at,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        active_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_cell_size(PixelCellSize::from_pixel_dimensions(
        pixel_width,
        pixel_height,
    ));
    client
}

#[test]
fn a_cell_pixel_size_query_answers_with_the_tab_cell_size_or_the_floating_cell_size() {
    let (mut runtime, fake_pty_backend) = build_test_server();
    let tiled_pane_id = insert_test_terminal_engine(&mut runtime);
    let floating_pane_id = insert_test_terminal_engine(&mut runtime);
    let stray_pane_id = insert_test_terminal_engine(&mut runtime);
    for pane_id in [tiled_pane_id, floating_pane_id, stray_pane_id] {
        spawn_test_pane(&fake_pty_backend, pane_id);
    }
    let (db_tab_id, web_tab_id) = (TabId::new(), TabId::new());
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.tabs.insert(
        db_tab_id,
        Tab::from_root_pane(db_tab_id, "db".to_owned(), 0, PaneId::new()),
    );
    session.tabs.insert(
        web_tab_id,
        Tab::from_root_pane(web_tab_id, "web".to_owned(), 1, tiled_pane_id),
    );
    let forty_cells = FloatingPaneDimension::Cells(NonZeroU16::new(40).expect("40 is nonzero"));
    session
        .floating_set
        .add_member(FloatingMember {
            pane_id: floating_pane_id,
            desired_size: FloatingPaneSize {
                width: forty_cells,
                height: forty_cells,
            },
            solved_size: Size {
                column_count: 40,
                row_count: 40,
            },
        })
        .expect("the floating set is empty");
    // alice attaches first and views `db` with 10x20 px cells; bob views
    // `web` with 8x16 px cells.
    let alice_client = build_measured_client(
        session.session_id,
        db_tab_id,
        SystemTime::UNIX_EPOCH,
        10,
        20,
    );
    let bob_client = build_measured_client(
        session.session_id,
        web_tab_id,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        8,
        16,
    );
    session.attach_client(alice_client);
    session.attach_client(bob_client);
    runtime.session_by_id.insert(session.session_id, session);

    // XTWINOPS 16: report the cell size in pixels.
    for pane_id in [tiled_pane_id, floating_pane_id, stray_pane_id] {
        runtime.handle_pty_output(pane_id, b"\x1b[16t");
    }

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(tiled_pane_id)
            .unwrap(),
        vec![b"\x1b[6;16;8t".to_vec()]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(floating_pane_id)
            .unwrap(),
        vec![b"\x1b[6;20;10t".to_vec()]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(stray_pane_id)
            .unwrap(),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn bytes_update_only_the_owning_panes_grid() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    let other_pane_id = insert_test_terminal_engine(&mut runtime);

    runtime.handle_pty_output(pane_id, b"hi");

    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 0), 'h');
    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 1), 'i');
    assert_eq!(
        runtime.list_terminal_engines()[&pane_id]
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 2)
    );

    // The other pane's engine is untouched.
    assert_eq!(get_pane_cell_character(&runtime, other_pane_id, 0, 0), ' ');
    assert_eq!(
        runtime.list_terminal_engines()[&other_pane_id]
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 0)
    );
}

#[test]
fn an_escape_sequence_split_across_two_events_decodes_once() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);

    // SGR 31 (red foreground) split mid-sequence across two output events:
    // the pane's parser carries the partial sequence between handler calls.
    runtime.handle_pty_output(pane_id, b"\x1b[3");
    runtime.handle_pty_output(pane_id, b"1mx");

    let terminal_engines = runtime.list_terminal_engines();
    let grid_cell = terminal_engines[&pane_id]
        .get_terminal_state()
        .get_active_grid()
        .get_cell(0, 0)
        .expect("cell in bounds");
    let mut red = Style::default();
    red.set_foreground_color(Color::Indexed(1));
    assert_eq!(grid_cell.get_character(), 'x');
    assert_eq!(grid_cell.get_style(), red);
}

#[test]
fn output_schedules_a_render() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);

    runtime.handle_pty_output(pane_id, b"hi");

    // PtyOutput was marked pending and nothing has rendered yet, so a render
    // is due immediately.
    assert!(runtime.render_scheduler.claim_due_render(Instant::now()));
}

#[test]
fn synchronized_body_keeps_the_committed_state_and_side_effects_hidden() {
    let (mut runtime, fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    spawn_test_pane(&fake_pty_backend, pane_id);
    let event_deliveries = runtime.subscribe(ClientId::new());
    let render_time = Instant::now();
    runtime.handle_pty_output(pane_id, b"old");
    assert!(runtime.render_scheduler.claim_due_render(render_time));

    runtime.handle_pty_output(pane_id, b"\x1b[?2026h");
    runtime.handle_pty_output(pane_id, b"\x1b[2J\x1b[Hnew\x1b[5n\x1b]133;C\x07");

    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 0), 'o');
    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 1), 'l');
    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 2), 'd');
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("pane exists"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(event_deliveries.try_iter().collect::<Vec<_>>(), Vec::new());
    assert!(!runtime.render_scheduler.claim_due_render(render_time));

    runtime.handle_pty_output(pane_id, b"\x1b[?2026l");

    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 0), 'n');
    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 1), 'e');
    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 2), 'w');
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("pane exists"),
        [b"\x1b[0n".to_vec()]
    );
    assert_eq!(
        event_deliveries.try_iter().collect::<Vec<_>>(),
        [Delivery::Event(Event::PaneCommandStarted(
            PaneCommandStarted { pane_id }
        ))]
    );
    assert_eq!(
        runtime.render_scheduler.compute_next_wakeup(render_time),
        Some(FRAME_INTERVAL_DURATION)
    );
    assert!(runtime
        .render_scheduler
        .claim_due_render(render_time + FRAME_INTERVAL_DURATION));
}

#[test]
fn synchronized_deadline_uses_the_runtime_wakeup_and_common_delivery_path() {
    let (mut runtime, fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    spawn_test_pane(&fake_pty_backend, pane_id);
    let event_deliveries = runtime.subscribe(ClientId::new());
    let current_time = Instant::now();
    let engine = runtime
        .terminal_engine_by_pane_id
        .get_mut(&pane_id)
        .expect("the pane has an engine");
    let _ = engine.process_pty_output_with_shell_integration_at(b"\x1b[?2026h", current_time);
    let _ = engine
        .process_pty_output_with_shell_integration_at(b"X\x1b[5n\x1b]133;C\x07", current_time);

    assert_eq!(
        runtime.compute_next_render_wakeup(current_time + Duration::from_millis(149)),
        Some(Duration::from_millis(1))
    );
    assert!(!runtime.poll_render(current_time + Duration::from_millis(149)));
    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 0), ' ');
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("pane exists"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(event_deliveries.try_iter().collect::<Vec<_>>(), Vec::new());

    assert_eq!(
        runtime.compute_next_render_wakeup(current_time + Duration::from_millis(150)),
        Some(Duration::ZERO)
    );
    assert!(runtime.poll_render(current_time + Duration::from_millis(150)));
    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 0), 'X');
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("pane exists"),
        [b"\x1b[0n".to_vec()]
    );
    assert_eq!(
        event_deliveries.try_iter().collect::<Vec<_>>(),
        [Delivery::Event(Event::PaneCommandStarted(
            PaneCommandStarted { pane_id }
        ))]
    );
}

#[test]
fn shell_markers_publish_command_events_in_order() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    let event_deliveries = runtime.subscribe(ClientId::new());

    runtime.handle_pty_output(pane_id, b"\x1b]133;C\x07\x1b]133;D;137\x07");

    assert_eq!(
        event_deliveries.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::PaneCommandStarted(PaneCommandStarted { pane_id })),
            Delivery::Event(Event::PaneCommandFinished(PaneCommandFinished {
                pane_id,
                exit_code: Some(137),
            })),
        ]
    );
}

#[test]
fn duplicate_shell_starts_publish_one_command_pair() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    let event_deliveries = runtime.subscribe(ClientId::new());

    runtime.handle_pty_output(pane_id, b"\x1b]133;C\x07\x1b]133;C\x07\x1b]133;D;0\x07");

    assert_eq!(
        event_deliveries.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::PaneCommandStarted(PaneCommandStarted { pane_id })),
            Delivery::Event(Event::PaneCommandFinished(PaneCommandFinished {
                pane_id,
                exit_code: Some(0),
            })),
        ]
    );
}

#[test]
fn an_unmatched_finish_and_plain_output_publish_no_command_events() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    let event_deliveries = runtime.subscribe(ClientId::new());

    runtime.handle_pty_output(pane_id, b"\x1b]133;D;1\x07plain output");

    assert_eq!(event_deliveries.try_iter().collect::<Vec<_>>(), Vec::new());
}

#[test]
fn command_lifecycle_state_is_independent_per_pane() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let first_pane_id = insert_test_terminal_engine(&mut runtime);
    let second_pane_id = insert_test_terminal_engine(&mut runtime);
    let event_deliveries = runtime.subscribe(ClientId::new());

    runtime.handle_pty_output(first_pane_id, b"\x1b]133;C\x07");
    runtime.handle_pty_output(second_pane_id, b"\x1b]133;C\x07");
    runtime.handle_pty_output(first_pane_id, b"\x1b]133;D;0\x07");
    runtime.handle_pty_output(second_pane_id, b"\x1b]133;D\x07");

    assert_eq!(
        event_deliveries.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::PaneCommandStarted(PaneCommandStarted {
                pane_id: first_pane_id,
            })),
            Delivery::Event(Event::PaneCommandStarted(PaneCommandStarted {
                pane_id: second_pane_id,
            })),
            Delivery::Event(Event::PaneCommandFinished(PaneCommandFinished {
                pane_id: first_pane_id,
                exit_code: Some(0),
            })),
            Delivery::Event(Event::PaneCommandFinished(PaneCommandFinished {
                pane_id: second_pane_id,
                exit_code: None,
            })),
        ]
    );
}

#[test]
fn a_device_querys_reply_is_written_back_to_the_pty() {
    let (mut runtime, fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    spawn_test_pane(&fake_pty_backend, pane_id);

    // DSR 5 (operating status) embedded in ordinary output.
    runtime.handle_pty_output(pane_id, b"hi\x1b[5n");

    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        vec![b"\x1b[0n".to_vec()]
    );
}

#[test]
fn replies_from_one_chunk_are_written_as_one_batch_in_query_order() {
    let (mut runtime, fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    spawn_test_pane(&fake_pty_backend, pane_id);

    runtime.handle_pty_output(pane_id, b"\x1b[5n\x1b[6n");

    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        vec![b"\x1b[0n\x1b[1;1R".to_vec()]
    );
}

#[test]
fn output_without_a_query_writes_nothing_back() {
    let (mut runtime, fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    spawn_test_pane(&fake_pty_backend, pane_id);

    runtime.handle_pty_output(pane_id, b"hi\x1b[31m");

    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_failed_reply_write_is_dropped_and_output_still_lands() {
    let (mut runtime, fake_pty_backend) = build_test_server();
    // The engine exists but the pane was never spawned in the backend, so the
    // reply write fails with an unknown-pane error.
    let pane_id = insert_test_terminal_engine(&mut runtime);

    runtime.handle_pty_output(pane_id, b"x\x1b[5n");

    // The chunk still reached the grid and scheduled a render; the failed
    // write left no record.
    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 0), 'x');
    assert!(runtime.render_scheduler.claim_due_render(Instant::now()));
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap_err(),
        PtyError::UnknownPane { pane_id }
    );
}

#[test]
fn bytes_for_a_pane_with_no_engine_are_ignored() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let live_pane_id = insert_test_terminal_engine(&mut runtime);
    let closed_pane_id = PaneId::new();

    runtime.handle_pty_output(closed_pane_id, b"\x1b[31mboom");

    // No engine changed, no engine was created, and no render was scheduled.
    assert_eq!(get_pane_cell_character(&runtime, live_pane_id, 0, 0), ' ');
    assert_eq!(runtime.list_terminal_engines().len(), 1);
    assert!(!runtime.render_scheduler.claim_due_render(Instant::now()));
}

#[test]
fn an_empty_chunk_schedules_a_render_and_leaves_the_grid_alone() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);

    runtime.handle_pty_output(pane_id, b"");

    assert_eq!(get_pane_cell_character(&runtime, pane_id, 0, 0), ' ');
    assert_eq!(
        runtime.list_terminal_engines()[&pane_id]
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 0)
    );
    assert!(runtime.render_scheduler.claim_due_render(Instant::now()));
}

#[test]
fn lines_scrolled_off_the_top_enter_the_panes_scrollback() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);

    // Five lines on a three-row grid: the first two scroll off the top.
    runtime.handle_pty_output(pane_id, b"a\r\nb\r\nc\r\nd\r\ne");

    let scrollback_state = runtime.list_terminal_engines()[&pane_id]
        .get_terminal_state()
        .get_scrollback();
    assert_eq!(scrollback_state.get_total_pushed_line_count(), 2);
    assert_eq!(scrollback_state.get_retained_line_count(), 2);
}

#[test]
fn erasing_the_scrollback_empties_it_and_keeps_the_push_count() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    runtime.handle_pty_output(pane_id, b"a\r\nb\r\nc\r\nd\r\ne");

    // CSI 3 J drops every retained line.
    runtime.handle_pty_output(pane_id, b"\x1b[3J");

    let scrollback_state = runtime.list_terminal_engines()[&pane_id]
        .get_terminal_state()
        .get_scrollback();
    assert_eq!(scrollback_state.get_retained_line_count(), 0);
    assert_eq!(scrollback_state.get_total_pushed_line_count(), 2);
}

#[test]
fn entering_the_alternate_screen_keeps_the_primary_scrollback() {
    let (mut runtime, _fake_pty_backend) = build_test_server();
    let pane_id = insert_test_terminal_engine(&mut runtime);
    runtime.handle_pty_output(pane_id, b"a\r\nb\r\nc\r\nd\r\ne");

    runtime.handle_pty_output(pane_id, b"\x1b[?1049h");

    let terminal_state = runtime.list_terminal_engines()[&pane_id].get_terminal_state();
    assert!(!terminal_state.is_primary_screen_active());
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 2);
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .expect("cell")
            .get_character(),
        ' '
    );
}
