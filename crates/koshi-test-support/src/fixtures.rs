//! Shared test fixtures.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use koshi_core::command::{NewPaneArgs, NewPanePlacement};
use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::SessionId;
use koshi_core::key::{
    BindingModifierFlags, KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags,
};
use koshi_core::notification::{NotificationQueue, NotificationQueueOwner};
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_ipc::endpoint::{
    compute_socket_address, resolve_clipboard_tool_directory_path, EndpointFile,
};
use koshi_ipc::handshake::{Handshake, Peer};
use koshi_ipc::image_chunk::{build_image_chunk_frame_head, ImageChunkFrame};
use koshi_ipc::painted_update::{decode_session_binary_frame, SessionBinaryFrame};
use koshi_ipc::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult, ProcessImageId, PROTOCOL_VERSION,
};
use koshi_ipc::router::{compute_router_socket_address, resolve_router_endpoint_path};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter, Listener};
use serde_json::value::RawValue;
use tempfile::TempDir;
use uuid::Uuid;

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

/// The cell rect at `(origin_column, origin_row)`, `column_count` columns wide
/// and `row_count` rows tall: `build_cell_rect(2, 1, 10, 5)` covers columns `2`
/// to `11` of rows `1` to `5`.
#[must_use]
pub fn build_cell_rect(
    origin_column: u16,
    origin_row: u16,
    column_count: u16,
    row_count: u16,
) -> Rect {
    Rect::from_origin_and_size(
        Point {
            column: origin_column,
            row: origin_row,
        },
        Size {
            column_count,
            row_count,
        },
    )
}

/// The UUID `00000000-0000-0000-0000-0000000000NN`, where `NN` is
/// `suffix_number` in two decimal digits: `4` gives
/// `00000000-0000-0000-0000-000000000004`, and `12` gives
/// `00000000-0000-0000-0000-000000000012`.
///
/// # Panics
///
/// Panics when `suffix_number` is above `99`.
#[must_use]
pub fn build_test_uuid_with_suffix(suffix_number: u8) -> Uuid {
    Uuid::parse_str(&format!(
        "00000000-0000-0000-0000-0000000000{suffix_number:02}"
    ))
    .expect("literal UUID parses")
}

/// An empty notification queue of `owner` at revision 0, in the epoch
/// `00000000-0000-0000-0000-000000000001`. Two calls with one owner give equal
/// queues.
///
/// # Panics
///
/// Never: the queue's wire form is fixed.
#[must_use]
pub fn build_fixed_epoch_notification_queue(owner: NotificationQueueOwner) -> NotificationQueue {
    serde_json::from_value(serde_json::json!({
        "owner": owner,
        "epoch": build_test_uuid_with_suffix(1),
        "revision": 0,
        "queued_entries": []
    }))
    .expect("an empty queue decodes")
}

/// A `new-pane` request with nothing chosen: the focused pane splits
/// rightward and runs the default shell.
#[must_use]
pub fn build_new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        placement: NewPanePlacement::Split {
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
        },
        working_directory: None,
        spawn_spec: None,
        client_id: None,
    }
}

/// The spawn spec a fake child launches with: `/bin/zsh`, no arguments, no
/// working directory, and no environment variables.
#[must_use]
pub fn build_spawn_spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        arguments: Vec::new(),
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Zsh,
    }
}

/// `byte_count` bytes that count from `0` to `250` and then start again at
/// `0`: the byte at index `n` is `n % 251`. `build_counting_bytes(253)` ends
/// with `249, 250, 0, 1`.
#[must_use]
pub fn build_counting_bytes(byte_count: usize) -> Vec<u8> {
    (0..byte_count)
        .map(|byte_index| u8::try_from(byte_index % 251).expect("a value below 251 fits in u8"))
        .collect()
}

/// The image chunk frame of `chunk_bytes` at `byte_offset` of the image
/// `image_transfer_id`, read back through
/// [`decode_session_binary_frame`] as a client reads it.
///
/// # Panics
/// When the built frame does not decode as an image chunk.
#[must_use]
pub fn build_image_chunk_frame(
    image_transfer_id: u64,
    byte_offset: u64,
    is_last_chunk: bool,
    chunk_bytes: &[u8],
) -> ImageChunkFrame {
    let mut frame_payload =
        build_image_chunk_frame_head(image_transfer_id, byte_offset, is_last_chunk).to_vec();
    frame_payload.extend_from_slice(chunk_bytes);
    let Ok(SessionBinaryFrame::ImageChunk(image_chunk_frame)) =
        decode_session_binary_frame(frame_payload)
    else {
        panic!("an image chunk frame built from its own head decodes");
    };
    image_chunk_frame
}

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

