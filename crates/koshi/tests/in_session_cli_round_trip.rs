//! What a `koshi` verb typed inside a session does, from the words on the
//! command line to the number the process exits with.
//!
//! Each test walks one verb the whole way: `Cli::try_parse_from` reads the real argv,
//! [`CliCommand::build_action_command`](koshi::cli::CliCommand::build_action_command) maps it to
//! the core command, that command crosses a real control socket inside a [`CommandEnvelope`], the
//! dispatcher applies it, the attached client is told what changed, and the answer becomes the exit
//! code the binary reports.
//!
//! Each test starts its own session through
//! `common::in_process_session::RunningSession::start_session`: a session
//! server on a thread of this process, a real control socket in a fresh
//! temporary runtime directory under a short base path, and a fake PTY backend
//! in place of the panes' real children. The backend records every spawn,
//! resize and kill — a recorded [`KillPolicy`], a recorded [`PtySize`] — and
//! launches no process.
//!
//! Each attached client has a reader thread that forwards what it reads into a
//! queue this thread reads with a deadline. A session that never reports the
//! change fails the test at that deadline. Dropping the session stops its
//! serving thread.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use koshi::cli::{Cli, ResolvedTargets};
use koshi_core::command::{CliExitCode, Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::event::{
    Event, InputModeChanged, LayoutChanged, PaneClosing, PaneCreated, PaneFocused, PaneRemoved,
    PtyResized, RejectReason,
};
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::process::{KillPolicy, PtySize};
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{IpcRequest, IpcRequestKind, IpcResponse, IpcResult};
use koshi_link::error::CliError;
use koshi_test_support::fixtures::build_test_runtime_directory;

use common::in_process_session::{
    attach_test_client, list_emitted_events, AttachedClient, RunningSession,
};
use common::session_connection::open_session_connection;
use common::WAIT_DURATION;

/// How long a poll pauses between attempts.
const CLI_ROUND_TRIP_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(10);

/// The next `event_count` frames the attached client is told about the session's
/// structure, in arrival order. Fails the test once [`WAIT_DURATION`] has passed
/// with fewer than `event_count` of them.
fn receive_session_events(client: &AttachedClient, event_count: usize) -> Vec<SessionEvent> {
    (0..event_count)
        .map(|_| {
            client
                .session_events
                .recv_timeout(WAIT_DURATION)
                .expect("the session reports the change")
        })
        .collect()
}

/// Run one `koshi` invocation typed inside `pane_id` by `client`, and hand back
/// what the session answered and the exit code the binary reports for that
/// answer.
///
/// The steps are the binary's, in its order: parse the argv, map the
/// subcommand to its core command, submit the command over the session's
/// control socket, then turn the result into the process exit status.
fn run_cli_invocation(
    session: &RunningSession,
    client: &AttachedClient,
    pane_id: PaneId,
    argv: &[&str],
) -> (CommandResult, CliExitCode) {
    let parsed_cli = Cli::try_parse_from(argv).expect("the argv parses");
    let (_, action_command) = parsed_cli
        .command
        .as_ref()
        .expect("the argv carries a subcommand")
        .build_action_command(&ResolvedTargets::default(), Direction::Right)
        .expect("the subcommand is an action verb");

    let command_result = submit_session_command(session, client, pane_id, action_command);
    let exit_code = match report_command_result(&command_result) {
        Ok(()) => CliExitCode::Success,
        Err(command_error) => CliExitCode::from(&command_error),
    };
    (command_result, exit_code)
}

/// The same walk as [`run_cli_invocation`] for a verb typed OUTSIDE any pane: the real argv, the
/// core command [`CliCommand::build_action_command`](koshi::cli::CliCommand::build_action_command)
/// builds, and the client
/// [`CliCommand::get_source_client_id`](koshi::cli::CliCommand::get_source_client_id) reads off the
/// same parse, all the way to the session's answer and the exit code.
///
/// The command travels as [`CommandSource::ExternalCli`], the only source that
/// carries a target client.
fn run_external_cli_invocation(
    session: &RunningSession,
    argv: &[&str],
) -> (CommandResult, CliExitCode) {
    let parsed_cli = Cli::try_parse_from(argv).expect("the argv parses");
    let parsed_command = parsed_cli
        .command
        .as_ref()
        .expect("the argv carries a subcommand");
    let (_, action_command) = parsed_command
        .build_action_command(&ResolvedTargets::default(), Direction::Right)
        .expect("the subcommand is an action verb");

    let command_result = koshi_link::ipc_client::submit_external_command_via_runtime_directory(
        session.runtime_directory.path(),
        None,
        session.session_id,
        parsed_command.get_source_client_id(),
        action_command,
    )
    .expect("the session answers the command");
    let exit_code = match report_command_result(&command_result) {
        Ok(()) => CliExitCode::Success,
        Err(command_error) => CliExitCode::from(&command_error),
    };
    (command_result, exit_code)
}

/// Submit `command` to `session` over its control socket, enveloped the way the
/// CLI running inside `pane_id` envelopes it, and hand back the dispatcher's
/// result.
fn submit_session_command(
    session: &RunningSession,
    client: &AttachedClient,
    pane_id: PaneId,
    command: Command,
) -> CommandResult {
    let session_endpoint = session.load_session_endpoint();
    let mut connection = open_session_connection(&session_endpoint);
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_in_session_cli(
            session.session_id,
            Some(client.client_id),
            pane_id,
            PathBuf::from(session_endpoint.socket_address),
        ),
        command,
    );
    let submit_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
    };
    connection
        .send(&submit_request)
        .expect("the server reads the command");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the command");
    assert_eq!(ipc_response.request_id, Some(2));
    match ipc_response.answer_result {
        IpcResult::CommandResult(command_result) => command_result,
        unexpected_result => panic!("the command was answered with {unexpected_result:?}"),
    }
}

