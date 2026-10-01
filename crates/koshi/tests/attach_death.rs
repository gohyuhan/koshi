//! What an attached client sees when its session server ends.
//!
//! A real session server runs as its own process. The test joins it — Hello
//! then Attach on one connection — and reads the event stream after the server
//! ends:
//!
//! - A killed server writes no frame, and the read fails with `ipc peer
//!   disconnected`. A `koshi attach` client prints `the session ended
//!   unexpectedly` and exits with the runtime-action code.
//! - A session that `koshi kill-session` ends writes [`SessionEvent::Quit`] on
//!   every attached stream.
//! - A `koshi detach --all` ends a `koshi attach` client with exit code `0`.
//! - With `auto-close-session #true`, the session server process ends when its
//!   last client leaves. With the default, it keeps running.
//!
//! Each test serves its own temporary runtime directory, and starts every
//! `koshi` process under its own temporary home. The tests that start `koshi
//! attach` or write a `koshi.kdl` run on Unix only.

#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Child;
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
#[cfg(unix)]
use koshi_ipc::endpoint::EndpointFile;
#[cfg(unix)]
use koshi_ipc::event::SessionEvent;
#[cfg(unix)]
use koshi_ipc::protocol::{IpcRequest, IpcRequestKind, IpcResponse, IpcResult};
#[cfg(unix)]
use koshi_ipc::router::resolve_router_endpoint_path;
use koshi_test_support::fixtures::build_test_runtime_directory;

mod common;

use common::session_connection::{
    attach_client_on_connection, read_session_ending, wait_for_session_connection,
};
#[cfg(unix)]
use common::{
    build_koshi_command_under_home, resolve_runtime_directory_under_home, write_test_config,
    POLL_INTERVAL_DURATION, WAIT_DURATION,
};
use common::{build_short_test_directory, start_session_server_under_home};

/// The router serving `runtime_directory`, which the attaching client started
/// on finding none running. Dropping it ends the process the router endpoint
/// file names, and does nothing when that file cannot be read.
#[cfg(unix)]
struct RouterStartedByClient {
    runtime_directory: PathBuf,
}

#[cfg(unix)]
impl Drop for RouterStartedByClient {
    fn drop(&mut self) {
        let Ok(router_endpoint) =
            EndpointFile::load_from_path(&resolve_router_endpoint_path(&self.runtime_directory))
        else {
            return;
        };
        common::terminate_process(router_endpoint.process_id);
    }
}

/// A `koshi attach` the test started. Dropping it ends that process.
#[cfg(unix)]
struct RunningClient {
    child_process: Child,
}

