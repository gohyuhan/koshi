//! Tests for the state a session server carries across a process-image swap:
//! a populated server drained into a resume file and rebuilt from it, what the
//! drain leaves behind, a sequence the swap cut in half finishing in the next
//! image, what the header still yields when the body cannot be read, what a
//! body format this build does not know is answered with, and what a body
//! written in the older client format reads back as.

use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, FocusPaneArgs, FocusTarget, GridPos,
    NewPaneArgs, NewTabArgs, Selection, SelectionKind,
};
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, TabId};
use koshi_core::process::PtySize;
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_pty::portable::CarriedPtyPane;
use koshi_session::client::{Client, ClientOrigin, ClientRegistry};
use koshi_terminal::grid::state::Cell;
use koshi_test_support::fake_pty::FakePtyBackend;
use tempfile::TempDir;

use super::*;
use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

/// The viewport of the client the session is bootstrapped with.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// The viewport of the second client, sized apart from [`VIEWPORT`] so the two
/// clients are told apart by what they hold.
const SECOND_VIEWPORT: Size = Size {
    cols: 100,
    rows: 30,
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

/// The top row of a screen as text; a blank cell reads as a space.
fn first_row(state: &TerminalState) -> String {
    let (_, cols) = state.active_grid().dimensions();
    (0..cols)
        .map(|col| state.active_grid().cell(0, col).map_or(' ', Cell::ch))
        .collect()
}

/// Run `command` as a keybinding of `client`, and panic unless it was applied.
fn apply(server: &mut Server, client: ClientId, command: Command) {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::UNIX_EPOCH,
        command,
    );
    let command_id = envelope.id;
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
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let mut server = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        inbox_tx.clone(),
    );

    let session_id = SessionId::new();
    let first_client = server
        .bootstrap_local_named(
            session_id,
            "carried".to_string(),
            VIEWPORT,
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap the session");
    let first_tab = *server.sessions[&session_id]
        .tabs
        .keys()
        .next()
        .expect("the bootstrapped tab");

    // Two splits on the first tab: rightward, then downward inside the pane the
    // first split created, so the tree holds a split inside a split.
    for direction in [Direction::Right, Direction::Down] {
        apply(
            &mut server,
            first_client,
            Command::NewPane(NewPaneArgs {
                source: None,
                tab: None,
                direction,
                stacked: false,
                cwd: None,
                command: None,
                client: None,
            }),
        );
    }

    // A second tab, which moves the first client onto it and gives the session
    // its fourth pane.
    apply(
        &mut server,
        first_client,
        Command::NewTab(NewTabArgs::default()),
    );
    let second_tab = *server.sessions[&session_id]
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
        SECOND_VIEWPORT,
        None,
        first_tab,
        SystemTime::UNIX_EPOCH,
        false,
    );

    let panes = pane_ids(&server, session_id, first_tab);
    // The second client focuses the last pane of the first tab, so the two
    // clients hold different focus as well as different tabs.
    apply(
        &mut server,
        second_client,
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(panes[2]),
            client: None,
        }),
    );
    let session = server.sessions.get_mut(&session_id).expect("the session");
    let first = session
        .clients
        .get_mut(first_client)
        .expect("the first client");
    first.zoom_pane(first_tab, panes[0]);
    first.set_scroll_offset(panes[1], 7);
    let second = session
        .clients
        .get_mut(second_client)
        .expect("the second client");
    second.zoom_pane(first_tab, panes[2]);
    second.set_scroll_offset(panes[0], 12);
    second.set_selection(
        panes[1],
        Selection {
            kind: SelectionKind::Word,
            anchor: GridPos { row: 3, col: 4 },
            cursor: GridPos { row: 3, col: 9 },
        },
    );

    // Distinct output per pane, so a screen that came back under the wrong pane
    // is caught.
    for (index, pane) in live_panes(&server, session_id).into_iter().enumerate() {
        server.handle_pty_output(pane, format!("pane {index} output").as_bytes());
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

/// The panes of `tab`, in layout order.
fn pane_ids(server: &Server, session_id: SessionId, tab: TabId) -> Vec<PaneId> {
    server.sessions[&session_id].tabs[&tab]
        .layout()
        .leaf_panes()
}

/// Every pane of `session_id`, in tab order then layout order.
fn live_panes(server: &Server, session_id: SessionId) -> Vec<PaneId> {
    server.sessions[&session_id]
        .tabs
        .values()
        .flat_map(|tab| tab.layout().leaf_panes())
        .collect()
}

/// One carried record per live pane, as the concrete PTY backend reports them:
/// a made-up process id and descriptor per pane, and a size the server's own
/// record overrides.
fn carried_pty_panes(server: &Server, session_id: SessionId) -> Vec<CarriedPtyPane> {
    live_panes(server, session_id)
        .into_iter()
        .enumerate()
        .map(|(index, pane_id)| CarriedPtyPane {
            pane_id,
            #[cfg(unix)]
            terminal_fd: Some(20 + index as i32),
            pid: 5000 + index as u32,
            size: PtySize { cols: 1, rows: 1 },
            exit: None,
        })
        .collect()
}

/// Rebuild a server from `body`, over detached handles for every pane the
/// header names and the sizes that header carries.
fn resumed_server(header: &ResumeHeader, body: ResumeBody) -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let handles: HashMap<PaneId, PtyHandle> = header
        .panes
        .iter()
        .map(|pane| (pane.pane_id, PtyHandle::detached(pane.pane_id)))
        .collect();
    let sizes: HashMap<PaneId, PtySize> = header
        .panes
        .iter()
        .map(|pane| {
            (
                pane.pane_id,
                PtySize {
                    cols: pane.cols,
                    rows: pane.rows,
                },
            )
        })
        .collect();
    let server = Server::resume(
        pty_backend,
        inbox_rx,
        inbox_tx.clone(),
        body,
        handles,
        sizes,
    );
    (server, inbox_tx)
}

