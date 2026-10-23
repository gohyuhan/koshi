//! The `koshi` program run under the name of a clipboard tool, the way a
//! program in a pane runs it, from the arguments to the exit code.
//!
//! Each test links the `koshi` binary under test as `xclip` or `wl-copy` into
//! a folder of stand-ins. A test that needs the real tool writes a fake one of
//! the same name into a second folder that comes after the stand-ins on
//! `PATH`. The fake writes its arguments, one per line, to `arguments.txt` in
//! the test folder, and its standard input to `input.bin`, then runs one last
//! command.
//!
//! The tests that copy inside a session start one with
//! `common::in_process_session::RunningSession`, and run the stand-in with the
//! `KOSHI_*` variables of the session's root pane. The tests of a copy whose
//! `Commit` gets no answer run the stand-in against a fake session that
//! answers the `Prepare` with `Prepared`, reads the `Commit`, then closes the
//! connection. The tests of a session's
//! folder of stand-ins start a `koshi serve-session` process under a fresh test
//! home, with a `PATH` that names one folder holding a fake `xclip`.
//!
//! The stand-in runs on Linux and macOS only, and the tests that run it are
//! built there only. On Windows a session makes no folder of stand-ins, and a
//! new pane's `PATH` names none.

mod common;

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
#[cfg(unix)]
use std::process::{Command, Output};
use std::time::Instant;

#[cfg(unix)]
use koshi_config::layer::{PartialKoshiConfig, PartialPaneClipboardWriteConfig};
#[cfg(unix)]
use koshi_core::ids::PaneId;
use koshi_core::ids::SessionId;
#[cfg(unix)]
use koshi_ipc::endpoint::compute_socket_address;
use koshi_ipc::endpoint::{resolve_clipboard_tool_directory_path, EndpointFile};
#[cfg(unix)]
use koshi_ipc::event::SessionEvent;
#[cfg(unix)]
use koshi_ipc::protocol::{
    IpcRequest, IpcRequestKind, IpcResponse, IpcResult, MAX_CLIPBOARD_COPY_PART_BYTE_COUNT,
};
#[cfg(unix)]
use koshi_ipc::transport::{Connection, Listener};
#[cfg(unix)]
use koshi_test_support::fixtures::{
    build_accepted_hello_result, build_test_runtime_directory, write_session_endpoint_file,
};
use koshi_test_support::fixtures::{start_program_process, write_printing_program};
use tempfile::TempDir;

#[cfg(unix)]
use common::in_process_session::{attach_test_client, forward_session_events, RunningSession};
use common::session_connection::{
    attach_client_on_connection, create_pane, open_session_connection,
};
use common::{
    build_koshi_command_under_home, build_shell_spawn_spec, build_short_test_directory,
    resolve_runtime_directory_under_home, write_test_config, SessionProcess,
    POLL_INTERVAL_DURATION, SESSION_SERVER_NAME, WAIT_DURATION,
};

/// The folder of stand-ins, named as a session names its own.
#[cfg(unix)]
const CLIPBOARD_TOOL_DIRECTORY_NAME: &str =
    "session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.clipboard-tools";

/// The last command of a fake real tool that exits with code 3.
#[cfg(unix)]
const EXIT_WITH_CODE_3_COMMAND: &str = "exit 3";

/// The last command of a fake real tool that ends itself with `SIGTERM`.
#[cfg(unix)]
const END_BY_SIGTERM_COMMAND: &str = "kill -TERM $$";

/// The script a pane runs to write its `PATH` and a line end into
/// `pane-path.tmp` in the test home, then rename that file to
/// `pane-path.txt`. The pane starts in the test home.
#[cfg(unix)]
const WRITE_PANE_PATH_SCRIPT: &str = "printf '%s\\n' \"$PATH\" > \"$HOME/pane-path.tmp\"\n\
                                      /bin/mv \"$HOME/pane-path.tmp\" \"$HOME/pane-path.txt\"\n";

/// The script a pane runs to write its `PATH` and a line end into
/// `pane-path.tmp` in the test home, then rename that file to
/// `pane-path.txt`. The pane starts in the test home.
#[cfg(windows)]
const WRITE_PANE_PATH_SCRIPT: &str =
    ">pane-path.tmp echo %PATH%& move /Y pane-path.tmp pane-path.txt >NUL";

