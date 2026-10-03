//! `koshi update` on a build from source and `koshi restart-servers` against
//! real session servers: every running session restarts into its program file,
//! including a session already on this version, and each prints one line.

use std::path::Path;
use std::process::Output;

use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::EndpointFile;

mod common;

use common::{
    build_koshi_command_under_home, build_short_test_directory,
    resolve_runtime_directory_under_home, start_session_server_under_home, write_test_config,
    RunningProcess, SessionProcess,
};
use koshi_test_support::fixtures::start_program_process;

/// The version of the `koshi` binary this build produced.
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

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
