//! Frame cost on the session plane. For 1, 4 and 9 panes of 200×50 cells, each
//! filled by one `fullscreen` repaint, the program prints the bytes of one
//! painted update and the median time of 5 runs for each of its three steps:
//!
//! - build: `Server::build_snapshot`, then `build_wire_frame`
//! - encode: `PaintedUpdateSender::build_update_frames`
//! - decode: `PaintedUpdateReceiver::accept_session_frame` on every part
//!
//! Each run gives the mean of 1000 updates, after one run that is not counted.
//! The program measures two updates: the full frame of a client that holds
//! nothing, and the frame in which one row of the top-left pane changed.
//!
//! Example — `cargo bench -p koshi-runtime --bench frame_cost` prints lines
//! such as `9 panes, one row changed: 260 bytes, build 20.5 µs, encode 3.0 µs,
//! decode 1.5 µs`.

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, NewPaneArgs, NewPanePlacement,
    ResizePaneArgs,
};
use koshi_core::event::Event;
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId};
use koshi_ipc::transport::ReceivedFrame;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::runtime::frame::build_wire_frame;
use koshi_runtime::runtime::painted_update::{
    PaintedUpdateReceiver, PaintedUpdateSender, ReceivedPaneView, SessionFrame,
};
use koshi_runtime::runtime::pty_inbox::InboxSink;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::output_workloads::{
    build_workload_bytes, OutputWorkload, DEFAULT_WORKLOAD_SEED,
};

const PANE_COLUMN_COUNT: u16 = 200;
const PANE_ROW_COUNT: u16 = 50;
/// The panes on each side of the measured grids: 1×1, 2×2 and 3×3.
const GRID_SIDE_PANE_COUNTS: [u16; 3] = [1, 2, 3];
const MEASUREMENT_RUN_COUNT: usize = 5;
const UPDATE_COUNT_PER_RUN: u32 = 1000;
/// The row, counted from 1, that each one-row update writes.
const CHANGED_ROW_NUMBER: u16 = 25;

/// The bytes of one painted update and the time each of its steps took.
#[derive(Debug, Default, Clone, Copy)]
struct UpdateCost {
    update_byte_count: usize,
    build_duration: Duration,
    encode_duration: Duration,
    decode_duration: Duration,
}

impl UpdateCost {
    /// Add the durations of `update_cost` to these, and keep its byte count.
    fn add_update(&mut self, update_cost: UpdateCost) {
        self.update_byte_count = update_cost.update_byte_count;
        self.build_duration += update_cost.build_duration;
        self.encode_duration += update_cost.encode_duration;
        self.decode_duration += update_cost.decode_duration;
    }
}

fn main() {
    for grid_side_pane_count in GRID_SIDE_PANE_COUNTS {
        benchmark_pane_grid(grid_side_pane_count);
    }
}

fn benchmark_pane_grid(grid_side_pane_count: u16) {
    let (mut server, client_id, pane_ids) = build_pane_grid_server(grid_side_pane_count);
    let pane_count = pane_ids.len();

    let _ = measure_full_frames(&server, client_id);
    let full_frame_run_costs: Vec<UpdateCost> = (0..MEASUREMENT_RUN_COUNT)
        .map(|_| measure_full_frames(&server, client_id))
        .collect();
    print_update_cost(
        &format!("{pane_count} panes, full frame"),
        &full_frame_run_costs,
    );

    let _ = measure_one_row_changes(&mut server, client_id, pane_ids[0]);
    let one_row_run_costs: Vec<UpdateCost> = (0..MEASUREMENT_RUN_COUNT)
        .map(|_| measure_one_row_changes(&mut server, client_id, pane_ids[0]))
        .collect();
    print_update_cost(
        &format!("{pane_count} panes, one row changed"),
        &one_row_run_costs,
    );
}

