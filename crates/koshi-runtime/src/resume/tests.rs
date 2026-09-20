//! Tests for the state a session server carries across a process-image swap:
//! a populated server drained into a resume file and rebuilt from it, what the
//! drain leaves behind, a sequence the swap cut in half finishing in the next
//! image, what the header still yields when the body cannot be read, what a
//! body format this build does not know is answered with, what a body written
//! in the older client format reads back as, what a pane carried pane's size, exit
//! status and applied quit read back as, and what a write over an existing
//! file and a write into a directory that is not there each do.

use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, FocusPaneArgs, FocusTarget,
    GridPosition, NewPaneArgs, NewTabArgs, Selection, SelectionKind,
};
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, TabId};
use koshi_core::process::PtySize;
use koshi_pty::backend::state::CarriedPtyPane;
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_session::client::{Client, ClientOrigin, ClientRegistry};
use koshi_terminal::engine::GraphicsEvent;
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};
use koshi_terminal::grid::state::Cell;
use koshi_test_support::fake_pty::FakePtyBackend;
use tempfile::TempDir;

use super::*;
use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

/// The viewport of the client the session is bootstrapped with.
const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The viewport of the second client, sized apart from [`TEST_VIEWPORT_SIZE`] so the two
/// clients are told apart by what they hold.
const SECOND_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 30,
};

/// The pieces a test drives a carried server through: the server itself, the
/// session it serves, its two clients, and its two tabs.
struct Populated {
    server: Server,
    /// Kept alive so the runtime inbox never loses its last sender.
    _inbox_tx: mpsc::Sender<RuntimeEvent>,
    session_id: SessionId,
    first_client: ClientId,
    second_client: ClientId,
    first_tab: TabId,
    second_tab: TabId,
}

/// Return the top row of a screen as text; a blank cell reads as a space.
fn get_first_terminal_row(terminal_state: &TerminalState) -> String {
    let (_, column_count) = terminal_state.get_active_grid().get_grid_dimensions();
    (0..column_count)
        .map(|column_index| {
            terminal_state
                .get_active_grid()
                .get_cell(0, column_index)
                .map_or(' ', Cell::get_character)
        })
        .collect()
}

/// Run `command` as a keybinding of `client_id`, and panic unless it was applied.
fn apply_keybinding_command(server: &mut Server, client_id: ClientId, command: Command) {
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::UNIX_EPOCH,
        command,
    );
    let command_id = envelope.command_id;
    match server.submit_command(envelope) {
        CommandResult::Ok {
            command_id: applied,
            ..
        } => assert_eq!(applied, command_id),
        other => panic!("the command must be applied, got {other:?}"),
    }
}