/// The line end that [`WRITE_PANE_PATH_SCRIPT`] writes after the `PATH`.
const PANE_PATH_LINE_END: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// One test folder: a folder of stand-ins that holds a link named after a
/// clipboard tool, such as `xclip`, to the `koshi` binary under test, and an
/// empty folder for the fake real tool.
#[cfg(unix)]
struct StandInFixture {
    /// The test folder. Dropping it removes every file the test made.
    test_directory: TempDir,
    /// The link, named after the clipboard tool, to the `koshi` binary under
    /// test.
    stand_in_path: PathBuf,
    /// The folder of stand-ins.
    clipboard_tool_directory: PathBuf,
    /// The folder the fake real tool goes into.
    real_tool_directory: PathBuf,
}

#[cfg(unix)]
impl StandInFixture {
    /// The `PATH` that lists the folder of stand-ins, then the folder of the
    /// fake real tool.
    fn build_path_entries(&self) -> OsString {
        std::env::join_paths([&self.clipboard_tool_directory, &self.real_tool_directory])
            .expect("no folder holds the separator")
    }

    /// The bytes the fake real tool wrote to `file_name` in the test folder,
    /// or `None` when it wrote no such file.
    fn read_recorded_file(&self, file_name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.test_directory.path().join(file_name)).ok()
    }
}

/// A [`StandInFixture`] whose stand-in is named `tool_name`, and whose real
/// tool folder holds a fake `tool_name` that runs `real_tool_last_command`
/// after it records its arguments and its standard input, or nothing when
/// `real_tool_last_command` is `None`. The fake names every file and program
/// by its absolute path, so it runs with any `PATH`.
#[cfg(unix)]
fn build_stand_in_fixture(tool_name: &str, real_tool_last_command: Option<&str>) -> StandInFixture {
    let test_directory = build_short_test_directory();
    let clipboard_tool_directory = test_directory.path().join(CLIPBOARD_TOOL_DIRECTORY_NAME);
    let real_tool_directory = test_directory.path().join("bin");
    std::fs::create_dir(&clipboard_tool_directory).expect("the stand-in folder is made");
    std::fs::create_dir(&real_tool_directory).expect("the real tool folder is made");
    let stand_in_path = clipboard_tool_directory.join(tool_name);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_koshi"), &stand_in_path)
        .expect("the stand-in link is made");
    if let Some(real_tool_last_command) = real_tool_last_command {
        let real_tool_script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n/bin/cat > '{}'\n{real_tool_last_command}\n",
            test_directory.path().join("arguments.txt").display(),
            test_directory.path().join("input.bin").display(),
        );
        let real_tool_path = real_tool_directory.join(tool_name);
        std::fs::write(&real_tool_path, real_tool_script).expect("the fake real tool is written");
        std::fs::set_permissions(&real_tool_path, std::fs::Permissions::from_mode(0o755))
            .expect("the fake real tool may run");
    }
    StandInFixture {
        test_directory,
        stand_in_path,
        clipboard_tool_directory,
        real_tool_directory,
    }
}