/// A server on a fake PTY backend holding one session. Its attached client
/// views `grid_side_pane_count` columns of `grid_side_pane_count` panes, each
/// 200×50 cells and filled by one `fullscreen` repaint seeded with
/// `DEFAULT_WORKLOAD_SEED` plus the pane's index. Returns the server, the
/// client's id and the pane ids, column by column from the top-left pane.
///
/// # Panics
/// When the session refuses a split or a resize, or a pane does not come out
/// at 200×50 cells.
fn build_pane_grid_server(grid_side_pane_count: u16) -> (Server, ClientId, Vec<PaneId>) {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender),
    )));
    let mut server =
        Server::from_runtime_parts(pty_backend, runtime_event_receiver, Arc::default());
    let viewport_size = Size {
        column_count: grid_side_pane_count * (PANE_COLUMN_COUNT + 2),
        row_count: grid_side_pane_count * (PANE_ROW_COUNT + 2) + 2,
    };
    let session_id = SessionId::new();
    server
        .bootstrap_session(
            session_id,
            "bench".to_string(),
            viewport_size,
            SystemTime::UNIX_EPOCH,
        )
        .expect("the fake PTY backend spawns the first pane");
    let tab_id = *server.list_sessions()[&session_id]
        .tabs
        .keys()
        .next()
        .expect("the session holds its first tab");
    let client_id = ClientId::new();
    let _ = server.handle_client_attach(
        session_id,
        client_id,
        viewport_size,
        None,
        tab_id,
        None,
        SystemTime::UNIX_EPOCH,
        false,
    );
    let first_pane_id = *server
        .list_terminal_engines()
        .keys()
        .next()
        .expect("the session holds its first pane");

    let mut column_top_pane_ids = vec![first_pane_id];
    for _ in 1..grid_side_pane_count {
        let last_column_pane_id = column_top_pane_ids[column_top_pane_ids.len() - 1];
        column_top_pane_ids.push(create_split_pane(
            &mut server,
            client_id,
            last_column_pane_id,
            Direction::Right,
        ));
    }
    for column_top_pane_id in &column_top_pane_ids[..column_top_pane_ids.len() - 1] {
        resize_pane_to_length(
            &mut server,
            client_id,
            *column_top_pane_id,
            Direction::Right,
            PANE_COLUMN_COUNT,
        );
    }

    let mut pane_ids = Vec::new();
    for column_top_pane_id in column_top_pane_ids {
        let mut column_pane_ids = vec![column_top_pane_id];
        for _ in 1..grid_side_pane_count {
            let last_row_pane_id = column_pane_ids[column_pane_ids.len() - 1];
            column_pane_ids.push(create_split_pane(
                &mut server,
                client_id,
                last_row_pane_id,
                Direction::Down,
            ));
        }
        for row_pane_id in &column_pane_ids[..column_pane_ids.len() - 1] {
            resize_pane_to_length(
                &mut server,
                client_id,
                *row_pane_id,
                Direction::Down,
                PANE_ROW_COUNT,
            );
        }
        pane_ids.extend(column_pane_ids);
    }

    for (pane_index, pane_id) in pane_ids.iter().enumerate() {
        assert_eq!(
            read_pane_grid_size(&server, *pane_id),
            (PANE_ROW_COUNT, PANE_COLUMN_COUNT),
            "pane {pane_index} of the {grid_side_pane_count}×{grid_side_pane_count} grid"
        );
        let repaint_bytes = build_workload_bytes(
            OutputWorkload::Fullscreen,
            1,
            DEFAULT_WORKLOAD_SEED + pane_index as u64,
        );
        server.handle_pty_output(*pane_id, &repaint_bytes);
    }
    (server, client_id, pane_ids)
}

/// Split `source_pane_id` toward `direction` as a keybinding of `client_id`,
/// and return the new pane's id.
fn create_split_pane(
    server: &mut Server,
    client_id: ClientId,
    source_pane_id: PaneId,
    direction: Direction,
) -> PaneId {
    let emitted_events = submit_client_command(
        server,
        client_id,
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(source_pane_id),
                tab_id: None,
                direction,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: Some(client_id),
        }),
    );
    emitted_events
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::PaneCreated(pane_created) => Some(pane_created.pane_id),
            _ => None,
        })
        .expect("the split reports the new pane")
}

/// Move the `direction` border of `pane_id` until the pane holds
/// `target_cell_count` columns (`Left`, `Right`) or rows (`Up`, `Down`).
fn resize_pane_to_length(
    server: &mut Server,
    client_id: ClientId,
    pane_id: PaneId,
    direction: Direction,
    target_cell_count: u16,
) {
    let (row_count, column_count) = read_pane_grid_size(server, pane_id);
    let current_cell_count = match direction {
        Direction::Left | Direction::Right => column_count,
        Direction::Up | Direction::Down => row_count,
    };
    if current_cell_count == target_cell_count {
        return;
    }
    let _ = submit_client_command(
        server,
        client_id,
        Command::ResizePane(ResizePaneArgs {
            pane_id: Some(pane_id),
            direction,
            resize_amount_cells: target_cell_count as i16 - current_cell_count as i16,
        }),
    );
}

/// Run `command` as a keybinding of `client_id`, and return the events it
/// emitted. Panics when the session refuses it.
fn submit_client_command(server: &mut Server, client_id: ClientId, command: Command) -> Vec<Event> {
    match server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        command,
    )) {
        CommandResult::Ok { emitted_events, .. } => emitted_events,
        CommandResult::Rejected { reason, help, .. } => {
            panic!("the session refuses a bench command: {reason:?}, {help:?}")
        }
    }
}

/// The `(row_count, column_count)` of the grid of `pane_id`'s engine.
fn read_pane_grid_size(server: &Server, pane_id: PaneId) -> (u16, u16) {
    server.list_terminal_engines()[&pane_id]
        .get_terminal_state()
        .get_active_grid()
        .get_grid_dimensions()
}

/// The summed cost of `UPDATE_COUNT_PER_RUN` full frames, each carried from a
/// new sender to a new receiver.
fn measure_full_frames(server: &Server, client_id: ClientId) -> UpdateCost {
    let mut run_cost = UpdateCost::default();
    for _ in 0..UPDATE_COUNT_PER_RUN {
        let mut painted_update_sender = PaintedUpdateSender::new();
        let mut painted_update_receiver = PaintedUpdateReceiver::new();
        run_cost.add_update(carry_painted_update(
            server,
            client_id,
            &mut painted_update_sender,
            &mut painted_update_receiver,
        ));
    }
    run_cost
}