/// A server holding one session with two tabs and four panes — the first tab
/// split twice so its tree nests a split inside a split — two clients on
/// different tabs with their own focus, zoom, scroll offset and selection, and
/// output fed into every pane's engine.
fn populated_server() -> Populated {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let mut server = Server::from_runtime_parts(pty_backend, inbox_rx, inbox_tx.clone());

    let session_id = SessionId::new();
    let first_client = server
        .bootstrap_local_named(
            session_id,
            "carried".to_string(),
            TEST_VIEWPORT_SIZE,
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap the session");
    let first_tab = *server.session_by_id[&session_id]
        .tabs
        .keys()
        .next()
        .expect("the bootstrapped tab");

    // Two splits on the first tab: rightward, then downward inside the pane the
    // first split created, so the tree holds a split inside a split.
    for direction in [Direction::Right, Direction::Down] {
        apply_keybinding_command(
            &mut server,
            first_client,
            Command::NewPane(NewPaneArgs {
                source_pane_id: None,
                tab_id: None,
                direction,
                should_stack: false,
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
        first_client,
        Command::NewTab(NewTabArgs::default()),
    );
    let second_tab = *server.session_by_id[&session_id]
        .tabs
        .keys()
        .find(|&&tab| tab != first_tab)
        .expect("the created tab");

    // A second client, left on the first tab, so the two clients hold different
    // active tabs.
    let second_client = ClientId::new();
    server.handle_client_attach(
        session_id,
        second_client,
        SECOND_VIEWPORT_SIZE,
        None,
        first_tab,
        SystemTime::UNIX_EPOCH,
        false,
    );

    let panes = list_tab_pane_ids(&server, session_id, first_tab);
    // The second client focuses the last pane of the first tab, so the two
    // clients hold different focus as well as different tabs.
    apply_keybinding_command(
        &mut server,
        second_client,
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(panes[2]),
            client_id: None,
        }),
    );
    let session = server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session");
    let first_client_state = session
        .clients
        .get_client_mut_by_id(first_client)
        .expect("the first client");
    first_client_state.zoom_pane(first_tab, panes[0]);
    first_client_state.set_scroll_offset(panes[1], 7);
    let second_client_state = session
        .clients
        .get_client_mut_by_id(second_client)
        .expect("the second client");
    second_client_state.zoom_pane(first_tab, panes[2]);
    second_client_state.set_scroll_offset(panes[0], 12);
    second_client_state.set_selection(
        panes[1],
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

    // Distinct output per pane, so a screen that came back under the wrong pane
    // is caught.
    for (pane_index, pane_id) in list_session_pane_ids(&server, session_id)
        .into_iter()
        .enumerate()
    {
        server.handle_pty_output(pane_id, format!("pane {pane_index} output").as_bytes());
    }

    Populated {
        server,
        _inbox_tx: inbox_tx,
        session_id,
        first_client,
        second_client,
        first_tab,
        second_tab,
    }
}

/// Return the pane ids of `tab`, in layout order.
fn list_tab_pane_ids(server: &Server, session_id: SessionId, tab: TabId) -> Vec<PaneId> {
    server.session_by_id[&session_id].tabs[&tab]
        .get_layout_tree()
        .list_leaf_pane_ids()
}

/// Return every pane id of `session_id`, in tab order then layout order.
fn list_session_pane_ids(server: &Server, session_id: SessionId) -> Vec<PaneId> {
    server.session_by_id[&session_id]
        .tabs
        .values()
        .flat_map(|tab| tab.get_layout_tree().list_leaf_pane_ids())
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
        terminal_state_by_pane_id: HashMap::new(),
        undecoded_bytes_by_pane_id: HashMap::new(),
        graphics_undecoded_bytes_by_pane_id: HashMap::new(),
        graphics_screen_continuation_by_pane_id: HashMap::new(),
        graphics_screen_wrapper_active_by_pane_id: HashMap::new(),
        graphics_tmux_continuation_by_pane_id: HashMap::new(),
        graphics_tmux_wrapper_active_by_pane_id: HashMap::new(),
        graphics_events_by_pane_id: HashMap::new(),
        graphics_transport_by_pane_id: HashMap::new(),
        synchronized_output_by_pane_id: HashMap::new(),
        carried_quit,
    }
}

/// Build a resumed server from `body`, over detached handles for every pane the
/// header names and the sizes that header carries.
fn build_resumed_server(
    header: &ResumeHeader,
    body: ResumeBody,
) -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let pty_handle_by_pane_id: HashMap<PaneId, PtyHandle> = header
        .carried_panes
        .iter()
        .map(|pane| (pane.pane_id, PtyHandle::from_detached_pane_id(pane.pane_id)))
        .collect();
    let pty_size_by_pane_id: HashMap<PaneId, PtySize> = header
        .carried_panes
        .iter()
        .map(|pane| {
            (
                pane.pane_id,
                PtySize {
                    column_count: pane.column_count,
                    row_count: pane.row_count,
                },
            )
        })
        .collect();
    let server = Server::resume(
        pty_backend,
        inbox_rx,
        inbox_tx.clone(),
        body,
        pty_handle_by_pane_id,
        pty_size_by_pane_id,
    );
    (server, inbox_tx)
}

#[test]
fn a_carried_session_reads_back_with_every_tab_pane_client_and_screen() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let expected_tabs = populated.server.session_by_id[&session_id].tabs.clone();
    let expected_records = populated.server.session_by_id[&session_id].panes.clone();
    let expected_sizes = populated.server.pty_size_by_pane_id.clone();

    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");
    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");
    let (resumed, _inbox_tx) = build_resumed_server(&read_header, read_body);

    assert_eq!(read_header, header, "the header must read back unchanged");
    assert_eq!(read_header.session_id, session_id);
    assert_eq!(read_header.session_name, "carried");
    assert_eq!(resumed.session_by_id.len(), 1, "one session must come back");
    let session = &resumed.session_by_id[&session_id];
    assert_eq!(session.session_name, "carried");
    assert_eq!(session.tabs, expected_tabs, "every tab and its layout tree");
    assert_eq!(
        session.panes, expected_records,
        "every pane carried by the header"
    );
    assert_eq!(
        session.clients.client_count(),
        2,
        "both clients must come back"
    );

    let resumed_first_client = session
        .clients
        .get_client_by_id(populated.first_client)
        .expect("the first client");
    let first_tab_panes = expected_tabs[&populated.first_tab]
        .get_layout_tree()
        .list_leaf_pane_ids();
    assert_eq!(resumed_first_client.get_active_tab(), populated.second_tab);
    assert_eq!(resumed_first_client.get_viewport_size(), TEST_VIEWPORT_SIZE);
    assert_eq!(
        resumed_first_client.get_zoomed_pane(populated.first_tab),
        Some(first_tab_panes[0])
    );
    assert_eq!(
        resumed_first_client.get_scroll_offset(first_tab_panes[1]),
        7
    );
    assert_eq!(resumed_first_client.get_selection(first_tab_panes[1]), None);

    let resumed_second_client = session
        .clients
        .get_client_by_id(populated.second_client)
        .expect("the second client");
    assert_eq!(resumed_second_client.get_active_tab(), populated.first_tab);
    assert_eq!(
        resumed_second_client.get_viewport_size(),
        SECOND_VIEWPORT_SIZE
    );
    assert_eq!(
        resumed_second_client.get_focused_pane(populated.first_tab),
        Some(first_tab_panes[2])
    );
    assert_eq!(
        resumed_second_client.get_zoomed_pane(populated.first_tab),
        Some(first_tab_panes[2])
    );
    assert_eq!(
        resumed_second_client.get_scroll_offset(first_tab_panes[0]),
        12
    );
    assert_eq!(
        resumed_second_client.get_selection(first_tab_panes[1]),
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
        body.terminal_state_by_pane_id.len(),
        4,
        "four panes must have a screen"
    );
    for (pane_id, screen) in &body.terminal_state_by_pane_id {
        assert_eq!(
            resumed.terminal_engine_by_pane_id[pane_id].get_terminal_state(),
            screen,
            "pane {pane_id} must come back with the screen it went out with"
        );
    }
    // Every pane was fed its own text, so a screen that came back under the
    // wrong pane reads the wrong line here.
    for (pane_index, pane) in panes.iter().enumerate() {
        assert_eq!(
            get_first_terminal_row(
                resumed.terminal_engine_by_pane_id[&pane.pane_id].get_terminal_state(),
            )
            .trim_end(),
            format!("pane {pane_index} output")
        );
    }
    assert_eq!(
        resumed.pty_size_by_pane_id, expected_sizes,
        "every pane's size"
    );
    let mut resumed_handles: Vec<PaneId> = resumed.pty_handle_by_pane_id.keys().copied().collect();
    resumed_handles.sort();
    let mut carried_ids: Vec<PaneId> = panes.iter().map(|pane| pane.pane_id).collect();
    carried_ids.sort();
    assert_eq!(
        resumed_handles, carried_ids,
        "one handle per carried pane must come back"
    );
}