/// Run the stand-in in `stand_in_fixture` with `tool_arguments`,
/// `standard_input_bytes` on its standard input, `path_entries` as its `PATH`,
/// and `pane_variables` as the only other environment variables, and hand
/// back how it ended.
#[cfg(unix)]
fn run_stand_in(
    stand_in_fixture: &StandInFixture,
    tool_arguments: &[&str],
    standard_input_bytes: &[u8],
    path_entries: &OsString,
    pane_variables: &[(&str, String)],
) -> Output {
    let mut stand_in_process = Command::new(&stand_in_fixture.stand_in_path)
        .args(tool_arguments)
        .env_clear()
        .env("PATH", path_entries)
        .envs(pane_variables.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the stand-in starts");
    stand_in_process
        .stdin
        .take()
        .expect("the stand-in reads a pipe")
        .write_all(standard_input_bytes)
        .expect("the stand-in takes its input");
    stand_in_process
        .wait_with_output()
        .expect("the stand-in ends")
}

/// The `KOSHI_*` variables of the pane `pane_id` in the session `session_id`,
/// with `KOSHI_RUNTIME_DIR` naming `runtime_directory`.
#[cfg(unix)]
fn build_pane_variables(
    runtime_directory: &Path,
    session_id: SessionId,
    pane_id: PaneId,
) -> Vec<(&'static str, String)> {
    vec![
        ("KOSHI", "1".to_string()),
        ("KOSHI_SESSION_ID", session_id.to_string()),
        ("KOSHI_PANE_ID", pane_id.to_string()),
        ("KOSHI_RUNTIME_DIR", runtime_directory.display().to_string()),
    ]
}

/// The `KOSHI_*` variables of `session`'s root pane, with `KOSHI_RUNTIME_DIR`
/// naming the session's runtime directory.
#[cfg(unix)]
fn build_root_pane_variables(session: &RunningSession) -> Vec<(&'static str, String)> {
    build_pane_variables(
        session.runtime_directory.path(),
        session.session_id,
        session.list_pane_ids()[0],
    )
}

/// Serve one connection for `session_id` in `runtime_directory` as a session
/// that takes a copy and gives no answer. It answers the Hello, answers the
/// `Prepare` behind the Hello with `Prepared`, reads the `Commit`, then closes
/// the connection. Hands back the session's thread, which ends with the
/// `Prepare` and the `Commit` it read.
#[cfg(unix)]
fn spawn_session_leaving_the_commit_unanswered(
    runtime_directory: &Path,
    session_id: SessionId,
) -> std::thread::JoinHandle<(IpcRequest, IpcRequest)> {
    let session_listener = Listener::bind(&compute_socket_address(runtime_directory, session_id))
        .expect("bind the fake session");
    write_session_endpoint_file(
        runtime_directory,
        session_id,
        "k7QxSecret",
        std::process::id(),
    );
    std::thread::spawn(move || {
        let mut session_connection = session_listener.accept().expect("accept the stand-in");
        let hello_request: IpcRequest = session_connection.recv().expect("read the Hello");
        let prepare_request: IpcRequest = session_connection
            .recv()
            .expect("read the Prepare behind the Hello");
        session_connection
            .send(&IpcResponse {
                request_id: Some(hello_request.request_id),
                answer_result: build_accepted_hello_result(),
            })
            .expect("answer the Hello");
        session_connection
            .send(&IpcResponse {
                request_id: Some(prepare_request.request_id),
                answer_result: IpcResult::Prepared,
            })
            .expect("answer the Prepare");
        let commit_request: IpcRequest = session_connection.recv().expect("read the Commit");
        (prepare_request, commit_request)
    })
}

/// A session whose `pane-clipboard-writes` allows writes.
#[cfg(unix)]
fn start_session_allowing_pane_clipboard_writes() -> RunningSession {
    RunningSession::start_session_with_app_config(Some(PartialKoshiConfig {
        pane_clipboard_writes: Some(PartialPaneClipboardWriteConfig {
            should_allow_writes: Some(true),
            maximum_copy_byte_count: None,
        }),
        ..PartialKoshiConfig::default()
    }))
}

/// Attach to `session` as a client on another machine: the connection's Hello
/// says it carries a caller on another machine, as the router's Hello does.
/// Hands back the session events the client reads.
#[cfg(unix)]
fn attach_client_from_another_machine(
    session: &RunningSession,
) -> std::sync::mpsc::Receiver<SessionEvent> {
    let endpoint_file = session.load_session_endpoint();
    let mut connection =
        Connection::connect(&endpoint_file.socket_address).expect("the socket answers");
    let mut hello_request_kind =
        IpcRequestKind::build_hello_request(endpoint_file.connection_token.clone());
    if let IpcRequestKind::Hello { is_remote, .. } = &mut hello_request_kind {
        *is_remote = true;
    }
    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: hello_request_kind,
        })
        .expect("the session reads the Hello");
    let hello_response: IpcResponse = connection.recv().expect("the session answers the Hello");
    let IpcResult::Hello { .. } = hello_response.answer_result else {
        panic!(
            "the Hello was answered with {:?}",
            hello_response.answer_result
        );
    };
    let _ = attach_client_on_connection(&mut connection, session.session_id);
    let (session_frame_reader, client_frame_writer) = connection.split();
    forward_session_events(session_frame_reader, client_frame_writer)
}

