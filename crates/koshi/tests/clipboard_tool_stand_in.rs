//! The `koshi` program run under the name of a clipboard tool, the way a
//! program in a pane runs it, from the arguments to the exit code.
//!
//! Each test links the `koshi` binary under test as `xclip` into a folder of
//! stand-ins. A test that needs the real `xclip` writes a fake one into a
//! second folder that comes after the stand-ins on `PATH`. The fake writes its
//! arguments, one per line, to `arguments.txt` in the test folder, and its
//! standard input to `input.bin`, then runs one last command.
//!
//! The tests that copy inside a session start one with
//! `common::in_process_session::RunningSession`, and run the stand-in with the
//! `KOSHI_*` variables of the session's root pane. The tests of a session's
//! folder of stand-ins start a `koshi serve-session` process under a fresh test
//! home, with a `PATH` that names one folder holding a fake `xclip`.
#![cfg(unix)]

mod common;

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use koshi_config::layer::{PartialKoshiConfig, PartialPaneClipboardWriteConfig};
use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::{resolve_clipboard_tool_directory_path, EndpointFile};
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{IpcRequest, IpcRequestKind, IpcResponse, IpcResult};
use koshi_ipc::transport::Connection;
use koshi_test_support::fixtures::start_program_process;
use tempfile::TempDir;

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
const CLIPBOARD_TOOL_DIRECTORY_NAME: &str =
    "session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.clipboard-tools";

/// The last command of a fake real `xclip` that exits with code 3.
const EXIT_WITH_CODE_3_COMMAND: &str = "exit 3";

/// The last command of a fake real `xclip` that ends itself with `SIGTERM`.
const END_BY_SIGTERM_COMMAND: &str = "kill -TERM $$";

/// One test folder: a folder of stand-ins that holds the link `xclip` to the
/// `koshi` binary under test, and an empty folder for the fake real `xclip`.
struct StandInFixture {
    /// The test folder. Dropping it removes every file the test made.
    test_directory: TempDir,
    /// The link named `xclip` to the `koshi` binary under test.
    stand_in_path: PathBuf,
    /// The folder of stand-ins.
    clipboard_tool_directory: PathBuf,
    /// The folder the fake real `xclip` goes into.
    real_tool_directory: PathBuf,
}

impl StandInFixture {
    /// The `PATH` that lists the folder of stand-ins, then the folder of the
    /// fake real `xclip`.
    fn build_path(&self) -> OsString {
        std::env::join_paths([&self.clipboard_tool_directory, &self.real_tool_directory])
            .expect("no folder holds the separator")
    }

    /// The bytes the fake real `xclip` wrote to `file_name` in the test
    /// folder, or `None` when it wrote no such file.
    fn read_recorded_file(&self, file_name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.test_directory.path().join(file_name)).ok()
    }
}