/// The summed cost of `UPDATE_COUNT_PER_RUN` updates that each change one row
/// of `pane_id`, carried after one full frame from one sender to one
/// receiver. Each update writes `─` at row 25, column 1 when its index is
/// even, and `│` when it is odd.
fn measure_one_row_changes(
    server: &mut Server,
    client_id: ClientId,
    pane_id: PaneId,
) -> UpdateCost {
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    let _ = carry_painted_update(
        server,
        client_id,
        &mut painted_update_sender,
        &mut painted_update_receiver,
    );
    let mut run_cost = UpdateCost::default();
    for update_index in 0..UPDATE_COUNT_PER_RUN {
        let written_character = if update_index % 2 == 0 { '─' } else { '│' };
        server.handle_pty_output(
            pane_id,
            format!("\x1b[{CHANGED_ROW_NUMBER};1H{written_character}").as_bytes(),
        );
        run_cost.add_update(carry_painted_update(
            server,
            client_id,
            &mut painted_update_sender,
            &mut painted_update_receiver,
        ));
    }
    run_cost
}

/// Build the update that turns what `painted_update_sender` sent last into the
/// view of `client_id`, then decode and apply each of its parts with
/// `painted_update_receiver`.
///
/// # Panics
/// When the update changes nothing, does not fit the frame cap, or applies as
/// anything other than the snapshot's painted frame: the same JSON part, and
/// for each pane the same id, cursor, scroll position, retained line count
/// and rows.
fn carry_painted_update(
    server: &Server,
    client_id: ClientId,
    painted_update_sender: &mut PaintedUpdateSender,
    painted_update_receiver: &mut PaintedUpdateReceiver,
) -> UpdateCost {
    let build_start_instant = Instant::now();
    let render_snapshot = server
        .build_snapshot(client_id)
        .expect("the client is attached");
    let painted_frame = build_wire_frame(&render_snapshot);
    let encode_start_instant = Instant::now();
    let part_payloads = painted_update_sender
        .build_update_frames(&render_snapshot, &painted_frame, false)
        .expect("the update fits the frame cap")
        .expect("the update changes what the client holds");
    let encode_end_instant = Instant::now();
    let update_byte_count = part_payloads.iter().map(Vec::len).sum();

    let decode_start_instant = Instant::now();
    let mut last_session_frame = None;
    for part_payload in part_payloads {
        last_session_frame = painted_update_receiver
            .accept_session_frame(ReceivedFrame::<()>::Binary(part_payload))
            .expect("a sent part applies");
    }
    let decode_end_instant = Instant::now();
    let Some(SessionFrame::Painted(received_painted_frame)) = last_session_frame else {
        panic!("the last part gives the painted frame");
    };
    assert_eq!(*received_painted_frame.painted_frame, painted_frame);
    let expected_pane_views: Vec<ReceivedPaneView> = render_snapshot
        .pane_snapshots
        .iter()
        .map(|pane_snapshot| ReceivedPaneView {
            pane_id: pane_snapshot.pane_id,
            cursor_snapshot: pane_snapshot.cursor_snapshot,
            view_top_row_index: pane_snapshot.view_top_row_index,
            retained_line_count: pane_snapshot.scrollback_metadata.retained_line_count,
            terminal_grid_view: pane_snapshot.terminal_grid_view.clone(),
        })
        .collect();
    assert_eq!(received_painted_frame.pane_views, expected_pane_views);

    UpdateCost {
        update_byte_count,
        build_duration: encode_start_instant - build_start_instant,
        encode_duration: encode_end_instant - encode_start_instant,
        decode_duration: decode_end_instant - decode_start_instant,
    }
}

fn print_update_cost(case_name: &str, run_costs: &[UpdateCost]) {
    println!(
        "{case_name}: {} bytes, build {:.1} µs, encode {:.1} µs, decode {:.1} µs",
        run_costs[0].update_byte_count,
        compute_median_microseconds_per_update(
            run_costs
                .iter()
                .map(|run_cost| run_cost.build_duration)
                .collect()
        ),
        compute_median_microseconds_per_update(
            run_costs
                .iter()
                .map(|run_cost| run_cost.encode_duration)
                .collect()
        ),
        compute_median_microseconds_per_update(
            run_costs
                .iter()
                .map(|run_cost| run_cost.decode_duration)
                .collect()
        ),
    );
}

/// The median of `run_durations`, divided by `UPDATE_COUNT_PER_RUN`, in
/// microseconds.
fn compute_median_microseconds_per_update(mut run_durations: Vec<Duration>) -> f64 {
    run_durations.sort_unstable();
    run_durations[run_durations.len() / 2].as_secs_f64() * 1_000_000.0
        / f64::from(UPDATE_COUNT_PER_RUN)
}
