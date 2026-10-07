//! Shared test fixtures.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_core::key::{
    BindingModifierFlags, KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags,
};
use koshi_ipc::endpoint::{compute_socket_address, EndpointFile};
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::router::{compute_router_socket_address, resolve_router_endpoint_path};
use koshi_ipc::transport::{Connection, Listener};
use serde_json::value::RawValue;
use tempfile::TempDir;

/// How long [`start_program_process`] keeps trying while the operating system
/// reports the program file as held open for writing: 20 seconds.
pub const BUSY_PROGRAM_WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long [`start_program_process`] pauses between attempts: 20
/// milliseconds.
pub const BUSY_PROGRAM_RETRY_INTERVAL_DURATION: Duration = Duration::from_millis(20);

/// Each keybinding modifier beside the reported modifier that stands for it.
/// The two sets number their bits differently, so a chord's bit pattern is
/// never a reported bit pattern.
const REPORTED_MODIFIER_BY_BINDING_MODIFIER: [(BindingModifierFlags, KeyModifierFlags); 4] = [
    (BindingModifierFlags::CTRL, KeyModifierFlags::CTRL),
    (BindingModifierFlags::ALT, KeyModifierFlags::ALT),
    (BindingModifierFlags::SHIFT, KeyModifierFlags::SHIFT),
    (BindingModifierFlags::SUPER, KeyModifierFlags::SUPER),
];

/// The complete key event a terminal reports for one chord: a press, with no
/// alternative keys and no associated text.
///
/// [`KeyInput::to_binding_chord`] on the result gives `chord` back.
#[must_use]
pub fn build_key_input_for_chord(chord: KeyChord) -> KeyInput {
    let mut modifier_flags = KeyModifierFlags::NONE;
    for (binding_modifier, reported_modifier) in REPORTED_MODIFIER_BY_BINDING_MODIFIER {
        if chord.modifier_flags.has_all_modifiers(binding_modifier) {
            modifier_flags = modifier_flags.union(reported_modifier);
        }
    }
    KeyInput {
        key: KeyIdentity::Key(chord.key),
        key_event_kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags,
    }
}

/// The process id `2147483647`, the largest `pid_t`. No running process holds
/// it.
pub const NO_SUCH_PROCESS_ID: u32 = 2_147_483_647;

/// Create an isolated runtime directory and remove it when the returned
/// [`TempDir`] drops.
///
/// Unix uses `/tmp` as the parent directory. Windows uses
/// [`std::env::temp_dir`].
///
/// # Panics
///
/// Panics when the directory cannot be created.
#[must_use]
pub fn build_test_runtime_directory() -> TempDir {
    #[cfg(unix)]
    let base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base_directory = std::env::temp_dir();
    TempDir::new_in(base_directory).expect("a temporary runtime directory")
}

/// Write a program named `program_stem` into `program_directory` that adds
/// one line to the file at `run_log_path` each time it runs, then prints
/// `printed_line`: a shell script on Unix, and a `.cmd` file on Windows. Hands
/// back the program's path.
///
/// Example: `printed_line` `koshi 9.9.9` makes a program that stands in for a
/// koshi binary answering `--version` with that line.
///
/// # Panics
///
/// Panics when the program cannot be written or made runnable.
pub fn write_printing_program(
    program_directory: &Path,
    program_stem: &str,
    run_log_path: &Path,
    printed_line: &str,
) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let program_path = program_directory.join(program_stem);
        let program_text = format!(
            "#!/bin/sh\necho run >> '{}'\necho '{printed_line}'\n",
            run_log_path.display()
        );
        std::fs::write(&program_path, program_text).expect("the program is written");
        std::fs::set_permissions(&program_path, std::fs::Permissions::from_mode(0o755))
            .expect("the program runs");
        program_path
    }
    #[cfg(windows)]
    {
        let program_path = program_directory.join(format!("{program_stem}.cmd"));
        let program_text = format!(
            "@echo off\r\necho run>>\"{}\"\r\necho {printed_line}\r\n",
            run_log_path.display()
        );
        std::fs::write(&program_path, program_text).expect("the program is written");
        program_path
    }
}