#[test]
fn carrying_the_state_out_leaves_the_server_holding_nothing() {
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);

    let (_header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");

    assert_eq!(
        populated.server.terminal_engine_by_pane_id.len(),
        0,
        "every engine must have moved out"
    );
    assert_eq!(
        populated.server.session_by_id.len(),
        0,
        "every session must have moved out"
    );
    assert_eq!(
        body.terminal_state_by_pane_id.len(),
        4,
        "every engine must be in the body"
    );
    assert_eq!(
        body.session_by_id.len(),
        1,
        "the session must be in the body"
    );
    assert_eq!(
        body.undecoded_bytes_by_pane_id.len(),
        0,
        "no pane's parser was mid-sequence, so nothing is held"
    );
}

#[test]
fn a_report_the_swap_cut_in_half_finishes_in_the_next_image() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let pane = list_session_pane_ids(&populated.server, session_id)[0];
    let panes = build_carried_pty_panes(&populated.server, session_id);

    // The shell reports /Users/yuhan/Projects/koshi through OSC 7, and the last
    // chunk before the swap ends after `/Proj`.
    populated
        .server
        .handle_pty_output(pane, b"\x1b]7;file://host/Users/yuhan/Proj");

    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");
    assert_eq!(
        body.undecoded_bytes_by_pane_id,
        HashMap::from([(pane, b"\x1b]7;file://host/Users/yuhan/Proj".to_vec())]),
        "only the pane mid-report holds bytes, and it holds all of them"
    );
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");
    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");
    let (mut resumed, _inbox_tx) = build_resumed_server(&read_header, read_body);

    assert_eq!(
        resumed.terminal_engine_by_pane_id[&pane]
            .get_terminal_state()
            .get_current_working_directory(),
        None,
        "a report with no terminator sets no directory"
    );
    let initial_terminal_row =
        get_first_terminal_row(resumed.terminal_engine_by_pane_id[&pane].get_terminal_state());

    resumed.handle_pty_output(pane, b"ects/koshi\x07");

    let terminal_state = resumed.terminal_engine_by_pane_id[&pane].get_terminal_state();
    let reported_working_directory = terminal_state
        .get_current_working_directory()
        .expect("the report finished");
    assert_eq!(reported_working_directory.get_host(), Some("host"));
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/Users/yuhan/Projects/koshi")
    );
    assert_eq!(
        get_first_terminal_row(terminal_state),
        initial_terminal_row,
        "the rest of the report joined the sequence instead of printing"
    );
}

#[test]
fn a_body_written_without_the_held_bytes_reads_back_with_none() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");

    // The body with its map of held bytes taken out of the JSON.
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&resume_file_path).expect("read the file"))
            .expect("valid json");
    on_disk["raw_body"]
        .as_object_mut()
        .expect("the body is a map")
        .remove("undecoded_bytes_by_pane_id");
    std::fs::write(
        &resume_file_path,
        serde_json::to_vec(&on_disk).expect("encode"),
    )
    .expect("rewrite the file");

    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_header.resume_format, &raw_body).expect("read the body back");

    assert_eq!(
        read_body.terminal_state_by_pane_id.len(),
        4,
        "every screen still reads back"
    );
    assert_eq!(read_body.undecoded_bytes_by_pane_id, HashMap::new());
}

#[test]
fn the_header_names_every_pane_with_the_size_the_server_holds_for_it() {
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let sizes = populated.server.pty_size_by_pane_id.clone();

    let (header, _body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");

    assert_eq!(header.resume_format, RESUME_FORMAT);
    assert_eq!(
        header.carried_panes.len(),
        4,
        "one carried pane per live pane"
    );
    for (pane_index, carried_pane) in header.carried_panes.iter().enumerate() {
        assert_eq!(carried_pane.pane_id, panes[pane_index].pane_id);
        assert_eq!(carried_pane.process_id, 5000 + pane_index as u32);
        #[cfg(unix)]
        assert_eq!(carried_pane.terminal_fd, Some(20 + pane_index as i32));
        #[cfg(windows)]
        assert_eq!(carried_pane.terminal_fd, None);
        let held = sizes[&carried_pane.pane_id];
        assert_eq!(
            (carried_pane.column_count, carried_pane.row_count),
            (held.column_count, held.row_count),
            "the header carries the size the server holds, not the backend's"
        );
    }
}

#[test]
fn a_pane_the_server_holds_no_size_for_takes_the_size_the_backend_reports() {
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let forgotten = panes[2].pane_id;
    populated.server.pty_size_by_pane_id.remove(&forgotten);

    let (header, _body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");

    let carried_pane = header
        .carried_panes
        .iter()
        .find(|carried_pane| carried_pane.pane_id == forgotten)
        .expect("the pane the server forgot");
    assert_eq!((carried_pane.column_count, carried_pane.row_count), (1, 1));
}

#[test]
fn an_unreadable_body_still_leaves_every_pane_descriptor_and_process_id() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");

    // Only the body is broken; the header on disk is untouched.
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&resume_file_path).expect("read the file"))
            .expect("valid json");
    on_disk["raw_body"] = serde_json::json!({ "session_by_id": "not-a-map" });
    std::fs::write(
        &resume_file_path,
        serde_json::to_vec(&on_disk).expect("encode"),
    )
    .expect("rewrite the file");

    let (read_header, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    assert_eq!(read_header, header, "the header survives a broken body");
    assert_eq!(read_header.carried_panes.len(), 4);
    for (pane_index, carried_pane) in read_header.carried_panes.iter().enumerate() {
        assert_eq!(carried_pane.process_id, 5000 + pane_index as u32);
        #[cfg(unix)]
        assert_eq!(carried_pane.terminal_fd, Some(20 + pane_index as i32));
        #[cfg(windows)]
        assert_eq!(carried_pane.terminal_fd, None);
    }
    match read_resume_body(read_header.resume_format, &raw_body) {
        Err(StorageError::Corrupt { detail }) => {
            assert_eq!(
                detail,
                "resume body is unreadable: invalid type: string \"not-a-map\", expected a map at line 1 column 28"
            );
        }
        other => panic!("expected a corrupt body, got {other:?}"),
    }
}