#[test]
fn a_carried_session_reads_back_with_every_tab_pane_client_and_screen() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = carried_pty_panes(&populated.server, session_id);
    let expected_tabs = populated.server.sessions[&session_id].tabs.clone();
    let expected_records = populated.server.sessions[&session_id].panes.clone();
    let expected_sizes = populated.server.pty_sizes.clone();

    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);
    write(&path, &header, &body).expect("write the resume file");
    let (read_header, raw_body) = read_header(&path).expect("read the header back");
    let read_body = read_body(read_header.format, &raw_body).expect("read the body back");
    let (resumed, _inbox_tx) = resumed_server(&read_header, read_body);

    assert_eq!(read_header, header, "the header must read back unchanged");
    assert_eq!(read_header.session_id, session_id);
    assert_eq!(read_header.session_name, "carried");
    assert_eq!(resumed.sessions.len(), 1, "one session must come back");
    let session = &resumed.sessions[&session_id];
    assert_eq!(session.name, "carried");
    assert_eq!(session.tabs, expected_tabs, "every tab and its layout tree");
    assert_eq!(session.panes, expected_records, "every pane record");
    assert_eq!(session.clients.len(), 2, "both clients must come back");

    let first = session
        .clients
        .get(populated.first_client)
        .expect("the first client");
    let first_tab_panes = expected_tabs[&populated.first_tab].layout().leaf_panes();
    assert_eq!(first.active_tab(), populated.second_tab);
    assert_eq!(first.viewport(), VIEWPORT);
    assert_eq!(
        first.zoomed_pane(populated.first_tab),
        Some(first_tab_panes[0])
    );
    assert_eq!(first.scroll_offset(first_tab_panes[1]), 7);
    assert_eq!(first.selection(first_tab_panes[1]), None);

    let second = session
        .clients
        .get(populated.second_client)
        .expect("the second client");
    assert_eq!(second.active_tab(), populated.first_tab);
    assert_eq!(second.viewport(), SECOND_VIEWPORT);
    assert_eq!(
        second.focused_pane(populated.first_tab),
        Some(first_tab_panes[2])
    );
    assert_eq!(
        second.zoomed_pane(populated.first_tab),
        Some(first_tab_panes[2])
    );
    assert_eq!(second.scroll_offset(first_tab_panes[0]), 12);
    assert_eq!(
        second.selection(first_tab_panes[1]),
        Some(Selection {
            kind: SelectionKind::Word,
            anchor: GridPos { row: 3, col: 4 },
            cursor: GridPos { row: 3, col: 9 },
        })
    );

    assert_eq!(body.engines.len(), 4, "four panes must have a screen");
    for (pane_id, screen) in &body.engines {
        assert_eq!(
            resumed.terminal_engines[pane_id].state(),
            screen,
            "pane {pane_id} must come back with the screen it went out with"
        );
    }
    // Every pane was fed its own text, so a screen that came back under the
    // wrong pane reads the wrong line here.
    for (index, pane) in panes.iter().enumerate() {
        assert_eq!(
            first_row(resumed.terminal_engines[&pane.pane_id].state()).trim_end(),
            format!("pane {index} output")
        );
    }
    assert_eq!(resumed.pty_sizes, expected_sizes, "every pane's size");
    let mut resumed_handles: Vec<PaneId> = resumed.pty_handles.keys().copied().collect();
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
    let panes = carried_pty_panes(&populated.server, session_id);

    let (_header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);

    assert_eq!(
        populated.server.terminal_engines.len(),
        0,
        "every engine must have moved out"
    );
    assert_eq!(
        populated.server.sessions.len(),
        0,
        "every session must have moved out"
    );
    assert_eq!(body.engines.len(), 4, "every engine must be in the body");
    assert_eq!(body.sessions.len(), 1, "the session must be in the body");
    assert_eq!(
        body.undecoded.len(),
        0,
        "no pane's parser was mid-sequence, so nothing is held"
    );
}