/// A [`StandInFixture`] whose real tool folder holds a fake `xclip` that runs
/// `real_tool_last_command` after it records its arguments and its standard
/// input, or nothing when `real_tool_last_command` is `None`. The fake names
/// every file and program by its absolute path, so it runs with any `PATH`.
fn build_stand_in_fixture(real_tool_last_command: Option<&str>) -> StandInFixture {
    let test_directory = build_short_test_directory();
    let clipboard_tool_directory = test_directory.path().join(CLIPBOARD_TOOL_DIRECTORY_NAME);
    let real_tool_directory = test_directory.path().join("bin");
    std::fs::create_dir(&clipboard_tool_directory).expect("the stand-in folder is made");
    std::fs::create_dir(&real_tool_directory).expect("the real tool folder is made");
    let stand_in_path = clipboard_tool_directory.join("xclip");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_koshi"), &stand_in_path)
        .expect("the stand-in link is made");
    if let Some(real_tool_last_command) = real_tool_last_command {
        let real_tool_script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n/bin/cat > '{}'\n{real_tool_last_command}\n",
            test_directory.path().join("arguments.txt").display(),
            test_directory.path().join("input.bin").display(),
        );
        let real_tool_path = real_tool_directory.join("xclip");
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

/// Run the stand-in in `fixture` with `tool_arguments`, `standard_input_bytes`
/// on its standard input, `path` as its `PATH`, and `pane_variables` as the
/// only other environment variables, and hand back how it ended.
fn run_stand_in(
    fixture: &StandInFixture,
    tool_arguments: &[&str],
    standard_input_bytes: &[u8],
    path: &OsString,
    pane_variables: &[(&str, String)],
) -> Output {
    let mut stand_in_process = Command::new(&fixture.stand_in_path)
        .args(tool_arguments)
        .env_clear()
        .env("PATH", path)
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

/// The `KOSHI_*` variables of `session`'s root pane, with `KOSHI_RUNTIME_DIR`
/// naming the session's runtime directory.
fn build_root_pane_variables(session: &RunningSession) -> Vec<(&'static str, String)> {
    let root_pane_id = session.list_pane_ids()[0];
    vec![
        ("KOSHI", "1".to_string()),
        ("KOSHI_SESSION_ID", session.session_id.to_string()),
        ("KOSHI_PANE_ID", root_pane_id.to_string()),
        (
            "KOSHI_RUNTIME_DIR",
            session.runtime_directory.path().display().to_string(),
        ),
    ]
}

/// A session whose `pane-clipboard-writes` allows writes.
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
    forward_session_events(move || connection.recv::<SessionEvent>())
}

/// The bytes of the next host write among `session_events`, the session
/// events a client reads. Every other event before it is passed over.
///
/// # Panics
/// When no host write comes within [`WAIT_DURATION`].
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

#[test]
fn a_read_runs_the_real_tool_in_place_with_its_arguments_and_input() {
    let fixture = build_stand_in_fixture(Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &fixture,
        &["-o", "-selection", "clipboard"],
        b"unread",
        &fixture.build_path(),
        &[],
    );

    assert_eq!(stand_in_output.status.code(), Some(3));
    assert_eq!(
        fixture.read_recorded_file("arguments.txt"),
        Some(b"-o\n-selection\nclipboard\n".to_vec())
    );
    assert_eq!(
        fixture.read_recorded_file("input.bin"),
        Some(b"unread".to_vec())
    );
}

#[test]
fn a_copy_outside_a_session_runs_the_real_tool_with_every_byte_of_the_input() {
    let fixture = build_stand_in_fixture(Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &fixture,
        &["-selection", "clipboard", "-rmlastnl"],
        b"hi\n",
        &fixture.build_path(),
        &[],
    );

    assert_eq!(stand_in_output.status.code(), Some(3));
    assert_eq!(
        fixture.read_recorded_file("arguments.txt"),
        Some(b"-selection\nclipboard\n-rmlastnl\n".to_vec())
    );
    assert_eq!(
        fixture.read_recorded_file("input.bin"),
        Some(b"hi\n".to_vec())
    );
}

#[test]
fn a_stand_in_with_no_real_tool_on_path_exits_with_127() {
    let fixture = build_stand_in_fixture(None);

    let stand_in_output = run_stand_in(&fixture, &["-o"], b"", &fixture.build_path(), &[]);

    assert_eq!(stand_in_output.status.code(), Some(127));
    assert_eq!(
        String::from_utf8_lossy(&stand_in_output.stderr),
        "koshi: no xclip found on PATH outside koshi's stand-in folder\n"
    );
}

#[test]
fn a_copy_whose_real_tool_a_signal_ends_exits_with_128_plus_the_signal_number() {
    let fixture = build_stand_in_fixture(Some(END_BY_SIGTERM_COMMAND));

    let stand_in_output = run_stand_in(&fixture, &[], b"hi", &fixture.build_path(), &[]);

    assert_eq!(stand_in_output.status.signal(), None);
    assert_eq!(stand_in_output.status.code(), Some(128 + 15));
    assert_eq!(
        fixture.read_recorded_file("input.bin"),
        Some(b"hi".to_vec())
    );
}

#[test]
fn a_copy_in_a_pane_only_a_client_on_another_machine_focuses_reaches_that_client_alone() {
    let session = start_session_allowing_pane_clipboard_writes();
    let remote_session_events = attach_client_from_another_machine(&session);
    let fixture = build_stand_in_fixture(Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &fixture,
        &["-selection", "clipboard", "-rmlastnl"],
        b"hi\n",
        &fixture.build_path(),
        &build_root_pane_variables(&session),
    );

    assert_eq!(stand_in_output.status.code(), Some(0));
    assert_eq!(fixture.read_recorded_file("arguments.txt"), None);
    assert_eq!(
        wait_for_next_host_write(&remote_session_events),
        b"\x1b]52;c;aGk=\x07"
    );
}

#[test]
fn a_copy_that_is_not_utf8_runs_the_real_tool_and_reaches_no_client() {
    let session = start_session_allowing_pane_clipboard_writes();
    let remote_session_events = attach_client_from_another_machine(&session);
    let fixture = build_stand_in_fixture(Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &fixture,
        &["-selection", "clipboard"],
        b"hi\xff",
        &fixture.build_path(),
        &build_root_pane_variables(&session),
    );

    assert_eq!(stand_in_output.status.code(), Some(3));
    assert_eq!(
        fixture.read_recorded_file("input.bin"),
        Some(b"hi\xff".to_vec())
    );
    // The UTF-8 copy made next is the first copy the client gets.
    let utf8_stand_in_output = run_stand_in(
        &fixture,
        &["-selection", "clipboard"],
        b"ok",
        &fixture.build_path(),
        &build_root_pane_variables(&session),
    );
    assert_eq!(utf8_stand_in_output.status.code(), Some(0));
    assert_eq!(
        wait_for_next_host_write(&remote_session_events),
        b"\x1b]52;c;b2s=\x07"
    );
}

#[test]
fn a_copy_in_a_pane_a_client_on_this_machine_focuses_runs_the_real_tool_with_every_byte() {
    let session = start_session_allowing_pane_clipboard_writes();
    let _local_client = attach_test_client(&session);
    let fixture = build_stand_in_fixture(Some(EXIT_WITH_CODE_3_COMMAND));

    let stand_in_output = run_stand_in(
        &fixture,
        &["-selection", "clipboard", "-rmlastnl"],
        b"hi\n",
        &fixture.build_path(),
        &build_root_pane_variables(&session),
    );

    assert_eq!(stand_in_output.status.code(), Some(3));
    assert_eq!(
        fixture.read_recorded_file("input.bin"),
        Some(b"hi\n".to_vec())
    );
}

/// A `koshi serve-session` process under a fresh test home, whose `PATH`
/// names only `tool_directory`, a folder that holds a fake `xclip`.
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
    let fake_tool_path = tool_directory.join("xclip");
    std::fs::write(&fake_tool_path, "#!/bin/sh\nexit 0\n").expect("the fake xclip is written");
    std::fs::set_permissions(&fake_tool_path, std::fs::Permissions::from_mode(0o755))
        .expect("the fake xclip may run");
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
    };
    let session_endpoint =
        session_process.wait_for_session_endpoint(&runtime_directory, session_id);
    SessionWithFakeTool {
        home_directory,
        runtime_directory,
        session_id,
        session_endpoint,
        session_process,
        tool_directory,
    }
}