/// What the binary reports for a dispatched command: an applied command is a
/// success, and a rejected one is the [`CliError`] the exit-code table reads.
fn report_command_result(command_result: &CommandResult) -> Result<(), CliError> {
    match command_result {
        CommandResult::Ok { .. } => Ok(()),
        CommandResult::Rejected { reason, help, .. } => Err(CliError::CommandRejected {
            reason: *reason,
            help: help.clone(),
        }),
    }
}

/// The tab the session's only tab is, read from its own report.
fn get_only_tab_id(session: &RunningSession) -> TabId {
    let session_overview = session.fetch_session_overview();
    assert_eq!(session_overview.tabs.len(), 1);
    session_overview.tabs[0].tab_id
}

/// The size the pane's child was last given, which is the size the layout
/// solved for that pane.
fn get_last_pane_size(session: &RunningSession, pane_id: PaneId) -> PtySize {
    *session
        .fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("the pane was spawned")
        .last()
        .expect("the spawn recorded the pane's first size")
}

/// The kills the backend recorded for `pane_id`, read again until one is
/// there. A closed pane's child is killed on a thread of its own, and the
/// record can land after the command is answered.
fn wait_for_pane_kill_policies(session: &RunningSession, pane_id: PaneId) -> Vec<KillPolicy> {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        let pane_kill_policies = session
            .fake_pty_backend
            .list_pane_kill_policies(pane_id)
            .expect("the pane was spawned");
        if !pane_kill_policies.is_empty() {
            return pane_kill_policies;
        }
        assert!(
            Instant::now() < deadline,
            "the closed pane's child was never killed"
        );
        std::thread::sleep(CLI_ROUND_TRIP_POLL_INTERVAL_DURATION);
    }
}

/// The lock mode the session holds for `client_id`.
fn get_client_lock_mode(session: &RunningSession, client_id: ClientId) -> LockMode {
    let session_overview = session.fetch_session_overview();
    let matching_client = session_overview
        .clients
        .iter()
        .find(|client_discovery| client_discovery.client_id == client_id)
        .expect("the client is attached to the session");
    matching_client.lock_mode
}