/// The bytes of the next host write among `session_events`, the session
/// events a client reads. Every other event before it is passed over.
///
/// # Panics
/// When no host write comes within [`WAIT_DURATION`].
#[cfg(unix)]
fn wait_for_next_host_write(session_events: &std::sync::mpsc::Receiver<SessionEvent>) -> Vec<u8> {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        let session_event = session_events
            .recv_timeout(wait_deadline.saturating_duration_since(Instant::now()))
            .expect("the session writes the copy to the client's terminal");
        if let SessionEvent::HostWrite { host_write_bytes } = session_event {
            return host_write_bytes;
        }
    }
}

#[cfg(unix)]
#[test]
fn a_read_runs_the_real_tool_in_place_with_its_arguments_and_input() {
    let stand_in_fixture = build_stand_in_fixture("xclip", Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["-o", "-selection", "clipboard"],
        b"unread",
        &stand_in_fixture.build_path_entries(),
        &[],
    );

    assert_eq!(stand_in_output.status.code(), Some(3));
    assert_eq!(
        stand_in_fixture.read_recorded_file("arguments.txt"),
        Some(b"-o\n-selection\nclipboard\n".to_vec())
    );
    assert_eq!(
        stand_in_fixture.read_recorded_file("input.bin"),
        Some(b"unread".to_vec())
    );
}

#[cfg(unix)]
#[test]
fn a_copy_outside_a_session_runs_the_real_tool_with_every_byte_of_the_input() {
    let stand_in_fixture = build_stand_in_fixture("xclip", Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["-selection", "clipboard", "-rmlastnl"],
        b"hi\n",
        &stand_in_fixture.build_path_entries(),
        &[],
    );

    assert_eq!(stand_in_output.status.code(), Some(3));
    assert_eq!(
        stand_in_fixture.read_recorded_file("arguments.txt"),
        Some(b"-selection\nclipboard\n-rmlastnl\n".to_vec())
    );
    assert_eq!(
        stand_in_fixture.read_recorded_file("input.bin"),
        Some(b"hi\n".to_vec())
    );
}

#[cfg(unix)]
#[test]
fn a_copy_started_by_a_program_that_ignores_child_exits_exits_with_the_real_tools_code() {
    use std::os::unix::process::CommandExt;

    let stand_in_fixture = build_stand_in_fixture("xclip", Some(EXIT_WITH_CODE_3_COMMAND));
    let mut stand_in_command = Command::new(&stand_in_fixture.stand_in_path);
    stand_in_command
        .env_clear()
        .env("PATH", stand_in_fixture.build_path_entries())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `signal` is async-signal-safe, and the closure touches no other
    // state of the forked child.
    unsafe {
        stand_in_command.pre_exec(|| {
            libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            Ok(())
        });
    }

    let stand_in_output = stand_in_command.output().expect("the stand-in runs");

    assert_eq!(
        stand_in_output.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&stand_in_output.stderr)
    );
    assert_eq!(
        stand_in_fixture.read_recorded_file("input.bin"),
        Some(Vec::new())
    );
}

#[cfg(unix)]
#[test]
fn a_stand_in_with_no_real_tool_on_path_exits_with_127() {
    let stand_in_fixture = build_stand_in_fixture("xclip", None);

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["-o"],
        b"",
        &stand_in_fixture.build_path_entries(),
        &[],
    );

    assert_eq!(stand_in_output.status.code(), Some(127));
    assert_eq!(
        String::from_utf8_lossy(&stand_in_output.stderr),
        "koshi: no xclip found on PATH outside koshi's stand-in folder\n"
    );
}

#[cfg(unix)]
#[test]
fn a_copy_whose_real_tool_a_signal_ends_exits_with_128_plus_the_signal_number() {
    let stand_in_fixture = build_stand_in_fixture("xclip", Some(END_BY_SIGTERM_COMMAND));

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &[],
        b"hi",
        &stand_in_fixture.build_path_entries(),
        &[],
    );

    assert_eq!(stand_in_output.status.signal(), None);
    assert_eq!(stand_in_output.status.code(), Some(128 + 15));
    assert_eq!(
        stand_in_fixture.read_recorded_file("input.bin"),
        Some(b"hi".to_vec())
    );
}

