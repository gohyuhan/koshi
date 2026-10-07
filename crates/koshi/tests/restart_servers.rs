//! `koshi update` on a build from source and `koshi restart-servers` against
//! real session servers: every running session restarts into its program file,
//! including a session already on this version, and each prints one line.
//! Servers that koshi 0.2.0 to 0.4.0 started are stand-ins that answer in the
//! envelope of that release: a router is ended and a router of this build
//! starts. A koshi 0.2.0 session is ended once the user answers yes, and keeps
//! running otherwise. A koshi 0.2.0 session that is ended is a copy of this
//! test binary saved as `koshi`, running [`run_koshi_0_2_0_session_process`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use koshi_core::command::CliExitCode;
use koshi_core::ids::{parse_prefixed_uuid, PaneId, SessionId};
use koshi_ipc::endpoint::{compute_socket_address, EndpointFile};
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::router::{compute_router_socket_address, resolve_router_endpoint_path};
use koshi_ipc::transport::{Connection, Listener};
use koshi_link::ipc_client::list_advertised_sessions;
use serde_json::value::RawValue;

mod common;

use common::{
    build_koshi_command_under_home, build_short_test_directory,
    resolve_runtime_directory_under_home, start_router_process, start_session_server_under_home,
    write_test_config, RunningProcess, SessionProcess,
};
use koshi_test_support::fixtures::{
    close_connection_after_peer_hangs_up, spawn_previous_release_session, start_program_process,
    KOSHI_0_2_0_HELLO_ANSWER_TEXT, KOSHI_0_2_0_RESTART_REFUSAL_TEXT,
    PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT,
};

/// The version of the `koshi` binary this build produced.
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The environment variable naming the runtime directory in which
/// [`run_koshi_0_2_0_session_process`] serves a session of koshi 0.2.0.
const KOSHI_0_2_0_DIRECTORY_VARIABLE: &str = "KOSHI_TEST_KOSHI_0_2_0_DIRECTORY";

/// The environment variable holding the id of the session, such as
/// `session-<uuid>`, that [`run_koshi_0_2_0_session_process`] serves.
const KOSHI_0_2_0_SESSION_VARIABLE: &str = "KOSHI_TEST_KOSHI_0_2_0_SESSION";

/// How long a stand-in session of koshi 0.2.0 has to advertise itself, and to
/// end once koshi ended it.
const STAND_IN_WAIT_DURATION: Duration = Duration::from_secs(10);

/// One session server under a test home, and the endpoint file it advertised
/// before any restart. Dropping it ends the session server it started.
struct AdvertisedSession {
    _session_process: SessionProcess,
    session_id: SessionId,
    endpoint_before_restart: EndpointFile,
}

/// Start `session_count` session servers under `home_directory`, serving the
/// runtime directory [`resolve_runtime_directory_under_home`] names, and wait
/// for each to advertise its endpoint file.
fn start_advertised_sessions(
    home_directory: &Path,
    session_count: usize,
) -> Vec<AdvertisedSession> {
    let runtime_directory = resolve_runtime_directory_under_home(home_directory);
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    (0..session_count)
        .map(|_| {
            let session_id = SessionId::new();
            let mut session_process =
                start_session_server_under_home(home_directory, &runtime_directory, session_id);
            let endpoint_before_restart =
                session_process.wait_for_session_endpoint(&runtime_directory, session_id);
            AdvertisedSession {
                _session_process: session_process,
                session_id,
                endpoint_before_restart,
            }
        })
        .collect()
}