#[test]
fn a_body_format_this_build_does_not_know_is_refused_by_both_numbers() {
    let too_new = RESUME_FORMAT + 1;

    match read_resume_body(too_new, serde_json::value::RawValue::NULL) {
        Err(StorageError::Corrupt { detail }) => {
            assert_eq!(
                detail,
                format!(
                    "resume body format {too_new} is outside the {RESUME_FORMAT_MIN} to {RESUME_FORMAT} range this build reads"
                )
            );
        }
        other => panic!("expected a refused format, got {other:?}"),
    }
}

#[test]
fn a_resume_body_rejects_graphics_transport_that_exceeds_wrapper_depth() {
    let pane = PaneId::new();
    let mut nested = serde_json::json!({ "carry_bytes": [] });
    for _ in 0..9 {
        nested = serde_json::json!({ "screen_inner_transport": nested });
    }
    let serialized_resume_body = serde_json::json!({
        "session_by_id": {},
        "terminal_state_by_pane_id": {},
        "graphics_transport_by_pane_id": { pane.get_uuid().to_string(): nested },
    });
    let raw_resume_body =
        serde_json::value::RawValue::from_string(serialized_resume_body.to_string())
            .expect("the body is json");

    match read_resume_body(RESUME_FORMAT, &raw_resume_body) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail.split(" at line ").next(),
            Some("resume body is unreadable: graphics wrapper nesting exceeds the supported limit")
        ),
        other => panic!("expected an over-depth graphics body to be corrupt, got {other:?}"),
    }
}

#[test]
fn a_resume_body_rejects_graphics_transport_with_too_many_carry_bytes() {
    let pane = PaneId::new();
    let oversized = vec![0u8; 64 * 1024 + 1];
    let serialized_resume_body = serde_json::json!({
        "session_by_id": {},
        "terminal_state_by_pane_id": {},
        "graphics_transport_by_pane_id": {
            pane.get_uuid().to_string(): { "carry_bytes": oversized }
        },
    });
    let raw_resume_body =
        serde_json::value::RawValue::from_string(serialized_resume_body.to_string())
            .expect("the body is json");

    match read_resume_body(RESUME_FORMAT, &raw_resume_body) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail.split(" at line ").next(),
            Some("resume body is unreadable: graphics carry exceeds 65536 bytes")
        ),
        other => panic!("expected an oversized graphics carry to be corrupt, got {other:?}"),
    }
}

#[test]
fn a_resume_body_rejects_legacy_graphics_carry_that_exceeds_the_limit() {
    let pane = PaneId::new();
    let oversized = vec![0u8; koshi_terminal::graphics::MAX_GRAPHICS_CARRY_BYTE_COUNT + 1];
    let serialized_resume_body = serde_json::json!({
        "session_by_id": {},
        "terminal_state_by_pane_id": {},
        "graphics_undecoded_bytes_by_pane_id": { pane.get_uuid().to_string(): oversized },
    });
    let raw_resume_body =
        serde_json::value::RawValue::from_string(serialized_resume_body.to_string())
            .expect("the body is json");

    match read_resume_body(RESUME_FORMAT, &raw_resume_body) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail.split(" at line ").next(),
            Some("resume body is unreadable: graphics carry exceeds 65536 bytes")
        ),
        other => panic!("expected oversized legacy graphics carry to be corrupt, got {other:?}"),
    }
}

#[test]
fn a_resume_body_rejects_queued_image_bytes_that_do_not_match_dimensions() {
    let pane = PaneId::new();
    let event: GraphicsEvent = Ok(ImageRecord {
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
    let mut event = serde_json::to_value(event).expect("the image event is json");
    event["Ok"]["image"]["rgba_bytes"] = serde_json::json!([255, 0, 0]);
    let serialized_resume_body = serde_json::json!({
        "session_by_id": {},
        "terminal_state_by_pane_id": {},
        "graphics_events_by_pane_id": { pane.get_uuid().to_string(): [event] },
    });
    let raw_resume_body =
        serde_json::value::RawValue::from_string(serialized_resume_body.to_string())
            .expect("the body is json");

    match read_resume_body(RESUME_FORMAT, &raw_resume_body) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail.split(" at line ").next(),
            Some("resume body is unreadable: decoded image RGBA length does not match its dimensions")
        ),
        other => panic!("expected an invalid queued image to be corrupt, got {other:?}"),
    }
}

#[test]
fn a_resume_body_rejects_graphics_error_text_over_the_control_limit() {
    let pane = PaneId::new();
    let event: GraphicsEvent = Err(koshi_terminal::graphics::GraphicsError::UnsupportedAction {
        protocol: GraphicsProtocol::Kitty,
        action: String::new(),
    });
    let mut event = serde_json::to_value(event).expect("the graphics error is json");
    event["Err"]["UnsupportedAction"]["action"] = serde_json::Value::String(
        "x".repeat(koshi_terminal::graphics::MAX_GRAPHICS_CONTROL_BYTE_COUNT + 1),
    );
    let serialized_resume_body = serde_json::json!({
        "session_by_id": {},
        "terminal_state_by_pane_id": {},
        "graphics_events_by_pane_id": { pane.get_uuid().to_string(): [event] },
    });
    let raw_resume_body =
        serde_json::value::RawValue::from_string(serialized_resume_body.to_string())
            .expect("the body is json");

    match read_resume_body(RESUME_FORMAT, &raw_resume_body) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail.split(" at line ").next(),
            Some("resume body is unreadable: graphics error text exceeds 8192 bytes")
        ),
        other => panic!("expected oversized graphics error text to be corrupt, got {other:?}"),
    }
}