/// One solve per client viewing `tab_id`, read over the control socket by the
/// library call `koshi debug dump-layout` makes. Fails the test when the
/// session reports any tab other than `tab_id` alone.
fn list_tab_layout_solves(
    session: &RunningSession,
    tab_id: TabId,
) -> Vec<koshi_ipc::layout::SolvedTab> {
    let session_layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        None,
        session.session_id,
        None,
    )
    .expect("the session describes its layout");
    assert_eq!(session_layout.tabs.len(), 1);
    assert_eq!(session_layout.tabs[0].tab_id, tab_id);
    session_layout.tabs[0].solved_tabs.clone()
}

/// The mode `client_id` uses for the tab, taken from `layout_solves`.
fn get_client_layout_mode(
    layout_solves: &[koshi_ipc::layout::SolvedTab],
    client_id: ClientId,
) -> koshi_layout::mode::LayoutMode {
    layout_solves
        .iter()
        .find(|layout_solve| layout_solve.client_id == client_id)
        .expect("the client views the tab")
        .layout_mode
}

/// Run `koshi new-pane --direction right` in `pane_id` as `client`, and hand
/// back the pane it created. The tab then holds the two panes side by side.
fn build_neighboring_pane(
    session: &RunningSession,
    client: &AttachedClient,
    pane_id: PaneId,
) -> PaneId {
    let (command_result, exit_code) = run_cli_invocation(
        session,
        client,
        pane_id,
        &["koshi", "new-pane", "--direction", "right"],
    );
    assert_eq!(exit_code, CliExitCode::Success);
    match list_emitted_events(&command_result) {
        [Event::PaneCreated(created_pane), ..] => created_pane.pane_id,
        unexpected_events => panic!("expected a pane to be created, got {unexpected_events:?}"),
    }
}

#[test]
fn new_pane_over_the_socket_splits_and_reports_success() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let tab_id = get_only_tab_id(&session);

    let (command_result, exit_code) = run_cli_invocation(
        &session,
        &client,
        root_pane_id,
        &["koshi", "new-pane", "--direction", "right"],
    );

    // The split spawned exactly one more child, and the CLI's `--direction`
    // put it beside the pane the command was typed in.
    assert_eq!(session.list_pane_ids().len(), 2);
    let created_pane_id = session.list_pane_ids()[1];
    assert_eq!(
        list_emitted_events(&command_result),
        [
            Event::PaneCreated(PaneCreated {
                pane_id: created_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id: client.client_id,
                tab_id: Some(tab_id),
                pane_id: created_pane_id,
                previous_pane_id: Some(root_pane_id),
            }),
            // Both children are sized to the rects the split solved.
            Event::PtyResized(PtyResized {
                pane_id: created_pane_id,
                pty_size: PtySize {
                    column_count: 38,
                    row_count: 20
                },
            }),
            Event::PtyResized(PtyResized {
                pane_id: root_pane_id,
                pty_size: PtySize {
                    column_count: 38,
                    row_count: 20
                },
            }),
        ]
    );
    assert_eq!(
        receive_session_events(&client, 3),
        vec![
            SessionEvent::PaneCreated {
                pane_id: created_pane_id,
                tab_id: Some(tab_id),
            },
            SessionEvent::LayoutChanged { tab_id },
            SessionEvent::PaneFocused {
                client_id: client.client_id,
                tab_id: Some(tab_id),
                pane_id: created_pane_id,
                previous_pane_id: Some(root_pane_id),
            },
        ]
    );
    assert_eq!(exit_code, CliExitCode::Success);
}