/// Open a pane in `session` that writes its `PATH` into `pane-path.txt` in
/// the test home, and hand back that `PATH` once the file is there.
///
/// # Panics
/// When the file is not there within [`WAIT_DURATION`].
fn read_new_pane_path(session: &SessionWithFakeTool) -> String {
    let mut client_connection = open_session_connection(&session.session_endpoint);
    let client_id = attach_client_on_connection(&mut client_connection, session.session_id);
    let mut control_connection = open_session_connection(&session.session_endpoint);
    create_pane(
        &mut control_connection,
        session.session_id,
        client_id,
        Some(build_shell_spawn_spec(
            "printf '%s\\n' \"$PATH\" > \"$HOME/pane-path.tmp\"\n\
             /bin/mv \"$HOME/pane-path.tmp\" \"$HOME/pane-path.txt\"\n",
        )),
    );
    let pane_path_file = session.home_directory.path().join("pane-path.txt");
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Ok(pane_path_text) = std::fs::read_to_string(&pane_path_file) {
            return pane_path_text;
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
            "{}:{}\n",
            clipboard_tool_directory.display(),
            session.tool_directory.display()
        )
    );

    kill_session(&mut session);

    assert!(!clipboard_tool_directory.exists());
}

#[test]
fn a_session_with_writes_off_makes_no_stand_ins_and_leaves_a_new_panes_path_alone() {
    let mut session = start_session_with_fake_tool("version 2\n");
    let clipboard_tool_directory =
        resolve_clipboard_tool_directory_path(&session.runtime_directory, session.session_id);

    assert!(!clipboard_tool_directory.exists());
    assert_eq!(
        read_new_pane_path(&session),
        format!("{}\n", session.tool_directory.display())
    );

    kill_session(&mut session);
}