/// Start the program `process_command` names, and hand back the running
/// process.
///
/// A start that fails with [`ErrorKind::ExecutableFileBusy`] (`ETXTBSY`: a
/// process holds the program file open for writing) is tried again every
/// [`BUSY_PROGRAM_RETRY_INTERVAL_DURATION`].
///
/// # Panics
///
/// Panics when the program file is still busy after
/// [`BUSY_PROGRAM_WAIT_DURATION`], or the start fails for any other reason.
pub fn start_program_process(process_command: &mut Command) -> Child {
    let busy_deadline = Instant::now() + BUSY_PROGRAM_WAIT_DURATION;
    loop {
        match process_command.spawn() {
            Ok(started_process) => return started_process,
            Err(spawn_error) if spawn_error.kind() == ErrorKind::ExecutableFileBusy => {
                assert!(
                    Instant::now() < busy_deadline,
                    "the program at {} was still busy after {BUSY_PROGRAM_WAIT_DURATION:?}",
                    process_command.get_program().to_string_lossy()
                );
                std::thread::sleep(BUSY_PROGRAM_RETRY_INTERVAL_DURATION);
            }
            Err(spawn_error) => panic!(
                "the program at {} starts: {spawn_error}",
                process_command.get_program().to_string_lossy()
            ),
        }
    }
}

/// How many times a program from [`write_printing_program`] that logs to
/// `run_log_path` has run. `0` when the log does not exist.
#[must_use]
pub fn count_program_runs(run_log_path: &Path) -> usize {
    std::fs::read_to_string(run_log_path)
        .map(|run_log_text| run_log_text.lines().count())
        .unwrap_or(0)
}

/// Advertise `session_id` in `runtime_directory` under the token made from
/// `connection_secret` and under `process_id`, the way a session server does
/// every time it binds, and hand back what was written.
///
/// # Panics
///
/// Panics when the endpoint file cannot be written.
pub fn write_session_endpoint_file(
    runtime_directory: &Path,
    session_id: SessionId,
    connection_secret: &str,
    process_id: u32,
) -> EndpointFile {
    let session_endpoint = EndpointFile {
        socket_address: compute_socket_address(runtime_directory, session_id),
        connection_token: ConnectionToken::from_secret(connection_secret),
        process_id,
    };
    session_endpoint
        .write_to_path(&EndpointFile::resolve_endpoint_file_path(
            runtime_directory,
            session_id,
        ))
        .expect("write the session endpoint file");
    session_endpoint
}

/// Write the endpoint file a koshi 0.1.0 window writes for `session_id` in
/// `runtime_directory`, the fields `{socket, token}` alone, and hand back its
/// path.
///
/// # Panics
///
/// Panics when the endpoint file cannot be written.
pub fn write_koshi_0_1_0_window_endpoint_file(
    runtime_directory: &Path,
    session_id: SessionId,
) -> PathBuf {
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let window_endpoint_text = serde_json::json!({
        "socket": compute_socket_address(runtime_directory, session_id),
        "token": "k7QxSecret",
    })
    .to_string();
    std::fs::write(&endpoint_file_path, window_endpoint_text)
        .expect("write the window's endpoint file");
    endpoint_file_path
}

/// The answer a session of koshi 0.2.0 to 0.4.0 gives a frame in this build's
/// envelope, such as this build's Hello: `malformed_request`, in the envelope
/// of that release.
pub const PREVIOUS_RELEASE_MALFORMED_REQUEST_ANSWER_TEXT: &str = r#"{"request_id":null,"result":{"Error":{"code":"malformed_request","message":"unknown field `request_kind`, expected `request_id` or `kind`"}}}"#;

/// The answer a koshi 0.4.0 session gives the Hello of its own envelope.
pub const KOSHI_0_4_0_HELLO_ANSWER_TEXT: &str =
    r#"{"request_id":1,"result":{"Hello":{"protocol_version":3,"version":"0.4.0"}}}"#;

/// The answer a koshi 0.4.0 session gives the Restart of its own envelope.
pub const KOSHI_0_4_0_RESTARTING_ANSWER_TEXT: &str = r#"{"request_id":2,"result":"Restarting"}"#;

/// The answer a koshi 0.2.0 session gives the Hello of its own envelope: it
/// names no build.
pub const KOSHI_0_2_0_HELLO_ANSWER_TEXT: &str =
    r#"{"request_id":1,"result":{"Hello":{"protocol_version":2}}}"#;

/// The answer a koshi 0.2.0 session gives a Restart: `unsupported_kind`.
pub const KOSHI_0_2_0_RESTART_REFUSAL_TEXT: &str = r#"{"request_id":2,"result":{"Error":{"code":"unsupported_kind","message":"this Koshi has no request kind named Restart"}}}"#;