#[test]
fn a_resume_body_rejects_a_graphics_event_list_over_the_engine_limit() {
    let pane = PaneId::new();
    let event: GraphicsEvent = Ok(ImageRecord {
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
    let event = serde_json::to_value(event).expect("the image event is json");
    let events = vec![event; koshi_terminal::engine::MAX_GRAPHICS_EVENT_COUNT + 1];
    let serialized_resume_body = serde_json::json!({
        "session_by_id": {},
        "terminal_state_by_pane_id": {},
        "graphics_events_by_pane_id": { pane.get_uuid().to_string(): events },
    });
    let raw_resume_body =
        serde_json::value::RawValue::from_string(serialized_resume_body.to_string())
            .expect("the body is json");

    match read_resume_body(RESUME_FORMAT, &raw_resume_body) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail.split(" at line ").next(),
            Some("resume body is unreadable: graphics event count exceeds 64")
        ),
        other => panic!("expected an oversized graphics event list to be corrupt, got {other:?}"),
    }
}

#[test]
fn a_resume_body_accepts_the_queue_full_report_after_queued_events() {
    let pane = PaneId::new();
    let event: GraphicsEvent = Ok(ImageRecord {
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
    let event = serde_json::to_value(event).expect("the image event is json");
    let mut events = vec![event; koshi_terminal::engine::MAX_GRAPHICS_EVENT_COUNT];
    events.push(serde_json::json!({
        "Err": {
            "QueueFull": { "dropped_event_count": 2 }
        }
    }));
    let serialized_resume_body = serde_json::json!({
        "session_by_id": {},
        "terminal_state_by_pane_id": {},
        "graphics_events_by_pane_id": { pane.get_uuid().to_string(): events },
    });
    let raw_resume_body =
        serde_json::value::RawValue::from_string(serialized_resume_body.to_string())
            .expect("the body is json");

    let parsed_resume_body =
        read_resume_body(RESUME_FORMAT, &raw_resume_body).expect("the valid queue batch reads");

    assert_eq!(
        parsed_resume_body.graphics_events_by_pane_id[&pane].len(),
        koshi_terminal::engine::MAX_GRAPHICS_EVENT_BATCH_COUNT
    );
}

#[test]
fn a_header_naming_an_unknown_format_still_reads_back_whole() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let session_id = SessionId::new();
    let header = ResumeHeader {
        resume_format: RESUME_FORMAT + 1,
        session_id,
        session_name: "from-a-newer-build".to_string(),
        carried_panes: vec![CarriedPane {
            pane_id: PaneId::new(),
            process_id: 4242,
            row_count: 20,
            column_count: 78,
            terminal_fd: Some(9),
            terminal_name: Some("/dev/ttys009".to_string()),
            exit_status: None,
        }],
    };
    let resume_body = ResumeBody {
        session_by_id: HashMap::new(),
        terminal_state_by_pane_id: HashMap::new(),
        undecoded_bytes_by_pane_id: HashMap::new(),
        graphics_undecoded_bytes_by_pane_id: HashMap::new(),
        graphics_screen_continuation_by_pane_id: HashMap::new(),
        graphics_screen_wrapper_active_by_pane_id: HashMap::new(),
        graphics_tmux_continuation_by_pane_id: HashMap::new(),
        graphics_tmux_wrapper_active_by_pane_id: HashMap::new(),
        graphics_events_by_pane_id: HashMap::new(),
        graphics_transport_by_pane_id: HashMap::new(),
        synchronized_output_by_pane_id: HashMap::new(),
        carried_quit: None,
    };
    write_resume_file(&resume_file_path, &header, &resume_body).expect("write the resume file");

    let (read, _raw_body) = read_resume_header(&resume_file_path).expect("read the header back");

    assert_eq!(read, header, "any build reads the header of any other");
}

#[test]
fn a_header_written_without_a_terminal_name_reads_back_with_none() {
    // A build that records no terminal name writes a pane carried pane without that
    // field. This build must still read that carried pane, and read the pane back
    // with no name rather than refusing the whole header.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let written = serde_json::json!({
        "header": {
            "resume_format": RESUME_FORMAT,
            "session_id": session_id,
            "session_name": "from-a-build-without-the-name",
            "carried_panes": [{
                "pane_id": pane_id,
                "process_id": 4242,
                "row_count": 20,
                "column_count": 78,
                "terminal_fd": 9,
            }],
        },
        "raw_body": {
            "session_by_id": {},
            "terminal_state_by_pane_id": {}
        },
    });
    std::fs::write(
        &resume_file_path,
        serde_json::to_vec(&written).expect("encode"),
    )
    .expect("write the resume file");

    let (read, _raw_body) = read_resume_header(&resume_file_path).expect("read the header back");

    assert_eq!(
        read,
        ResumeHeader {
            resume_format: RESUME_FORMAT,
            session_id,
            session_name: "from-a-build-without-the-name".to_string(),
            carried_panes: vec![CarriedPane {
                pane_id,
                process_id: 4242,
                row_count: 20,
                column_count: 78,
                terminal_fd: Some(9),
                terminal_name: None,
                exit_status: None,
            }],
        }
    );
}

#[test]
fn a_resumed_server_starts_with_no_socket_and_no_shutdown_pending() {
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");

    let (resumed, _inbox_tx) = build_resumed_server(&header, body);

    assert!(!resumed.is_quit_requested, "no quit is pending");
    assert!(!resumed.is_draining, "teardown has not begun");
    assert!(
        !resumed.should_shutdown_immediately,
        "no zero-grace quit is pending"
    );
    assert!(
        resumed.ipc_server().is_none(),
        "the control socket is bound after the swap, not carried through it"
    );
    assert_eq!(resumed.subscriptions.len(), 0, "no subscriber is carried");
}