#[test]
fn close_pane_over_the_socket_kills_the_child_and_removes_the_pane() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let tab_id = get_only_tab_id(&session);
    let created_pane_id = build_neighboring_pane(&session, &client, root_pane_id);
    // The split's own frames are the previous test's subject.
    let _ = receive_session_events(&client, 3);

    let (command_result, exit_code) =
        run_cli_invocation(&session, &client, created_pane_id, &["koshi", "close-pane"]);

    // The close is one transaction: the pane the command was typed in leaves
    // the layout, the client's focus falls back to the pane that stayed, and
    // that pane's child is resized to the space it took back.
    assert_eq!(
        list_emitted_events(&command_result),
        [
            Event::PaneClosing(PaneClosing {
                pane_id: created_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: created_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id: client.client_id,
                tab_id: Some(tab_id),
                pane_id: root_pane_id,
                previous_pane_id: Some(created_pane_id),
            }),
            Event::PtyResized(PtyResized {
                pane_id: root_pane_id,
                pty_size: PtySize {
                    column_count: 78,
                    row_count: 20
                },
            }),
        ]
    );
    assert_eq!(
        receive_session_events(&client, 4),
        vec![
            SessionEvent::PaneClosing {
                pane_id: created_pane_id,
            },
            SessionEvent::PaneRemoved {
                pane_id: created_pane_id,
                tab_id: Some(tab_id),
            },
            SessionEvent::LayoutChanged { tab_id },
            SessionEvent::PaneFocused {
                client_id: client.client_id,
                tab_id: Some(tab_id),
                pane_id: root_pane_id,
                previous_pane_id: Some(created_pane_id),
            },
        ]
    );
    assert_eq!(exit_code, CliExitCode::Success);

    // With no `--force`, the pane's own close policy picks the kill: a
    // graceful one carrying the standard window.
    assert_eq!(
        wait_for_pane_kill_policies(&session, created_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
    // The pane that stayed takes the whole tab back.
    assert_eq!(
        get_last_pane_size(&session, root_pane_id),
        PtySize {
            column_count: 78,
            row_count: 20
        }
    );
    let session_overview = session.fetch_session_overview();
    assert_eq!(
        session_overview
            .panes
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<_>>(),
        vec![root_pane_id]
    );
}

#[test]
fn resize_pane_over_the_socket_moves_the_border_and_resizes_the_child() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let tab_id = get_only_tab_id(&session);
    let created_pane_id = build_neighboring_pane(&session, &client, root_pane_id);
    let _ = receive_session_events(&client, 3);
    // The split left both children at 38 columns by 20 rows.
    assert_eq!(
        get_last_pane_size(&session, created_pane_id),
        PtySize {
            column_count: 38,
            row_count: 20
        }
    );

    let (command_result, exit_code) = run_cli_invocation(
        &session,
        &client,
        created_pane_id,
        &["koshi", "resize-pane", "--direction", "left", "--size", "5"],
    );

    // Moving the pane's left border outward by 5 cells widens it to 43 and
    // narrows the neighbor on that side to 33.
    assert_eq!(
        list_emitted_events(&command_result),
        [
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PtyResized(PtyResized {
                pane_id: root_pane_id,
                pty_size: PtySize {
                    column_count: 33,
                    row_count: 20
                },
            }),
            Event::PtyResized(PtyResized {
                pane_id: created_pane_id,
                pty_size: PtySize {
                    column_count: 43,
                    row_count: 20
                },
            }),
        ]
    );
    assert_eq!(
        receive_session_events(&client, 1),
        vec![SessionEvent::LayoutChanged { tab_id }]
    );
    assert_eq!(exit_code, CliExitCode::Success);

    assert_eq!(
        get_last_pane_size(&session, created_pane_id),
        PtySize {
            column_count: 43,
            row_count: 20
        }
    );
    assert_eq!(
        get_last_pane_size(&session, root_pane_id),
        PtySize {
            column_count: 33,
            row_count: 20
        }
    );
}

#[test]
fn lock_over_the_socket_puts_the_client_in_locked_input_mode() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    assert_eq!(
        get_client_lock_mode(&session, client.client_id),
        LockMode::Normal
    );

    let (command_result, exit_code) =
        run_cli_invocation(&session, &client, root_pane_id, &["koshi", "lock"]);

    assert_eq!(
        list_emitted_events(&command_result),
        [Event::InputModeChanged(InputModeChanged {
            client_id: client.client_id,
            lock_mode: LockMode::Locked,
        })]
    );
    assert_eq!(exit_code, CliExitCode::Success);
    assert_eq!(
        get_client_lock_mode(&session, client.client_id),
        LockMode::Locked
    );
}