#[cfg(unix)]
#[test]
fn a_copy_in_a_pane_only_a_client_on_another_machine_focuses_reaches_that_client_alone() {
    let session = start_session_allowing_pane_clipboard_writes();
    let remote_session_events = attach_client_from_another_machine(&session);
    let stand_in_fixture = build_stand_in_fixture("xclip", Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["-selection", "clipboard", "-rmlastnl"],
        b"hi\n",
        &stand_in_fixture.build_path_entries(),
        &build_root_pane_variables(&session),
    );

    assert_eq!(stand_in_output.status.code(), Some(0));
    assert_eq!(stand_in_fixture.read_recorded_file("arguments.txt"), None);
    assert_eq!(
        wait_for_next_host_write(&remote_session_events),
        b"\x1b]52;c;aGk=\x07"
    );
}

#[cfg(unix)]
#[test]
fn a_copy_that_is_not_utf8_runs_the_real_tool_and_reaches_no_client() {
    let session = start_session_allowing_pane_clipboard_writes();
    let remote_session_events = attach_client_from_another_machine(&session);
    let stand_in_fixture = build_stand_in_fixture("xclip", Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["-selection", "clipboard"],
        b"hi\xff",
        &stand_in_fixture.build_path_entries(),
        &build_root_pane_variables(&session),
    );

    assert_eq!(stand_in_output.status.code(), Some(3));
    assert_eq!(
        stand_in_fixture.read_recorded_file("input.bin"),
        Some(b"hi\xff".to_vec())
    );
    // The UTF-8 copy made next is the first copy the client gets.
    let utf8_stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["-selection", "clipboard"],
        b"ok",
        &stand_in_fixture.build_path_entries(),
        &build_root_pane_variables(&session),
    );
    assert_eq!(utf8_stand_in_output.status.code(), Some(0));
    assert_eq!(
        wait_for_next_host_write(&remote_session_events),
        b"\x1b]52;c;b2s=\x07"
    );
}

#[cfg(unix)]
#[test]
fn a_copy_past_max_copy_bytes_runs_the_real_tool_with_every_byte_read_then_the_rest() {
    // The first part holds 12582774 bytes, past the 8388608 of
    // `max-copy-bytes`, and 4096 bytes follow the one byte read behind it.
    let session = start_session_allowing_pane_clipboard_writes();
    let _remote_session_events = attach_client_from_another_machine(&session);
    let stand_in_fixture = build_stand_in_fixture("xclip", Some(EXIT_WITH_CODE_3_COMMAND));
    let input_bytes = vec![b'a'; MAX_CLIPBOARD_COPY_PART_BYTE_COUNT + 4097];

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["-selection", "clipboard"],
        &input_bytes,
        &stand_in_fixture.build_path_entries(),
        &build_root_pane_variables(&session),
    );

    assert_eq!(stand_in_output.status.code(), Some(3));
    assert!(
        stand_in_fixture.read_recorded_file("input.bin") == Some(input_bytes),
        "the real tool reads every byte of the input"
    );
}

#[cfg(unix)]
#[test]
fn a_copy_in_a_pane_a_client_on_this_machine_focuses_runs_the_real_tool_with_every_byte() {
    let session = start_session_allowing_pane_clipboard_writes();
    let _local_client = attach_test_client(&session);
    let stand_in_fixture = build_stand_in_fixture("xclip", Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["-selection", "clipboard", "-rmlastnl"],
        b"hi\n",
        &stand_in_fixture.build_path_entries(),
        &build_root_pane_variables(&session),
    );

    assert_eq!(stand_in_output.status.code(), Some(3));
    assert_eq!(
        stand_in_fixture.read_recorded_file("input.bin"),
        Some(b"hi\n".to_vec())
    );
}

#[cfg(unix)]
#[test]
fn a_copy_of_standard_input_whose_commit_gets_no_answer_exits_with_1_and_runs_no_real_tool() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let session_thread =
        spawn_session_leaving_the_commit_unanswered(runtime_directory.path(), session_id);
    let stand_in_fixture = build_stand_in_fixture("xclip", Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["-selection", "clipboard"],
        b"hi",
        &stand_in_fixture.build_path_entries(),
        &build_pane_variables(runtime_directory.path(), session_id, pane_id),
    );

    let (prepare_request, commit_request) = session_thread.join().expect("the fake session ends");
    assert_eq!(
        prepare_request,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Prepare(Box::new(IpcRequestKind::ClipboardCopy {
                pane_id,
                clipboard_targets: "c".to_string(),
                copied_bytes: b"hi".to_vec(),
            })),
        }
    );
    assert_eq!(
        commit_request,
        IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Commit,
        }
    );
    assert_eq!(stand_in_output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&stand_in_output.stderr),
        "koshi: xclip: IPC unavailable: the session took the request and gave no answer \
         (ipc peer disconnected); it may still run, so check before you run it again\n"
    );
    assert_eq!(stand_in_fixture.read_recorded_file("arguments.txt"), None);
    assert_eq!(stand_in_fixture.read_recorded_file("input.bin"), None);
}