#[test]
fn reading_a_resume_file_that_is_not_there_is_an_io_failure() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("missing.resume");

    match read_resume_header(&resume_file_path) {
        Err(StorageError::Io { detail }) => {
            assert!(
                detail.starts_with(&format!(
                    "read resume state at {}: ",
                    resume_file_path.display()
                )),
                "the failure must name the resume_file_path, got {detail}"
            );
        }
        other => panic!("expected an io failure, got {other:?}"),
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
        other => panic!("expected a corrupt failure, got {other:?}"),
    }
}

#[test]
fn a_body_format_below_the_oldest_this_build_reads_is_refused_by_both_numbers() {
    // The floor is checked as well as the ceiling: a build whose oldest format
    // has moved up must refuse a file written before that move rather than
    // read it as the shape it no longer has.
    let too_old = RESUME_FORMAT_MIN - 1;

    match read_resume_body(too_old, serde_json::value::RawValue::NULL) {
        Err(StorageError::Corrupt { detail }) => {
            assert_eq!(
                detail,
                format!(
                    "resume body format {too_old} is outside the {RESUME_FORMAT_MIN} to {RESUME_FORMAT} range this build reads"
                )
            );
        }
        other => panic!("expected a refused format, got {other:?}"),
    }
}

#[test]
fn a_resume_file_whose_bytes_stop_part_way_is_a_corrupt_failure_naming_the_path() {
    // A whole resume file lands at once, so a file cut short is disk damage
    // rather than a half-finished write. The header is inside the same JSON
    // document as the body, so bytes that stop part way cost the reader both.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");

    let whole = std::fs::read(&resume_file_path).expect("read the file back");
    let cut = whole.len() / 2;
    assert!(cut > 0, "the file must have bytes to cut");
    std::fs::write(&resume_file_path, &whole[..cut]).expect("rewrite the file cut short");

    match read_resume_header(&resume_file_path) {
        Err(StorageError::Corrupt { detail }) => {
            assert!(
                detail.starts_with(&format!(
                    "resume state at {} is unreadable: ",
                    resume_file_path.display()
                )),
                "the failure must name the resume_file_path, got {detail}"
            );
        }
        other => panic!("expected a corrupt failure, got {other:?}"),
    }
}

#[test]
fn a_body_missing_one_of_its_two_halves_is_corrupt_while_the_header_still_reads() {
    // The body is one JSON object with two named halves. A body holding only
    // the sessions is readable JSON, so nothing before the decode catches it —
    // the decode itself must, and it must cost the caller no pane carried pane.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");

    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&resume_file_path).expect("read the file"))
            .expect("valid json");
    on_disk["raw_body"]
        .as_object_mut()
        .expect("a body object")
        .remove("terminal_state_by_pane_id");
    std::fs::write(
        &resume_file_path,
        serde_json::to_vec(&on_disk).expect("encode"),
    )
    .expect("rewrite the file");

    let (read_back, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    assert_eq!(read_back, header, "the header survives a half body");
    assert_eq!(read_back.carried_panes.len(), 4, "with every pane it named");

    match read_resume_body(read_back.resume_format, &raw_body) {
        Err(StorageError::Corrupt { detail }) => {
            assert_eq!(
                detail,
                "resume body is unreadable: missing field `terminal_state_by_pane_id` at line 1 column \
                 "
                .to_string()
                    + &raw_body.get().len().to_string(),
                "the failure must name the half that is missing"
            );
        }
        other => panic!("expected a corrupt body, got {other:?}"),
    }
}

#[test]
fn a_session_holding_no_pane_carries_out_and_reads_back_with_no_pane() {
    // The swap runs whatever the session holds. A header naming no pane must
    // round-trip as an empty list rather than as an absent field, so the image
    // that reads it takes nothing back and waits for nothing.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("empty.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;

    let (header, body) = populated.server.carry_out(&[]).expect("a session to carry");
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");
    let (read_back, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_back.resume_format, &raw_body).expect("read the body back");

    assert_eq!(
        read_back.carried_panes,
        Vec::new(),
        "no pane crosses the swap"
    );
    assert_eq!(read_back.resume_format, RESUME_FORMAT);
    assert_eq!(read_back.session_id, session_id);
    assert_eq!(read_back.session_name, "carried");
    assert_eq!(
        read_body.terminal_state_by_pane_id.len(),
        4,
        "the screens still cross, since the header names what the backend holds"
    );

    let (resumed, _inbox_tx) = build_resumed_server(&read_back, read_body);
    assert_eq!(
        resumed.pty_handle_by_pane_id.len(),
        0,
        "and no handle comes back"
    );
    assert_eq!(
        resumed.pty_size_by_pane_id.len(),
        0,
        "and no size comes back"
    );
    assert_eq!(
        resumed.session_by_id.len(),
        1,
        "the session itself still does"
    );
}