#[test]
fn a_report_the_swap_cut_in_half_finishes_in_the_next_image() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let pane = live_panes(&populated.server, session_id)[0];
    let panes = carried_pty_panes(&populated.server, session_id);

    // The shell reports /Users/yuhan/Projects/koshi through OSC 7, and the last
    // chunk before the swap ends after `/Proj`.
    populated
        .server
        .handle_pty_output(pane, b"\x1b]7;file://host/Users/yuhan/Proj");

    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);
    assert_eq!(
        body.undecoded,
        HashMap::from([(pane, b"\x1b]7;file://host/Users/yuhan/Proj".to_vec())]),
        "only the pane mid-report holds bytes, and it holds all of them"
    );
    write(&path, &header, &body).expect("write the resume file");
    let (read_header, raw_body) = read_header(&path).expect("read the header back");
    let read_body = read_body(read_header.format, &raw_body).expect("read the body back");
    let (mut resumed, _inbox_tx) = resumed_server(&read_header, read_body);

    assert_eq!(
        resumed.terminal_engines[&pane].state().current_cwd(),
        None,
        "a report with no terminator sets no directory"
    );
    let before = first_row(resumed.terminal_engines[&pane].state());

    resumed.handle_pty_output(pane, b"ects/koshi\x07");

    let state = resumed.terminal_engines[&pane].state();
    let cwd = state.current_cwd().expect("the report finished");
    assert_eq!(cwd.host(), Some("host"));
    assert_eq!(cwd.path(), Path::new("/Users/yuhan/Projects/koshi"));
    assert_eq!(
        first_row(state),
        before,
        "the rest of the report joined the sequence instead of printing"
    );
}

#[test]
fn a_body_written_without_the_held_bytes_reads_back_with_none() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);
    write(&path, &header, &body).expect("write the resume file");

    // The body with its map of held bytes taken out of the JSON.
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read the file")).expect("valid json");
    on_disk["body"]
        .as_object_mut()
        .expect("the body is a map")
        .remove("undecoded");
    std::fs::write(&path, serde_json::to_vec(&on_disk).expect("encode")).expect("rewrite the file");

    let (read_header, raw_body) = read_header(&path).expect("read the header back");
    let read_body = read_body(read_header.format, &raw_body).expect("read the body back");

    assert_eq!(read_body.engines.len(), 4, "every screen still reads back");
    assert_eq!(read_body.undecoded, HashMap::new());
}

#[test]
fn the_header_names_every_pane_with_the_size_the_server_holds_for_it() {
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = carried_pty_panes(&populated.server, session_id);
    let sizes = populated.server.pty_sizes.clone();

    let (header, _body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);

    assert_eq!(header.format, RESUME_FORMAT);
    assert_eq!(header.panes.len(), 4, "one record per live pane");
    for (index, record) in header.panes.iter().enumerate() {
        assert_eq!(record.pane_id, panes[index].pane_id);
        assert_eq!(record.pid, 5000 + index as u32);
        #[cfg(unix)]
        assert_eq!(record.terminal_fd, Some(20 + index as i32));
        #[cfg(windows)]
        assert_eq!(record.terminal_fd, None);
        let held = sizes[&record.pane_id];
        assert_eq!(
            (record.cols, record.rows),
            (held.cols, held.rows),
            "the header carries the size the server holds, not the backend's"
        );
    }
}