#[cfg(unix)]
impl Drop for RunningClient {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// Start the `koshi` binary as a client attaching to `session_id`, the way a
/// user types `koshi attach <id>`.
#[cfg(unix)]
fn start_attaching_client(home_directory: &Path, session_id: SessionId) -> RunningClient {
    let child_process = build_koshi_command_under_home(home_directory)
        .arg("attach")
        .arg(session_id.to_string())
        .spawn()
        .expect("the koshi binary starts");
    RunningClient { child_process }
}

/// `the client exited <status>: <what it wrote to its error stream>` once
/// `attaching_client` has ended, for a failure message. `None` while it runs,
/// or while its state cannot be read.
#[cfg(unix)]
fn describe_client_exit(attaching_client: &mut RunningClient) -> Option<String> {
    let exit_status = attaching_client.child_process.try_wait().ok().flatten()?;
    let mut client_error_text = String::new();
    if let Some(stderr_pipe) = attaching_client.child_process.stderr.as_mut() {
        let _ = stderr_pipe.read_to_string(&mut client_error_text);
    }
    Some(format!(
        "the client exited {exit_status}: {}",
        client_error_text.trim()
    ))
}

/// Wait until the session server answers a Hello, then close that connection.
#[cfg(unix)]
fn wait_for_session_server(runtime_directory: &Path, session_id: SessionId) {
    drop(wait_for_session_connection(runtime_directory, session_id));
}

/// Poll the session server's overview every [`POLL_INTERVAL_DURATION`] until
/// it lists one attached client.
///
/// # Panics
/// When the overview cannot be read, or lists no single client within
/// [`WAIT_DURATION`]. The message names how `attaching_client` exited, when it
/// has.
#[cfg(unix)]
fn wait_for_attached_client(
    runtime_directory: &Path,
    session_id: SessionId,
    attaching_client: &mut RunningClient,
) {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        let session_overview = koshi_link::discovery::fetch_session_overview(
            runtime_directory,
            None,
            session_id,
            None,
        )
        .expect("the session server describes itself");
        if session_overview.clients.len() == 1 {
            return;
        }
        assert!(
            Instant::now() < wait_deadline,
            "no client attached to {session_id}; {}",
            describe_client_exit(attaching_client)
                .unwrap_or_else(|| "the client is still running".to_string())
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
}

/// Wait for `attaching_client` to end, polling every
/// [`POLL_INTERVAL_DURATION`], and hand back its exit status, its standard
/// output, and its error stream.
///
/// # Panics
/// When it still runs after [`WAIT_DURATION`], or either stream is not a pipe
/// of UTF-8 text.
#[cfg(unix)]
fn read_client_ending(
    attaching_client: &mut RunningClient,
) -> (std::process::ExitStatus, String, String) {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    let exit_status = loop {
        if let Some(exit_status) = attaching_client
            .child_process
            .try_wait()
            .expect("the client's state can be read")
        {
            break exit_status;
        }
        assert!(
            Instant::now() < wait_deadline,
            "the attaching client kept running"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    };

    let mut client_output_text = String::new();
    let mut client_error_text = String::new();
    attaching_client
        .child_process
        .stdout
        .take()
        .expect("the client's output is a pipe")
        .read_to_string(&mut client_output_text)
        .expect("the client's output reads as text");
    attaching_client
        .child_process
        .stderr
        .take()
        .expect("the client's errors are a pipe")
        .read_to_string(&mut client_error_text)
        .expect("the client's errors read as text");
    (exit_status, client_output_text, client_error_text)
}

#[test]
fn a_killed_session_server_ends_the_stream_with_a_read_failure() {
    let home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let mut session_process = start_session_server_under_home(
        home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (mut viewer_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    attach_client_on_connection(&mut viewer_connection, session_id);

    session_process.terminate_session_server();

    // The server wrote neither goodbye frame, so the read reaches end of
    // stream on a socket the operating system closed with its process.
    let read_error =
        read_session_ending(viewer_connection).expect_err("the stream ends with a read failure");
    assert_eq!(read_error.to_string(), "ipc peer disconnected");
}

#[cfg(unix)]
#[test]
fn a_killed_session_server_ends_the_attaching_client_with_the_death_message() {
    let home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);
    let _client_started_router = RouterStartedByClient {
        runtime_directory: runtime_directory.clone(),
    };

    wait_for_session_server(&runtime_directory, session_id);
    let mut attaching_client = start_attaching_client(home_directory.path(), session_id);
    wait_for_attached_client(&runtime_directory, session_id, &mut attaching_client);

    session_process.terminate_session_server();

    let (exit_status, _, client_error_text) = read_client_ending(&mut attaching_client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
    assert_eq!(
        client_error_text,
        format!(
            "koshi: the session ended unexpectedly\n  \
             run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`\n"
        )
    );
}

/// A real client rides the session replacing its own process image: it leaves
/// the socket it was on, waits for the one the new image binds, and attaches
/// again there. The detach at the end ends the client, which only a client
/// reading a live connection reads.
#[cfg(unix)]
#[test]
fn an_attaching_client_comes_back_after_the_session_replaces_its_image() {
    let home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    let session_id = SessionId::new();
    let _session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);
    let _client_started_router = RouterStartedByClient {
        runtime_directory: runtime_directory.clone(),
    };

    wait_for_session_server(&runtime_directory, session_id);
    let mut attaching_client = start_attaching_client(home_directory.path(), session_id);
    wait_for_attached_client(&runtime_directory, session_id, &mut attaching_client);

    let endpoint_before_restart = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id),
    )
    .expect("the session advertises a socket");
    let (mut control_connection, _) = wait_for_session_connection(&runtime_directory, session_id);
    control_connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Restart,
        })
        .expect("the server reads the restart");
    let ipc_response: IpcResponse = control_connection
        .recv()
        .expect("the server answers the restart");
    assert_eq!(ipc_response.answer_result, IpcResult::Restarting);
    drop(control_connection);