#[test]
fn a_session_holding_many_panes_carries_every_one_of_them_in_order() {
    // Nothing in the file caps how many panes it names. Each carried pane must keep
    // its own descriptor, process id and size, and keep the order the backend
    // reported, so the image that reads it takes back the right terminal for
    // each pane.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("many.resume");
    let mut populated = populated_server();

    let many: Vec<CarriedPtyPane> = (0..64)
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

    let (header, body) = populated
        .server
        .carry_out(&many)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");
    let (read_back, _raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");

    assert_eq!(
        read_back.carried_panes.len(),
        64,
        "every pane must have a carried pane"
    );
    for (pane_index, carried_pane) in read_back.carried_panes.iter().enumerate() {
        assert_eq!(
            carried_pane.pane_id, many[pane_index].pane_id,
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
        // None of these panes is one the server holds a size for, so each takes
        // the size the backend reported.
        assert_eq!(
            (carried_pane.column_count, carried_pane.row_count),
            (40 + pane_index as u16, 10 + pane_index as u16),
            "carried pane {pane_index}"
        );
    }
}

#[test]
fn held_bytes_naming_a_pane_the_body_carries_no_screen_for_are_dropped_with_that_pane() {
    // The held bytes are keyed on their own, so a body can name a pane the
    // screens do not. The rebuilt server must open no screen for it: a pane
    // with no screen has no parser those bytes could finish a sequence in.
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let (header, mut body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");
    let stray = PaneId::new();
    body.undecoded_bytes_by_pane_id
        .insert(stray, b"\x1b[".to_vec());
    let mut carried_screens: Vec<PaneId> = body.terminal_state_by_pane_id.keys().copied().collect();
    carried_screens.sort();

    let (resumed, _inbox_tx) = build_resumed_server(&header, body);

    let mut rebuilt: Vec<PaneId> = resumed.terminal_engine_by_pane_id.keys().copied().collect();
    rebuilt.sort();
    assert_eq!(
        rebuilt, carried_screens,
        "only the panes the body carried a screen for come back, and the pane \
         named by the held bytes alone opens none"
    );
}

#[test]
fn a_screen_the_header_names_no_pane_for_comes_back_with_no_handle_and_no_size() {
    // The header and the body are written together, so the two agree in every
    // file this build writes. A body naming one more pane than the header must
    // still come back readable, with that pane holding a screen and nothing to
    // drive it.
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let unlisted_pane_id = PaneId::new();
    populated.server.terminal_engine_by_pane_id.insert(
        unlisted_pane_id,
        koshi_terminal::engine::TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );
    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");

    let mut named_by_the_header: Vec<PaneId> = header
        .carried_panes
        .iter()
        .map(|pane| pane.pane_id)
        .collect();
    named_by_the_header.sort();

    let (resumed, _inbox_tx) = build_resumed_server(&header, body);

    let mut screens: Vec<PaneId> = resumed.terminal_engine_by_pane_id.keys().copied().collect();
    screens.sort();
    let named_panes_and_unlisted_pane = {
        let mut pane_ids = named_by_the_header.clone();
        pane_ids.push(unlisted_pane_id);
        pane_ids.sort();
        pane_ids
    };
    assert_eq!(
        screens, named_panes_and_unlisted_pane,
        "every carried screen comes back"
    );

    let mut driven: Vec<PaneId> = resumed.pty_handle_by_pane_id.keys().copied().collect();
    driven.sort();
    assert_eq!(
        driven, named_by_the_header,
        "and only the panes the header named have something driving them"
    );

    let mut sized: Vec<PaneId> = resumed.pty_size_by_pane_id.keys().copied().collect();
    sized.sort();
    assert_eq!(sized, named_by_the_header, "and only they carry a size");
}

#[test]
fn a_body_whose_two_halves_are_swapped_is_corrupt_before_any_pane_is_touched() {
    // The header is what every build reads, whatever the body says. Bytes whose
    // header half is not a header at all name no pane, so the read fails and no
    // descriptor and no process id reaches the caller.
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("swapped.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&resume_file_path).expect("read the file"))
            .expect("valid json");
    let swapped = serde_json::json!({
        "header": on_disk["raw_body"].take(),
        "raw_body": on_disk["header"].take(),
    });
    std::fs::write(
        &resume_file_path,
        serde_json::to_vec(&swapped).expect("encode"),
    )
    .expect("rewrite the file");

    match read_resume_header(&resume_file_path) {
        // The position the decoder names counts bytes into a file whose size
        // follows the carried screens, so the sentence is read up to it.
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail.split(" at line ").next(),
            Some(
                format!(
                    "resume state at {} is unreadable: missing field `resume_format`",
                    resume_file_path.display()
                )
                .as_str()
            )
        ),
        other => panic!("expected a corrupt header, got {other:?}"),
    }
}

#[test]
fn a_carried_session_with_its_client_comes_back_whole() {
    // The body carries every attached client: a carried pane written by this build
    // reads back with the identity it went out with, wherever it connected
    // from.
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
            TEST_VIEWPORT_SIZE,
            None,
            tab_id,
            origin,
            "C-swift-otter".to_string(),
            3,
        ));
        let header = ResumeHeader {
            resume_format: RESUME_FORMAT,
            session_id,
            session_name: "carried".to_string(),
            carried_panes: Vec::new(),
        };
        let resume_body = ResumeBody {
            session_by_id: HashMap::from([(session_id, session)]),
            terminal_state_by_pane_id: HashMap::new(),
            undecoded_bytes_by_pane_id: HashMap::new(),
            graphics_undecoded_bytes_by_pane_id: HashMap::new(),
            graphics_screen_continuation_by_pane_id: HashMap::new(),
            graphics_screen_wrapper_active_by_pane_id: HashMap::new(),
            graphics_tmux_continuation_by_pane_id: HashMap::new(),
            graphics_tmux_wrapper_active_by_pane_id: HashMap::new(),
            graphics_events_by_pane_id: HashMap::new(),
            graphics_transport_by_pane_id: HashMap::new(),
            synchronized_output_by_pane_id: HashMap::new(),
            carried_quit: None,
        };
        write_resume_file(&resume_file_path, &header, &resume_body).expect("write the resume file");

        let (read_back, raw_body) =
            read_resume_header(&resume_file_path).expect("read the header back");

        // Format 4 writes the origin and no authority key. The header's format
        // number and the client carried pane's shape move together.
        let encoded_json: serde_json::Value =
            serde_json::from_str(raw_body.get()).expect("the body is json");
        let client_record_json = &encoded_json["session_by_id"][session_id.get_uuid().to_string()]
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
            read_resume_body(read_back.resume_format, &raw_body).expect("read the body back");

        assert_eq!(read_back.resume_format, RESUME_FORMAT);
        assert_eq!(RESUME_FORMAT, 4);
        let resumed_session = &read_body.session_by_id[&session_id];
        assert_eq!(resumed_session.session_id, session_id);
        let client = resumed_session
            .clients
            .get_client_by_id(client_id)
            .expect("the carried client");
        assert_eq!(client.get_client_id(), client_id);
        assert_eq!(client.get_origin(), origin);
        assert_eq!(client.get_label(), "C-swift-otter");
        assert_eq!(client.get_color(), 3);
        assert_eq!(client.get_active_tab(), tab_id);
    }
}