#[test]
fn unlock_over_the_socket_returns_the_client_to_normal_input_mode() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let (_, locked_exit_code) =
        run_cli_invocation(&session, &client, root_pane_id, &["koshi", "lock"]);
    assert_eq!(locked_exit_code, CliExitCode::Success);
    assert_eq!(
        get_client_lock_mode(&session, client.client_id),
        LockMode::Locked
    );

    let (command_result, exit_code) =
        run_cli_invocation(&session, &client, root_pane_id, &["koshi", "unlock"]);

    assert_eq!(
        list_emitted_events(&command_result),
        [Event::InputModeChanged(InputModeChanged {
            client_id: client.client_id,
            lock_mode: LockMode::Normal,
        })]
    );
    assert_eq!(exit_code, CliExitCode::Success);
    assert_eq!(
        get_client_lock_mode(&session, client.client_id),
        LockMode::Normal
    );
}

#[test]
fn new_tab_with_a_client_flag_switches_that_client_onto_the_new_tab() {
    let session = RunningSession::start_session();
    let issuer_client = attach_test_client(&session);
    let named_client = attach_test_client(&session);
    assert_ne!(issuer_client.client_id, named_client.client_id);
    let root_pane_id = session.list_pane_ids()[0];
    let first_tab_id = get_only_tab_id(&session);
    let named_client_id_text = named_client.client_id.to_string();

    let (_, exit_code) = run_cli_invocation(
        &session,
        &issuer_client,
        root_pane_id,
        &["koshi", "new-tab", "--client", &named_client_id_text],
    );

    assert_eq!(exit_code, CliExitCode::Success);
    let session_overview = session.fetch_session_overview();
    assert_eq!(session_overview.tabs.len(), 2);
    assert_eq!(session_overview.tabs[0].tab_id, first_tab_id);
    let new_tab_id = session_overview.tabs[1].tab_id;

    let session_layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        None,
        session.session_id,
        None,
    )
    .expect("the session describes its layout");
    let get_active_tab_id = |client_id: ClientId| {
        session_layout
            .clients
            .iter()
            .find(|client_focus| client_focus.client_id == client_id)
            .expect("the client is attached to the session")
            .active_tab_id
    };
    assert_eq!(get_active_tab_id(named_client.client_id), new_tab_id);
    // The client that typed the command stays where it was.
    assert_eq!(get_active_tab_id(issuer_client.client_id), first_tab_id);
}

#[test]
fn a_client_flag_zooms_that_client_and_leaves_the_other_tiled() {
    let session = RunningSession::start_session();
    let issuer_client = attach_test_client(&session);
    let named_client = attach_test_client(&session);
    assert_ne!(issuer_client.client_id, named_client.client_id);
    let root_pane_id = session.list_pane_ids()[0];
    let tab_id = get_only_tab_id(&session);

    let named_client_id_text = named_client.client_id.to_string();
    let (command_result, exit_code) = run_external_cli_invocation(
        &session,
        &[
            "koshi",
            "toggle-pane-fullscreen",
            "--client",
            &named_client_id_text,
        ],
    );

    let CommandResult::Ok { .. } = &command_result else {
        panic!("the command was answered with {command_result:?}");
    };
    assert_eq!(exit_code, CliExitCode::Success);
    // The zoom lands on the named client's own view. The client that sent the
    // command keeps its tiled one.
    let layout_solves = list_tab_layout_solves(&session, tab_id);
    assert_eq!(layout_solves.len(), 2);
    assert_eq!(
        get_client_layout_mode(&layout_solves, named_client.client_id),
        koshi_layout::mode::LayoutMode::Fullscreen {
            focused_pane_id: root_pane_id
        }
    );
    assert_eq!(
        get_client_layout_mode(&layout_solves, issuer_client.client_id),
        koshi_layout::mode::LayoutMode::Tiled
    );
}