    // Every image binds a socket under a fresh token, so a token other than the
    // one read above is the new image serving.
    let advertise_deadline = Instant::now() + WAIT_DURATION;
    loop {
        let endpoint_attempt = EndpointFile::load_from_path(
            &EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id),
        );
        if endpoint_attempt.is_ok_and(|advertised_endpoint| {
            advertised_endpoint.connection_token.expose_secret()
                != endpoint_before_restart.connection_token.expose_secret()
        }) {
            break;
        }
        assert!(
            Instant::now() < advertise_deadline,
            "the session advertised no new socket; {}",
            describe_client_exit(&mut attaching_client)
                .unwrap_or_else(|| "the client is still running".to_string())
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
    // The record is carried across the swap, so it is there before the client
    // comes back and a detach can land while nobody holds it. Asked for until
    // the client takes it.
    let detach_deadline = Instant::now() + WAIT_DURATION;
    while attaching_client
        .child_process
        .try_wait()
        .expect("the client's state can be read")
        .is_none()
    {
        let detach_output = build_koshi_command_under_home(home_directory.path())
            .arg("detach")
            .arg("--all")
            .arg(session_id.to_string())
            .output()
            .expect("the koshi binary starts");
        assert_eq!(
            detach_output.status.code(),
            Some(CliExitCode::Success.get_exit_code()),
            "the detach left {}",
            String::from_utf8_lossy(&detach_output.stderr)
        );
        assert!(
            Instant::now() < detach_deadline,
            "the client never took a detach, so it never came back on the new socket"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }

    let (exit_status, client_output_text, client_error_text) =
        read_client_ending(&mut attaching_client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(client_error_text, "");
    assert!(
        client_output_text.ends_with(&format!("detached from session {session_id}\n")),
        "the client ended with {client_output_text:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_detach_ends_the_attaching_client_with_a_success() {
    let home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    let session_id = SessionId::new();
    let _session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);
    let _client_started_router = RouterStartedByClient {
        runtime_directory: runtime_directory.clone(),
    };

    wait_for_session_server(&runtime_directory, session_id);
    let mut attaching_client = start_attaching_client(home_directory.path(), session_id);
    wait_for_attached_client(&runtime_directory, session_id, &mut attaching_client);

    // The session keeps running, so the goodbye frame the server writes as it
    // closes the client's queue is the whole ending the client reads.
    let detach_output = build_koshi_command_under_home(home_directory.path())
        .arg("detach")
        .arg("--all")
        .arg(session_id.to_string())
        .output()
        .expect("the koshi binary starts");
    assert_eq!(
        detach_output.status.code(),
        Some(CliExitCode::Success.get_exit_code()),
        "the detach left {}",
        String::from_utf8_lossy(&detach_output.stderr)
    );

    let (exit_status, client_output_text, client_error_text) =
        read_client_ending(&mut attaching_client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(client_error_text, "");
    // The client leaves the alternate screen before it prints, so what it says
    // about the ending is the last thing on its output.
    assert!(
        client_output_text.ends_with(&format!("detached from session {session_id}\n")),
        "the client ended with {client_output_text:?}"
    );
}

/// `auto-close-session #true`: the session server process really ends when its
/// last client leaves, not merely that a flag was set.
#[cfg(unix)]
#[test]
fn auto_close_ends_the_session_server_process_when_the_last_client_leaves() {
    let home_directory = build_short_test_directory();
    write_test_config(
        home_directory.path(),
        "version 1\nauto-close-session #true\n",
    );
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);

    let (mut viewer_connection, _) = wait_for_session_connection(&runtime_directory, session_id);
    attach_client_on_connection(&mut viewer_connection, session_id);
    // The connection ending is what the server reads as this client leaving.
    drop(viewer_connection);

    assert!(
        session_process.wait_for_session_server_exit(),
        "the session server outlived its last client"
    );
}

/// A session told to end writes the quit frame on every attached stream before
/// its process goes, so the client says the session ended instead of reporting
/// it dead.
#[cfg(unix)]
#[test]
fn a_kill_session_ends_every_attached_stream_with_the_quit_frame() {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);

    let (mut viewer_connection, _) = wait_for_session_connection(&runtime_directory, session_id);
    attach_client_on_connection(&mut viewer_connection, session_id);

    // The stream is read while `kill-session` runs. `kill-session` names no
    // client, so the session ends rather than one client leaving it.
    let session_ending_reader = std::thread::spawn(move || read_session_ending(viewer_connection));
    let kill_output = build_koshi_command_under_home(home_directory.path())
        .arg("kill-session")
        .arg(session_id.to_string())
        .output()
        .expect("the koshi binary starts");

    assert_eq!(
        session_ending_reader
            .join()
            .expect("the stream reader ends")
            .expect("the stream ends with a frame"),
        SessionEvent::Quit,
        "the kill left {}",
        String::from_utf8_lossy(&kill_output.stderr).trim()
    );
    assert_eq!(
        kill_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(String::from_utf8_lossy(&kill_output.stdout), "");
    assert_eq!(String::from_utf8_lossy(&kill_output.stderr), "");
    assert!(
        session_process.wait_for_session_server_exit(),
        "the session server outlived the kill"
    );
}

/// The default leaves the session server running with nothing attached, which is
/// what makes `koshi attach` able to rejoin it.
#[cfg(unix)]
#[test]
fn a_session_server_outlives_its_last_client_by_default() {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);

    let (mut viewer_connection, _) = wait_for_session_connection(&runtime_directory, session_id);
    attach_client_on_connection(&mut viewer_connection, session_id);
    drop(viewer_connection);

    // It answers a fresh connection after the client that was attached is gone.
    let (rejoined_connection, _) = wait_for_session_connection(&runtime_directory, session_id);
    drop(rejoined_connection);
    assert_eq!(
        session_process
            .child_process
            .try_wait()
            .expect("the session server's state can be read"),
        None,
        "the session server ended without being asked to"
    );
}