/// The body of a stand-in session of koshi 0.2.0: with
/// [`KOSHI_0_2_0_DIRECTORY_VARIABLE`] and [`KOSHI_0_2_0_SESSION_VARIABLE`] set,
/// it binds that session's address in that directory, writes the endpoint
/// file koshi 0.2.0 writes, `{socket, token, pid}`, naming its own process,
/// and answers every connection as [`answer_as_koshi_0_2_0`] does until it is
/// ended. Without them, it does nothing.
#[test]
#[ignore = "runs only inside a stand-in koshi 0.2.0 session process"]
fn run_koshi_0_2_0_session_process() {
    let Some(runtime_directory) = std::env::var_os(KOSHI_0_2_0_DIRECTORY_VARIABLE) else {
        return;
    };
    let runtime_directory = PathBuf::from(runtime_directory);
    let session_id = SessionId::from_uuid(
        parse_prefixed_uuid(
            &std::env::var(KOSHI_0_2_0_SESSION_VARIABLE).expect("the stand-in names its session"),
            "session",
        )
        .expect("the stand-in names a session id"),
    );
    let socket_address = compute_socket_address(&runtime_directory, session_id);
    let session_listener = Listener::bind(&socket_address).expect("bind the stand-in session");
    let endpoint_file_text = serde_json::json!({
        "socket": socket_address,
        "token": "k7QxSecret",
        "pid": std::process::id(),
    })
    .to_string();
    std::fs::write(
        EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id),
        endpoint_file_text,
    )
    .expect("write the endpoint file of koshi 0.2.0");
    loop {
        let session_connection = session_listener.accept().expect("accept a caller");
        std::thread::spawn(move || answer_as_koshi_0_2_0(session_connection));
    }
}

/// Answer each frame `session_connection` reads as a session of koshi 0.2.0
/// does, until the caller hangs up: a Hello in the envelope of that release
/// with [`KOSHI_0_2_0_HELLO_ANSWER_TEXT`], a Restart in that envelope with
/// [`KOSHI_0_2_0_RESTART_REFUSAL_TEXT`], and every other frame, this build's
/// envelope included, with [`PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT`].
fn answer_as_koshi_0_2_0(mut session_connection: Connection) {
    while let Ok(request_frame) = session_connection.recv::<serde_json::Value>() {
        let answer_text = if request_frame["kind"]["Hello"].is_object() {
            KOSHI_0_2_0_HELLO_ANSWER_TEXT
        } else if request_frame["kind"] == "Restart" {
            KOSHI_0_2_0_RESTART_REFUSAL_TEXT
        } else {
            PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT
        };
        let answer_frame =
            RawValue::from_string(answer_text.to_string()).expect("the answer is JSON");
        if session_connection.send(&answer_frame).is_err() {
            return;
        }
    }
}