#[cfg(unix)]
#[test]
fn a_copy_of_argument_text_whose_commit_gets_no_answer_exits_with_1_and_runs_no_real_tool() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let session_thread =
        spawn_session_leaving_the_commit_unanswered(runtime_directory.path(), session_id);
    let stand_in_fixture = build_stand_in_fixture("wl-copy", Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &stand_in_fixture,
        &["hi"],
        b"",
        &stand_in_fixture.build_path_entries(),
        &build_pane_variables(runtime_directory.path(), session_id, pane_id),
    );

    let (prepare_request, commit_request) = session_thread.join().expect("the fake session ends");
    assert_eq!(
        prepare_request,
        IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Prepare(Box::new(IpcRequestKind::ClipboardCopy {
                pane_id,
                clipboard_targets: "c".to_string(),
                copied_bytes: b"hi".to_vec(),
            })),
        }
    );
    assert_eq!(
        commit_request,
        IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Commit,
        }
    );
    assert_eq!(stand_in_output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&stand_in_output.stderr),
        "koshi: wl-copy: IPC unavailable: the session took the request and gave no answer \
         (ipc peer disconnected); it may still run, so check before you run it again\n"
    );
    assert_eq!(stand_in_fixture.read_recorded_file("arguments.txt"), None);
    assert_eq!(stand_in_fixture.read_recorded_file("input.bin"), None);
}

/// A `koshi serve-session` process under a fresh test home, whose `PATH`
/// names only `tools`, a folder in the test home that holds a fake `xclip`.
struct SessionWithFakeTool {
    /// The test home. Dropping it removes every file the test made.
    home_directory: TempDir,
    /// The runtime directory the session serves.
    runtime_directory: PathBuf,
    /// The session the process serves.
    session_id: SessionId,
    /// The endpoint file the session advertises.
    session_endpoint: EndpointFile,
    /// The session server process. Dropping it ends the process.
    session_process: SessionProcess,
    /// The folder that holds the fake `xclip`, and the session's whole `PATH`.
    #[cfg(unix)]
    tool_directory: PathBuf,
}

/// Start a session server under a fresh test home whose `koshi.kdl` holds
/// `config_text`, and return once it serves its socket.
fn start_session_with_fake_tool(config_text: &str) -> SessionWithFakeTool {
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), config_text);
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let tool_directory = home_directory.path().join("tools");
    std::fs::create_dir(&tool_directory).expect("the tool folder is made");
    write_printing_program(
        &tool_directory,
        "xclip",
        &home_directory.path().join("xclip-runs.log"),
        "",
    );
    let session_id = SessionId::new();
    let mut session_process = SessionProcess {
        child_process: start_program_process(
            build_koshi_command_under_home(home_directory.path())
                .env("PATH", &tool_directory)
                .arg("serve-session")
                .arg(session_id.to_string())
                .arg(SESSION_SERVER_NAME)
                .arg("--runtime-dir")
                .arg(&runtime_directory)
                .stdout(Stdio::null()),
        ),
        runtime_directory: runtime_directory.clone(),
        session_id,
    };
    let session_endpoint =
        session_process.wait_for_session_endpoint(&runtime_directory, session_id);
    SessionWithFakeTool {
        home_directory,
        runtime_directory,
        session_id,
        session_endpoint,
        session_process,
        #[cfg(unix)]
        tool_directory,
    }
}