/// Make the folder of stand-in clipboard tools of `session_id` in
/// `runtime_directory`, holding one empty file named `xclip`, and hand back
/// the folder's path.
///
/// # Panics
///
/// Panics when the folder or the file cannot be written.
pub fn write_clipboard_tool_directory(runtime_directory: &Path, session_id: SessionId) -> PathBuf {
    let clipboard_tool_directory =
        resolve_clipboard_tool_directory_path(runtime_directory, session_id);
    std::fs::create_dir(&clipboard_tool_directory).expect("the folder is made");
    std::fs::write(clipboard_tool_directory.join("xclip"), b"").expect("the stand-in is written");
    clipboard_tool_directory
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

/// Serve the session `session_id` in `runtime_directory` as a session that
/// listens before it writes its endpoint file, on a thread of its own:
///
/// 1. Bind the session's socket, and write its endpoint file under the token
///    made from `stale_connection_secret`. Both are done before this returns.
/// 2. Refuse the first caller's Hello with `BadToken`: the session holds the
///    token made from `connection_secret`.
/// 3. Wait 200 ms after the first caller hangs up, then write the endpoint
///    file under the token made from `connection_secret`.
/// 4. Accept the second caller's Hello and answer it with build `9.9.9`.
///    Answer each request after it with what `answer_request` gives for its
///    kind, and for the held kind on a `Commit`.
///
/// A request behind a refused Hello gets the `HelloRequired` refusal. Each
/// connection ends when its caller hangs up, and the thread ends after the
/// second. The endpoint file names this test process.
///
/// # Panics
///
/// Panics when the socket cannot be bound, an endpoint file cannot be
/// written, or a caller cannot be accepted.
pub fn spawn_session_listening_before_it_advertises(
    runtime_directory: &Path,
    session_id: SessionId,
    stale_connection_secret: &str,
    connection_secret: &str,
    mut answer_request: impl FnMut(&IpcRequestKind) -> IpcResult + Send + 'static,
) -> JoinHandle<()> {
    let session_listener = Listener::bind(&compute_socket_address(runtime_directory, session_id))
        .expect("bind the stand-in session");
    write_session_endpoint_file(
        runtime_directory,
        session_id,
        stale_connection_secret,
        std::process::id(),
    );
    let advertising_runtime_directory = runtime_directory.to_path_buf();
    let session_connection_secret = connection_secret.to_string();
    std::thread::spawn(move || {
        let session_connection_token = ConnectionToken::from_secret(&session_connection_secret);
        answer_session_requests(
            &session_listener,
            session_connection_token.clone(),
            &mut answer_request,
        );
        std::thread::sleep(Duration::from_millis(200));
        write_session_endpoint_file(
            &advertising_runtime_directory,
            session_id,
            &session_connection_secret,
            std::process::id(),
        );
        answer_session_requests(
            &session_listener,
            session_connection_token,
            &mut answer_request,
        );
    })
}

/// Serve the session `session_id` in `runtime_directory` as a session that
/// replaces its process image while its first caller waits, on a thread of
/// its own:
///
/// 1. Bind the session's socket, and write its endpoint file under the token
///    made from `replaced_connection_secret`. Both are done before this
///    returns.
/// 2. Serve the first caller with `answer_before_replacement`: an accepted
///    Hello gets build `9.9.9` and this process's image, a `Prepare` is
///    answered `Prepared`, a `Commit` gets what the answer function gives for
///    the held request's kind, and every other request gets what it gives for
///    its own kind.
/// 3. Once that caller hangs up, write the endpoint file under the token made
///    from `connection_secret`, as the new image does when it binds.
/// 4. Serve each caller after it with `answer_after_replacement`, the same
///    way, until one presents that token in an accepted Hello. A caller that
///    read the endpoint file before step 3 presents the old token and is
///    refused.
///
/// The thread ends after the caller with the accepted Hello hangs up. The
/// endpoint file names this test process.
///
/// Example: an `answer_before_replacement` that gives
/// [`build_restarting_refusal`] for a `SubmitCommand` refuses the first
/// caller's `Commit`, and the second caller's command runs.
///
/// # Panics
///
/// Panics when the socket cannot be bound, an endpoint file cannot be
/// written, or a caller cannot be accepted.
pub fn spawn_session_replacing_its_image(
    runtime_directory: &Path,
    session_id: SessionId,
    replaced_connection_secret: &str,
    connection_secret: &str,
    mut answer_before_replacement: impl FnMut(&IpcRequestKind) -> IpcResult + Send + 'static,
    mut answer_after_replacement: impl FnMut(&IpcRequestKind) -> IpcResult + Send + 'static,
) -> JoinHandle<()> {
    let session_listener = Listener::bind(&compute_socket_address(runtime_directory, session_id))
        .expect("bind the stand-in session");
    write_session_endpoint_file(
        runtime_directory,
        session_id,
        replaced_connection_secret,
        std::process::id(),
    );
    let advertising_runtime_directory = runtime_directory.to_path_buf();
    let replaced_connection_token = ConnectionToken::from_secret(replaced_connection_secret);
    let connection_secret = connection_secret.to_string();
    std::thread::spawn(move || {
        answer_session_requests(
            &session_listener,
            replaced_connection_token,
            &mut answer_before_replacement,
        );
        write_session_endpoint_file(
            &advertising_runtime_directory,
            session_id,
            &connection_secret,
            std::process::id(),
        );
        while !answer_session_requests(
            &session_listener,
            ConnectionToken::from_secret(&connection_secret),
            &mut answer_after_replacement,
        ) {}
    })
}

/// The refusal a session answers a request with once it has started to
/// replace its process image: `Restarting`, reading `the session is
/// restarting; ask again in a moment`.
#[must_use]
pub fn build_restarting_refusal() -> IpcResult {
    IpcResult::Error(IpcErrorPayload {
        code: IpcErrorCode::Restarting,
        message: "the session is restarting; ask again in a moment".to_string(),
    })
}

/// The answer an accepted session Hello gets: [`PROTOCOL_VERSION`], the
/// workspace build version, and this process's image, as
/// [`ProcessImageId::get_own`] gives it.
#[must_use]
pub fn build_accepted_hello_result() -> IpcResult {
    IpcResult::Hello {
        protocol_version: PROTOCOL_VERSION,
        build_version: env!("CARGO_PKG_VERSION").to_string(),
        process_image_id: ProcessImageId::get_own(),
    }
}

/// Accept one caller on `session_listener` and answer each request it sends
/// until it hangs up, behind the gate a session holding
/// `session_connection_token` keeps for a caller of its own user. An accepted
/// Hello gets build `9.9.9` and this process's image, as
/// [`ProcessImageId::get_own`] gives it, and a refused request gets the gate's
/// refusal. A `Prepare` is answered `Prepared` and its request is held. A
/// `Commit` gets what `answer_request` gives for the held request's kind, and
/// a `RequestFailed` refusal when nothing is held. Every other request gets
/// what `answer_request` gives for its kind.
///
/// Gives whether the caller's Hello was accepted.
fn answer_session_requests(
    session_listener: &Listener,
    session_connection_token: ConnectionToken,
    answer_request: &mut impl FnMut(&IpcRequestKind) -> IpcResult,
) -> bool {
    let mut session_connection = session_listener.accept().expect("accept the caller");
    let mut session_handshake = Handshake::from_expected_token_and_peer(
        session_connection_token,
        Peer::Local {
            is_same_user: true,
            is_other_user_access_allowed: false,
        },
    );
    let mut prepared_request_kind: Option<IpcRequestKind> = None;
    while let Ok(ipc_request) = session_connection.recv::<IpcRequest>() {
        let answer_result = match session_handshake.validate_request_kind(&ipc_request.request_kind)
        {
            Err(refusal) => IpcResult::Error(refusal),
            Ok(()) => match ipc_request.request_kind {
                IpcRequestKind::Hello { .. } => IpcResult::Hello {
                    protocol_version: session_handshake
                        .get_agreed_protocol_version()
                        .expect("an accepted Hello settles a version"),
                    build_version: "9.9.9".to_string(),
                    process_image_id: ProcessImageId::get_own(),
                },
                IpcRequestKind::Prepare(held_request_kind) => {
                    prepared_request_kind = Some(*held_request_kind);
                    IpcResult::Prepared
                }
                IpcRequestKind::Commit => match prepared_request_kind.take() {
                    Some(held_request_kind) => answer_request(&held_request_kind),
                    None => IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::RequestFailed,
                        message: "no request waits for a Commit on this connection".to_string(),
                    }),
                },
                request_kind => answer_request(&request_kind),
            },
        };
        // A send to a caller that has hung up fails, and the failure is
        // dropped.
        let _ = session_connection.send(&IpcResponse {
            request_id: Some(ipc_request.request_id),
            answer_result,
        });
    }
    session_handshake.get_agreed_protocol_version().is_some()
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