/// Start a stand-in session of koshi 0.2.0 serving `session_id` in
/// `runtime_directory`: a copy of this test binary saved as `koshi`, or
/// `koshi.exe` on Windows, in `program_directory`, running
/// [`run_koshi_0_2_0_session_process`]. Waits until its endpoint file is
/// written.
///
/// # Panics
///
/// Panics when the copy or the start fails, or when no endpoint file is
/// written within [`STAND_IN_WAIT_DURATION`].
fn start_koshi_0_2_0_session(
    program_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> SessionProcess {
    let program_path = program_directory.join(if cfg!(windows) { "koshi.exe" } else { "koshi" });
    std::fs::create_dir_all(program_directory).expect("a directory for the stand-in program");
    std::fs::copy(
        std::env::current_exe().expect("the test binary has a path"),
        &program_path,
    )
    .expect("the test binary is copied");
    let session_process = SessionProcess {
        child_process: start_program_process(
            ProcessCommand::new(&program_path)
                .args([
                    "--exact",
                    "run_koshi_0_2_0_session_process",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env(KOSHI_0_2_0_DIRECTORY_VARIABLE, runtime_directory)
                .env(KOSHI_0_2_0_SESSION_VARIABLE, session_id.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        ),
    };
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let wait_end = Instant::now() + STAND_IN_WAIT_DURATION;
    while EndpointFile::load_from_path(&endpoint_file_path).is_err() {
        assert!(
            Instant::now() < wait_end,
            "the stand-in session of koshi 0.2.0 never wrote its endpoint file"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    session_process
}

/// Whether `session_process` ends within [`STAND_IN_WAIT_DURATION`].
fn wait_for_session_process_to_end(session_process: &mut SessionProcess) -> bool {
    let wait_end = Instant::now() + STAND_IN_WAIT_DURATION;
    while !session_process.has_session_server_exited() {
        if Instant::now() >= wait_end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Start two stand-in sessions of koshi 0.2.0 serving `runtime_directory`, as
/// [`start_koshi_0_2_0_session`] starts each, from the program directories
/// `stand-in-1` and `stand-in-2` under `home_directory`. Hands back both
/// processes, and both session ids in the order [`list_advertised_sessions`]
/// lists them, which is the order `koshi restart-servers` names them in.
fn start_two_koshi_0_2_0_sessions(
    home_directory: &Path,
    runtime_directory: &Path,
) -> (Vec<SessionProcess>, [SessionId; 2]) {
    let stand_in_sessions = ["stand-in-1", "stand-in-2"]
        .into_iter()
        .map(|program_directory_name| {
            start_koshi_0_2_0_session(
                &home_directory.join(program_directory_name),
                runtime_directory,
                SessionId::new(),
            )
        })
        .collect();
    let listed_session_ids =
        list_advertised_sessions(runtime_directory).expect("the runtime directory is read");
    let [first_listed_session_id, second_listed_session_id] = listed_session_ids[..] else {
        panic!("the runtime directory lists two sessions: {listed_session_ids:?}");
    };
    (
        stand_in_sessions,
        [first_listed_session_id, second_listed_session_id],
    )
}

/// Run `koshi_command` with `input_text` on its standard input, to its end,
/// and hand back its exit status and both output streams.
fn run_koshi_command_with_input(koshi_command: &mut ProcessCommand, input_text: &str) -> Output {
    let mut koshi_process = start_program_process(koshi_command.stdin(Stdio::piped()));
    koshi_process
        .stdin
        .take()
        .expect("the standard input is a pipe")
        .write_all(input_text.as_bytes())
        .expect("the answer is written");
    koshi_process
        .wait_with_output()
        .expect("the koshi binary runs to its end")
}

/// The question `koshi restart-servers` asks before it ends `session_ids`,
/// which koshi 0.2.0 started, named in that order.
///
/// Example: two ids give `koshi <version> cannot move session-<uuid>,
/// session-<uuid>, which koshi 0.2.0 started. End them and the programs in
/// their panes? [y/N] `.
fn format_end_question(session_ids: &[SessionId]) -> String {
    let session_id_list = session_ids
        .iter()
        .map(SessionId::to_string)
        .collect::<Vec<String>>()
        .join(", ");
    match session_ids.len() {
        1 => format!(
            "koshi {BUILD_VERSION} cannot move {session_id_list}, which koshi 0.2.0 started. End \
             it and the programs in its panes? [y/N] "
        ),
        _ => format!(
            "koshi {BUILD_VERSION} cannot move {session_id_list}, which koshi 0.2.0 started. End \
             them and the programs in their panes? [y/N] "
        ),
    }
}

/// Run `koshi <command_name>` under `home_directory` to its end, and hand back
/// its exit status and both output streams.
fn run_koshi_command(home_directory: &Path, command_name: &str) -> Output {
    start_program_process(build_koshi_command_under_home(home_directory).arg(command_name))
        .wait_with_output()
        .expect("the koshi binary runs to its end")
}

/// The endpoint file `advertised_session` advertises now, and a guard that
/// ends the process it names. Panics when the file still carries the token it
/// carried before the restart.
fn load_restarted_endpoint(
    home_directory: &Path,
    advertised_session: &AdvertisedSession,
) -> (EndpointFile, RunningProcess) {
    let restarted_endpoint =
        EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
            &resolve_runtime_directory_under_home(home_directory),
            advertised_session.session_id,
        ))
        .expect("the restarted session advertises its endpoint file");
    assert_ne!(
        restarted_endpoint.connection_token,
        advertised_session.endpoint_before_restart.connection_token,
        "{} kept the socket it served before the restart",
        advertised_session.session_id
    );
    let restarted_process_guard = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };
    (restarted_endpoint, restarted_process_guard)
}

/// The line `koshi update` and `koshi restart-servers` print for a session
/// that came back on this build.
fn format_restarted_line(session_id: SessionId) -> String {
    format!("{session_id} restarted into koshi {BUILD_VERSION}; its panes keep running")
}

#[test]
fn update_on_a_build_from_source_restarts_a_session_already_on_this_version() {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let advertised_sessions = start_advertised_sessions(home_directory.path(), 1);

    let update_output = run_koshi_command(home_directory.path(), "update");

    assert_eq!(
        String::from_utf8_lossy(&update_output.stderr),
        "",
        "koshi update wrote to standard error"
    );
    assert_eq!(
        String::from_utf8_lossy(&update_output.stdout),
        format!(
            "koshi {BUILD_VERSION} at {} was built from source; koshi update downloads nothing \
             for it, and restarts every running server into the koshi at that path\n{}\n",
            Path::new(env!("CARGO_BIN_EXE_koshi")).display(),
            format_restarted_line(advertised_sessions[0].session_id)
        )
    );
    assert_eq!(
        update_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    let _restarted_session =
        load_restarted_endpoint(home_directory.path(), &advertised_sessions[0]);
}

#[test]
fn restart_servers_restarts_every_running_session_and_prints_one_line_for_each() {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let advertised_sessions = start_advertised_sessions(home_directory.path(), 2);

    let restart_output = run_koshi_command(home_directory.path(), "restart-servers");

    assert_eq!(
        String::from_utf8_lossy(&restart_output.stderr),
        "",
        "koshi restart-servers wrote to standard error"
    );
    let mut printed_lines: Vec<String> = String::from_utf8_lossy(&restart_output.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    printed_lines.sort();
    let mut expected_lines: Vec<String> = advertised_sessions
        .iter()
        .map(|advertised_session| format_restarted_line(advertised_session.session_id))
        .collect();
    expected_lines.sort();
    assert_eq!(printed_lines, expected_lines);
    assert_eq!(
        restart_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    let _restarted_sessions: Vec<(EndpointFile, RunningProcess)> = advertised_sessions
        .iter()
        .map(|advertised_session| {
            load_restarted_endpoint(home_directory.path(), advertised_session)
        })
        .collect();
}

#[test]
fn restart_servers_ends_a_router_of_koshi_0_4_0_and_starts_a_router_of_this_build() {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    // A router of this build serving another directory stands in for the
    // process of the koshi 0.4.0 router: a koshi process of this user.
    let other_runtime_directory = home_directory.path().join("other");
    std::fs::create_dir_all(&other_runtime_directory).expect("another runtime directory");
    let mut previous_release_router =
        start_router_process(home_directory.path(), &other_runtime_directory);
    let router_socket_address = compute_router_socket_address(&runtime_directory);
    let router_listener =
        Listener::bind(&router_socket_address).expect("bind the stand-in router address");
    EndpointFile {
        socket_address: router_socket_address,
        connection_token: ConnectionToken::generate(),
        process_id: previous_release_router.get_process_id(),
    }
    .write_to_path(&resolve_router_endpoint_path(&runtime_directory))
    .expect("write the router endpoint file");
    let stand_in_thread = std::thread::spawn(move || {
        let mut router_connection = router_listener
            .accept()
            .expect("accept koshi restart-servers");
        drop(router_listener);
        let _hello_request: Box<RawValue> = router_connection.recv().expect("read the hello");
        router_connection
            .send(
                &RawValue::from_string(PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string())
                    .expect("the answer is JSON"),
            )
            .expect("send the hello answer");
        close_connection_after_peer_hangs_up(router_connection);
    });

    let restart_output = run_koshi_command(home_directory.path(), "restart-servers");

    stand_in_thread
        .join()
        .expect("the stand-in served its connection");
    let started_router_endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(&runtime_directory))
            .expect("the started router advertises its endpoint file");
    let _started_router = RunningProcess {
        process_id: started_router_endpoint.process_id,
    };
    assert_eq!(
        String::from_utf8_lossy(&restart_output.stderr),
        "",
        "koshi restart-servers wrote to standard error"
    );
    assert_eq!(
        String::from_utf8_lossy(&restart_output.stdout),
        format!(
            "koshi ended the running router (process {}), which ran a koshi version this one \
             cannot talk to, and started a router on koshi {BUILD_VERSION}; every session keeps \
             running\n",
            previous_release_router.get_process_id()
        )
    );
    assert_eq!(
        restart_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert!(previous_release_router.has_router_exited());
}

#[test]
fn restart_servers_leaves_a_session_of_koshi_0_2_0_running_without_a_yes() {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let stand_in_thread = spawn_previous_release_session(
        &runtime_directory,
        session_id,
        "k7QxSecret",
        vec![
            vec![
                PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string(),
                PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT.to_string(),
            ],
            vec![
                KOSHI_0_2_0_HELLO_ANSWER_TEXT.to_string(),
                KOSHI_0_2_0_RESTART_REFUSAL_TEXT.to_string(),
            ],
        ],
    );

    let restart_output = run_koshi_command(home_directory.path(), "restart-servers");

    stand_in_thread
        .join()
        .expect("the stand-in served both connections");
    assert_eq!(String::from_utf8_lossy(&restart_output.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&restart_output.stderr),
        format!(
            "{}koshi: {session_id} keeps running koshi 0.2.0, and so do its panes; run koshi \
             restart-servers again to end it\nkoshi: not every running koshi server now runs \
             koshi {BUILD_VERSION}; see the lines above\n",
            format_end_question(&[session_id])
        )
    );
    assert_eq!(
        restart_output.status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
}

#[test]
fn restart_servers_ends_a_session_of_koshi_0_2_0_once_the_user_answers_yes() {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut stand_in_session = start_koshi_0_2_0_session(
        &home_directory.path().join("stand-in"),
        &runtime_directory,
        session_id,
    );

    let restart_output = run_koshi_command_with_input(
        build_koshi_command_under_home(home_directory.path()).arg("restart-servers"),
        "y\n",
    );

    assert_eq!(
        String::from_utf8_lossy(&restart_output.stderr),
        format_end_question(&[session_id])
    );
    assert_eq!(
        String::from_utf8_lossy(&restart_output.stdout),
        format!("{session_id} ran koshi 0.2.0; koshi ended it and the programs in its panes\n")
    );
    assert_eq!(
        restart_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert!(wait_for_session_process_to_end(&mut stand_in_session));
}

#[test]
fn restart_servers_in_a_pane_of_a_session_of_koshi_0_2_0_ends_that_session_after_the_others() {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let (mut stand_in_sessions, [first_listed_session_id, second_listed_session_id]) =
        start_two_koshi_0_2_0_sessions(home_directory.path(), &runtime_directory);

    let restart_output = run_koshi_command_with_input(
        build_koshi_command_under_home(home_directory.path())
            .arg("restart-servers")
            .env("KOSHI", "1")
            .env("KOSHI_SESSION_ID", first_listed_session_id.to_string())
            .env("KOSHI_PANE_ID", PaneId::new().to_string()),
        "y\n",
    );

    assert_eq!(
        String::from_utf8_lossy(&restart_output.stderr),
        format_end_question(&[first_listed_session_id, second_listed_session_id])
    );
    assert_eq!(
        String::from_utf8_lossy(&restart_output.stdout),
        format!(
            "{second_listed_session_id} ran koshi 0.2.0; koshi ended it and the programs in its \
             panes\n{first_listed_session_id} ran koshi 0.2.0; koshi ended it and the programs in \
             its panes\n"
        )
    );
    assert_eq!(
        restart_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    for stand_in_session in &mut stand_in_sessions {
        assert!(wait_for_session_process_to_end(stand_in_session));
    }
}

#[test]
fn restart_servers_ends_every_session_of_koshi_0_2_0_after_its_output_closes() {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let (mut stand_in_sessions, listed_session_ids) =
        start_two_koshi_0_2_0_sessions(home_directory.path(), &runtime_directory);
    let end_question = format_end_question(&listed_session_ids);
    let mut koshi_process = start_program_process(
        build_koshi_command_under_home(home_directory.path())
            .arg("restart-servers")
            .stdin(Stdio::piped()),
    );
    let question_length = end_question.len();
    let mut koshi_error_output = koshi_process
        .stderr
        .take()
        .expect("the standard error is a pipe");
    let (question_sender, question_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut question_bytes = vec![0; question_length];
        let question_read = koshi_error_output.read_exact(&mut question_bytes);
        drop(koshi_error_output);
        let _ = question_sender.send(question_read.map(|()| question_bytes));
    });

    let question_bytes = question_receiver
        .recv_timeout(common::WAIT_DURATION)
        .expect("the question arrives on standard error")
        .expect("the question is printed");
    drop(
        koshi_process
            .stdout
            .take()
            .expect("the standard output is a pipe"),
    );
    koshi_process
        .stdin
        .take()
        .expect("the standard input is a pipe")
        .write_all(b"y\n")
        .expect("the answer is written");
    let restart_status = koshi_process
        .wait()
        .expect("the koshi binary runs to its end");

    assert_eq!(String::from_utf8_lossy(&question_bytes), end_question);
    assert_eq!(
        restart_status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    for stand_in_session in &mut stand_in_sessions {
        assert!(wait_for_session_process_to_end(stand_in_session));
    }
}

#[cfg(unix)]
#[test]
fn a_session_in_the_runtime_directory_of_koshi_0_2_0_is_named_by_list_sessions_and_ended_by_restart_servers(
) {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let previous_release_runtime_directory =
        common::resolve_data_directory_under_home(home_directory.path()).join("run");
    std::fs::create_dir_all(&previous_release_runtime_directory)
        .expect("the runtime directory of koshi 0.2.0 under the test home");
    let session_id = SessionId::new();
    let mut stand_in_session = start_koshi_0_2_0_session(
        &home_directory.path().join("stand-in"),
        &previous_release_runtime_directory,
        session_id,
    );
    let empty_listing = koshi::output::render_sessions(&[], koshi::cli::OutputFormat::Table);

    let listing_output = run_koshi_command(home_directory.path(), "list-sessions");

    assert_eq!(
        String::from_utf8_lossy(&listing_output.stderr),
        format!(
            "koshi: 1 session that an older koshi started runs from {}, which this koshi does not \
             list; run koshi restart-servers to move it or end it\n",
            previous_release_runtime_directory.display()
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&listing_output.stdout),
        empty_listing
    );
    assert_eq!(
        listing_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );

    let restart_output = run_koshi_command_with_input(
        build_koshi_command_under_home(home_directory.path()).arg("restart-servers"),
        "y\n",
    );

    assert_eq!(
        String::from_utf8_lossy(&restart_output.stderr),
        format_end_question(&[session_id])
    );
    assert_eq!(
        String::from_utf8_lossy(&restart_output.stdout),
        format!("{session_id} ran koshi 0.2.0; koshi ended it and the programs in its panes\n")
    );
    assert_eq!(
        restart_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert!(wait_for_session_process_to_end(&mut stand_in_session));

    let listing_output_after_restart = run_koshi_command(home_directory.path(), "list-sessions");

    assert_eq!(
        String::from_utf8_lossy(&listing_output_after_restart.stderr),
        ""
    );
    assert_eq!(
        String::from_utf8_lossy(&listing_output_after_restart.stdout),
        empty_listing
    );
}