#[test]
fn a_pane_the_server_holds_no_size_for_takes_the_size_the_backend_reports() {
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = carried_pty_panes(&populated.server, session_id);
    let forgotten = panes[2].pane_id;
    populated.server.pty_sizes.remove(&forgotten);

    let (header, _body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);

    let record = header
        .panes
        .iter()
        .find(|record| record.pane_id == forgotten)
        .expect("the pane the server forgot");
    assert_eq!((record.cols, record.rows), (1, 1));
}

#[test]
fn an_unreadable_body_still_leaves_every_pane_descriptor_and_process_id() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);
    write(&path, &header, &body).expect("write the resume file");

    // Only the body is broken; the header on disk is untouched.
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read the file")).expect("valid json");
    on_disk["body"] = serde_json::json!({ "sessions": "not-a-map" });
    std::fs::write(&path, serde_json::to_vec(&on_disk).expect("encode")).expect("rewrite the file");

    let (read_header, raw_body) = read_header(&path).expect("read the header back");
    assert_eq!(read_header, header, "the header survives a broken body");
    assert_eq!(read_header.panes.len(), 4);
    for (index, record) in read_header.panes.iter().enumerate() {
        assert_eq!(record.pid, 5000 + index as u32);
        #[cfg(unix)]
        assert_eq!(record.terminal_fd, Some(20 + index as i32));
        #[cfg(windows)]
        assert_eq!(record.terminal_fd, None);
    }
    match read_body(read_header.format, &raw_body) {
        Err(StorageError::Corrupt { detail }) => {
            assert!(
                detail.starts_with("resume body is unreadable: "),
                "the failure must say the body is unreadable, got {detail}"
            );
        }
        other => panic!("expected a corrupt body, got {other:?}"),
    }
}

#[test]
fn a_body_format_this_build_does_not_know_is_refused_by_both_numbers() {
    let too_new = RESUME_FORMAT + 1;

    match read_body(too_new, serde_json::value::RawValue::NULL) {
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
fn a_header_naming_an_unknown_format_still_reads_back_whole() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session.resume");
    let session_id = SessionId::new();
    let header = ResumeHeader {
        format: RESUME_FORMAT + 1,
        session_id,
        session_name: "from-a-newer-build".to_string(),
        panes: vec![CarriedPane {
            pane_id: PaneId::new(),
            pid: 4242,
            rows: 20,
            cols: 78,
            terminal_fd: Some(9),
            terminal_name: Some("/dev/ttys009".to_string()),
            exit: None,
        }],
    };
    let body = ResumeBody {
        sessions: HashMap::new(),
        engines: HashMap::new(),
        undecoded: HashMap::new(),
        quit: None,
    };
    write(&path, &header, &body).expect("write the resume file");

    let (read, _raw_body) = read_header(&path).expect("read the header back");

    assert_eq!(read, header, "any build reads the header of any other");
}

#[test]
fn a_header_written_without_a_terminal_name_reads_back_with_none() {
    // A build that records no terminal name writes a pane record without that
    // field. This build must still read that record, and read the pane back
    // with no name rather than refusing the whole header.
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session.resume");
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let written = serde_json::json!({
        "header": {
            "format": RESUME_FORMAT,
            "session_id": session_id,
            "session_name": "from-a-build-without-the-name",
            "panes": [{
                "pane_id": pane_id,
                "pid": 4242,
                "rows": 20,
                "cols": 78,
                "terminal_fd": 9,
            }],
        },
        "body": { "sessions": {}, "engines": {} },
    });
    std::fs::write(&path, serde_json::to_vec(&written).expect("encode"))
        .expect("write the resume file");

    let (read, _raw_body) = read_header(&path).expect("read the header back");

    assert_eq!(
        read,
        ResumeHeader {
            format: RESUME_FORMAT,
            session_id,
            session_name: "from-a-build-without-the-name".to_string(),
            panes: vec![CarriedPane {
                pane_id,
                pid: 4242,
                rows: 20,
                cols: 78,
                terminal_fd: Some(9),
                terminal_name: None,
                exit: None,
            }],
        }
    );
}

#[test]
fn a_resumed_server_starts_with_no_socket_and_no_shutdown_pending() {
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);

    let (resumed, _inbox_tx) = resumed_server(&header, body);

    assert!(!resumed.quit_requested, "no quit is pending");
    assert!(!resumed.draining, "teardown has not begun");
    assert!(!resumed.immediate_shutdown, "no zero-grace quit is pending");
    assert!(
        resumed.ipc_server().is_none(),
        "the control socket is bound after the swap, not carried through it"
    );
    assert_eq!(resumed.subscriptions.len(), 0, "no subscriber is carried");
}