#[test]
fn a_fullscreen_command_naming_no_client_is_refused_and_zooms_nothing() {
    let session = RunningSession::start_session();
    let issuer_client = attach_test_client(&session);
    let named_client = attach_test_client(&session);
    assert_ne!(issuer_client.client_id, named_client.client_id);
    let tab_id = get_only_tab_id(&session);

    let (command_result, exit_code) =
        run_external_cli_invocation(&session, &["koshi", "toggle-pane-fullscreen"]);

    match &command_result {
        CommandResult::Rejected { reason, .. } => {
            assert_eq!(*reason, RejectReason::TargetAmbiguous);
        }
        unexpected_result => panic!("the command was answered with {unexpected_result:?}"),
    }
    assert_eq!(exit_code, CliExitCode::RuntimeAction);
    // A refused toggle changes nothing: both clients still solve the tab tiled.
    let layout_solves = list_tab_layout_solves(&session, tab_id);
    assert_eq!(layout_solves.len(), 2);
    assert_eq!(
        get_client_layout_mode(&layout_solves, named_client.client_id),
        koshi_layout::mode::LayoutMode::Tiled
    );
    assert_eq!(
        get_client_layout_mode(&layout_solves, issuer_client.client_id),
        koshi_layout::mode::LayoutMode::Tiled
    );
}

// --- The debug dumps ---

#[test]
fn dump_layout_over_the_socket_describes_a_tab_no_client_is_viewing() {
    // The session is seeded headless: its tab has a tree, and no viewport to
    // solve it against.
    let session = RunningSession::start_session();
    let root_pane_id = session.list_pane_ids()[0];

    let session_layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        None,
        session.session_id,
        None,
    )
    .expect("the session describes its layout");

    assert_eq!(session_layout.session_id, session.session_id);
    assert_eq!(session_layout.session_name, "quiet-lake");
    assert_eq!(session_layout.tabs.len(), 1);
    assert_eq!(session_layout.tabs[0].tab_index, 0);
    assert_eq!(
        session_layout.tabs[0].layout_tree,
        koshi_layout::tree::LayoutNode::Pane(root_pane_id)
    );
    assert_eq!(session_layout.tabs[0].solved_tabs, Vec::new());
    assert_eq!(session_layout.clients, Vec::new());

    let expected_output = format!(
        "session {} quiet-lake\n  tab {} {} index 0\n    tree\n      pane {root_pane_id}\n    \
         no client views this tab\n  clients\n",
        session.session_id, session_layout.tabs[0].tab_id, session_layout.tabs[0].tab_name
    );
    assert_eq!(
        koshi::output::render_layouts(&[session_layout], koshi::cli::OutputFormat::Table),
        expected_output
    );
}

#[test]
fn dump_layout_over_the_socket_shows_the_attached_clients_solved_rectangles() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];

    let session_layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        None,
        session.session_id,
        None,
    )
    .expect("the session describes its layout");

    assert_eq!(session_layout.tabs.len(), 1);
    let solved_tabs = &session_layout.tabs[0].solved_tabs;
    assert_eq!(solved_tabs.len(), 1);
    assert_eq!(solved_tabs[0].client_id, client.client_id);
    // The tab solves against the terminal minus its two chrome rows: an 80x24
    // client results in an 80x22 viewport.
    assert_eq!(
        solved_tabs[0].viewport_size,
        Size {
            column_count: 80,
            row_count: 22
        }
    );
    assert_eq!(
        solved_tabs[0].layout_mode,
        koshi_layout::mode::LayoutMode::Tiled
    );
    assert_eq!(
        solved_tabs[0].pane_rects,
        vec![koshi_ipc::layout::SolvedPane {
            pane_id: root_pane_id,
            outer_rect: koshi_core::geometry::Rect::from_origin_and_size(
                koshi_core::geometry::Point { column: 0, row: 0 },
                Size {
                    column_count: 80,
                    row_count: 22
                },
            ),
        }],
    );
    assert_eq!(solved_tabs[0].suppressed_pane_ids, Vec::new());
    assert!(!solved_tabs[0].is_every_pane_suppressed);
    assert_eq!(solved_tabs[0].stack_headers, Vec::new());
    assert_eq!(
        session_layout.clients,
        vec![koshi_ipc::layout::ClientFocus {
            client_id: client.client_id,
            active_tab_id: session_layout.tabs[0].tab_id,
            focused_pane_id: Some(root_pane_id),
        }],
    );
}