/// Serve the session `session_id` in `runtime_directory` as a session of koshi
/// 0.2.0 to 0.4.0 would: one connection for each entry of
/// `answer_texts_by_connection`, in order. The endpoint file names this
/// process and the token made from `connection_secret`.
///
/// On each connection, the stand-in reads one frame and writes the next
/// answer of the entry, until the entry has no answer left or a read fails.
/// An answer is written whether or not the caller still reads. The connection
/// closes once the caller hung up. Hands back the frames each connection read,
/// as their JSON text.
///
/// # Panics
///
/// Panics when the socket cannot be bound, the endpoint file cannot be
/// written, a caller cannot be accepted, or an answer is not JSON.
pub fn spawn_previous_release_session(
    runtime_directory: &Path,
    session_id: SessionId,
    connection_secret: &str,
    answer_texts_by_connection: Vec<Vec<String>>,
) -> JoinHandle<Vec<Vec<String>>> {
    let session_listener = Listener::bind(&compute_socket_address(runtime_directory, session_id))
        .expect("bind the stand-in session");
    write_session_endpoint_file(
        runtime_directory,
        session_id,
        connection_secret,
        std::process::id(),
    );
    spawn_previous_release_server(session_listener, answer_texts_by_connection)
}

/// Serve the router of `runtime_directory` as a router of koshi 0.2.0 to 0.4.0
/// would, the way [`spawn_previous_release_session`] serves a session. The
/// endpoint file is the one [`write_router_endpoint_file`] writes for
/// `connection_secret`, naming the process id `5000`.
///
/// # Panics
///
/// Panics when the socket cannot be bound, the endpoint file cannot be
/// written, a caller cannot be accepted, or an answer is not JSON.
pub fn spawn_previous_release_router(
    runtime_directory: &Path,
    connection_secret: &str,
    answer_texts_by_connection: Vec<Vec<String>>,
) -> JoinHandle<Vec<Vec<String>>> {
    let router_listener = Listener::bind(&compute_router_socket_address(runtime_directory))
        .expect("bind the stand-in router");
    write_router_endpoint_file(runtime_directory, connection_secret);
    spawn_previous_release_server(router_listener, answer_texts_by_connection)
}

/// Serve one connection on `server_listener` for each entry of
/// `answer_texts_by_connection`, in order, on a thread of its own. Each
/// connection runs as [`spawn_previous_release_session`] describes, and the
/// thread hands back the frames each connection read, as their JSON text.
fn spawn_previous_release_server(
    server_listener: Listener,
    answer_texts_by_connection: Vec<Vec<String>>,
) -> JoinHandle<Vec<Vec<String>>> {
    std::thread::spawn(move || {
        let mut request_texts_by_connection = Vec::new();
        for answer_texts in answer_texts_by_connection {
            let mut server_connection = server_listener.accept().expect("accept the caller");
            let mut request_texts = Vec::new();
            for answer_text in answer_texts {
                let Ok(request_frame) = server_connection.recv::<Box<RawValue>>() else {
                    break;
                };
                request_texts.push(request_frame.get().to_string());
                let answer_frame = RawValue::from_string(answer_text).expect("the answer is JSON");
                let _ = server_connection.send(&answer_frame);
            }
            close_connection_after_peer_hangs_up(server_connection);
            request_texts_by_connection.push(request_texts);
        }
        request_texts_by_connection
    })
}

/// Advertise a router in `runtime_directory` under the token made from
/// `connection_secret` and the process id `5000`, the way a router does each
/// time it starts.
///
/// # Panics
///
/// Panics when the endpoint file cannot be written.
pub fn write_router_endpoint_file(runtime_directory: &Path, connection_secret: &str) {
    EndpointFile {
        socket_address: compute_router_socket_address(runtime_directory),
        connection_token: ConnectionToken::from_secret(connection_secret),
        process_id: 5000,
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("write the router endpoint file");
}

/// Read every byte `connection` receives, and drop it, until the peer hangs
/// up or a read fails. `connection` is closed when this returns.
///
/// Example: a stand-in session wrote a `BadToken` refusal and a
/// `HelloRequired` answer, and the caller read only the refusal. This returns
/// once the caller closed its end, and then closes the stand-in's end.
pub fn close_connection_after_peer_hangs_up(connection: Connection) {
    let (mut raw_reader, _raw_writer) = connection.split_raw();
    let _ = std::io::copy(&mut raw_reader, &mut std::io::sink());
}

/// Take the exclusive update lock in `runtime_directory` the way `koshi
/// update` does, and hand back the file that holds it.
///
/// # Panics
///
/// Panics when the lock file cannot be opened or locked.
pub fn hold_update_lock(runtime_directory: &Path) -> std::fs::File {
    let update_lock_file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(koshi_ipc::endpoint::resolve_update_lock_path(
            runtime_directory,
        ))
        .expect("open the update lock file");
    update_lock_file.lock().expect("take the update lock");
    update_lock_file
}

#[cfg(test)]
mod tests;