/// Send `ipc_result` on `connection` as the answer to the request numbered
/// `request_id`.
///
/// # Panics
///
/// Panics when the answer cannot be written.
pub fn send_ipc_reply(connection: &mut Connection, request_id: u64, ipc_result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            answer_result: ipc_result,
        })
        .expect("send scripted reply");
}

/// Send `ipc_request`'s kind inside a `Prepare` on `connection`, under
/// `ipc_request`'s request id.
///
/// # Panics
///
/// Panics when the `Prepare` cannot be written.
pub fn send_prepare_request(connection: &mut Connection, ipc_request: IpcRequest) {
    connection
        .send(&IpcRequest {
            request_id: ipc_request.request_id,
            request_kind: IpcRequestKind::Prepare(Box::new(ipc_request.request_kind)),
        })
        .expect("send the Prepare");
}

/// Read the `Prepared` answer numbered `request_id` on `connection`, then send
/// the `Commit` numbered `request_id`.
///
/// # Panics
///
/// Panics when the answer cannot be read, is not `Prepared` under
/// `request_id`, or the `Commit` cannot be written.
#[track_caller]
pub fn read_prepared_and_send_commit(connection: &mut Connection, request_id: u64) {
    let prepare_reply: IpcResponse = connection.recv().expect("the Prepare is answered");
    assert_eq!(
        prepare_reply,
        IpcResponse {
            request_id: Some(request_id),
            answer_result: IpcResult::Prepared,
        }
    );
    connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Commit,
        })
        .expect("send the Commit");
}