#[test]
fn reading_a_resume_file_that_is_not_there_is_an_io_failure() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("missing.resume");

    match read_header(&path) {
        Err(StorageError::Io { detail }) => {
            assert!(
                detail.starts_with(&format!("read resume state at {}: ", path.display())),
                "the failure must name the path, got {detail}"
            );
        }
        other => panic!("expected an io failure, got {other:?}"),
    }
}

#[test]
fn reading_bytes_that_are_not_a_resume_file_is_a_corrupt_failure() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("junk.resume");
    std::fs::write(&path, b"not json").expect("write junk");

    match read_header(&path) {
        Err(StorageError::Corrupt { detail }) => {
            assert!(
                detail.starts_with(&format!(
                    "resume state at {} is unreadable: ",
                    path.display()
                )),
                "the failure must name the path, got {detail}"
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

    match read_body(too_old, serde_json::value::RawValue::NULL) {
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
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);
    write(&path, &header, &body).expect("write the resume file");

    let whole = std::fs::read(&path).expect("read the file back");
    let cut = whole.len() / 2;
    assert!(cut > 0, "the file must have bytes to cut");
    std::fs::write(&path, &whole[..cut]).expect("rewrite the file cut short");

    match read_header(&path) {
        Err(StorageError::Corrupt { detail }) => {
            assert!(
                detail.starts_with(&format!(
                    "resume state at {} is unreadable: ",
                    path.display()
                )),
                "the failure must name the path, got {detail}"
            );
        }
        other => panic!("expected a corrupt failure, got {other:?}"),
    }
}

#[test]
fn a_body_missing_one_of_its_two_halves_is_corrupt_while_the_header_still_reads() {
    // The body is one JSON object with two named halves. A body holding only
    // the sessions is readable JSON, so nothing before the decode catches it —
    // the decode itself must, and it must cost the caller no pane record.
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);
    write(&path, &header, &body).expect("write the resume file");

    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read the file")).expect("valid json");
    on_disk["body"]
        .as_object_mut()
        .expect("a body object")
        .remove("engines");
    std::fs::write(&path, serde_json::to_vec(&on_disk).expect("encode")).expect("rewrite the file");

    let (read_back, raw_body) = read_header(&path).expect("read the header back");
    assert_eq!(read_back, header, "the header survives a half body");
    assert_eq!(read_back.panes.len(), 4, "with every pane it named");

    match read_body(read_back.format, &raw_body) {
        Err(StorageError::Corrupt { detail }) => {
            assert_eq!(
                detail,
                "resume body is unreadable: missing field `engines` at line 1 column \
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
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("empty.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;

    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &[]);
    write(&path, &header, &body).expect("write the resume file");
    let (read_back, raw_body) = read_header(&path).expect("read the header back");
    let read_body = read_body(read_back.format, &raw_body).expect("read the body back");

    assert_eq!(read_back.panes, Vec::new(), "no pane crosses the swap");
    assert_eq!(read_back.format, RESUME_FORMAT);
    assert_eq!(read_back.session_id, session_id);
    assert_eq!(read_back.session_name, "carried");
    assert_eq!(
        read_body.engines.len(),
        4,
        "the screens still cross, since the header names what the backend holds"
    );

    let (resumed, _inbox_tx) = resumed_server(&read_back, read_body);
    assert_eq!(resumed.pty_handles.len(), 0, "and no handle comes back");
    assert_eq!(resumed.pty_sizes.len(), 0, "and no size comes back");
    assert_eq!(resumed.sessions.len(), 1, "the session itself still does");
}

#[test]
fn a_session_holding_many_panes_carries_every_one_of_them_in_order() {
    // Nothing in the file caps how many panes it names. Each record must keep
    // its own descriptor, process id and size, and keep the order the backend
    // reported, so the image that reads it takes back the right terminal for
    // each pane.
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("many.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;

    let many: Vec<CarriedPtyPane> = (0..64)
        .map(|index| CarriedPtyPane {
            pane_id: PaneId::new(),
            #[cfg(unix)]
            terminal_fd: Some(100 + index),
            pid: 7000 + index as u32,
            size: PtySize {
                cols: 40 + index as u16,
                rows: 10 + index as u16,
            },
            exit: None,
        })
        .collect();

    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &many);
    write(&path, &header, &body).expect("write the resume file");
    let (read_back, _raw_body) = read_header(&path).expect("read the header back");

    assert_eq!(read_back.panes.len(), 64, "every pane must have a record");
    for (index, record) in read_back.panes.iter().enumerate() {
        assert_eq!(record.pane_id, many[index].pane_id, "record {index}");
        assert_eq!(record.pid, 7000 + index as u32, "record {index}");
        #[cfg(unix)]
        assert_eq!(
            record.terminal_fd,
            Some(100 + index as i32),
            "record {index}"
        );
        #[cfg(windows)]
        assert_eq!(record.terminal_fd, None, "record {index}");
        // None of these panes is one the server holds a size for, so each takes
        // the size the backend reported.
        assert_eq!(
            (record.cols, record.rows),
            (40 + index as u16, 10 + index as u16),
            "record {index}"
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
    let panes = carried_pty_panes(&populated.server, session_id);
    let (header, mut body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);
    let stray = PaneId::new();
    body.undecoded.insert(stray, b"\x1b[".to_vec());
    let mut carried_screens: Vec<PaneId> = body.engines.keys().copied().collect();
    carried_screens.sort();

    let (resumed, _inbox_tx) = resumed_server(&header, body);

    let mut rebuilt: Vec<PaneId> = resumed.terminal_engines.keys().copied().collect();
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
    let panes = carried_pty_panes(&populated.server, session_id);
    let extra = PaneId::new();
    populated.server.terminal_engines.insert(
        extra,
        koshi_terminal::engine::TerminalEngine::new(PtySize { cols: 80, rows: 24 }),
    );
    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);

    let mut named_by_the_header: Vec<PaneId> =
        header.panes.iter().map(|pane| pane.pane_id).collect();
    named_by_the_header.sort();

    let (resumed, _inbox_tx) = resumed_server(&header, body);

    let mut screens: Vec<PaneId> = resumed.terminal_engines.keys().copied().collect();
    screens.sort();
    let mut with_the_extra = named_by_the_header.clone();
    with_the_extra.push(extra);
    with_the_extra.sort();
    assert_eq!(screens, with_the_extra, "every carried screen comes back");

    let mut driven: Vec<PaneId> = resumed.pty_handles.keys().copied().collect();
    driven.sort();
    assert_eq!(
        driven, named_by_the_header,
        "and only the panes the header named have something driving them"
    );

    let mut sized: Vec<PaneId> = resumed.pty_sizes.keys().copied().collect();
    sized.sort();
    assert_eq!(sized, named_by_the_header, "and only they carry a size");
}

#[test]
fn a_body_whose_two_halves_are_swapped_is_corrupt_before_any_pane_is_touched() {
    // The header is what every build reads, whatever the body says. Bytes whose
    // header half is not a header at all name no pane, so the read fails and no
    // descriptor and no process id reaches the caller.
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("swapped.resume");
    let mut populated = populated_server();
    let session_id = populated.session_id;
    let panes = carried_pty_panes(&populated.server, session_id);
    let (header, body) = populated
        .server
        .carry_out(session_id, "carried".to_string(), &panes);
    write(&path, &header, &body).expect("write the resume file");
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read the file")).expect("valid json");
    let swapped = serde_json::json!({
        "header": on_disk["body"].take(),
        "body": on_disk["header"].take(),
    });
    std::fs::write(&path, serde_json::to_vec(&swapped).expect("encode")).expect("rewrite the file");

    match read_header(&path) {
        // The position the decoder names counts bytes into a file whose size
        // follows the carried screens, so the sentence is read up to it.
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail.split(" at line ").next(),
            Some(
                format!(
                    "resume state at {} is unreadable: missing field `format`",
                    path.display()
                )
                .as_str()
            )
        ),
        other => panic!("expected a corrupt header, got {other:?}"),
    }
}

#[test]
fn a_carried_session_with_its_client_comes_back_whole() {
    // The body carries every attached client: a record written by this build
    // reads back with the identity it went out with, wherever it connected
    // from.
    for (origin, written_origin) in [
        (ClientOrigin::Local, "Local"),
        (ClientOrigin::Remote, "Remote"),
    ] {
        let dir = TempDir::new().expect("create temp dir");
        let path = dir.path().join("client.resume");
        let session_id = SessionId::new();
        let client_id = ClientId::new();
        let tab_id = TabId::new();
        let mut session = Session::new(
            session_id,
            "carried".to_string(),
            SystemTime::UNIX_EPOCH,
            ClientRegistry::new(),
        );
        session.attach_client(Client::new(
            client_id,
            session_id,
            SystemTime::UNIX_EPOCH,
            VIEWPORT,
            None,
            tab_id,
            origin,
            "C-swift-otter".to_string(),
            3,
        ));
        let header = ResumeHeader {
            format: RESUME_FORMAT,
            session_id,
            session_name: "carried".to_string(),
            panes: Vec::new(),
        };
        let body = ResumeBody {
            sessions: HashMap::from([(session_id, session)]),
            engines: HashMap::new(),
            undecoded: HashMap::new(),
            quit: None,
        };
        write(&path, &header, &body).expect("write the resume file");

        let (read_back, raw_body) = read_header(&path).expect("read the header back");

        // Format 2 writes the origin and no authority key. The header's format
        // number and the client record's shape move together.
        let encoded: serde_json::Value =
            serde_json::from_str(raw_body.get()).expect("the body is json");
        let record = &encoded["sessions"][session_id.as_uuid().to_string()]["clients"]["records"]
            [client_id.as_uuid().to_string()];
        assert_eq!(
            record["origin"],
            serde_json::Value::String(written_origin.to_string())
        );
        assert_eq!(
            record.get("tier"),
            None,
            "format {RESUME_FORMAT} writes no authority key"
        );

        let read_body = read_body(read_back.format, &raw_body).expect("read the body back");

        assert_eq!(read_back.format, RESUME_FORMAT);
        assert_eq!(RESUME_FORMAT, 2);
        let carried = &read_body.sessions[&session_id];
        assert_eq!(carried.id, session_id);
        let client = carried.clients.get(client_id).expect("the carried client");
        assert_eq!(client.id(), client_id);
        assert_eq!(client.origin(), origin);
        assert_eq!(client.label(), "C-swift-otter");
        assert_eq!(client.colour(), 3);
        assert_eq!(client.active_tab(), tab_id);
    }
}

#[test]
fn a_carried_file_written_before_this_change_still_reads() {
    // A format 1 body carries a `tier` key on every client. This build reads
    // that body and takes the client back with the identity it names.
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let mut clients = serde_json::Map::new();
    clients.insert(
        client_id.as_uuid().to_string(),
        serde_json::json!({
            "id": client_id,
            "session_id": session_id,
            "attached_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "viewport": { "cols": 80, "rows": 24 },
            "active_tab": tab_id,
            "origin": "Local",
            "label": "C-swift-otter",
            "colour": 3,
            "tier": "Admin",
            "focus_by_tab": {},
            "lock_mode": "Normal",
            "mouse_select": false,
            "scroll_by_pane": {},
            "selection_by_pane": {},
            "zoom_by_tab": {},
        }),
    );
    let mut sessions = serde_json::Map::new();
    sessions.insert(
        session_id.as_uuid().to_string(),
        serde_json::json!({
            "id": session_id,
            "name": "carried",
            "created_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "tabs": {},
            "panes": { "records": {} },
            "clients": { "records": clients },
            "config_snapshot": null,
            "lifecycle": "Starting",
        }),
    );
    let written = serde_json::json!({ "sessions": sessions, "engines": {} });
    let raw = serde_json::value::RawValue::from_string(written.to_string())
        .expect("the body is one json value");

    assert_eq!(RESUME_FORMAT_MIN, 1);
    let body = read_body(1, &raw).expect("a body written before this change reads back");

    let carried = &body.sessions[&session_id];
    assert_eq!(carried.id, session_id);
    let client = carried.clients.get(client_id).expect("the carried client");
    assert_eq!(client.id(), client_id);
    assert_eq!(client.origin(), ClientOrigin::Local);
    assert_eq!(client.label(), "C-swift-otter");
    assert_eq!(client.colour(), 3);
    assert_eq!(client.active_tab(), tab_id);
    assert_eq!(client.viewport(), Size { cols: 80, rows: 24 });
}