/// Open a pane in `session` that runs [`WRITE_PANE_PATH_SCRIPT`], and hand
/// back the `PATH` it wrote, without its line end, once `pane-path.txt` is
/// in the test home.
///
/// # Panics
/// When the file is not there within [`WAIT_DURATION`], or does not end with
/// [`PANE_PATH_LINE_END`].
fn read_new_pane_path(session: &SessionWithFakeTool) -> String {
    let mut client_connection = open_session_connection(&session.session_endpoint);
    let client_id = attach_client_on_connection(&mut client_connection, session.session_id);
    let mut control_connection = open_session_connection(&session.session_endpoint);
    create_pane(
        &mut control_connection,
        session.session_id,
        client_id,
        Some(build_shell_spawn_spec(WRITE_PANE_PATH_SCRIPT)),
    );
    let pane_path_file = session.home_directory.path().join("pane-path.txt");
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Ok(pane_path_text) = std::fs::read_to_string(&pane_path_file) {
            return pane_path_text
                .strip_suffix(PANE_PATH_LINE_END)
                .expect("the pane ends its PATH with a line end")
                .to_string();
        }
        assert!(
            Instant::now() < wait_deadline,
            "the pane never wrote its PATH"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
}

/// Run `koshi kill-session` for `session` and wait until its server process
/// ends.
///
/// # Panics
/// When the command fails or the process is still running after
/// [`WAIT_DURATION`].
fn kill_session(session: &mut SessionWithFakeTool) {
    let kill_output = build_koshi_command_under_home(session.home_directory.path())
        .arg("kill-session")
        .arg(session.session_id.to_string())
        .output()
        .expect("kill-session runs");
    assert!(
        kill_output.status.success(),
        "kill-session failed: {}",
        String::from_utf8_lossy(&kill_output.stderr)
    );
    assert!(
        session.session_process.wait_for_session_server_exit(),
        "the session server is still running; {}",
        session.session_process.format_process_status()
    );
}

/// The name of each entry in `directory`, in name order.
#[cfg(unix)]
fn list_entry_names(directory: &Path) -> Vec<String> {
    let mut entry_names: Vec<String> = std::fs::read_dir(directory)
        .expect("the folder reads")
        .map(|directory_entry| {
            directory_entry
                .expect("the entry reads")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    entry_names.sort();
    entry_names
}

#[cfg(unix)]
#[test]
fn a_session_allowing_writes_puts_its_stand_ins_first_on_a_new_panes_path_until_it_quits() {
    let mut session =
        start_session_with_fake_tool("version 2\npane-clipboard-writes {\n    allow #true\n}\n");
    let clipboard_tool_directory =
        resolve_clipboard_tool_directory_path(&session.runtime_directory, session.session_id);

    assert_eq!(
        list_entry_names(&clipboard_tool_directory),
        vec!["xclip".to_string()]
    );
    assert_eq!(
        std::fs::canonicalize(clipboard_tool_directory.join("xclip"))
            .expect("the link leads to a file"),
        std::fs::canonicalize(env!("CARGO_BIN_EXE_koshi")).expect("the binary is there")
    );
    assert_eq!(
        std::fs::metadata(&clipboard_tool_directory)
            .expect("the folder is there")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        read_new_pane_path(&session),
        format!(
            "{}:{}",
            clipboard_tool_directory.display(),
            session.tool_directory.display()
        )
    );

    kill_session(&mut session);

    assert!(!clipboard_tool_directory.exists());
}

#[cfg(unix)]
#[test]
fn a_session_with_writes_off_makes_no_stand_ins_and_leaves_a_new_panes_path_alone() {
    let mut session = start_session_with_fake_tool("version 2\n");
    let clipboard_tool_directory =
        resolve_clipboard_tool_directory_path(&session.runtime_directory, session.session_id);

    assert!(!clipboard_tool_directory.exists());
    assert_eq!(
        read_new_pane_path(&session),
        session.tool_directory.display().to_string()
    );

    kill_session(&mut session);
}

#[cfg(windows)]
#[test]
fn a_session_on_windows_allowing_writes_makes_no_stand_ins_and_puts_none_on_a_new_panes_path() {
    let mut session =
        start_session_with_fake_tool("version 2\npane-clipboard-writes {\n    allow #true\n}\n");
    let clipboard_tool_directory =
        resolve_clipboard_tool_directory_path(&session.runtime_directory, session.session_id);

    let pane_path = read_new_pane_path(&session);

    assert!(!clipboard_tool_directory.exists());
    let pane_path_folders: Vec<PathBuf> = std::env::split_paths(&pane_path).collect();
    assert!(
        !pane_path_folders.contains(&clipboard_tool_directory),
        "the pane's PATH {pane_path:?} names the folder of stand-ins"
    );

    kill_session(&mut session);
}