#[test]
fn a_resume_format_before_current_baseline_is_rejected() {
    let retired_resume_format = RESUME_FORMAT - 1;

    match read_resume_body(retired_resume_format, serde_json::value::RawValue::NULL) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail,
            format!(
                "resume body format {retired_resume_format} is outside the {RESUME_FORMAT_MIN} to {RESUME_FORMAT} range this build reads"
            )
        ),
        other => panic!("expected a retired format to be refused, got {other:?}"),
    }
}

#[test]
fn a_carried_pane_reports_the_size_its_rows_and_cols_name() {
    let pane = CarriedPane {
        pane_id: PaneId::new(),
        process_id: 4242,
        row_count: 20,
        column_count: 78,
        terminal_fd: Some(9),
        terminal_name: Some("/dev/ttys009".to_string()),
        exit_status: None,
    };

    assert_eq!(
        pane.get_pty_size(),
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
        let header = build_resume_header(SessionId::new(), Vec::new());
        let resume_body = build_resume_body_with_quit(Some(quit_kind));
        write_resume_file(&resume_file_path, &header, &resume_body).expect("write the resume file");

        let (read_back, raw_body) =
            read_resume_header(&resume_file_path).expect("read the header back");
        let read_body =
            read_resume_body(read_back.resume_format, &raw_body).expect("read the body back");

        assert_eq!(read_body.carried_quit, Some(quit_kind));
    }
}

#[test]
fn a_body_written_without_a_quit_reads_back_with_none() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = build_carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(&panes)
        .expect("a session to carry");
    write_resume_file(&resume_file_path, &header, &body).expect("write the resume file");

    // The body with its quit key taken out of the JSON.
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&resume_file_path).expect("read the file"))
            .expect("valid json");
    on_disk["raw_body"]
        .as_object_mut()
        .expect("the body is a map")
        .remove("carried_quit");
    std::fs::write(
        &resume_file_path,
        serde_json::to_vec(&on_disk).expect("encode"),
    )
    .expect("rewrite the file");

    let (read_back, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    let read_body =
        read_resume_body(read_back.resume_format, &raw_body).expect("read the body back");

    assert_eq!(read_body.carried_quit, None);
    assert_eq!(
        read_body.terminal_state_by_pane_id.len(),
        4,
        "every screen still reads back"
    );
}

#[test]
fn a_pane_whose_child_was_reaped_carries_that_exit_status_across_the_file() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("reaped.resume");
    let header = build_resume_header(
        SessionId::new(),
        vec![
            CarriedPane {
                pane_id: PaneId::new(),
                process_id: 4242,
                row_count: 20,
                column_count: 78,
                terminal_fd: Some(9),
                terminal_name: Some("/dev/ttys009".to_string()),
                exit_status: Some(ExitStatus::ExitCode(3)),
            },
            CarriedPane {
                pane_id: PaneId::new(),
                process_id: 4243,
                row_count: 20,
                column_count: 78,
                terminal_fd: Some(10),
                terminal_name: Some("/dev/ttys010".to_string()),
                exit_status: Some(ExitStatus::Signaled(9)),
            },
        ],
    );
    write_resume_file(
        &resume_file_path,
        &header,
        &build_resume_body_with_quit(None),
    )
    .expect("write the resume file");

    let (read_back, _raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");

    assert_eq!(read_back, header);
    assert_eq!(
        read_back
            .carried_panes
            .iter()
            .map(|pane| pane.exit_status)
            .collect::<Vec<Option<ExitStatus>>>(),
        vec![Some(ExitStatus::ExitCode(3)), Some(ExitStatus::Signaled(9))]
    );
}

#[test]
fn writing_a_resume_file_replaces_the_bytes_already_there() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    std::fs::write(&resume_file_path, b"the bytes of an older write").expect("write the old file");
    let header = build_resume_header(
        SessionId::new(),
        vec![CarriedPane {
            pane_id: PaneId::new(),
            process_id: 4242,
            row_count: 20,
            column_count: 78,
            terminal_fd: Some(9),
            terminal_name: Some("/dev/ttys009".to_string()),
            exit_status: None,
        }],
    );

    write_resume_file(
        &resume_file_path,
        &header,
        &build_resume_body_with_quit(None),
    )
    .expect("write the resume file");

    let (read_back, raw_body) =
        read_resume_header(&resume_file_path).expect("read the header back");
    assert_eq!(read_back, header);
    let read_body =
        read_resume_body(read_back.resume_format, &raw_body).expect("read the body back");
    assert_eq!(read_body.session_by_id.len(), 0);
    assert_eq!(read_body.terminal_state_by_pane_id.len(), 0);
}

#[test]
fn writing_into_a_directory_that_is_not_there_is_an_io_failure_naming_it() {
    let resume_test_directory = TempDir::new().expect("create temp dir");
    let missing_parent_directory = resume_test_directory.path().join("gone");
    let resume_file_path = missing_parent_directory.join("session.resume");
    let header = build_resume_header(SessionId::new(), Vec::new());

    match write_resume_file(
        &resume_file_path,
        &header,
        &build_resume_body_with_quit(None),
    ) {
        Err(StorageError::Io { detail }) => assert!(
            detail.starts_with(&format!(
                "create temp in {}: ",
                missing_parent_directory.display()
            )),
            "the failure must name the directory, got {detail}"
        ),
        other => panic!("expected an io failure, got {other:?}"),
    }
    assert!(
        !missing_parent_directory.exists(),
        "and the directory must not be created"
    );
}