#[test]
fn dump_layout_over_the_socket_narrowed_to_one_tab_describes_that_tab_alone() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let (_, exit_code) = run_cli_invocation(&session, &client, root_pane_id, &["koshi", "new-tab"]);
    assert_eq!(exit_code, CliExitCode::Success);
    let wanted_tab_id = session.fetch_session_overview().tabs[1].tab_id;

    let session_layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        None,
        session.session_id,
        Some(wanted_tab_id),
    )
    .expect("the session describes its layout");

    assert_eq!(session_layout.tabs.len(), 1);
    assert_eq!(session_layout.tabs[0].tab_id, wanted_tab_id);
    assert_eq!(session_layout.tabs[0].tab_index, 1);
}

#[test]
fn dump_layout_over_the_socket_narrowed_to_an_unknown_tab_reports_the_tab_missing() {
    // The session answers; it simply holds no such tab. Naming a tab and
    // getting an empty answer is a missing target, not a successful dump.
    let session = RunningSession::start_session();
    let unknown_tab_id = TabId::new();

    let layout_error = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        None,
        session.session_id,
        Some(unknown_tab_id),
    )
    .expect_err("the session holds no such tab");

    assert_eq!(
        layout_error.to_string(),
        CliError::CommandRejected {
            reason: RejectReason::TargetNotFound,
            help: Some(format!("no running session has tab {unknown_tab_id}")),
        }
        .to_string(),
    );
}

#[test]
fn dump_layout_against_a_session_that_is_not_running_reports_it_as_not_running() {
    let runtime_directory = build_test_runtime_directory();
    let missing_session_id = SessionId::new();

    let session_error = koshi_link::ipc_client::fetch_layout(
        runtime_directory.path(),
        None,
        missing_session_id,
        None,
    )
    .expect_err("nothing advertises that session");

    let CliError::SessionNotFound { session_name } = session_error else {
        panic!("expected SessionNotFound, got {session_error:?}");
    };
    assert_eq!(session_name, missing_session_id.to_string());
}

#[test]
fn dump_state_over_the_socket_hides_a_pane_commands_arguments() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let (_, exit_code) = run_cli_invocation(
        &session,
        &client,
        root_pane_id,
        &["koshi", "run", "--", "mysql", "-pHUNTER2"],
    );
    assert_eq!(exit_code, CliExitCode::Success);

    let mut redacted_session_overviews = vec![session.fetch_session_overview()];
    koshi_link::discovery::redact_pane_commands(&mut redacted_session_overviews);

    let command_pane = redacted_session_overviews[0]
        .panes
        .iter()
        .find(|pane_discovery| pane_discovery.pane_id != root_pane_id)
        .expect("the command pane is listed");
    assert_eq!(
        command_pane.command_argv,
        Some(vec!["mysql".to_string(), "***".to_string()]),
    );

    let rendered_output = koshi::output::render_dump_state(
        &redacted_session_overviews,
        koshi::cli::OutputFormat::Table,
    );
    assert!(rendered_output.contains("mysql ***"), "{rendered_output}");
    assert!(
        !rendered_output.contains("HUNTER2"),
        "the password never reaches the dump: {rendered_output}"
    );
}

#[test]
fn a_resize_with_no_neighbor_is_refused_and_reports_the_action_exit_code() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];

    // The session holds one pane: no pane sits beside it, and neither of its
    // borders can move.
    let (command_result, exit_code) = run_cli_invocation(
        &session,
        &client,
        root_pane_id,
        &["koshi", "resize-pane", "--direction", "left", "--size", "5"],
    );

    let CommandResult::Rejected { reason, .. } = &command_result else {
        panic!("expected a rejection, got {command_result:?}");
    };
    assert_eq!(*reason, RejectReason::InvalidState);
    assert_eq!(exit_code, CliExitCode::RuntimeAction);
    assert_eq!(session.list_pane_ids().len(), 1);
    assert_eq!(
        get_last_pane_size(&session, root_pane_id),
        PtySize {
            column_count: 78,
            row_count: 20
        }
    );
}