/// Send `ipc_request` with [`send_prepare_request`], then
/// [`read_prepared_and_send_commit`] under its request id. The held request's
/// answer is the next frame on `connection`, under that id.
///
/// # Panics
///
/// Panics as those two functions state.
#[track_caller]
pub fn send_committed_request(connection: &mut Connection, ipc_request: IpcRequest) {
    let request_id = ipc_request.request_id;
    send_prepare_request(connection, ipc_request);
    read_prepared_and_send_commit(connection, request_id);
}

/// Send `ipc_request` through `frame_writer` as a `Prepare`, read its
/// `Prepared` answer from `frame_reader`, then send its `Commit`: the
/// [`send_committed_request`] steps on the two halves of one link. The held
/// request's answer is the next frame `frame_reader` reads, under the request
/// id of `ipc_request`.
///
/// # Panics
///
/// Panics when a frame cannot be written or read, or when the answer to the
/// `Prepare` is not `Prepared` under the request id of `ipc_request`.
#[track_caller]
pub fn send_committed_request_through_halves(
    frame_reader: &mut FrameReader,
    frame_writer: &mut FrameWriter,
    ipc_request: IpcRequest,
) {
    let request_id = ipc_request.request_id;
    frame_writer
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Prepare(Box::new(ipc_request.request_kind)),
        })
        .expect("send the Prepare");
    let prepare_reply: IpcResponse = frame_reader.recv().expect("the Prepare is answered");
    assert_eq!(
        prepare_reply,
        IpcResponse {
            request_id: Some(request_id),
            answer_result: IpcResult::Prepared,
        }
    );
    frame_writer
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Commit,
        })
        .expect("send the Commit");
}

/// The report that a client painted update `update_sequence`, under request
/// id `update_sequence`.
#[must_use]
pub fn build_paint_report_request(update_sequence: u64) -> IpcRequest {
    IpcRequest {
        request_id: update_sequence,
        request_kind: IpcRequestKind::PaintedUpdate { update_sequence },
    }
}

/// Open one local socket connection at a fresh address and return its two
/// ends: the one that connected, then the one that accepted. On Unix the
/// address is a socket file under `/tmp`. On Windows it is a pipe name. A new
/// session id makes each address unique.
///
/// # Panics
///
/// Panics when the address cannot be bound, connected to, or accepted on.
#[must_use]
pub fn connect_test_socket_pair() -> (Connection, Connection) {
    #[cfg(unix)]
    let socket_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let socket_directory = std::env::temp_dir();
    let socket_address = compute_socket_address(&socket_directory, SessionId::new());
    let listener = Listener::bind(&socket_address).expect("bind the control socket");
    let accept_thread =
        std::thread::spawn(move || listener.accept().expect("accept the connection"));
    let client_connection =
        Connection::connect(&socket_address).expect("connect to the control socket");
    let session_connection = accept_thread.join().expect("the accepting thread finished");
    (client_connection, session_connection)
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
